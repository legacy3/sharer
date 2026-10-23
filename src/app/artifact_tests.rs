//! Capture workbench actions and local-artifact retention regressions.

use super::*;
use sharer::upload::UploaderKind;

struct ArtifactFixture {
    window: AppWindow,
    storage: Rc<PlatformStorage>,
    activity: Rc<RefCell<Option<ActivitySnapshot>>>,
    receiver: Receiver<Job>,
    entry: HistoryEntry,
    directory: tempfile::TempDir,
}

impl ArtifactFixture {
    fn new() -> Self {
        slint::platform::set_platform(Box::new(super::tests::SoftwareTestPlatform)).ok();
        let window = AppWindow::new().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
        let path = directory.path().join("capture.png");

        std::fs::write(&path, [1, 2, 3]).unwrap();
        let mut entry = HistoryEntry::local("capture.png".to_owned(), 3, &path);

        entry.id = storage.insert_history(&entry).unwrap();
        let activity = Rc::new(RefCell::new(None));
        let (sender, receiver) = mpsc::channel();
        let mut config = AppConfig {
            uploader_kind: UploaderKind::Uguu,
            capture_directory: directory.path().to_string_lossy().into_owned(),
            ..AppConfig::default()
        };

        config.behavior.onboarding_complete = true;
        views::settings::initialize(&window, &config);
        let resources = CallbackResources {
            activity: Rc::clone(&activity),
            history_loader: Rc::new(RefCell::new(
                views::history::HistoryLoader::new(&storage).unwrap(),
            )),
            hotkeys: Rc::new(RefCell::new(None)),
            hotkey_status: Rc::new(RefCell::new(String::new())),
            job_tx: sender.clone(),
            config: Rc::new(RefCell::new(config)),
            storage: Rc::clone(&storage),
            recording_stop: Rc::new(RefCell::new(None)),
            update_state: Rc::new(RefCell::new(None)),
            upload_cancellation: Arc::new(UploadCancellation::default()),
            tray_available: Rc::new(Cell::new(false)),
            require_proxy: false,
        };

        wire_latest_upload_callback(&window, &sender, &resources);
        let fixture = Self {
            window,
            storage,
            activity,
            receiver,
            entry,
            directory,
        };

        fixture.restore_receipt();
        fixture
    }

    fn restore_receipt(&self) {
        let snapshot = ActivitySnapshot {
            entry_id: self.entry.id,
            delete_url: String::new(),
            detail: "Saved locally".to_owned(),
            history_saved: true,
            link: String::new(),
            local_path: self.entry.local_path.clone(),
            status: "Saved locally".to_owned(),
        };

        apply_activity(&self.window, &snapshot);
        *self.activity.borrow_mut() = Some(snapshot);
    }

    fn polling_state(&self) -> (PollingState, Sender<JobResult>) {
        let (sender, receiver) = mpsc::channel();

        (
            PollingState {
                activity: Rc::clone(&self.activity),
                preview: RefCell::new(preview::Previewer::new()),
                hotkeys: Rc::new(RefCell::new(None)),
                job_tx: mpsc::channel().0,
                result_rx: receiver,
                recording_stop: Rc::new(RefCell::new(None)),
                storage: Rc::clone(&self.storage),
                tray: Rc::new(RefCell::new(None)),
                ui: Rc::new(RefCell::new(None)),
                update_rx: mpsc::channel().1,
                update_state: Rc::new(RefCell::new(None)),
                upload_progress: Arc::new(UploadProgress::new()),
                upload_cancellation: Arc::new(UploadCancellation::default()),
                quit_after_job: Rc::new(Cell::new(false)),
                instance_show_rx: mpsc::channel().1,
            },
            sender,
        )
    }

    fn restore_unsaved_upload(&self) {
        let mut activity = self.activity.borrow_mut();
        let snapshot = activity.as_mut().unwrap();

        snapshot.history_saved = false;
        snapshot.link = "https://public.example/previous".to_owned();
        snapshot.delete_url = "https://delete.example/previous-private-capability".to_owned();
        snapshot.status = "Uploaded".to_owned();
        snapshot.detail = "History entry could not be saved".to_owned();
        apply_activity(&self.window, snapshot);
        self.window.set_artifact_preview_state(2);
    }
}

