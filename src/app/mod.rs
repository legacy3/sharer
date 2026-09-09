//! Desktop window wiring and background job coordination.

use std::{
    cell::{Cell, RefCell},
    fmt::Write as _,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use num_traits::ToPrimitive as _;
use slint::ComponentHandle as _;

use crate::{
    AppWindow,
    hotkeys::{HotkeyAction, Hotkeys},
    icon,
    tray::{Tray, TrayAction},
    update::{UpdateCheck, spawn_check},
    views,
};
use sharer::{
    capture, clipboard,
    config::{AppConfig, CaptureResolution, ShortcutConfig, display_lifetime},
    history::HistoryEntry,
    proxy::{TorProxy, preferred_upload_route},
    storage::PlatformStorage,
    upload::{UploadClient, UploadPayload, UploadTarget},
    validate_lifetime,
};

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(750);
const CAPTURE_HIDE_DELAY: Duration = Duration::from_millis(100);
type RecordingControl = Rc<RefCell<Option<Sender<()>>>>;
type UiSlot = Rc<RefCell<Option<UiSession>>>;

mod region;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloseAction {
    KeepShown,
    HideToTray,
    Quit,
}

#[derive(Clone, Copy, Debug)]
struct CloseState {
    busy: bool,
    minimize_to_tray: bool,
}

#[derive(Debug)]
enum JobSource {
    File(PathBuf),
    Clipboard,
    Prepared {
        payload: UploadPayload,
        save_directory: Option<PathBuf>,
    },
    Recording {
        region: capture::DesktopRegion,
        stop: Receiver<()>,
        filename_stem: String,
        frames_per_second: u32,
        max_seconds: u32,
        capture_resolution: CaptureResolution,
        save_directory: Option<PathBuf>,
    },
}

#[derive(Debug)]
struct Job {
    source: JobSource,
    uploader: UploadTarget,
    lifetime_seconds: u32,
    require_tor: bool,
    remove_exif: bool,
}

struct PollingState {
    activity: Rc<RefCell<Option<ActivitySnapshot>>>,
    hotkeys: Rc<RefCell<Option<Hotkeys>>>,
    job_tx: Sender<Job>,
    result_rx: Receiver<JobResult>,
    recording_stop: RecordingControl,
    tray: Rc<RefCell<Option<Tray>>>,
    ui: UiSlot,
    update_rx: Receiver<UpdateCheck>,
    update_state: Rc<RefCell<Option<UpdateCheck>>>,
    upload_progress: Arc<UploadProgress>,
}

#[derive(Clone)]
struct CallbackResources {
    activity: Rc<RefCell<Option<ActivitySnapshot>>>,
    hotkeys: Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: Rc<RefCell<String>>,
    job_tx: Sender<Job>,
    config: Rc<RefCell<AppConfig>>,
    storage: Rc<PlatformStorage>,
    recording_stop: RecordingControl,
    update_state: Rc<RefCell<Option<UpdateCheck>>>,
}

struct UiSession {
    close_requested: Rc<Cell<bool>>,
    history: Rc<RefCell<views::history::HistoryController>>,
    transient: bool,
    window: AppWindow,
}

#[derive(Debug)]
struct ActivitySnapshot {
    delete_url: String,
    detail: String,
    history_saved: bool,
    link: String,
    status: String,
}

#[derive(Debug)]
enum JobResult {
    Success(ProcessedUpload),
    Error(String),
}

#[derive(Debug)]
struct UploadProgress {
    transferred: AtomicU64,
    total: AtomicU64,
}

impl UploadProgress {
    const PENDING: u64 = u64::MAX;

    fn new() -> Self {
        Self {
            transferred: AtomicU64::new(0),
            total: AtomicU64::new(Self::PENDING),
        }
    }

    fn reset(&self) {
        self.transferred.store(0, Ordering::Relaxed);
        self.total.store(Self::PENDING, Ordering::Release);
    }

    fn update(&self, transferred: u64, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.transferred.store(transferred, Ordering::Release);
    }

    fn snapshot(&self) -> Option<(u64, u64)> {
        let transferred = self.transferred.load(Ordering::Acquire);
        let total = self.total.load(Ordering::Relaxed);

        (total != Self::PENDING).then_some((transferred, total))
    }
}

#[derive(Debug)]
struct ProcessedUpload {
    entry: HistoryEntry,
    history_warning: Option<String>,
    clipboard_warning: Option<String>,
    local_copy: Option<PathBuf>,
    capture_warning: Option<String>,
}

pub fn run(show_on_start: bool) -> Result<()> {
    let storage = Rc::new(PlatformStorage::open()?);
    let mut config = AppConfig::load(storage.as_ref())?;

    views::settings::ensure_default_capture_directory(&mut config)?;

    if let Ok(enabled) = crate::autostart::is_enabled() {
        config.behavior.start_at_login = enabled;
    }

    let tor_proxy = TorProxy::detect();
    let config = Rc::new(RefCell::new(config));
    let activity = Rc::new(RefCell::new(None));
    let hotkeys = Rc::new(RefCell::new(None));
    let hotkey_status = Rc::new(RefCell::new(String::new()));
    let recording_stop = Rc::new(RefCell::new(None));
    let update_state = Rc::new(RefCell::new(None));

    match Hotkeys::register(&config.borrow().shortcuts) {
        Ok(registered) => {
            *hotkeys.borrow_mut() = Some(registered);
            "Shortcuts active".clone_into(&mut hotkey_status.borrow_mut());
        }

        Err(error) => {
            *hotkey_status.borrow_mut() = format!("Shortcuts unavailable: {error}");
        }
    }

    let (job_tx, job_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let update_rx = spawn_check(
        tor_proxy.as_ref().map(|proxy| proxy.url().to_owned()),
        config.borrow().privacy.require_tor,
    )?;
    let upload_progress = Arc::new(UploadProgress::new());

    spawn_worker(job_rx, result_tx, Arc::clone(&upload_progress))?;
    let callback_resources = CallbackResources {
        activity: Rc::clone(&activity),
        hotkeys: Rc::clone(&hotkeys),
        hotkey_status: Rc::clone(&hotkey_status),
        job_tx: job_tx.clone(),
        config: Rc::clone(&config),
        storage: Rc::clone(&storage),
        recording_stop: Rc::clone(&recording_stop),
        update_state: Rc::clone(&update_state),
    };
    let ui = Rc::new(RefCell::new(None));
    let tray = Rc::new(RefCell::new(None));
    let polling_state = PollingState {
        activity,
        hotkeys,
        job_tx,
        result_rx,
        recording_stop,
        tray,
        ui,
        update_rx,
        update_state,
        upload_progress,
    };

    if show_on_start {
        open_ui(&polling_state, &callback_resources, false)?;
    }

    let tray_for_startup = Rc::clone(&polling_state.tray);
    let ui_for_startup = Rc::clone(&polling_state.ui);
    let resources_for_startup = callback_resources.clone();
    let startup_error = Rc::new(RefCell::new(None));
    let startup_error_for_tray = Rc::clone(&startup_error);

    // TrayIcon requires the native event loop to be running before construction.
    slint::Timer::single_shot(Duration::ZERO, move || match create_tray() {
        Ok(created) => *tray_for_startup.borrow_mut() = Some(created),

        Err(error) => match ensure_ui(&ui_for_startup, &resources_for_startup, false) {
            Ok(window) => {
                window.set_minimize_to_tray(false);
                window.set_status_detail(format!("System tray unavailable: {error:#}").into());

                if !window.window().is_visible()
                    && let Err(show_error) = window.show()
                {
                    *startup_error_for_tray.borrow_mut() = Some(
                        anyhow::Error::from(show_error)
                            .context(format!("system tray unavailable: {error:#}")),
                    );
                    let _ = slint::quit_event_loop();
                }
            }

            Err(ui_error) => {
                *startup_error_for_tray.borrow_mut() =
                    Some(ui_error.context(format!("system tray unavailable: {error:#}")));
                let _ = slint::quit_event_loop();
            }
        },
    });
    let timer = start_polling(polling_state, callback_resources);

    slint::run_event_loop_until_quit().context("application event loop failed")?;
    drop(timer);

    if let Some(error) = startup_error.borrow_mut().take() {
        return Err(error);
    }

    Ok(())
}

fn create_tray() -> Result<Tray> {
    #[cfg(target_os = "linux")]
    {
        anyhow::ensure!(
            appindicator_available(),
            "AppIndicator runtime library is not installed"
        );
        gtk::init().context("GTK could not initialize the system tray")?;
    }

    icon::tray_icon().and_then(Tray::create)
}

fn open_ui(state: &PollingState, resources: &CallbackResources, transient: bool) -> Result<()> {
    let window = ensure_ui(&state.ui, resources, transient)?;

    show_ui(&state.ui, &window)
}

fn show_ui(ui: &UiSlot, window: &AppWindow) -> Result<()> {
    if let Err(error) = window.show() {
        drop(ui.borrow_mut().take());

        return Err(error).context("failed to show application window");
    }

    Ok(())
}

fn ensure_ui(ui: &UiSlot, resources: &CallbackResources, transient: bool) -> Result<AppWindow> {
    if ui.borrow().is_none() {
        let window = AppWindow::new().context("failed to create application window")?;

        views::settings::initialize(&window, &resources.config.borrow());
        window.set_hotkey_status(resources.hotkey_status.borrow().as_str().into());

        if !transient && let Some(activity) = resources.activity.borrow().as_ref() {
            apply_activity(&window, activity);
        }

        if let Some(update) = resources.update_state.borrow().as_ref() {
            apply_update(&window, update);
        }

        let history = views::history::initialize(&window, &resources.storage, transient)?;
        let close_requested = Rc::new(Cell::new(false));

        wire_callbacks(
            &window,
            &resources.job_tx,
            resources,
            &history,
            &close_requested,
        );
        *ui.borrow_mut() = Some(UiSession {
            close_requested,
            history,
            transient,
            window,
        });
    }

    if !transient {
        return promote_ui(ui, &resources.storage);
    }

    current_window(ui).context("application window was not retained")
}

fn promote_ui(ui: &UiSlot, storage: &PlatformStorage) -> Result<AppWindow> {
    let (window, history, transient) = {
        let ui = ui.borrow();
        let session = ui.as_ref().context("application window was not retained")?;

        (
            session.window.clone_strong(),
            Rc::clone(&session.history),
            session.transient,
        )
    };

    if transient {
        if let Err(error) = views::history::refresh_current(&window, storage, history.as_ref()) {
            views::history::resume_empty(&window, history.as_ref());
            append_status_detail(&window, &format!("History unavailable: {error:#}"));
        }

        if let Some(session) = ui.borrow_mut().as_mut() {
            session.transient = false;
        }
    }

    Ok(window)
}

#[cfg(target_os = "linux")]
#[expect(
    unsafe_code,
    reason = "probing a known shared library is the only way to avoid an upstream loader panic"
)]
fn appindicator_available() -> bool {
    ["libayatana-appindicator3.so.1", "libappindicator3.so.1"]
        .iter()
        .any(|name| {
            // SAFETY: Names are fixed and the library handle does not escape.
            unsafe { libloading::Library::new(name).is_ok() }
        })
}

