use std::{
    cell::{Cell, RefCell},
    io::{Read as _, Write as _},
    net::TcpListener,
    rc::Rc,
    sync::Arc,
    thread,
};

use sharer::history::HistoryEntry;
use sharer::storage::PlatformStorage;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::winit_030::winit::{
    event::{ElementState, WindowEvent},
    keyboard::{KeyCode, PhysicalKey},
};
use slint::{ComponentHandle as _, Model as _, PhysicalPosition, PhysicalSize};

use super::{
    ActivitySnapshot, AppWindow, CloseAction, CloseState, FailureStage, Job, JobCallbacks,
    JobSource, QuitDecision, QuitState, UploadOptions, UploadProgress, close_action,
    force_full_repaint, invalidate_activity_local_path,
    job::receive_job,
    native_renderer::{preferred_renderer, renderer_name},
    process_job_with_history, quit_decision,
    region::snap_changed,
    restore_event_requests_full_repaint, result_status, shortcut_event_should_capture,
};
use crate::cli::RendererPreference;
use sharer::upload::{UploadCancellation, UploadPayload, UploadTarget, UploaderKind};

thread_local! {
    static SOFTWARE_WINDOW: Rc<MinimalSoftwareWindow> =
        MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
}

pub(super) struct SoftwareTestPlatform;

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
        copy_link: false,
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

fn fake_upload_server(status: &str, body: &str) -> (String, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();

        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];

        loop {
            let count = stream.read(&mut chunk).unwrap();

            assert_ne!(count, 0, "upload ended before its complete request body");
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;

                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .expect("upload must declare its content length");

            if request.len() >= header_end + 4 + length {
                break;
            }
        }

        stream.write_all(response.as_bytes()).unwrap();
        request
    });

    (format!("http://{address}/upload"), server)
}

fn local_history_capture(directory: &std::path::Path, storage: &PlatformStorage) -> HistoryEntry {
    local_history_file(directory, storage, "capture.png", &[1, 2, 3])
}

fn local_history_file(
    directory: &std::path::Path,
    storage: &PlatformStorage,
    filename: &str,
    bytes: &[u8],
) -> HistoryEntry {
    let captures = directory.join("captures");

    std::fs::create_dir(&captures).unwrap();
    let path = captures.join(filename);

    std::fs::write(&path, bytes).unwrap();
    let mut entry = HistoryEntry::local(
        filename.to_owned(),
        u64::try_from(bytes.len()).unwrap(),
        &path,
    );

    entry.created_at = 123;
    entry.id = storage.insert_history(&entry).unwrap();
    entry
}

fn history_upload_job(
    original: HistoryEntry,
    endpoint: String,
    cancellation: Arc<UploadCancellation>,
) -> Job {
    Job {
        source: JobSource::History(original),
        upload: Some(custom_upload(endpoint)),
        remove_exif: false,
        cancellation,
        restore_window_after_capture: false,
    }
}

fn initialize_test_history(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
) -> Rc<RefCell<crate::views::history::HistoryController>> {
    let deleted: crate::views::history::LocalDeletionObserver = Rc::new(|_, _, _| {});
    let removed: crate::views::history::HistoryRemovalObserver = Rc::new(|_, _| {});

    crate::views::history::initialize(window, storage, false, &deleted, &removed).unwrap()
}

#[test]
fn history_upload_uses_saved_settings_and_updates_original_without_changing_the_file() {
    use crate::views::settings;
    use sharer::config::AppConfig;

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
    let bytes = include_bytes!("../../tests/fixtures/images/minnie_sdr_with_exif.jpg");
    let original = local_history_file(directory.path(), &storage, "capture.jpg", bytes);
    let path = std::path::Path::new(&original.local_path);
    let captures = path.parent().unwrap();
    let (endpoint, server) = fake_upload_server(
        "200 OK",
        r#"{"data":{"originalName":"capture.jpg","size":3,"link":"https://public.example/retry","deleteLink":"https://delete.example/retry-token","expiresAt":"soon"}}"#,
    );
    let mut saved = AppConfig {
        uploader_kind: UploaderKind::Custom,
        uploader_url: endpoint,
        lifetime_seconds: 90,
        capture_directory: directory
            .path()
            .join("must-not-be-created")
            .to_string_lossy()
            .into_owned(),
        ..AppConfig::default()
    };

    saved.behavior.onboarding_complete = true;
    saved.behavior.copy_link_after_upload = false;
    saved.behavior.auto_upload_captures = false;
    saved.privacy.remove_exif = true;
    settings::initialize(&window, &saved);
    let history = initialize_test_history(&window, &storage);
    let (sender, receiver) = std::sync::mpsc::channel();
    let cancellation = Arc::new(UploadCancellation::default());

    super::wire_history_upload_callback(&window, &sender, &storage, &history, &cancellation);
    window.set_current_page(1);
    window.invoke_upload_history_local(window.get_history_rows().row_data(0).unwrap().action_token);
    assert!(window.get_busy());
    assert!(window.get_uploading());
    assert_eq!(
        window.get_current_page(),
        0,
        "show progress and cancellation"
    );
    let job = receive_job(&receiver, false).unwrap();
    let progress = Arc::new(UploadProgress::new());
    let result = process_job_with_history(
        job,
        &progress,
        JobCallbacks {
            persist_history: |entry: &mut HistoryEntry, original: Option<&HistoryEntry>| {
                storage
                    .complete_history_upload(original.unwrap(), entry)
                    .unwrap();
                None
            },
            capture_complete: || {},
        },
    )
    .unwrap();
    let request = server.join().unwrap();

    assert!(request.starts_with(b"POST /upload?time=90 HTTP/1.1\r\n"));
    let image_start = request
        .windows(2)
        .position(|part| part == b"\xff\xd8")
        .unwrap();
    let image_end = request
        .windows(2)
        .rposition(|part| part == b"\xff\xd9")
        .unwrap()
        + 2;

    assert!(
        metastrip::extract_metadata(&request[image_start..image_end])
            .unwrap()
            .exif
            .is_none()
    );
    assert_eq!(storage.history_count().unwrap(), 1);
    assert_eq!(
        storage.history_entry(original.id).unwrap().unwrap(),
        result.entry
    );
    assert_eq!(result.entry.id, original.id);
    assert_eq!(result.entry.created_at, original.created_at);
    assert_eq!(result.entry.local_path, original.local_path);
    assert_eq!(result.entry.link, "https://public.example/retry");
    assert_eq!(
        result.entry.delete_url,
        "https://delete.example/retry-token"
    );
    assert_eq!(result.local_copy.as_deref(), Some(path));
    assert!(!result.clipboard_copied);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(captures).unwrap().count(), 1);
    assert!(!std::path::Path::new(&saved.capture_directory).exists());
    window.set_busy(false);
    window.invoke_upload_history_local(window.get_history_rows().row_data(0).unwrap().action_token);
    assert!(
        receiver.try_recv().is_err(),
        "a cached local row must not re-upload a saved link"
    );
}

