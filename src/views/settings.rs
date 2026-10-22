//! Settings-view initialization, validation, and folder actions.

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use anyhow::{Context as _, Result};
use slint::ComponentHandle as _;

use crate::AppWindow;
use sharer::{
    config::{
        AppConfig, BehaviorConfig, CaptureNameMode, CaptureNamingConfig, CaptureResolution,
        PrivacyConfig, ProviderCredentials, RecordingFormat, ResizeQuality, ShortcutConfig,
        TrayClickAction,
    },
    storage::{
        capture_directory_needs_default, capture_directory_or_default, default_capture_directory,
        ensure_capture_directory,
    },
    upload::{LitterboxRetention, UploadTarget, UploaderKind, canonical_uploader_endpoint},
    validate_lifetime, validate_recording_fps, validate_recording_gib, validate_recording_seconds,
};

pub(crate) fn initialize(window: &AppWindow, config: &AppConfig) {
    window.set_provider_names(slint::ModelRc::new(slint::VecModel::from(
        UploaderKind::AVAILABLE
            .iter()
            .map(|kind| slint::SharedString::from(kind.name()))
            .collect::<Vec<_>>(),
    )));
    window.set_provider_limits(slint::ModelRc::new(slint::VecModel::from(
        (0..=8)
            .filter_map(UploaderKind::from_index)
            .map(|kind| slint::SharedString::from(kind.limits_description()))
            .collect::<Vec<_>>(),
    )));
    window.set_lifetime_seconds(config.lifetime_seconds.cast_signed());
    window.set_litterbox_retention(config.litterbox_retention.index());
    window.set_recording_fps(config.recording_fps.cast_signed());
    window.set_recording_max_seconds(config.recording_max_seconds.cast_signed());
    window.set_recording_max_gib(config.recording_max_gib.cast_signed());
    window.set_recording_mp4(config.recording_format == RecordingFormat::Mp4);
    window.set_save_captures(config.behavior.save_captures);
    window.set_auto_upload_captures(config.behavior.auto_upload_captures);
    window.set_force_sdr_captures(config.behavior.force_sdr_captures);
    window.set_capture_resolution(config.behavior.capture_resolution.index());
    window.set_resize_quality(config.behavior.resize_quality.index());
    window.set_retina_scaling_available(cfg!(target_os = "macos"));
    window.set_remove_exif(config.privacy.remove_exif);
    window.set_capture_directory(config.capture_directory.clone().into());
    window.set_uploader_kind(config.uploader_kind.index());
    window.set_uploader_name(config.uploader_kind.name().into());
    window.set_uploader_url(config.uploader_url.clone().into());
    window.set_imgur_credential(config.uploader_credentials.imgur.clone().into());
    window.set_vgy_credential(config.uploader_credentials.vgy.clone().into());
    window.set_sul_credential(config.uploader_credentials.sul.clone().into());
    window.set_pixeldrain_credential(config.uploader_credentials.pixeldrain.clone().into());
    window.set_upload_header_name(config.upload_header_name.clone().into());
    window.set_upload_header_value(config.upload_header_value.clone().into());
    window.set_upload_header_endpoint(
        config
            .upload_header_endpoint
            .clone()
            .unwrap_or_default()
            .into(),
    );
    window.set_minimize_to_tray(config.behavior.minimize_to_tray);
    window.set_start_at_login(config.behavior.start_at_login);
    window.set_capture_name_mode(config.capture_naming.mode.index());
    window.set_capture_custom_name(config.capture_naming.custom_name.clone().into());
    window.set_require_tor(config.privacy.require_tor);
    window.set_completion_sound(config.behavior.completion_sound);
    window.set_copy_link_after_upload(config.behavior.copy_link_after_upload);
    window.set_tray_click_action(config.behavior.tray_click_action.index());
    window.set_region_hotkey(config.shortcuts.region.clone().into());
    window.set_recording_hotkey(config.shortcuts.recording.clone().into());
    window.set_screen_hotkey(config.shortcuts.screen.clone().into());
    window.set_clipboard_hotkey(config.shortcuts.clipboard.clone().into());
    set_shortcut_labels(window, &config.shortcuts);
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    window.set_onboarding_complete(config.behavior.onboarding_complete);
    initialize_wizard(window, config, false);
    window.set_show_onboarding(!config.behavior.onboarding_complete);

    if !config.behavior.onboarding_complete {
        window.set_status_text("Welcome to ShareR".into());
        window.set_status_detail("Choose where uploads go. You can change this later.".into());
    } else if !config.behavior.auto_upload_captures {
        window.set_status_text("Ready".into());
        window.set_status_detail("Captures are saved locally and are not uploaded".into());
    } else if config.upload_target().validate().is_err() {
        window.set_current_page(2);
        window.set_status_text("Setup required".into());
        window.set_status_detail("Configure an uploader or disable automatic uploads".into());
    } else {
        window.set_status_text("Ready".into());
        let retention = if config.uploader_kind.supports_lifetime() {
            if config.lifetime_seconds == 0 {
                "No expiry requested; server retention applies".to_owned()
            } else {
                format!(
                    "Requested expiry after {}",
                    sharer::config::display_lifetime(config.lifetime_seconds)
                )
            }
        } else if config.uploader_kind == UploaderKind::Litterbox {
            config.litterbox_retention.description().to_owned()
        } else {
            format!("Retention managed by {}", config.uploader_kind.name())
        };

        window.set_status_detail(
            format!(
                "{}  -  {}",
                retention,
                if config.behavior.copy_link_after_upload {
                    "links are copied automatically"
                } else {
                    "copy the link from the last result"
                }
            )
            .into(),
        );
    }

    window.set_settings_dirty(false);
}

