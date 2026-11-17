//! grep：内置 ripgrep 引擎，不依赖系统上的 `rg` 二进制。
//!
//! ## 为什么要在进程内跑
//!
//! 早先这版实现直接执行系统上的 `rg`。用户的机器上不一定有它 —— 没有时模型拿到的是
//! 「cannot run ripgrep (is `rg` installed?)」，而用户既不知道怎么装，也不会想到一个
//! 搜索工具还需要另装程序。
//!
//! ripgrep 本身就是一组 Rust 库。这里直接把它们编进进程：
//!
//! * [`ignore`] —— 目录遍历、`.gitignore` / `.ignore` 规则、隐藏文件过滤。这就是
//!   `rg` 默认的「不搜 `.git`、跳过隐藏文件、遵守 ignore 文件」那套行为。
//! * [`grep_searcher`] —— 搜索驱动：按行读、给行号、前后上下文、二进制检测、内存映射。
//! * [`grep_regex`] —— 与 `rg` 同源的 Rust `regex` 匹配器（语法、大小写行为一致）。
//!
//! 输出格式刻意对齐 `rg --line-number --no-heading --color=never`，包括上下文行的
//! `路径-行号-内容` 与分组之间的 `--`。模型在训练数据里见过大量这种格式，改格式
//! 只会让它多花 token 重新理解。
//!
//! 搜索是阻塞 I/O，放在 `spawn_blocking` 里跑 —— 工具是 async 的，直接在 async
//! 上下文里遍历大盘会让整个运行时停住（会话事件、取消信号都进不来）。

use std::path::{Path, PathBuf};

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde_json::{Value, json};

use super::{Tool, ToolResult};

/// 与 carter 那版一致的上限（防爆 token）。
const MAX_OUTPUT: usize = 30_000;

/// `context` 参数的合理上限。模型偶尔会写出离谱的数字，`before_context(n)` 会按
/// 行数分配缓冲，不能让它任意大。
const MAX_CONTEXT: u64 = 1_000;

const DESCRIPTION: &str = "用内置 ripgrep 引擎搜索文件内容（不依赖系统安装 rg）。\
     支持正则、按 glob 过滤、限定目录、上下文行、忽略大小写。\
     默认遵守 .gitignore 并跳过隐藏文件。";

pub struct EmbeddedGrep {
    /// 会话的工作目录。只用于把结果里的文件路径显示成相对路径。
    cwd: PathBuf,
}

impl EmbeddedGrep {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait::async_trait]
impl Tool for EmbeddedGrep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        // 与 carter 那版逐字段一致：换的是实现，不是给模型的契约。
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "正则表达式" },
                "path": { "type": "string", "description": "搜索目录或文件（缺省当前目录）" },
                "glob": { "type": "string", "description": "文件过滤 glob，如 *.rs" },
                "case_insensitive": { "type": "boolean", "description": "忽略大小写（缺省 false）" },
                "context": { "type": "integer", "description": "前后各显示几行上下文（缺省 0）" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => p.to_string(),
            Some(_) => return ToolResult::err("pattern 不能为空"),
            None => return ToolResult::err("missing or non-string argument: pattern"),
        };
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.clone());
        let glob = args
            .get("glob")
            .and_then(Value::as_str)
            .filter(|g| !g.trim().is_empty())
            .map(str::to_string);
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let context = args
            .get("context")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(MAX_CONTEXT) as usize;

        let req = SearchRequest {
            pattern,
            path,
            glob,
            case_insensitive,
            context,
            cwd: self.cwd.clone(),
        };

        match tokio::task::spawn_blocking(move || search(req)).await {
            Ok(Ok(out)) => ToolResult::ok(out),
            Ok(Err(e)) => ToolResult::err(e),
            // spawn_blocking 只在 panic 时走到这里。
            Err(e) => ToolResult::err(format!("grep 执行异常：{e}")),
        }
    }
}

struct SearchRequest {
    pattern: String,
    path: PathBuf,
    glob: Option<String>,
    case_insensitive: bool,
    context: usize,
    cwd: PathBuf,
}

