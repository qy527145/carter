//! 多模态资源（图片）管理。
//!
//! 设计核心：**消息文本里只存引用**，磁盘里按 sha256 内容寻址存原始字节。
//! 这样会话 JSONL 完全是文本（不存 base64），既向后兼容、又便于压缩 / diff / gc。
//!
//! 引用语法：`[img:<hash>.<ext>]`（lowercase hex，下同），任意位置嵌入文本。
//! 发 API 前，provider 边界把每条 user/assistant 文本切分成 (text, image) 段，
//! 加载磁盘文件 base64 后组装成多模态请求体。
//!
//! 存储路径：`~/.carter/images/<hash>.<ext>`（按内容寻址，重复粘贴自动去重）。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::paths::carter_home;

/// 一条「文本+图片」混排消息切片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// 文本片段（可空）。
    Text(String),
    /// 图片引用：磁盘 `images/<hash>.<ext>` 文件。
    Image(ImageRef),
}

/// 图片引用：哈希 + 扩展名（扩展名同时承载 MIME 推断）。
/// 序列化形态：`[img:<hash>.<ext>]`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub hash: String,
    pub ext: String,
}

impl ImageRef {
    /// 还原引用字符串形态。
    pub fn token(&self) -> String {
        format!("[img:{}.{}]", self.hash, self.ext)
    }

    /// 推断 MIME。未知扩展名回落 `application/octet-stream`。
    pub fn mime(&self) -> &'static str {
        mime_from_ext(&self.ext)
    }

    /// 文件磁盘绝对路径。
    pub fn path(&self) -> PathBuf {
        images_dir().join(format!("{}.{}", self.hash, self.ext))
    }
}

/// `~/.carter/images/`。
pub fn images_dir() -> PathBuf {
    carter_home().join("images")
}

/// 把原始字节按内容寻址存入 image store；若已存在则跳过写。
/// `ext_hint` 可由调用方提供（如来自源文件扩展名）；否则按字节嗅探。
/// 返回引用句柄。
pub fn put_bytes(bytes: &[u8], ext_hint: Option<&str>) -> std::io::Result<ImageRef> {
    let ext = ext_hint
        .map(|s| s.trim_start_matches('.').to_ascii_lowercase())
        .or_else(|| sniff_ext(bytes).map(str::to_string))
        .unwrap_or_else(|| "bin".to_string());

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hex_lower(&hasher.finalize());

    let dir = images_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{hash}.{ext}"));
    if !path.exists() {
        std::fs::write(&path, bytes)?;
    }
    Ok(ImageRef { hash, ext })
}

/// 从磁盘路径导入：读字节，扩展名从源文件名取。
pub fn put_path(src: &Path) -> std::io::Result<ImageRef> {
    let bytes = std::fs::read(src)?;
    let ext = src
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_string);
    put_bytes(&bytes, ext.as_deref())
}

/// 加载引用对应的字节（发 API 时用）。
pub fn read(rf: &ImageRef) -> std::io::Result<Vec<u8>> {
    std::fs::read(rf.path())
}

/// 把任意嵌入了 `[img:...]` 的文本切成段序列。
/// 无引用时返回单个 `Segment::Text`；末尾仅当上一段是 Image 才补一个空 Text（保证回环可重组）。
pub fn parse_segments(text: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut cursor: usize = 0;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let Some(open_rel) = rest.find("[img:") else {
            out.push(Segment::Text(rest.to_string()));
            return out;
        };
        let open = cursor + open_rel;
        let after_open = open + "[img:".len();
        let Some(close_rel) = text[after_open..].find(']') else {
            out.push(Segment::Text(text[cursor..].to_string()));
            return out;
        };
        let close = after_open + close_rel;
        let body = &text[after_open..close];
        // body 形如 `<hash>.<ext>`，仅当确实能拆出两段时才识别为引用。
        if let Some((hash, ext)) = body.rsplit_once('.') {
            if !hash.is_empty()
                && !ext.is_empty()
                && hash.chars().all(|c| c.is_ascii_hexdigit())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
            {
                if open > cursor {
                    out.push(Segment::Text(text[cursor..open].to_string()));
                }
                out.push(Segment::Image(ImageRef {
                    hash: hash.to_string(),
                    ext: ext.to_ascii_lowercase(),
                }));
                cursor = close + 1;
                continue;
            }
        }
        // 不是合法引用 → 当文本处理，跳过这个 `[img:` 起点的 1 字节避免死循环。
        out.push(Segment::Text(text[cursor..open + 1].to_string()));
        cursor = open + 1;
    }
    out
}