pub(crate) fn initialize_wizard(window: &AppWindow, config: &AppConfig, reset: bool) {
    window.set_onboarding_reset(reset);
    window.set_onboarding_step(0);
    window.set_onboarding_error("".into());
    window.set_wizard_provider(if config.behavior.auto_upload_captures {
        config.uploader_kind.index()
    } else {
        -1
    });
    window.set_wizard_auto_upload(config.behavior.auto_upload_captures);
    window.set_wizard_sound(config.behavior.completion_sound);
    window.set_wizard_start_at_login(config.behavior.start_at_login);
    window.set_wizard_imgur_credential(config.uploader_credentials.imgur.clone().into());
    window.set_wizard_sul_credential(config.uploader_credentials.sul.clone().into());
    window.set_wizard_pixeldrain_credential(config.uploader_credentials.pixeldrain.clone().into());
    window.set_wizard_uploader_url(config.uploader_url.clone().into());
    window.set_wizard_header_name(config.upload_header_name.clone().into());
    window.set_wizard_header_value(config.upload_header_value.clone().into());
    window.set_wizard_header_endpoint(
        config
            .upload_header_endpoint
            .clone()
            .unwrap_or_default()
            .into(),
    );
    window.set_wizard_lifetime_seconds(config.lifetime_seconds.cast_signed());
    window.set_wizard_litterbox_retention(config.litterbox_retention.index());
    window.set_wizard_save_captures(config.behavior.save_captures);
    window.set_wizard_capture_directory(config.capture_directory.clone().into());
}

pub(crate) fn reset_preferences(config: &AppConfig) -> AppConfig {
    AppConfig {
        uploader_kind: config.uploader_kind,
        uploader_url: config.uploader_url.clone(),
        uploader_credentials: config.uploader_credentials.clone(),
        upload_header_name: config.upload_header_name.clone(),
        upload_header_value: config.upload_header_value.clone(),
        upload_header_endpoint: config.upload_header_endpoint.clone(),
        capture_directory: config.capture_directory.clone(),
        ..AppConfig::default()
    }
}