#[test]
fn latest_upload_reuses_history_and_honors_busy_draft_onboarding_and_invalid_settings() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;

    for gate in ["busy", "draft", "onboarding"] {
        window.set_busy(gate == "busy");
        window.set_settings_dirty(gate == "draft");
        window.set_show_onboarding(gate == "onboarding");
        window.invoke_upload_last();

        assert!(fixture.receiver.try_recv().is_err(), "{gate}");
        assert_eq!(window.get_last_local_path(), fixture.entry.local_path);
    }

    window.set_busy(false);
    window.set_settings_dirty(false);
    window.set_show_onboarding(false);
    window.set_uploader_kind(UploaderKind::Custom.index());
    window.set_uploader_url("".into());
    window.invoke_upload_last();

    assert!(fixture.receiver.try_recv().is_err());
    assert!(window.get_artifact_action_error());
    assert_eq!(window.get_last_local_path(), fixture.entry.local_path);
    assert_eq!(window.get_artifact_status_text(), "Saved locally");
    window.set_uploader_kind(UploaderKind::Uguu.index());
    window.invoke_upload_last();
    let job = fixture.receiver.try_recv().unwrap();
    let JobSource::History(entry) = job.source else {
        panic!("latest saved artifact should update its existing history entry");
    };

    assert_eq!(entry, fixture.entry);
    assert!(window.get_busy());
    assert!(window.get_artifact_action_detail().is_empty());
    assert_eq!(fixture.storage.history_count().unwrap(), 1);
}

#[test]
fn latest_upload_rejects_changed_history_and_missing_files_without_losing_receipt() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;

    fixture
        .storage
        .clear_history_local_path(fixture.entry.id)
        .unwrap();
    window.invoke_upload_last();
    assert!(fixture.receiver.try_recv().is_err());
    assert!(
        window
            .get_artifact_action_detail()
            .contains("History entry changed")
    );
    fixture.storage.remove_history(fixture.entry.id).unwrap();
    std::fs::remove_file(&fixture.entry.local_path).unwrap();
    window.invoke_upload_last();
    assert!(fixture.receiver.try_recv().is_err());
    assert!(
        window
            .get_artifact_action_detail()
            .contains("no longer exists")
    );
    std::fs::create_dir(&fixture.entry.local_path).unwrap();
    window.invoke_upload_last();
    assert!(fixture.receiver.try_recv().is_err());
    assert_eq!(window.get_last_local_path(), fixture.entry.local_path);
    std::fs::remove_dir(&fixture.entry.local_path).unwrap();
    std::fs::write(&fixture.entry.local_path, [1, 2, 3]).unwrap();
    window.set_last_link("https://public.example/already-uploaded".into());
    window.invoke_upload_last();
    assert!(fixture.receiver.try_recv().is_err());
    assert!(
        window
            .get_artifact_action_detail()
            .contains("already has a public link")
    );
}

#[test]
fn unpersisted_artifact_keeps_its_original_file_and_history_after_upload_cancellation() {
    let fixture = ArtifactFixture::new();

    fixture.storage.remove_history(fixture.entry.id).unwrap();
    fixture.window.invoke_upload_last();
    let mut job = fixture.receiver.try_recv().unwrap();

    assert!(
        matches!(&job.source, JobSource::ExistingLocal(entry) if entry.local_path == fixture.entry.local_path)
    );
    job.upload = Some(UploadOptions {
        uploader: UploadTarget {
            kind: UploaderKind::Custom,
            custom_url: "http://127.0.0.1:1/upload".to_owned(),
            ..UploadTarget::default()
        },
        lifetime_seconds: 0,
        require_tor: false,
        copy_link: false,
    });
    job.remove_exif = false;
    job.cancellation.cancel();
    let result = process_job_with_history(
        job,
        &Arc::new(UploadProgress::new()),
        JobCallbacks {
            persist_history: |entry: &mut HistoryEntry, original: Option<&HistoryEntry>| {
                assert!(original.is_none());
                entry.id = fixture.storage.insert_history(entry).unwrap();
                None
            },
            capture_complete: || {},
        },
    )
    .unwrap();

    assert_eq!(result.entry.local_path, fixture.entry.local_path);
    assert_eq!(
        result.local_copy.as_deref(),
        Some(std::path::Path::new(&fixture.entry.local_path))
    );
    assert_eq!(
        result.upload_failure.unwrap().stage,
        FailureStage::UploadCancelled
    );
    assert_eq!(fixture.storage.history_count().unwrap(), 1);
    assert_eq!(std::fs::read(&fixture.entry.local_path).unwrap(), [1, 2, 3]);
    assert!(fixture.directory.path().join("capture.png").is_file());
}

