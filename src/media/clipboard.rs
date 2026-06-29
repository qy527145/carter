//! 剪贴板二进制图片读取 —— Cmd+V / Ctrl+V 直接粘贴系统剪贴板里的图片到 carter。
//!
//! 实现：`arboard` crate 提供的跨平台 API (`Clipboard::get_image()`) 返回 RGBA8 原始字节，
//! 我们用 `image` crate 编码成 PNG 后再调 `put_bytes` 入库（自动 downsample + 内容寻址）。
//!
//! 平台支持：
//! - macOS：NSPasteboard `public.png` / `public.tiff` → arboard 已包好
//! - Windows：clipboard API `CF_DIB` → arboard 已包好
//! - Linux X11/Wayland：arboard 提供 wayland-data-control 后端
//!
//! 失败语义：剪贴板没图片 / 后端访问失败 → 返回 `Ok(None)`，让调用方 fallback 到普通文本粘贴。
//! 真正的 IO 错误（编码失败 / 写盘失败）才返回 `Err`。

use super::{put_bytes, ImageRef};

/// 尝试从系统剪贴板读取图片并入库。成功返回 `Some(ImageRef)`；剪贴板无图片返回 `None`。
pub fn paste_image() -> std::io::Result<Option<ImageRef>> {
    // 1. 拿 Clipboard 句柄。arboard 在某些 Linux 远端环境下可能失败（如 SSH 无 X）。
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("clipboard: init failed ({e}); treating as no image");
            return Ok(None);
        }
    };

    // 2. 读图片。返回 ImageData { width, height, bytes (RGBA8) }。
    //    没图片时 arboard 返回 ContentNotAvailable，按 Ok(None) 处理。
    let img = match clipboard.get_image() {
        Ok(i) => i,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(e) => {
            tracing::debug!("clipboard: get_image failed ({e}); treating as no image");
            return Ok(None);
        }
    };

    // 3. RGBA8 → PNG bytes（image crate）。arboard 的 bytes 是 row-major RGBA。
    let buffer = match image::RgbaImage::from_raw(
        img.width as u32,
        img.height as u32,
        img.bytes.into_owned(),
    ) {
        Some(b) => b,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "clipboard image: RGBA buffer dimensions mismatch",
            ));
        }
    };
    let mut png: Vec<u8> = Vec::new();
    if let Err(e) = image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("clipboard image: PNG encode failed: {e}"),
        ));
    }

    // 4. 走标准入库路径（自动 downsample + 内容寻址）。
    let r = put_bytes(&png, Some("png"))?;
    Ok(Some(r))
}