pub(crate) fn wizard_config_from_window(
    window: &AppWindow,
    saved: &AppConfig,
) -> Result<AppConfig> {
    let mut next = if window.get_onboarding_reset() {
        reset_preferences(saved)
    } else {
        saved.clone()
    };
    let provider = window.get_wizard_provider();

    if provider >= 0 {
        next.uploader_kind = UploaderKind::from_index(provider).context("choose an uploader")?;

        match next.uploader_kind {
            UploaderKind::Custom => {
                window
                    .get_wizard_uploader_url()
                    .trim()
                    .clone_into(&mut next.uploader_url);
                window
                    .get_wizard_header_name()
                    .trim()
                    .clone_into(&mut next.upload_header_name);
                window
                    .get_wizard_header_value()
                    .as_str()
                    .clone_into(&mut next.upload_header_value);
                next.upload_header_endpoint = Some(window.get_wizard_header_endpoint().to_string());
                next.lifetime_seconds = u32::try_from(window.get_wizard_lifetime_seconds())
                    .context("link lifetime must be zero or a positive number of seconds")?;
                validate_lifetime(next.lifetime_seconds)?;
            }

            UploaderKind::Litterbox => {
                next.litterbox_retention =
                    LitterboxRetention::from_index(window.get_wizard_litterbox_retention())
                        .context("choose a supported Litterbox retention")?;
            }

            UploaderKind::Imgur => {
                window
                    .get_wizard_imgur_credential()
                    .trim()
                    .clone_into(&mut next.uploader_credentials.imgur);
            }

            UploaderKind::Pixeldrain => {
                window
                    .get_wizard_pixeldrain_credential()
                    .trim()
                    .clone_into(&mut next.uploader_credentials.pixeldrain);
            }

            UploaderKind::Sul => {
                window
                    .get_wizard_sul_credential()
                    .trim()
                    .clone_into(&mut next.uploader_credentials.sul);
            }

            UploaderKind::Uguu
            | UploaderKind::Catbox
            | UploaderKind::Vgy
            | UploaderKind::TransferSh => {}
        }
    }

    next.behavior.auto_upload_captures = provider >= 0 && window.get_wizard_auto_upload();
    next.behavior.save_captures |= !next.behavior.auto_upload_captures;
    next.behavior.completion_sound = window.get_wizard_sound();
    next.behavior.start_at_login = window.get_wizard_start_at_login();
    next.behavior.onboarding_complete = true;
    ensure_default_capture_directory(&mut next)?;

    if provider >= 0 {
        next.upload_target().validate()?;
    }

    Ok(next)
}

pub(crate) fn set_shortcut_labels(window: &AppWindow, shortcuts: &ShortcutConfig) {
    window
        .set_region_hotkey_label(sharer::presentation::display_shortcut(&shortcuts.region).into());
    window.set_recording_hotkey_label(
        sharer::presentation::display_shortcut(&shortcuts.recording).into(),
    );
    window
        .set_screen_hotkey_label(sharer::presentation::display_shortcut(&shortcuts.screen).into());
    window.set_clipboard_hotkey_label(
        sharer::presentation::display_shortcut(&shortcuts.clipboard).into(),
    );
}

pub(crate) fn wire_folder_callbacks(window: &AppWindow) {
    let window_weak = window.as_weak();

    window.on_choose_capture_folder(move || {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        let current = PathBuf::from(window.get_capture_directory().as_str());
        let mut dialog = rfd::FileDialog::new();

        if !current.as_os_str().is_empty() {
            dialog = dialog.set_directory(current);
        }

        if let Some(path) = dialog.pick_folder() {
            window.set_capture_directory(path.to_string_lossy().into_owned().into());
        }
    });

    let window_weak = window.as_weak();

    window.on_open_capture_folder(move || {
        if let Some(window) = window_weak.upgrade()
            && let Err(error) = open_capture_folder(&window)
        {
            window.set_status_text("Could not open folder".into());
            window.set_status_detail(format!("{error:#}").into());
        }
    });
}

