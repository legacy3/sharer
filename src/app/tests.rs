use std::{
    cell::{Cell, RefCell},
    io::{Read as _, Write as _},
    net::TcpListener,
    rc::Rc,
    sync::Arc,
    thread,
};

use sharer::history::HistoryEntry;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::winit_030::winit::{
    event::{ElementState, WindowEvent},
    keyboard::{KeyCode, PhysicalKey},
};
use slint::{ComponentHandle as _, PhysicalPosition, PhysicalSize};

use super::{
    ActivitySnapshot, AppWindow, CloseAction, CloseState, FailureStage, Job, JobCallbacks,
    JobSource, QuitDecision, QuitState, RendererPreference, UploadOptions, UploadProgress,
    close_action, force_full_repaint, invalidate_activity_local_path, process_job_with_history,
    quit_decision, region::snap_changed, renderer_name, restore_event_requests_full_repaint,
    result_status, shortcut_event_should_capture,
};
use sharer::upload::{UploadCancellation, UploadPayload, UploadTarget, UploaderKind};

thread_local! {
    static SOFTWARE_WINDOW: Rc<MinimalSoftwareWindow> =
        MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
}

struct SoftwareTestPlatform;

impl Platform for SoftwareTestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(SOFTWARE_WINDOW.with(Clone::clone))
    }
}

fn custom_upload(endpoint: String) -> UploadOptions {
    UploadOptions {
        uploader: UploadTarget {
            kind: UploaderKind::Custom,
            custom_url: endpoint,
            ..UploadTarget::default()
        },
        lifetime_seconds: 60,
        require_tor: false,
    }
}

fn prepared_capture(
    directory: &std::path::Path,
    upload: UploadOptions,
    cancellation: Arc<UploadCancellation>,
) -> Job {
    Job {
        source: JobSource::Prepared {
            payload: UploadPayload::from_bytes(
                vec![1, 2, 3],
                "capture.png".to_owned(),
                "image/png".to_owned(),
            ),
            save_directory: Some(directory.to_owned()),
        },
        upload: Some(upload),
        remove_exif: false,
        cancellation,
        restore_window_after_capture: false,
    }
}

fn fake_upload_server(status: &str, body: &str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);

        stream.write_all(response.as_bytes()).unwrap();
    });

    (format!("http://{address}/upload"), server)
}

#[test]
fn close_decision_keeps_only_tray_or_busy_windows_alive() {
    assert_eq!(
        close_action(CloseState {
            busy: true,
            minimize_to_tray: false,
        }),
        CloseAction::KeepShown
    );
    assert_eq!(
        close_action(CloseState {
            busy: true,
            minimize_to_tray: true,
        }),
        CloseAction::KeepShown
    );
    assert_eq!(
        close_action(CloseState {
            busy: false,
            minimize_to_tray: true,
        }),
        CloseAction::HideToTray
    );
    assert_eq!(
        close_action(CloseState {
            busy: false,
            minimize_to_tray: false,
        }),
        CloseAction::Quit
    );
}

#[test]
fn quit_during_upload_waits_for_worker_cleanup_and_only_cancels_once() {
    assert_eq!(
        quit_decision(QuitState {
            busy: true,
            uploading: true,
            already_pending: false,
        }),
        QuitDecision::WaitForUpload { cancel: true }
    );
    assert_eq!(
        quit_decision(QuitState {
            busy: true,
            uploading: true,
            already_pending: true,
        }),
        QuitDecision::WaitForUpload { cancel: false }
    );
    assert_eq!(
        quit_decision(QuitState {
            busy: true,
            uploading: false,
            already_pending: false,
        }),
        QuitDecision::BlockedByCapture
    );
    assert_eq!(
        quit_decision(QuitState {
            busy: false,
            uploading: false,
            already_pending: true,
        }),
        QuitDecision::ExitNow
    );
}

#[test]
fn capture_completion_is_reported_before_local_persistence() {
    let directory = tempfile::tempdir().unwrap();
    let capture_complete = Cell::new(false);
    let job = Job {
        source: JobSource::Prepared {
            payload: UploadPayload::from_bytes(
                vec![1, 2, 3],
                "capture.png".to_owned(),
                "image/png".to_owned(),
            ),
            save_directory: Some(directory.path().to_owned()),
        },
        upload: None,
        remove_exif: false,
        cancellation: Arc::new(UploadCancellation::default()),
        restore_window_after_capture: true,
    };

    process_job_with_history(
        job,
        &Arc::new(UploadProgress::new()),
        JobCallbacks {
            persist_history: |_entry: &mut HistoryEntry| {
                assert!(capture_complete.get());
                None
            },
            capture_complete: || capture_complete.set(true),
        },
    )
    .unwrap();

    assert!(capture_complete.get());
}

