use anyhow::Result;
use reqwest::Url;

use super::{Provider, ResponseMetadata};
use crate::upload::{UploadReceipt, UploadTarget, parse_uploader_url, plain_receipt};

const UPLOAD_URL: &str = "https://uguu.se/upload?output=text";

#[derive(Debug)]
pub(super) struct Uguu;

impl Provider for Uguu {
    const NAME: &'static str = "Uguu";
    const FIELD: &'static str = "files[]";

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn parse(
        body: &[u8],
        _target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        let mut receipt =
            plain_receipt(body, metadata.filename, metadata.size_bytes, String::new())?;

        "3 hours (host policy)".clone_into(&mut receipt.expires_at);
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_distinguishes_host_policy_from_a_confirmed_expiry() {
        let receipt = Uguu::parse(
            b"https://files.example.test/capture.png",
            &UploadTarget::default(),
            ResponseMetadata {
                filename: "capture.png".to_owned(),
                size_bytes: 42,
                delete_header: "",
            },
        )
        .unwrap();

        assert_eq!(receipt.expires_at, "3 hours (host policy)");
        assert!(receipt.delete_url.is_empty());
    }

    #[test]
    fn endpoint_uses_the_current_upload_route() {
        let endpoint = Uguu::endpoint(&UploadTarget::default(), 60).unwrap();

        assert_eq!(endpoint.as_str(), "https://uguu.se/upload?output=text");
    }
}
