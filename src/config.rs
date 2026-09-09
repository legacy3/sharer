//! Persistent desktop preferences.

use std::fmt;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    DEFAULT_RECORDING_SECONDS, MAX_LIFETIME_SECONDS,
    storage::Storage,
    upload::{UploadTarget, UploaderKind},
    validate_lifetime, validate_recording_fps, validate_recording_seconds,
};

const DEFAULT_LIFETIME_SECONDS: u32 = 24 * 60 * 60;
const DEFAULT_RECORDING_FPS: u32 = 30;
const CONFIG_KEY: &str = "application";

/// User preferences stored in the platform configuration directory.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub uploader_kind: UploaderKind,
    pub uploader_url: String,
    pub uploader_credentials: ProviderCredentials,
    pub upload_header_name: String,
    pub upload_header_value: String,
    pub lifetime_seconds: u32,
    pub recording_fps: u32,
    pub recording_max_seconds: u32,
    pub capture_directory: String,
    pub capture_naming: CaptureNamingConfig,
    pub shortcuts: ShortcutConfig,
    #[serde(flatten)]
    pub behavior: BehaviorConfig,
    #[serde(flatten)]
    pub privacy: PrivacyConfig,
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
    pub force_sdr_captures: bool,
    pub capture_resolution: CaptureResolution,
    #[serde(alias = "downscale_filter")]
    pub resize_quality: ResizeQuality,
    pub minimize_to_tray: bool,
    pub start_at_login: bool,
    pub completion_sound: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            save_captures: true,
            force_sdr_captures: false,
            capture_resolution: CaptureResolution::Native,
            resize_quality: ResizeQuality::Fast,
            minimize_to_tray: true,
            start_at_login: false,
            completion_sound: false,
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

/// Privacy controls for capture names, image metadata, and network routing.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PrivacyConfig {
    pub require_tor: bool,
    #[serde(alias = "strip_capture_metadata")]
    pub private_capture_names: bool,
    pub remove_exif: bool,
}

/// Credentials isolated by provider so switching uploaders cannot reuse another service's secret.
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ProviderCredentials {
    pub imgur: String,
    pub vgy: String,
    pub sul: String,
}

impl ProviderCredentials {
    /// Return the credential belonging to an uploader.
    #[must_use]
    pub fn for_provider(&self, kind: UploaderKind) -> &str {
        match kind {
            UploaderKind::Imgur => &self.imgur,
            UploaderKind::Vgy => &self.vgy,
            UploaderKind::Sul => &self.sul,
            UploaderKind::Custom | UploaderKind::Uguu | UploaderKind::TransferSh => "",
        }
    }

    /// Replace only the credential belonging to an uploader.
    pub fn set_for_provider(&mut self, kind: UploaderKind, credential: String) {
        match kind {
            UploaderKind::Imgur => self.imgur = credential,
            UploaderKind::Vgy => self.vgy = credential,
            UploaderKind::Sul => self.sul = credential,
            UploaderKind::Custom | UploaderKind::Uguu | UploaderKind::TransferSh => {}
        }
    }
}

impl fmt::Debug for ProviderCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentials")
            .field("imgur_configured", &!self.imgur.is_empty())
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
    Custom,
}

impl CaptureNameMode {
    /// Convert a desktop selector index to a naming mode.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::ActiveWindow),
            1 => Some(Self::Random),
            2 => Some(Self::Custom),
            _ => None,
        }
    }

    /// Convert the mode to the desktop selector index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::ActiveWindow => 0,
            Self::Random => 1,
            Self::Custom => 2,
        }
    }
}

impl Default for CaptureNamingConfig {
    fn default() -> Self {
        Self {
            mode: CaptureNameMode::ActiveWindow,
            custom_name: "screenshot".to_owned(),
        }
    }
}

