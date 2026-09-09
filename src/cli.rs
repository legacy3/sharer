//! Command-line contract shared by desktop and headless use.

use std::{fmt, num::ParseIntError, path::PathBuf, str::FromStr};

use clap::Parser;

use sharer::{
    MAX_HISTORY_PAGE_SIZE, MAX_LIFETIME_SECONDS, upload::UploaderKind, validate_lifetime,
};

/// Lightweight host-agnostic uploader.
#[derive(Parser)]
#[command(version, about)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "boolean fields directly represent independent command-line switches"
)]
pub struct Cli {
    /// Start the desktop app without opening its window (used by login startup).
    #[arg(long, hide = true)]
    pub background: bool,

    /// File to upload. With no file or action, launch the desktop app.
    #[arg(
        value_name = "FILE",
        conflicts_with_all = ["clipboard", "screenshot", "history", "init"]
    )]
    pub file: Option<PathBuf>,

    /// Upload a copied file, image, or text without opening the GUI.
    #[arg(long, conflicts_with_all = ["screenshot", "history", "init"])]
    pub clipboard: bool,

    /// Capture and upload the primary display without opening the GUI.
    #[arg(long, conflicts_with_all = ["history", "init"])]
    pub screenshot: bool,

    /// List saved upload links and their deletion URLs.
    #[arg(long, conflicts_with = "init")]
    pub history: bool,

    /// Save an uploader provider or custom endpoint and exit.
    #[arg(
        long,
        value_name = "PROVIDER_OR_URL",
        conflicts_with_all = ["uploader", "uploader_url"]
    )]
    pub init: Option<String>,

    /// Override the saved provider for this upload.
    #[arg(long, value_name = "PROVIDER", conflicts_with = "history")]
    pub uploader: Option<UploaderKind>,

    /// Override the saved uploader endpoint for this upload.
    #[arg(long, value_name = "URL")]
    pub uploader_url: Option<String>,

    /// Provider credential, such as an Imgur client ID or s-ul API key.
    #[arg(long, value_name = "VALUE", conflicts_with = "history")]
    pub credential: Option<String>,

    /// Add one Custom-uploader request header, for example `Authorization: Bearer token`.
    #[arg(long, value_name = "NAME: VALUE", conflicts_with = "history")]
    pub header: Option<HeaderArgument>,

    /// Emit history as JSON for scripts.
    #[arg(long, requires = "history")]
    pub json: bool,

    /// Maximum history entries to return.
    #[arg(
        long,
        requires = "history",
        value_name = "COUNT",
        default_value_t = 20,
        value_parser = parse_history_limit
    )]
    pub history_limit: usize,

    /// Number of newest history entries to skip.
    #[arg(long, requires = "history", value_name = "COUNT", default_value_t = 0)]
    pub history_offset: usize,

    /// Custom-uploader deletion lifetime in seconds (maximum one week).
    #[arg(long, value_parser = parse_lifetime, value_name = "SECONDS")]
    pub time: Option<u32>,

    /// Do not replace clipboard text with the returned link.
    #[arg(long)]
    pub no_copy: bool,

    /// Refuse to upload unless a local Tor SOCKS5 proxy is available.
    #[arg(long, visible_alias = "tor")]
    pub require_tor: bool,
}

impl Cli {
    #[must_use]
    pub fn is_headless(&self) -> bool {
        self.file.is_some()
            || self.clipboard
            || self.screenshot
            || self.history
            || self.init.is_some()
            || self.uploader.is_some()
            || self.uploader_url.is_some()
            || self.credential.is_some()
            || self.header.is_some()
    }
}

impl fmt::Debug for Cli {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cli")
            .field("background", &self.background)
            .field("file", &self.file)
            .field("clipboard", &self.clipboard)
            .field("screenshot", &self.screenshot)
            .field("history", &self.history)
            .field("init_configured", &self.init.is_some())
            .field("uploader", &self.uploader)
            .field("uploader_url_configured", &self.uploader_url.is_some())
            .field("credential_configured", &self.credential.is_some())
            .field("header", &self.header)
            .field("json", &self.json)
            .field("history_limit", &self.history_limit)
            .field("history_offset", &self.history_offset)
            .field("time", &self.time)
            .field("no_copy", &self.no_copy)
            .field("require_tor", &self.require_tor)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HeaderArgument {
    pub name: String,
    pub value: String,
}

impl fmt::Debug for HeaderArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HeaderArgument")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .finish()
    }
}

