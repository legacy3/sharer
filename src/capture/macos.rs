//! ScreenCaptureKit-backed capture for macOS.

use std::{
    fmt,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use anyhow::{Context as _, Result};
use core_graphics::window::{
    create_window_list, kCGNullWindowID, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly,
};
use image::RgbaImage;
use num_traits::ToPrimitive as _;
use screencapturekit::{
    cm::SCFrameStatus,
    prelude::*,
    screenshot_manager::{CGImage, CGImageExt, SCScreenshotManager},
    shareable_content::SCShareableContentInfo,
    stream::configuration::SCCaptureResolutionType,
};

use super::{
    CaptureColorMode, CapturedScreen, DesktopBounds, DesktopRegion, WindowRegion, encode_sdr_png,
    intersect_bounds, resize_rgba,
};
use crate::config::{CaptureResolution, ResizeQuality};
use crate::upload::UploadPayload;

const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

struct PreparedDisplay {
    filter: SCContentFilter,
    desktop_bounds: DesktopBounds,
    pixel_size: (u32, u32),
    window_regions: Vec<WindowRegion>,
}

#[derive(Default)]
struct FrameState {
    frame: Option<CMSampleBuffer>,
    error: Option<String>,
}

#[derive(Default)]
struct FrameMailbox {
    state: Mutex<FrameState>,
    ready: Condvar,
}

impl FrameMailbox {
    fn replace_frame(&self, frame: CMSampleBuffer) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        state.frame = Some(frame);
        self.ready.notify_one();
    }

    fn set_error(&self, error: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        state.error = Some(error);
        self.ready.notify_all();
    }

    fn next_frame(&self) -> Result<CMSampleBuffer> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut state, timed_out) = self
            .ready
            .wait_timeout_while(state, FRAME_TIMEOUT, |state| {
                state.frame.is_none() && state.error.is_none()
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(error) = state.error.take() {
            anyhow::bail!("ScreenCaptureKit stream stopped: {error}");
        }

        if timed_out.timed_out() {
            anyhow::bail!("timed out waiting for a ScreenCaptureKit frame");
        }

        state
            .frame
            .take()
            .context("ScreenCaptureKit signalled without providing a frame")
    }
}

/// Persistent native stream used by animated region recording.
pub struct RegionCapturer {
    stream: SCStream,
    mailbox: Arc<FrameMailbox>,
    width: u32,
    height: u32,
}

impl RegionCapturer {
    /// Start a bounded ScreenCaptureKit stream for the selected region.
    pub fn new(region: DesktopRegion, capture_resolution: CaptureResolution) -> Result<Self> {
        let prepared = prepare_display_containing(region)?;
        let (width, height) = match capture_resolution {
            CaptureResolution::Native => (
                scaled_dimension(
                    region.width(),
                    prepared.pixel_size.0,
                    prepared.desktop_bounds.width,
                )?,
                scaled_dimension(
                    region.height(),
                    prepared.pixel_size.1,
                    prepared.desktop_bounds.height,
                )?,
            ),

            CaptureResolution::Logical => (region.width(), region.height()),
        };
        let local_x = region.x().saturating_sub(prepared.desktop_bounds.x);
        let local_y = region.y().saturating_sub(prepared.desktop_bounds.y);
        let source_rect = CGRect::new(
            f64::from(local_x),
            f64::from(local_y),
            f64::from(region.width()),
            f64::from(region.height()),
        );
        let configuration = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_source_rect(source_rect)
            .with_pixel_format(PixelFormat::BGRA)
            .with_capture_resolution_type(SCCaptureResolutionType::Best)
            .with_shows_cursor(true)
            .with_queue_depth(2);
        let mailbox = Arc::new(FrameMailbox::default());
        let error_mailbox = Arc::clone(&mailbox);
        let delegate = ErrorHandler::new(move |error| error_mailbox.set_error(error.to_string()));
        let mut stream = SCStream::new_with_delegate(&prepared.filter, &configuration, delegate);
        let frame_mailbox = Arc::clone(&mailbox);
        let handler = stream.add_output_handler(
            move |sample: CMSampleBuffer, output_type: SCStreamOutputType| {
                if output_type == SCStreamOutputType::Screen
                    && sample.is_valid()
                    && sample.is_data_ready()
                    && sample.frame_status().is_none_or(SCFrameStatus::has_content)
                {
                    frame_mailbox.replace_frame(sample);
                }
            },
            SCStreamOutputType::Screen,
        );

        anyhow::ensure!(
            handler.is_some(),
            "ScreenCaptureKit rejected the frame handler"
        );
        stream
            .start_capture()
            .context("failed to start the ScreenCaptureKit recording stream")?;

        Ok(Self {
            stream,
            mailbox,
            width,
            height,
        })
    }

