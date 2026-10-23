//! Stream, verify, and publish an optional `FFmpeg` executable in application storage.

use std::{
    future::Future,
    io::{Read, Seek as _, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use futures_util::future::{Either, select};
use reqwest::Client;
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use super::EXECUTABLE;
use sharer::{
    proxy::preferred_upload_route,
    upload::{UploadRoute, ensure_tls_provider},
};

mod resolver;

const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;

pub(crate) struct DownloadSource {
    pub(crate) provider: &'static str,
    archive_url: &'static str,
    sha256: &'static str,
}

pub(crate) fn source() -> Result<DownloadSource> {
    source_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn source_for(os: &str, architecture: &str) -> Result<DownloadSource> {
    // Release URLs and vendor checksums are pinned together. Updating ShareR can
    // update this manifest without trusting a mutable upstream download alias.
    let (provider, archive_url, sha256) = match (os, architecture) {
        ("windows", "x86_64") => (
            "gyan.dev",
            "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-essentials_build.zip",
            "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba",
        ),

        ("macos", "x86_64") => (
            "ffmpeg.martin-riedl.de",
            "https://ffmpeg.martin-riedl.de/download/macos/amd64/1789931006_9.0.2/ffmpeg.zip",
            "7c6b4125b191cbf773832dc51f424cf2b6bb7da43007d1e066f95909e47cacd4",
        ),

        ("macos", "aarch64") => (
            "ffmpeg.martin-riedl.de",
            "https://ffmpeg.martin-riedl.de/download/macos/arm64/1789931890_9.0.2/ffmpeg.zip",
            "c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924",
        ),

        ("linux", "x86_64") => (
            "ffmpeg.martin-riedl.de",
            "https://ffmpeg.martin-riedl.de/download/linux/amd64/1789931100_9.0.2/ffmpeg.zip",
            "fa8ecf4abbd290d98f7d188b8649cc6b391ae209a98452be955a15aab1909d7f",
        ),

        ("linux", "aarch64") => (
            "ffmpeg.martin-riedl.de",
            "https://ffmpeg.martin-riedl.de/download/linux/arm64/1789931697_9.0.2/ffmpeg.zip",
            "93a76ae90db5474eecdf951a729857c64f3de23567228d6a7d5e6e8e3cd1021b",
        ),

        _ => anyhow::bail!(
            "Automatic FFmpeg download is not available for this platform or CPU; install FFmpeg manually"
        ),
    };

    Ok(DownloadSource {
        provider,
        archive_url,
        sha256,
    })
}

pub(crate) fn install(
    require_proxy: bool,
    cancelled: &AtomicBool,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    let directory = super::managed_directory()?;

    install_into(&directory, &source()?, require_proxy, cancelled, progress)
}

fn install_into(
    directory: &Path,
    source: &DownloadSource,
    require_proxy: bool,
    cancelled: &AtomicBool,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    check_cancelled(cancelled)?;
    std::fs::create_dir_all(directory)
        .context("failed to create the FFmpeg installation folder")?;
    let staging = tempfile::Builder::new()
        .prefix(".download-")
        .tempdir_in(directory)?;
    let mut archive = tempfile::tempfile_in(staging.path())?;

    run_download(download(
        source,
        require_proxy,
        cancelled,
        &mut archive,
        progress,
    ))?;
    archive.seek(SeekFrom::Start(0))?;
    let candidate = extract_executable(archive, staging.path(), cancelled)?;
    let candidate = candidate.into_temp_path();

    validate_encoder(&candidate, cancelled)?;
    check_cancelled(cancelled)?;
    std::fs::write(
        directory.join("SOURCE.txt"),
        format!(
            "FFmpeg downloaded from {}\nSHA-256: {}\nBuild provider: {}\n",
            source.archive_url, source.sha256, source.provider
        ),
    )?;
    let destination = directory.join(EXECUTABLE);

    candidate
        .persist_noclobber(&destination)
        .map_err(|error| error.error)
        .context("failed to publish FFmpeg; an existing executable was not replaced")?;

    Ok(destination)
}

fn run_download(operation: impl Future<Output = Result<()>>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    let result = runtime.block_on(operation);

    // System DNS may still be running in Tokio's blocking pool after its request
    // is cancelled. It owns no archive/staging handles; do not wait indefinitely
    // for it before returning control to the setup dialog.
    runtime.shutdown_timeout(Duration::from_millis(100));
    result
}

async fn download(
    source: &DownloadSource,
    require_proxy: bool,
    cancelled: &AtomicBool,
    output: &mut impl Write,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    ensure_tls_provider();
    let builder = Client::builder()
        .dns_resolver(resolver::InstallerResolver)
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(180))
        .user_agent(concat!("ShareR/", env!("CARGO_PKG_VERSION")));
    let builder = match preferred_upload_route(require_proxy)? {
        UploadRoute::Direct => builder,
        UploadRoute::Socks5(proxy) => builder.proxy(reqwest::Proxy::all(proxy)?),
    };
    let client = builder
        .build()
        .context("failed to create the FFmpeg download client")?;
    let mut response = cancellable(client.get(source.archive_url).send(), cancelled)
        .await??
        .error_for_status()?;
    let length = response.content_length();

    anyhow::ensure!(
        length.is_none_or(|size| size <= MAX_DOWNLOAD_BYTES),
        "FFmpeg download exceeds the 256 MiB limit"
    );
    let mut writer = DownloadWriter::new(output, length);

    while let Some(chunk) = cancellable(response.chunk(), cancelled).await?? {
        writer.push(&chunk, cancelled)?;
        progress(writer.total, length);
        tokio::task::yield_now().await;
    }

    anyhow::ensure!(
        writer.finish()? == source.sha256,
        "FFmpeg download checksum did not match; nothing was installed"
    );

    Ok(())
}

async fn cancellable<F>(future: F, cancelled: &AtomicBool) -> Result<F::Output>
where
    F: Future,
{
    check_cancelled(cancelled)?;
    let cancellation = async {
        while !cancelled.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };

    match select(Box::pin(future), Box::pin(cancellation)).await {
        Either::Left((result, _)) => Ok(result),
        Either::Right(_) => anyhow::bail!("FFmpeg download cancelled"),
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    anyhow::ensure!(
        !cancelled.load(Ordering::Acquire),
        "FFmpeg download cancelled"
    );

    Ok(())
}

struct DownloadWriter<W> {
    output: W,
    length: Option<u64>,
    total: u64,
    hash: Sha256,
}

impl<W> DownloadWriter<W>
where
    W: Write,
{
    fn new(output: W, length: Option<u64>) -> Self {
        Self {
            output,
            length,
            total: 0,
            hash: Sha256::new(),
        }
    }

    fn push(&mut self, chunk: &[u8], cancelled: &AtomicBool) -> Result<()> {
        check_cancelled(cancelled)?;
        self.total += u64::try_from(chunk.len())?;
        anyhow::ensure!(
            self.total <= MAX_DOWNLOAD_BYTES,
            "FFmpeg download exceeds the 256 MiB limit"
        );
        self.output.write_all(chunk)?;
        self.hash.update(chunk);

        Ok(())
    }

    fn finish(self) -> Result<String> {
        anyhow::ensure!(
            self.length.is_none_or(|length| length == self.total),
            "FFmpeg download was incomplete"
        );

        Ok(hex::encode(self.hash.finalize()))
    }
}

fn extract_executable(
    reader: impl Read + std::io::Seek,
    directory: &Path,
    cancelled: &AtomicBool,
) -> Result<NamedTempFile> {
    let mut archive = zip::ZipArchive::new(reader).context("invalid FFmpeg download archive")?;

    anyhow::ensure!(
        archive.len() <= MAX_ARCHIVE_ENTRIES,
        "FFmpeg archive contains too many entries"
    );
    let mut output = tempfile::Builder::new()
        .suffix(if cfg!(windows) { ".exe" } else { "" })
        .tempfile_in(directory)?;
    let mut found = false;

    for index in 0..archive.len() {
        check_cancelled(cancelled)?;
        let mut entry = archive.by_index(index)?;
        let Some(path) = entry.enclosed_name() else {
            continue;
        };

        if path.file_name() != Some(std::ffi::OsStr::new(EXECUTABLE)) {
            continue;
        }

        anyhow::ensure!(!found, "FFmpeg archive contains duplicate executables");
        anyhow::ensure!(
            !entry.is_dir() && !entry.is_symlink(),
            "FFmpeg archive executable is not a regular file"
        );
        anyhow::ensure!(
            entry.size() > 0 && entry.size() <= MAX_EXECUTABLE_BYTES,
            "FFmpeg executable exceeds the size limit"
        );
        let expected = entry.size();
        let mut buffer = vec![0; 64 * 1024];
        let mut written = 0_u64;

        loop {
            check_cancelled(cancelled)?;
            let count = entry.read(&mut buffer)?;

            if count == 0 {
                break;
            }

            written += u64::try_from(count)?;
            anyhow::ensure!(
                written <= MAX_EXECUTABLE_BYTES,
                "FFmpeg executable exceeds the size limit"
            );
            output.write_all(&buffer[..count])?;
        }

        anyhow::ensure!(
            written == expected,
            "FFmpeg archive executable was truncated"
        );
        found = true;
    }

    anyhow::ensure!(found, "FFmpeg executable was not found in the download");
    output.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        output
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }

    Ok(output)
}

fn validate_encoder(executable: &Path, cancelled: &AtomicBool) -> Result<()> {
    let diagnostic = tempfile::tempfile()?;
    let mut command = Command::new(executable);

    command
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "color=s=2x2:d=0.04",
            "-frames:v",
            "1",
            "-threads",
            "1",
            "-c:v",
            "libx264",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(diagnostic));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .context("downloaded FFmpeg could not be started on this system")?;
    let started = Instant::now();
    let result = loop {
        if let Err(error) = check_cancelled(cancelled) {
            break Err(error);
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "downloaded FFmpeg does not support the required H.264 encoder"
                    ))
                };
            }

            Err(error) => break Err(error.into()),

            Ok(None) => {}
        }

        if started.elapsed() >= Duration::from_secs(10) {
            break Err(anyhow::anyhow!("downloaded FFmpeg validation timed out"));
        }

        thread::sleep(Duration::from_millis(20));
    };

    let _ = child.kill();
    let _ = child.wait();

    result
}

#[cfg(test)]
mod tests;
