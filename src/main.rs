#![cfg_attr(windows, windows_subsystem = "windows")]

//! Desktop and headless entry point for `ShareR`.

#[cfg(feature = "desktop")]
mod app;
#[cfg(feature = "desktop")]
mod autostart;
mod cli;
#[cfg(feature = "desktop")]
mod hotkeys;
#[cfg(feature = "desktop")]
mod icon;
#[cfg(feature = "desktop")]
mod naming;
#[cfg(feature = "desktop")]
mod recording;
#[cfg(feature = "desktop")]
mod sound;
#[cfg(feature = "desktop")]
mod tray;
#[cfg(feature = "desktop")]
mod update;
#[cfg(feature = "desktop")]
mod views;

#[expect(
    trivial_numeric_casts,
    missing_debug_implementations,
    unreachable_pub,
    unused_qualifications,
    clippy::clone_on_ref_ptr,
    clippy::semicolon_outside_block,
    reason = "Slint-generated Rust contains redundant casts and private generated types"
)]
#[cfg(feature = "desktop")]
mod slint_generated {
    slint::include_modules!();
}

use std::{io::Write as _, process::ExitCode};

use anyhow::{Context, Result, bail};
use clap::Parser as _;
use cli::Cli;
use sharer::{
    clipboard,
    config::AppConfig,
    history::HistoryEntry,
    proxy::preferred_upload_route,
    storage::{PlatformStorage, default_capture_directory},
    upload::{UploadClient, UploadPayload, UploadTarget, UploaderKind, validate_uploader_url},
};
#[cfg(feature = "desktop")]
use slint_generated::{AppWindow, HistoryRow, RegionWindow};

fn main() -> ExitCode {
    configure_low_memory_window_hiding();
    let cli = Cli::parse();

    prepare_windows_console(cli.is_headless());
    let mut output = std::io::stdout().lock();
    let result = if cli.is_headless() {
        run_headless(&cli, &mut output)
    } else {
        run_desktop(cli.background)
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,

        Err(error) => {
            let mut error_output = std::io::stderr().lock();
            let _ = writeln!(error_output, "ShareR: {error:#}");

            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "desktop")]
#[expect(
    unsafe_code,
    reason = "Rust 2024 requires an unsafe block for a process-wide variable set before threads"
)]
fn configure_low_memory_window_hiding() {
    // SAFETY: This runs before the process starts any threads.
    unsafe {
        std::env::set_var("SLINT_DESTROY_WINDOW_ON_HIDE", "1");
    }
}

#[cfg(not(feature = "desktop"))]
const fn configure_low_memory_window_hiding() {}

#[cfg(windows)]
fn prepare_windows_console(headless: bool) {
    if !cfg!(feature = "desktop") || headless {
        let _ = consolex::attach();
    }
}

#[cfg(not(windows))]
const fn prepare_windows_console(_headless: bool) {}

#[cfg(feature = "desktop")]
fn run_desktop(background: bool) -> Result<()> {
    app::run(!background)
}

#[cfg(not(feature = "desktop"))]
fn run_desktop(_background: bool) -> Result<()> {
    bail!("desktop UI is not included in this build; rebuild with `--features desktop`")
}

fn run_headless(cli: &Cli, output: &mut impl std::io::Write) -> Result<()> {
    let storage = PlatformStorage::open()?;

    if cli.history {
        return write_history(cli, &storage, output);
    }

    let mut config = AppConfig::load(&storage)?;

    if let Some(initializer) = &cli.init {
        let target = initialized_upload_target(initializer, cli, &config)?;

        apply_upload_target(&mut config, target);
        config.save(&storage)?;
        writeln!(
            output,
            "Uploader configured: {}",
            config.uploader_kind.name()
        )
        .context("failed to confirm uploader configuration")?;

        return Ok(());
    }

    let target = upload_target_for_command(cli, &config)
        .context("configure an uploader with `ShareR --init PROVIDER_OR_URL` or use Settings")?;

    anyhow::ensure!(
        cli.time.is_none() || target.kind.supports_lifetime(),
        "--time is only supported by the Custom uploader"
    );

    let lifetime = cli.time.unwrap_or(config.lifetime_seconds);
    let mut payload = if let Some(path) = &cli.file {
        UploadPayload::from_path(path)?
    } else if cli.clipboard {
        clipboard::read_payload()?
    } else if cli.screenshot {
        capture_headless(&config)?
    } else {
        bail!("no headless upload source was selected");
    };

    if cli.screenshot && config.behavior.save_captures {
        let directory = if config.capture_directory.trim().is_empty() {
            default_capture_directory()?
        } else {
            config.capture_directory.as_str().into()
        };

        payload
            .save_generated_copy(&directory)
            .context("failed to retain local screenshot")?;
    }

    if config.privacy.remove_exif && !cli.screenshot {
        payload
            .remove_exif()
            .context("failed to remove image EXIF metadata")?;
    }

    let route = preferred_upload_route(cli.require_tor || config.privacy.require_tor)?;
    let receipt = UploadClient::for_target(&target, &route)?
        .upload(payload, lifetime)
        .context("upload failed")?;
    let entry = HistoryEntry::from_receipt(receipt);

    storage.insert_history(&entry).with_context(|| {
        format!(
            "upload succeeded, but saving it to history failed; deletion URL: {}",
            entry.delete_url
        )
    })?;

    writeln!(output, "{}", entry.link).with_context(|| {
        format!(
            "upload succeeded and was saved to history, but writing its link failed; deletion URL: {}",
            entry.delete_url
        )
    })?;

    if !cli.no_copy {
        let _ = clipboard::copy_link(&entry.link);
    }

    Ok(())
}