impl FromStr for HeaderArgument {
    type Err = HeaderArgumentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, value) = value
            .split_once(':')
            .ok_or_else(|| HeaderArgumentError::new("missing ':' separator"))?;
        let name = name.trim();
        let value = value.trim_start();

        sharer::upload::validate_upload_header(name, value)
            .map_err(|error| HeaderArgumentError::new(error.to_string()))?;

        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
        })
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("header must be a valid `Name: value` pair: {detail}")]
pub struct HeaderArgumentError {
    detail: String,
}

impl HeaderArgumentError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

fn parse_lifetime(value: &str) -> Result<u32, CliLifetimeError> {
    let seconds = value.parse::<u32>().map_err(CliLifetimeError::NotInteger)?;

    validate_lifetime(seconds).map_err(CliLifetimeError::OutOfRange)
}

fn parse_history_limit(value: &str) -> Result<usize, CliHistoryLimitError> {
    let limit = value
        .parse::<usize>()
        .map_err(CliHistoryLimitError::NotInteger)?;

    if !(1..=MAX_HISTORY_PAGE_SIZE).contains(&limit) {
        return Err(CliHistoryLimitError::OutOfRange(limit));
    }

    Ok(limit)
}

#[derive(Debug, thiserror::Error)]
enum CliLifetimeError {
    #[error("lifetime must be a whole number of seconds: {0}")]
    NotInteger(ParseIntError),
    #[error("lifetime must be between 1 and {MAX_LIFETIME_SECONDS} seconds: {0}")]
    OutOfRange(sharer::LifetimeError),
}

#[derive(Debug, thiserror::Error)]
enum CliHistoryLimitError {
    #[error("history page size must be a whole number: {0}")]
    NotInteger(ParseIntError),
    #[error("history page size must be between 1 and {MAX_HISTORY_PAGE_SIZE}, got {0}")]
    OutOfRange(usize),
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    #[test]
    fn accepts_positional_file_argument() {
        let cli = Cli::try_parse_from(["sharer", "capture.png", "--time", "60"]).unwrap();

        assert_eq!(cli.file, Some(PathBuf::from("capture.png")));
        assert_eq!(cli.time, Some(60));
        assert!(cli.is_headless());
    }

    #[test]
    fn rejects_lifetime_longer_than_a_week() {
        let error = Cli::try_parse_from(["sharer", "capture.png", "--time", "604801"]).unwrap_err();

        assert!(error.to_string().contains("between 1 and 604800"));
    }

    #[test]
    fn accepts_endpoint_initialization() {
        let cli = Cli::try_parse_from(["sharer", "--init", "https://example.test/upload"]).unwrap();

        assert_eq!(cli.init.as_deref(), Some("https://example.test/upload"));
        assert!(cli.is_headless());
    }

    #[test]
    fn accepts_builtin_provider_and_credential() {
        let cli = Cli::try_parse_from([
            "ShareR",
            "capture.png",
            "--uploader",
            "imgur",
            "--credential",
            "client-id",
        ])
        .unwrap();

        assert_eq!(cli.uploader, Some(UploaderKind::Imgur));
        assert_eq!(cli.credential.as_deref(), Some("client-id"));
    }

    #[test]
    fn accepts_custom_upload_header() {
        let cli = Cli::try_parse_from([
            "sharer",
            "capture.png",
            "--header",
            "Authorization: Bearer secret",
        ])
        .unwrap();

        assert_eq!(
            cli.header,
            Some(HeaderArgument {
                name: "Authorization".to_owned(),
                value: "Bearer secret".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_unbounded_history_pages() {
        let error =
            Cli::try_parse_from(["sharer", "--history", "--history-limit", "1001"]).unwrap_err();

        assert!(error.to_string().contains("1000"));
    }

    #[test]
    fn background_startup_flag_still_selects_the_desktop() {
        let cli = Cli::try_parse_from(["sharer", "--background"]).unwrap();

        assert!(cli.background);
        assert!(!cli.is_headless());
    }
}
