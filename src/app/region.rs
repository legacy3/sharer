//! Interactive region selection and recording controls.

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{self, Sender},
    time::Duration,
};

use slint::ComponentHandle as _;

use crate::{AppWindow, RegionWindow, views};
use sharer::{
    capture,
    config::{CaptureResolution, ResizeQuality},
    upload::UploadTarget,
};

use super::{
    CAPTURE_HIDE_DELAY, Job, JobSource, RecordingControl, clear_last_receipt, show_error,
    upload_status_detail, valid_window_lifetime, valid_window_upload_target,
};

#[derive(Clone, Debug)]
struct RegionUpload {
    uploader: UploadTarget,
    filename_stem: String,
    use_snapped_window_name: bool,
    lifetime_seconds: u32,
    recording_fps: u32,
    recording_max_seconds: u32,
    capture_resolution: CaptureResolution,
    resize_quality: ResizeQuality,
    save_directory: Option<PathBuf>,
    require_tor: bool,
    remove_exif: bool,
    purpose: RegionPurpose,
}

#[derive(Clone, Debug)]
enum RegionPurpose {
    Screenshot,
    Recording(RecordingControl),
}

pub(super) fn start_screenshot(window: &AppWindow, sender: Sender<Job>) {
    begin_region_selection(window, sender, RegionPurpose::Screenshot);
}

