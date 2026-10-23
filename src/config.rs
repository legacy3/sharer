//! Persistent desktop preferences.

use std::fmt;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    DEFAULT_RECORDING_GIB, DEFAULT_RECORDING_SECONDS, MAX_LIFETIME_SECONDS, MAX_RECORDING_GIB,
    storage::Storage,
    upload::{LitterboxRetention, UploadTarget, UploaderKind, canonical_uploader_endpoint},
    validate_lifetime, validate_recording_fps, validate_recording_gib, validate_recording_seconds,
};

const DEFAULT_LIFETIME_SECONDS: u32 = 24 * 60 * 60;
const DEFAULT_RECORDING_FPS: u32 = 30;
const CONFIG_KEY: &str = "application";

/// User preferences stored in the platform configuration directory.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppConfig {
    /// Settings disclosure level; changing it never changes capture or upload behavior.
    pub settings_mode: SettingsMode,
    #[serde(default)]
    pub uploader_kind: UploaderKind,
    pub uploader_url: String,
    pub uploader_credentials: ProviderCredentials,
    pub upload_header_name: String,
    pub upload_header_value: String,
    /// Authorized destination; absent only in configurations predating endpoint binding.
    #[serde(default)]
    pub upload_header_endpoint: Option<String>,
    /// Requested custom-uploader lifetime; zero leaves retention to the server.
    pub lifetime_seconds: u32,
    /// Litterbox retention is separate from the custom-server lifetime.
    pub litterbox_retention: LitterboxRetention,
    pub recording_fps: u32,
    pub recording_max_seconds: u32,
    pub recording_max_gib: u32,
    pub recording_format: RecordingFormat,
    pub capture_directory: String,
    pub capture_naming: CaptureNamingConfig,
    pub shortcuts: ShortcutConfig,
    #[serde(flatten)]
    pub behavior: BehaviorConfig,
    #[serde(flatten)]
    pub privacy: PrivacyConfig,
}

/// Amount of configuration detail shown in the desktop settings view.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsMode {
    #[default]
    Simple,
    Advanced,
}

/// Output format for region recordings.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingFormat {
    #[default]
    Webp,
    Mp4,
}

/// Boolean desktop behavior kept together without changing the on-disk keys.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent persisted checkboxes are clearer as named boolean preferences"
)]
pub struct BehaviorConfig {
    pub save_captures: bool,
    #[serde(default)]
    pub auto_upload_captures: bool,
    pub force_sdr_captures: bool,
    pub capture_resolution: CaptureResolution,
    #[serde(alias = "downscale_filter")]
    pub resize_quality: ResizeQuality,
    pub minimize_to_tray: bool,
    pub start_at_login: bool,
    pub completion_sound: bool,
    pub copy_link_after_upload: bool,
    pub onboarding_complete: bool,
    pub tray_click_action: TrayClickAction,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            save_captures: true,
            auto_upload_captures: true,
            force_sdr_captures: false,
            capture_resolution: CaptureResolution::Native,
            resize_quality: ResizeQuality::Fast,
            minimize_to_tray: true,
            start_at_login: false,
            completion_sound: true,
            copy_link_after_upload: true,
            onboarding_complete: false,
            tray_click_action: TrayClickAction::Region,
        }
    }
}

/// Action invoked by a single left click on the tray icon.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayClickAction {
    #[default]
    Region,
    Show,
    Screenshot,
    Recording,
    Clipboard,
    None,
}

impl TrayClickAction {
    /// Convert a desktop selector index to a tray click action.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Region),
            1 => Some(Self::Show),
            2 => Some(Self::Screenshot),
            3 => Some(Self::Recording),
            4 => Some(Self::Clipboard),
            5 => Some(Self::None),
            _ => None,
        }
    }

    /// Convert the action to the desktop selector index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Region => 0,
            Self::Show => 1,
            Self::Screenshot => 2,
            Self::Recording => 3,
            Self::Clipboard => 4,
            Self::None => 5,
        }
    }
}

