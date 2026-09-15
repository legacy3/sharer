//! Desktop window wiring and background job coordination.

use std::{
    cell::{Cell, RefCell},
    fmt::Write as _,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use num_traits::ToPrimitive as _;
use slint::ComponentHandle as _;
use slint::winit_030::WinitWindowAccessor as _;
use slint::winit_030::winit::{
    event::ElementState,
    keyboard::{KeyCode, PhysicalKey},
};

use crate::{
    AppWindow,
    cli::RendererPreference,
    hotkeys::{HotkeyAction, Hotkeys, captured_shortcut, shortcuts_enabled},
    icon,
    tray::{Tray, TrayAction},
    update::{UpdateCheck, spawn_check},
    views,
};
use sharer::{
    capture, clipboard,
    config::{
        AppConfig, CaptureResolution, ResizeQuality, ShortcutConfig, TrayClickAction,
        display_lifetime,
    },
    history::HistoryEntry,
    proxy::{TorProxy, preferred_upload_route},
    storage::PlatformStorage,
    upload::{
        UploadCancellation, UploadClient, UploadPayload, UploadTarget, is_upload_cancelled,
        is_upload_timeout,
    },
    validate_lifetime,
};

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(750);
const CAPTURE_HIDE_DELAY: Duration = Duration::from_millis(100);
type RecordingControl = Rc<RefCell<Option<Sender<()>>>>;
type UiSlot = Rc<RefCell<Option<UiSession>>>;

mod job;
mod region;
mod result;

use job::process_job;
#[cfg(test)]
use job::process_job_with_history;
use result::{
    append_status_detail, apply_activity, apply_update, current_window,
    invalidate_latest_local_path, promote_visible_transient_ui, receive_job_result, receive_update,
    release_idle_transient_ui, release_requested_ui, update_upload_progress,
};
#[cfg(test)]
use result::{invalidate_activity_local_path, result_status};

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
    RegionCapture {
        selection: capture::SelectedRegionCapture,
        filename_stem: String,
        capture_resolution: CaptureResolution,
        resize_quality: ResizeQuality,
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
    upload: Option<UploadOptions>,
    remove_exif: bool,
    cancellation: Arc<UploadCancellation>,
    restore_window_after_capture: bool,
}

#[derive(Clone, Debug)]
struct UploadOptions {
    uploader: UploadTarget,
    lifetime_seconds: u32,
    require_tor: bool,
}

struct PollingState {
    activity: Rc<RefCell<Option<ActivitySnapshot>>>,
    hotkeys: Rc<RefCell<Option<Hotkeys>>>,
    job_tx: Sender<Job>,
    result_rx: Receiver<JobResult>,
    recording_stop: RecordingControl,
    storage: Rc<PlatformStorage>,
    tray: Rc<RefCell<Option<Tray>>>,
    ui: UiSlot,
    update_rx: Receiver<UpdateCheck>,
    update_state: Rc<RefCell<Option<UpdateCheck>>>,
    upload_progress: Arc<UploadProgress>,
    upload_cancellation: Arc<UploadCancellation>,
    quit_after_job: Rc<Cell<bool>>,
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
    upload_cancellation: Arc<UploadCancellation>,
}

struct UiSession {
    close_requested: Rc<Cell<bool>>,
    history: Rc<RefCell<views::history::HistoryController>>,
    transient: bool,
    window: AppWindow,
}

#[derive(Debug)]
struct ActivitySnapshot {
    entry_id: i64,
    delete_url: String,
    detail: String,
    history_saved: bool,
    link: String,
    local_path: String,
    status: String,
}

#[derive(Debug)]
enum JobResult {
    CaptureComplete,
    Success(Box<ProcessedUpload>),
    Error {
        message: String,
        stage: FailureStage,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuitDecision {
    ExitNow,
    WaitForUpload { cancel: bool },
    BlockedByCapture,
}

#[derive(Clone, Copy, Debug)]
struct QuitState {
    busy: bool,
    uploading: bool,
    already_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureStage {
    Capture,
    Save,
    Settings,
    Upload,
    UploadCancelled,
    UploadTimedOut,
}

impl FailureStage {
    const fn status(self) -> &'static str {
        match self {
            Self::Capture => "Capture failed",
            Self::Save => "Save failed",
            Self::Settings => "Settings invalid",
            Self::Upload => "Upload failed",
            Self::UploadCancelled => "Upload cancelled",
            Self::UploadTimedOut => "Upload timed out",
        }
    }
}

#[derive(Debug)]
struct JobFailure {
    error: anyhow::Error,
    stage: FailureStage,
}

impl JobFailure {
    const fn new(error: anyhow::Error, stage: FailureStage) -> Self {
        Self { error, stage }
    }
}

#[derive(Debug)]
struct UploadFailure {
    message: String,
    stage: FailureStage,
}

#[derive(Debug)]
struct UploadProgress {
    active: AtomicBool,
    transferred: AtomicU64,
    total: AtomicU64,
}

impl UploadProgress {
    const PENDING: u64 = u64::MAX;

    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            transferred: AtomicU64::new(0),
            total: AtomicU64::new(Self::PENDING),
        }
    }

    fn reset(&self) {
        self.active.store(false, Ordering::Release);
        self.transferred.store(0, Ordering::Relaxed);
        self.total.store(Self::PENDING, Ordering::Release);
    }

    fn start(&self) {
        self.active.store(true, Ordering::Release);
    }

    fn finish(&self) {
        self.active.store(false, Ordering::Release);
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
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
    upload_failure: Option<UploadFailure>,
}

struct JobCallbacks<P, C> {
    persist_history: P,
    capture_complete: C,
}

pub fn run(show_on_start: bool, renderer: RendererPreference) -> Result<()> {
    select_renderer(renderer)?;

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
            shortcut_status(&config.borrow().shortcuts).clone_into(&mut hotkey_status.borrow_mut());
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
    let upload_cancellation = Arc::new(UploadCancellation::default());
    let quit_after_job = Rc::new(Cell::new(false));

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
        upload_cancellation: Arc::clone(&upload_cancellation),
    };
    let ui = Rc::new(RefCell::new(None));
    let tray = Rc::new(RefCell::new(None));
    let polling_state = PollingState {
        activity,
        hotkeys,
        job_tx,
        result_rx,
        recording_stop,
        storage,
        tray,
        ui,
        update_rx,
        update_state,
        upload_progress,
        upload_cancellation,
        quit_after_job,
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
    let timer = start_polling(polling_state, callback_resources, Instant::now);

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

    force_full_repaint(window);
    foreground_ui(window);

    Ok(())
}

fn foreground_ui(window: &AppWindow) {
    if window
        .window()
        .with_winit_window(foreground_native_window)
        .is_some()
    {
        return;
    }

    let window = window.as_weak();
    let _ = slint::spawn_local(async move {
        let Some(window) = window.upgrade() else {
            return;
        };
        let Ok(native_window) = window.window().winit_window().await else {
            return;
        };

        foreground_native_window(&native_window);
    });
}

fn foreground_native_window(native_window: &slint::winit_030::winit::window::Window) {
    native_window.set_minimized(false);
    native_window.focus_window();
    native_window.request_redraw();

    #[cfg(target_os = "linux")]
    {
        use slint::winit_030::winit::{
            platform::wayland::WindowExtWayland as _, window::UserAttentionType,
        };

        if native_window.xdg_toplevel().is_some() {
            native_window.request_user_attention(Some(UserAttentionType::Informational));
        }
    }
}

fn select_renderer(preference: RendererPreference) -> Result<()> {
    let Some(renderer_name) = renderer_name(preference) else {
        return Ok(());
    };

    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(renderer_name.into())
        .select()
        .with_context(|| format!("failed to initialize the {renderer_name} renderer"))
}

const fn renderer_name(preference: RendererPreference) -> Option<&'static str> {
    match preference {
        RendererPreference::Auto => None,
        RendererPreference::Gpu => Some(platform_gpu_renderer()),
        RendererPreference::Software => Some("software"),
    }
}

const fn platform_gpu_renderer() -> &'static str {
    if cfg!(target_os = "macos") {
        "skia"
    } else {
        "femtovg"
    }
}