fn begin_region_selection(window: &AppWindow, sender: Sender<Job>, purpose: RegionPurpose) {
    let Some(uploader) = valid_window_upload_target(window) else {
        return;
    };

    if window.get_busy() {
        return;
    }

    let Some(lifetime_seconds) = valid_window_lifetime(window) else {
        return;
    };
    let recording_fps = match views::settings::validated_recording_fps(window) {
        Ok(recording_fps) => recording_fps,

        Err(error) => {
            show_error(window, &error.to_string());

            return;
        }
    };
    let recording_max_seconds = match views::settings::validated_recording_seconds(window) {
        Ok(seconds) => seconds,

        Err(error) => {
            show_error(window, &error.to_string());

            return;
        }
    };
    let Ok(naming) = views::settings::naming_from_window(window) else {
        show_error(window, "Choose a valid screenshot name");

        return;
    };
    let private_capture_names = window.get_private_capture_names();
    let use_snapped_window_name =
        !private_capture_names && naming.mode == sharer::config::CaptureNameMode::ActiveWindow;
    let color_mode = capture_color_mode(window);
    let capture_resolution = capture_resolution(window);

    clear_last_receipt(window);
    window.set_busy(true);
    window.set_status_text("Select a region".into());
    window.set_status_detail("Drag across the frozen screen  -  press Esc to cancel".into());
    let _ = window.hide();
    let main_window = window.as_weak();

    slint::Timer::single_shot(CAPTURE_HIDE_DELAY, move || {
        let capture = capture::capture_region_source(color_mode, capture_resolution);
        let Some(window) = main_window.upgrade() else {
            return;
        };
        let filename_stem = match crate::naming::screenshot_stem(&naming, private_capture_names) {
            Ok(stem) => stem,

            Err(error) => {
                let _ = window.show();

                show_error(&window, &error.to_string());

                return;
            }
        };

        match capture {
            Ok(screen) => show_region_selector(
                &window,
                sender,
                screen,
                RegionUpload {
                    uploader,
                    filename_stem,
                    use_snapped_window_name,
                    lifetime_seconds,
                    recording_fps,
                    recording_max_seconds,
                    capture_resolution,
                    resize_quality: resize_quality(&window),
                    save_directory: views::settings::configured_capture_directory(&window),
                    require_tor: window.get_require_tor(),
                    remove_exif: window.get_remove_exif(),
                    purpose,
                },
            ),

            Err(error) => {
                let _ = window.show();

                show_error(&window, &format!("{error:#}"));
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

fn show_region_selector(
    main_window: &AppWindow,
    sender: Sender<Job>,
    mut screen: capture::CapturedScreen,
    upload: RegionUpload,
) {
    let selector = match RegionWindow::new() {
        Ok(selector) => selector,

        Err(error) => {
            let _ = main_window.show();

            show_error(
                main_window,
                &format!("failed to create region selector: {error}"),
            );

            return;
        }
    };
    let scaled_preview = screen.scaled_selector_preview();
    let preview = scaled_preview.as_ref().unwrap_or_else(|| screen.preview());
    let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        preview.as_raw(),
        preview.width(),
        preview.height(),
    );

    screen.release_redundant_buffers();

    selector.set_capture_image(slint::Image::from_rgba8(pixels));
    selector.set_selection_instruction(
        match &upload.purpose {
            RegionPurpose::Screenshot => "Drag to capture  -  click a window  -  Esc to cancel",
            RegionPurpose::Recording(_) => "Drag to record  -  click a window  -  Esc to cancel",
        }
        .into(),
    );
    selector.window().set_fullscreen(true);
    let holder = Rc::new(RefCell::new(Some(selector)));
    let screen = Rc::new(screen);

    if let Some(selector) = holder.borrow().as_ref() {
        wire_region_hover(selector, Rc::clone(&screen));
    }

    let holder_for_selection = Rc::clone(&holder);
    let main_weak = main_window.as_weak();

    if let Some(selector) = holder.borrow().as_ref() {
        selector.on_selected(move |x, y, width, height| {
            dismiss_region_selector(&holder_for_selection);

            let Some(window) = main_weak.upgrade() else {
                return;
            };

            let region = capture::NormalizedRegion::new([x, y, width, height]);
            let mut upload = upload.clone();

            if upload.use_snapped_window_name
                && let Some(title) = screen.window_title_for_region(region)
            {
                upload.filename_stem = crate::naming::window_title_stem(title);
            }

            match &upload.purpose {
                RegionPurpose::Screenshot => {
                    queue_cropped_region(&window, &sender, &screen, region, &upload);
                }

                RegionPurpose::Recording(recording_stop) => {
                    start_region_recording(
                        &window,
                        &sender,
                        &screen,
                        region,
                        &upload,
                        recording_stop,
                    );
                }
            }
        });
    }

    let holder_for_cancel = Rc::clone(&holder);
    let main_weak = main_window.as_weak();

    if let Some(selector) = holder.borrow().as_ref() {
        selector.on_cancelled(move || {
            dismiss_region_selector(&holder_for_cancel);

            if let Some(window) = main_weak.upgrade() {
                window.set_busy(false);
                window.set_status_text("Ready".into());
                window.set_status_detail("Region selection cancelled".into());
                let _ = window.show();
            }
        });
        wire_region_close(selector);

        if let Err(error) = selector.show() {
            let _ = main_window.show();

            dismiss_region_selector(&holder);

            show_error(
                main_window,
                &format!("failed to show region selector: {error}"),
            );
        }
    }
}

fn wire_region_hover(selector: &RegionWindow, screen: Rc<capture::CapturedScreen>) {
    let selector_weak = selector.as_weak();
    let last_snap = Cell::new(None);

    selector.on_window_hovered(move |x, y| {
        let Some(selector) = selector_weak.upgrade() else {
            return;
        };
        let next_snap = screen.window_region_at([x, y]);

        if !snap_changed(&last_snap, next_snap) {
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

fn dismiss_region_selector(holder: &Rc<RefCell<Option<RegionWindow>>>) {
    if let Some(selector) = holder.borrow().as_ref() {
        let _ = selector.hide();
    }

    // Defer component destruction until its active callback unwinds.
    let holder = Rc::clone(holder);

    slint::Timer::single_shot(Duration::ZERO, move || {
        drop(holder.borrow_mut().take());
    });
}

pub(super) fn toggle_recording(
    window: &AppWindow,
    sender: &Sender<Job>,
    recording_stop: &RecordingControl,
) {
    if let Some(stop) = recording_stop.borrow_mut().take() {
        let _ = stop.send(());

        window.set_recording(false);
        window.set_status_text("Encoding recording...".into());
        window.set_status_detail("Finishing the animated WebP before upload".into());

        return;
    }

    if window.get_busy() {
        return;
    }

    if let Err(error) = capture::ensure_recording_supported() {
        show_error(window, &error.to_string());

        return;
    }

    begin_region_selection(
        window,
        sender.clone(),
        RegionPurpose::Recording(Rc::clone(recording_stop)),
    );
}

fn queue_cropped_region(
    window: &AppWindow,
    sender: &Sender<Job>,
    screen: &capture::CapturedScreen,
    region: capture::NormalizedRegion,
    upload: &RegionUpload,
) {
    let payload = match capture::crop_captured_region(
        screen,
        region,
        upload.capture_resolution,
        upload.resize_quality,
    ) {
        Ok(mut payload) => {
            payload.filename =
                capture::named_capture_filename(&upload.filename_stem, &payload.filename);
            payload
        }

        Err(error) => {
            let _ = window.show();

            show_error(window, &format!("{error:#}"));

            return;
        }
    };

    let _ = window.show();

    window.set_uploading(true);
    window.set_upload_progress(0.0);
    window.set_status_text("Uploading...".into());
    window
        .set_status_detail(upload_status_detail(&upload.uploader, upload.lifetime_seconds).into());

    if sender
        .send(Job {
            source: JobSource::Prepared {
                payload,
                save_directory: upload.save_directory.clone(),
            },
            uploader: upload.uploader.clone(),
            lifetime_seconds: upload.lifetime_seconds,
            require_tor: upload.require_tor,
            remove_exif: upload.remove_exif,
        })
        .is_err()
    {
        let _ = window.show();

        show_error(window, "Upload worker stopped unexpectedly");
    }
}

fn start_region_recording(
    window: &AppWindow,
    sender: &Sender<Job>,
    screen: &capture::CapturedScreen,
    region: capture::NormalizedRegion,
    upload: &RegionUpload,
    recording_stop: &RecordingControl,
) {
    let region = match screen.desktop_region(region) {
        Ok(region) => region,

        Err(error) => {
            let _ = window.show();

            show_error(window, &format!("{error:#}"));

            return;
        }
    };
    let (stop_sender, stop_receiver) = mpsc::channel();

    *recording_stop.borrow_mut() = Some(stop_sender);
    window.set_recording(true);
    window.set_busy(true);
    window.set_uploading(true);
    window.set_upload_progress(0.0);
    window.set_status_text("Recording region...".into());
    window.set_status_detail("Press Record again or use its shortcut to stop".into());
    let _ = window.show();

    if sender
        .send(Job {
            source: JobSource::Recording {
                region,
                stop: stop_receiver,
                filename_stem: upload.filename_stem.clone(),
                frames_per_second: upload.recording_fps,
                max_seconds: upload.recording_max_seconds,
                capture_resolution: upload.capture_resolution,
                save_directory: upload.save_directory.clone(),
            },
            uploader: upload.uploader.clone(),
            lifetime_seconds: upload.lifetime_seconds,
            require_tor: upload.require_tor,
            remove_exif: upload.remove_exif,
        })
        .is_err()
    {
        *recording_stop.borrow_mut() = None;
        window.set_recording(false);
        show_error(window, "Upload worker stopped unexpectedly");
    }
}