/// 用字节嗅探常见图片格式。返回扩展名（不带点）。
fn sniff_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some("png");
    }
    if bytes.len() >= 3 && &bytes[..3] == b"\xFF\xD8\xFF" {
        return Some("jpg");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// 扩展名 → MIME。
pub fn mime_from_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// 路径扩展名是否被视为图片。
pub fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .map(|e| matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
        ))
        .unwrap_or(false)
}

/// 把一段「来自用户输入」的文本里的 `@图片路径` 自动转成 `[img:...]` 引用。
/// 规则：
/// - 仅识别 `@` 后面跟 **图片扩展名** 的相对/绝对路径（避免误抓邮件、@提及等）。
/// - 路径以下一个空白字符 / 字符串末尾为界。
/// - 相对路径以 `cwd` 解析；不存在或非图片 → 原样保留（不破坏 agent 的 `@文件` 语义）。
/// - 注册失败（IO 错误）也原样保留并记录 warn。
///
/// 这是给 TUI 提交路径用的、对用户透明的预处理；oneshot 也可复用。
pub fn inline_user_attachments(text: &str, cwd: &Path) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        // 寻找下一个 `@`；将 `@` 之前的内容原样写入。
        let slice = &text[i..];
        let Some(at_rel) = slice.find('@') else {
            out.push_str(slice);
            break;
        };
        let at = i + at_rel;
        // `@` 必须在词边界（行首 / 空白后），不破坏邮箱式 `a@b`。
        let prev_is_boundary = at == 0
            || text[..at]
                .chars()
                .next_back()
                .map(char::is_whitespace)
                .unwrap_or(true);
        if !prev_is_boundary {
            // 写到 `@` 含本身一字符后继续。
            out.push_str(&text[i..at + 1]);
            i = at + 1;
            continue;
        }
        // 取 `@` 之后到下一个空白/字符串末尾为路径候选。
        let path_start = at + 1;
        let path_end = text[path_start..]
            .find(char::is_whitespace)
            .map(|d| path_start + d)
            .unwrap_or(text.len());
        let candidate = &text[path_start..path_end];
        let cand_path = Path::new(candidate);
        let abs_path = if cand_path.is_absolute() {
            cand_path.to_path_buf()
        } else {
            cwd.join(cand_path)
        };
        // 只处理图片扩展名且文件确实存在的；其它原样保留。
        if is_image_path(cand_path) && abs_path.is_file() {
            // 写入 `@` 之前的部分。
            out.push_str(&text[i..at]);
            match put_path(&abs_path) {
                Ok(rf) => out.push_str(&rf.token()),
                Err(e) => {
                    tracing::warn!("media: inline attach {} failed: {e}", abs_path.display());
                    out.push_str(&text[at..path_end]);
                }
            }
            i = path_end;
        } else {
            // 不是图片 → 把 `@candidate` 原样写出，继续扫描。
            out.push_str(&text[i..path_end]);
            i = path_end;
        }
    }
    out
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pure_text() {
        let segs = parse_segments("hello world");
        assert_eq!(segs, vec![Segment::Text("hello world".into())]);
    }

    #[test]
    fn parse_single_image_ref() {
        let segs = parse_segments("看下这个 [img:abc123.png] 图片");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0], Segment::Text("看下这个 ".into()));
        assert!(matches!(&segs[1], Segment::Image(r) if r.hash == "abc123" && r.ext == "png"));
        assert_eq!(segs[2], Segment::Text(" 图片".into()));
    }

    #[test]
    fn parse_image_only() {
        let segs = parse_segments("[img:deadbeef.jpg]");
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], Segment::Image(r) if r.ext == "jpg"));
    }

    #[test]
    fn parse_multiple() {
        let segs = parse_segments("a[img:11.png]b[img:22.gif]");
        assert_eq!(segs.len(), 4);
    }

    #[test]
    fn parse_invalid_token_kept_as_text() {
        // 缺扩展名 / 非 hex → 当文本，且不死循环。可能拆成多段 Text（实现细节），
        // 合并后等价于原文。
        fn join_text(segs: Vec<Segment>) -> String {
            segs.into_iter()
                .map(|s| match s {
                    Segment::Text(t) => t,
                    Segment::Image(r) => r.token(),
                })
                .collect()
        }
        assert_eq!(join_text(parse_segments("[img:nope]")), "[img:nope]");
        assert_eq!(join_text(parse_segments("[img:zz.png]")), "[img:zz.png]");
    }

    #[test]
    fn token_roundtrip() {
        let r = ImageRef { hash: "abc".into(), ext: "png".into() };
        let segs = parse_segments(&r.token());
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], Segment::Image(x) if x == &r));
    }

    #[test]
    fn sniff_png_jpg_gif_webp() {
        assert_eq!(sniff_ext(b"\x89PNG\r\n\x1a\n..."), Some("png"));
        assert_eq!(sniff_ext(b"\xFF\xD8\xFF\xE0..."), Some("jpg"));
        assert_eq!(sniff_ext(b"GIF89a..."), Some("gif"));
        let mut webp = Vec::from(b"RIFF\0\0\0\0WEBP" as &[u8]);
        webp.extend_from_slice(b"...");
        assert_eq!(sniff_ext(&webp), Some("webp"));
        assert_eq!(sniff_ext(b"not an image"), None);
    }

    #[test]
    fn mime_lookup() {
        assert_eq!(mime_from_ext("png"), "image/png");
        assert_eq!(mime_from_ext("JPG"), "image/jpeg");
        assert_eq!(mime_from_ext("unknown"), "application/octet-stream");
    }

    #[test]
    fn is_image_path_matches_common_exts() {
        assert!(is_image_path(Path::new("a/b.png")));
        assert!(is_image_path(Path::new("c.JPEG")));
        assert!(!is_image_path(Path::new("c.rs")));
    }

    /// 串行化所有「会动 CARTER_HOME」的测试，避免 cargo test 多线程下 env 竞态。
    fn home_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn put_bytes_is_content_addressed_and_idempotent() {
        let _g = home_test_lock();
        // 用临时目录避免污染真实 home。
        let tmp = std::env::temp_dir().join(format!("carter-imgtest-{}", crate::session::now_ms()));
        // SAFETY: 单进程测试，且我们用唯一目录。
        unsafe { std::env::set_var("CARTER_HOME", &tmp); }
        let bytes = b"\x89PNG\r\n\x1a\nfake";
        let a = put_bytes(bytes, None).unwrap();
        let b = put_bytes(bytes, None).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.ext, "png");
        // 写第二次不应破坏文件。
        assert!(a.path().exists());
        // 清理
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var("CARTER_HOME"); }
    }

    #[test]
    fn inline_attachments_replaces_image_paths_only() {
        let _g = home_test_lock();
        // 准备隔离 CARTER_HOME + 工作目录，并放一个真实 png 文件。
        let tmp = std::env::temp_dir().join(format!("carter-attach-{}", crate::session::now_ms()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cwd = tmp.join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let png = cwd.join("pic.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nbytes").unwrap();
        let txt = cwd.join("notes.txt");
        std::fs::write(&txt, b"hi").unwrap();
        // SAFETY: 单进程测试。
        unsafe { std::env::set_var("CARTER_HOME", &tmp); }

        // 命中：相对路径图片 → 被替换为 token。
        let out = inline_user_attachments("看下 @pic.png 这张图", &cwd);
        assert!(out.contains("[img:"));
        assert!(!out.contains("@pic.png"));

        // 不命中：非图片 @ 路径保留原样（不破坏 agent `@文件` 语义）。
        let out = inline_user_attachments("引用 @notes.txt 看看", &cwd);
        assert!(out.contains("@notes.txt"));

        // 不命中：邮箱式 a@b 不识别（非词边界）。
        let out = inline_user_attachments("contact a@b.com", &cwd);
        assert_eq!(out, "contact a@b.com");

        // 不命中：路径不存在的图片，原样保留。
        let out = inline_user_attachments("@missing.png ok", &cwd);
        assert!(out.starts_with("@missing.png"));

        // 清理
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var("CARTER_HOME"); }
    }
}
