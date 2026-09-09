//! Settings-view initialization, validation, and folder actions.

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use anyhow::{Context as _, Result};
use slint::ComponentHandle as _;

use crate::AppWindow;
use sharer::{
    config::{
        AppConfig, BehaviorConfig, CaptureNameMode, CaptureNamingConfig, CaptureResolution,
        PrivacyConfig, ProviderCredentials, ResizeQuality, ShortcutConfig,
    },
    storage::{default_capture_directory, ensure_capture_directory},
    upload::{UploadTarget, UploaderKind},
    validate_lifetime, validate_recording_fps, validate_recording_seconds,
};

pub(crate) fn initialize(window: &AppWindow, config: &AppConfig) {
    window.set_lifetime_seconds(config.lifetime_seconds.cast_signed());
    window.set_recording_fps(config.recording_fps.cast_signed());
    window.set_recording_max_seconds(config.recording_max_seconds.cast_signed());
    window.set_save_captures(config.behavior.save_captures);
    window.set_force_sdr_captures(config.behavior.force_sdr_captures);
    window.set_capture_resolution(config.behavior.capture_resolution.index());
    window.set_resize_quality(config.behavior.resize_quality.index());
    window.set_retina_scaling_available(cfg!(target_os = "macos"));
    window.set_private_capture_names(config.privacy.private_capture_names);
    window.set_remove_exif(config.privacy.remove_exif);
    window.set_capture_directory(config.capture_directory.clone().into());
    window.set_uploader_kind(config.uploader_kind.index());
    window.set_uploader_url(config.uploader_url.clone().into());
    window.set_imgur_credential(config.uploader_credentials.imgur.clone().into());
    window.set_vgy_credential(config.uploader_credentials.vgy.clone().into());
    window.set_sul_credential(config.uploader_credentials.sul.clone().into());
    window.set_upload_header_name(config.upload_header_name.clone().into());
    window.set_upload_header_value(config.upload_header_value.clone().into());
    window.set_minimize_to_tray(config.behavior.minimize_to_tray);
    window.set_start_at_login(config.behavior.start_at_login);
    window.set_capture_name_mode(config.capture_naming.mode.index());
    window.set_capture_custom_name(config.capture_naming.custom_name.clone().into());
    window.set_require_tor(config.privacy.require_tor);
    window.set_completion_sound(config.behavior.completion_sound);
    window.set_region_hotkey(config.shortcuts.region.clone().into());
    window.set_recording_hotkey(config.shortcuts.recording.clone().into());
    window.set_screen_hotkey(config.shortcuts.screen.clone().into());
    window.set_clipboard_hotkey(config.shortcuts.clipboard.clone().into());
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    window.set_settings_dirty(false);

    if config.upload_target().validate().is_err() {
        window.set_current_page(2);
        window.set_status_text("Setup required".into());
        window.set_status_detail("Choose and configure an uploader in Settings to begin".into());
    } else {
        window.set_status_text("Ready".into());
        window.set_status_detail(
            format!(
                "Uploads expire after {}  -  links are copied automatically",
                sharer::config::display_lifetime(config.lifetime_seconds)
            )
            .into(),
        );
    }
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
        || window.get_upload_header_name().trim() != saved.upload_header_name
        || window.get_upload_header_value().as_str() != saved.upload_header_value
        || window.get_lifetime_seconds() != saved.lifetime_seconds.cast_signed()
}

fn capture_settings_are_dirty(window: &AppWindow, saved: &AppConfig) -> bool {
    window.get_recording_fps() != saved.recording_fps.cast_signed()
        || window.get_recording_max_seconds() != saved.recording_max_seconds.cast_signed()
        || window.get_save_captures() != saved.behavior.save_captures
        || window.get_force_sdr_captures() != saved.behavior.force_sdr_captures
        || window.get_capture_resolution() != saved.behavior.capture_resolution.index()
        || window.get_resize_quality() != saved.behavior.resize_quality.index()
        || window.get_capture_directory().trim() != saved.capture_directory
        || window.get_capture_name_mode() != saved.capture_naming.mode.index()
        || window.get_capture_custom_name().trim() != saved.capture_naming.custom_name
}

fn privacy_settings_are_dirty(window: &AppWindow, saved: &AppConfig) -> bool {
    window.get_private_capture_names() != saved.privacy.private_capture_names
        || window.get_remove_exif() != saved.privacy.remove_exif
        || window.get_require_tor() != saved.privacy.require_tor
}

