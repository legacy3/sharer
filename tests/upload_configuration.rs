//! Saved upload settings remain compatible as providers and expiry options evolve.

use sharer::{
    MAX_LIFETIME_SECONDS,
    config::{AppConfig, ProviderCredentials},
    storage::{PlatformStorage, Storage as _},
    upload::{LitterboxRetention, UploaderKind},
};

#[test]
fn new_provider_credentials_default_empty_and_stay_redacted() {
    let mut credentials: ProviderCredentials = serde_json::from_str(
        r#"{"imgur":"imgur-secret","vgy":"legacy-secret","sul":"sul-secret"}"#,
    )
    .unwrap();

    assert!(credentials.pixeldrain.is_empty());
    credentials.set_for_provider(UploaderKind::Pixeldrain, "pixel-secret".to_owned());
    assert_eq!(
        credentials.for_provider(UploaderKind::Pixeldrain),
        "pixel-secret"
    );
    assert_eq!(
        credentials.for_provider(UploaderKind::Imgur),
        "imgur-secret"
    );

    for kind in [
        UploaderKind::Custom,
        UploaderKind::Uguu,
        UploaderKind::Catbox,
        UploaderKind::Litterbox,
        UploaderKind::TransferSh,
    ] {
        credentials.set_for_provider(kind, "must-not-leak".to_owned());
        assert!(credentials.for_provider(kind).is_empty());
    }

    let debug = format!("{credentials:?}");

    assert!(!debug.contains("secret"));
    assert!(debug.contains("pixeldrain_configured: true"));
}

#[test]
fn saved_lifetimes_preserve_no_expiry_and_exact_seconds() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    for lifetime_seconds in [
        0,
        1,
        90,
        5_400,
        AppConfig::default().lifetime_seconds,
        MAX_LIFETIME_SECONDS,
    ] {
        let saved = AppConfig {
            lifetime_seconds,
            ..AppConfig::default()
        };

        saved.save(&storage).unwrap();
        assert_eq!(
            AppConfig::load(&storage).unwrap().lifetime_seconds,
            lifetime_seconds
        );
    }
}

#[test]
fn litterbox_defaults_preserve_existing_custom_lifetimes() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    for seconds in [0, 90, 604_800] {
        let legacy = format!(r#"{{"uploader_kind":"litterbox","lifetime_seconds":{seconds}}}"#);

        storage.write("application", legacy.as_bytes()).unwrap();
        let loaded = AppConfig::load(&storage).unwrap();

        assert_eq!(
            loaded.litterbox_retention,
            LitterboxRetention::SeventyTwoHours
        );
        assert_eq!(loaded.lifetime_seconds, seconds);
        assert_eq!(
            loaded.upload_target().litterbox_retention,
            LitterboxRetention::SeventyTwoHours
        );
    }
}

#[test]
fn litterbox_retention_round_trips_independently_of_custom_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    for retention in [
        LitterboxRetention::OneHour,
        LitterboxRetention::TwelveHours,
        LitterboxRetention::TwentyFourHours,
        LitterboxRetention::SeventyTwoHours,
    ] {
        for seconds in [0, 90, 5_400] {
            let saved = AppConfig {
                uploader_kind: UploaderKind::Litterbox,
                litterbox_retention: retention,
                lifetime_seconds: seconds,
                ..AppConfig::default()
            };

            saved.save(&storage).unwrap();
            let loaded = AppConfig::load(&storage).unwrap();

            assert_eq!(loaded, saved);
            assert_eq!(loaded.upload_target().litterbox_retention, retention);
            assert_eq!(
                loaded
                    .upload_target_for(UploaderKind::Custom)
                    .litterbox_retention,
                retention
            );
        }
    }
}

#[test]
fn unsupported_litterbox_retention_does_not_silently_change_the_request() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let unsupported = br#"{"litterbox_retention":"unlimited"}"#;

    storage.write("application", unsupported).unwrap();
    assert!(AppConfig::load(&storage).is_err());
    assert_eq!(storage.read("application").unwrap().unwrap(), unsupported);
}