#[test]
fn capture_completion_is_reported_when_preparation_fails() {
    let capture_complete = Cell::new(false);
    let job = Job {
        source: JobSource::File(std::path::PathBuf::from("missing-capture-source")),
        upload: None,
        remove_exif: false,
        cancellation: Arc::new(UploadCancellation::default()),
        restore_window_after_capture: true,
    };

    assert!(
        process_job_with_history(
            job,
            &Arc::new(UploadProgress::new()),
            JobCallbacks {
                persist_history: |_entry: &mut HistoryEntry| None,
                capture_complete: || capture_complete.set(true),
            },
        )
        .is_err()
    );
    assert!(capture_complete.get());
}

#[test]
fn deleted_history_file_invalidates_persisted_latest_activity_by_id_or_path() {
    let path = std::path::Path::new("C:/captures/latest.png");
    let activity = RefCell::new(Some(ActivitySnapshot {
        entry_id: 42,
        delete_url: String::new(),
        detail: "Saved locally".to_owned(),
        history_saved: true,
        link: String::new(),
        local_path: path.to_string_lossy().into_owned(),
        status: "Saved locally".to_owned(),
    }));

    assert!(invalidate_activity_local_path(
        &activity,
        42,
        std::path::Path::new("C:/elsewhere/capture.png")
    ));
    assert_eq!(activity.borrow().as_ref().unwrap().local_path, "");

    activity.borrow_mut().as_mut().unwrap().local_path = path.to_string_lossy().into_owned();
    assert!(invalidate_activity_local_path(&activity, 7, path));
    assert_eq!(activity.borrow().as_ref().unwrap().local_path, "");

    activity.borrow_mut().as_mut().unwrap().local_path = path.to_string_lossy().into_owned();
    assert!(!invalidate_activity_local_path(
        &activity,
        7,
        std::path::Path::new("C:/captures/other.png")
    ));
    assert_eq!(
        activity.borrow().as_ref().unwrap().local_path,
        path.to_string_lossy()
    );
}

#[test]
fn completed_upload_and_local_copy_are_both_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let (endpoint, server) = fake_upload_server(
        "200 OK",
        r#"{"data":{"originalName":"capture.png","size":3,"link":"https://public.example/capture","deleteLink":"https://delete.example/SENSITIVE-CAPABILITY","expiresAt":"soon"}}"#,
    );
    let mut persisted = false;
    let result = process_job_with_history(
        prepared_capture(
            directory.path(),
            custom_upload(endpoint),
            Arc::new(UploadCancellation::default()),
        ),
        &Arc::new(UploadProgress::new()),
        JobCallbacks {
            persist_history: |entry: &mut HistoryEntry| {
                persisted = true;
                entry.id = 7;
                None
            },
            capture_complete: || {},
        },
    )
    .unwrap();

    server.join().unwrap();
    assert!(persisted);
    assert_eq!(result.entry.id, 7);
    assert_eq!(result.entry.link, "https://public.example/capture");
    assert!(result.entry.local_path.ends_with("capture.png"));
    assert!(result.upload_failure.is_none());
}

#[test]
fn failed_upload_after_local_save_keeps_a_local_history_result() {
    let directory = tempfile::tempdir().unwrap();
    let (endpoint, server) = fake_upload_server("503 Service Unavailable", "try later");
    let mut persisted = false;
    let result = process_job_with_history(
        prepared_capture(
            directory.path(),
            custom_upload(endpoint),
            Arc::new(UploadCancellation::default()),
        ),
        &Arc::new(UploadProgress::new()),
        JobCallbacks {
            persist_history: |entry: &mut HistoryEntry| {
                persisted = true;
                entry.id = 8;
                None
            },
            capture_complete: || {},
        },
    )
    .unwrap();

    server.join().unwrap();
    assert!(persisted);
    assert_eq!(result.entry.id, 8);
    assert!(result.entry.link.is_empty());
    assert!(std::path::Path::new(&result.entry.local_path).is_file());
    assert_eq!(result.upload_failure.unwrap().stage, FailureStage::Upload);
}

#[test]
fn cancelled_upload_after_local_save_keeps_a_local_history_result() {
    let directory = tempfile::tempdir().unwrap();
    let cancellation = Arc::new(UploadCancellation::default());

    cancellation.cancel();
    let result = process_job_with_history(
        prepared_capture(
            directory.path(),
            custom_upload("http://127.0.0.1:1/upload".to_owned()),
            cancellation,
        ),
        &Arc::new(UploadProgress::new()),
        JobCallbacks {
            persist_history: |_entry: &mut HistoryEntry| None,
            capture_complete: || {},
        },
    )
    .unwrap();

    assert!(std::path::Path::new(&result.entry.local_path).is_file());
    assert_eq!(
        result.upload_failure.unwrap().stage,
        FailureStage::UploadCancelled
    );
}