fn initialized_upload_target(
    initializer: &str,
    cli: &Cli,
    config: &AppConfig,
) -> Result<UploadTarget> {
    let kind = if initializer.contains("://") {
        validate_uploader_url(initializer)?;
        UploaderKind::Custom
    } else {
        initializer.parse::<UploaderKind>()?
    };
    let mut target = config.upload_target_for(kind);

    if kind == UploaderKind::Custom && initializer.contains("://") {
        initializer.trim().clone_into(&mut target.custom_url);
    }

    apply_cli_upload_overrides(&mut target, cli);
    target.validate()?;
    Ok(target)
}

fn upload_target_for_command(cli: &Cli, config: &AppConfig) -> Result<UploadTarget> {
    let kind = cli.uploader.unwrap_or(config.uploader_kind);
    let mut target = config.upload_target_for(kind);

    if let Some(endpoint) = &cli.uploader_url {
        target.kind = UploaderKind::Custom;
        target.credential.clear();
        endpoint.trim().clone_into(&mut target.custom_url);
    }

    apply_cli_upload_overrides(&mut target, cli);
    target.validate()?;
    Ok(target)
}

fn apply_cli_upload_overrides(target: &mut UploadTarget, cli: &Cli) {
    if let Some(credential) = &cli.credential {
        credential.trim().clone_into(&mut target.credential);
    }

    if let Some(header) = &cli.header {
        header.name.clone_into(&mut target.header_name);
        header.value.clone_into(&mut target.header_value);
    }
}

fn apply_upload_target(config: &mut AppConfig, target: UploadTarget) {
    config.uploader_kind = target.kind;
    config.uploader_url = target.custom_url;
    config
        .uploader_credentials
        .set_for_provider(target.kind, target.credential);
    config.upload_header_name = target.header_name;
    config.upload_header_value = target.header_value;
}

fn write_history(
    cli: &Cli,
    storage: &PlatformStorage,
    output: &mut impl std::io::Write,
) -> Result<()> {
    let entries = storage.history_page(cli.history_offset, cli.history_limit)?;

    if cli.json {
        serde_json::to_writer_pretty(&mut *output, &entries)
            .context("failed to write JSON history")?;
        writeln!(output).context("failed to finish JSON history")?;
    } else if entries.is_empty() {
        writeln!(output, "No uploads yet").context("failed to write history")?;
    } else {
        for entry in entries {
            writeln!(
                output,
                "{}\t{} bytes\texpires {}\n{}\ndelete: {}\n",
                entry.filename, entry.size_bytes, entry.expires_at, entry.link, entry.delete_url
            )
            .context("failed to write history")?;
        }
    }

    Ok(())
}

#[cfg(feature = "desktop")]
fn capture_headless(config: &AppConfig) -> Result<UploadPayload> {
    let color_mode = if config.behavior.force_sdr_captures {
        sharer::capture::CaptureColorMode::Sdr
    } else {
        sharer::capture::CaptureColorMode::Automatic
    };
    let mut payload = sharer::capture::primary_monitor(
        color_mode,
        config.behavior.capture_resolution,
        config.behavior.resize_quality,
    )
    .context("screenshot capture requires a graphical session")?;
    let filename_stem =
        naming::screenshot_stem(&config.capture_naming, config.privacy.private_capture_names)?;

    payload.filename = sharer::capture::named_capture_filename(&filename_stem, &payload.filename);
    Ok(payload)
}

#[cfg(not(feature = "desktop"))]
fn capture_headless(_config: &AppConfig) -> Result<UploadPayload> {
    bail!("screenshot support is not included in this build; file uploads work headlessly")
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use sharer::config::ProviderCredentials;

    use super::*;

    #[test]
    fn cli_provider_override_uses_only_that_providers_credential() {
        let config = AppConfig {
            uploader_kind: UploaderKind::Sul,
            uploader_credentials: ProviderCredentials {
                imgur: "imgur-secret".to_owned(),
                vgy: "vgy-secret".to_owned(),
                sul: "sul-secret".to_owned(),
            },
            ..AppConfig::default()
        };
        let imgur = Cli::try_parse_from(["ShareR", "--uploader", "imgur"]).unwrap();
        let uguu = Cli::try_parse_from(["ShareR", "--uploader", "uguu"]).unwrap();

        assert_eq!(
            upload_target_for_command(&imgur, &config)
                .unwrap()
                .credential,
            "imgur-secret"
        );
        assert!(
            upload_target_for_command(&uguu, &config)
                .unwrap()
                .credential
                .is_empty()
        );
    }
}
