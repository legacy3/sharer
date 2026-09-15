//! Global shortcut registration and event mapping.

use anyhow::{Context as _, Result, anyhow, bail};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, hotkey::HotKey};
use slint::winit_030::winit::keyboard::{KeyCode, ModifiersState};

use sharer::config::ShortcutConfig;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HotkeyAction {
    Region,
    Recording,
    Screenshot,
    Clipboard,
}

/// Owns registered global shortcuts for the application's lifetime.
pub(super) struct Hotkeys {
    _manager: Option<GlobalHotKeyManager>,
    region: Option<HotKey>,
    recording: Option<HotKey>,
    screenshot: Option<HotKey>,
    clipboard: Option<HotKey>,
}

impl Hotkeys {
    /// Register the configured global shortcuts.
    ///
    /// # Errors
    ///
    /// Returns the platform registration error, including conflicting shortcuts.
    pub(super) fn register(config: &ShortcutConfig) -> Result<Self> {
        let [region, recording, screenshot, clipboard] = parse_shortcuts(config)?;
        let enabled = [region, recording, screenshot, clipboard]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        let manager = if enabled.is_empty() {
            None
        } else {
            let manager =
                GlobalHotKeyManager::new().context("failed to initialize global shortcuts")?;

            manager
                .register_all(&enabled)
                .context("one or more shortcuts are already in use")?;
            Some(manager)
        };

        Ok(Self {
            _manager: manager,
            region,
            recording,
            screenshot,
            clipboard,
        })
    }

    #[must_use]
    pub(super) fn poll(&self) -> Option<HotkeyAction> {
        let event = GlobalHotKeyEvent::receiver().try_recv().ok()?;

        if event.state != global_hotkey::HotKeyState::Pressed {
            return None;
        }

        match event.id {
            id if self.region.is_some_and(|shortcut| id == shortcut.id()) => {
                Some(HotkeyAction::Region)
            }

            id if self.recording.is_some_and(|shortcut| id == shortcut.id()) => {
                Some(HotkeyAction::Recording)
            }

            id if self.screenshot.is_some_and(|shortcut| id == shortcut.id()) => {
                Some(HotkeyAction::Screenshot)
            }

            id if self.clipboard.is_some_and(|shortcut| id == shortcut.id()) => {
                Some(HotkeyAction::Clipboard)
            }

            _ => None,
        }
    }
}

impl std::fmt::Debug for Hotkeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hotkeys")
            .field("region", &self.region)
            .field("recording", &self.recording)
            .field("screenshot", &self.screenshot)
            .field("clipboard", &self.clipboard)
            .finish_non_exhaustive()
    }
}

pub(super) fn validate_shortcuts(config: &ShortcutConfig) -> Result<()> {
    let _ = parse_shortcuts(config)?;

    Ok(())
}

fn parse_shortcuts(config: &ShortcutConfig) -> Result<[Option<HotKey>; 4]> {
    let region = parse_shortcut("region", &config.region)?;
    let recording = parse_shortcut("recording", &config.recording)?;
    let screenshot = parse_shortcut("entire-screen", &config.screen)?;
    let clipboard = parse_shortcut("clipboard", &config.clipboard)?;

    let named_shortcuts = [
        ("region", region),
        ("recording", recording),
        ("entire-screen", screenshot),
        ("clipboard", clipboard),
    ];

    for (index, (name, shortcut)) in named_shortcuts.iter().enumerate() {
        if let Some((other_name, _)) = shortcut.and_then(|shortcut| {
            named_shortcuts[..index].iter().find(|(_, other_shortcut)| {
                other_shortcut.is_some_and(|other| other.id() == shortcut.id())
            })
        }) {
            bail!("{name} shortcut duplicates the {other_name} shortcut");
        }
    }

    Ok([region, recording, screenshot, clipboard])
}

fn parse_shortcut(name: &str, value: &str) -> Result<Option<HotKey>> {
    let value = value.trim();

    if value.is_empty() {
        return Ok(None);
    }

    value
        .parse()
        .map(Some)
        .map_err(|source| anyhow!("invalid {name} shortcut: {source}"))
}

pub(super) fn captured_shortcut(
    modifiers: ModifiersState,
    code: KeyCode,
) -> Result<Option<String>> {
    if matches!(
        code,
        KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    ) {
        return Ok(None);
    }

    let mut shortcut = String::new();

    if modifiers.control_key() {
        shortcut.push_str("control+");
    }

    if modifiers.shift_key() {
        shortcut.push_str("shift+");
    }

    if modifiers.alt_key() {
        shortcut.push_str("alt+");
    }

    if modifiers.super_key() {
        shortcut.push_str("super+");
    }

    let code = format!("{code:?}");
    let display_code = code
        .strip_prefix("Key")
        .or_else(|| code.strip_prefix("Digit"))
        .unwrap_or(&code);

    shortcut.push_str(display_code);

    let _ = shortcut
        .parse::<HotKey>()
        .map_err(|source| anyhow!("unsupported shortcut: {source}"))?;

    Ok(Some(shortcut))
}

pub(super) fn shortcuts_enabled(config: &ShortcutConfig) -> bool {
    [
        &config.region,
        &config.recording,
        &config.screen,
        &config.clipboard,
    ]
    .into_iter()
    .any(|shortcut| !shortcut.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_shortcuts_name_both_conflicting_actions() {
        let mut config = ShortcutConfig::default();

        config.clipboard.clone_from(&config.screen);
        let error = parse_shortcuts(&config).unwrap_err();

        assert_eq!(
            error.to_string(),
            "clipboard shortcut duplicates the entire-screen shortcut"
        );
    }

    #[test]
    fn empty_shortcuts_are_disabled_and_do_not_conflict() {
        let config = ShortcutConfig {
            region: String::new(),
            recording: "  ".to_owned(),
            screen: String::new(),
            clipboard: String::new(),
        };

        assert_eq!(parse_shortcuts(&config).unwrap(), [None; 4]);
        assert!(!shortcuts_enabled(&config));
    }

    #[test]
    fn captured_shortcuts_are_canonical_and_ignore_modifier_only_presses() {
        let modifiers = ModifiersState::CONTROL | ModifiersState::SHIFT;

        assert_eq!(
            captured_shortcut(modifiers, KeyCode::KeyV).unwrap(),
            Some("control+shift+V".to_owned())
        );
        assert_eq!(
            captured_shortcut(modifiers, KeyCode::ControlLeft).unwrap(),
            None
        );
        assert_eq!(
            captured_shortcut(ModifiersState::empty(), KeyCode::PrintScreen).unwrap(),
            Some("PrintScreen".to_owned())
        );
    }
}
