//! Global shortcut registration and event mapping.

use anyhow::{Context as _, Result, anyhow, bail};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, hotkey::HotKey};

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
    _manager: GlobalHotKeyManager,
    region: HotKey,
    recording: HotKey,
    screenshot: HotKey,
    clipboard: HotKey,
}

impl Hotkeys {
    /// Register the configured global shortcuts.
    ///
    /// # Errors
    ///
    /// Returns the platform registration error, including conflicting shortcuts.
    pub(super) fn register(config: &ShortcutConfig) -> Result<Self> {
        let [region, recording, screenshot, clipboard] = parse_shortcuts(config)?;

        let manager =
            GlobalHotKeyManager::new().context("failed to initialize global shortcuts")?;

        manager
            .register_all(&[region, recording, screenshot, clipboard])
            .context("one or more shortcuts are already in use")?;

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
            id if id == self.region.id() => Some(HotkeyAction::Region),
            id if id == self.recording.id() => Some(HotkeyAction::Recording),
            id if id == self.screenshot.id() => Some(HotkeyAction::Screenshot),
            id if id == self.clipboard.id() => Some(HotkeyAction::Clipboard),
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

fn parse_shortcuts(config: &ShortcutConfig) -> Result<[HotKey; 4]> {
    let region: HotKey = config
        .region
        .parse()
        .map_err(|source| anyhow!("invalid region shortcut: {source}"))?;
    let screenshot: HotKey = config
        .screen
        .parse()
        .map_err(|source| anyhow!("invalid entire-screen shortcut: {source}"))?;
    let recording: HotKey = config
        .recording
        .parse()
        .map_err(|source| anyhow!("invalid recording shortcut: {source}"))?;
    let clipboard: HotKey = config
        .clipboard
        .parse()
        .map_err(|source| anyhow!("invalid clipboard shortcut: {source}"))?;

    let named_shortcuts = [
        ("region", &region),
        ("recording", &recording),
        ("entire-screen", &screenshot),
        ("clipboard", &clipboard),
    ];

    for (index, (name, shortcut)) in named_shortcuts.iter().enumerate() {
        if let Some((other_name, _other_shortcut)) = named_shortcuts[..index]
            .iter()
            .find(|(_other_name, other_shortcut)| other_shortcut.id() == shortcut.id())
        {
            bail!("{name} shortcut duplicates the {other_name} shortcut");
        }
    }

    Ok([region, recording, screenshot, clipboard])
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
}
