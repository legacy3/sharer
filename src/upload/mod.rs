//! Host-agnostic multipart upload protocol.

use std::{
    fmt,
    io::{Cursor, ErrorKind, Read, Seek, SeekFrom, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt as _;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use futures_util::StreamExt as _;
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
    multipart,
};
use serde::Deserialize;

use crate::validate_lifetime;

mod metadata;
mod providers;

use metadata::{MetadataFormat, load_and_strip_exif, strip_exif_in_place};

pub use providers::UploaderKind;

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

/// Owned provider configuration passed to the upload worker.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct UploadTarget {
    pub kind: UploaderKind,
    pub custom_url: String,
    pub credential: String,
    pub header_name: String,
    pub header_value: String,
}

impl UploadTarget {
    /// Validate endpoint, credential, and custom-header requirements.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected provider configuration is incomplete or malformed.
    pub fn validate(&self) -> Result<()> {
        if self.kind == UploaderKind::Custom {
            validate_uploader_url(&self.custom_url)?;
            validate_upload_header(&self.header_name, &self.header_value)?;
        }

        anyhow::ensure!(
            !self.kind.requires_credential() || !self.credential.trim().is_empty(),
            "{} requires a credential",
            self.kind.name()
        );
        Ok(())
    }
}

impl fmt::Debug for UploadTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadTarget")
            .field("kind", &self.kind)
            .field("custom_url_configured", &!self.custom_url.is_empty())
            .field("credential_configured", &!self.credential.is_empty())
            .field("header_name", &self.header_name)
            .field("header_value", &"[redacted]")
            .finish()
    }
}

/// Network route used for an upload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UploadRoute {
    Direct,
    Socks5(String),
}

/// Data ready to be sent as the multipart `file` field.
#[derive(Debug)]
pub struct UploadPayload {
    body: PayloadBody,
    pub filename: String,
    pub mime: String,
}

enum PayloadBody {
    Bytes(Vec<u8>),
    File {
        file: cap_std::fs::File,
        length: u64,
    },
    Reader {
        reader: Box<dyn ReadSeek>,
        length: u64,
    },
}

trait ReadSeek: Read + Seek + Send {}

impl<T> ReadSeek for T where T: Read + Seek + Send {}

impl fmt::Debug for PayloadBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => formatter
                .debug_tuple("Bytes")
                .field(&format_args!("{} bytes", bytes.len()))
                .finish(),

            Self::File { length, .. } => formatter
                .debug_struct("File")
                .field("length", length)
                .finish_non_exhaustive(),

            Self::Reader { length, .. } => formatter
                .debug_struct("Reader")
                .field("length", length)
                .finish_non_exhaustive(),
        }
    }
}

