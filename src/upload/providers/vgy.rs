use anyhow::{Context, Result};
use reqwest::{Url, multipart};
use serde::Deserialize;

use super::{CredentialRequirement, Provider, ResponseMetadata};
use crate::upload::{UploadReceipt, UploadTarget, parse_uploader_url};

const UPLOAD_URL: &str = "https://vgy.me/upload";

#[derive(Debug)]
pub(super) struct Vgy;

impl Provider for Vgy {
    const NAME: &'static str = "vgy.me";
    const FIELD: &'static str = "file";
    const CREDENTIAL: CredentialRequirement = CredentialRequirement::Optional;
    const IMAGES_ONLY: bool = true;

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn form(part: multipart::Part, target: &UploadTarget) -> multipart::Form {
        let form = multipart::Form::new().part(Self::FIELD, part);

        if target.credential.is_empty() {
            form
        } else {
            form.text("userkey", target.credential.clone())
        }
    }

    fn parse(
        body: &[u8],
        _target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        let response: VgyResponse =
            serde_json::from_slice(body).context("vgy.me returned invalid JSON")?;

        anyhow::ensure!(!response.error, "vgy.me rejected the upload");

        Ok(UploadReceipt {
            original_name: metadata.filename,
            size_bytes: metadata.size_bytes,
            link: response.image,
            delete_url: response.delete,
            expires_at: String::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct VgyResponse {
    #[serde(default, alias = "Error")]
    error: bool,
    #[serde(alias = "Image")]
    image: String,
    #[serde(default, alias = "Delete")]
    delete: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_optional_deletion_link() {
        let receipt = Vgy::parse(
            br#"{"image":"https://vgy.me/a.png","delete":"https://vgy.me/delete/a"}"#,
            &UploadTarget::default(),
            ResponseMetadata {
                filename: "a.png".to_owned(),
                size_bytes: 42,
                delete_header: "",
            },
        )
        .unwrap();

        assert_eq!(receipt.link, "https://vgy.me/a.png");
        assert_eq!(receipt.delete_url, "https://vgy.me/delete/a");
    }
}