pub(crate) fn wire_dirty_callback(window: &AppWindow, saved_config: &Rc<RefCell<AppConfig>>) {
    let window_weak = window.as_weak();

    window.on_custom_endpoint_edited(move || {
        if let Some(window) = window_weak.upgrade() {
            let endpoint = canonical_uploader_endpoint(window.get_uploader_url().as_str());

            if endpoint.as_deref().ok() != Some(window.get_upload_header_endpoint().as_str()) {
                window.set_upload_header_name("".into());
                window.set_upload_header_value("".into());
                window.set_upload_header_endpoint("".into());
            }
        }
    });

    let window_weak = window.as_weak();

    window.on_custom_header_edited(move || {
        if let Some(window) = window_weak.upgrade() {
            window.set_upload_header_endpoint(
                canonical_uploader_endpoint(window.get_uploader_url().as_str())
                    .unwrap_or_default()
                    .into(),
            );
        }
    });

    let window_weak = window.as_weak();

    window.on_wizard_endpoint_edited(move || {
        if let Some(window) = window_weak.upgrade() {
            let endpoint = canonical_uploader_endpoint(window.get_wizard_uploader_url().as_str());

            if endpoint.as_deref().ok() != Some(window.get_wizard_header_endpoint().as_str()) {
                window.set_wizard_header_name("".into());
                window.set_wizard_header_value("".into());
                window.set_wizard_header_endpoint("".into());
            }
        }
    });

    let window_weak = window.as_weak();

    window.on_wizard_header_edited(move || {
        if let Some(window) = window_weak.upgrade() {
            window.set_wizard_header_endpoint(
                canonical_uploader_endpoint(window.get_wizard_uploader_url().as_str())
                    .unwrap_or_default()
                    .into(),
            );
        }
    });

    let window_weak = window.as_weak();
    let saved_config = Rc::clone(saved_config);

    window.on_settings_edited(move || {
        if let Some(window) = window_weak.upgrade() {
            window.set_settings_dirty(settings_are_dirty(&window, &saved_config.borrow()));
        }
    });
}

fn settings_are_dirty(window: &AppWindow, saved: &AppConfig) -> bool {
    upload_settings_are_dirty(window, saved)
        || capture_settings_are_dirty(window, saved)
        || privacy_settings_are_dirty(window, saved)
        || general_settings_are_dirty(window, saved)
}

fn upload_settings_are_dirty(window: &AppWindow, saved: &AppConfig) -> bool {
    window.get_uploader_kind() != saved.uploader_kind.index()
        || window.get_uploader_url().trim() != saved.uploader_url
        || window.get_imgur_credential().trim() != saved.uploader_credentials.imgur
        || window.get_vgy_credential().trim() != saved.uploader_credentials.vgy
        || window.get_sul_credential().trim() != saved.uploader_credentials.sul
        || window.get_pixeldrain_credential().trim() != saved.uploader_credentials.pixeldrain
        || window.get_upload_header_name().trim() != saved.upload_header_name
        || window.get_upload_header_value().as_str() != saved.upload_header_value
        || window.get_upload_header_endpoint().as_str()
            != saved.upload_header_endpoint.as_deref().unwrap_or_default()
        || window.get_lifetime_seconds() != saved.lifetime_seconds.cast_signed()
        || window.get_litterbox_retention() != saved.litterbox_retention.index()
}

fn capture_settings_are_dirty(window: &AppWindow, saved: &AppConfig) -> bool {
    window.get_recording_fps() != saved.recording_fps.cast_signed()
        || window.get_recording_max_seconds() != saved.recording_max_seconds.cast_signed()
        || window.get_recording_max_gib() != saved.recording_max_gib.cast_signed()
        || recording_format(window) != saved.recording_format
        || window.get_save_captures() != saved.behavior.save_captures
        || window.get_auto_upload_captures() != saved.behavior.auto_upload_captures
        || window.get_force_sdr_captures() != saved.behavior.force_sdr_captures
        || window.get_capture_resolution() != saved.behavior.capture_resolution.index()
        || window.get_resize_quality() != saved.behavior.resize_quality.index()
        || window.get_capture_directory().trim() != saved.capture_directory
        || window.get_capture_name_mode() != saved.capture_naming.mode.index()
        || window.get_capture_custom_name().trim() != saved.capture_naming.custom_name
}