impl UploadPayload {
    /// Create an in-memory payload for generated or clipboard content.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>, filename: String, mime: String) -> Self {
        Self {
            body: PayloadBody::Bytes(bytes),
            filename,
            mime,
        }
    }

    /// Create a payload backed by an owned seekable reader without copying its contents.
    #[must_use]
    pub fn from_reader<R>(reader: R, length: u64, filename: String, mime: String) -> Self
    where
        R: Read + Seek + Send + 'static,
    {
        Self {
            body: PayloadBody::Reader {
                reader: Box::new(reader),
                length,
            },
            filename,
            mime,
        }
    }

    /// Open a file for streaming without transcoding it, retaining HDR and other metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be read.
    pub fn from_path(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let filename_os = path
            .file_name()
            .context("selected upload path has no filename")?;
        let directory = Dir::open_ambient_dir(parent, ambient_authority())
            .with_context(|| format!("failed to open {}", parent.display()))?;
        let file = directory
            .open(filename_os)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;

        anyhow::ensure!(metadata.is_file(), "upload path is not a regular file");

        let filename = filename_os.to_string_lossy().into_owned();
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .essence_str()
            .to_owned();

        Ok(Self {
            body: PayloadBody::File {
                file,
                length: metadata.len(),
            },
            filename,
            mime,
        })
    }

    /// Remove EXIF chunks from JPEG, PNG, and WebP payloads without recompressing pixels.
    ///
    /// File-backed images remain streamed when they do not contain EXIF metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when a supported image is malformed or cannot be read.
    pub fn remove_exif(&mut self) -> Result<()> {
        let format = match self.mime.as_str() {
            "image/jpeg" => MetadataFormat::Jpeg,
            "image/png" => MetadataFormat::Png,
            "image/webp" => MetadataFormat::Webp,
            _ => return Ok(()),
        };
        let stripped_body = match &mut self.body {
            PayloadBody::Bytes(bytes) => {
                strip_exif_in_place(bytes, format)?;
                None
            }

            PayloadBody::File { file, length } => load_and_strip_exif(file, *length, format)?,

            PayloadBody::Reader { reader, length } => {
                load_and_strip_exif(reader.as_mut(), *length, format)?
            }
        };

        if let Some(bytes) = stripped_body {
            self.body = PayloadBody::Bytes(bytes);
        }

        Ok(())
    }

    /// Save an in-memory generated capture without replacing an existing file.
    ///
    /// A numeric suffix is added when the requested filename already exists.
    ///
    /// # Errors
    ///
    /// Returns an error for non-generated payloads, unsafe filenames, or filesystem failures.
    pub fn save_generated_copy(&mut self, directory: &Path) -> Result<PathBuf> {
        if matches!(self.body, PayloadBody::File { .. }) {
            bail!("only generated in-memory captures can be stored locally");
        }

        let filename = Path::new(&self.filename);

        anyhow::ensure!(
            filename.file_name() == Some(filename.as_os_str()),
            "generated capture filename is not a plain filename"
        );

        Dir::create_ambient_dir_all(directory, ambient_authority())
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let output = Dir::open_ambient_dir(directory, ambient_authority())
            .with_context(|| format!("failed to open {}", directory.display()))?;

        for index in 0..10_000 {
            let candidate = available_filename(filename, index);
            let mut options = OpenOptions::new();

            options.write(true).create_new(true);

            #[cfg(unix)]
            options.mode(0o600);

            let mut file = match output.open_with(&candidate, &options) {
                Ok(file) => file,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("failed to create local capture"),
            };

            let write_result = match &mut self.body {
                PayloadBody::Bytes(bytes) => file
                    .write_all(bytes)
                    .context("failed to write local capture"),

                PayloadBody::Reader { reader, .. } => {
                    let copy_result = std::io::copy(&mut **reader, &mut file)
                        .map(|_written| ())
                        .context("failed to stream local capture");
                    let rewind_result = reader
                        .seek(SeekFrom::Start(0))
                        .map(|_position| ())
                        .context("failed to rewind local capture");

                    copy_result.and(rewind_result)
                }

                PayloadBody::File { .. } => Err(anyhow::anyhow!(
                    "only generated in-memory captures can be stored locally"
                )),
            };

            drop(file);

            if let Err(error) = write_result {
                let _ = output.remove_file(&candidate);

                return Err(error);
            }

            return Ok(directory.join(candidate));
        }

        bail!("too many local captures share the same filename")
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        match &self.body {
            PayloadBody::Bytes(bytes) => Some(bytes),
            PayloadBody::File { .. } | PayloadBody::Reader { .. } => None,
        }
    }

    fn length(&self) -> u64 {
        match &self.body {
            PayloadBody::Bytes(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            PayloadBody::File { length, .. } | PayloadBody::Reader { length, .. } => *length,
        }
    }

    fn into_part_with_progress<F>(self, progress: F) -> Result<multipart::Part>
    where
        F: Fn(u64, u64) + Send + 'static,
    {
        let (reader, length) = match self.body {
            PayloadBody::Bytes(bytes) => {
                let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);

                (PayloadReader::Bytes(Cursor::new(bytes)), length)
            }

            PayloadBody::File { file, length } => (PayloadReader::File(file), length),

            PayloadBody::Reader { reader, length } => (PayloadReader::Reader(reader), length),
        };
        let reader = ProgressReader::new(reader, length, progress);
        let stream = futures_util::stream::unfold(Some(reader), |state| async move {
            let mut reader = state?;
            let mut buffer = vec![0_u8; UPLOAD_CHUNK_BYTES];

            match reader.read(&mut buffer) {
                Ok(0) => None,

                Ok(read) => {
                    buffer.truncate(read);
                    Some((Ok::<_, std::io::Error>(buffer), Some(reader)))
                }

                Err(error) => Some((Err(error), None)),
            }
        });
        let body = reqwest::Body::wrap_stream(stream);
        let part = multipart::Part::stream_with_length(body, length);

        part.file_name(self.filename)
            .mime_str(&self.mime)
            .context("invalid upload MIME type")
    }
}

