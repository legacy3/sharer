//! Non-blocking GitHub release checks derived from package repository metadata.

use std::{io::Read as _, sync::mpsc, time::Duration};

use anyhow::{Context as _, Result};
use reqwest::{StatusCode, blocking::Client, header};
use semver::Version;
use serde::Deserialize;
use sharer::upload::ensure_tls_provider;

const MAX_RELEASE_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) enum UpdateCheck {
    Available(UpdateInfo),
    Current,
    Failed(String),
}

#[derive(Debug)]
pub(super) struct UpdateInfo {
    pub(super) version: String,
    pub(super) url: String,
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
}

pub(super) fn spawn_check(
    tor_proxy: Option<String>,
    require_tor: bool,
) -> Result<mpsc::Receiver<UpdateCheck>> {
    let (sender, receiver) = mpsc::channel();

    if require_tor && tor_proxy.is_none() {
        let _ = sender.send(UpdateCheck::Failed(
            "Skipped because Tor is required but unavailable".to_owned(),
        ));

        return Ok(receiver);
    }

    std::thread::Builder::new()
        .name("sharer-update-check".to_owned())
        .spawn(move || {
            let result = check(tor_proxy.as_deref());
            let event = match result {
                Ok(Some(info)) => UpdateCheck::Available(info),
                Ok(None) => UpdateCheck::Current,
                Err(error) => UpdateCheck::Failed(format!("{error:#}")),
            };

            let _ = sender.send(event);
        })
        .context("failed to start update checker")?;

    Ok(receiver)
}

fn check(tor_proxy: Option<&str>) -> Result<Option<UpdateInfo>> {
    let repository = url::Url::parse(env!("CARGO_PKG_REPOSITORY"))
        .context("package repository URL is invalid")?;
    let repository_path = repository.path().trim_matches('/').to_owned();

    anyhow::ensure!(
        repository.host_str() == Some("github.com") && repository_path.split('/').count() == 2,
        "package repository must identify a GitHub owner and repository"
    );
    let mut endpoint = repository;

    endpoint
        .set_host(Some("api.github.com"))
        .context("could not select the GitHub API host")?;
    endpoint.set_path(&format!("/repos/{repository_path}/releases/latest"));
    ensure_tls_provider();
    let builder = Client::builder().timeout(Duration::from_secs(8));
    let builder = if let Some(proxy) = tor_proxy {
        builder.proxy(reqwest::Proxy::all(proxy).context("invalid Tor proxy URL")?)
    } else {
        builder
    };
    let client = builder.build().context("failed to create update client")?;
    let response = client
        .get(endpoint)
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(
            header::USER_AGENT,
            concat!("sharer/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .context("GitHub release check failed")?;

    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    response
        .error_for_status_ref()
        .context("GitHub release check returned an error")?;
    let limit = u64::try_from(MAX_RELEASE_RESPONSE_BYTES).unwrap_or(u64::MAX);
    let mut body = Vec::with_capacity(4_096);

    response
        .take(limit.saturating_add(1))
        .read_to_end(&mut body)
        .context("failed to read GitHub release response")?;
    anyhow::ensure!(
        body.len() <= MAX_RELEASE_RESPONSE_BYTES,
        "GitHub release response was too large"
    );
    let release = serde_json::from_slice::<LatestRelease>(&body)
        .context("GitHub release response was invalid")?;
    let latest = Version::parse(release.tag_name.trim_start_matches('v'))
        .context("latest release tag is not semantic versioning")?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("package version is not semantic versioning")?;

    Ok((latest > current).then_some(UpdateInfo {
        version: latest.to_string(),
        url: release.html_url,
    }))
}

#[cfg(test)]
mod tests {
    use semver::Version;

    #[test]
    fn semantic_versions_compare_release_order() {
        assert!(Version::parse("0.2.0").unwrap() > Version::parse("0.1.9").unwrap());
    }
}