/// User-facing resize quality used when a Retina region is reduced to logical size.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeQuality {
    /// Fast linear reduction.
    #[serde(alias = "triangle")]
    #[default]
    Fast,
    /// Cubic reduction balancing sharpness and ringing.
    #[serde(alias = "catmull_rom")]
    Balanced,
    /// Sharp sinc-based reduction suited to text and interface captures.
    #[serde(alias = "lanczos3")]
    Sharp,
}

impl ResizeQuality {
    /// Convert a desktop selector index to a resize quality.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Fast),
            1 => Some(Self::Balanced),
            2 => Some(Self::Sharp),
            _ => None,
        }
    }

    /// Convert the filter to the desktop selector index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Fast => 0,
            Self::Balanced => 1,
            Self::Sharp => 2,
        }
    }
}

/// Pixel density used for generated screenshots and recordings.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureResolution {
    /// Keep every physical display pixel, including Retina scaling.
    #[default]
    Native,
    /// Match macOS logical point dimensions for smaller output.
    Logical,
}

impl CaptureResolution {
    /// Convert a desktop selector index to a capture resolution.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Native),
            1 => Some(Self::Logical),
            _ => None,
        }
    }

    /// Convert the resolution to the desktop selector index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Native => 0,
            Self::Logical => 1,
        }
    }
}

/// Privacy controls for image metadata and network routing.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PrivacyConfig {
    /// Require a local SOCKS5 proxy; the serialized name is retained for compatibility.
    pub require_tor: bool,
    /// Retained only to migrate the original private-name checkbox.
    #[serde(alias = "strip_capture_metadata")]
    pub private_capture_names: bool,
    /// Remove identifying image metadata; the serialized name is retained for compatibility.
    pub remove_exif: bool,
}

/// Credentials isolated by provider so switching uploaders cannot reuse another service's secret.
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ProviderCredentials {
    pub imgur: String,
    pub pixeldrain: String,
    pub vgy: String,
    pub sul: String,
}

impl ProviderCredentials {
    /// Return the credential belonging to an uploader.
    #[must_use]
    pub fn for_provider(&self, kind: UploaderKind) -> &str {
        match kind {
            UploaderKind::Imgur => &self.imgur,
            UploaderKind::Pixeldrain => &self.pixeldrain,
            UploaderKind::Vgy => &self.vgy,
            UploaderKind::Sul => &self.sul,
            UploaderKind::Custom
            | UploaderKind::Uguu
            | UploaderKind::Catbox
            | UploaderKind::Litterbox
            | UploaderKind::TransferSh => "",
        }
    }

    /// Replace only the credential belonging to an uploader.
    pub fn set_for_provider(&mut self, kind: UploaderKind, credential: String) {
        match kind {
            UploaderKind::Imgur => self.imgur = credential,
            UploaderKind::Pixeldrain => self.pixeldrain = credential,
            UploaderKind::Vgy => self.vgy = credential,
            UploaderKind::Sul => self.sul = credential,
            UploaderKind::Custom
            | UploaderKind::Uguu
            | UploaderKind::Catbox
            | UploaderKind::Litterbox
            | UploaderKind::TransferSh => {}
        }
    }
}

impl fmt::Debug for ProviderCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentials")
            .field("imgur_configured", &!self.imgur.is_empty())
            .field("pixeldrain_configured", &!self.pixeldrain.is_empty())
            .field("vgy_configured", &!self.vgy.is_empty())
            .field("sul_configured", &!self.sul.is_empty())
            .finish()
    }
}

/// Naming preferences applied only to generated captures.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct CaptureNamingConfig {
    pub mode: CaptureNameMode,
    pub custom_name: String,
}

/// Strategy used to name generated captures.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureNameMode {
    #[default]
    ActiveWindow,
    Random,
    Friendly,
    Custom,
}

impl CaptureNameMode {
    /// Convert a desktop selector index to a naming mode.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::ActiveWindow),
            1 => Some(Self::Random),
            2 => Some(Self::Friendly),
            3 => Some(Self::Custom),
            _ => None,
        }
    }

    /// Convert the mode to the desktop selector index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::ActiveWindow => 0,
            Self::Random => 1,
            Self::Friendly => 2,
            Self::Custom => 3,
        }
    }
}