fn force_full_repaint(window: &AppWindow) {
    window.set_repaint_sequence(window.get_repaint_sequence().wrapping_add(1));
    window.window().request_redraw();
}

fn install_window_event_filter(window: &AppWindow, resources: &CallbackResources) {
    use slint::winit_030::EventResult;
    use slint::winit_030::winit::{event::WindowEvent, keyboard::ModifiersState};

    let weak = window.as_weak();
    let repaint_pending = Cell::new(false);
    let modifiers = Cell::new(ModifiersState::empty());
    let hotkeys = Rc::clone(&resources.hotkeys);
    let hotkey_status = Rc::clone(&resources.hotkey_status);
    let config = Rc::clone(&resources.config);

    window.window().on_winit_window_event(move |_, event| {
        if restore_event_requests_full_repaint(&repaint_pending, event)
            && let Some(window) = weak.upgrade()
        {
            force_full_repaint(&window);
        }

        if let WindowEvent::ModifiersChanged(next) = event {
            modifiers.set(next.state());
        }

        let Some(window) = weak.upgrade() else {
            return EventResult::Propagate;
        };

        if window.get_shortcut_recording() == 0 {
            return EventResult::Propagate;
        }

        if matches!(event, WindowEvent::Focused(false)) {
            window.set_shortcut_recording(0);
            restore_configured_hotkeys(&window, &hotkeys, &hotkey_status, &config);

            return EventResult::Propagate;
        }

        let WindowEvent::KeyboardInput { event, .. } = event else {
            return EventResult::Propagate;
        };

        if !shortcut_event_should_capture(event.state, event.repeat, event.physical_key) {
            return EventResult::PreventDefault;
        }

        let PhysicalKey::Code(code) = event.physical_key else {
            cancel_shortcut_recording(
                &window,
                &hotkeys,
                &hotkey_status,
                &config,
                "This key cannot be used as a global shortcut",
            );

            return EventResult::PreventDefault;
        };

        if code == KeyCode::Escape {
            cancel_shortcut_recording(
                &window,
                &hotkeys,
                &hotkey_status,
                &config,
                "Shortcut unchanged",
            );

            return EventResult::PreventDefault;
        }

        if matches!(code, KeyCode::Backspace | KeyCode::Delete) {
            set_recorded_shortcut(&window, String::new());
            window.set_shortcut_recording(0);
            restore_configured_hotkeys(&window, &hotkeys, &hotkey_status, &config);
            window.set_status_detail("Shortcut disabled. Save changes to apply it.".into());

            return EventResult::PreventDefault;
        }

        match captured_shortcut(modifiers.get(), code) {
            Ok(Some(shortcut)) => {
                set_recorded_shortcut(&window, shortcut);
                window.set_shortcut_recording(0);
                restore_configured_hotkeys(&window, &hotkeys, &hotkey_status, &config);
                window.set_status_detail("Shortcut recorded. Save changes to apply it.".into());
            }

            Ok(None) => {}

            Err(error) => cancel_shortcut_recording(
                &window,
                &hotkeys,
                &hotkey_status,
                &config,
                &error.to_string(),
            ),
        }

        EventResult::PreventDefault
    });
}