#[test]
fn artifact_feedback_and_preview_release_preserve_the_capture_outcome() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;

    window.set_status_text("Upload failed".into());
    window.set_status_detail("Network unavailable; saved locally".into());
    run_last_local_action(window, |_| Ok("Local file copied to clipboard"));
    assert_eq!(
        window.get_artifact_action_detail(),
        "Local file copied to clipboard"
    );
    assert!(!window.get_artifact_action_error());
    run_last_local_action(window, |_| anyhow::bail!("test local action failure"));
    assert!(window.get_artifact_action_error());
    assert_eq!(window.get_status_text(), "Upload failed");
    assert_eq!(
        window.get_status_detail(),
        "Network unavailable; saved locally"
    );
    let image = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(2, 2);

    window.set_artifact_preview(slint::Image::from_rgba8(image));
    window.set_artifact_preview_state(2);
    window.set_artifact_name("capture.png".into());
    invalidate_latest_local_path(
        window,
        &fixture.activity,
        fixture.entry.id,
        std::path::Path::new(&fixture.entry.local_path),
    );
    assert_eq!(window.get_artifact_preview().size().width, 0);
    assert_eq!(window.get_artifact_preview().size().height, 0);
    assert_eq!(window.get_artifact_preview_state(), 0);
    assert!(window.get_artifact_name().is_empty());
    assert!(window.get_artifact_action_detail().is_empty());
    assert!(window.get_last_local_path().is_empty());
    set_artifact_action_status(window, "old action feedback", true);
    clear_last_receipt(window);
    assert!(window.get_artifact_action_detail().is_empty());
    assert!(!window.get_artifact_action_error());
}

#[test]
fn malformed_existing_artifact_preserves_local_receipt_when_metadata_cleanup_fails() {
    let fixture = ArtifactFixture::new();

    for source in [
        JobSource::History(fixture.entry.clone()),
        JobSource::ExistingLocal(fixture.entry.clone()),
    ] {
        let persisted = matches!(source, JobSource::History(_));
        let callbacks_run = Cell::new(0);
        let result = process_job_with_history(
            Job {
                source,
                upload: None,
                remove_exif: true,
                cancellation: Arc::new(UploadCancellation::default()),
                restore_window_after_capture: false,
            },
            &Arc::new(UploadProgress::new()),
            JobCallbacks {
                persist_history: |entry: &mut HistoryEntry, original: Option<&HistoryEntry>| {
                    assert!(!persisted);
                    assert!(original.is_none());
                    callbacks_run.set(callbacks_run.get() + 1);
                    entry.id = 42;
                    None
                },
                capture_complete: || {},
            },
        )
        .unwrap();

        assert_eq!(result.entry.local_path, fixture.entry.local_path);
        assert_eq!(
            result.entry.id,
            if persisted { fixture.entry.id } else { 42 }
        );
        assert_eq!(callbacks_run.get(), i32::from(!persisted));
        assert_eq!(result.upload_failure.unwrap().stage, FailureStage::Upload);
        assert_eq!(std::fs::read(&fixture.entry.local_path).unwrap(), [1, 2, 3]);
    }
}

#[test]
fn disconnected_upload_worker_preserves_latest_receipt_and_preview() {
    let fixture = ArtifactFixture::new();
    let (sender, receiver) = mpsc::channel();

    drop(receiver);
    fixture.window.set_artifact_preview_state(2);
    fixture.window.set_artifact_name("capture.png".into());
    queue_job(
        &fixture.window,
        &sender,
        JobSource::History(fixture.entry.clone()),
        Arc::new(UploadCancellation::default()),
    );

    assert!(!fixture.window.get_busy());
    assert!(!fixture.window.get_uploading());
    assert_eq!(
        fixture.window.get_last_local_path(),
        fixture.entry.local_path
    );
    assert_eq!(fixture.window.get_artifact_preview_state(), 2);
    assert_eq!(fixture.window.get_artifact_name(), "capture.png");
    assert!(fixture.window.get_artifact_action_error());
    assert!(fixture.activity.borrow().is_some());
    assert_eq!(fixture.storage.history_count().unwrap(), 1);
}