impl Default for CaptureNamingConfig {
    fn default() -> Self {
        Self {
            mode: CaptureNameMode::ActiveWindow,
            custom_name: String::new(),
        }
    }
}

/// User-editable global shortcut strings understood by `global-hotkey`.
/// An empty string disables the corresponding action's shortcut.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ShortcutConfig {
    pub region: String,
    pub recording: String,
    pub screen: String,
    pub clipboard: String,
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self {
            region: "control+PrintScreen".to_owned(),
            recording: "control+shift+PrintScreen".to_owned(),
            screen: "PrintScreen".to_owned(),
            clipboard: String::new(),
        }
    }
}

impl AppConfig {
    /// Load saved settings through injected storage, using defaults when absent.
    ///
    /// # Errors
    ///
    /// Returns an error when saved settings cannot be read or decoded.
    pub fn load(storage: &impl Storage) -> Result<Self> {
        let Some(contents) = storage
            .read(CONFIG_KEY)
            .context("failed to read configuration")?
        else {
            return Ok(Self::default());
        };
        let stored = serde_json::from_slice::<serde_json::Value>(&contents).ok();
        let onboarding_key_present = stored
            .as_ref()
            .and_then(|value| value.get("onboarding_complete"))
            .is_some();
        let mut config =
            serde_json::from_slice::<Self>(&contents).context("failed to decode configuration")?;

        // Configurations written before first-run setup existed belong to users
        // who already chose their settings; do not show the wizard to them.
        if !onboarding_key_present {
            config.behavior.onboarding_complete = true;
        }

        if config.upload_header_endpoint.is_none() {
            config.upload_header_endpoint =
                Some(canonical_uploader_endpoint(&config.uploader_url).unwrap_or_default());
        }

        if validate_lifetime(config.lifetime_seconds).is_err() {
            config.lifetime_seconds = DEFAULT_LIFETIME_SECONDS;
        }

        if validate_recording_fps(config.recording_fps).is_err() {
            config.recording_fps = DEFAULT_RECORDING_FPS;
        }

        if validate_recording_seconds(config.recording_max_seconds).is_err() {
            config.recording_max_seconds = DEFAULT_RECORDING_SECONDS;
        }

        if config.recording_max_gib > MAX_RECORDING_GIB {
            config.recording_max_gib = MAX_RECORDING_GIB;
        } else if validate_recording_gib(config.recording_max_gib).is_err() {
            config.recording_max_gib = DEFAULT_RECORDING_GIB;
        }

        if config.privacy.private_capture_names {
            config.capture_naming.mode = CaptureNameMode::Random;
            config.privacy.private_capture_names = false;
        }

        if config.capture_naming.mode != CaptureNameMode::Custom
            && config.capture_naming.custom_name == "screenshot"
        {
            config.capture_naming.custom_name.clear();
        }

        Ok(config)
    }

    /// Persist settings through the configured storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration cannot be encoded or committed.
    pub fn save(&self, storage: &impl Storage) -> Result<()> {
        validate_lifetime(self.lifetime_seconds)?;
        validate_recording_fps(self.recording_fps)?;
        validate_recording_seconds(self.recording_max_seconds)?;
        validate_recording_gib(self.recording_max_gib)?;
        anyhow::ensure!(
            self.behavior.save_captures || self.behavior.auto_upload_captures,
            "enable local capture copies or automatic capture uploads"
        );

        if self.behavior.auto_upload_captures {
            self.upload_target().validate()?;
        }

        self.write_preferences(storage)
    }

    /// Persist presentation mode on the saved configuration without validating operational fields.
    ///
    /// This allows settings disclosure to change while an existing uploader still needs setup.
    /// Call this on saved preferences, never on a form containing unsaved operational changes.
    ///
    /// # Errors
    ///
    /// Returns an error without changing the mode when storage cannot be encoded or committed.
    pub fn save_settings_mode(&mut self, storage: &impl Storage, mode: SettingsMode) -> Result<()> {
        let mut next = self.clone();

        next.settings_mode = mode;
        next.write_preferences(storage)?;
        self.settings_mode = mode;
        Ok(())
    }