fn search(req: SearchRequest) -> Result<String, String> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(req.case_insensitive)
        // 支持 `(?i)` 之类内联标志、与 rg 相同的 Rust regex 语法。
        .build(&req.pattern)
        .map_err(|e| format!("正则表达式无效：{e}"))?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(req.context)
        .after_context(req.context)
        // 与 rg 默认一致：遇到 NUL 就判定为二进制，停止搜这个文件。
        // 判定前的匹配结果也不输出 —— 二进制文件里的「匹配」通常没有意义。
        .binary_detection(BinaryDetection::quit(b'\0'))
        .build();

    let meta = std::fs::metadata(&req.path)
        .map_err(|e| format!("无法访问 {}：{e}", req.path.display()))?;

    let mut out = String::new();
    let mut any_match = false;
    let mut truncated = false;
    let mut errors: Vec<String> = Vec::new();

    if meta.is_file() {
        // 显式给了一个文件：与 rg 一致，省略文件名前缀。`glob` 对显式文件不生效
        // （rg 也是这么做的 —— 用户点名要搜它）。
        run_one(
            &mut searcher,
            &matcher,
            &req.path,
            None,
            &req,
            &mut out,
            &mut any_match,
            &mut truncated,
            &mut errors,
        );
    } else if meta.is_dir() {
        let mut walk = WalkBuilder::new(&req.path);
        // `.gitignore` / `.ignore` / 隐藏文件 / 全局 gitignore —— 与 rg 默认一致。
        walk.standard_filters(true);
        if let Some(g) = &req.glob {
            let mut overrides = OverrideBuilder::new(&req.path);
            overrides
                .add(g)
                .map_err(|e| format!("glob 无效（{g}）：{e}"))?;
            let overrides = overrides
                .build()
                .map_err(|e| format!("glob 无效（{g}）：{e}"))?;
            walk.overrides(overrides);
        }

        // 单线程遍历：输出顺序稳定，且避免为一次搜索拉起线程池。文件多时
        // `ignore` 的单线程也足够快（正则匹配本身是线性扫描）。
        for entry in walk.build() {
            if truncated {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    // 权限不足之类：记下来，继续搜能搜的。
                    errors.push(e.to_string());
                    continue;
                }
            };
            // 只搜普通文件。符号链接默认不跟随（与 rg 一致），否则可能绕进环。
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let display = display_path(&req.cwd, entry.path());
            let before = out.len();
            run_one(
                &mut searcher,
                &matcher,
                entry.path(),
                Some(&display),
                &req,
                &mut out,
                &mut any_match,
                &mut truncated,
                &mut errors,
            );
            // rg 在上下文档位下用 `--` 分隔不同文件的匹配组。
            if req.context > 0 && out.len() > before && before > 0 {
                out.insert_str(before, "--\n");
            }
        }
    } else {
        return Err(format!(
            "无法搜索 {}：不是普通文件或目录",
            req.path.display()
        ));
    }

    if !any_match {
        if let Some(e) = errors.first() {
            return Err(format!("搜索没有结果，且过程中出错：{e}"));
        }
        return Ok(format!("no matches for: {}", req.pattern));
    }

    if truncated {
        out.push_str("\n... [输出超过 30 KB，已截断；缩小搜索范围或用 glob 过滤]");
    }
    if !errors.is_empty() {
        out.push_str(&format!(
            "\n[note] {} 个路径无法读取（例如权限不足），已跳过",
            errors.len()
        ));
    }
    Ok(out)
}

/// 搜一个文件，把结果追加到 `out`。
///
/// `display` = `None` 表示输出不带文件名前缀（单个显式文件，与 rg 一致）。
#[allow(clippy::too_many_arguments)]
fn run_one(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
    display: Option<&str>,
    req: &SearchRequest,
    out: &mut String,
    any_match: &mut bool,
    truncated: &mut bool,
    errors: &mut Vec<String>,
) {
    let remaining = MAX_OUTPUT.saturating_sub(out.len());
    let mut sink = FileSink::new(display, remaining, req.context > 0);
    if let Err(e) = searcher.search_path(matcher, path, &mut sink) {
        errors.push(format!("{}: {e}", path.display()));
        return;
    }
    // 二进制文件：即使判定前命中过也不输出 —— 残缺结果比没有结果更误导。
    if !sink.binary && sink.any_match {
        *any_match = true;
        out.push_str(&sink.body);
    }
    if sink.hit_limit {
        *truncated = true;
    }
}

/// 把文件路径显示成相对于会话 cwd 的形式。
///
/// 用户给的路径可能是绝对路径（如 `path` 参数）；直接打印出来又长又费 token，而且模型
/// 的系统提示里已经写了工作目录。不在 cwd 之下的路径（用户点名搜了别处）保持原样。
fn display_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// 单文件的输出缓冲。
///
/// 先写进自己的 `body` 再交给调用方，而不是直接写到全局输出：二进制检测可能在
/// 命中之后才触发，那时已写出的内容需要整体丢弃。上下文分隔符也只在**本文件内**
/// 判断，跨文件的 `--` 由调用方补。
struct FileSink<'a> {
    body: String,
    /// `None` = 不输出文件名前缀。
    display: Option<&'a str>,
    /// 本文件是否命中过。
    any_match: bool,
    /// 是否检测到二进制数据。
    binary: bool,
    /// 本文件还能写多少字节（全局 30 KB 预算的剩余部分）。
    limit: usize,
    /// 是否因为预算耗尽而停止。
    hit_limit: bool,
    /// 是否已经写过内容 —— 决定是否需要上下文分隔符。
    wrote_any: bool,
    /// 是否启用了上下文（只有启用时 rg 才打 `--`）。
    context_enabled: bool,
}