fn wire_callbacks(
    window: &AppWindow,
    job_tx: &Sender<Job>,
    resources: &CallbackResources,
    history: &Rc<RefCell<views::history::HistoryController>>,
    close_requested: &Rc<Cell<bool>>,
) {
    views::settings::wire_folder_callbacks(window);

    let sender = job_tx.clone();
    let window_weak = window.as_weak();

    window.on_capture_region(move || {
        if let Some(window) = window_weak.upgrade() {
            region::start_screenshot(&window, sender.clone());
        }
    });

    let sender = job_tx.clone();
    let window_weak = window.as_weak();
    let recording_stop_for_callback = Rc::clone(&resources.recording_stop);

    window.on_record_region(move || {
        if let Some(window) = window_weak.upgrade() {
            region::toggle_recording(&window, &sender, &recording_stop_for_callback);
        }
    });

    let sender = job_tx.clone();
    let window_weak = window.as_weak();

    window.on_capture_screen(move || {
        if let Some(window) = window_weak.upgrade() {
            queue_screenshot(&window, &sender);
        }
    });

    let sender = job_tx.clone();
    let window_weak = window.as_weak();

    window.on_upload_clipboard(move || {
        if let Some(window) = window_weak.upgrade() {
            queue_job(&window, &sender, JobSource::Clipboard);
        }
    });

    let sender = job_tx.clone();
    let window_weak = window.as_weak();

    window.on_choose_file(move || {
        if let Some(path) = rfd::FileDialog::new().pick_file()
            && let Some(window) = window_weak.upgrade()
        {
            queue_job(&window, &sender, JobSource::File(path));
        }
    });

    let window_weak = window.as_weak();

    window.on_open_link(move || {
        if let Some(window) = window_weak.upgrade() {
            let link = window.get_last_link();

            if !link.is_empty() {
                let _ = open::that(link.as_str());
            }
        }
    });

    let window_weak = window.as_weak();

    window.on_copy_last_link(move || {
        if let Some(window) = window_weak.upgrade() {
            let link = window.get_last_link();

            if !link.is_empty() {
                match clipboard::copy_link(link.as_str()) {
                    Ok(()) => window.set_status_detail("Public link copied to clipboard".into()),

                    Err(error) => {
                        window.set_status_detail(format!("Could not copy link: {error:#}").into());
                    }
                }
            }
        }
    });

    let window_weak = window.as_weak();

    window.on_copy_last_delete(move || {
        if let Some(window) = window_weak.upgrade() {
            let link = window.get_last_delete_url();

            if !link.is_empty() {
                match clipboard::copy_link(link.as_str()) {
                    Ok(()) => {
                        window.set_status_detail("Deletion URL copied to clipboard".into());
                    }

                    Err(error) => {
                        window.set_status_detail(
                            format!("Could not copy deletion URL: {error:#}").into(),
                        );
                    }
                }
            }
        }
    });

    wire_management_callbacks(
        window,
        &resources.hotkeys,
        &resources.hotkey_status,
        &resources.config,
        &resources.storage,
    );
    wire_close_callback(window, history, close_requested);
}

