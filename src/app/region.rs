//! Interactive region selection and recording controls.

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        mpsc::{self, Sender},
    },
    time::Duration,
};

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
use anyhow::{Context as _, Result};
use num_traits::ToPrimitive as _;
use slint::ComponentHandle as _;
use slint::winit_030::WinitWindowAccessor as _;

use crate::{AppWindow, RegionWindow, views};
#[cfg(target_os = "macos")]
use sharer::upload::UploadPayload;
use sharer::{
    capture,
    config::{CaptureResolution, RecordingFormat, ResizeQuality},
    upload::UploadCancellation,
};

use super::{
    CAPTURE_HIDE_DELAY, FailureStage, Job, JobSource, RecordingControl, UploadOptions,
    begin_artifact_attempt, present_error, upload_status_detail, valid_window_lifetime,
    valid_window_upload_target,
};

#[derive(Clone, Debug)]
struct RegionUpload {
    upload: Option<UploadOptions>,
    filename_stem: String,
    use_snapped_window_name: bool,
    recording_fps: u32,
    recording_max_seconds: u32,
    recording_max_gib: u32,
    recording_format: RecordingFormat,
    capture_resolution: CaptureResolution,
    resize_quality: ResizeQuality,
    save_directory: Option<PathBuf>,
    remove_exif: bool,
    restore_window: bool,
    purpose: RegionPurpose,
    cancellation: Arc<UploadCancellation>,
}

#[derive(Clone, Debug)]
enum RegionPurpose {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Screenshot,
    Recording(RecordingControl),
}

#[cfg(not(target_os = "macos"))]
pub(super) fn start_screenshot(
    window: &AppWindow,
    sender: Sender<Job>,
    cancellation: Arc<UploadCancellation>,
) {
    begin_region_selection(window, sender, RegionPurpose::Screenshot, cancellation);
}

#[cfg(target_os = "macos")]
pub(super) fn start_screenshot(
    window: &AppWindow,
    sender: Sender<Job>,
    cancellation: Arc<UploadCancellation>,
) {
    if window.get_busy() {
        return;
    }

    if window.get_auto_upload_captures() {
        if valid_window_lifetime(window).is_none() || valid_window_upload_target(window).is_none() {
            return;
        }
    } else if !window.get_save_captures() {
        present_error(
            window,
            FailureStage::Settings,
            super::CAPTURE_OUTPUT_REQUIRED,
        );

        return;
    }

    let settings = (|| {
        let naming = views::settings::naming_from_window(window)?;
        let filename_stem = crate::naming::screenshot_stem(&naming)?;
        let save_directory = views::settings::configured_capture_directory(window)?;

        Ok::<_, anyhow::Error>((filename_stem, save_directory))
    })();
    let (filename_stem, save_directory) = match settings {
        Ok(settings) => settings,

        Err(error) => {
            present_error(window, FailureStage::Settings, &format!("{error:#}"));

            return;
        }
    };
    let restore_window = window.window().is_visible() && !window.window().is_minimized();

    begin_selection_activity(window, "Drag to capture a region, or press Esc to cancel");
    let _ = window.hide();
    let main_window = window.as_weak();

    slint::Timer::single_shot(CAPTURE_HIDE_DELAY, move || {
        let event_window = main_window.clone();
        let event_sender = sender.clone();
        let event_cancellation = Arc::clone(&cancellation);
        let spawn_result = std::thread::Builder::new()
            .name("sharer-macos-capture".to_owned())
            .spawn(move || {
                let result = native_macos_screenshot();

                let _ = slint::invoke_from_event_loop(move || {
                    let Some(window) = event_window.upgrade() else {
                        return;
                    };

                    match result {
                        Ok(Some(mut payload)) => {
                            payload.filename =
                                capture::named_capture_filename(&filename_stem, &payload.filename);
                            window.set_busy(false);
                            restore_window_if_needed(&window, restore_window);
                            super::queue_capture_job(
                                &window,
                                &event_sender,
                                JobSource::Prepared {
                                    payload,
                                    save_directory,
                                },
                                event_cancellation,
                            );
                        }

                        Ok(None) => {
                            cancel_region_selection(&window, restore_window);
                        }

                        Err(error) => {
                            restore_window_if_needed(&window, restore_window);
                            present_error(&window, FailureStage::Capture, &format!("{error:#}"));
                        }
                    }
                });
            });

        if let Err(error) = spawn_result {
            if let Some(window) = main_window.upgrade() {
                restore_window_if_needed(&window, restore_window);
                present_error(
                    &window,
                    FailureStage::Capture,
                    &format!("failed to start the macOS capture tool: {error}"),
                );
            }
        }
    });
}

