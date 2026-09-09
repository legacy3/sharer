//! Small system tray menu for background uploads.

use anyhow::{Context as _, Result};
use tray_icon::{
    MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
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

pub(super) struct Tray {
    _icon: TrayIcon,
    actions: [(MenuId, TrayAction); 8],
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
            .with_menu_on_left_click(!cfg!(windows))
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
        })
    }

    #[must_use]
    pub(super) fn poll(&self) -> Option<TrayAction> {
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            return self
                .actions
                .iter()
                .find_map(|(id, action)| (event.id == *id).then_some(*action));
        }

        TrayIconEvent::receiver().try_iter().find_map(|event| {
            matches!(
                event,
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            )
            .then_some(TrayAction::Show)
        })
    }
}

impl std::fmt::Debug for Tray {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Tray").finish_non_exhaustive()
    }
}
