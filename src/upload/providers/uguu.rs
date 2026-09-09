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
        plain_receipt(body, metadata.filename, metadata.size_bytes, String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_uses_the_current_upload_route() {
        let endpoint = Uguu::endpoint(&UploadTarget::default(), 60).unwrap();

        assert_eq!(endpoint.as_str(), "https://uguu.se/upload?output=text");
    }
}
