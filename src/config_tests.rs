use super::*;

#[test]
fn legacy_settings_use_simple_view_without_resetting_advanced_values() {
    let config: AppConfig = serde_json::from_str(
        r#"{
            "recording_fps": 48,
            "recording_max_seconds": 900,
            "recording_max_gib": 3,
            "lifetime_seconds": 43200,
            "require_tor": true,
            "tray_click_action": "show",
            "capture_naming": {"mode": "custom", "custom_name": "project"}
        }"#,
    )
    .unwrap();

    assert_eq!(config.settings_mode, SettingsMode::Simple);
    assert_eq!(config.recording_fps, 48);
    assert_eq!(config.recording_max_seconds, 900);
    assert_eq!(config.recording_max_gib, 3);
    assert_eq!(config.lifetime_seconds, 43_200);
    assert!(config.privacy.require_tor);
    assert_eq!(config.behavior.tray_click_action, TrayClickAction::Show);
    assert_eq!(config.capture_naming.custom_name, "project");
}

#[test]
fn settings_mode_persists_without_changing_operational_preferences() {
    use crate::storage::PlatformStorage;

    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut config = AppConfig {
        recording_fps: 48,
        recording_max_seconds: 900,
        lifetime_seconds: 43_200,
        ..AppConfig::default()
    };

    config.privacy.require_tor = true;
    config.behavior.onboarding_complete = true;
    config.uploader_credentials.imgur = "existing-client-id".to_owned();
    let original = config.clone();

    config
        .save_settings_mode(&storage, SettingsMode::Advanced)
        .unwrap();
    let mut restored = AppConfig::load(&storage).unwrap();

    assert_eq!(restored, config);
    restored
        .save_settings_mode(&storage, SettingsMode::Simple)
        .unwrap();
    assert_eq!(AppConfig::load(&storage).unwrap(), original);
}

#[test]
fn settings_mode_can_change_while_saved_uploader_needs_setup() {
    use crate::storage::PlatformStorage;

    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut saved = AppConfig {
        uploader_kind: UploaderKind::Custom,
        ..AppConfig::default()
    };

    assert!(saved.upload_target().validate().is_err());
    saved
        .save_settings_mode(&storage, SettingsMode::Advanced)
        .unwrap();
    assert_eq!(AppConfig::load(&storage).unwrap(), saved);
    assert!(saved.upload_target().validate().is_err());
}

#[test]
fn failed_settings_mode_persistence_keeps_previous_mode() {
    struct ReadOnlyStorage;

    impl Storage for ReadOnlyStorage {
        fn read(&self, _name: &str) -> std::io::Result<Option<Vec<u8>>> {
            Ok(None)
        }

        fn write(&self, _name: &str, _contents: &[u8]) -> std::io::Result<()> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
    }

    let mut saved = AppConfig::default();
    let before = saved.clone();

    assert!(
        saved
            .save_settings_mode(&ReadOnlyStorage, SettingsMode::Advanced)
            .is_err()
    );
    assert_eq!(saved, before);
}

