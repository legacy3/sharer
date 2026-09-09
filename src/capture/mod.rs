//! Native screenshot and recording capture for each desktop platform.

use std::io::Cursor;

use anyhow::{Context, Result};
use image::{DynamicImage, ImageFormat, RgbaImage, imageops};
use num_traits::ToPrimitive as _;

use crate::{
    config::{CaptureResolution, ResizeQuality},
    upload::UploadPayload,
};

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::RegionCapturer;

/// Return the title of the active capturable window, when the platform exposes one.
#[must_use]
pub fn active_window_title() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        macos::active_window_title()
    }

    #[cfg(not(target_os = "macos"))]
    {
        xcap::Window::all()
            .ok()?
            .into_iter()
            .find(|window| window.is_focused().unwrap_or(false))?
            .title()
            .ok()
    }
}

#[cfg(any(windows, test))]
const SCRGB_REFERENCE_NITS: f32 = 80.0;
#[cfg(any(windows, test))]
const ULTRA_HDR_LINEAR_REFERENCE_NITS: f32 = 203.0;

#[cfg(any(windows, test))]
fn scrgb_to_ultra_hdr_linear(value: f32) -> f32 {
    value * (SCRGB_REFERENCE_NITS / ULTRA_HDR_LINEAR_REFERENCE_NITS)
}

/// Apply a configured stem while retaining the automatically detected HDR marker.
#[must_use]
pub fn named_capture_filename(filename_stem: &str, encoded_filename: &str) -> String {
    let suffix = if encoded_filename.ends_with("-hdr.jpg") {
        "-hdr.jpg"
    } else {
        ".png"
    };

    format!("{filename_stem}{suffix}")
}

/// Color encoding selected for generated screenshots.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CaptureColorMode {
    #[default]
    Automatic,
    Sdr,
}

/// A desktop frame retained for interactive region selection.
#[derive(Debug)]
pub struct CapturedScreen {
    preview: RgbaImage,
    desktop_bounds: DesktopBounds,
    window_regions: Vec<WindowRegion>,
    #[cfg(windows)]
    color_mode: CaptureColorMode,
    #[cfg(windows)]
    hdr_frame: windows_hdr::HdrFrame,
}

#[derive(Debug)]
pub(super) struct WindowRegion {
    bounds: [f32; 4],
    title: String,
}

impl CapturedScreen {
    /// Return the SDR preview shown by the region selector.
    #[must_use]
    pub const fn preview(&self) -> &RgbaImage {
        &self.preview
    }

    /// Return the logical size used to present the selector on the desktop.
    #[must_use]
    pub const fn selector_dimensions(&self) -> (u32, u32) {
        (self.desktop_bounds.width, self.desktop_bounds.height)
    }

    /// Build a smaller selector-only preview when display pixels exceed logical points.
    #[must_use]
    pub fn scaled_selector_preview(&self) -> Option<RgbaImage> {
        let (width, height) = self.selector_dimensions();
        let source_area = u64::from(self.preview.width()) * u64::from(self.preview.height());
        let selector_area = u64::from(width) * u64::from(height);

        (width > 0 && height > 0 && selector_area < source_area)
            .then(|| imageops::resize(&self.preview, width, height, imageops::FilterType::Triangle))
    }

    /// Release the Windows capture representation that the selected output mode cannot use.
    pub fn release_redundant_buffers(&mut self) {
        #[cfg(windows)]
        match self.color_mode {
            CaptureColorMode::Automatic => self.preview = RgbaImage::new(0, 0),
            CaptureColorMode::Sdr => self.hdr_frame.release_pixels(),
        }
    }

    /// Return the topmost visible window rectangle under a normalized point.
    #[must_use]
    pub fn window_region_at(&self, point: [f32; 2]) -> Option<[f32; 4]> {
        window_region_at(&self.window_regions, point)
    }

    /// Return the title of a snapped window selection.
    #[must_use]
    pub fn window_title_for_region(&self, selected: NormalizedRegion) -> Option<&str> {
        const TOLERANCE: f32 = 0.000_1;

        self.window_regions.iter().find_map(|window| {
            let bounds = window.bounds;
            let matches = (bounds[0] - selected.x).abs() < TOLERANCE
                && (bounds[1] - selected.y).abs() < TOLERANCE
                && (bounds[2] - selected.width).abs() < TOLERANCE
                && (bounds[3] - selected.height).abs() < TOLERANCE;

            matches.then_some(window.title.as_str())
        })
    }

    /// Convert a normalized selection into logical desktop coordinates.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected rectangle is empty.
    pub fn desktop_region(&self, region: NormalizedRegion) -> Result<DesktopRegion> {
        let bounds = normalized_desktop_bounds(self.desktop_bounds, region)?;

        Ok(DesktopRegion {
            x: bounds.x,
            y: bounds.y,
            width: bounds.width,
            height: bounds.height,
        })
    }
}

/// A selected rectangle in logical virtual-desktop coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DesktopBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