#[test]
fn saving_and_discarding_settings_preserve_failed_artifact_outcome_and_retry_source() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;
    let config = Rc::new(RefCell::new(
        views::settings::config_from_window(window).unwrap(),
    ));
    let hotkeys = Rc::new(RefCell::new(None));
    let hotkey_status = Rc::new(RefCell::new(String::new()));

    views::settings::wire_dirty_callback(window, &config);
    preferences::wire_settings_callbacks(
        window,
        &hotkeys,
        &hotkey_status,
        &config,
        &fixture.storage,
    );
    {
        let mut activity = fixture.activity.borrow_mut();
        let snapshot = activity.as_mut().unwrap();

        snapshot.status = "Saved locally, upload failed".to_owned();
        snapshot.detail = "Network unavailable; local file retained".to_owned();
        apply_activity(window, snapshot);
    }

    window.set_force_require_proxy(true);
    window.set_completion_sound(true);
    window.invoke_settings_edited();
    window.invoke_save_settings();
    assert_eq!(window.get_status_text(), "Ready");
    assert_eq!(window.get_status_detail(), "Settings saved");
    assert_eq!(
        window.get_artifact_status_text(),
        "Saved locally, upload failed"
    );
    assert_eq!(
        window.get_artifact_status_detail(),
        "Network unavailable; local file retained"
    );
    assert_eq!(window.get_last_local_path(), fixture.entry.local_path);
    window.set_completion_sound(false);
    window.invoke_settings_edited();
    window.invoke_discard_settings();
    assert_eq!(window.get_status_text(), "Changes discarded");
    assert_eq!(
        window.get_artifact_status_text(),
        "Saved locally, upload failed"
    );
    assert_eq!(
        window.get_artifact_status_detail(),
        "Network unavailable; local file retained"
    );
    assert!(window.get_force_require_proxy());
    assert!(
        matches!(latest_upload_source(window, &fixture.storage, &fixture.activity).unwrap(), JobSource::History(entry) if entry.id == fixture.entry.id)
    );
    clear_last_receipt(window);
    assert!(window.get_artifact_status_text().is_empty());
    assert!(window.get_artifact_status_detail().is_empty());
}

#[test]
fn failed_new_attempt_preserves_unsaved_previous_receipt() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;
    let (state, result_sender) = fixture.polling_state();
    let (job_sender, job_receiver) = mpsc::channel();
    let mut detail = String::new();

    fixture.restore_unsaved_upload();
    queue_job(
        window,
        &job_sender,
        JobSource::File(fixture.directory.path().join("missing-new-file.png")),
        Arc::new(UploadCancellation::default()),
    );
    assert!(window.get_busy());
    assert!(window.get_artifact_is_previous());
    assert_eq!(
        window.get_last_delete_url(),
        "https://delete.example/previous-private-capability"
    );
    let failure = process_job_with_history(
        job_receiver.try_recv().unwrap(),
        &state.upload_progress,
        JobCallbacks {
            persist_history: |_: &mut HistoryEntry, _: Option<&HistoryEntry>| {
                panic!("a failed preparation cannot replace existing history");
            },
            capture_complete: || {},
        },
    )
    .unwrap_err();

    result_sender
        .send(JobResult::Error {
            message: failure.error.to_string(),
            stage: failure.stage,
        })
        .unwrap();
    receive_job_result(window, &state.result_rx, &state, &mut detail);
    assert_eq!(window.get_status_text(), "Upload failed");
    assert_eq!(window.get_artifact_status_text(), "Uploaded");
    assert_eq!(
        window.get_artifact_status_detail(),
        "History entry could not be saved"
    );
    assert_eq!(window.get_last_link(), "https://public.example/previous");
    assert_eq!(
        window.get_last_delete_url(),
        "https://delete.example/previous-private-capability"
    );
    assert!(!window.get_last_delete_saved());
    assert_eq!(window.get_artifact_preview_state(), 2);
    assert!(!window.get_busy());
    assert!(!window.get_uploading());
    assert!(!window.get_recording());
    assert_eq!(
        fixture.activity.borrow().as_ref().unwrap().delete_url,
        "https://delete.example/previous-private-capability"
    );

    present_error(
        window,
        FailureStage::Settings,
        "Selected uploader is not configured",
    );
    assert_eq!(window.get_status_text(), "Settings invalid");
    assert_eq!(window.get_artifact_status_text(), "Uploaded");
    assert_eq!(
        window.get_last_delete_url(),
        "https://delete.example/previous-private-capability"
    );
    result_sender
        .send(JobResult::Error {
            message: "New upload cancelled".to_owned(),
            stage: FailureStage::UploadCancelled,
        })
        .unwrap();
    receive_job_result(window, &state.result_rx, &state, &mut detail);
    assert_eq!(window.get_status_text(), "Upload cancelled");
    assert_eq!(window.get_last_link(), "https://public.example/previous");
}