    /// Return the newest available stream frame as tightly packed RGBA pixels.
    pub fn capture_rgba(&self) -> Result<Vec<u8>> {
        sample_buffer_rgba(self.mailbox.next_frame()?, self.width, self.height)
    }

    /// Return the physical pixel dimensions emitted by this capture source.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl fmt::Debug for RegionCapturer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegionCapturer")
            .field("backend", &"ScreenCaptureKit")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Drop for RegionCapturer {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}

pub(super) fn capture_primary(
    color_mode: CaptureColorMode,
    capture_resolution: CaptureResolution,
    resize_quality: ResizeQuality,
) -> Result<UploadPayload> {
    let _ = color_mode;
    let prepared = prepare_primary_display()?;
    let image = capture_image(&prepared)?;
    let image = if capture_resolution == CaptureResolution::Logical {
        resize_rgba(
            &image,
            prepared.desktop_bounds.width,
            prepared.desktop_bounds.height,
            resize_quality,
        )
    } else {
        image
    };

    encode_sdr_png(image, "screenshot.png")
}

pub(super) fn capture_region_source(
    color_mode: CaptureColorMode,
    capture_resolution: CaptureResolution,
) -> Result<CapturedScreen> {
    let _ = color_mode;
    let _ = capture_resolution;
    let prepared = prepare_primary_display()?;
    let preview = capture_image(&prepared)?;

    Ok(CapturedScreen {
        preview,
        desktop_bounds: prepared.desktop_bounds,
        window_regions: prepared.window_regions,
    })
}

pub(super) fn active_window_title() -> Option<String> {
    let own_pid = std::process::id();

    SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .ok()?
        .windows()
        .into_iter()
        .find(|window| {
            window.is_active()
                && window.window_layer() == 0
                && window.owning_application().is_none_or(|application| {
                    u32::try_from(application.process_id()).ok() != Some(own_pid)
                })
        })?
        .title()
}

fn capture_image(prepared: &PreparedDisplay) -> Result<RgbaImage> {
    let configuration = SCStreamConfiguration::new()
        .with_width(prepared.pixel_size.0)
        .with_height(prepared.pixel_size.1)
        .with_pixel_format(PixelFormat::BGRA)
        .with_capture_resolution_type(SCCaptureResolutionType::Best)
        .with_shows_cursor(true);
    let image = SCScreenshotManager::capture_image(&prepared.filter, &configuration)
        .context("failed to capture the display with ScreenCaptureKit")?;

    rgba_image(image)
}

fn rgba_image(image: CGImage) -> Result<RgbaImage> {
    let width = u32::try_from(image.width()).context("captured display is too wide")?;
    let height = u32::try_from(image.height()).context("captured display is too tall")?;
    let pixels = image
        .rgba_data()
        .context("failed to read ScreenCaptureKit screenshot pixels")?;

    RgbaImage::from_raw(width, height, pixels)
        .context("ScreenCaptureKit returned an invalid RGBA screenshot")
}

fn prepare_primary_display() -> Result<PreparedDisplay> {
    prepare_display(|display| {
        let frame = display.frame();

        frame.origin.x.abs() + frame.origin.y.abs()
    })
}

fn prepare_display_containing(region: DesktopRegion) -> Result<PreparedDisplay> {
    prepare_display(|display| {
        let Some(bounds) = desktop_bounds(display) else {
            return f64::INFINITY;
        };
        let region_right = i64::from(region.x()) + i64::from(region.width());
        let region_bottom = i64::from(region.y()) + i64::from(region.height());
        let display_right = i64::from(bounds.x) + i64::from(bounds.width);
        let display_bottom = i64::from(bounds.y) + i64::from(bounds.height);

        if region.x() >= bounds.x
            && region.y() >= bounds.y
            && region_right <= display_right
            && region_bottom <= display_bottom
        {
            0.0
        } else {
            f64::INFINITY
        }
    })
}

fn prepare_display(score: impl Fn(&SCDisplay) -> f64) -> Result<PreparedDisplay> {
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .context("failed to enumerate ScreenCaptureKit content; allow Screen Recording access")?;
    let display = content
        .displays()
        .into_iter()
        .min_by(|left, right| score(left).total_cmp(&score(right)))
        .context("ScreenCaptureKit returned no displays")?;

    anyhow::ensure!(
        score(&display).is_finite(),
        "capture region crosses a display boundary"
    );
    let desktop_bounds = desktop_bounds(&display).context("display has invalid bounds")?;
    let window_regions = visible_window_regions(&content, desktop_bounds);
    let own_apps = content
        .applications()
        .into_iter()
        .filter(|application| {
            u32::try_from(application.process_id()).ok() == Some(std::process::id())
        })
        .collect::<Vec<_>>();
    let own_app_refs = own_apps.iter().collect::<Vec<_>>();
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_applications(&own_app_refs, &[])
        .try_build()
        .context("failed to configure ScreenCaptureKit display capture")?;
    let pixel_size = SCShareableContentInfo::for_filter(&filter)
        .map(|info| info.pixel_size())
        .unwrap_or_else(|| (display.width(), display.height()));

    anyhow::ensure!(
        pixel_size.0 > 0 && pixel_size.1 > 0,
        "display has no capturable pixels"
    );

    Ok(PreparedDisplay {
        filter,
        desktop_bounds,
        pixel_size,
        window_regions,
    })
}

fn desktop_bounds(display: &SCDisplay) -> Option<DesktopBounds> {
    bounds_from_rect(display.frame())
}

fn bounds_from_rect(rect: CGRect) -> Option<DesktopBounds> {
    let x = rect.origin.x.round().to_i32()?;
    let y = rect.origin.y.round().to_i32()?;
    let width = rect.size.width.round().to_u32()?;
    let height = rect.size.height.round().to_u32()?;

    (width > 0 && height > 0).then_some(DesktopBounds {
        x,
        y,
        width,
        height,
    })
}

fn scaled_dimension(points: u32, display_pixels: u32, display_points: u32) -> Result<u32> {
    anyhow::ensure!(display_points > 0, "display has no logical extent");
    let scaled = u64::from(points)
        .saturating_mul(u64::from(display_pixels))
        .div_ceil(u64::from(display_points));
    let scaled = u32::try_from(scaled).context("capture region is too large")?;

    anyhow::ensure!(scaled > 0, "capture region has no physical pixels");
    Ok(scaled)
}

fn visible_window_regions(
    content: &SCShareableContent,
    display_bounds: DesktopBounds,
) -> Vec<WindowRegion> {
    let Some(display_width) = display_bounds.width.to_f32() else {
        return Vec::new();
    };
    let Some(display_height) = display_bounds.height.to_f32() else {
        return Vec::new();
    };
    let own_pid = std::process::id();

    ordered_windows(content)
        .into_iter()
        .filter(|window| {
            window.is_on_screen()
                && window.window_layer() == 0
                && window.owning_application().is_none_or(|application| {
                    u32::try_from(application.process_id()).ok() != Some(own_pid)
                })
        })
        .filter_map(|window| {
            let title = window.title()?.trim().to_owned();

            (!title.is_empty()).then_some((window, title))
        })
        .filter_map(|(window, title)| {
            intersect_bounds(bounds_from_rect(window.frame())?, display_bounds)
                .map(|bounds| (bounds, title))
        })
        .filter_map(|(bounds, title)| {
            let x = (bounds.x - display_bounds.x).to_f32()? / display_width;
            let y = (bounds.y - display_bounds.y).to_f32()? / display_height;
            let width = bounds.width.to_f32()? / display_width;
            let height = bounds.height.to_f32()? / display_height;

            (width > 0.02 && height > 0.02).then_some(WindowRegion {
                bounds: [x, y, width, height],
                title,
            })
        })
        .collect()
}

fn ordered_windows(content: &SCShareableContent) -> Vec<SCWindow> {
    let mut windows = content.windows();
    let Some(window_ids) = create_window_list(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return windows;
    };
    let ranks = window_ids
        .iter()
        .enumerate()
        .map(|(rank, window_id)| (*window_id, rank))
        .collect::<std::collections::BTreeMap<_, _>>();

    windows.sort_by_key(|window| {
        ranks
            .get(&window.window_id())
            .copied()
            .unwrap_or(usize::MAX)
    });
    windows
}

fn sample_buffer_rgba(sample: CMSampleBuffer, width: u32, height: u32) -> Result<Vec<u8>> {
    let pixel_buffer = sample
        .image_buffer()
        .context("ScreenCaptureKit frame contains no pixel buffer")?;
    let guard = pixel_buffer
        .lock_read_only()
        .map_err(|code| anyhow::anyhow!("failed to lock ScreenCaptureKit pixels ({code})"))?;

    anyhow::ensure!(
        guard.width() == usize::try_from(width)? && guard.height() == usize::try_from(height)?,
        "ScreenCaptureKit returned an unexpected frame size"
    );
    let row_bytes = usize::try_from(width)?.saturating_mul(4);

    anyhow::ensure!(
        guard.bytes_per_row() >= row_bytes,
        "ScreenCaptureKit frame stride is invalid"
    );
    let source = guard.as_slice();
    let capacity = usize::try_from(height)?.saturating_mul(row_bytes);
    let mut rgba = Vec::with_capacity(capacity);

    for row in source
        .chunks_exact(guard.bytes_per_row())
        .take(usize::try_from(height)?)
    {
        for pixel in row[..row_bytes].chunks_exact(4) {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }

    anyhow::ensure!(
        rgba.len() == capacity,
        "ScreenCaptureKit frame data is incomplete"
    );
    Ok(rgba)
}
