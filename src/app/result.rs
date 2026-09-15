use super::*;

pub(super) fn current_window(ui: &UiSlot) -> Option<AppWindow> {
    ui.borrow()
        .as_ref()
        .map(|session| session.window.clone_strong())
}

pub(super) fn release_requested_ui(ui: &UiSlot) {
    let requested = ui
        .borrow()
        .as_ref()
        .is_some_and(|session| session.close_requested.get());

    if requested {
        drop(ui.borrow_mut().take());
    }
}

pub(super) fn promote_visible_transient_ui(ui: &UiSlot, storage: &PlatformStorage) {
    let should_promote = ui
        .borrow()
        .as_ref()
        .is_some_and(|session| session.transient && session.window.window().is_visible());

    if should_promote {
        let _ = promote_ui(ui, storage);
    }
}

pub(super) fn release_idle_transient_ui(ui: &UiSlot, storage: &PlatformStorage) {
    let should_release = ui.borrow().as_ref().is_some_and(|session| {
        session.transient && !session.window.window().is_visible() && !session.window.get_busy()
    });

    if !should_release {
        return;
    }

    let reveal_error = ui
        .borrow()
        .as_ref()
        .is_some_and(|session| session.window.get_status_text().ends_with("failed"));

    if reveal_error {
        let shown = promote_ui(ui, storage)
            .and_then(|window| show_ui(ui, &window))
            .is_ok();

        if !shown {
            drop(ui.borrow_mut().take());
        }
    } else {
        drop(ui.borrow_mut().take());
    }
}

pub(super) fn append_status_detail(window: &AppWindow, message: &str) {
    let detail = window.get_status_detail();

    if detail.is_empty() {
        window.set_status_detail(message.into());
    } else {
        window.set_status_detail(format!("{detail}  -  {message}").into());
    }
}

pub(super) fn update_upload_progress(window: &AppWindow, upload_progress: &UploadProgress) {
    if upload_progress.is_active() && !window.get_uploading() {
        window.set_uploading(true);
        window.set_status_text("Uploading...".into());
        window.set_status_detail("Sending the capture to the configured uploader".into());
    }

    if !window.get_uploading() {
        return;
    }

    let Some((transferred, total)) = upload_progress.snapshot() else {
        return;
    };
    let progress = if total == 0 {
        1.0
    } else {
        transferred.to_f32().unwrap_or(f32::MAX) / total.to_f32().unwrap_or(f32::MAX)
    };

    window.set_upload_progress(progress.clamp(0.0, 1.0));

    if transferred >= total {
        window.set_status_detail("Upload transferred  -  waiting for the server response".into());
    }
}

pub(super) fn receive_job_result(
    window: &AppWindow,
    result_rx: &Receiver<JobResult>,
    state: &PollingState,
    status: &mut String,
) {
    let Ok(result) = result_rx.try_recv() else {
        return;
    };

    if matches!(result, JobResult::CaptureComplete) {
        let _ = window.show();

        foreground_ui(window);

        return;
    }

    let JobResult::Success(upload) = result else {
        let JobResult::Error { message, stage } = result else {
            return;
        };

        window.set_recording(false);
        *state.recording_stop.borrow_mut() = None;
        window.set_busy(false);
        window.set_uploading(false);
        let failure_status = stage.status();

        window.set_status_text(failure_status.into());
        window.set_status_detail(message.as_str().into());
        *state.activity.borrow_mut() = Some(ActivitySnapshot {
            entry_id: 0,
            delete_url: String::new(),
            detail: message,
            history_saved: false,
            link: String::new(),
            local_path: String::new(),
            status: failure_status.to_owned(),
        });
        finish_pending_quit(state);

        return;
    };

    let upload = *upload;

    window.set_busy(false);
    window.set_recording(false);
    window.set_uploading(false);
    window.set_upload_progress(1.0);
    *state.recording_stop.borrow_mut() = None;
    let uploaded = !upload.entry.link.is_empty();
    let partial_upload_failure = upload.upload_failure.as_ref();
    let play_sound =
        uploaded && window.get_completion_sound() && upload.clipboard_warning.is_none();
    let history_saved = upload.history_warning.is_none();
    let entry = upload.entry;

    window.set_last_link(entry.link.as_str().into());
    window.set_last_local_path(entry.local_path.as_str().into());
    window.set_last_delete_url(entry.delete_url.as_str().into());
    window.set_last_delete_saved(history_saved);
    let status_text = result_status(
        uploaded,
        partial_upload_failure.map(|failure| failure.stage),
    );

    window.set_status_text(status_text.into());

    if play_sound {
        crate::sound::play_completion_async();
    }

    status.clear();
    status.push_str(&entry.filename);

    if let Some(path) = upload.local_copy {
        status.push_str("  -  saved to ");
        let _ = write!(status, "{}", path.display());
    }

    for warning in [upload.capture_warning, upload.history_warning]
        .into_iter()
        .flatten()
    {
        status.push_str("  -  ");
        status.push_str(&warning);
    }

    if uploaded {
        status.push_str("  -  ");
        status.push_str(
            upload
                .clipboard_warning
                .as_deref()
                .unwrap_or("link copied to clipboard"),
        );
    }

    if let Some(failure) = partial_upload_failure {
        status.push_str("  -  saved locally while ");
        status.push_str(match failure.stage {
            FailureStage::UploadCancelled => "upload was cancelled: ",
            FailureStage::UploadTimedOut => "upload timed out: ",
            _ => "upload failed: ",
        });
        status.push_str(&failure.message);
    }

    refresh_history_after_insert(window, state, history_saved, status);

    window.set_status_detail(status.as_str().into());
    *state.activity.borrow_mut() = Some(ActivitySnapshot {
        entry_id: entry.id,
        delete_url: entry.delete_url,
        detail: status.clone(),
        history_saved,
        link: entry.link,
        local_path: entry.local_path,
        status: status_text.to_owned(),
    });
    finish_pending_quit(state);
}