#[test]
fn failed_and_cancelled_history_uploads_preserve_original_rows_files_and_local_actions() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();

    for cancelled in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
        let original = local_history_capture(directory.path(), &storage);
        let window = AppWindow::new().unwrap();
        let history = initialize_test_history(&window, &storage);
        let token = window.get_history_rows().row_data(0).unwrap().action_token;
        let cancellation = Arc::new(UploadCancellation::default());
        let (endpoint, server) = if cancelled {
            cancellation.cancel();
            ("http://127.0.0.1:1/upload".to_owned(), None)
        } else {
            let (endpoint, server) = fake_upload_server("503 Service Unavailable", "try later");

            (endpoint, Some(server))
        };
        let result = process_job_with_history(
            history_upload_job(original.clone(), endpoint, cancellation),
            &Arc::new(UploadProgress::new()),
            JobCallbacks {
                persist_history: |_: &mut HistoryEntry, _: Option<&HistoryEntry>| {
                    panic!("a failed history upload must not persist a new row")
                },
                capture_complete: || {},
            },
        )
        .unwrap();

        if let Some(server) = server {
            server.join().unwrap();
        }

        if !cancelled {
            let detail = super::result::upload_detail(&result, false);

            assert!(
                detail.contains("503"),
                "the result must expose the server status, not only a generic upload failure"
            );
        }

        assert_eq!(
            result.upload_failure.unwrap().stage,
            if cancelled {
                FailureStage::UploadCancelled
            } else {
                FailureStage::Upload
            }
        );
        assert_eq!(result.entry, original);
        assert_eq!(
            result.local_copy.as_deref(),
            Some(std::path::Path::new(&original.local_path))
        );
        assert!(!result.clipboard_copied);
        assert_eq!(storage.history_page(0, 25).unwrap(), vec![original.clone()]);
        assert_eq!(std::fs::read(&original.local_path).unwrap(), [1, 2, 3]);
        assert_eq!(
            std::fs::read_dir(directory.path().join("captures"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            crate::views::history::local_upload_entry(&storage, &history, token.as_str()).unwrap(),
            original,
        );
        assert_eq!(
            window
                .get_history_rows()
                .row_data(0)
                .unwrap()
                .local_path
                .as_str(),
            original.local_path,
        );
    }
}