fn wire_management_callbacks(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &Rc<RefCell<String>>,
    config: &Rc<RefCell<AppConfig>>,
    storage: &Rc<PlatformStorage>,
) {
    views::settings::wire_dirty_callback(window, config);

    let window_weak = window.as_weak();

    window.on_open_update(move || {
        if let Some(window) = window_weak.upgrade() {
            let url = window.get_update_url();

            if !url.is_empty() {
                let _ = open::that(url.as_str());
            }
        }
    });

    let window_weak = window.as_weak();
    let hotkeys = Rc::clone(hotkeys);
    let hotkey_status = Rc::clone(hotkey_status);
    let config = Rc::clone(config);
    let storage = Rc::clone(storage);

    window.on_save_settings(move || {
        if let Some(window) = window_weak.upgrade() {
            match views::settings::config_from_window(&window) {
                Ok(next) => save_settings(
                    &window,
                    &hotkeys,
                    &hotkey_status,
                    &config,
                    storage.as_ref(),
                    next,
                ),

                Err(error) => {
                    window.set_status_text("Settings not saved".into());
                    window.set_status_detail(error.to_string().into());
                }
            }
        }
    });
}

fn save_settings(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &RefCell<String>,
    config: &RefCell<AppConfig>,
    storage: &PlatformStorage,
    next: AppConfig,
) {
    let previous = config.borrow().clone();
    let startup_changed = next.behavior.start_at_login != previous.behavior.start_at_login;
    let shortcuts_changed = next.shortcuts != previous.shortcuts;

    if startup_changed
        && let Err(error) = crate::autostart::set_enabled(next.behavior.start_at_login)
    {
        show_settings_error(window, &error.to_string());

        return;
    }

    if shortcuts_changed
        && let Err(error) = replace_hotkeys(hotkeys, &next.shortcuts, &previous.shortcuts)
    {
        if startup_changed {
            let _ = crate::autostart::set_enabled(previous.behavior.start_at_login);
        }

        let status = if hotkeys.borrow().is_some() {
            format!("Previous shortcuts remain active: {error}")
        } else {
            format!("Shortcuts unavailable: {error}")
        };

        status.clone_into(&mut hotkey_status.borrow_mut());
        window.set_hotkey_status(status.into());
        show_settings_error(window, &error.to_string());

        return;
    }

    if let Err(error) = next.save(storage) {
        let rollback_error = shortcuts_changed
            .then(|| replace_hotkeys(hotkeys, &previous.shortcuts, &next.shortcuts).err())
            .flatten();

        if startup_changed {
            let _ = crate::autostart::set_enabled(previous.behavior.start_at_login);
        }

        let detail = rollback_error.as_ref().map_or_else(
            || format!("Could not save settings: {error}"),
            |rollback_error| {
                format!(
                    "Could not save settings: {error}; shortcut rollback failed: {rollback_error}"
                )
            },
        );

        if shortcuts_changed {
            let status = if let Some(rollback_error) = rollback_error {
                if hotkeys.borrow().is_some() {
                    format!("Unsaved shortcuts remain active: {rollback_error}")
                } else {
                    format!("Shortcuts unavailable after rollback: {rollback_error}")
                }
            } else {
                "Shortcuts active".to_owned()
            };

            status.clone_into(&mut hotkey_status.borrow_mut());
            window.set_hotkey_status(status.into());
        }

        show_settings_error(window, &detail);

        return;
    }

    *config.borrow_mut() = next;
    window.set_settings_dirty(false);

    if shortcuts_changed {
        "Shortcuts active".clone_into(&mut hotkey_status.borrow_mut());
        window.set_hotkey_status("Shortcuts active".into());
    }

    window.set_status_text("Ready".into());
    window.set_status_detail("Settings saved".into());
}