pub(super) fn finish_pending_quit(state: &PollingState) {
    if state.quit_after_job.replace(false) {
        let _ = slint::quit_event_loop();
    }
}

pub(super) const fn result_status(
    uploaded: bool,
    upload_failure: Option<FailureStage>,
) -> &'static str {
    if uploaded {
        "Uploaded"
    } else {
        match upload_failure {
            Some(FailureStage::UploadCancelled) => "Saved locally, upload cancelled",
            Some(FailureStage::UploadTimedOut) => "Saved locally, upload timed out",
            Some(_) => "Saved locally, upload failed",
            None => "Saved locally",
        }
    }
}

pub(super) fn refresh_history_after_insert(
    window: &AppWindow,
    state: &PollingState,
    history_saved: bool,
    status: &mut String,
) {
    if !history_saved {
        return;
    }

    let ui = state.ui.borrow();
    let Some(session) = ui.as_ref().filter(|session| !session.transient) else {
        return;
    };

    if let Err(error) =
        views::history::observe_inserted(window, state.storage.as_ref(), session.history.as_ref())
    {
        status.push_str("  -  history view refresh failed: ");
        let _ = write!(status, "{error:#}");
    }
}

pub(super) fn apply_activity(window: &AppWindow, activity: &ActivitySnapshot) {
    window.set_status_text(activity.status.as_str().into());
    window.set_status_detail(activity.detail.as_str().into());
    window.set_last_link(activity.link.as_str().into());
    window.set_last_local_path(activity.local_path.as_str().into());
    window.set_last_delete_url(activity.delete_url.as_str().into());
    window.set_last_delete_saved(activity.history_saved);
}

pub(super) fn invalidate_latest_local_path(
    window: &AppWindow,
    activity: &RefCell<Option<ActivitySnapshot>>,
    entry_id: i64,
    path: &std::path::Path,
) -> bool {
    let window_matches = std::path::Path::new(window.get_last_local_path().as_str()) == path;
    let activity_matches = invalidate_activity_local_path(activity, entry_id, path);

    if window_matches || activity_matches {
        window.set_last_local_path(String::new().into());
    }

    window_matches || activity_matches
}

pub(super) fn invalidate_activity_local_path(
    activity: &RefCell<Option<ActivitySnapshot>>,
    entry_id: i64,
    path: &std::path::Path,
) -> bool {
    let mut activity = activity.borrow_mut();
    let matches = activity.as_ref().is_some_and(|snapshot| {
        (entry_id != 0 && snapshot.entry_id == entry_id)
            || std::path::Path::new(&snapshot.local_path) == path
    });

    if matches && let Some(snapshot) = activity.as_mut() {
        snapshot.local_path.clear();
    }

    matches
}

pub(super) fn receive_update(
    window: &AppWindow,
    update_rx: &Receiver<UpdateCheck>,
    update_state: &RefCell<Option<UpdateCheck>>,
) {
    let Ok(update) = update_rx.try_recv() else {
        return;
    };

    apply_update(window, &update);
    *update_state.borrow_mut() = Some(update);
}

pub(super) fn apply_update(window: &AppWindow, update: &UpdateCheck) {
    match update {
        UpdateCheck::Available(info) => {
            window.set_update_available(true);
            window.set_update_version(info.version.as_str().into());
            window.set_update_url(info.url.as_str().into());
            window.set_update_status("A newer release is available".into());
        }

        UpdateCheck::Current => {
            window.set_update_available(false);
            window.set_update_status("You are running the latest release".into());
        }

        UpdateCheck::Failed(error) => {
            window.set_update_available(false);
            window.set_update_status(format!("Update check unavailable: {error}").into());
        }
    }
}

impl From<HotkeyAction> for TrayAction {
    fn from(action: HotkeyAction) -> Self {
        match action {
            HotkeyAction::Region => Self::Region,
            HotkeyAction::Recording => Self::Recording,
            HotkeyAction::Screenshot => Self::Screenshot,
            HotkeyAction::Clipboard => Self::Clipboard,
        }
    }
}