#[test]
fn local_copy_failure_is_reported_as_a_save_failure() {
    let directory = tempfile::tempdir().unwrap();
    let not_a_directory = directory.path().join("capture-root");

    std::fs::write(&not_a_directory, b"not a directory").unwrap();
    let job = Job {
        source: JobSource::Prepared {
            payload: UploadPayload::from_bytes(
                vec![1, 2, 3],
                "capture.png".to_owned(),
                "image/png".to_owned(),
            ),
            save_directory: Some(not_a_directory),
        },
        upload: None,
        remove_exif: false,
        cancellation: Arc::new(UploadCancellation::default()),
        restore_window_after_capture: true,
    };
    let error = process_job_with_history(
        job,
        &Arc::new(UploadProgress::new()),
        JobCallbacks {
            persist_history: |_entry: &mut HistoryEntry| {
                panic!("failed saves must not be persisted")
            },
            capture_complete: || {},
        },
    )
    .unwrap_err();

    assert_eq!(error.stage, FailureStage::Save);
}

#[test]
fn result_and_error_labels_match_the_completed_stage() {
    assert_eq!(FailureStage::Capture.status(), "Capture failed");
    assert_eq!(FailureStage::Save.status(), "Save failed");
    assert_eq!(FailureStage::Settings.status(), "Settings invalid");
    assert_eq!(FailureStage::Upload.status(), "Upload failed");
    assert_eq!(
        result_status(false, Some(FailureStage::UploadCancelled)),
        "Saved locally, upload cancelled"
    );
    assert_eq!(
        result_status(false, Some(FailureStage::UploadTimedOut)),
        "Saved locally, upload timed out"
    );
    assert_eq!(
        result_status(false, Some(FailureStage::Upload)),
        "Saved locally, upload failed"
    );
}

#[test]
fn renderer_selection_preserves_auto_and_software_fallbacks() {
    assert_eq!(renderer_name(RendererPreference::Auto), None);
    assert_eq!(
        renderer_name(RendererPreference::Software),
        Some("software")
    );
    assert_eq!(
        renderer_name(RendererPreference::Gpu),
        Some(if cfg!(target_os = "macos") {
            "skia"
        } else {
            "femtovg"
        })
    );
}

#[test]
fn restored_or_revealed_windows_request_a_full_repaint() {
    let repaint_pending = Cell::new(false);

    assert!(!restore_event_requests_full_repaint(
        &repaint_pending,
        &WindowEvent::Focused(false)
    ));
    assert!(restore_event_requests_full_repaint(
        &repaint_pending,
        &WindowEvent::Occluded(false)
    ));
    assert!(!restore_event_requests_full_repaint(
        &repaint_pending,
        &WindowEvent::Focused(true)
    ));

    assert!(!restore_event_requests_full_repaint(
        &repaint_pending,
        &WindowEvent::Occluded(true)
    ));
    assert!(restore_event_requests_full_repaint(
        &repaint_pending,
        &WindowEvent::Focused(true)
    ));
}

#[test]
fn shortcut_capture_accepts_print_screen_on_key_up() {
    let print_screen = PhysicalKey::Code(KeyCode::PrintScreen);
    let regular_key = PhysicalKey::Code(KeyCode::KeyV);

    assert!(shortcut_event_should_capture(
        ElementState::Released,
        false,
        print_screen
    ));
    assert!(shortcut_event_should_capture(
        ElementState::Pressed,
        false,
        regular_key
    ));
    assert!(!shortcut_event_should_capture(
        ElementState::Released,
        false,
        regular_key
    ));
    assert!(!shortcut_event_should_capture(
        ElementState::Pressed,
        true,
        regular_key
    ));
}

#[test]
fn repaint_sequence_invalidates_the_entire_software_surface() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let software_window = SOFTWARE_WINDOW.with(Clone::clone);
    let size = PhysicalSize::new(320, 240);

    software_window.set_size(size);

    let app = AppWindow::new().unwrap();

    app.show().unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); size.width as usize * size.height as usize];

    assert!(software_window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, size.width as usize);
    }));

    force_full_repaint(&app);

    let mut damaged_origin = None;
    let mut damaged_size = None;

    assert!(software_window.draw_if_needed(|renderer| {
        let damage = renderer.render(&mut pixels, size.width as usize);
        damaged_origin = Some(damage.bounding_box_origin());
        damaged_size = Some(damage.bounding_box_size());
    }));
    assert_eq!(damaged_origin, Some(PhysicalPosition::new(0, 0)));
    assert_eq!(damaged_size, Some(size));
}

#[test]
fn repeated_hover_does_not_trigger_redundant_snap_updates() {
    let last = Cell::new(None);
    let window = Some([0.1, 0.2, 0.3, 0.4]);

    assert!(snap_changed(&last, window));
    assert!(!snap_changed(&last, window));
    assert!(snap_changed(&last, None));
    assert!(!snap_changed(&last, None));
}
