//! Small system tray menu for background uploads.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use tray_icon::{
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TrayAction {
    Show,
    Region,
    Recording,
    Screenshot,
    Clipboard,
    ChooseFile,
    OpenCaptureFolder,
    Quit,
}

const FALLBACK_DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
#[cfg(windows)]
const MAX_SYSTEM_DOUBLE_CLICK_INTERVAL_MILLIS: u32 = 5_000;

#[cfg(windows)]
fn system_double_click_interval() -> Duration {
    let interval_millis = windows_double_click_interval_millis();

    if (1..=MAX_SYSTEM_DOUBLE_CLICK_INTERVAL_MILLIS).contains(&interval_millis) {
        return Duration::from_millis(u64::from(interval_millis));
    }

    // Invalid platform values use the conventional default.
    FALLBACK_DOUBLE_CLICK_INTERVAL
}

#[cfg(not(windows))]
const fn system_double_click_interval() -> Duration {
    // Platforms without an available native value use the conventional default.
    FALLBACK_DOUBLE_CLICK_INTERVAL
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Windows exposes the double-click interval through a parameterless Win32 API"
)]
fn windows_double_click_interval_millis() -> u32 {
    // SAFETY: GetDoubleClickTime takes no pointers and only reads the user's system setting.
    unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() }
}

#[derive(Clone, Copy, Debug)]
struct PendingClick {
    action: Option<TrayAction>,
    expires_at: Instant,
}

#[derive(Debug)]
struct ClickState {
    double_click_interval: Duration,
    pending: Option<PendingClick>,
    suppress_double_click_until: Option<Instant>,
    suppress_release_until: Option<Instant>,
}

impl ClickState {
    const fn new(double_click_interval: Duration) -> Self {
        Self {
            double_click_interval,
            pending: None,
            suppress_double_click_until: None,
            suppress_release_until: None,
        }
    }

    fn take_expired(&mut self, now: Instant) -> Option<TrayAction> {
        if self.pending.is_none_or(|pending| pending.expires_at > now) {
            return None;
        }

        self.pending.take().and_then(|pending| pending.action)
    }

    fn released(
        &mut self,
        now: Instant,
        configured_action: Option<TrayAction>,
    ) -> Option<TrayAction> {
        if self
            .suppress_release_until
            .take_if(|deadline| now <= *deadline)
            .is_some()
        {
            return None;
        }

        self.suppress_release_until = None;

        if self
            .pending
            .is_some_and(|pending| now <= pending.expires_at)
        {
            self.pending = None;
            self.suppress_double_click_until = Some(now + self.double_click_interval);

            return Some(TrayAction::Show);
        }

        self.pending = Some(PendingClick {
            action: configured_action,
            expires_at: now + self.double_click_interval,
        });

        None
    }

    fn double_clicked(&mut self, now: Instant) -> Option<TrayAction> {
        if self
            .suppress_double_click_until
            .take_if(|deadline| now <= *deadline)
            .is_some()
        {
            return None;
        }

        self.suppress_double_click_until = None;
        self.pending = None;
        self.suppress_release_until = Some(now + self.double_click_interval);

        Some(TrayAction::Show)
    }