enum PayloadReader {
    Bytes(Cursor<Vec<u8>>),
    File(cap_std::fs::File),
    Reader(Box<dyn ReadSeek>),
}

impl Read for PayloadReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Bytes(reader) => reader.read(buffer),
            Self::File(reader) => reader.read(buffer),
            Self::Reader(reader) => reader.read(buffer),
        }
    }
}

struct ProgressReader<R, F> {
    inner: R,
    total: u64,
    transferred: u64,
    last_percent: u8,
    progress: F,
}

impl<R, F> ProgressReader<R, F> {
    const fn new(inner: R, total: u64, progress: F) -> Self {
        Self {
            inner,
            total,
            transferred: 0,
            last_percent: 0,
            progress,
        }
    }
}

impl<R, F> Read for ProgressReader<R, F>
where
    R: Read,
    F: Fn(u64, u64),
{
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;

        self.transferred = self
            .transferred
            .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        let transferred = self.transferred.min(self.total);
        let percent = transferred
            .saturating_mul(100)
            .checked_div(self.total)
            .and_then(|percent| u8::try_from(percent).ok())
            .unwrap_or(100);

        if percent > self.last_percent || read == 0 {
            self.last_percent = percent;
            (self.progress)(transferred, self.total);
        }

        Ok(read)
    }
}

fn available_filename(filename: &Path, index: u32) -> PathBuf {
    if index == 0 {
        return filename.to_owned();
    }

    let stem = filename
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("capture");
    let extension = filename.extension().and_then(|value| value.to_str());
    let candidate = extension.map_or_else(
        || format!("{stem}-{index}"),
        |extension| format!("{stem}-{index}.{extension}"),
    );

    PathBuf::from(candidate)
}

/// Blocking client intended to run on the application's upload worker.
#[derive(Debug)]
pub struct UploadClient {
    client: Client,
    runtime: tokio::runtime::Runtime,
    target: UploadTarget,
}

impl UploadClient {
    /// Build an uploader for a specific endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL is invalid or the HTTP client cannot be built.
    pub fn new(
        endpoint: &str,
        route: &UploadRoute,
        custom_header: Option<(&str, &str)>,
    ) -> Result<Self> {
        let (header_name, header_value) = custom_header.unwrap_or_default();
        let target = UploadTarget {
            kind: UploaderKind::Custom,
            custom_url: endpoint.to_owned(),
            credential: String::new(),
            header_name: header_name.to_owned(),
            header_value: header_value.to_owned(),
        };

        Self::for_target(&target, route)
    }

    /// Build an uploader for a validated built-in or custom target.
    ///
    /// # Errors
    ///
    /// Returns an error if the target, URL, credential, or HTTP client is invalid.
    pub fn for_target(target: &UploadTarget, route: &UploadRoute) -> Result<Self> {
        target.validate()?;
        let default_headers = target.kind.headers(target)?;

        ensure_tls_provider();
        let builder = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .default_headers(default_headers)
            .user_agent(concat!("sharer/", env!("CARGO_PKG_VERSION")));
        let builder = match route {
            UploadRoute::Direct => builder,

            UploadRoute::Socks5(url) => builder
                .no_proxy()
                .proxy(reqwest::Proxy::all(url).context("invalid SOCKS5 proxy URL")?),
        };
        let client = builder.build().context("failed to build HTTP client")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to initialize the upload runtime")?;

        Ok(Self {
            client,
            runtime,
            target: target.clone(),
        })
    }