#[cfg(target_os = "macos")]
fn native_macos_screenshot() -> Result<Option<UploadPayload>> {
    let directory = tempfile::Builder::new()
        .prefix("sharer-capture-")
        .tempdir()
        .context("failed to create a temporary capture directory")?;
    let path = directory.path().join("capture.png");
    let output = Command::new("/usr/sbin/screencapture")
        .args(["-i", "-s", "-x", "-t", "png"])
        .arg(&path)
        .output()
        .context("failed to launch the macOS capture tool")?;

    if path.is_file() {
        let payload = UploadPayload::from_generated_path(&path)
            .context("failed to read the screenshot produced by macOS")?;

        anyhow::ensure!(!payload.is_empty(), "macOS produced an empty screenshot");

        return Ok(Some(payload));
    }

    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();

    if message.is_empty() {
        Ok(None)
    } else {
        anyhow::bail!("macOS capture failed: {message}")
    }
}

fn recording_limits(window: &AppWindow, purpose: &RegionPurpose) -> Option<(u32, u32, u32)> {
    if matches!(purpose, RegionPurpose::Screenshot) {
        return Some((
            sharer::MIN_RECORDING_FPS,
            sharer::DEFAULT_RECORDING_SECONDS,
            sharer::DEFAULT_RECORDING_GIB,
        ));
    }

    let fps = match views::settings::validated_recording_fps(window) {
        Ok(fps) => fps,

        Err(error) => {
            present_error(window, FailureStage::Settings, &error.to_string());

            return None;
        }
    };
    let seconds = match views::settings::validated_recording_seconds(window) {
        Ok(seconds) => seconds,

        Err(error) => {
            present_error(window, FailureStage::Settings, &error.to_string());

            return None;
        }
    };
    let gib = match views::settings::validated_recording_gib(window) {
        Ok(gib) => gib,

        Err(error) => {
            present_error(window, FailureStage::Settings, &error.to_string());

            return None;
        }
    };

    Some((fps, seconds, gib))
}

fn begin_region_selection(
    window: &AppWindow,
    sender: Sender<Job>,
    purpose: RegionPurpose,
    cancellation: Arc<UploadCancellation>,
) {
    if window.get_busy() {
        return;
    }

    if !window.get_auto_upload_captures() && !window.get_save_captures() {
        present_error(
            window,
            FailureStage::Settings,
            super::CAPTURE_OUTPUT_REQUIRED,
        );

        return;
    }

    let upload = if window.get_auto_upload_captures() {
        let Some(lifetime_seconds) = valid_window_lifetime(window) else {
            return;
        };
        let Some(uploader) = valid_window_upload_target(window) else {
            return;
        };

        Some(UploadOptions {
            uploader,
            lifetime_seconds,
            require_tor: window.get_require_tor(),
            copy_link: window.get_copy_link_after_upload(),
        })
    } else {
        None
    };
    let Some((recording_fps, recording_max_seconds, recording_max_gib)) =
        recording_limits(window, &purpose)
    else {
        return;
    };
    let Ok(naming) = views::settings::naming_from_window(window) else {
        present_error(
            window,
            FailureStage::Settings,
            "Choose a valid screenshot name",
        );

        return;
    };
    let use_snapped_window_name = naming.mode == sharer::config::CaptureNameMode::ActiveWindow;
    let save_directory = match views::settings::configured_capture_directory(window) {
        Ok(directory) => directory,

        Err(error) => {
            present_error(window, FailureStage::Settings, &error.to_string());

            return;
        }
    };
    let color_mode = capture_color_mode(window);
    let capture_resolution = capture_resolution(window);
    let restore_window = window.window().is_visible() && !window.window().is_minimized();

    begin_selection_activity(window, "Drag across the preview  -  press Esc to cancel");
    let _ = window.hide();
    let main_window = window.as_weak();

    slint::Timer::single_shot(CAPTURE_HIDE_DELAY, move || {
        let capture = capture::capture_region_sources(color_mode, capture_resolution);
        let Some(window) = main_window.upgrade() else {
            return;
        };
        let filename_stem = match crate::naming::screenshot_stem(&naming) {
            Ok(stem) => stem,

            Err(error) => {
                restore_window_if_needed(&window, restore_window);

                present_error(&window, FailureStage::Settings, &error.to_string());

                return;
            }
        };

        match capture {
            Ok(screens) => show_region_selectors(
                &window,
                &sender,
                screens,
                &RegionUpload {
                    upload,
                    filename_stem,
                    use_snapped_window_name,
                    recording_fps,
                    recording_max_seconds,
                    recording_max_gib,
                    recording_format: views::settings::recording_format(&window),
                    capture_resolution,
                    resize_quality: resize_quality(&window),
                    save_directory,
                    remove_exif: window.get_remove_exif(),
                    restore_window,
                    purpose,
                    cancellation,
                },
            ),

            Err(error) => {
                restore_window_if_needed(&window, restore_window);

                present_error(&window, FailureStage::Capture, &format!("{error:#}"));
            }
        }
    });
}