fn privacy_settings_are_dirty(window: &AppWindow, saved: &AppConfig) -> bool {
    window.get_remove_exif() != saved.privacy.remove_exif
        || window.get_require_tor() != saved.privacy.require_tor
}

fn general_settings_are_dirty(window: &AppWindow, saved: &AppConfig) -> bool {
    window.get_minimize_to_tray() != saved.behavior.minimize_to_tray
        || window.get_start_at_login() != saved.behavior.start_at_login
        || window.get_completion_sound() != saved.behavior.completion_sound
        || window.get_copy_link_after_upload() != saved.behavior.copy_link_after_upload
        || window.get_tray_click_action() != saved.behavior.tray_click_action.index()
        || window.get_region_hotkey().as_str() != saved.shortcuts.region
        || window.get_recording_hotkey().as_str() != saved.shortcuts.recording
        || window.get_screen_hotkey().as_str() != saved.shortcuts.screen
        || window.get_clipboard_hotkey().as_str() != saved.shortcuts.clipboard
}

pub(crate) fn config_from_window(window: &AppWindow) -> Result<AppConfig> {
    let lifetime_seconds = u32::try_from(window.get_lifetime_seconds())
        .context("link lifetime must be zero or a positive number of seconds")?;

    validate_lifetime(lifetime_seconds)?;
    let uploader = upload_target_values_from_window(window)?;
    let shortcuts = ShortcutConfig {
        region: window.get_region_hotkey().trim().to_owned(),
        recording: window.get_recording_hotkey().trim().to_owned(),
        screen: window.get_screen_hotkey().trim().to_owned(),
        clipboard: window.get_clipboard_hotkey().trim().to_owned(),
    };

    crate::hotkeys::validate_shortcuts(&shortcuts)?;
    let capture_naming = naming_from_window(window)?;
    let capture_resolution = CaptureResolution::from_index(window.get_capture_resolution())
        .context("invalid capture resolution")?;
    let resize_quality =
        ResizeQuality::from_index(window.get_resize_quality()).context("invalid resize quality")?;
    let save_captures = window.get_save_captures();
    let auto_upload_captures = window.get_auto_upload_captures();
    let capture_directory = capture_directory_or_default(window.get_capture_directory().as_str())?
        .to_string_lossy()
        .into_owned();

    anyhow::ensure!(
        save_captures || auto_upload_captures,
        "enable local capture copies or automatic capture uploads"
    );

    if auto_upload_captures {
        uploader.validate()?;
    }

    let tray_click_action = TrayClickAction::from_index(window.get_tray_click_action())
        .context("invalid tray click action")?;

    Ok(AppConfig {
        uploader_kind: uploader.kind,
        uploader_url: uploader.custom_url,
        uploader_credentials: ProviderCredentials {
            imgur: window.get_imgur_credential().trim().to_owned(),
            vgy: window.get_vgy_credential().trim().to_owned(),
            sul: window.get_sul_credential().trim().to_owned(),
            pixeldrain: window.get_pixeldrain_credential().trim().to_owned(),
        },
        upload_header_name: uploader.header_name,
        upload_header_value: uploader.header_value,
        upload_header_endpoint: Some(uploader.header_endpoint),
        lifetime_seconds,
        litterbox_retention: uploader.litterbox_retention,
        recording_fps: validated_recording_fps(window)?,
        recording_max_seconds: validated_recording_seconds(window)?,
        recording_max_gib: validated_recording_gib(window)?,
        recording_format: recording_format(window),
        capture_directory,
        capture_naming,
        shortcuts,
        behavior: BehaviorConfig {
            save_captures,
            auto_upload_captures,
            force_sdr_captures: window.get_force_sdr_captures(),
            capture_resolution,
            resize_quality,
            minimize_to_tray: window.get_minimize_to_tray(),
            start_at_login: window.get_start_at_login(),
            completion_sound: window.get_completion_sound(),
            copy_link_after_upload: window.get_copy_link_after_upload(),
            onboarding_complete: window.get_onboarding_complete(),
            tray_click_action,
        },
        privacy: PrivacyConfig {
            require_tor: window.get_require_tor(),
            private_capture_names: false,
            remove_exif: window.get_remove_exif(),
        },
    })
}