#[test]
fn actual_new_artifact_replaces_previous_unsaved_receipt_and_invalidates_its_preview() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;
    let (state, result_sender) = fixture.polling_state();
    let mut detail = String::new();

    fixture.restore_unsaved_upload();
    begin_artifact_attempt(window);
    result_sender
        .send(JobResult::Success(Box::new(ProcessedUpload {
            entry: fixture.entry.clone(),
            history_warning: None,
            clipboard_warning: None,
            clipboard_copied: false,
            local_copy: Some(PathBuf::from(&fixture.entry.local_path)),
            capture_warning: None,
            upload_failure: Some(JobFailure::new(
                anyhow::anyhow!("new upload failed"),
                FailureStage::Upload,
            )),
        })))
        .unwrap();
    receive_job_result(window, &state.result_rx, &state, &mut detail);
    assert_eq!(
        window.get_artifact_status_text(),
        "Saved locally, upload failed"
    );
    assert!(!window.get_artifact_is_previous());
    assert!(window.get_last_link().is_empty());
    assert!(window.get_last_delete_url().is_empty());
    assert_eq!(window.get_last_local_path(), fixture.entry.local_path);
    assert_eq!(window.get_artifact_preview_state(), 0);
    assert!(
        fixture
            .activity
            .borrow()
            .as_ref()
            .unwrap()
            .delete_url
            .is_empty()
    );
}

#[test]
fn failure_without_a_previous_artifact_remains_a_visible_terminal_error() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;
    let (state, sender) = fixture.polling_state();
    let mut detail = String::new();

    clear_last_receipt(window);
    window.set_busy(true);
    window.set_uploading(true);
    window.set_recording(true);
    sender
        .send(JobResult::Error {
            message: "No clipboard image or file".to_owned(),
            stage: FailureStage::Upload,
        })
        .unwrap();
    receive_job_result(window, &state.result_rx, &state, &mut detail);
    assert_eq!(window.get_status_text(), "Upload failed");
    assert_eq!(window.get_status_detail(), "No clipboard image or file");
    assert!(!window.get_busy());
    assert!(!window.get_uploading());
    assert!(!window.get_recording());
    assert!(!window.get_artifact_is_previous());
    assert!(!has_artifact(window));
    assert_eq!(
        state.activity.borrow().as_ref().unwrap().status,
        "Upload failed"
    );
}

#[test]
fn delayed_history_deletion_never_clears_a_new_artifact_with_a_reused_row_id() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;

    window.set_artifact_preview_state(2);
    let previous_path = fixture.directory.path().join("previous-capture.png");

    assert!(!invalidate_latest_local_path(
        window,
        &fixture.activity,
        fixture.entry.id,
        &previous_path
    ));
    assert_eq!(window.get_last_local_path(), fixture.entry.local_path);
    assert_eq!(window.get_artifact_preview_state(), 2);
    assert_eq!(
        fixture.activity.borrow().as_ref().unwrap().local_path,
        fixture.entry.local_path
    );
    assert!(invalidate_latest_local_path(
        window,
        &fixture.activity,
        fixture.entry.id + 1,
        std::path::Path::new(&fixture.entry.local_path)
    ));
    assert!(window.get_last_local_path().is_empty());
    assert!(
        fixture
            .activity
            .borrow()
            .as_ref()
            .unwrap()
            .local_path
            .is_empty()
    );
}

#[test]
fn deletion_page_confirmation_rejects_a_replaced_capture_receipt() {
    let fixture = ArtifactFixture::new();
    let window = &fixture.window;

    wire_last_result_callbacks(window);
    window.set_last_link("https://public.example/first.png".into());
    window.set_last_delete_url("https://delete.example/private-token".into());
    window.set_artifact_name("first.png".into());
    window.invoke_open_last_delete();
    assert!(window.get_confirmation_visible());
    assert_eq!(window.get_confirmation_target(), "first.png");
    assert!(
        !window
            .get_confirmation_description()
            .contains("private-token")
    );
    window.set_last_link("https://public.example/second.png".into());
    window.invoke_confirmation_response(true);
    assert!(!window.get_confirmation_visible());
    assert!(window.get_artifact_action_error());
    assert_eq!(
        window.get_artifact_action_detail(),
        "The capture changed; choose its deletion action again"
    );
}
