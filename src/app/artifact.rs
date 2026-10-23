//! Latest capture receipt, local actions, and explicit upload retries.

use super::*;

pub(super) fn wire_last_result_callbacks(window: &AppWindow) {
    let window_weak = window.as_weak();

    window.on_open_link(move || {
        if let Some(window) = window_weak.upgrade() {
            let link = window.get_last_link();

            if !link.is_empty() {
                match open::that(link.as_str()) {
                    Ok(()) => set_artifact_action_status(
                        &window,
                        "Opened public link in the browser",
                        false,
                    ),

                    Err(_) => {
                        set_artifact_action_status(
                            &window,
                            "Could not open public link in the browser",
                            true,
                        );
                    }
                }
            }
        }
    });

    let window_weak = window.as_weak();

    window.on_copy_last_link(move || {
        if let Some(window) = window_weak.upgrade() {
            let link = window.get_last_link();

            if !link.is_empty() {
                match clipboard::copy_link(link.as_str()) {
                    Ok(()) => set_artifact_action_status(
                        &window,
                        "Public link copied to clipboard",
                        false,
                    ),

                    Err(error) => {
                        set_artifact_action_status(
                            &window,
                            &format!("Could not copy link: {error:#}"),
                            true,
                        );
                    }
                }
            }
        }
    });

    wire_copy_deletion_callback(window);

    let window_weak = window.as_weak();

    window.on_open_last_delete(move || {
        if let Some(window) = window_weak.upgrade() {
            let link = window.get_last_delete_url();

            if link.is_empty() {
                return;
            }

            let public_link = window.get_last_link();
            let local_path = window.get_last_local_path();
            let target = window.get_artifact_name();
            let name = target.clone();

            views::confirmation::deletion_page(&window, target.as_str(), move |window| {
                if window.get_last_delete_url() != link
                    || window.get_last_link() != public_link
                    || window.get_last_local_path() != local_path
                    || window.get_artifact_name() != name
                {
                    set_artifact_action_status(
                        window,
                        "The capture changed; choose its deletion action again",
                        true,
                    );

                    return;
                }

                let result = open::that(link.as_str());

                set_artifact_action_status(
                    window,
                    views::history::deletion_page_status(&result),
                    result.is_err(),
                );
            });
        }
    });

    wire_last_local_callbacks(window);
}

fn wire_copy_deletion_callback(window: &AppWindow) {
    let window_weak = window.as_weak();

    window.on_copy_last_delete(move || {
        if let Some(window) = window_weak.upgrade() {
            let link = window.get_last_delete_url();

            if !link.is_empty() {
                match clipboard::copy_link(link.as_str()) {
                    Ok(()) => {
                        set_artifact_action_status(
                            &window,
                            "Deletion URL copied to clipboard",
                            false,
                        );
                    }

                    Err(error) => {
                        set_artifact_action_status(
                            &window,
                            &format!("Could not copy deletion URL: {error:#}"),
                            true,
                        );
                    }
                }
            }
        }
    });
}

fn wire_last_local_callbacks(window: &AppWindow) {
    let window_weak = window.as_weak();

    window.on_open_last_local(move || {
        if let Some(window) = window_weak.upgrade() {
            run_last_local_action(&window, |path| {
                anyhow::ensure!(path.is_file(), "local capture no longer exists");
                open::that(path).context("failed to open local capture")?;
                Ok("Opened local capture")
            });
        }
    });

    let window_weak = window.as_weak();

    window.on_reveal_last_local(move || {
        if let Some(window) = window_weak.upgrade() {
            run_last_local_action(&window, |path| {
                views::history::reveal_file(path)?;
                Ok("Opened capture folder")
            });
        }
    });

    let window_weak = window.as_weak();

    window.on_copy_last_file(move || {
        if let Some(window) = window_weak.upgrade() {
            run_last_local_action(&window, |path| {
                clipboard::copy_file(path)?;
                Ok("Local file copied to clipboard")
            });
        }
    });
}