#[test]
fn history_upload_preserves_new_links_when_original_changes_during_upload() {
    for change in ["removed", "path cleared", "uploaded", "id reused"] {
        let directory = tempfile::tempdir().unwrap();
        let storage = PlatformStorage::open_in(directory.path()).unwrap();
        let concurrent = PlatformStorage::open_in(directory.path()).unwrap();
        let original = local_history_capture(directory.path(), &storage);
        let changed = RefCell::new(None);
        let (endpoint, server) = fake_upload_server(
            "200 OK",
            r#"{"data":{"originalName":"capture.png","size":3,"link":"https://public.example/new","deleteLink":"https://delete.example/new-token","expiresAt":"soon"}}"#,
        );
        let result = process_job_with_history(
            history_upload_job(
                original.clone(),
                endpoint,
                Arc::new(UploadCancellation::default()),
            ),
            &Arc::new(UploadProgress::new()),
            JobCallbacks {
                persist_history: |entry: &mut HistoryEntry, original: Option<&HistoryEntry>| {
                    storage
                        .complete_history_upload(original.unwrap(), entry)
                        .unwrap();
                    None
                },
                capture_complete: || {
                    match change {
                        "removed" => concurrent.remove_history(original.id).unwrap(),

                        "path cleared" => concurrent.clear_history_local_path(original.id).unwrap(),

                        "uploaded" => {
                            let mut other = original.clone();

                            other.link = "https://public.example/other".into();
                            other.delete_url = "https://delete.example/other-token".into();
                            concurrent
                                .complete_history_upload(&original, &mut other)
                                .unwrap();
                        }

                        "id reused" => {
                            concurrent.remove_history(original.id).unwrap();
                            let mut other = original.clone();

                            other.filename = "different-capture.png".into();
                            assert_eq!(concurrent.insert_history(&other).unwrap(), original.id);
                        }

                        _ => unreachable!(),
                    }

                    *changed.borrow_mut() = concurrent.history_entry(original.id).unwrap();
                },
            },
        )
        .unwrap();

        server.join().unwrap();
        assert_eq!(result.entry.link, "https://public.example/new");
        assert_eq!(result.entry.delete_url, "https://delete.example/new-token");
        assert_eq!(
            storage.history_entry(result.entry.id).unwrap().unwrap(),
            result.entry
        );

        if let Some(changed) = changed.borrow().as_ref() {
            assert_ne!(result.entry.id, changed.id);
            assert_eq!(
                storage.history_entry(changed.id).unwrap().as_ref(),
                Some(changed)
            );
            assert_eq!(storage.history_count().unwrap(), 2);
        } else {
            assert_eq!(storage.history_count().unwrap(), 1);
        }

        assert_eq!(std::fs::read(&original.local_path).unwrap(), [1, 2, 3]);
        assert_eq!(
            std::fs::read_dir(directory.path().join("captures"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn history_upload_gates_reject_busy_unsaved_setup_stale_and_missing_sources() {
    use crate::views::{history, settings};
    use sharer::config::AppConfig;

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
    let original = local_history_capture(directory.path(), &storage);
    let mut saved = AppConfig {
        uploader_kind: UploaderKind::Custom,
        uploader_url: "http://127.0.0.1:1/upload".to_owned(),
        ..AppConfig::default()
    };

    saved.behavior.onboarding_complete = true;
    saved.behavior.copy_link_after_upload = false;
    settings::initialize(&window, &saved);
    let history = initialize_test_history(&window, &storage);
    let (sender, receiver) = std::sync::mpsc::channel();
    let cancellation = Arc::new(UploadCancellation::default());

    super::wire_history_upload_callback(&window, &sender, &storage, &history, &cancellation);
    let token = window.get_history_rows().row_data(0).unwrap().action_token;

    for gate in ["busy", "settings", "setup"] {
        window.set_busy(gate == "busy");
        window.set_settings_dirty(gate == "settings");
        window.set_show_onboarding(gate == "setup");
        window.invoke_upload_history_local(token.clone());
        assert!(receiver.try_recv().is_err(), "{gate}");
        assert_eq!(
            storage.history_entry(original.id).unwrap().as_ref(),
            Some(&original)
        );

        if gate == "settings" {
            assert_eq!(window.get_current_page(), 2);
        }
    }

    window.set_busy(false);
    window.set_settings_dirty(false);
    window.set_show_onboarding(false);
    history::refresh_current(&window, &storage, &history).unwrap();
    window.invoke_upload_history_local(token);
    assert!(
        receiver.try_recv().is_err(),
        "stale generation must not upload"
    );
    let token = window.get_history_rows().row_data(0).unwrap().action_token;

    std::fs::remove_file(&original.local_path).unwrap();
    window.invoke_upload_history_local(token.clone());
    assert!(receiver.try_recv().is_err(), "missing file must not upload");
    let failure = window.get_history_action_detail();

    assert!(window.get_history_action_failed() && failure.contains("no longer exists"));
    assert_eq!(
        storage.history_entry(original.id).unwrap().as_ref(),
        Some(&original)
    );
    std::fs::create_dir(&original.local_path).unwrap();
    window.invoke_upload_history_local(token.clone());
    assert!(receiver.try_recv().is_err(), "directories must not upload");
    std::fs::remove_dir(&original.local_path).unwrap();
    std::fs::write(&original.local_path, [1, 2, 3]).unwrap();
    storage.clear_history_local_path(original.id).unwrap();
    window.invoke_upload_history_local(token);
    assert!(
        receiver.try_recv().is_err(),
        "changed local path must not upload"
    );
    history::refresh_current(&window, &storage, &history).unwrap();
    window.invoke_upload_history_local(window.get_history_rows().row_data(0).unwrap().action_token);
    assert!(
        receiver.try_recv().is_err(),
        "entries without local paths must not upload"
    );
    let mut removed = original.clone();

    removed.id = storage.insert_history(&removed).unwrap();
    history::refresh_current(&window, &storage, &history).unwrap();
    let removed_token = window.get_history_rows().row_data(0).unwrap().action_token;

    storage.remove_history(removed.id).unwrap();
    window.invoke_upload_history_local(removed_token);
    assert!(
        receiver.try_recv().is_err(),
        "removed entries must not upload"
    );
    let mut uploaded = original;

    uploaded.link = "https://public.example/already-uploaded".into();
    uploaded.id = storage.insert_history(&uploaded).unwrap();
    history::refresh_current(&window, &storage, &history).unwrap();
    let uploaded_id = uploaded.id.to_string();
    let uploaded_token = window
        .get_history_rows()
        .iter()
        .find(|row| row.entry_id == uploaded_id.as_str())
        .unwrap()
        .action_token;

    window.invoke_upload_history_local(uploaded_token);
    assert!(
        receiver.try_recv().is_err(),
        "uploaded entries must not upload"
    );
}

#[test]
fn history_upload_rechecks_missing_files_in_worker_and_enforces_session_proxy() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let original = local_history_capture(directory.path(), &storage);
    let (sender, receiver) = std::sync::mpsc::channel();

    sender
        .send(history_upload_job(
            original.clone(),
            "http://127.0.0.1:1/upload".into(),
            Arc::new(UploadCancellation::default()),
        ))
        .unwrap();
    let job = receive_job(&receiver, true).unwrap();

    assert!(job.upload.as_ref().unwrap().require_tor);
    std::fs::remove_file(&original.local_path).unwrap();
    let error = process_job_with_history(
        job,
        &Arc::new(UploadProgress::new()),
        JobCallbacks {
            persist_history: |_: &mut HistoryEntry, _: Option<&HistoryEntry>| {
                panic!("missing local files must not change history")
            },
            capture_complete: || {},
        },
    )
    .unwrap_err();

    assert_eq!(error.stage, FailureStage::Upload);
    assert_eq!(
        storage.history_entry(original.id).unwrap().as_ref(),
        Some(&original)
    );
    assert_eq!(storage.history_count().unwrap(), 1);
}

#[test]
fn worker_enforces_the_session_requirement_for_every_queued_upload() {
    let directory = tempfile::tempdir().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();

    // Both initial settings and later edits can produce jobs without the saved Tor preference.
    for saved_requirement in [false, true, false] {
        let mut upload = custom_upload("https://uploads.example.test/upload".to_owned());

        upload.require_tor = saved_requirement;
        sender
            .send(prepared_capture(
                directory.path(),
                upload,
                Arc::new(UploadCancellation::default()),
            ))
            .unwrap();
        let job = receive_job(&receiver, true).unwrap();

        assert!(job.upload.unwrap().require_tor);
    }
}

#[test]
fn normal_sessions_preserve_each_jobs_tor_preference_and_local_captures() {
    let directory = tempfile::tempdir().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();

    for saved_requirement in [false, true] {
        let mut upload = custom_upload("https://uploads.example.test/upload".to_owned());

        upload.require_tor = saved_requirement;
        sender
            .send(prepared_capture(
                directory.path(),
                upload,
                Arc::new(UploadCancellation::default()),
            ))
            .unwrap();
        let job = receive_job(&receiver, false).unwrap();

        assert_eq!(job.upload.unwrap().require_tor, saved_requirement);
    }

    let mut local = prepared_capture(
        directory.path(),
        custom_upload(String::new()),
        Arc::new(UploadCancellation::default()),
    );

    local.upload = None;
    sender.send(local).unwrap();

    assert!(receive_job(&receiver, true).unwrap().upload.is_none());
}

#[test]
fn session_requirement_does_not_change_saved_settings() {
    let directory = tempfile::tempdir().unwrap();
    let storage = sharer::storage::PlatformStorage::open_in(directory.path()).unwrap();
    let config = sharer::config::AppConfig::default();
    let (sender, receiver) = std::sync::mpsc::channel();

    config.save(&storage).unwrap();
    sender
        .send(prepared_capture(
            directory.path(),
            custom_upload("https://uploads.example.test/upload".to_owned()),
            Arc::new(UploadCancellation::default()),
        ))
        .unwrap();

    assert!(
        receive_job(&receiver, true)
            .unwrap()
            .upload
            .unwrap()
            .require_tor
    );

    // An unrelated settings save must not persist the session-only override.
    let mut next = sharer::config::AppConfig::load(&storage).unwrap();

    next.behavior.completion_sound = true;
    next.save(&storage).unwrap();

    assert!(
        !sharer::config::AppConfig::load(&storage)
            .unwrap()
            .privacy
            .require_tor
    );
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
fn closing_to_tray_preserves_settings_and_setup_drafts() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();

    for (dirty, setup) in [(true, false), (false, true), (false, false)] {
        let window = AppWindow::new().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
        let config = Rc::new(RefCell::new(sharer::config::AppConfig::default()));
        let local_deleted: crate::views::history::LocalDeletionObserver = Rc::new(|_, _, _| {});
        let history_removed: crate::views::history::HistoryRemovalObserver = Rc::new(|_, _| {});
        let history = crate::views::history::initialize(
            &window,
            &storage,
            false,
            &local_deleted,
            &history_removed,
        )
        .unwrap();
        let close_requested = Rc::new(Cell::new(false));

        window.set_tray_available(true);
        window.set_settings_dirty(dirty);
        window.set_show_onboarding(setup);
        window.set_uploader_url("https://unsaved.example/upload".into());
        window.set_wizard_provider(3);
        super::wire_close_callback(&window, &history, &close_requested, &config);
        window.show().unwrap();

        let ui = Rc::new(RefCell::new(Some(super::UiSession {
            close_requested,
            history,
            transient: false,
            window: window.clone_strong(),
        })));

        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::CloseRequested);
        super::release_requested_ui(&ui);

        assert!(!window.window().is_visible());
        assert_eq!(ui.borrow().is_some(), dirty || setup);

        if dirty || setup {
            let reopened = super::current_window(&ui).unwrap();

            super::show_ui(&ui, &reopened).unwrap();
            assert!(reopened.window().is_visible());
            assert_eq!(reopened.get_settings_dirty(), dirty);
            assert_eq!(reopened.get_show_onboarding(), setup);
            assert_eq!(
                reopened.get_uploader_url(),
                "https://unsaved.example/upload"
            );
            assert_eq!(reopened.get_wizard_provider(), 3);
            reopened.hide().unwrap();
        }
    }
}

#[test]
fn cancelled_quit_returns_to_settings_without_discarding_the_draft() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();

    window.set_uploader_url("https://unsaved.example/upload".into());
    window.set_settings_dirty(true);
    window.set_current_page(0);

    assert!(!super::may_quit_with_draft(&window, || false));
    assert!(window.window().is_visible());
    assert_eq!(window.get_current_page(), 2);
    assert!(window.get_settings_dirty());
    assert_eq!(window.get_uploader_url(), "https://unsaved.example/upload");
    assert!(super::may_quit_with_draft(&window, || true));

    window.set_settings_dirty(false);
    assert!(super::may_quit_with_draft(&window, || {
        panic!("saved settings must not trigger a discard prompt")
    }));
    window.hide().unwrap();
}

#[test]
fn history_action_feedback_preserves_capture_and_upload_status() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();

    for (status, detail, uploading) in [
        ("Capture failed", "No screen could be captured", false),
        ("Uploaded", "Link copied to clipboard", false),
        ("Uploading...", "Waiting for the server response", true),
    ] {
        window.set_status_text(status.into());
        window.set_status_detail(detail.into());
        window.set_busy(uploading);
        window.set_uploading(uploading);

        crate::views::history::set_action_status(&window, "Local capture no longer exists", true);
        assert_eq!(window.get_status_text(), status);
        assert_eq!(window.get_status_detail(), detail);
        assert_eq!(
            window.get_history_action_detail(),
            "Local capture no longer exists"
        );
        assert!(window.get_history_action_failed());

        crate::views::history::set_action_status(&window, "Public link copied to clipboard", false);
        assert_eq!(window.get_status_text(), status);
        assert_eq!(window.get_status_detail(), detail);
        assert_eq!(
            window.get_history_action_detail(),
            "Public link copied to clipboard"
        );
        assert!(!window.get_history_action_failed());
    }
}

#[test]
fn new_upload_clears_previous_history_feedback() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();

    window.set_uploader_kind(UploaderKind::Uguu.index());
    window.set_current_page(1);
    crate::views::history::set_action_status(&window, "Local capture no longer exists", true);

    super::queue_job(
        &window,
        &sender,
        super::JobSource::Clipboard,
        Arc::new(UploadCancellation::default()),
    );

    assert!(receiver.try_recv().is_ok());
    assert!(window.get_busy());
    assert!(window.get_history_action_detail().is_empty());
    assert!(!window.get_history_action_failed());

    // Completing the job cannot reveal feedback from the previous history action.
    window.set_status_text("Uploaded".into());
    window.set_status_detail("Link copied to clipboard".into());
    window.set_uploading(false);
    window.set_busy(false);
    assert!(window.get_history_action_detail().is_empty());
    assert_eq!(window.get_status_text(), "Uploaded");
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
            persist_history: |_entry: &mut HistoryEntry, _original: Option<&HistoryEntry>| {
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
                persist_history: |_entry: &mut HistoryEntry, _original: Option<&HistoryEntry>| None,
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
    assert!(
        activity
            .borrow()
            .as_ref()
            .unwrap()
            .detail
            .contains("Local file no longer available")
    );
    assert_eq!(activity.borrow().as_ref().unwrap().status, "Saved locally");

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
            persist_history: |entry: &mut HistoryEntry, _original: Option<&HistoryEntry>| {
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
fn upload_without_clipboard_copy_retains_local_save_failure() {
    let directory = tempfile::tempdir().unwrap();
    let not_a_directory = directory.path().join("capture-root");

    std::fs::write(&not_a_directory, b"not a directory").unwrap();
    let (endpoint, server) = fake_upload_server(
        "200 OK",
        r#"{"data":{"originalName":"capture.png","size":3,"link":"https://public.example/capture","deleteLink":"https://public.example/delete/fake-token","expiresAt":"soon"}}"#,
    );
    let mut upload = custom_upload(endpoint);

    upload.copy_link = false;
    let result = process_job_with_history(
        prepared_capture(
            &not_a_directory,
            upload,
            Arc::new(UploadCancellation::default()),
        ),
        &Arc::new(UploadProgress::new()),
        JobCallbacks {
            persist_history: |_entry: &mut HistoryEntry, _original: Option<&HistoryEntry>| None,
            capture_complete: || {},
        },
    )
    .unwrap();

    server.join().unwrap();
    assert!(!result.entry.link.is_empty());
    assert!(result.entry.local_path.is_empty());
    assert!(result.local_copy.is_none());
    assert!(result.capture_warning.is_some());
    assert!(!result.clipboard_copied);
    assert!(result.clipboard_warning.is_none());
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
            persist_history: |entry: &mut HistoryEntry, _original: Option<&HistoryEntry>| {
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
            persist_history: |_entry: &mut HistoryEntry, _original: Option<&HistoryEntry>| None,
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
            persist_history: |_entry: &mut HistoryEntry, _original: Option<&HistoryEntry>| {
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
        preferred_renderer(RendererPreference::Auto, true),
        Some("software")
    );
    assert_eq!(preferred_renderer(RendererPreference::Auto, false), None);
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
fn mp4_setting_round_trips_and_marks_settings_dirty() {
    use crate::views::settings;
    use sharer::config::{AppConfig, RecordingFormat};

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let config = AppConfig::default();

    settings::initialize(&window, &config);
    let saved_config = Rc::new(RefCell::new(config));

    settings::wire_dirty_callback(&window, &saved_config);
    assert!(!window.get_recording_mp4());
    window.set_recording_mp4(true);
    window.invoke_settings_edited();
    assert!(window.get_settings_dirty());
    window.set_recording_mp4(false);
    window.invoke_settings_edited();
    assert!(!window.get_settings_dirty());
    window.set_recording_mp4(true);
    let saved = settings::config_from_window(&window).unwrap();

    assert_eq!(saved.recording_format, RecordingFormat::Mp4);
    *saved_config.borrow_mut() = saved.clone();
    settings::initialize(&window, &saved);
    assert!(window.get_recording_mp4());
    assert!(!window.get_settings_dirty());
    window.set_recording_mp4(false);
    window.invoke_settings_edited();
    assert!(window.get_settings_dirty());
    assert_eq!(
        settings::config_from_window(&window)
            .unwrap()
            .recording_format,
        RecordingFormat::Webp
    );
}

#[test]
fn gui_endpoint_edits_clear_saved_headers_and_queued_targets_keep_their_destination() {
    use crate::views::settings;
    use sharer::config::AppConfig;

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let config = AppConfig {
        uploader_kind: UploaderKind::Custom,
        uploader_url: "https://uploads.example/upload".to_owned(),
        upload_header_name: "X-Api-Key".to_owned(),
        upload_header_value: "fake-saved-secret".to_owned(),
        upload_header_endpoint: Some("https://uploads.example/upload".to_owned()),
        ..AppConfig::default()
    };

    settings::initialize(&window, &config);
    settings::wire_dirty_callback(&window, &Rc::new(RefCell::new(config)));
    let queued = settings::upload_target_from_window(&window).unwrap();

    // A raw property update cannot authorize the saved header for a new target.
    window.set_uploader_url("https://other.example/upload".into());
    assert!(settings::upload_target_from_window(&window).is_err());

    // The endpoint field's edit callback clears the old credentials immediately.
    window.invoke_custom_endpoint_edited();
    assert!(window.get_upload_header_name().is_empty());
    assert!(window.get_upload_header_value().is_empty());
    assert!(settings::upload_target_from_window(&window).is_ok());
    assert_eq!(queued.custom_url, "https://uploads.example/upload");
    assert_eq!(queued.header_value, "fake-saved-secret");
    assert!(queued.validate().is_ok());

    window.set_upload_header_name("X-Api-Key".into());
    window.set_upload_header_value("fake-new-secret".into());
    window.invoke_custom_header_edited();
    let target = settings::upload_target_from_window(&window).unwrap();

    assert_eq!(target.header_endpoint, "https://other.example/upload");
    assert_eq!(target.header_value, "fake-new-secret");
}

#[test]
fn gui_round_trips_recording_limits() {
    use crate::views::settings;
    use sharer::config::AppConfig;

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();

    settings::initialize(&window, &AppConfig::default());
    window.set_recording_max_gib(3);
    window.set_recording_max_seconds(3_600);
    window.set_recording_fps(30);

    let config = settings::config_from_window(&window).unwrap();

    assert_eq!(config.recording_max_gib, 3);
    assert_eq!(config.recording_max_seconds, 3_600);
    assert_eq!(config.recording_fps, 30);
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

#[test]
fn capture_completion_releases_stop_control_and_failure_clears_receipt_capabilities() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = sharer::storage::PlatformStorage::open_in(directory.path()).unwrap();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (stop_tx, _stop_rx) = std::sync::mpsc::channel();
    let state = super::PollingState {
        activity: Rc::new(RefCell::new(None)),
        hotkeys: Rc::new(RefCell::new(None)),
        job_tx: std::sync::mpsc::channel().0,
        result_rx,
        recording_stop: Rc::new(RefCell::new(Some(stop_tx))),
        storage: Rc::new(storage),
        tray: Rc::new(RefCell::new(None)),
        ui: Rc::new(RefCell::new(None)),
        update_rx: std::sync::mpsc::channel().1,
        update_state: Rc::new(RefCell::new(None)),
        upload_progress: Arc::new(UploadProgress::new()),
        upload_cancellation: Arc::new(UploadCancellation::default()),
        quit_after_job: Rc::new(Cell::new(false)),
        instance_show_rx: std::sync::mpsc::channel().1,
    };
    let mut status = String::new();

    window.set_busy(true);
    window.set_recording(true);
    result_tx
        .send(super::JobResult::CaptureComplete {
            restore_window: false,
        })
        .unwrap();
    super::receive_job_result(&window, &state.result_rx, &state, &mut status);

    assert!(!window.get_recording());
    assert!(window.get_busy());
    assert!(state.recording_stop.borrow().is_none());
    window.set_last_link("https://public.example/previous".into());
    window.set_last_local_path("previous.webp".into());
    window.set_last_delete_url("https://delete.example/fake-capability".into());
    window.set_last_delete_saved(true);
    result_tx
        .send(super::JobResult::Error {
            message: "capture unavailable".to_owned(),
            stage: FailureStage::Capture,
        })
        .unwrap();
    super::receive_job_result(&window, &state.result_rx, &state, &mut status);

    assert!(!window.get_busy());
    assert!(window.get_last_link().is_empty());
    assert!(window.get_last_local_path().is_empty());
    assert!(window.get_last_delete_url().is_empty());
    assert!(!window.get_last_delete_saved());
    let activity = state.activity.borrow();
    let activity = activity.as_ref().unwrap();

    assert!(activity.link.is_empty());
    assert!(activity.local_path.is_empty());
    assert!(activity.delete_url.is_empty());
    assert!(!activity.history_saved);
}

#[test]
fn removing_latest_history_marks_its_receipt_unsaved_without_losing_recovery_links() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let activity = RefCell::new(Some(ActivitySnapshot {
        entry_id: 7,
        delete_url: "https://delete.example/fake-capability".to_owned(),
        detail: String::new(),
        history_saved: true,
        link: "https://public.example/capture".to_owned(),
        local_path: "capture.webp".to_owned(),
        status: String::new(),
    }));

    super::apply_activity(&window, activity.borrow().as_ref().unwrap());
    assert!(!super::result::invalidate_latest_history_saved(
        &window, &activity, 8
    ));
    assert!(window.get_last_delete_saved());
    assert!(activity.borrow().as_ref().unwrap().history_saved);
    assert!(super::result::invalidate_latest_history_saved(
        &window, &activity, 7
    ));
    assert!(!window.get_last_delete_saved());
    assert!(!activity.borrow().as_ref().unwrap().history_saved);
    assert!(!window.get_last_delete_url().is_empty());
    assert!(!window.get_last_local_path().is_empty());
    assert!(!window.get_last_link().is_empty());

    window.set_last_delete_saved(true);
    super::apply_activity(&window, activity.borrow().as_ref().unwrap());

    assert!(!window.get_last_delete_saved());
}

#[test]
fn setup_replay_cancel_preserves_saved_settings_and_invalid_drafts_are_recoverable() {
    use crate::views::settings;
    use sharer::config::{AppConfig, ProviderCredentials, ShortcutConfig};
    use sharer::storage::{PlatformStorage, Storage as _};

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
    let mut saved = AppConfig {
        uploader_kind: UploaderKind::Custom,
        uploader_url: "https://uploads.example.test/upload".to_owned(),
        upload_header_name: "X-Api-Key".to_owned(),
        upload_header_value: "fake-saved-key".to_owned(),
        upload_header_endpoint: Some("https://uploads.example.test/upload".to_owned()),
        uploader_credentials: ProviderCredentials {
            imgur: "fake-imgur-id".to_owned(),
            vgy: "fake-vgy-key".to_owned(),
            pixeldrain: "fake-pixeldrain-key".to_owned(),
            sul: "fake-sul-key".to_owned(),
        },
        capture_directory: directory.path().to_string_lossy().into_owned(),
        shortcuts: ShortcutConfig {
            region: String::new(),
            recording: String::new(),
            screen: String::new(),
            clipboard: String::new(),
        },
        lifetime_seconds: 90,
        litterbox_retention: sharer::upload::LitterboxRetention::TwelveHours,
        ..AppConfig::default()
    };

    saved.behavior.onboarding_complete = true;
    saved.behavior.completion_sound = false;
    saved.save(storage.as_ref()).unwrap();
    let saved_bytes = storage.read("application").unwrap();
    let config = Rc::new(RefCell::new(saved.clone()));
    let hotkeys = Rc::new(RefCell::new(None));
    let hotkey_status = Rc::new(RefCell::new(String::new()));

    settings::initialize(&window, &saved);
    settings::wire_dirty_callback(&window, &config);
    super::onboarding::wire_onboarding_callbacks(
        &window,
        &hotkeys,
        &hotkey_status,
        &config,
        &storage,
    );

    window.invoke_onboarding_restart();
    assert!(window.get_show_onboarding());
    window.set_wizard_provider(UploaderKind::Litterbox.index());
    window.set_wizard_litterbox_retention(0);
    window.set_wizard_provider(-1);
    window.set_wizard_start_at_login(true);
    window.set_wizard_imgur_credential("unsaved-key".into());
    window.invoke_onboarding_skip();
    assert!(!window.get_show_onboarding());
    assert_eq!(*config.borrow(), saved);
    assert_eq!(storage.read("application").unwrap(), saved_bytes);
    assert_eq!(window.get_imgur_credential().as_str(), "fake-imgur-id");
    assert!(!window.get_start_at_login());
    assert_eq!(window.get_litterbox_retention(), 1);

    window.invoke_onboarding_restart();
    window.set_wizard_uploader_url("not a URL".into());
    window.invoke_wizard_endpoint_edited();
    window.invoke_onboarding_continue();
    assert_eq!(window.get_onboarding_step(), 0);
    assert!(!window.get_onboarding_error().is_empty());
    assert_eq!(storage.read("application").unwrap(), saved_bytes);
    assert_eq!(window.get_upload_header_value().as_str(), "fake-saved-key");
    window.set_wizard_uploader_url("https://other.example.test/upload".into());
    window.invoke_wizard_endpoint_edited();
    assert!(window.get_wizard_header_value().is_empty());
    window.set_wizard_header_name("X-Api-Key".into());
    window.set_wizard_header_value("fake-new-key".into());
    window.invoke_wizard_header_edited();
    window.invoke_onboarding_continue();
    assert_eq!(window.get_onboarding_step(), 1);
    window.invoke_onboarding_finish();
    assert!(!window.get_show_onboarding());
    let committed = AppConfig::load(storage.as_ref()).unwrap();

    assert_eq!(committed.uploader_url, "https://other.example.test/upload");
    assert_eq!(
        committed.upload_header_endpoint.as_deref(),
        Some(committed.uploader_url.as_str())
    );
    assert_eq!(committed.upload_header_value, "fake-new-key");
    assert_eq!(committed.uploader_credentials, saved.uploader_credentials);
    assert_eq!(committed.lifetime_seconds, 90);
    assert_eq!(committed.litterbox_retention, saved.litterbox_retention);
    assert!(!committed.behavior.completion_sound);
}

#[test]
fn settings_reset_is_a_draft_and_preserves_connections_and_capture_folder() {
    use crate::views::settings;
    use sharer::config::AppConfig;

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut saved = AppConfig {
        uploader_kind: UploaderKind::Sul,
        capture_directory: directory.path().to_string_lossy().into_owned(),
        uploader_url: "https://uploads.example.test/upload".to_owned(),
        upload_header_name: "X-Api-Key".to_owned(),
        upload_header_value: "fake-key".to_owned(),
        upload_header_endpoint: Some("https://uploads.example.test/upload".to_owned()),
        ..AppConfig::default()
    };

    saved.uploader_credentials.sul = "fake-sul-key".to_owned();
    saved.behavior.onboarding_complete = true;
    saved.behavior.auto_upload_captures = false;
    saved.behavior.completion_sound = false;
    settings::initialize(&window, &saved);
    let reset = settings::reset_preferences(&saved);

    settings::initialize_wizard(&window, &reset, true);
    let draft = settings::wizard_config_from_window(&window, &saved).unwrap();

    assert_eq!(draft.uploader_kind, saved.uploader_kind);
    assert_eq!(draft.uploader_credentials, saved.uploader_credentials);
    assert_eq!(draft.uploader_url, saved.uploader_url);
    assert_eq!(draft.upload_header_value, saved.upload_header_value);
    assert_eq!(draft.upload_header_endpoint, saved.upload_header_endpoint);
    assert_eq!(draft.capture_directory, saved.capture_directory);
    assert!(draft.behavior.auto_upload_captures);
    assert!(draft.behavior.completion_sound);
    assert!(!window.get_auto_upload_captures());
    assert!(!window.get_completion_sound());
}

#[test]
fn first_run_local_only_exit_ignores_invalid_wizard_inputs() {
    use crate::views::settings;
    use sharer::config::AppConfig;
    use sharer::storage::PlatformStorage;

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
    let config = Rc::new(RefCell::new(AppConfig::default()));
    let hotkeys = Rc::new(RefCell::new(None));
    let hotkey_status = Rc::new(RefCell::new(String::new()));

    settings::initialize(&window, &config.borrow());
    super::onboarding::wire_onboarding_callbacks(
        &window,
        &hotkeys,
        &hotkey_status,
        &config,
        &storage,
    );
    window.set_wizard_provider(0);
    window.set_wizard_uploader_url("invalid".into());
    window.set_wizard_start_at_login(true);
    window.invoke_onboarding_skip();

    let saved = AppConfig::load(storage.as_ref()).unwrap();

    assert!(saved.behavior.onboarding_complete);
    assert!(saved.behavior.save_captures);
    assert!(!saved.behavior.auto_upload_captures);
    assert!(!saved.behavior.start_at_login);
    assert!(saved.uploader_url.is_empty());
    assert!(!window.get_show_onboarding());
}

#[test]
fn exact_lifetimes_survive_gui_edits_setup_and_queued_uploads() {
    use crate::views::settings;
    use sharer::config::AppConfig;

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();

    for seconds in [0, 1, 60, 90, 5_400, 604_800] {
        let window = AppWindow::new().unwrap();
        let mut saved = AppConfig {
            uploader_kind: UploaderKind::Custom,
            uploader_url: "https://uploads.example.test/upload".to_owned(),
            lifetime_seconds: seconds,
            capture_directory: "captures-kept".to_owned(),
            ..AppConfig::default()
        };

        saved.behavior.onboarding_complete = true;
        settings::initialize(&window, &saved);
        assert_eq!(window.get_lifetime_seconds(), seconds.cast_signed());
        assert_eq!(super::valid_window_lifetime(&window), Some(seconds));
        window.set_completion_sound(false);
        assert_eq!(
            settings::config_from_window(&window)
                .unwrap()
                .lifetime_seconds,
            seconds
        );
        assert_eq!(
            settings::wizard_config_from_window(&window, &saved)
                .unwrap()
                .lifetime_seconds,
            seconds
        );

        let (sender, receiver) = std::sync::mpsc::channel();

        super::queue_job(
            &window,
            &sender,
            JobSource::Clipboard,
            Arc::new(UploadCancellation::default()),
        );
        assert_eq!(
            receiver
                .try_recv()
                .unwrap()
                .upload
                .unwrap()
                .lifetime_seconds,
            seconds
        );
    }
}

#[test]
fn replay_supports_every_provider_and_local_only_keeps_connection_secrets() {
    use crate::views::settings;
    use sharer::config::{AppConfig, ProviderCredentials};

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut saved = AppConfig {
        uploader_url: "https://uploads.example.test/upload".to_owned(),
        upload_header_name: "X-Api-Key".to_owned(),
        upload_header_value: "fake-custom-key".to_owned(),
        upload_header_endpoint: Some("https://uploads.example.test/upload".to_owned()),
        uploader_credentials: ProviderCredentials {
            imgur: "fake-imgur-id".to_owned(),
            vgy: "fake-vgy-key".to_owned(),
            pixeldrain: "fake-pixeldrain-key".to_owned(),
            sul: "fake-sul-key".to_owned(),
        },
        capture_directory: directory.path().to_string_lossy().into_owned(),
        ..AppConfig::default()
    };

    saved.behavior.onboarding_complete = true;
    saved.behavior.save_captures = false;

    for kind in UploaderKind::AVAILABLE {
        let index = kind.index();

        saved.uploader_kind = kind;
        settings::initialize_wizard(&window, &saved, false);
        assert_eq!(window.get_wizard_provider(), index);
        assert_eq!(
            settings::wizard_config_from_window(&window, &saved).unwrap(),
            saved
        );
    }

    window.set_wizard_uploader_url("invalid-unsaved-endpoint".into());
    window.set_wizard_sul_credential("unsaved-secret".into());
    window.set_wizard_provider(-1);
    let local = settings::wizard_config_from_window(&window, &saved).unwrap();

    assert!(local.behavior.save_captures);
    assert!(!local.behavior.auto_upload_captures);
    assert_eq!(local.uploader_kind, saved.uploader_kind);
    assert_eq!(local.uploader_credentials, saved.uploader_credentials);
    assert_eq!(local.uploader_url, saved.uploader_url);
    assert_eq!(local.upload_header_value, saved.upload_header_value);
}

#[test]
fn unsaved_settings_and_open_setup_cannot_start_capture_or_upload_jobs() {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();

    window.set_settings_dirty(true);
    assert!(!super::can_start_job(&window));
    assert_eq!(window.get_current_page(), 2);
    window.set_settings_dirty(false);
    window.set_show_onboarding(true);
    assert!(!super::can_start_job(&window));
    window.set_show_onboarding(false);
    window.set_busy(true);
    assert!(!super::can_start_job(&window));
    window.set_busy(false);
    assert!(super::can_start_job(&window));
}

#[test]
fn wizard_custom_headers_require_explicit_authorization_for_a_new_endpoint() {
    use crate::views::settings;
    use sharer::config::AppConfig;

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let saved = AppConfig {
        uploader_kind: UploaderKind::Custom,
        uploader_url: "https://uploads.example.test/upload".to_owned(),
        upload_header_name: "X-Api-Key".to_owned(),
        upload_header_value: "fake-saved-key".to_owned(),
        upload_header_endpoint: Some("https://uploads.example.test/upload".to_owned()),
        capture_directory: "captures-kept".to_owned(),
        ..AppConfig::default()
    };

    settings::initialize(&window, &saved);
    settings::wire_dirty_callback(&window, &Rc::new(RefCell::new(saved.clone())));
    window.set_wizard_uploader_url("https://other.example.test/upload".into());
    assert!(settings::wizard_config_from_window(&window, &saved).is_err());
    window.invoke_wizard_endpoint_edited();
    assert!(window.get_wizard_header_value().is_empty());
    assert!(settings::wizard_config_from_window(&window, &saved).is_ok());
    window.set_wizard_header_name("X-Api-Key".into());
    window.set_wizard_header_value("fake-new-key".into());
    window.invoke_wizard_header_edited();
    let authorized = settings::wizard_config_from_window(&window, &saved).unwrap();

    assert_eq!(
        authorized.upload_header_endpoint.as_deref(),
        Some(authorized.uploader_url.as_str())
    );
    assert_eq!(authorized.upload_header_value, "fake-new-key");
    assert_eq!(saved.upload_header_value, "fake-saved-key");
}

#[test]
fn litterbox_settings_preserve_custom_expiry_and_queue_the_selected_retention() {
    use crate::views::settings;
    use sharer::{config::AppConfig, upload::LitterboxRetention};

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let directory = tempfile::tempdir().unwrap();

    for seconds in [0, 90] {
        let window = AppWindow::new().unwrap();
        let mut saved = AppConfig {
            uploader_kind: UploaderKind::Litterbox,
            lifetime_seconds: seconds,
            capture_directory: directory.path().to_string_lossy().into_owned(),
            ..AppConfig::default()
        };

        saved.behavior.onboarding_complete = true;
        settings::initialize(&window, &saved);
        let saved_config = Rc::new(RefCell::new(saved));

        settings::wire_dirty_callback(&window, &saved_config);
        assert_eq!(window.get_litterbox_retention(), 3);
        window.set_litterbox_retention(1);
        window.invoke_settings_edited();
        assert!(window.get_settings_dirty());
        let selected = settings::config_from_window(&window).unwrap();

        assert_eq!(selected.lifetime_seconds, seconds);
        assert_eq!(
            selected.litterbox_retention,
            LitterboxRetention::TwelveHours
        );
        *saved_config.borrow_mut() = selected.clone();
        settings::initialize(&window, &selected);
        assert!(!window.get_settings_dirty());
        assert_eq!(window.get_litterbox_retention(), 1);
        let (sender, receiver) = std::sync::mpsc::channel();

        super::queue_job(
            &window,
            &sender,
            JobSource::Clipboard,
            Arc::new(UploadCancellation::default()),
        );
        let upload = receiver.try_recv().unwrap().upload.unwrap();

        assert_eq!(
            upload.uploader.litterbox_retention,
            LitterboxRetention::TwelveHours
        );
        assert_eq!(upload.lifetime_seconds, seconds);
        assert_eq!(window.get_status_detail().as_str(), "12 hours (requested)");
    }
}

#[test]
fn litterbox_wizard_changes_only_its_own_retention_when_selected() {
    use crate::views::settings;
    use sharer::{config::AppConfig, upload::LitterboxRetention};

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut saved = AppConfig {
        uploader_kind: UploaderKind::Litterbox,
        lifetime_seconds: 90,
        litterbox_retention: LitterboxRetention::TwelveHours,
        capture_directory: directory.path().to_string_lossy().into_owned(),
        ..AppConfig::default()
    };

    saved.behavior.onboarding_complete = true;
    settings::initialize_wizard(&window, &saved, false);
    assert_eq!(window.get_wizard_litterbox_retention(), 1);
    window.set_wizard_litterbox_retention(0);
    let applied = settings::wizard_config_from_window(&window, &saved).unwrap();

    assert_eq!(applied.litterbox_retention, LitterboxRetention::OneHour);
    assert_eq!(applied.lifetime_seconds, 90);
    assert_eq!(saved.litterbox_retention, LitterboxRetention::TwelveHours);

    for provider in [-1, UploaderKind::Catbox.index()] {
        window.set_wizard_provider(provider);
        let other = settings::wizard_config_from_window(&window, &saved).unwrap();

        assert_eq!(other.litterbox_retention, saved.litterbox_retention);
        assert_eq!(other.lifetime_seconds, 90);
    }

    settings::initialize_wizard(&window, &saved, false);
    window.set_wizard_litterbox_retention(4);
    assert!(settings::wizard_config_from_window(&window, &saved).is_err());
    let reset = settings::reset_preferences(&saved);

    assert_eq!(
        reset.litterbox_retention,
        LitterboxRetention::SeventyTwoHours
    );
}

#[test]
fn litterbox_finish_save_and_discard_persist_only_confirmed_retention() {
    use crate::views::settings;
    use sharer::{config::AppConfig, storage::Storage as _, upload::LitterboxRetention};

    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
    let mut saved = AppConfig {
        uploader_kind: UploaderKind::Litterbox,
        lifetime_seconds: 90,
        capture_directory: directory.path().to_string_lossy().into_owned(),
        ..AppConfig::default()
    };

    saved.behavior.onboarding_complete = true;
    saved.save(storage.as_ref()).unwrap();
    let config = Rc::new(RefCell::new(saved));
    let hotkeys = Rc::new(RefCell::new(None));
    let hotkey_status = Rc::new(RefCell::new(String::new()));

    settings::initialize(&window, &config.borrow());
    settings::wire_dirty_callback(&window, &config);
    super::onboarding::wire_onboarding_callbacks(
        &window,
        &hotkeys,
        &hotkey_status,
        &config,
        &storage,
    );
    super::preferences::wire_settings_callbacks(
        &window,
        &hotkeys,
        &hotkey_status,
        &config,
        &storage,
    );
    window.invoke_onboarding_restart();
    window.set_wizard_litterbox_retention(2);
    window.invoke_onboarding_continue();
    assert_eq!(window.get_onboarding_step(), 1);
    window.invoke_onboarding_finish();
    assert!(!window.get_show_onboarding());
    let finished = AppConfig::load(storage.as_ref()).unwrap();

    assert_eq!(
        finished.litterbox_retention,
        LitterboxRetention::TwentyFourHours
    );
    assert_eq!(finished.lifetime_seconds, 90);
    assert_eq!(window.get_litterbox_retention(), 2);
    let saved_bytes = storage.read("application").unwrap();

    window.set_litterbox_retention(0);
    window.invoke_settings_edited();
    assert!(window.get_settings_dirty());
    window.invoke_discard_settings();
    assert_eq!(window.get_litterbox_retention(), 2);
    assert_eq!(storage.read("application").unwrap(), saved_bytes);
    window.set_litterbox_retention(1);
    window.invoke_save_settings();
    let applied = AppConfig::load(storage.as_ref()).unwrap();

    assert_eq!(applied.litterbox_retention, LitterboxRetention::TwelveHours);
    assert_eq!(applied.lifetime_seconds, 90);
    assert_eq!(window.get_litterbox_retention(), 1);
    assert!(!window.get_settings_dirty());
}