/// Reusable capture source for a selected desktop region.
#[derive(Debug)]
#[cfg(not(target_os = "macos"))]
pub struct RegionCapturer {
    monitor: xcap::Monitor,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[cfg(not(target_os = "macos"))]
impl RegionCapturer {
    /// Prepare repeated capture of a selected desktop region.
    ///
    /// # Errors
    ///
    /// Returns an error when the region does not fit on an available monitor.
    pub fn new(region: DesktopRegion, capture_resolution: CaptureResolution) -> Result<Self> {
        let _ = capture_resolution;
        let monitor = xcap::Monitor::from_point(region.x, region.y)
            .context("failed to find the recording display")?;
        let monitor_x = monitor.x().context("failed to read display position")?;
        let monitor_y = monitor.y().context("failed to read display position")?;
        let x = u32::try_from(region.x.saturating_sub(monitor_x))
            .context("recording starts outside its display")?;
        let y = u32::try_from(region.y.saturating_sub(monitor_y))
            .context("recording starts outside its display")?;
        let monitor_width = monitor.width().context("failed to read display width")?;
        let monitor_height = monitor.height().context("failed to read display height")?;

        anyhow::ensure!(
            x.saturating_add(region.width) <= monitor_width
                && y.saturating_add(region.height) <= monitor_height,
            "recording region crosses a display boundary"
        );

        Ok(Self {
            monitor,
            x,
            y,
            width: region.width,
            height: region.height,
        })
    }

    /// Capture one RGBA frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform cannot capture the selected pixels.
    pub fn capture_rgba(&self) -> Result<Vec<u8>> {
        self.monitor
            .capture_region(self.x, self.y, self.width, self.height)
            .map(image::RgbaImage::into_raw)
            .context("failed to capture a recording frame")
    }