    /// Upload a payload with the requested deletion lifetime.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid lifetimes, HTTP failures, or malformed responses.
    pub fn upload(&self, payload: UploadPayload, lifetime_seconds: u32) -> Result<UploadReceipt> {
        self.upload_with_progress(payload, lifetime_seconds, |_transferred, _total| {})
    }

    /// Upload a payload and report body bytes as the HTTP client consumes them.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid lifetimes, HTTP failures, or malformed responses.
    pub fn upload_with_progress<F>(
        &self,
        payload: UploadPayload,
        lifetime_seconds: u32,
        progress: F,
    ) -> Result<UploadReceipt>
    where
        F: Fn(u64, u64) + Send + 'static,
    {
        validate_lifetime(lifetime_seconds)?;
        anyhow::ensure!(
            !self.target.kind.images_only() || payload.mime.starts_with("image/"),
            "{} only accepts images",
            self.target.kind.name()
        );
        let filename = payload.filename.clone();
        let size_bytes = payload.length();
        let url = self.target.kind.endpoint(&self.target, lifetime_seconds)?;
        let part = payload.into_part_with_progress(progress)?;
        let form = self.target.kind.form(part, &self.target);
        let (status, delete_header, body) = self.runtime.block_on(async {
            let response = self
                .client
                .post(url)
                .multipart(form)
                .send()
                .await
                .context("upload request failed")?;
            let status = response.status();
            let delete_header = response
                .headers()
                .get("x-url-delete")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let mut chunks = response.bytes_stream();
            let mut body = Vec::with_capacity(4_096);

            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.context("failed to read upload response")?;

                anyhow::ensure!(
                    body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
                    "uploader response exceeded {MAX_RESPONSE_BYTES} bytes"
                );
                body.extend_from_slice(&chunk);
                tokio::task::yield_now().await;
            }

            Ok::<_, anyhow::Error>((status, delete_header, body))
        })?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&body);
            let body = body.trim();
            let detail = body
                .char_indices()
                .nth(300)
                .map_or(body, |(boundary, _character)| &body[..boundary]);

            bail!("upload failed with HTTP {status}: {detail}");
        }

        let mut receipt = self.target.kind.parse(
            &body,
            &self.target,
            providers::ResponseMetadata {
                filename,
                size_bytes,
                delete_header: &delete_header,
            },
        )?;

        validate_response_url(&receipt.link, "public link")?;

        if !receipt.delete_url.is_empty() {
            validate_response_url(&receipt.delete_url, "deletion URL")?;
        }

        if receipt.expires_at.is_empty() {
            receipt.expires_at = format!("managed by {}", self.target.kind.name());
        }

        Ok(receipt)
    }
}

pub(super) fn plain_receipt(
    body: &[u8],
    filename: String,
    size_bytes: u64,
    delete_url: String,
) -> Result<UploadReceipt> {
    let link = std::str::from_utf8(body)
        .context("uploader returned non-UTF-8 text")?
        .trim()
        .to_owned();

    Ok(UploadReceipt {
        original_name: filename,
        size_bytes,
        link,
        delete_url,
        expires_at: String::new(),
    })
}

#[cfg(test)]
fn read_response_body(reader: impl Read) -> Result<Vec<u8>> {
    let limit = u64::try_from(MAX_RESPONSE_BYTES).unwrap_or(u64::MAX);
    let mut body = Vec::with_capacity(4096);

    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut body)
        .context("failed to read upload response")?;
    anyhow::ensure!(
        body.len() <= MAX_RESPONSE_BYTES,
        "uploader response exceeded {MAX_RESPONSE_BYTES} bytes"
    );

    Ok(body)
}