#[test]
fn provider_credentials_never_cross_provider_boundaries() {
    let config = AppConfig {
        uploader_credentials: ProviderCredentials {
            imgur: "imgur-secret".to_owned(),
            pixeldrain: "pixeldrain-secret".to_owned(),
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
        config
            .upload_target_for(UploaderKind::Pixeldrain)
            .credential,
        "pixeldrain-secret"
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
fn legacy_metadata_privacy_setting_is_still_readable() {
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

    let migrated = serde_json::from_str::<AppConfig>(r#"{"downscale_filter":"lanczos3"}"#).unwrap();

    assert_eq!(migrated.behavior.resize_quality, ResizeQuality::Sharp);
}

#[test]
fn new_configs_enable_uploads_and_sound_with_a_usable_provider() {
    let config = AppConfig::default();

    assert!(config.behavior.save_captures);
    assert!(config.behavior.auto_upload_captures);
    assert_eq!(config.uploader_kind, UploaderKind::Uguu);
    assert!(config.upload_target().validate().is_ok());
    assert!(config.behavior.completion_sound);
    assert!(config.behavior.copy_link_after_upload);
    assert!(!config.behavior.onboarding_complete);
    assert_eq!(config.recording_max_gib, crate::DEFAULT_RECORDING_GIB);
    assert_eq!(config.recording_format, RecordingFormat::Webp);
    assert!(config.shortcuts.clipboard.is_empty());
    assert_eq!(config.behavior.tray_click_action, TrayClickAction::Region);
    assert_eq!(config.capture_naming.mode, CaptureNameMode::ActiveWindow);
    assert!(config.capture_naming.custom_name.is_empty());
    assert_eq!(
        CaptureNameMode::from_index(2),
        Some(CaptureNameMode::Friendly)
    );
    assert_eq!(
        CaptureNameMode::from_index(3),
        Some(CaptureNameMode::Custom)
    );
}

#[test]
fn existing_configs_are_not_shown_the_first_run_setup_again() {
    use crate::storage::{PlatformStorage, Storage as _};

    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    storage
        .write(CONFIG_KEY, br#"{"completion_sound":false}"#)
        .unwrap();

    let config = AppConfig::load(&storage).unwrap();

    assert!(config.behavior.onboarding_complete);
    assert!(!config.behavior.completion_sound);
    assert!(!config.behavior.auto_upload_captures);
    assert_eq!(config.uploader_kind, UploaderKind::Custom);
    assert!(config.behavior.copy_link_after_upload);
}

#[test]
fn loading_saved_preferences_preserves_explicit_choices_and_credentials() {
    use crate::storage::PlatformStorage;

    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut saved = AppConfig::default();

    saved.behavior.auto_upload_captures = false;
    saved.behavior.completion_sound = false;
    saved.behavior.onboarding_complete = true;
    saved.uploader_kind = UploaderKind::Sul;
    saved.uploader_credentials.sul = "fake-sul-key".to_owned();
    saved.uploader_credentials.imgur = "fake-imgur-id".to_owned();
    saved.upload_header_name = "Authorization".to_owned();
    saved.upload_header_value = "fake-custom-key".to_owned();
    saved.uploader_url = "https://uploads.example.test/upload".to_owned();
    saved.upload_header_endpoint = Some(saved.uploader_url.clone());
    saved.save(&storage).unwrap();

    assert_eq!(AppConfig::load(&storage).unwrap(), saved);
}

#[test]
fn old_recording_limits_are_clamped_to_the_supported_container_limit() {
    use crate::storage::{PlatformStorage, Storage as _};

    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    storage
        .write(CONFIG_KEY, br#"{"recording_max_gib":16}"#)
        .unwrap();
    assert_eq!(
        AppConfig::load(&storage).unwrap().recording_max_gib,
        MAX_RECORDING_GIB
    );
    storage
        .write(CONFIG_KEY, br#"{"recording_max_gib":0}"#)
        .unwrap();
    assert_eq!(
        AppConfig::load(&storage).unwrap().recording_max_gib,
        DEFAULT_RECORDING_GIB
    );
}

#[test]
fn lifetimes_use_hours_and_days_in_status_text() {
    assert_eq!(display_lifetime(0), "No expiry requested");
    assert_eq!(display_lifetime(3_600), "1 hour");
    assert_eq!(display_lifetime(7_200), "2 hours");
    assert_eq!(display_lifetime(86_400), "1 day");
    assert_eq!(display_lifetime(172_800), "48 hours");
    assert_eq!(display_lifetime(604_800), "1 week");
    assert_eq!(display_lifetime(90), "90 seconds");
}

#[test]
fn tray_click_actions_round_trip_selector_indices() {
    let actions = [
        TrayClickAction::Region,
        TrayClickAction::Show,
        TrayClickAction::Screenshot,
        TrayClickAction::Recording,
        TrayClickAction::Clipboard,
        TrayClickAction::None,
    ];

    for action in actions {
        assert_eq!(TrayClickAction::from_index(action.index()), Some(action));
    }

    assert_eq!(TrayClickAction::from_index(6), None);
    assert_eq!(
        serde_json::to_string(&TrayClickAction::None).unwrap(),
        r#""none""#
    );
    assert_eq!(
        serde_json::from_str::<TrayClickAction>(r#""none""#).unwrap(),
        TrayClickAction::None
    );
}