pub(super) fn capture_color_mode(window: &AppWindow) -> capture::CaptureColorMode {
    if window.get_force_sdr_captures() {
        capture::CaptureColorMode::Sdr
    } else {
        capture::CaptureColorMode::Automatic
    }
}

pub(super) fn capture_resolution(window: &AppWindow) -> CaptureResolution {
    CaptureResolution::from_index(window.get_capture_resolution()).unwrap_or_default()
}

pub(super) fn resize_quality(window: &AppWindow) -> ResizeQuality {
    ResizeQuality::from_index(window.get_resize_quality()).unwrap_or_default()
}

pub(super) fn snap_changed(last: &Cell<Option<[f32; 4]>>, next: Option<[f32; 4]>) -> bool {
    if last.get() == next {
        false
    } else {
        last.set(next);
        true
    }
}

fn show_region_selectors(
    main_window: &AppWindow,
    sender: &Sender<Job>,
    screens: Vec<capture::CapturedScreen>,
    upload: &RegionUpload,
) {
    let holders = Rc::new(RefCell::new(Vec::<RegionWindow>::new()));

    for screen in screens {
        let selector = match RegionWindow::new() {
            Ok(selector) => selector,

            Err(error) => {
                dismiss_region_selectors(&holders);
                restore_window_if_needed(main_window, upload.restore_window);

                present_error(
                    main_window,
                    FailureStage::Capture,
                    &format!("failed to create region selector: {error}"),
                );

                return;
            }
        };
        let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            screen.preview().as_raw(),
            screen.preview().width(),
            screen.preview().height(),
        );

        selector.set_capture_image(slint::Image::from_rgba8(pixels));
        selector.set_selection_instruction(
            match &upload.purpose {
                RegionPurpose::Screenshot => "Drag to capture  -  click a window  -  Esc to cancel",

                RegionPurpose::Recording(_) => {
                    "Drag to record  -  click a window  -  Esc to cancel"
                }
            }
            .into(),
        );
        let (x, y) = screen.selector_position();
        let (width, height) = screen.selector_dimensions();

        // X11 needs the signed physical origin in its initial placement hints.
        // Unlike size, Slint preserves a pre-creation physical position as-is.
        // Wayland ignores this unsupported positioning request.
        #[cfg(target_os = "linux")]
        selector
            .window()
            .set_position(slint::PhysicalPosition::new(x, y));

        let screen = Rc::new(screen);

        wire_region_hover(
            &selector,
            Rc::clone(&screen),
            #[cfg(target_os = "linux")]
            main_window.as_weak(),
        );

        let holders_for_selection = Rc::clone(&holders);
        let main_weak = main_window.as_weak();
        let sender = sender.clone();
        let upload_for_selection = (*upload).clone();

        selector.on_selected(move |x, y, width, height| {
            handle_region_selection(
                [x, y, width, height],
                &screen,
                &holders_for_selection,
                &main_weak,
                &sender,
                upload_for_selection.clone(),
            );
        });

        wire_region_cancel(&selector, main_window, &holders, upload.restore_window);
        wire_region_close(&selector);

        holders.borrow_mut().push(selector.clone_strong());

        if let Err(error) = show_selector_when_ready(
            selector,
            main_window.as_weak(),
            Rc::clone(&holders),
            upload.restore_window,
            (x, y, width, height),
        ) {
            restore_window_if_needed(main_window, upload.restore_window);
            dismiss_region_selectors(&holders);

            present_error(
                main_window,
                FailureStage::Capture,
                &format!("failed to initialize region selector: {error}"),
            );

            return;
        }
    }
}

