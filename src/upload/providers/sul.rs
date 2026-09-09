use anyhow::{Context, Result};
use reqwest::{Url, multipart};
use serde::Deserialize;

use super::{CredentialRequirement, Provider, ResponseMetadata};
use crate::upload::{UploadReceipt, UploadTarget, parse_uploader_url};

const UPLOAD_URL: &str = "https://s-ul.eu/api/v1/upload";
const DELETE_URL: &str = "https://s-ul.eu/delete.php";

#[derive(Debug)]
pub(super) struct Sul;

impl Provider for Sul {
    const NAME: &'static str = "s-ul";
    const FIELD: &'static str = "file";
    const CREDENTIAL: CredentialRequirement = CredentialRequirement::Required;

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn form(part: multipart::Part, target: &UploadTarget) -> multipart::Form {
        multipart::Form::new()
            .part(Self::FIELD, part)
            .text("wizard", "true")
            .text("key", target.credential.clone())
            .text("client", "ShareR")
    }

    fn parse(
        body: &[u8],
        target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        let response: SulResponse =
            serde_json::from_slice(body).context("s-ul returned invalid JSON")?;

        anyhow::ensure!(response.error.is_empty(), "s-ul: {}", response.error);
        let link = format!(
            "{}{}/{}{}",
            response.protocol, response.domain, response.filename, response.extension
        );
        let mut delete_url = Url::parse(DELETE_URL).context("invalid s-ul deletion URL")?;

        delete_url
            .query_pairs_mut()
            .append_pair("key", &target.credential)
            .append_pair("file", &response.filename);

        Ok(UploadReceipt {
            original_name: metadata.filename,
            size_bytes: metadata.size_bytes,
            link,
            delete_url: delete_url.into(),
            expires_at: String::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct SulResponse {
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    filename: String,
    #[serde(default)]
    extension: String,
    #[serde(default)]
    error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_public_and_authenticated_deletion_links() {
        let target = UploadTarget {
            credential: "private key".to_owned(),
            ..UploadTarget::default()
        };
        let receipt = Sul::parse(
            br#"{"protocol":"https://","domain":"s-ul.eu","filename":"abc","extension":".zip"}"#,
            &target,
            ResponseMetadata {
                filename: "archive.zip".to_owned(),
                size_bytes: 42,
                delete_header: "",
            },
        )
        .unwrap();

        assert_eq!(receipt.link, "https://s-ul.eu/abc.zip");
        assert_eq!(
            receipt.delete_url,
            "https://s-ul.eu/delete.php?key=private+key&file=abc"
        );
    }
}