    /// Build the owned upload target sent to background workers.
    #[must_use]
    pub fn upload_target(&self) -> UploadTarget {
        self.upload_target_for(self.uploader_kind)
    }

    /// Build an owned target for a specific provider without crossing credential boundaries.
    #[must_use]
    pub fn upload_target_for(&self, kind: UploaderKind) -> UploadTarget {
        UploadTarget {
            kind,
            litterbox_retention: self.litterbox_retention,
            custom_url: self.uploader_url.clone(),
            credential: self.uploader_credentials.for_provider(kind).to_owned(),
            header_name: self.upload_header_name.clone(),
            header_value: self.upload_header_value.clone(),
            header_endpoint: self.upload_header_endpoint.clone().unwrap_or_default(),
        }
    }

    fn write_preferences(&self, storage: &impl Storage) -> Result<()> {
        let contents = serde_json::to_vec_pretty(self).context("failed to encode configuration")?;

        storage
            .write(CONFIG_KEY, &contents)
            .context("failed to save configuration")
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            settings_mode: SettingsMode::Simple,
            uploader_kind: UploaderKind::Uguu,
            uploader_url: String::new(),
            uploader_credentials: ProviderCredentials::default(),
            upload_header_name: String::new(),
            upload_header_value: String::new(),
            upload_header_endpoint: Some(String::new()),
            lifetime_seconds: DEFAULT_LIFETIME_SECONDS,
            litterbox_retention: LitterboxRetention::default(),
            recording_fps: DEFAULT_RECORDING_FPS,
            recording_max_seconds: DEFAULT_RECORDING_SECONDS,
            recording_max_gib: DEFAULT_RECORDING_GIB,
            recording_format: RecordingFormat::default(),
            capture_directory: String::new(),
            capture_naming: CaptureNamingConfig::default(),
            shortcuts: ShortcutConfig::default(),
            behavior: BehaviorConfig::default(),
            privacy: PrivacyConfig::default(),
        }
    }
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppConfig")
            .field("settings_mode", &self.settings_mode)
            .field("uploader_kind", &self.uploader_kind)
            .field("uploader_url_configured", &!self.uploader_url.is_empty())
            .field(
                "imgur_credential_configured",
                &!self.uploader_credentials.imgur.is_empty(),
            )
            .field(
                "vgy_credential_configured",
                &!self.uploader_credentials.vgy.is_empty(),
            )
            .field(
                "sul_credential_configured",
                &!self.uploader_credentials.sul.is_empty(),
            )
            .field("upload_header_name", &self.upload_header_name)
            .field("upload_header_value", &"[redacted]")
            .field(
                "upload_header_endpoint_configured",
                &self
                    .upload_header_endpoint
                    .as_ref()
                    .is_some_and(|endpoint| !endpoint.is_empty()),
            )
            .field("lifetime_seconds", &self.lifetime_seconds)
            .field("litterbox_retention", &self.litterbox_retention)
            .field("recording_fps", &self.recording_fps)
            .field("recording_max_seconds", &self.recording_max_seconds)
            .field("recording_max_gib", &self.recording_max_gib)
            .field("recording_format", &self.recording_format)
            .field("capture_directory", &self.capture_directory)
            .field("capture_naming", &self.capture_naming)
            .field("shortcuts", &self.shortcuts)
            .field("behavior", &self.behavior)
            .field("privacy", &self.privacy)
            .finish()
    }
}

/// Human-readable requested lifetime for status text.
#[must_use]
pub fn display_lifetime(seconds: u32) -> String {
    if seconds == 0 {
        return "No expiry requested".to_owned();
    }

    if seconds.is_multiple_of(3_600) {
        return match seconds / 3_600 {
            1 => "1 hour".to_owned(),
            24 => "1 day".to_owned(),
            168 => "1 week".to_owned(),
            hours => format!("{hours} hours"),
        };
    }

    match seconds {
        86_400 => "1 day".to_owned(),
        MAX_LIFETIME_SECONDS => "1 week".to_owned(),
        _ => format!("{seconds} seconds"),
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