fn shortcut_event_should_capture(
    state: ElementState,
    repeat: bool,
    physical_key: PhysicalKey,
) -> bool {
    !repeat
        && (state == ElementState::Pressed
            || physical_key == PhysicalKey::Code(KeyCode::PrintScreen))
}

fn set_recorded_shortcut(window: &AppWindow, shortcut: String) {
    let shortcut = shortcut.into();

    match window.get_shortcut_recording() {
        1 => window.set_region_hotkey(shortcut),
        2 => window.set_recording_hotkey(shortcut),
        3 => window.set_screen_hotkey(shortcut),
        4 => window.set_clipboard_hotkey(shortcut),
        _ => {}
    }
}

fn cancel_shortcut_recording(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &RefCell<String>,
    config: &RefCell<AppConfig>,
    detail: &str,
) {
    window.set_shortcut_recording(0);
    restore_configured_hotkeys(window, hotkeys, hotkey_status, config);
    window.set_status_detail(detail.into());
}

fn restore_configured_hotkeys(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &RefCell<String>,
    config: &RefCell<AppConfig>,
) {
    *hotkeys.borrow_mut() = None;
    let config = config.borrow();
    let status = match Hotkeys::register(&config.shortcuts) {
        Ok(registered) => {
            *hotkeys.borrow_mut() = Some(registered);
            shortcut_status(&config.shortcuts).to_owned()
        }

        Err(error) => format!("Shortcuts unavailable: {error}"),
    };

    status.clone_into(&mut hotkey_status.borrow_mut());
    window.set_hotkey_status(status.into());
}