fn show_settings_error(window: &AppWindow, error: &str) {
    window.set_status_text("Settings not saved".into());
    window.set_status_detail(error.into());
}

fn wire_close_callback(
    window: &AppWindow,
    history: &Rc<RefCell<views::history::HistoryController>>,
    close_requested: &Rc<Cell<bool>>,
) {
    let window_weak = window.as_weak();
    let history = Rc::clone(history);
    let close_requested = Rc::clone(close_requested);

    window.window().on_close_requested(move || {
        let Some(window) = window_weak.upgrade() else {
            let _ = slint::quit_event_loop();

            return slint::CloseRequestResponse::HideWindow;
        };

        let state = CloseState {
            busy: window.get_busy(),
            minimize_to_tray: window.get_minimize_to_tray(),
        };

        match close_action(state) {
            CloseAction::KeepShown => {
                window.set_status_detail(
                    "Finish or stop the current capture/upload before quitting".into(),
                );

                slint::CloseRequestResponse::KeepWindowShown
            }

            CloseAction::HideToTray => {
                views::history::suspend(history.as_ref());
                let _ = window.hide();

                close_requested.set(true);

                slint::CloseRequestResponse::KeepWindowShown
            }

            CloseAction::Quit => {
                let _ = slint::quit_event_loop();

                slint::CloseRequestResponse::HideWindow
            }
        }
    });
}