/// Validate a configured uploader endpoint.
///
/// # Errors
///
/// Returns an error when the value is not an absolute HTTP or HTTPS URL.
pub fn validate_uploader_url(endpoint: &str) -> Result<()> {
    let _ = parse_uploader_url(endpoint)?;

    Ok(())
}

/// Validate one optional custom HTTP request header without exposing its value.
///
/// # Errors
///
/// Returns an error for an empty/invalid name or a value containing forbidden bytes.
pub fn validate_upload_header(name: &str, value: &str) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::ensure!(
            value.is_empty(),
            "custom header value requires a header name"
        );

        return Ok(());
    }

    let _ = custom_header_map(name, value)?;

    Ok(())
}

fn custom_header_map(name: &str, value: &str) -> Result<HeaderMap> {
    let name =
        HeaderName::from_bytes(name.trim().as_bytes()).context("invalid custom header name")?;

    anyhow::ensure!(
        !is_reserved_upload_header(&name),
        "custom header name is controlled by the HTTP uploader"
    );
    let mut value = HeaderValue::from_str(value).context("invalid custom header value")?;

    value.set_sensitive(true);
    let mut headers = HeaderMap::with_capacity(1);

    headers.insert(name, value);

    Ok(headers)
}

fn is_reserved_upload_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "content-type"
            | "transfer-encoding"
            | "connection"
            | "proxy-connection"
            | "keep-alive"
            | "upgrade"
            | "trailer"
            | "te"
            | "expect"
    )
}

/// Ensure the Linux Rustls backend has a process-wide cryptography provider.
///
/// Other supported platforms use their native TLS backend, so this is a no-op there.
#[cfg(target_os = "linux")]
pub fn ensure_tls_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

#[cfg(not(target_os = "linux"))]
pub const fn ensure_tls_provider() {}

fn parse_uploader_url(endpoint: &str) -> Result<Url> {
    let endpoint = endpoint.trim();

    anyhow::ensure!(!endpoint.is_empty(), "uploader URL is not configured");
    let url = Url::parse(endpoint).context("uploader URL is invalid")?;

    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host().is_some(),
        "uploader URL must be an absolute HTTP or HTTPS URL"
    );

    Ok(url)
}

fn validate_response_url(value: &str, description: &str) -> Result<()> {
    let url =
        Url::parse(value).with_context(|| format!("uploader returned an invalid {description}"))?;

    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host().is_some(),
        "uploader returned a non-HTTP {description}"
    );

    Ok(())
}

fn endpoint_with_lifetime(endpoint: &Url, lifetime_seconds: u32) -> Url {
    let mut url = endpoint.clone();
    let retained_pairs = url
        .query_pairs()
        .filter(|(name, _value)| name != "time")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();

    url.query_pairs_mut()
        .clear()
        .extend_pairs(retained_pairs)
        .append_pair("time", &lifetime_seconds.to_string());
    url
}