fn general_settings_are_dirty(window: &AppWindow, saved: &AppConfig) -> bool {
    window.get_minimize_to_tray() != saved.behavior.minimize_to_tray
        || window.get_start_at_login() != saved.behavior.start_at_login
        || window.get_completion_sound() != saved.behavior.completion_sound
        || window.get_region_hotkey().as_str() != saved.shortcuts.region
        || window.get_recording_hotkey().as_str() != saved.shortcuts.recording
        || window.get_screen_hotkey().as_str() != saved.shortcuts.screen
        || window.get_clipboard_hotkey().as_str() != saved.shortcuts.clipboard
}

pub(crate) fn config_from_window(window: &AppWindow) -> Result<AppConfig> {
    let lifetime_seconds = u32::try_from(window.get_lifetime_seconds())
        .context("lifetime must be a positive number")?;

    validate_lifetime(lifetime_seconds)?;
    let uploader = upload_target_from_window(window)?;
    let shortcuts = ShortcutConfig {
        region: window.get_region_hotkey().to_string(),
        recording: window.get_recording_hotkey().to_string(),
        screen: window.get_screen_hotkey().to_string(),
        clipboard: window.get_clipboard_hotkey().to_string(),
    };

    crate::hotkeys::validate_shortcuts(&shortcuts)?;
    let capture_naming = naming_from_window(window)?;
    let capture_resolution = CaptureResolution::from_index(window.get_capture_resolution())
        .context("invalid capture resolution")?;
    let resize_quality =
        ResizeQuality::from_index(window.get_resize_quality()).context("invalid resize quality")?;
    let save_captures = window.get_save_captures();
    let capture_directory = window.get_capture_directory().trim().to_owned();

    anyhow::ensure!(
        !save_captures || !capture_directory.is_empty(),
        "capture directory cannot be empty while local copies are enabled"
    );

    Ok(AppConfig {
        uploader_kind: uploader.kind,
        uploader_url: uploader.custom_url,
        uploader_credentials: ProviderCredentials {
            imgur: window.get_imgur_credential().trim().to_owned(),
            vgy: window.get_vgy_credential().trim().to_owned(),
            sul: window.get_sul_credential().trim().to_owned(),
        },
        upload_header_name: uploader.header_name,
        upload_header_value: uploader.header_value,
        lifetime_seconds,
        recording_fps: validated_recording_fps(window)?,
        recording_max_seconds: validated_recording_seconds(window)?,
        capture_directory,
        capture_naming,
        shortcuts,
        behavior: BehaviorConfig {
            save_captures,
            force_sdr_captures: window.get_force_sdr_captures(),
            capture_resolution,
            resize_quality,
            minimize_to_tray: window.get_minimize_to_tray(),
            start_at_login: window.get_start_at_login(),
            completion_sound: window.get_completion_sound(),
        },
        privacy: PrivacyConfig {
            require_tor: window.get_require_tor(),
            private_capture_names: window.get_private_capture_names(),
            remove_exif: window.get_remove_exif(),
        },
    })
}

pub(crate) fn upload_target_from_window(window: &AppWindow) -> Result<UploadTarget> {
    let kind = UploaderKind::from_index(window.get_uploader_kind())
        .context("invalid uploader provider")?;
    let target = UploadTarget {
        kind,
        custom_url: window.get_uploader_url().trim().to_owned(),
        credential: match kind {
            UploaderKind::Imgur => window.get_imgur_credential(),
            UploaderKind::Vgy => window.get_vgy_credential(),
            UploaderKind::Sul => window.get_sul_credential(),
            UploaderKind::Custom | UploaderKind::Uguu | UploaderKind::TransferSh => "".into(),
        }
        .trim()
        .to_owned(),
        header_name: window.get_upload_header_name().trim().to_owned(),
        header_value: window.get_upload_header_value().to_string(),
    };

    target.validate()?;
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

pub(crate) fn configured_capture_directory(window: &AppWindow) -> Option<PathBuf> {
    window
        .get_save_captures()
        .then(|| PathBuf::from(window.get_capture_directory().as_str()))
}

pub(crate) fn open_capture_folder(window: &AppWindow) -> Result<()> {
    let directory = PathBuf::from(window.get_capture_directory().as_str());

    anyhow::ensure!(
        !directory.as_os_str().is_empty(),
        "choose a local capture directory first"
    );

    ensure_capture_directory(&directory)?;
    open::that(&directory).context("failed to open local capture directory")?;

    Ok(())
}

pub(crate) fn ensure_default_capture_directory(config: &mut AppConfig) -> Result<()> {
    if config.capture_directory.trim().is_empty() {
        config.capture_directory = default_capture_directory()?.to_string_lossy().into_owned();
    }

    Ok(())
}