const fn close_action(state: CloseState) -> CloseAction {
    if state.busy {
        CloseAction::KeepShown
    } else if state.minimize_to_tray {
        CloseAction::HideToTray
    } else {
        CloseAction::Quit
    }
}

fn queue_screenshot(window: &AppWindow, sender: &Sender<Job>) {
    if window.get_busy() {
        return;
    }

    let was_visible = window.window().is_visible();
    let window_weak = window.as_weak();
    let sender = sender.clone();

    clear_last_receipt(window);
    window.set_busy(true);
    window.set_status_text("Capturing screen...".into());
    window.set_status_detail("Waiting for ShareR to leave the captured frame".into());

    if was_visible {
        let _ = window.hide();
    }

    let delay = if was_visible {
        CAPTURE_HIDE_DELAY
    } else {
        Duration::ZERO
    };

    slint::Timer::single_shot(delay, move || {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        let result = (|| {
            let naming = views::settings::naming_from_window(&window)?;
            let filename_stem =
                crate::naming::screenshot_stem(&naming, window.get_private_capture_names())?;
            let mut payload = capture::primary_monitor(
                region::capture_color_mode(&window),
                region::capture_resolution(&window),
                region::resize_quality(&window),
            )?;

            payload.filename = capture::named_capture_filename(&filename_stem, &payload.filename);
            Ok::<_, anyhow::Error>(payload)
        })();

        if was_visible {
            let _ = window.show();
        }

        window.set_busy(false);

        match result {
            Ok(payload) => {
                let save_directory = views::settings::configured_capture_directory(&window);

                queue_job(
                    &window,
                    &sender,
                    JobSource::Prepared {
                        payload,
                        save_directory,
                    },
                );
            }

            Err(error) => show_error(&window, &format!("{error:#}")),
        }
    });
}

fn replace_hotkeys(
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    shortcuts: &ShortcutConfig,
    fallback: &ShortcutConfig,
) -> Result<()> {
    *hotkeys.borrow_mut() = None;

    match Hotkeys::register(shortcuts) {
        Ok(registered) => {
            *hotkeys.borrow_mut() = Some(registered);
            Ok(())
        }

        Err(error) => {
            match Hotkeys::register(fallback) {
                Ok(registered) => *hotkeys.borrow_mut() = Some(registered),

                Err(fallback_error) => {
                    return Err(error.context(format!(
                        "previous shortcuts could not be restored: {fallback_error}"
                    )));
                }
            }

            Err(error)
        }
    }
}