pub(super) fn wire_latest_upload_callback(
    window: &AppWindow,
    sender: &Sender<Job>,
    resources: &CallbackResources,
) {
    let window_weak = window.as_weak();
    let sender = sender.clone();
    let storage = Rc::clone(&resources.storage);
    let activity = Rc::clone(&resources.activity);
    let cancellation = Arc::clone(&resources.upload_cancellation);

    window.on_upload_last(move || {
        let Some(window) = window_weak.upgrade() else {
            return;
        };

        if !can_start_job(&window) {
            return;
        }

        let request = latest_upload_source(&window, &storage, &activity).and_then(|source| {
            upload_options_from_window(&window).map(|options| (source, options))
        });

        match request {
            Ok((source, options)) => {
                queue_upload_job(&window, &sender, source, options, Arc::clone(&cancellation));
            }

            Err(error) => set_artifact_action_status(&window, &format!("{error:#}"), true),
        }
    });
}

pub(super) fn latest_upload_source(
    window: &AppWindow,
    storage: &PlatformStorage,
    activity: &RefCell<Option<ActivitySnapshot>>,
) -> Result<JobSource> {
    anyhow::ensure!(
        window.get_last_link().is_empty(),
        "This capture already has a public link"
    );
    let path = PathBuf::from(window.get_last_local_path().as_str());

    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "This capture has no local file"
    );
    let metadata = path
        .metadata()
        .context("Local capture no longer exists or cannot be read")?;

    anyhow::ensure!(
        metadata.is_file(),
        "Local capture no longer exists or is not a file"
    );
    let activity = activity.borrow();

    if let Some(snapshot) = activity
        .as_ref()
        .filter(|snapshot| snapshot.history_saved && snapshot.entry_id != 0)
    {
        anyhow::ensure!(
            snapshot.link.is_empty() && std::path::Path::new(&snapshot.local_path) == path,
            "Latest capture changed; select the file again"
        );

        if let Some(entry) = storage.history_entry(snapshot.entry_id)? {
            anyhow::ensure!(
                entry.link.is_empty() && std::path::Path::new(&entry.local_path) == path,
                "History entry changed; select the file again"
            );

            return Ok(JobSource::History(entry));
        }
    }

    Ok(JobSource::ExistingLocal(HistoryEntry::local(
        path.file_name()
            .context("Local capture has no filename")?
            .to_string_lossy()
            .into_owned(),
        metadata.len(),
        &path,
    )))
}

pub(super) fn clear_last_receipt(window: &AppWindow) {
    preview::clear(window);
    set_artifact_result_status(window, "", "");
    set_artifact_action_status(window, "", false);
    window.set_history_action_detail("".into());
    window.set_history_action_failed(false);
    window.set_last_link("".into());
    window.set_last_local_path("".into());
    window.set_last_delete_url("".into());
    window.set_last_delete_saved(false);
    window.set_artifact_is_previous(false);
    window.invoke_receipt_cleared();
}

pub(super) fn has_artifact(window: &AppWindow) -> bool {
    !window.get_last_local_path().is_empty()
        || !window.get_last_link().is_empty()
        || !window.get_last_delete_url().is_empty()
}

pub(super) fn begin_artifact_attempt(window: &AppWindow) {
    window.set_history_action_detail("".into());
    window.set_history_action_failed(false);
    set_artifact_action_status(window, "", false);
    window.set_artifact_is_previous(has_artifact(window));
}

pub(super) fn run_last_local_action(
    window: &AppWindow,
    action: impl FnOnce(&std::path::Path) -> Result<&'static str>,
) {
    let path = PathBuf::from(window.get_last_local_path().as_str());

    if path.as_os_str().is_empty() {
        return;
    }

    match action(&path) {
        Ok(detail) => set_artifact_action_status(window, detail, false),

        Err(error) => {
            set_artifact_action_status(
                window,
                &format!("Local file action failed: {error:#}"),
                true,
            );
        }
    }
}

pub(super) fn set_artifact_action_status(window: &AppWindow, detail: &str, failed: bool) {
    window.set_artifact_action_detail(detail.into());
    window.set_artifact_action_error(failed);
}

pub(super) fn set_artifact_result_status(window: &AppWindow, status: &str, detail: &str) {
    window.set_artifact_status_text(status.into());
    window.set_artifact_status_detail(detail.into());
}