    /// Return the physical pixel dimensions emitted by this capture source.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl DesktopRegion {
    /// Return the left edge in virtual-desktop coordinates.
    #[must_use]
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Return the top edge in virtual-desktop coordinates.
    #[must_use]
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Return the selected width in pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Return the selected height in pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// Rectangle in normalized desktop coordinates.
#[derive(Clone, Copy, Debug)]
pub struct NormalizedRegion {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl NormalizedRegion {
    /// Create a region from `[x, y, width, height]`, each relative to the screen.
    #[must_use]
    pub const fn new(coordinates: [f32; 4]) -> Self {
        Self {
            x: coordinates[0],
            y: coordinates[1],
            width: coordinates[2],
            height: coordinates[3],
        }
    }
}

/// Capture the primary display.
///
/// Windows detects extended-range pixels in a 16-bit-float compositor frame.
/// HDR frames become backwards-compatible Ultra HDR JPEGs.
/// SDR frames and other platforms use RGBA PNGs.
/// Ordinary viewers see a tone-mapped base image while gain-map-aware viewers restore the range.
///
/// # Errors
///
/// Returns an error when display capture or encoding fails.
pub fn primary_monitor(
    color_mode: CaptureColorMode,
    capture_resolution: CaptureResolution,
    resize_quality: ResizeQuality,
) -> Result<UploadPayload> {
    #[cfg(windows)]
    {
        let _ = capture_resolution;
        let _ = resize_quality;

        return windows_hdr::capture_primary(color_mode);
    }

    #[cfg(target_os = "macos")]
    return macos::capture_primary(color_mode, capture_resolution, resize_quality);

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let _ = color_mode;
        let _ = capture_resolution;
        let _ = resize_quality;
        let monitor = primary_monitor_handle()?;
        let image = monitor
            .capture_image()
            .context("failed to capture the primary display")?;

        encode_sdr_png(image, "screenshot.png")
    }
}

/// Reject repeated-capture recording on native Wayland.
///
/// # Errors
///
/// Returns an error when region recording is requested from native Wayland on Linux.
pub fn ensure_recording_supported() -> Result<()> {
    #[cfg(target_os = "linux")]
    anyhow::ensure!(
        std::env::var_os("XDG_SESSION_TYPE")
            .is_none_or(|session| !session.eq_ignore_ascii_case("wayland")),
        "region recording currently requires an X11 session on Linux"
    );

    Ok(())
}

/// Capture the primary display for interactive region selection.
///
/// # Errors
///
/// Returns an error when a display is unavailable or capture fails.
pub fn capture_region_source(
    color_mode: CaptureColorMode,
    capture_resolution: CaptureResolution,
) -> Result<CapturedScreen> {
    #[cfg(target_os = "macos")]
    return macos::capture_region_source(color_mode, capture_resolution);

    #[cfg(not(target_os = "macos"))]
    let monitor = primary_monitor_handle()?;
    #[cfg(not(target_os = "macos"))]
    let desktop_bounds = monitor_bounds(&monitor)?;
    #[cfg(not(target_os = "macos"))]
    let window_regions = visible_window_regions(desktop_bounds);

    #[cfg(all(not(windows), not(target_os = "macos")))]
    let _ = color_mode;
    #[cfg(not(target_os = "macos"))]
    let _ = capture_resolution;

    #[cfg(windows)]
    {
        let hdr_frame = windows_hdr::capture_frame()?;
        let preview = windows_hdr::sdr_image(&hdr_frame)?;

        return Ok(CapturedScreen {
            preview,
            desktop_bounds,
            window_regions,
            color_mode,
            hdr_frame,
        });
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    let preview = monitor
        .capture_image()
        .context("failed to capture the primary display")?;

    #[cfg(all(not(windows), not(target_os = "macos")))]
    Ok(CapturedScreen {
        preview,
        desktop_bounds,
        window_regions,
    })
}

/// Crop a normalized selection and encode it as PNG.
///
/// # Errors
///
/// Returns an error if the selection is empty or encoding fails.
pub fn crop_region(image: &RgbaImage, region: NormalizedRegion) -> Result<UploadPayload> {
    let bounds = normalized_bounds(image, region)?;
    let cropped =
        imageops::crop_imm(image, bounds.x, bounds.y, bounds.width, bounds.height).to_image();

    encode_sdr_png(cropped, "region.png")
}

/// Crop an interactive region, preserving automatically detected Windows HDR.
///
/// # Errors
///
/// Returns an error if the selection is empty or encoding fails.
pub fn crop_captured_region(
    screen: &CapturedScreen,
    region: NormalizedRegion,
    capture_resolution: CaptureResolution,
    resize_quality: ResizeQuality,
) -> Result<UploadPayload> {
    #[cfg(windows)]
    {
        let _ = capture_resolution;
        let _ = resize_quality;
        let bounds =
            normalized_bounds_dimensions(screen.hdr_frame.width, screen.hdr_frame.height, region)?;

        return match screen.color_mode {
            CaptureColorMode::Automatic => windows_hdr::encode_region(&screen.hdr_frame, bounds),

            CaptureColorMode::Sdr => {
                let cropped = imageops::crop_imm(
                    screen.preview(),
                    bounds.x,
                    bounds.y,
                    bounds.width,
                    bounds.height,
                )
                .to_image();

                encode_sdr_png(cropped, "region.png")
            }
        };
    }

    #[cfg(not(windows))]
    {
        let bounds = normalized_bounds(screen.preview(), region)?;
        let cropped = imageops::crop_imm(
            screen.preview(),
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
        )
        .to_image();

        #[cfg(target_os = "macos")]
        let cropped = if capture_resolution == CaptureResolution::Logical {
            let logical = screen.desktop_region(region)?;

            resize_rgba(&cropped, logical.width(), logical.height(), resize_quality)
        } else {
            cropped
        };

        #[cfg(not(target_os = "macos"))]
        let _ = (capture_resolution, resize_quality);

        encode_sdr_png(cropped, "region.png")
    }
}

#[cfg(any(target_os = "macos", test))]
fn resize_rgba(image: &RgbaImage, width: u32, height: u32, quality: ResizeQuality) -> RgbaImage {
    let filter = match quality {
        ResizeQuality::Fast => imageops::FilterType::Triangle,
        ResizeQuality::Balanced => imageops::FilterType::CatmullRom,
        ResizeQuality::Sharp => imageops::FilterType::Lanczos3,
    };

    imageops::resize(image, width, height, filter)
}

#[derive(Clone, Copy, Debug)]
struct CropBounds {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[cfg(not(target_os = "macos"))]
fn primary_monitor_handle() -> Result<xcap::Monitor> {
    let monitors = xcap::Monitor::all().context("failed to enumerate displays")?;

    for monitor in monitors {
        if monitor
            .is_primary()
            .context("failed to identify the primary display")?
        {
            return Ok(monitor);
        }
    }

    anyhow::bail!("failed to find the primary display")
}

#[cfg(not(target_os = "macos"))]
fn monitor_bounds(monitor: &xcap::Monitor) -> Result<DesktopBounds> {
    Ok(DesktopBounds {
        x: monitor.x().context("failed to read display position")?,
        y: monitor.y().context("failed to read display position")?,
        width: monitor.width().context("failed to read display width")?,
        height: monitor.height().context("failed to read display height")?,
    })
}

#[cfg(not(target_os = "macos"))]
fn window_bounds(window: &xcap::Window) -> Result<DesktopBounds> {
    Ok(DesktopBounds {
        x: window.x().context("failed to read window position")?,
        y: window.y().context("failed to read window position")?,
        width: window.width().context("failed to read window width")?,
        height: window.height().context("failed to read window height")?,
    })
}

pub(super) fn intersect_bounds(left: DesktopBounds, right: DesktopBounds) -> Option<DesktopBounds> {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = (i64::from(left.x) + i64::from(left.width))
        .min(i64::from(right.x) + i64::from(right.width));
    let bottom_edge = (i64::from(left.y) + i64::from(left.height))
        .min(i64::from(right.y) + i64::from(right.height));
    let width = u32::try_from(right_edge.saturating_sub(i64::from(x))).ok()?;
    let height = u32::try_from(bottom_edge.saturating_sub(i64::from(y))).ok()?;

    (width > 0 && height > 0).then_some(DesktopBounds {
        x,
        y,
        width,
        height,
    })
}

#[cfg(not(target_os = "macos"))]
fn visible_window_regions(desktop_bounds: DesktopBounds) -> Vec<WindowRegion> {
    let Some(desktop_width) = desktop_bounds.width.to_f32() else {
        return Vec::new();
    };
    let Some(desktop_height) = desktop_bounds.height.to_f32() else {
        return Vec::new();
    };

    xcap::Window::all()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|window| {
            let title = window.title().ok()?;

            (!window.is_minimized().unwrap_or(true)
                && !title.is_empty()
                && !title.eq_ignore_ascii_case("ShareR"))
            .then_some((window, title))
        })
        .filter_map(|(window, title)| {
            intersect_bounds(window_bounds(&window).ok()?, desktop_bounds)
                .map(|bounds| (bounds, title))
        })
        .filter_map(|(bounds, title)| {
            let x = (bounds.x - desktop_bounds.x).to_f32()? / desktop_width;
            let y = (bounds.y - desktop_bounds.y).to_f32()? / desktop_height;
            let width = bounds.width.to_f32()? / desktop_width;
            let height = bounds.height.to_f32()? / desktop_height;

            (width > 0.02 && height > 0.02).then_some(WindowRegion {
                bounds: [x, y, width, height],
                title,
            })
        })
        .collect()
}

fn window_region_at(windows: &[WindowRegion], point: [f32; 2]) -> Option<[f32; 4]> {
    windows.iter().find_map(|window| {
        let [x, y, width, height] = window.bounds;

        (point[0] >= x && point[0] <= x + width && point[1] >= y && point[1] <= y + height)
            .then_some(window.bounds)
    })
}

fn normalized_desktop_bounds(
    desktop: DesktopBounds,
    region: NormalizedRegion,
) -> Result<DesktopRegion> {
    let desktop_width = desktop.width.to_f32().context("desktop is too wide")?;
    let desktop_height = desktop.height.to_f32().context("desktop is too tall")?;
    let x_offset = (region.x.clamp(0.0, 1.0) * desktop_width)
        .floor()
        .to_i32()
        .context("invalid desktop selection x coordinate")?;
    let y_offset = (region.y.clamp(0.0, 1.0) * desktop_height)
        .floor()
        .to_i32()
        .context("invalid desktop selection y coordinate")?;
    let width = (region.width.clamp(0.0, 1.0) * desktop_width)
        .round()
        .to_u32()
        .context("invalid desktop selection width")?
        .min(
            desktop
                .width
                .saturating_sub(x_offset.max(0).to_u32().unwrap_or(0)),
        );
    let height = (region.height.clamp(0.0, 1.0) * desktop_height)
        .round()
        .to_u32()
        .context("invalid desktop selection height")?
        .min(
            desktop
                .height
                .saturating_sub(y_offset.max(0).to_u32().unwrap_or(0)),
        );

    anyhow::ensure!(width > 0 && height > 0, "selected desktop region is empty");

    Ok(DesktopRegion {
        x: desktop.x.saturating_add(x_offset),
        y: desktop.y.saturating_add(y_offset),
        width,
        height,
    })
}

fn normalized_bounds(image: &RgbaImage, region: NormalizedRegion) -> Result<CropBounds> {
    normalized_bounds_dimensions(image.width(), image.height(), region)
}

fn normalized_bounds_dimensions(
    image_width: u32,
    image_height: u32,
    region: NormalizedRegion,
) -> Result<CropBounds> {
    let image_width_float = image_width.to_f32().context("image width is too large")?;
    let image_height_float = image_height.to_f32().context("image height is too large")?;
    let x = (region.x.clamp(0.0, 1.0) * image_width_float)
        .floor()
        .to_u32()
        .context("invalid horizontal selection")?;
    let y = (region.y.clamp(0.0, 1.0) * image_height_float)
        .floor()
        .to_u32()
        .context("invalid vertical selection")?;
    let width = (region.width.clamp(0.0, 1.0) * image_width_float)
        .round()
        .to_u32()
        .context("invalid selection width")?;
    let height = (region.height.clamp(0.0, 1.0) * image_height_float)
        .round()
        .to_u32()
        .context("invalid selection height")?;
    let width = width.min(image_width.saturating_sub(x));
    let height = height.min(image_height.saturating_sub(y));

    anyhow::ensure!(width > 0 && height > 0, "selected region is empty");

    Ok(CropBounds {
        x,
        y,
        width,
        height,
    })
}

fn encode_sdr_png(image: RgbaImage, filename: &str) -> Result<UploadPayload> {
    let mut bytes = Vec::new();

    DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .context("failed to encode screenshot")?;

    Ok(UploadPayload::from_bytes(
        bytes,
        filename.to_owned(),
        "image/png".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resize_quality_produces_the_requested_dimensions() {
        let image = RgbaImage::from_fn(7, 5, |x, y| {
            image::Rgba([
                u8::try_from(x * 31).unwrap(),
                u8::try_from(y * 47).unwrap(),
                u8::try_from((x + y) * 19).unwrap(),
                255,
            ])
        });

        for quality in [
            ResizeQuality::Fast,
            ResizeQuality::Balanced,
            ResizeQuality::Sharp,
        ] {
            let resized = resize_rgba(&image, 3, 2, quality);

            assert_eq!(resized.dimensions(), (3, 2));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn selector_preview_uses_logical_desktop_dimensions() {
        let screen = CapturedScreen {
            preview: RgbaImage::new(8, 4),
            desktop_bounds: DesktopBounds {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            },
            window_regions: Vec::new(),
        };
        let preview = screen.scaled_selector_preview().unwrap();

        assert_eq!(preview.dimensions(), (4, 2));
    }

    mod regions {
        use super::*;

        #[test]
        fn normalized_region_is_cropped_to_expected_dimensions() {
            let image = RgbaImage::new(200, 100);
            let region = NormalizedRegion::new([0.25, 0.2, 0.5, 0.4]);
            let payload = crop_region(&image, region).unwrap();
            let decoded = image::load_from_memory(payload.bytes().unwrap()).unwrap();

            assert_eq!((decoded.width(), decoded.height()), (100, 40));
            assert_eq!(payload.filename, "region.png");
        }

        #[test]
        fn normalized_region_maps_to_offset_desktop_coordinates() {
            let desktop = DesktopBounds {
                x: -1920,
                y: 100,
                width: 1920,
                height: 1080,
            };
            let region = NormalizedRegion::new([0.25, 0.5, 0.5, 0.25]);
            let bounds = normalized_desktop_bounds(desktop, region).unwrap();

            assert_eq!(
                bounds,
                DesktopRegion {
                    x: -1440,
                    y: 640,
                    width: 960,
                    height: 270,
                }
            );
        }

        #[test]
        fn hover_chooses_first_topmost_overlapping_window() {
            let top = [0.2, 0.2, 0.4, 0.4];
            let windows = [
                WindowRegion {
                    bounds: top,
                    title: "Top".to_owned(),
                },
                WindowRegion {
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    title: "Bottom".to_owned(),
                },
            ];

            assert_eq!(window_region_at(&windows, [0.3, 0.3]), Some(top));
            assert_eq!(window_region_at(&windows, [1.1, 0.5]), None);
        }
    }

    mod sdr {
        use super::*;

        #[test]
        fn generated_png_has_native_dimensions_and_no_optional_metadata() {
            let image = RgbaImage::from_pixel(17, 9, image::Rgba([12, 34, 56, 255]));
            let payload = encode_sdr_png(image, "private.png").unwrap();
            let bytes = payload.bytes().unwrap();
            let decoded = image::load_from_memory(bytes).unwrap();
            let mut chunks = Vec::new();
            let mut offset = 8;

            while offset + 12 <= bytes.len() {
                let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
                let end = offset + 12 + length as usize;

                if end > bytes.len() {
                    break;
                }

                chunks.push(&bytes[offset + 4..offset + 8]);
                offset = end;
            }

            assert_eq!((decoded.width(), decoded.height()), (17, 9));

            for forbidden in [b"tEXt", b"zTXt", b"iTXt", b"eXIf"] {
                assert!(!chunks.contains(&forbidden.as_slice()));
            }
        }

        #[test]
        fn generated_filename_preserves_automatic_hdr_marker() {
            assert_eq!(
                named_capture_filename("terminal", "region-hdr.jpg"),
                "terminal-hdr.jpg"
            );
            assert_eq!(
                named_capture_filename("terminal", "region.png"),
                "terminal.png"
            );
        }

        #[test]
        fn scrgb_reference_white_is_normalized_for_ultra_hdr() {
            let normalized = scrgb_to_ultra_hdr_linear(1.0);

            assert!((normalized - 80.0 / 203.0).abs() < f32::EPSILON);
            assert!((scrgb_to_ultra_hdr_linear(203.0 / 80.0) - 1.0).abs() < f32::EPSILON);
        }
    }
}

#[cfg(windows)]
mod windows_hdr {
    use std::{
        sync::{
            Arc, OnceLock,
            mpsc::{self, RecvTimeoutError},
        },
        time::Duration,
    };

    use anyhow::{Context as _, Result, bail};
    use half::f16;
    use num_traits::ToPrimitive as _;
    use ultrahdr::{Encoder, ImgLabel, RawImage, sys};
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
        QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };
    use windows_capture::{
        capture::{Context, GraphicsCaptureApiHandler},
        frame::Frame,
        graphics_capture_api::InternalCaptureControl,
        monitor::Monitor,
        settings::{
            ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
            MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
        },
    };

    use crate::upload::UploadPayload;

    const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
    #[derive(Debug)]
    pub(super) struct HdrFrame {
        pub(super) width: u32,
        pub(super) height: u32,
        rgba_f16: Vec<u8>,
        advanced_color_enabled: bool,
        sdr_channel_lut: Arc<OnceLock<Box<[u8]>>>,
    }

    impl HdrFrame {
        fn new(width: u32, height: u32, rgba_f16: Vec<u8>, advanced_color_enabled: bool) -> Self {
            Self {
                width,
                height,
                rgba_f16,
                advanced_color_enabled,
                sdr_channel_lut: Arc::new(OnceLock::new()),
            }
        }

        pub(super) fn release_pixels(&mut self) {
            self.rgba_f16 = Vec::new();
        }
    }

    struct CaptureOnce {
        sender: mpsc::Sender<HdrFrame>,
    }

    impl GraphicsCaptureApiHandler for CaptureOnce {
        type Flags = mpsc::Sender<HdrFrame>;
        type Error = anyhow::Error;

        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self { sender: ctx.flags })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            let width = frame.width();
            let height = frame.height();
            let mut buffer = frame.buffer().context("failed to map HDR capture frame")?;
            let mut packed = Vec::new();
            let rgba_f16 = if buffer.has_padding() {
                let _ = buffer.as_nopadding_buffer(&mut packed);

                packed
            } else {
                buffer.as_raw_buffer().to_vec()
            };
            let _ = self
                .sender
                .send(HdrFrame::new(width, height, rgba_f16, false));

            capture_control.stop();
            Ok(())
        }
    }

    pub(super) fn capture_primary(color_mode: super::CaptureColorMode) -> Result<UploadPayload> {
        let frame = capture_frame()?;

        encode_automatic(frame, "screenshot", color_mode)
    }

    pub(super) fn encode_region(
        frame: &HdrFrame,
        bounds: super::CropBounds,
    ) -> Result<UploadPayload> {
        let frame = crop_frame(frame, bounds);

        encode_automatic(frame, "region", super::CaptureColorMode::Automatic)
    }

    pub(super) fn capture_frame() -> Result<HdrFrame> {
        let monitor = Monitor::primary().context("failed to find the primary display")?;
        let (sender, receiver) = mpsc::channel();
        let advanced_color_enabled = advanced_color_enabled(&monitor).unwrap_or(false);
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba16F,
            sender,
        );

        let capture = CaptureOnce::start_free_threaded(settings)
            .context("Windows HDR capture failed to start")?;

        let mut frame = match receiver.recv_timeout(FRAME_TIMEOUT) {
            Ok(frame) => {
                capture
                    .wait()
                    .context("Windows HDR capture thread failed")?;
                frame
            }

            Err(RecvTimeoutError::Timeout) => {
                capture
                    .stop()
                    .context("failed to stop timed-out Windows HDR capture")?;
                bail!("Windows HDR capture timed out");
            }

            Err(RecvTimeoutError::Disconnected) => {
                capture
                    .wait()
                    .context("Windows HDR capture thread failed")?;
                bail!("HDR capture returned no frame");
            }
        };

        frame.advanced_color_enabled = advanced_color_enabled;
        Ok(frame)
    }

    fn has_extended_range(frame: &HdrFrame) -> bool {
        frame.rgba_f16.chunks_exact(8).any(|pixel| {
            pixel[..6].chunks_exact(2).any(|channel| {
                let value = f16::from_le_bytes([channel[0], channel[1]]).to_f32();

                value.is_finite() && value > 1.001
            })
        })
    }

    fn encode_automatic(
        frame: HdrFrame,
        filename_stem: &str,
        color_mode: super::CaptureColorMode,
    ) -> Result<UploadPayload> {
        if color_mode == super::CaptureColorMode::Automatic
            && frame.advanced_color_enabled
            && has_extended_range(&frame)
        {
            let bytes = encode_ultra_hdr(frame)?;

            Ok(UploadPayload::from_bytes(
                bytes,
                format!("{filename_stem}-hdr.jpg"),
                "image/jpeg".to_owned(),
            ))
        } else {
            let image = sdr_image(&frame)?;

            super::encode_sdr_png(image, &format!("{filename_stem}.png"))
        }
    }

    pub(super) fn sdr_image(frame: &HdrFrame) -> Result<image::RgbaImage> {
        let expected_len = frame.width as usize * frame.height as usize * 8;

        anyhow::ensure!(
            frame.rgba_f16.len() == expected_len,
            "invalid SDR frame buffer length"
        );
        let pixels = sdr_rgba8(frame)?;

        image::RgbaImage::from_raw(frame.width, frame.height, pixels)
            .context("invalid SDR capture dimensions")
    }

    fn sdr_rgba8(frame: &HdrFrame) -> Result<Vec<u8>> {
        let expected_len = frame.width as usize * frame.height as usize * 8;

        anyhow::ensure!(
            frame.rgba_f16.len() == expected_len,
            "invalid SDR frame buffer length"
        );
        let mut pixels = Vec::with_capacity(frame.width as usize * frame.height as usize * 4);
        let channel_lut = frame.sdr_channel_lut.get_or_init(build_sdr_channel_lut);

        for pixel in frame.rgba_f16.chunks_exact(8) {
            let linear = std::array::from_fn(|index| {
                let offset = index * 2;

                f16::from_le_bytes([pixel[offset], pixel[offset + 1]]).to_f32()
            });

            for channel in tone_map_scrgb(linear) {
                let index = usize::from(f16::from_f32(channel).to_bits());

                pixels.push(channel_lut[index]);
            }

            pixels.push(255);
        }

        Ok(pixels)
    }

    fn tone_map_scrgb(linear: [f32; 3]) -> [f32; 3] {
        const KNEE: f32 = 0.8;
        const SHOULDER: f32 = 0.4;

        let linear = linear.map(|channel| {
            if channel.is_finite() {
                channel.max(0.0)
            } else {
                0.0
            }
        });
        let peak = linear.into_iter().fold(0.0_f32, f32::max);

        if peak <= KNEE {
            return linear;
        }

        let distance = peak - KNEE;
        let mapped_peak = KNEE + (1.0 - KNEE) * distance / (distance + SHOULDER);
        let scale = mapped_peak / peak;

        linear.map(|channel| channel * scale)
    }

    fn build_sdr_channel_lut() -> Box<[u8]> {
        (0..=u16::MAX)
            .map(|bits| {
                let linear = f16::from_bits(bits).to_f32();
                let finite = if linear.is_nan() { 0.0 } else { linear };

                (linear_to_srgb(finite.clamp(0.0, 1.0)) * 255.0)
                    .round()
                    .to_u8()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }

    fn crop_frame(frame: &HdrFrame, bounds: super::CropBounds) -> HdrFrame {
        let stride = frame.width as usize * 8;
        let row_bytes = bounds.width as usize * 8;
        let mut rgba_f16 = Vec::with_capacity(row_bytes * bounds.height as usize);

        for row in bounds.y..bounds.y + bounds.height {
            let start = row as usize * stride + bounds.x as usize * 8;
            let end = start + row_bytes;

            rgba_f16.extend_from_slice(&frame.rgba_f16[start..end]);
        }

        HdrFrame {
            width: bounds.width,
            height: bounds.height,
            rgba_f16,
            advanced_color_enabled: frame.advanced_color_enabled,
            sdr_channel_lut: Arc::clone(&frame.sdr_channel_lut),
        }
    }

    fn encode_ultra_hdr(frame: HdrFrame) -> Result<Vec<u8>> {
        let expected_len = frame.width as usize * frame.height as usize * 8;

        anyhow::ensure!(
            frame.rgba_f16.len() == expected_len,
            "invalid HDR frame buffer length"
        );

        let mut sdr_pixels = sdr_rgba8(&frame)?;
        let mut pixels = frame.rgba_f16;
        let mut peak_linear = 1.0_f32;

        for pixel in pixels.chunks_exact_mut(8) {
            for channel in pixel[..6].chunks_exact_mut(2) {
                let value = f16::from_le_bytes([channel[0], channel[1]]).to_f32();
                let normalized = if value.is_finite() {
                    super::scrgb_to_ultra_hdr_linear(value.max(0.0))
                } else {
                    0.0
                };

                peak_linear = peak_linear.max(normalized);
                channel.copy_from_slice(&f16::from_f32(normalized).to_le_bytes());
            }
        }

        let mut raw = RawImage::packed(
            sys::uhdr_img_fmt::UHDR_IMG_FMT_64bppRGBAHalfFloat,
            frame.width,
            frame.height,
            &mut pixels,
            sys::uhdr_color_gamut::UHDR_CG_BT_709,
            sys::uhdr_color_transfer::UHDR_CT_LINEAR,
            sys::uhdr_color_range::UHDR_CR_FULL_RANGE,
        )
        .context("failed to describe the HDR frame")?;
        let mut sdr = RawImage::rgba8888(
            frame.width,
            frame.height,
            &mut sdr_pixels,
            sys::uhdr_color_gamut::UHDR_CG_BT_709,
            sys::uhdr_color_transfer::UHDR_CT_SRGB,
            sys::uhdr_color_range::UHDR_CR_FULL_RANGE,
        )
        .context("failed to describe the sharp SDR rendition")?;
        let mut encoder = Encoder::new().context("failed to create the Ultra HDR encoder")?;

        encoder
            .set_raw_image(&mut raw, ImgLabel::UHDR_HDR_IMG)
            .context("failed to provide the HDR frame")?;
        encoder
            .set_raw_image(&mut sdr, ImgLabel::UHDR_SDR_IMG)
            .context("failed to provide the SDR rendition")?;
        encoder
            .set_quality(100, ImgLabel::UHDR_BASE_IMG)
            .context("failed to configure base-image quality")?;
        encoder
            .set_quality(100, ImgLabel::UHDR_GAIN_MAP_IMG)
            .context("failed to configure gain-map quality")?;
        encoder
            .set_gainmap_scale_factor(1)
            .context("failed to configure gain-map size")?;
        encoder
            .set_using_multi_channel_gainmap(true)
            .context("failed to configure the gain map")?;
        encoder
            .set_target_display_peak_brightness((peak_linear * 203.0).clamp(203.0, 10_000.0))
            .context("failed to configure HDR peak brightness")?;
        encoder
            .set_output_format(sys::uhdr_codec::UHDR_CODEC_JPG)
            .context("failed to configure Ultra HDR JPEG output")?;
        encoder
            .set_preset(sys::uhdr_enc_preset::UHDR_USAGE_BEST_QUALITY)
            .context("failed to configure HDR encoding quality")?;
        encoder
            .encode()
            .context("failed to encode backwards-compatible Ultra HDR JPEG")?;

        encoder
            .encoded_stream()
            .context("Ultra HDR encoder returned no image")?
            .bytes()
            .map(<[u8]>::to_vec)
            .context("failed to read the encoded Ultra HDR JPEG")
    }

    #[expect(
        unsafe_code,
        reason = "Windows exposes Advanced Color state through pointer-based DisplayConfig APIs"
    )]
    fn advanced_color_enabled(monitor: &Monitor) -> Result<bool> {
        let device_name = monitor
            .device_name()
            .context("failed to identify the captured display")?;
        let mut path_count = 0;
        let mut mode_count = 0;

        // SAFETY: The API only writes the two initialized counts.
        unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
                .ok()
                .context("failed to size the active display configuration")?;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];

        // SAFETY: Buffers match the capacities returned by Windows.
        unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
            .ok()
            .context("failed to query the active display configuration")?;
        }

        for path in paths.iter().take(path_count as usize) {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                viewGdiDeviceName: [0; 32],
            };

            // SAFETY: The header identifies the initialized output buffer.
            if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
                continue;
            }