fn show_selector_when_ready(
    selector: RegionWindow,
    main_window: slint::Weak<AppWindow>,
    holders: Rc<RefCell<Vec<RegionWindow>>>,
    restore_window: bool,
    bounds: (i32, i32, u32, u32),
) -> Result<(), slint::EventLoopError> {
    slint::spawn_local(async move {
        // Before native creation Slint stores requested physical dimensions as
        // logical dimensions at scale 1. Wait for the actual monitor scale so a
        // 2200-pixel screen never becomes a 4400-pixel overlay at 200% scaling.
        let initialized = selector.window().winit_window().await;

        // Another monitor's selector may have been cancelled while this one
        // initialized. Never show a new overlay after the group was dismissed.
        if holders.borrow().is_empty() {
            return;
        }

        let result = async {
            #[cfg(target_os = "linux")]
            let native_window = initialized?;

            #[cfg(not(target_os = "linux"))]
            initialized?;

            let (x, y, width, height) = bounds;

            selector
                .window()
                .set_position(slint::PhysicalPosition::new(x, y));
            selector
                .window()
                .set_size(slint::PhysicalSize::new(width, height));

            #[cfg(target_os = "linux")]
            wait_for_x11_selector_size(&selector, &native_window, &holders, width, height).await?;

            if holders.borrow().is_empty() {
                return Ok(());
            }

            #[cfg(target_os = "linux")]
            if is_x11_window(&native_window) {
                // Slint intentionally restores decorations for native
                // fullscreen windows. A managed, monitor-sized X11 overlay
                // keeps no-frame stable through every property update.
                selector.set_keep_above(true);
                selector.show()?;
                wait_for_x11_selector_visibility(&native_window, &holders).await?;

                if !holders.borrow().is_empty() {
                    // Reassert the level after mapping so the WM observes it
                    // even if it ignored the pre-map EWMH state request.
                    native_window.set_window_level(
                        slint::winit_030::winit::window::WindowLevel::AlwaysOnTop,
                    );
                    native_window.focus_window();
                    native_window.request_redraw();
                }

                return Ok(());
            }

            #[cfg(target_os = "linux")]
            selector.show()?;

            stabilize_selector_on_monitor(&selector, x, y, width, height);

            #[cfg(not(target_os = "linux"))]
            selector.show()?;

            Ok::<_, slint::PlatformError>(())
        }
        .await;

        if let Err(error) = result {
            dismiss_region_selectors(&holders);

            if let Some(main_window) = main_window.upgrade() {
                restore_window_if_needed(&main_window, restore_window);
                present_error(
                    &main_window,
                    FailureStage::Capture,
                    &format!("failed to show region selector: {error}"),
                );
            }
        }
    })?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn is_x11_window(native_window: &slint::winit_030::winit::window::Window) -> bool {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

    native_window.window_handle().is_ok_and(|handle| {
        matches!(
            handle.as_raw(),
            RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_)
        )
    })
}