fn queue_job(window: &AppWindow, sender: &Sender<Job>, source: JobSource) {
    if window.get_busy() {
        return;
    }

    let Some(lifetime_seconds) = valid_window_lifetime(window) else {
        return;
    };
    let Some(uploader) = valid_window_upload_target(window) else {
        return;
    };

    clear_last_receipt(window);
    window.set_busy(true);
    window.set_uploading(true);
    window.set_upload_progress(0.0);
    window.set_status_text("Uploading...".into());
    window.set_status_detail(upload_status_detail(&uploader, lifetime_seconds).into());

    if sender
        .send(Job {
            source,
            uploader,
            lifetime_seconds,
            require_tor: window.get_require_tor(),
            remove_exif: window.get_remove_exif(),
        })
        .is_err()
    {
        show_error(window, "Upload worker stopped unexpectedly");
    }
}

fn clear_last_receipt(window: &AppWindow) {
    window.set_last_link("".into());
    window.set_last_delete_url("".into());
    window.set_last_delete_saved(false);
}

fn valid_window_lifetime(window: &AppWindow) -> Option<u32> {
    let Ok(lifetime_seconds) = u32::try_from(window.get_lifetime_seconds()) else {
        show_error(window, "Lifetime must be a positive number of seconds");

        return None;
    };

    if let Err(error) = validate_lifetime(lifetime_seconds) {
        show_error(window, &error.to_string());

        return None;
    }

    Some(lifetime_seconds)
}

fn valid_window_upload_target(window: &AppWindow) -> Option<UploadTarget> {
    match views::settings::upload_target_from_window(window) {
        Ok(target) => Some(target),

        Err(error) => {
            show_error(
                window,
                &format!("Configure the selected uploader in Settings: {error}"),
            );
            None
        }
    }
}

fn upload_status_detail(target: &UploadTarget, lifetime_seconds: u32) -> String {
    if target.kind.supports_lifetime() {
        format!("Expires after {}", display_lifetime(lifetime_seconds))
    } else {
        format!("Retention managed by {}", target.kind.name())
    }
}

fn spawn_worker(
    job_rx: Receiver<Job>,
    result_tx: Sender<JobResult>,
    upload_progress: Arc<UploadProgress>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("sharer-upload".to_owned())
        .spawn(move || {
            while let Ok(job) = job_rx.recv() {
                upload_progress.reset();
                let result = process_job(job, &upload_progress);
                let event = match result {
                    Ok(receipt) => JobResult::Success(receipt),
                    Err(error) => JobResult::Error(format!("{error:#}")),
                };
                let _ = result_tx.send(event);
            }
        })
        .context("failed to start upload worker")?;

    Ok(())
}

fn process_job(job: Job, upload_progress: &Arc<UploadProgress>) -> Result<ProcessedUpload> {
    let remove_exif =
        job.remove_exif && matches!(&job.source, JobSource::File(_) | JobSource::Clipboard);
    let (mut payload, save_directory) = match job.source {
        JobSource::File(path) => (UploadPayload::from_path(&path)?, None),

        JobSource::Clipboard => (clipboard::read_payload()?, None),

        JobSource::Prepared {
            payload,
            save_directory,
        } => (payload, save_directory),

        JobSource::Recording {
            region,
            stop,
            filename_stem,
            frames_per_second,
            max_seconds,
            capture_resolution,
            save_directory,
        } => (
            crate::recording::capture_region(
                region,
                &stop,
                &filename_stem,
                crate::recording::RecordingSettings {
                    frames_per_second,
                    max_seconds,
                    capture_resolution,
                },
                Instant::now,
            )?,
            save_directory,
        ),
    };

    if remove_exif {
        payload.remove_exif()?;
    }

    let (local_copy, capture_warning) = match save_directory {
        Some(directory) => match payload.save_generated_copy(&directory) {
            Ok(path) => (Some(path), None),

            Err(error) => (
                None,
                Some(format!("local copy could not be saved: {error:#}")),
            ),
        },

        None => (None, None),
    };

    let route = preferred_upload_route(job.require_tor)?;
    let client = UploadClient::for_target(&job.uploader, &route)?;
    let progress = Arc::clone(upload_progress);
    let receipt =
        client.upload_with_progress(payload, job.lifetime_seconds, move |transferred, total| {
            progress.update(transferred, total);
        })?;
    let entry = HistoryEntry::from_receipt(receipt);
    let history_warning = PlatformStorage::open()
        .and_then(|storage| storage.insert_history(&entry))
        .err()
        .map(|error| format!("history could not be saved: {error:#}"));
    let clipboard_warning = clipboard::copy_link(&entry.link)
        .err()
        .map(|error| format!("link was not copied: {error:#}"));

    Ok(ProcessedUpload {
        entry,
        history_warning,
        clipboard_warning,
        local_copy,
        capture_warning,
    })
}