            let source_name = String::from_utf16_lossy(
                source
                    .viewGdiDeviceName
                    .iter()
                    .take_while(|character| **character != 0)
                    .copied()
                    .collect::<Vec<_>>()
                    .as_slice(),
            );

            if source_name != device_name {
                continue;
            }

            let mut color = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                    size: std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };

            // SAFETY: The header identifies the initialized output buffer.
            let flags = unsafe {
                (DisplayConfigGetDeviceInfo(&mut color.header) == 0).then(|| color.Anonymous.value)
            };

            if let Some(flags) = flags {
                return Ok(flags & 0b10 != 0);
            }
        }

        Ok(false)
    }

    fn linear_to_srgb(linear: f32) -> f32 {
        if linear <= 0.003_130_8 {
            linear * 12.92
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use ultrahdr::{CompressedImage, Decoder};

        fn solid_frame(width: u32, height: u32, value: f32, hdr: bool) -> HdrFrame {
            let mut rgba_f16 = Vec::with_capacity(width as usize * height as usize * 8);

            for _ in 0..width * height {
                for channel in [value, value, value, 1.0] {
                    rgba_f16.extend_from_slice(&f16::from_f32(channel).to_le_bytes());
                }
            }

            HdrFrame::new(width, height, rgba_f16, hdr)
        }

        fn compressed_image(encoded: &mut [u8]) -> CompressedImage<'_> {
            CompressedImage::from_bytes(
                encoded,
                sys::uhdr_color_gamut::UHDR_CG_UNSPECIFIED,
                sys::uhdr_color_transfer::UHDR_CT_UNSPECIFIED,
                sys::uhdr_color_range::UHDR_CR_UNSPECIFIED,
            )
        }

        fn jpeg_frame_dimensions(bytes: &[u8]) -> Vec<(u16, u16)> {
            bytes
                .windows(9)
                .filter(|window| {
                    window[0] == 0xff
                        && matches!(
                            window[1],
                            0xc0 | 0xc1
                                | 0xc2
                                | 0xc3
                                | 0xc5
                                | 0xc6
                                | 0xc7
                                | 0xc9
                                | 0xca
                                | 0xcb
                                | 0xcd
                                | 0xce
                                | 0xcf
                        )
                })
                .map(|window| {
                    (
                        u16::from_be_bytes([window[7], window[8]]),
                        u16::from_be_bytes([window[5], window[6]]),
                    )
                })
                .collect()
        }

        mod sdr {
            use super::*;

            #[test]
            fn forced_sdr_uses_native_resolution_png_for_an_hdr_frame() {
                let frame = solid_frame(17, 9, 5.0, true);
                let payload =
                    encode_automatic(frame, "capture", super::super::super::CaptureColorMode::Sdr)
                        .unwrap();
                let decoded = image::load_from_memory(payload.bytes().unwrap()).unwrap();

                assert_eq!(payload.filename, "capture.png");
                assert_eq!(payload.mime, "image/png");
                assert_eq!((decoded.width(), decoded.height()), (17, 9));
            }

            #[test]
            fn tone_mapping_is_stable_for_black_white_and_invalid_channels() {
                let mut bytes = Vec::new();

                for channel in [0.0, 1.0, f32::NAN, 1.0] {
                    bytes.extend_from_slice(&f16::from_f32(channel).to_le_bytes());
                }

                let pixels = sdr_rgba8(&HdrFrame::new(1, 1, bytes, false)).unwrap();

                assert_eq!(pixels[0], 0);
                assert!(pixels[1] >= 235);
                assert_eq!(pixels[2], 0);
                assert_eq!(pixels[3], 255);
            }

            #[test]
            fn tone_mapping_retains_highlight_steps_and_color_ratios() {
                let lower = tone_map_scrgb([2.0, 1.0, 0.5]);
                let higher = tone_map_scrgb([5.0, 2.5, 1.25]);

                assert!(lower[0] < higher[0]);
                assert!(lower[0] < 1.0 && higher[0] < 1.0);
                assert!((lower[1] / lower[0] - 0.5).abs() < f32::EPSILON);
                assert!((higher[2] / higher[0] - 0.25).abs() < f32::EPSILON);
            }
        }

        mod hdr {
            use super::*;

            #[test]
            fn detection_requires_advanced_color_and_extended_pixels() {
                assert!(!has_extended_range(&solid_frame(2, 2, 1.0, true)));
                assert!(has_extended_range(&solid_frame(2, 2, 1.01, false)));

                let payload = encode_automatic(
                    solid_frame(2, 2, 1.01, false),
                    "capture",
                    super::super::super::CaptureColorMode::Automatic,
                )
                .unwrap();

                assert_eq!(payload.filename, "capture.png");
            }

            #[test]
            fn ultra_hdr_round_trip_retains_extended_luminance() {
                let mut encoded = encode_ultra_hdr(solid_frame(16, 16, 5.0, true)).unwrap();
                let mut compressed = compressed_image(&mut encoded);
                let mut decoder = Decoder::new().unwrap();

                decoder.set_image(&mut compressed).unwrap();
                let view = decoder
                    .decode_packed_view(
                        sys::uhdr_img_fmt::UHDR_IMG_FMT_64bppRGBAHalfFloat,
                        sys::uhdr_color_transfer::UHDR_CT_LINEAR,
                    )
                    .unwrap();
                let channel = view.row(0).unwrap();
                let decoded = f16::from_le_bytes([channel[0], channel[1]]).to_f32();

                assert!(decoded > 1.3, "decoded linear HDR value was {decoded}");
            }

            #[test]
            fn sdr_base_stays_sharp_and_gain_map_is_full_resolution() {
                let width = 32;
                let height = 16;
                let mut rgba_f16 = Vec::with_capacity(width * height * 8);

                for _y in 0..height {
                    for x in 0..width {
                        let value = if x % 2 == 0 { 0.0 } else { 1.0 };

                        for channel in [value, value, value, 1.0] {
                            rgba_f16.extend_from_slice(&f16::from_f32(channel).to_le_bytes());
                        }
                    }
                }

                let mut encoded =
                    encode_ultra_hdr(HdrFrame::new(width as u32, height as u32, rgba_f16, true))
                        .unwrap();
                let dimensions = jpeg_frame_dimensions(&encoded);

                assert!(
                    dimensions.iter().filter(|size| **size == (32, 16)).count() >= 2,
                    "base and gain map dimensions were {dimensions:?}"
                );

                let mut compressed = compressed_image(&mut encoded);
                let mut decoder = Decoder::new().unwrap();

                decoder.set_image(&mut compressed).unwrap();
                decoder.set_out_max_display_boost(1.0).unwrap();
                let view = decoder
                    .decode_packed_view(
                        sys::uhdr_img_fmt::UHDR_IMG_FMT_32bppRGBA8888,
                        sys::uhdr_color_transfer::UHDR_CT_SRGB,
                    )
                    .unwrap();
                let row = view.row(8).unwrap();
                let contrast = row
                    .chunks_exact(4)
                    .map(|pixel| i16::from(pixel[0]))
                    .collect::<Vec<_>>()
                    .windows(2)
                    .map(|pair| (pair[1] - pair[0]).unsigned_abs())
                    .min()
                    .unwrap();

                assert!(contrast > 180, "minimum one-pixel contrast was {contrast}");
            }

            #[test]
            fn ultra_hdr_contains_only_structural_metadata() {
                let encoded = encode_ultra_hdr(solid_frame(16, 16, 5.0, true)).unwrap();

                assert!(!encoded.windows(6).any(|window| window == b"Exif\0\0"));
                assert!(encoded.windows(3).any(|window| window == b"xmp"));
                assert!(
                    !encoded
                        .windows("private-window-title".len())
                        .any(|window| window == b"private-window-title")
                );
            }
        }
    }
}
