//! Cross-platform clipboard ingestion and result copying.

use std::io::Cursor;

use anyhow::{Context, Result, bail, ensure};
use arboard::Clipboard;
use image::{DynamicImage, ImageFormat, RgbaImage};

use crate::upload::UploadPayload;

/// Read the clipboard, preferring copied files, then images, then text.
///
/// # Errors
///
/// Returns an error when the clipboard is unavailable or contains no supported data.
pub fn read_payload() -> Result<UploadPayload> {
    let mut clipboard = Clipboard::new().context("clipboard is unavailable")?;

    if let Ok(files) = clipboard.get().file_list()
        && !files.is_empty()
    {
        ensure!(
            files.len() == 1,
            "clipboard contains multiple files; select one file to upload"
        );
        let path = &files[0];

        ensure!(path.is_file(), "clipboard path is not a regular file");

        return UploadPayload::from_path(path);
    }

    if let Ok(image) = clipboard.get_image() {
        let width = u32::try_from(image.width).context("clipboard image is too wide")?;
        let height = u32::try_from(image.height).context("clipboard image is too tall")?;
        let rgba = RgbaImage::from_raw(width, height, image.bytes.into_owned())
            .context("clipboard returned an invalid RGBA image")?;
        let mut bytes = Vec::new();

        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .context("failed to encode clipboard image")?;

        return Ok(UploadPayload::from_bytes(
            bytes,
            "clipboard.png".to_owned(),
            "image/png".to_owned(),
        ));
    }

    if let Ok(text) = clipboard.get_text()
        && !text.is_empty()
    {
        return Ok(UploadPayload::from_bytes(
            text.into_bytes(),
            "clipboard.txt".to_owned(),
            "text/plain; charset=utf-8".to_owned(),
        ));
    }

    bail!("clipboard contains no file, image, or text")
}

/// Replace clipboard text with the returned upload URL.
///
/// # Errors
///
/// Returns an error if the clipboard cannot be opened or changed.
pub fn copy_link(link: &str) -> Result<()> {
    Clipboard::new()
        .context("clipboard is unavailable")?
        .set_text(link)
        .context("failed to copy upload link")
}