fn start_polling(state: PollingState, resources: CallbackResources) -> Rc<slint::Timer> {
    let timer = Rc::new(slint::Timer::default());
    let timer_for_callback = Rc::clone(&timer);
    let mut status = String::with_capacity(256);

    timer.start(
        slint::TimerMode::Repeated,
        ACTIVE_POLL_INTERVAL,
        move || {
            release_requested_ui(&state.ui);

            #[cfg(target_os = "linux")]
            while gtk::glib::MainContext::default().pending() {
                let _ = gtk::glib::MainContext::default().iteration(false);
            }

            if let Some(window) = current_window(&state.ui) {
                update_upload_progress(&window, &state.upload_progress);
                receive_job_result(&window, &state.result_rx, &state, &mut status);
                receive_update(&window, &state.update_rx, &state.update_state);
            }

            promote_visible_transient_ui(&state.ui, &resources.storage);

            if let Some(action) = state.hotkeys.borrow().as_ref().and_then(Hotkeys::poll) {
                handle_action(action.into(), &state, &resources);
            }

            if let Some(action) = state.tray.borrow().as_ref().and_then(Tray::poll) {
                handle_action(action, &state, &resources);
            }

            release_idle_transient_ui(&state.ui, &resources.storage);
            let interval = current_window(&state.ui).map_or(IDLE_POLL_INTERVAL, |window| {
                if window.window().is_visible() || window.get_busy() {
                    ACTIVE_POLL_INTERVAL
                } else {
                    IDLE_POLL_INTERVAL
                }
            });

            if timer_for_callback.interval() != interval {
                timer_for_callback.set_interval(interval);
            }
        },
    );

    timer
}

fn current_window(ui: &UiSlot) -> Option<AppWindow> {
    ui.borrow()
        .as_ref()
        .map(|session| session.window.clone_strong())
}

fn release_requested_ui(ui: &UiSlot) {
    let requested = ui
        .borrow()
        .as_ref()
        .is_some_and(|session| session.close_requested.get());

    if requested {
        drop(ui.borrow_mut().take());
    }
}

fn promote_visible_transient_ui(ui: &UiSlot, storage: &PlatformStorage) {
    let should_promote = ui
        .borrow()
        .as_ref()
        .is_some_and(|session| session.transient && session.window.window().is_visible());

    if should_promote {
        let _ = promote_ui(ui, storage);
    }
}