impl<'a> FileSink<'a> {
    fn new(display: Option<&'a str>, limit: usize, context_enabled: bool) -> Self {
        Self {
            body: String::new(),
            display,
            any_match: false,
            binary: false,
            limit,
            hit_limit: false,
            wrote_any: false,
            context_enabled,
        }
    }

    /// 写一行。返回 `false` = 预算耗尽，调用方应停止搜索。
    fn push_line(&mut self, sep: char, line_number: Option<u64>, bytes: &[u8]) -> bool {
        if self.hit_limit {
            return false;
        }
        let mut line = String::new();
        if let Some(p) = self.display {
            line.push_str(p);
            line.push(sep);
        }
        if let Some(n) = line_number {
            line.push_str(&n.to_string());
            line.push(sep);
        }
        // 行尾换行由 searcher 带上；统一成以 `\n` 结尾，跨平台输出一致。
        let text = String::from_utf8_lossy(bytes);
        line.push_str(text.trim_end_matches('\n').trim_end_matches('\r'));
        line.push('\n');

        if self.body.len() + line.len() > self.limit {
            self.hit_limit = true;
            return false;
        }
        self.body.push_str(&line);
        self.wrote_any = true;
        true
    }
}

impl Sink for FileSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.any_match = true;
        // `:` 分隔的是命中行；上下文行用 `-`。
        Ok(self.push_line(':', mat.line_number(), mat.bytes()))
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        Ok(self.push_line('-', ctx.line_number(), ctx.bytes()))
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        if !self.context_enabled || !self.wrote_any || self.hit_limit {
            return Ok(true);
        }
        if self.body.len() + 3 > self.limit {
            self.hit_limit = true;
            return Ok(false);
        }
        self.body.push_str("--\n");
        Ok(true)
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, Self::Error> {
        self.binary = true;
        // 停止搜这个文件。调用方会丢弃 `body`。
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("carter-grep-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(dir: &Path, args: Value) -> ToolResult {
        let tool = EmbeddedGrep::new(dir.to_path_buf());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tool.execute(args))
    }

    #[test]
    fn finds_lines_with_line_numbers_and_relative_paths() {
        let d = tmp_dir("basic");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(
            d.join("src/main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let r = run(
            &d,
            json!({"pattern": "println", "path": d.join("src").to_str().unwrap()}),
        );
        assert!(r.ok, "{}", r.content);
        // 目录搜索：路径相对于会话 cwd，而不是绝对路径。
        let line = format!("src{}main.rs:2:", std::path::MAIN_SEPARATOR);
        assert!(r.content.contains(&line), "{}", r.content);
        assert!(!r.content.contains(d.to_str().unwrap()), "不该出现绝对路径");
    }

    #[test]
    fn a_single_file_omits_the_filename_prefix() {
        let d = tmp_dir("single");
        let f = d.join("a.txt");
        std::fs::write(&f, "alpha\nbeta\n").unwrap();

        let r = run(&d, json!({"pattern": "beta", "path": f.to_str().unwrap()}));
        assert!(r.ok, "{}", r.content);
        assert_eq!(r.content.trim(), "2:beta");
    }

    #[test]
    fn context_lines_use_dashes_and_groups_are_separated() {
        let d = tmp_dir("ctx");
        let f = d.join("a.txt");
        std::fs::write(&f, "a\nb\nc\nd\ne\nf\ng\n").unwrap();

        let r = run(
            &d,
            json!({"pattern": "c|g", "path": f.to_str().unwrap(), "context": 1}),
        );
        assert!(r.ok, "{}", r.content);
        let out = r.content;
        assert!(out.contains("2-b"), "{out}");
        assert!(out.contains("3:c"), "{out}");
        assert!(out.contains("4-d"), "{out}");
        // c 的上下文（2-4）与 g 的上下文（6-7）不连续。
        assert!(out.contains("--"), "{out}");
        assert!(out.contains("6-f"), "{out}");
        assert!(out.contains("7:g"), "{out}");
    }

    #[test]
    fn case_insensitive_and_glob_filters_work() {
        let d = tmp_dir("filters");
        std::fs::write(d.join("a.rs"), "Hello\n").unwrap();
        std::fs::write(d.join("b.txt"), "Hello\n").unwrap();

        let r = run(
            &d,
            json!({"pattern": "hello", "case_insensitive": true, "glob": "*.rs"}),
        );
        assert!(r.ok, "{}", r.content);
        assert!(r.content.contains("a.rs:1:Hello"), "{}", r.content);
        assert!(!r.content.contains("b.txt"), "{}", r.content);
    }

    #[test]
    fn gitignore_hidden_and_noise_dirs_are_skipped() {
        let d = tmp_dir("ignore");
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join(".git/config"), "needle\n").unwrap();
        std::fs::write(d.join("target/x.rs"), "needle\n").unwrap();
        std::fs::write(d.join(".hidden"), "needle\n").unwrap();
        std::fs::write(d.join("ignored.rs"), "needle\n").unwrap();
        std::fs::write(d.join("kept.rs"), "needle\n").unwrap();
        std::fs::write(d.join(".gitignore"), "ignored.rs\ntarget/\n").unwrap();

        let r = run(&d, json!({"pattern": "needle"}));
        assert!(r.ok, "{}", r.content);
        assert!(r.content.contains("kept.rs"), "{}", r.content);
        assert!(!r.content.contains("ignored.rs"), "{}", r.content);
        assert!(!r.content.contains("target/"), "{}", r.content);
        assert!(!r.content.contains(".git/"), "{}", r.content);
        assert!(!r.content.contains(".hidden"), "{}", r.content);
    }

    #[test]
    fn context_groups_across_files_are_separated() {
        let d = tmp_dir("ctx-files");
        std::fs::write(d.join("x.txt"), "a\nb\nc\nd\ne\n").unwrap();
        std::fs::write(d.join("y.txt"), "a\nb\nc\nd\ne\n").unwrap();

        let r = run(&d, json!({"pattern": "c", "context": 1}));
        assert!(r.ok, "{}", r.content);
        let out = r.content;
        assert!(out.contains("x.txt-2-b"), "{out}");
        assert!(out.contains("y.txt-2-b"), "{out}");
        // 两个文件的上下文组之间也要有分隔符（与 rg 一致）。
        assert_eq!(out.matches("\n--\n").count(), 1, "{out}");
    }

    #[test]
    fn oversized_output_is_capped_with_a_visible_marker() {
        let d = tmp_dir("truncate");
        let mut body = String::new();
        for i in 0..5_000 {
            body.push_str(&format!("match line {i} with some padding\n"));
        }
        std::fs::write(d.join("big.txt"), body).unwrap();

        let r = run(
            &d,
            json!({"pattern": "match", "path": d.join("big.txt").to_str().unwrap()}),
        );
        assert!(r.ok, "{}", r.content);
        assert!(
            r.content.len() <= MAX_OUTPUT + 200,
            "太长：{}",
            r.content.len()
        );
        assert!(r.content.contains("已截断"), "要标明截断了");
    }

    #[test]
    fn invalid_regex_is_an_error_result_not_a_panic() {
        let d = tmp_dir("badre");
        let r = run(&d, json!({"pattern": "(", "path": d.to_str().unwrap()}));
        assert!(!r.ok);
        assert!(r.content.contains("正则表达式无效"), "{}", r.content);
    }

    #[test]
    fn no_matches_says_so() {
        let d = tmp_dir("none");
        std::fs::write(d.join("a.txt"), "nothing here\n").unwrap();
        let r = run(&d, json!({"pattern": "zzz", "path": d.to_str().unwrap()}));
        assert!(r.ok);
        assert!(r.content.contains("no matches for: zzz"), "{}", r.content);
    }

    #[test]
    fn binary_files_are_skipped() {
        let d = tmp_dir("binary");
        std::fs::write(d.join("bin.dat"), b"needle\x00\x01\x02").unwrap();
        std::fs::write(d.join("text.txt"), "needle\n").unwrap();

        let r = run(
            &d,
            json!({"pattern": "needle", "path": d.to_str().unwrap()}),
        );
        assert!(r.ok, "{}", r.content);
        assert!(r.content.contains("text.txt"), "{}", r.content);
        assert!(!r.content.contains("bin.dat"), "{}", r.content);
    }

    #[test]
    fn metadata_is_the_same_contract_as_carters_grep() {
        let tool = EmbeddedGrep::new(std::env::temp_dir());
        assert_eq!(tool.name(), "grep");
        let params = tool.parameters();
        assert_eq!(params["required"][0], json!("pattern"));
        for key in ["pattern", "path", "glob", "case_insensitive", "context"] {
            assert!(params["properties"].get(key).is_some(), "缺参数 {key}");
        }
        // 换实现之后，描述里不该再让用户去装 rg。
        assert!(!tool.description().contains("已安装 rg"));
    }
}
