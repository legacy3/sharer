use anyhow::{Context, Result};
use reqwest::{Url, header::HeaderMap};
use serde::Deserialize;

use super::{CredentialRequirement, Provider, ResponseMetadata};
use crate::upload::{UploadReceipt, UploadTarget, parse_uploader_url};

const UPLOAD_URL: &str = "https://api.imgur.com/3/upload";
const DELETE_URL: &str = "https://imgur.com/delete/";

#[derive(Debug)]
pub(super) struct Imgur;

impl Provider for Imgur {
    const NAME: &'static str = "Imgur";
    const FIELD: &'static str = "image";
    const CREDENTIAL: CredentialRequirement = CredentialRequirement::Required;
    const IMAGES_ONLY: bool = true;

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn headers(target: &UploadTarget) -> Result<HeaderMap> {
        let mut value =
            reqwest::header::HeaderValue::from_str(&format!("Client-ID {}", target.credential))
                .context("invalid Imgur client ID")?;

        value.set_sensitive(true);
        let mut headers = HeaderMap::with_capacity(1);

        headers.insert(reqwest::header::AUTHORIZATION, value);
        Ok(headers)
    }

    fn parse(
        body: &[u8],
        _target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        let response: ImgurEnvelope =
            serde_json::from_slice(body).context("Imgur returned invalid JSON")?;

        anyhow::ensure!(response.success, "Imgur rejected the upload");

        Ok(UploadReceipt {
            original_name: metadata.filename,
            size_bytes: metadata.size_bytes,
            link: response.data.link,
            delete_url: format!("{DELETE_URL}{}", response.data.delete_hash),
            expires_at: String::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct ImgurEnvelope {
    success: bool,
    data: ImgurData,
}

#[derive(Debug, Deserialize)]
struct ImgurData {
    link: String,
    #[serde(rename = "deletehash")]
    delete_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_public_and_deletion_links() {
        let target = UploadTarget::default();
        let receipt = Imgur::parse(
            br#"{"success":true,"data":{"link":"https://i.imgur.com/a.png","deletehash":"secret"}}"#,
            &target,
            ResponseMetadata {
                filename: "a.png".to_owned(),
                size_bytes: 42,
                delete_header: "",
            },
        )
        .unwrap();

        assert_eq!(receipt.link, "https://i.imgur.com/a.png");
        assert_eq!(receipt.delete_url, "https://imgur.com/delete/secret");
    }
}