fn release_idle_transient_ui(ui: &UiSlot, storage: &PlatformStorage) {
    let should_release = ui.borrow().as_ref().is_some_and(|session| {
        session.transient && !session.window.window().is_visible() && !session.window.get_busy()
    });

    if !should_release {
        return;
    }

    let reveal_error = ui
        .borrow()
        .as_ref()
        .is_some_and(|session| session.window.get_status_text() == "Upload failed");

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

fn append_status_detail(window: &AppWindow, message: &str) {
    let detail = window.get_status_detail();

    if detail.is_empty() {
        window.set_status_detail(message.into());
    } else {
        window.set_status_detail(format!("{detail}  -  {message}").into());
    }
}

fn update_upload_progress(window: &AppWindow, upload_progress: &UploadProgress) {
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

fn receive_job_result(
    window: &AppWindow,
    result_rx: &Receiver<JobResult>,
    state: &PollingState,
    status: &mut String,
) {
    let Ok(result) = result_rx.try_recv() else {
        return;
    };

    let JobResult::Success(upload) = result else {
        let JobResult::Error(error) = result else {
            return;
        };

        window.set_recording(false);
        *state.recording_stop.borrow_mut() = None;
        show_error(window, &error);
        *state.activity.borrow_mut() = Some(ActivitySnapshot {
            delete_url: String::new(),
            detail: error,
            history_saved: false,
            link: String::new(),
            status: "Upload failed".to_owned(),
        });

        return;
    };

    window.set_busy(false);
    window.set_recording(false);
    window.set_uploading(false);
    window.set_upload_progress(1.0);
    *state.recording_stop.borrow_mut() = None;
    let play_sound = window.get_completion_sound() && upload.clipboard_warning.is_none();
    let history_saved = upload.history_warning.is_none();
    let entry = upload.entry;

    window.set_last_link(entry.link.as_str().into());
    window.set_last_delete_url(entry.delete_url.as_str().into());
    window.set_last_delete_saved(history_saved);
    window.set_status_text("Uploaded".into());

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

    status.push_str("  -  ");
    status.push_str(
        upload
            .clipboard_warning
            .as_deref()
            .unwrap_or("link copied to clipboard"),
    );

    if history_saved
        && let Some(session) = state
            .ui
            .borrow()
            .as_ref()
            .filter(|session| !session.transient)
    {
        views::history::observe_inserted(window, session.history.as_ref(), &entry);
    }

    window.set_status_detail(status.as_str().into());
    *state.activity.borrow_mut() = Some(ActivitySnapshot {
        delete_url: entry.delete_url,
        detail: status.clone(),
        history_saved,
        link: entry.link,
        status: "Uploaded".to_owned(),
    });
}

fn apply_activity(window: &AppWindow, activity: &ActivitySnapshot) {
    window.set_status_text(activity.status.as_str().into());
    window.set_status_detail(activity.detail.as_str().into());
    window.set_last_link(activity.link.as_str().into());
    window.set_last_delete_url(activity.delete_url.as_str().into());
    window.set_last_delete_saved(activity.history_saved);
}

fn receive_update(
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

fn apply_update(window: &AppWindow, update: &UpdateCheck) {
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

fn handle_action(action: TrayAction, state: &PollingState, resources: &CallbackResources) {
    if action == TrayAction::Quit {
        handle_quit(&state.ui);

        return;
    }

    let transient = action != TrayAction::Show;
    let Ok(window) = ensure_ui(&state.ui, resources, transient) else {
        return;
    };

    match action {
        TrayAction::Show => {
            let _ = show_ui(&state.ui, &window);
        }

        TrayAction::Region => {
            region::start_screenshot(&window, state.job_tx.clone());
        }

        TrayAction::Recording => {
            region::toggle_recording(&window, &state.job_tx, &state.recording_stop);
        }

        TrayAction::Screenshot => queue_screenshot(&window, &state.job_tx),

        TrayAction::Clipboard => queue_job(&window, &state.job_tx, JobSource::Clipboard),

        TrayAction::ChooseFile => {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                queue_job(&window, &state.job_tx, JobSource::File(path));
            }
        }

        TrayAction::OpenCaptureFolder => {
            if let Err(error) = views::settings::open_capture_folder(&window) {
                window.set_status_text("Could not open folder".into());
                window.set_status_detail(format!("{error:#}").into());
                let shown = promote_ui(&state.ui, &resources.storage)
                    .and_then(|window| show_ui(&state.ui, &window))
                    .is_ok();

                if !shown {
                    drop(state.ui.borrow_mut().take());
                }
            }
        }

        TrayAction::Quit => {}
    }
}

fn handle_quit(ui: &UiSlot) {
    if let Some(window) = current_window(ui)
        && window.get_busy()
    {
        let _ = window.show();

        window
            .set_status_detail("Finish or stop the current capture/upload before quitting".into());

        return;
    }

    let _ = slint::quit_event_loop();
}

fn show_error(window: &AppWindow, error: &str) {
    window.set_busy(false);
    window.set_uploading(false);
    window.set_status_text("Upload failed".into());
    window.set_status_detail(error.into());
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{CloseAction, CloseState, close_action, region::snap_changed};

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
    fn repeated_hover_does_not_trigger_redundant_snap_updates() {
        let last = Cell::new(None);
        let window = Some([0.1, 0.2, 0.3, 0.4]);

        assert!(snap_changed(&last, window));
        assert!(!snap_changed(&last, window));
        assert!(snap_changed(&last, None));
        assert!(!snap_changed(&last, None));
    }
}