#[cfg(target_os = "linux")]
async fn wait_for_x11_selector_size(
    selector: &RegionWindow,
    native_window: &slint::winit_030::winit::window::Window,
    holders: &Rc<RefCell<Vec<RegionWindow>>>,
    width: u32,
    height: u32,
) -> Result<(), slint::PlatformError> {
    use std::{future::poll_fn, task::Poll, time::Instant};

    // Wayland may acknowledge size only after mapping. Only X11 needs this
    // wait: its resize request returns before Slint processes native Resized.
    if !is_x11_window(native_window) {
        return Ok(());
    }

    let expected = slint::PhysicalSize::new(width, height);
    let deadline = Instant::now() + Duration::from_secs(2);

    poll_fn(|context| {
        if holders.borrow().is_empty() {
            return Poll::Ready(Ok(()));
        }

        let native_size = native_window.inner_size();

        if selector.window().size() == expected
            && native_size.width == width
            && native_size.height == height
        {
            return Poll::Ready(Ok(()));
        }

        if Instant::now() >= deadline {
            return Poll::Ready(Err(slint::PlatformError::from(
                "the X11 region selector did not reach the monitor size in time",
            )));
        }

        let waker = context.waker().clone();

        slint::Timer::single_shot(Duration::from_millis(16), move || waker.wake());

        Poll::Pending
    })
    .await
}

#[cfg(target_os = "linux")]
async fn wait_for_x11_selector_visibility(
    native_window: &slint::winit_030::winit::window::Window,
    holders: &Rc<RefCell<Vec<RegionWindow>>>,
) -> Result<(), slint::PlatformError> {
    use std::{future::poll_fn, task::Poll, time::Instant};

    let deadline = Instant::now() + Duration::from_secs(2);

    // Winit ignores X11 focus_window until VisibilityNotify acknowledges map.
    // Wait for that state, never a fixed delay, and stop if any selector closes.
    poll_fn(|context| {
        if holders.borrow().is_empty() || native_window.is_visible() == Some(true) {
            return Poll::Ready(Ok(()));
        }

        if Instant::now() >= deadline {
            return Poll::Ready(Err(slint::PlatformError::from(
                "the X11 region selector did not become visible in time",
            )));
        }

        let waker = context.waker().clone();

        slint::Timer::single_shot(Duration::from_millis(16), move || waker.wake());

        Poll::Pending
    })
    .await
}

fn handle_region_selection(
    coordinates: [f32; 4],
    screen: &capture::CapturedScreen,
    holders: &Rc<RefCell<Vec<RegionWindow>>>,
    main_window: &slint::Weak<AppWindow>,
    sender: &Sender<Job>,
    mut upload: RegionUpload,
) {
    let region = capture::NormalizedRegion::new(coordinates);

    if upload.use_snapped_window_name
        && let Some(title) = screen.window_title_for_region(region)
    {
        upload.filename_stem = crate::naming::window_title_stem(title);
    }

    let schedule = |action: Box<dyn FnOnce(&AppWindow)>| {
        let main_window = main_window.clone();

        dismiss_region_selectors_then(holders, move || {
            if let Some(window) = main_window.upgrade() {
                action(&window);
            }
        });
    };
    let selected = match upload.purpose.clone() {
        RegionPurpose::Screenshot => screen
            .selected_region_capture(region)
            .map(RegionSelection::Screenshot),

        RegionPurpose::Recording(recording_stop) => screen
            .desktop_region(region)
            .map(|region| RegionSelection::Recording(region, recording_stop)),
    };
    let selected = match selected {
        Ok(selected) => selected,

        Err(error) => {
            let restore_window = upload.restore_window;

            schedule(Box::new(move |window| {
                restore_window_if_needed(window, restore_window);
                present_error(window, FailureStage::Capture, &format!("{error:#}"));
            }));

            return;
        }
    };
    let sender = sender.clone();

    schedule(Box::new(move |window| match selected {
        RegionSelection::Screenshot(selection) => {
            queue_cropped_region(window, &sender, selection, &upload);
        }

        RegionSelection::Recording(region, recording_stop) => {
            start_region_recording(window, &sender, region, &upload, &recording_stop);
        }
    }));
}

enum RegionSelection {
    Screenshot(capture::SelectedRegionCapture),
    Recording(capture::DesktopRegion, RecordingControl),
}