pub(crate) fn upload_target_from_window(window: &AppWindow) -> Result<UploadTarget> {
    let target = upload_target_values_from_window(window)?;

    target.validate()?;
    Ok(target)
}

fn upload_target_values_from_window(window: &AppWindow) -> Result<UploadTarget> {
    let kind = UploaderKind::from_index(window.get_uploader_kind())
        .context("invalid uploader provider")?;
    let target = UploadTarget {
        kind,
        litterbox_retention: LitterboxRetention::from_index(window.get_litterbox_retention())
            .context("choose a supported Litterbox retention")?,
        custom_url: window.get_uploader_url().trim().to_owned(),
        credential: match kind {
            UploaderKind::Imgur => window.get_imgur_credential(),
            UploaderKind::Vgy => window.get_vgy_credential(),
            UploaderKind::Sul => window.get_sul_credential(),
            UploaderKind::Pixeldrain => window.get_pixeldrain_credential(),
            UploaderKind::Custom
            | UploaderKind::Uguu
            | UploaderKind::Catbox
            | UploaderKind::Litterbox
            | UploaderKind::TransferSh => "".into(),
        }
        .trim()
        .to_owned(),
        header_name: window.get_upload_header_name().trim().to_owned(),
        header_value: window.get_upload_header_value().to_string(),
        header_endpoint: window.get_upload_header_endpoint().to_string(),
    };

    Ok(target)
}

pub(crate) fn naming_from_window(window: &AppWindow) -> Result<CaptureNamingConfig> {
    let mode = CaptureNameMode::from_index(window.get_capture_name_mode())
        .context("invalid screenshot naming mode")?;
    let custom_name = window.get_capture_custom_name().trim().to_owned();

    anyhow::ensure!(
        mode != CaptureNameMode::Custom || !custom_name.is_empty(),
        "custom capture name cannot be empty"
    );

    Ok(CaptureNamingConfig { mode, custom_name })
}

pub(crate) fn validated_recording_fps(window: &AppWindow) -> Result<u32> {
    let frames_per_second = u32::try_from(window.get_recording_fps())
        .context("recording frame rate must be positive")?;

    validate_recording_fps(frames_per_second)?;

    Ok(frames_per_second)
}

pub(crate) fn validated_recording_seconds(window: &AppWindow) -> Result<u32> {
    let seconds = u32::try_from(window.get_recording_max_seconds())
        .context("recording limit must be positive")?;

    validate_recording_seconds(seconds)?;
    Ok(seconds)
}

pub(crate) fn recording_format(window: &AppWindow) -> RecordingFormat {
    if window.get_recording_mp4() {
        RecordingFormat::Mp4
    } else {
        RecordingFormat::Webp
    }
}

pub(crate) fn validated_recording_gib(window: &AppWindow) -> Result<u32> {
    let gib = u32::try_from(window.get_recording_max_gib())
        .context("recording size limit must be positive")?;

    validate_recording_gib(gib)?;
    Ok(gib)
}

pub(crate) fn configured_capture_directory(window: &AppWindow) -> Result<Option<PathBuf>> {
    if window.get_save_captures() {
        Ok(Some(capture_directory_or_default(
            window.get_capture_directory().as_str(),
        )?))
    } else {
        Ok(None)
    }
}

pub(crate) fn open_capture_folder(window: &AppWindow) -> Result<()> {
    let directory = capture_directory_or_default(window.get_capture_directory().as_str())?;

    ensure_capture_directory(&directory)?;
    open::that(&directory).context("failed to open local capture directory")?;

    Ok(())
}

pub(crate) fn ensure_default_capture_directory(config: &mut AppConfig) -> Result<()> {
    if capture_directory_needs_default(&config.capture_directory) {
        config.capture_directory = default_capture_directory()?.to_string_lossy().into_owned();
    }

    Ok(())
}