fn shortcut_status(shortcuts: &ShortcutConfig) -> &'static str {
    if shortcuts_enabled(shortcuts) {
        "Shortcuts active"
    } else {
        "Shortcuts disabled"
    }
}

fn restore_event_requests_full_repaint(
    repaint_pending: &Cell<bool>,
    event: &slint::winit_030::winit::event::WindowEvent,
) -> bool {
    use slint::winit_030::winit::event::WindowEvent;

    match event {
        WindowEvent::Focused(false) | WindowEvent::Occluded(true) => {
            repaint_pending.set(true);
            false
        }

        WindowEvent::Focused(true) | WindowEvent::Occluded(false) => repaint_pending.replace(false),

        _ => false,
    }
}

fn ensure_ui(ui: &UiSlot, resources: &CallbackResources, transient: bool) -> Result<AppWindow> {
    if ui.borrow().is_none() {
        let window = AppWindow::new().context("failed to create application window")?;

        install_window_event_filter(&window, resources);

        views::settings::initialize(&window, &resources.config.borrow());
        window.set_hotkey_status(resources.hotkey_status.borrow().as_str().into());

        if !transient && let Some(activity) = resources.activity.borrow().as_ref() {
            apply_activity(&window, activity);
        }

        if let Some(update) = resources.update_state.borrow().as_ref() {
            apply_update(&window, update);
        }

        let activity = Rc::clone(&resources.activity);
        let local_deleted: views::history::LocalDeletionObserver =
            Rc::new(move |window, id, path| {
                invalidate_latest_local_path(window, activity.as_ref(), id, path);
            });
        let history =
            views::history::initialize(&window, &resources.storage, transient, &local_deleted)?;
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
    let cancellation = Arc::clone(&resources.upload_cancellation);

    window.on_capture_region(move || {
        if let Some(window) = window_weak.upgrade() {
            region::start_screenshot(&window, sender.clone(), Arc::clone(&cancellation));
        }
    });

    let sender = job_tx.clone();
    let window_weak = window.as_weak();
    let recording_stop_for_callback = Rc::clone(&resources.recording_stop);
    let cancellation = Arc::clone(&resources.upload_cancellation);

    window.on_record_region(move || {
        if let Some(window) = window_weak.upgrade() {
            region::toggle_recording(
                &window,
                &sender,
                &recording_stop_for_callback,
                Arc::clone(&cancellation),
            );
        }
    });

    let sender = job_tx.clone();
    let window_weak = window.as_weak();
    let cancellation = Arc::clone(&resources.upload_cancellation);

    window.on_capture_screen(move || {
        if let Some(window) = window_weak.upgrade() {
            queue_screenshot(&window, &sender, Arc::clone(&cancellation));
        }
    });

    let sender = job_tx.clone();
    let window_weak = window.as_weak();
    let cancellation = Arc::clone(&resources.upload_cancellation);

    window.on_upload_clipboard(move || {
        if let Some(window) = window_weak.upgrade() {
            queue_job(
                &window,
                &sender,
                JobSource::Clipboard,
                Arc::clone(&cancellation),
            );
        }
    });

    let sender = job_tx.clone();
    let window_weak = window.as_weak();
    let cancellation = Arc::clone(&resources.upload_cancellation);

    window.on_choose_file(move || {
        if let Some(path) = rfd::FileDialog::new().pick_file()
            && let Some(window) = window_weak.upgrade()
        {
            queue_job(
                &window,
                &sender,
                JobSource::File(path),
                Arc::clone(&cancellation),
            );
        }
    });

    let window_weak = window.as_weak();
    let cancellation = Arc::clone(&resources.upload_cancellation);

    window.on_cancel_upload(move || {
        if let Some(window) = window_weak.upgrade()
            && window.get_uploading()
        {
            cancellation.cancel();
            window.set_status_text("Cancelling upload...".into());
            window.set_status_detail("Waiting for the active request to stop".into());
        }
    });

    wire_last_result_callbacks(window);
    wire_management_callbacks(
        window,
        &resources.hotkeys,
        &resources.hotkey_status,
        &resources.config,
        &resources.storage,
    );
    wire_close_callback(window, history, close_requested);
}

fn wire_last_result_callbacks(window: &AppWindow) {
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
    let recording_hotkeys = Rc::clone(hotkeys);

    window.on_shortcut_recording_started(move || {
        if let Some(window) = window_weak.upgrade() {
            *recording_hotkeys.borrow_mut() = None;
            window.set_hotkey_status("Shortcuts paused while recording".into());
        }
    });

    let window_weak = window.as_weak();
    let recording_hotkeys = Rc::clone(hotkeys);
    let recording_status = Rc::clone(hotkey_status);
    let recording_config = Rc::clone(config);

    window.on_shortcut_recording_finished(move || {
        if let Some(window) = window_weak.upgrade() {
            restore_configured_hotkeys(
                &window,
                &recording_hotkeys,
                &recording_status,
                &recording_config,
            );
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
            if shortcuts_enabled(&previous.shortcuts) {
                format!("Previous shortcuts remain active: {error}")
            } else {
                format!("Shortcuts remain disabled: {error}")
            }
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
                shortcut_status(&previous.shortcuts).to_owned()
            };

            status.clone_into(&mut hotkey_status.borrow_mut());
            window.set_hotkey_status(status.into());
        }

        show_settings_error(window, &detail);

        return;
    }

    window.set_capture_directory(next.capture_directory.clone().into());
    *config.borrow_mut() = next;
    window.set_settings_dirty(false);

    if shortcuts_changed {
        let status = shortcut_status(&config.borrow().shortcuts);

        status.clone_into(&mut hotkey_status.borrow_mut());
        window.set_hotkey_status(status.into());
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

        if window.get_shortcut_recording() != 0 {
            window.set_shortcut_recording(0);
            window.invoke_shortcut_recording_finished();
        }

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

fn queue_screenshot(
    window: &AppWindow,
    sender: &Sender<Job>,
    cancellation: Arc<UploadCancellation>,
) {
    if window.get_busy() {
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
    let color_mode = region::capture_color_mode(window);
    let capture_resolution = region::capture_resolution(window);
    let resize_quality = region::resize_quality(window);
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
        let result = capture::primary_monitor(color_mode, capture_resolution, resize_quality).map(
            |mut payload| {
                payload.filename =
                    capture::named_capture_filename(&filename_stem, &payload.filename);
                payload
            },
        );

        if was_visible {
            let _ = window.show();
        }

        window.set_busy(false);

        match result {
            Ok(payload) => {
                queue_capture_job(
                    &window,
                    &sender,
                    JobSource::Prepared {
                        payload,
                        save_directory,
                    },
                    cancellation,
                );
            }

            Err(error) => present_error(&window, FailureStage::Capture, &format!("{error:#}")),
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

fn queue_job(
    window: &AppWindow,
    sender: &Sender<Job>,
    source: JobSource,
    cancellation: Arc<UploadCancellation>,
) {
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
    cancellation.reset();

    if sender
        .send(Job {
            source,
            upload: Some(UploadOptions {
                uploader,
                lifetime_seconds,
                require_tor: window.get_require_tor(),
            }),
            remove_exif: window.get_remove_exif(),
            cancellation,
            restore_window_after_capture: false,
        })
        .is_err()
    {
        present_error(
            window,
            FailureStage::Upload,
            "Upload worker stopped unexpectedly",
        );
    }
}

fn queue_capture_job(
    window: &AppWindow,
    sender: &Sender<Job>,
    source: JobSource,
    cancellation: Arc<UploadCancellation>,
) {
    if window.get_busy() {
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
        })
    } else {
        None
    };

    if upload.is_none() && !window.get_save_captures() {
        present_error(
            window,
            FailureStage::Settings,
            "Enable local capture copies or automatic capture uploads",
        );

        return;
    }

    clear_last_receipt(window);
    window.set_busy(true);
    window.set_upload_progress(0.0);

    if let Some(options) = &upload {
        if window.get_save_captures() {
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

    cancellation.reset();

    if sender
        .send(Job {
            source,
            upload,
            remove_exif: window.get_remove_exif(),
            cancellation,
            restore_window_after_capture: false,
        })
        .is_err()
    {
        present_error(
            window,
            FailureStage::Capture,
            "Capture worker stopped unexpectedly",
        );
    }
}

fn clear_last_receipt(window: &AppWindow) {
    window.set_last_link("".into());
    window.set_last_local_path("".into());
    window.set_last_delete_url("".into());
    window.set_last_delete_saved(false);
}

fn run_last_local_action(
    window: &AppWindow,
    action: impl FnOnce(&std::path::Path) -> Result<&'static str>,
) {
    let path = PathBuf::from(window.get_last_local_path().as_str());

    if path.as_os_str().is_empty() {
        return;
    }

    match action(&path) {
        Ok(detail) => window.set_status_detail(detail.into()),

        Err(error) => {
            window.set_status_detail(format!("Local file action failed: {error:#}").into());
        }
    }
}

fn valid_window_lifetime(window: &AppWindow) -> Option<u32> {
    let Ok(lifetime_seconds) = u32::try_from(window.get_lifetime_seconds()) else {
        present_error(
            window,
            FailureStage::Settings,
            "Lifetime must be a positive number of seconds",
        );

        return None;
    };

    if let Err(error) = validate_lifetime(lifetime_seconds) {
        present_error(window, FailureStage::Settings, &error.to_string());

        return None;
    }

    Some(lifetime_seconds)
}

fn valid_window_upload_target(window: &AppWindow) -> Option<UploadTarget> {
    match views::settings::upload_target_from_window(window) {
        Ok(target) => Some(target),

        Err(error) => {
            present_error(
                window,
                FailureStage::Settings,
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
                let restore_window_after_capture = job.restore_window_after_capture;
                let result = process_job(job, &upload_progress, || {
                    if restore_window_after_capture {
                        let _ = result_tx.send(JobResult::CaptureComplete);
                    }
                });

                upload_progress.finish();
                let event = match result {
                    Ok(receipt) => JobResult::Success(Box::new(receipt)),

                    Err(error) => JobResult::Error {
                        message: format!("{:#}", error.error),
                        stage: error.stage,
                    },
                };
                let _ = result_tx.send(event);
            }
        })
        .context("failed to start upload worker")?;

    Ok(())
}

fn start_polling(
    state: PollingState,
    resources: CallbackResources,
    now: fn() -> Instant,
) -> Rc<slint::Timer> {
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

            let tray_click_action =
                tray_action_for_click(resources.config.borrow().behavior.tray_click_action);
            let tray_action = state
                .tray
                .borrow_mut()
                .as_mut()
                .and_then(|tray| tray.poll(tray_click_action, now()));

            if let Some(action) = tray_action {
                handle_action(action, &state, &resources);
            }

            release_idle_transient_ui(&state.ui, &resources.storage);
            let active_ui = current_window(&state.ui)
                .is_some_and(|window| window.window().is_visible() || window.get_busy());
            let tray_click_pending = state
                .tray
                .borrow()
                .as_ref()
                .is_some_and(Tray::has_pending_click);
            let interval = if active_ui || tray_click_pending {
                ACTIVE_POLL_INTERVAL
            } else {
                IDLE_POLL_INTERVAL
            };

            if timer_for_callback.interval() != interval {
                timer_for_callback.set_interval(interval);
            }
        },
    );

    timer
}

const fn tray_action_for_click(action: TrayClickAction) -> Option<TrayAction> {
    match action {
        TrayClickAction::Region => Some(TrayAction::Region),
        TrayClickAction::Show => Some(TrayAction::Show),
        TrayClickAction::Screenshot => Some(TrayAction::Screenshot),
        TrayClickAction::Recording => Some(TrayAction::Recording),
        TrayClickAction::Clipboard => Some(TrayAction::Clipboard),
        TrayClickAction::None => None,
    }
}

fn handle_action(action: TrayAction, state: &PollingState, resources: &CallbackResources) {
    if action == TrayAction::Quit {
        handle_quit(&state.ui, &state.upload_cancellation, &state.quit_after_job);

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
            region::start_screenshot(
                &window,
                state.job_tx.clone(),
                Arc::clone(&state.upload_cancellation),
            );
        }

        TrayAction::Recording => {
            region::toggle_recording(
                &window,
                &state.job_tx,
                &state.recording_stop,
                Arc::clone(&state.upload_cancellation),
            );
        }

        TrayAction::Screenshot => queue_screenshot(
            &window,
            &state.job_tx,
            Arc::clone(&state.upload_cancellation),
        ),

        TrayAction::Clipboard => queue_job(
            &window,
            &state.job_tx,
            JobSource::Clipboard,
            Arc::clone(&state.upload_cancellation),
        ),

        TrayAction::ChooseFile => {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                queue_job(
                    &window,
                    &state.job_tx,
                    JobSource::File(path),
                    Arc::clone(&state.upload_cancellation),
                );
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

fn handle_quit(ui: &UiSlot, upload_cancellation: &UploadCancellation, quit_after_job: &Cell<bool>) {
    let window = current_window(ui);
    let decision = quit_decision(QuitState {
        busy: window.as_ref().is_some_and(AppWindow::get_busy),
        uploading: window.as_ref().is_some_and(AppWindow::get_uploading),
        already_pending: quit_after_job.get(),
    });

    match decision {
        QuitDecision::ExitNow => {
            let _ = slint::quit_event_loop();
        }

        QuitDecision::WaitForUpload { cancel } => {
            quit_after_job.set(true);

            if cancel {
                upload_cancellation.cancel();
            }

            if let Some(window) = window {
                window.set_status_text("Cancelling upload...".into());
                window.set_status_detail(
                    "ShareR will quit after the local result and history are safely finalized"
                        .into(),
                );
            }
        }

        QuitDecision::BlockedByCapture => {
            if let Some(window) = window {
                let _ = window.show();

                window.set_status_detail(
                    "Finish or stop the current capture/upload before quitting".into(),
                );
            }
        }
    }
}

const fn quit_decision(state: QuitState) -> QuitDecision {
    if state.busy && state.uploading {
        QuitDecision::WaitForUpload {
            cancel: !state.already_pending,
        }
    } else if state.busy {
        QuitDecision::BlockedByCapture
    } else {
        QuitDecision::ExitNow
    }
}

fn present_error(window: &AppWindow, stage: FailureStage, error: &str) {
    window.set_busy(false);
    window.set_uploading(false);
    window.set_status_text(stage.status().into());
    window.set_status_detail(error.into());
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