#[cfg(not(target_os = "macos"))]
fn stabilize_selector_on_monitor(selector: &RegionWindow, x: i32, y: i32, width: u32, height: u32) {
    let positioned = selector.window().with_winit_window(|native_window| {
        let monitor = native_window.available_monitors().find(|monitor| {
            let position = monitor.position();

            position.x == x && position.y == y
        });

        if let Some(monitor) = monitor {
            native_window.set_fullscreen(Some(
                slint::winit_030::winit::window::Fullscreen::Borderless(Some(monitor)),
            ));
            native_window
                .set_window_level(slint::winit_030::winit::window::WindowLevel::AlwaysOnTop);
            native_window.request_redraw();
            true
        } else {
            false
        }
    });

    if positioned != Some(true) {
        selector
            .window()
            .set_position(slint::PhysicalPosition::new(x, y));
        selector
            .window()
            .set_size(slint::PhysicalSize::new(width, height));
        selector.window().request_redraw();
    }
}

#[cfg(target_os = "macos")]
fn stabilize_selector_on_monitor(
    selector: &RegionWindow,
    _x: i32,
    _y: i32,
    _width: u32,
    _height: u32,
) {
    selector.window().set_fullscreen(true);
    selector.window().request_redraw();
}

#[cfg(target_os = "linux")]
fn synchronize_x11_selector_scale(
    window: &slint::Window,
    physical_size: slint::PhysicalSize,
    scale_factor: f64,
    inner_size_writer: &slint::winit_030::winit::event::InnerSizeWriter,
) -> Result<(), slint::PlatformError> {
    let mut writer = inner_size_writer.clone();

    writer
        .request_inner_size(slint::winit_030::winit::dpi::PhysicalSize::new(
            physical_size.width,
            physical_size.height,
        ))
        .map_err(|error| {
            slint::PlatformError::from(format!(
                "failed to keep the region selector on its monitor: {error}"
            ))
        })?;

    // Winit can omit Resized when the physical size stays unchanged. Keep
    // Slint's logical root consistent with its cached physical size before
    // normal propagation; that still performs accessibility and input work.
    if std::env::var("SLINT_SCALE_FACTOR").is_err() {
        let scale_factor = scale_factor
            .to_f32()
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .ok_or_else(|| {
                slint::PlatformError::from("the monitor reported an unsupported display scale")
            })?;

        window
            .try_dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor })
            .map_err(|error| {
                slint::PlatformError::from(format!(
                    "failed to update the region display scale: {error}"
                ))
            })?;
    }

    window
        .try_dispatch_event(slint::platform::WindowEvent::Resized {
            size: window.size().to_logical(window.scale_factor()),
        })
        .map_err(|error| {
            slint::PlatformError::from(format!("failed to resize the region preview: {error}"))
        })
}

fn wire_region_hover(
    selector: &RegionWindow,
    screen: Rc<capture::CapturedScreen>,
    #[cfg(target_os = "linux")] main_window: slint::Weak<AppWindow>,
) {
    let selector_weak = selector.as_weak();
    let last_snap = Cell::new(None);

    selector
        .window()
        .on_winit_window_event(move |slint_window, event| {
            let Some(selector) = selector_weak.upgrade() else {
                return slint::winit_030::EventResult::Propagate;
            };

            match event {
                #[cfg(target_os = "linux")]
                slint::winit_030::winit::event::WindowEvent::ScaleFactorChanged {
                    scale_factor,
                    inner_size_writer,
                } if slint_window.with_winit_window(is_x11_window) == Some(true) => {
                    let (width, height) = screen.selector_dimensions();
                    let result = synchronize_x11_selector_scale(
                        slint_window,
                        slint::PhysicalSize::new(width, height),
                        *scale_factor,
                        inner_size_writer,
                    );

                    if let Err(error) = result {
                        selector.invoke_cancelled();

                        if let Some(main_window) = main_window.upgrade() {
                            present_error(&main_window, FailureStage::Capture, &error.to_string());
                        }

                        return slint::winit_030::EventResult::PreventDefault;
                    }
                }

                slint::winit_030::winit::event::WindowEvent::CursorMoved { position, .. } => {
                    let overlay_size = slint_window.size();

                    if overlay_size.width > 0 && overlay_size.height > 0 {
                        let point = [
                            (position.x / f64::from(overlay_size.width))
                                .to_f32()
                                .unwrap_or(0.0),
                            (position.y / f64::from(overlay_size.height))
                                .to_f32()
                                .unwrap_or(0.0),
                        ];
                        let target = screen.window_target_at(point);
                        let next_snap =
                            Some(target.map_or([0.0, 0.0, 1.0, 1.0], |(bounds, _title)| bounds));

                        update_selector_snap(&selector, &last_snap, next_snap);
                    }
                }

                slint::winit_030::winit::event::WindowEvent::CursorLeft { .. } => {
                    update_selector_snap(&selector, &last_snap, None);
                }

                _ => {}
            }

            slint::winit_030::EventResult::Propagate
        });
}