/// Successful upload response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UploadReceipt {
    pub original_name: String,
    #[serde(rename = "size")]
    pub size_bytes: u64,
    pub link: String,
    #[serde(rename = "deleteLink")]
    pub delete_url: String,
    pub expires_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetime_is_appended_as_seconds() {
        let endpoint = Url::parse("https://uploads.example/upload?source=desktop").unwrap();
        let url = endpoint_with_lifetime(&endpoint, 3_600);

        assert_eq!(
            url.as_str(),
            "https://uploads.example/upload?source=desktop&time=3600"
        );
    }

    #[test]
    fn configured_lifetime_is_replaced() {
        let endpoint = Url::parse("https://uploads.example/upload?time=12&token=public").unwrap();
        let url = endpoint_with_lifetime(&endpoint, 60);

        assert_eq!(
            url.as_str(),
            "https://uploads.example/upload?token=public&time=60"
        );
    }

    #[test]
    fn response_links_must_be_absolute_http_urls() {
        assert!(validate_response_url("https://files.example/a", "public link").is_ok());
        assert!(validate_response_url("file:///tmp/secret", "public link").is_err());
        assert!(validate_response_url("javascript:alert(1)", "public link").is_err());
        assert!(validate_response_url("/relative", "public link").is_err());
    }

    #[test]
    fn error_detail_truncation_preserves_utf8_boundaries() {
        let body = "\u{00e9}".repeat(301);
        let detail = body
            .char_indices()
            .nth(300)
            .map_or(body.as_str(), |(boundary, _character)| &body[..boundary]);

        assert_eq!(detail.chars().count(), 300);
    }

    #[test]
    fn generated_payload_retains_bytes() {
        let payload = UploadPayload::from_bytes(
            vec![1, 2, 3],
            "sample.bin".to_owned(),
            "application/octet-stream".to_owned(),
        );

        assert_eq!(payload.bytes(), Some([1, 2, 3].as_slice()));
    }

    #[test]
    fn response_body_is_bounded() {
        let accepted = vec![0_u8; MAX_RESPONSE_BYTES];
        let rejected = vec![0_u8; MAX_RESPONSE_BYTES + 1];

        assert_eq!(
            read_response_body(Cursor::new(accepted)).unwrap().len(),
            MAX_RESPONSE_BYTES
        );
        assert!(read_response_body(Cursor::new(rejected)).is_err());
    }

    #[test]
    fn progress_callback_count_is_bounded_by_percentage() {
        use std::sync::{Arc, Mutex};

        let events = Arc::new(Mutex::new(Vec::new()));
        let captured_events = Arc::clone(&events);
        let source = Cursor::new(vec![0_u8; 1024 * 1024]);
        let mut reader = ProgressReader::new(source, 1024 * 1024, move |done, total| {
            captured_events.lock().unwrap().push((done, total));
        });
        let mut buffer = [0_u8; 1024];

        while reader.read(&mut buffer).unwrap() != 0 {}

        let events = events.lock().unwrap();

        assert!(events.len() <= 101);
        assert_eq!(events.last(), Some(&(1024 * 1024, 1024 * 1024)));
    }

    #[test]
    fn custom_header_validation_rejects_injection() {
        assert!(validate_upload_header("Authorization", "Bearer secret").is_ok());
        assert!(validate_upload_header("", "secret").is_err());
        assert!(validate_upload_header("Authorization\r\nInjected", "secret").is_err());
        assert!(validate_upload_header("Authorization", "secret\r\nInjected: yes").is_err());
        assert!(validate_upload_header("Content-Length", "1").is_err());
        assert!(validate_upload_header("Host", "elsewhere.example").is_err());
        assert!(validate_upload_header("Content-Type", "text/plain").is_err());
        assert!(validate_upload_header("Expect", "100-continue").is_err());
    }

    #[test]
    fn generated_copies_never_replace_an_existing_capture() {
        let directory = tempfile::tempdir().unwrap();
        let mut payload = UploadPayload::from_bytes(
            vec![1, 2, 3],
            "sample.png".to_owned(),
            "image/png".to_owned(),
        );

        let first = payload.save_generated_copy(directory.path()).unwrap();
        let second = payload.save_generated_copy(directory.path()).unwrap();

        assert_eq!(first.file_name().unwrap(), "sample.png");
        assert_eq!(second.file_name().unwrap(), "sample-1.png");
        assert_eq!(std::fs::read(first).unwrap(), [1, 2, 3]);
        assert_eq!(std::fs::read(second).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn generated_reader_is_rewound_after_each_local_copy() {
        let directory = tempfile::tempdir().unwrap();
        let mut payload = UploadPayload::from_reader(
            Cursor::new(vec![4, 5, 6]),
            3,
            "sample.webp".to_owned(),
            "image/webp".to_owned(),
        );

        let first = payload.save_generated_copy(directory.path()).unwrap();
        let second = payload.save_generated_copy(directory.path()).unwrap();

        assert_eq!(std::fs::read(first).unwrap(), [4, 5, 6]);
        assert_eq!(std::fs::read(second).unwrap(), [4, 5, 6]);
    }
}