/// User-editable global shortcut strings understood by `global-hotkey`.
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
            clipboard: "control+shift+V".to_owned(),
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
        let mut config =
            serde_json::from_slice::<Self>(&contents).context("failed to decode configuration")?;

        if validate_lifetime(config.lifetime_seconds).is_err() {
            config.lifetime_seconds = DEFAULT_LIFETIME_SECONDS;
        }

        if validate_recording_fps(config.recording_fps).is_err() {
            config.recording_fps = DEFAULT_RECORDING_FPS;
        }

        if validate_recording_seconds(config.recording_max_seconds).is_err() {
            config.recording_max_seconds = DEFAULT_RECORDING_SECONDS;
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
        self.upload_target().validate()?;
        let contents = serde_json::to_vec_pretty(self).context("failed to encode configuration")?;

        storage
            .write(CONFIG_KEY, &contents)
            .context("failed to save configuration")
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
            custom_url: self.uploader_url.clone(),
            credential: self.uploader_credentials.for_provider(kind).to_owned(),
            header_name: self.upload_header_name.clone(),
            header_value: self.upload_header_value.clone(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            uploader_kind: UploaderKind::Custom,
            uploader_url: String::new(),
            uploader_credentials: ProviderCredentials::default(),
            upload_header_name: String::new(),
            upload_header_value: String::new(),
            lifetime_seconds: DEFAULT_LIFETIME_SECONDS,
            recording_fps: DEFAULT_RECORDING_FPS,
            recording_max_seconds: DEFAULT_RECORDING_SECONDS,
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
            .field("lifetime_seconds", &self.lifetime_seconds)
            .field("recording_fps", &self.recording_fps)
            .field("recording_max_seconds", &self.recording_max_seconds)
            .field("capture_directory", &self.capture_directory)
            .field("capture_naming", &self.capture_naming)
            .field("shortcuts", &self.shortcuts)
            .field("behavior", &self.behavior)
            .field("privacy", &self.privacy)
            .finish()
    }
}

/// Human-readable bounded lifetime for status text.
#[must_use]
pub fn display_lifetime(seconds: u32) -> String {
    match seconds {
        3_600 => "1 hour".to_owned(),
        86_400 => "1 day".to_owned(),
        MAX_LIFETIME_SECONDS => "1 week".to_owned(),
        _ => format!("{seconds} seconds"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_credentials_never_cross_provider_boundaries() {
        let config = AppConfig {
            uploader_credentials: ProviderCredentials {
                imgur: "imgur-secret".to_owned(),
                vgy: "vgy-secret".to_owned(),
                sul: "sul-secret".to_owned(),
            },
            ..AppConfig::default()
        };

        assert_eq!(
            config.upload_target_for(UploaderKind::Imgur).credential,
            "imgur-secret"
        );
        assert_eq!(
            config.upload_target_for(UploaderKind::Vgy).credential,
            "vgy-secret"
        );
        assert_eq!(
            config.upload_target_for(UploaderKind::Sul).credential,
            "sul-secret"
        );
        assert!(
            config
                .upload_target_for(UploaderKind::Custom)
                .credential
                .is_empty()
        );
        assert!(
            config
                .upload_target_for(UploaderKind::Uguu)
                .credential
                .is_empty()
        );
    }

    #[test]
    fn legacy_metadata_privacy_setting_migrates_to_private_names() {
        let privacy =
            serde_json::from_str::<PrivacyConfig>(r#"{"strip_capture_metadata":true}"#).unwrap();

        assert!(privacy.private_capture_names);
        assert!(!privacy.remove_exif);
    }

    #[test]
    fn existing_configs_default_to_native_capture_resolution() {
        let config = serde_json::from_str::<AppConfig>("{}").unwrap();

        assert_eq!(
            config.behavior.capture_resolution,
            CaptureResolution::Native
        );
        assert_eq!(config.behavior.resize_quality, ResizeQuality::Fast);
        assert_eq!(
            CaptureResolution::from_index(0),
            Some(CaptureResolution::Native)
        );
        assert_eq!(
            CaptureResolution::from_index(1),
            Some(CaptureResolution::Logical)
        );
        assert_eq!(CaptureResolution::from_index(2), None);
        assert_eq!(ResizeQuality::from_index(0), Some(ResizeQuality::Fast));
        assert_eq!(ResizeQuality::from_index(1), Some(ResizeQuality::Balanced));
        assert_eq!(ResizeQuality::from_index(2), Some(ResizeQuality::Sharp));
        assert_eq!(ResizeQuality::from_index(3), None);

        let migrated =
            serde_json::from_str::<AppConfig>(r#"{"downscale_filter":"lanczos3"}"#).unwrap();

        assert_eq!(migrated.behavior.resize_quality, ResizeQuality::Sharp);
    }
}