fn update_selector_snap(
    selector: &RegionWindow,
    last_snap: &Cell<Option<[f32; 4]>>,
    next_snap: Option<[f32; 4]>,
) {
    if !snap_changed(last_snap, next_snap) {
        return;
    }

    if let Some(region) = next_snap {
        selector.set_snap_x(region[0]);
        selector.set_snap_y(region[1]);
        selector.set_snap_width(region[2]);
        selector.set_snap_height(region[3]);
        selector.set_snap_active(true);
    } else {
        selector.set_snap_active(false);
    }
}

fn begin_selection_activity(window: &AppWindow, detail: &str) {
    // Selection is provisional. Preserve the artifact, including action feedback;
    // a confirmed attempt keeps its recovery links until a replacement succeeds.
    window.set_busy(true);
    window.set_status_text("Select a region".into());
    window.set_status_detail(detail.into());
}

fn cancel_region_selection(window: &AppWindow, restore_window: bool) {
    window.set_busy(false);
    window.set_status_text("Ready".into());
    window.set_status_detail("Region selection cancelled".into());
    restore_window_if_needed(window, restore_window);
}

fn wire_region_cancel(
    selector: &RegionWindow,
    main_window: &AppWindow,
    holders: &Rc<RefCell<Vec<RegionWindow>>>,
    restore_window: bool,
) {
    let holders = Rc::clone(holders);
    let main_weak = main_window.as_weak();

    selector.on_cancelled(move || {
        dismiss_region_selectors(&holders);

        if let Some(window) = main_weak.upgrade() {
            cancel_region_selection(&window, restore_window);
        }
    });
}

fn wire_region_close(selector: &RegionWindow) {
    let selector_weak = selector.as_weak();

    selector.window().on_close_requested(move || {
        if let Some(selector) = selector_weak.upgrade() {
            selector.invoke_cancelled();
        }

        slint::CloseRequestResponse::KeepWindowShown
    });
}

fn dismiss_region_selectors(holders: &Rc<RefCell<Vec<RegionWindow>>>) {
    dismiss_region_selectors_then(holders, || {});
}

fn dismiss_region_selectors_then(
    holders: &Rc<RefCell<Vec<RegionWindow>>>,
    after_dismiss: impl FnOnce() + 'static,
) {
    // Pending native-window initializers must observe cancellation immediately,
    // while the components themselves stay alive until input dispatch ends.
    let selectors = std::mem::take(&mut *holders.borrow_mut());

    for selector in &selectors {
        let _ = selector.hide();
    }

    slint::Timer::single_shot(Duration::ZERO, move || {
        drop(selectors);
        after_dismiss();
    });
}

fn restore_window_if_needed(window: &AppWindow, restore: bool) {
    if restore {
        let _ = window.show();

        super::foreground_ui(window);
    }
}

pub(super) fn toggle_recording(
    window: &AppWindow,
    sender: &Sender<Job>,
    recording_stop: &RecordingControl,
    cancellation: Arc<UploadCancellation>,
) {
    if let Some(stop) = recording_stop.borrow_mut().take() {
        let requested = stop.send(()).is_ok();

        window.set_recording(false);

        if !requested {
            return;
        }

        window.set_status_text("Encoding recording...".into());
        window.set_status_detail(
            if window.get_auto_upload_captures() {
                "Finishing the recording before upload"
            } else {
                "Finishing the recording for local storage"
            }
            .into(),
        );

        return;
    }

    if window.get_busy() {
        return;
    }

    if let Err(error) = capture::ensure_recording_supported() {
        present_error(window, FailureStage::Capture, &error.to_string());

        return;
    }

    if views::settings::recording_format(window) == RecordingFormat::Mp4
        && crate::recording::ffmpeg::find().is_none()
    {
        super::ffmpeg_setup::show_missing(window);

        return;
    }

    begin_region_selection(
        window,
        sender.clone(),
        RegionPurpose::Recording(Rc::clone(recording_stop)),
        cancellation,
    );
}