    const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

impl Default for ClickState {
    fn default() -> Self {
        Self::new(system_double_click_interval())
    }
}

pub(super) struct Tray {
    _icon: TrayIcon,
    actions: [(MenuId, TrayAction); 8],
    click_state: ClickState,
}

impl Tray {
    pub(super) fn create(icon: tray_icon::Icon) -> Result<Self> {
        let show = MenuItem::new("Show ShareR", true, None);
        let region = MenuItem::new("Capture region", true, None);
        let recording = MenuItem::new("Record region", true, None);
        let screenshot = MenuItem::new("Capture screenshot", true, None);
        let clipboard = MenuItem::new("Upload clipboard", true, None);
        let choose_file = MenuItem::new("Upload file...", true, None);
        let open_capture_folder = MenuItem::new("Open capture folder", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();

        menu.append_items(&[
            &show,
            &PredefinedMenuItem::separator(),
            &region,
            &recording,
            &screenshot,
            &clipboard,
            &choose_file,
            &PredefinedMenuItem::separator(),
            &open_capture_folder,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .context("failed to build the tray menu")?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("ShareR")
            .with_icon(icon)
            .build()
            .context("failed to create the system tray icon")?;

        Ok(Self {
            _icon: icon,
            actions: [
                (show.id().clone(), TrayAction::Show),
                (region.id().clone(), TrayAction::Region),
                (recording.id().clone(), TrayAction::Recording),
                (screenshot.id().clone(), TrayAction::Screenshot),
                (clipboard.id().clone(), TrayAction::Clipboard),
                (choose_file.id().clone(), TrayAction::ChooseFile),
                (
                    open_capture_folder.id().clone(),
                    TrayAction::OpenCaptureFolder,
                ),
                (quit.id().clone(), TrayAction::Quit),
            ],
            click_state: ClickState::default(),
        })
    }

    #[must_use]
    pub(super) fn poll(
        &mut self,
        left_click_action: Option<TrayAction>,
        now: Instant,
    ) -> Option<TrayAction> {
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            return self
                .actions
                .iter()
                .find_map(|(id, action)| (event.id == *id).then_some(*action));
        }

        if let Some(action) = self.click_state.take_expired(now) {
            return Some(action);
        }

        TrayIconEvent::receiver()
            .try_iter()
            .find_map(|event| match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => self.click_state.released(now, left_click_action),
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => self.click_state.double_clicked(now),
                _ => None,
            })
    }

    #[must_use]
    pub(super) const fn has_pending_click(&self) -> bool {
        self.click_state.has_pending()
    }
}

impl std::fmt::Debug for Tray {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Tray").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_click_waits_exactly_the_supplied_short_interval() {
        let now = Instant::now();
        let interval = Duration::from_millis(25);
        let just_before_expiry = interval.checked_sub(Duration::from_millis(1)).unwrap();
        let mut state = ClickState::new(interval);

        assert_eq!(state.released(now, Some(TrayAction::Region)), None);
        assert_eq!(state.take_expired(now + just_before_expiry), None);
        assert!(state.has_pending());
        assert_eq!(state.take_expired(now + interval), Some(TrayAction::Region));
    }

    #[test]
    fn second_release_within_supplied_long_interval_opens_app() {
        let now = Instant::now();
        let interval = Duration::from_secs(2);
        let mut state = ClickState::new(interval);

        assert_eq!(state.released(now, Some(TrayAction::Region)), None);
        assert_eq!(
            state.released(now + Duration::from_millis(1_500), Some(TrayAction::Region)),
            Some(TrayAction::Show)
        );
        assert_eq!(state.take_expired(now + interval), None);
    }

    #[test]
    fn native_double_click_suppresses_release_for_supplied_default_interval() {
        let now = Instant::now();
        let mut state = ClickState::new(FALLBACK_DOUBLE_CLICK_INTERVAL);

        assert_eq!(state.released(now, Some(TrayAction::Region)), None);
        assert_eq!(
            state.double_clicked(now + Duration::from_millis(100)),
            Some(TrayAction::Show)
        );
        assert_eq!(
            state.released(now + Duration::from_millis(110), Some(TrayAction::Region)),
            None
        );
        assert!(!state.has_pending());
    }

    #[test]
    fn no_action_still_allows_double_click_to_open_app() {
        let now = Instant::now();
        let mut state = ClickState::new(FALLBACK_DOUBLE_CLICK_INTERVAL);

        assert_eq!(state.released(now, None), None);
        assert_eq!(
            state.released(now + Duration::from_millis(100), None),
            Some(TrayAction::Show)
        );
    }

    #[test]
    fn no_action_single_click_expires_without_an_action() {
        let now = Instant::now();
        let mut state = ClickState::new(FALLBACK_DOUBLE_CLICK_INTERVAL);

        assert_eq!(state.released(now, None), None);
        assert_eq!(
            state.take_expired(now + FALLBACK_DOUBLE_CLICK_INTERVAL),
            None
        );
        assert!(!state.has_pending());
    }
}
