//! `ShareR` tray logo decoding.

use anyhow::{Context, Result};

fn rgba() -> Result<image::RgbaImage> {
    let bytes = include_bytes!("../assets/sharer.png");

    Ok(image::load_from_memory(bytes)
        .context("embedded logo is not an image")?
        .into_rgba8())
}

pub(super) fn tray_icon() -> Result<tray_icon::Icon> {
    let image = image::imageops::resize(&rgba()?, 32, 32, image::imageops::FilterType::Lanczos3);
    let (width, height) = image.dimensions();

    tray_icon::Icon::from_rgba(image.into_raw(), width, height)
        .context("embedded logo has invalid dimensions")
}