fn queue_cropped_region(
    window: &AppWindow,
    sender: &Sender<Job>,
    selection: capture::SelectedRegionCapture,
    upload: &RegionUpload,
) {
    let window = window.as_weak();
    let sender = sender.clone();
    let upload = upload.clone();

    // Hidden selector components can remain in a compositor frame after destruction. Keep every
    // ShareR window hidden until that frame has cleared before asking the worker to recapture.
    slint::Timer::single_shot(CAPTURE_HIDE_DELAY, move || {
        let Some(window) = window.upgrade() else {
            return;
        };

        send_cropped_region_job(&window, &sender, selection, &upload);
    });
}

fn send_cropped_region_job(
    window: &AppWindow,
    sender: &Sender<Job>,
    selection: capture::SelectedRegionCapture,
    upload: &RegionUpload,
) {
    begin_artifact_attempt(window);
    window.set_upload_progress(0.0);

    if let Some(options) = &upload.upload {
        if upload.save_directory.is_some() {
            window.set_uploading(false);
            window.set_status_text("Saving capture...".into());
            window.set_status_detail("Keeping a local copy before upload".into());
        } else {
            window.set_uploading(true);
            window.set_status_text("Uploading...".into());
            window.set_status_detail(
                upload_status_detail(&options.uploader, options.lifetime_seconds).into(),
            );
        }
    } else {
        window.set_uploading(false);
        window.set_status_text("Saving capture...".into());
        window.set_status_detail("Keeping this capture on your computer".into());
    }

    upload.cancellation.reset();

    if sender
        .send(Job {
            source: JobSource::RegionCapture {
                selection,
                filename_stem: upload.filename_stem.clone(),
                capture_resolution: upload.capture_resolution,
                resize_quality: upload.resize_quality,
                save_directory: upload.save_directory.clone(),
            },
            upload: upload.upload.clone(),
            remove_exif: upload.remove_exif,
            cancellation: Arc::clone(&upload.cancellation),
            restore_window_after_capture: upload.restore_window,
        })
        .is_err()
    {
        restore_window_if_needed(window, upload.restore_window);

        present_error(
            window,
            FailureStage::Capture,
            "Capture worker stopped unexpectedly",
        );
    }
}

fn start_region_recording(
    window: &AppWindow,
    sender: &Sender<Job>,
    region: capture::DesktopRegion,
    upload: &RegionUpload,
    recording_stop: &RecordingControl,
) {
    let (stop_sender, stop_receiver) = mpsc::channel();

    begin_artifact_attempt(window);
    *recording_stop.borrow_mut() = Some(stop_sender);
    window.set_recording(true);
    window.set_busy(true);
    window.set_uploading(false);
    window.set_upload_progress(0.0);
    window.set_status_text("Recording region...".into());
    window.set_status_detail("Press Stop or use the recording shortcut to finish".into());
    restore_window_if_needed(window, upload.restore_window);
    upload.cancellation.reset();

    if sender
        .send(Job {
            source: JobSource::Recording {
                region,
                stop: stop_receiver,
                filename_stem: upload.filename_stem.clone(),
                frames_per_second: upload.recording_fps,
                max_seconds: upload.recording_max_seconds,
                max_gib: upload.recording_max_gib,
                format: upload.recording_format,
                capture_resolution: upload.capture_resolution,
                save_directory: upload.save_directory.clone(),
            },
            upload: upload.upload.clone(),
            remove_exif: upload.remove_exif,
            cancellation: Arc::clone(&upload.cancellation),
            restore_window_after_capture: false,
        })
        .is_err()
    {
        *recording_stop.borrow_mut() = None;
        window.set_recording(false);
        present_error(
            window,
            FailureStage::Capture,
            "Capture worker stopped unexpectedly",
        );
    }
}

#[cfg(test)]
mod tests;
