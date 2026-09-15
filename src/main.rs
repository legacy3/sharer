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
use cli::{Cli, RendererPreference};
use sharer::{
    clipboard,
    config::AppConfig,
    history::HistoryEntry,
    proxy::preferred_upload_route,
    storage::{PlatformStorage, default_capture_directory, monthly_capture_directory},
    upload::{UploadClient, UploadPayload, UploadTarget, UploaderKind, validate_uploader_url},
};
#[cfg(feature = "desktop")]
use slint_generated::{AppWindow, HistoryRow, RegionWindow};

fn main() -> ExitCode {
    let cli = Cli::parse();

    prepare_windows_console(cli.is_headless());
    let mut output = std::io::stdout().lock();
    let result = if cli.is_headless() {
        run_headless(&cli, &mut output)
    } else {
        run_desktop(cli.background, cli.renderer)
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

#[cfg(windows)]
fn prepare_windows_console(headless: bool) {
    if !cfg!(feature = "desktop") || headless {
        let _ = consolex::attach();
    }
}

#[cfg(not(windows))]
const fn prepare_windows_console(_headless: bool) {}

#[cfg(feature = "desktop")]
fn run_desktop(background: bool, renderer: RendererPreference) -> Result<()> {
    app::run(!background, renderer)
}

#[cfg(not(feature = "desktop"))]
fn run_desktop(_background: bool, _renderer: RendererPreference) -> Result<()> {
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

    let local_screenshot = if cli.screenshot && config.behavior.save_captures {
        let directory = if config.capture_directory.trim().is_empty() {
            default_capture_directory()?
        } else {
            config.capture_directory.as_str().into()
        };

        Some(
            payload
                .save_generated_copy(&monthly_capture_directory(&directory))
                .context("failed to retain local screenshot")?,
        )
    } else {
        None
    };

    if config.privacy.remove_exif && !cli.screenshot {
        payload
            .remove_exif()
            .context("failed to remove image EXIF metadata")?;
    }

    let captured_filename = payload.filename.clone();
    let captured_size = payload.len();
    let receipt = (|| -> Result<_> {
        let route = preferred_upload_route(cli.require_tor || config.privacy.require_tor)?;

        UploadClient::for_target(&target, &route)?.upload(payload, lifetime)
    })();
    let entry = match receipt {
        Ok(receipt) => HistoryEntry::from_receipt(receipt, local_screenshot.as_deref()),

        Err(upload_error) => {
            let Some(path) = local_screenshot.as_deref() else {
                return Err(upload_error).context("upload failed");
            };

            return retain_local_screenshot_after_upload_failure(
                path,
                captured_filename,
                captured_size,
                |entry| storage.insert_history(entry).map(|_id| ()),
            );
        }
    };

    persist_and_write_upload_result(output, &entry, |entry| {
        storage.insert_history(entry).map(|_id| ())
    })?;

    if !cli.no_copy {
        let _ = clipboard::copy_link(&entry.link);
    }

    Ok(())
}

fn persist_and_write_upload_result(
    output: &mut impl std::io::Write,
    entry: &HistoryEntry,
    persist: impl FnOnce(&HistoryEntry) -> Result<()>,
) -> Result<()> {
    let history_saved = persist(entry).is_ok();
    let link_written = writeln!(output, "{}", entry.link).is_ok();

    match (history_saved, link_written) {
        (true, true) => Ok(()),

        (false, true) => bail!(
            "upload succeeded and its public link was written, but saving it to history failed; \
             its deletion capability was not preserved"
        ),

        (true, false) => bail!(
            "upload succeeded and was saved to history, but writing its public link failed; \
             recover it from upload history"
        ),

        (false, false) => bail!(
            "upload succeeded, but neither its public link nor its history record could be written; \
             its deletion capability was not preserved"
        ),
    }
}

fn retain_local_screenshot_after_upload_failure(
    path: &std::path::Path,
    filename: String,
    size_bytes: u64,
    persist: impl FnOnce(&HistoryEntry) -> Result<()>,
) -> Result<()> {
    let entry = HistoryEntry::local(filename, size_bytes, path);

    if persist(&entry).is_ok() {
        bail!(
            "upload failed; screenshot saved locally at {} and recorded in upload history",
            path.display()
        );
    }

    bail!(
        "upload failed; screenshot saved locally at {}, but its local history record could not be saved",
        path.display()
    )
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
    let filename_stem = naming::screenshot_stem(&config.capture_naming)?;

    payload.filename = sharer::capture::named_capture_filename(&filename_stem, &payload.filename);
    Ok(payload)
}

#[cfg(not(feature = "desktop"))]
fn capture_headless(_config: &AppConfig) -> Result<UploadPayload> {
    bail!("screenshot support is not included in this build; file uploads work headlessly")
}

#[cfg(test)]
mod tests {
    use std::io;

    use sharer::config::ProviderCredentials;
    use sharer::upload::UploadReceipt;

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

    fn sensitive_entry() -> HistoryEntry {
        HistoryEntry::from_receipt(
            UploadReceipt {
                original_name: "capture.png".to_owned(),
                size_bytes: 42,
                link: "https://public.example/capture".to_owned(),
                delete_url: "SENSITIVE-DELETION-CAPABILITY".to_owned(),
                expires_at: "soon".to_owned(),
            },
            None,
        )
    }

    #[test]
    fn history_failure_does_not_expose_deletion_capability() {
        let mut output = Vec::new();
        let error = persist_and_write_upload_result(&mut output, &sensitive_entry(), |entry| {
            Err(anyhow::anyhow!(entry.delete_url.clone()))
        })
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("deletion capability was not preserved"));
        assert!(!message.contains("SENSITIVE-DELETION-CAPABILITY"));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "https://public.example/capture\n"
        );
    }

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic output failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn output_failure_points_to_history_without_exposing_deletion_capability() {
        let error = persist_and_write_upload_result(
            &mut FailingWriter,
            &sensitive_entry(),
            |_entry| Ok(()),
        )
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("recover it from upload history"));
        assert!(!message.contains("SENSITIVE-DELETION-CAPABILITY"));
    }

    #[test]
    fn history_and_output_failure_still_hide_deletion_capability() {
        let error =
            persist_and_write_upload_result(&mut FailingWriter, &sensitive_entry(), |entry| {
                Err(anyhow::anyhow!(entry.delete_url.clone()))
            })
            .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("neither its public link nor its history record"));
        assert!(!message.contains("SENSITIVE-DELETION-CAPABILITY"));
    }

    #[test]
    fn failed_screenshot_upload_retains_local_history_without_error_details() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.png");

        std::fs::write(&path, b"capture").unwrap();
        let mut persisted = None;

        let error = retain_local_screenshot_after_upload_failure(
            &path,
            "capture.png".to_owned(),
            7,
            |entry| {
                persisted = Some(entry.clone());
                Ok(())
            },
        )
        .unwrap_err();
        let entry = persisted.unwrap();
        let message = error.to_string();

        assert_eq!(entry.local_path, path.to_string_lossy());
        assert!(entry.link.is_empty());
        assert!(message.contains(path.to_string_lossy().as_ref()));
        assert!(message.contains("recorded in upload history"));
        assert!(!message.contains("SENSITIVE-DELETION-CAPABILITY"));
    }

    #[test]
    fn failed_screenshot_upload_reports_local_history_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.png");

        let error = retain_local_screenshot_after_upload_failure(
            &path,
            "capture.png".to_owned(),
            7,
            |_entry| Err(anyhow::anyhow!("SENSITIVE-DELETION-CAPABILITY")),
        )
        .unwrap_err();
        let message = error.to_string();

        assert!(message.contains(path.to_string_lossy().as_ref()));
        assert!(message.contains("history record could not be saved"));
        assert!(!message.contains("SENSITIVE-DELETION-CAPABILITY"));
    }
}
