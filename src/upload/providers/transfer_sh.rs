use anyhow::Result;
use reqwest::Url;

use super::{Provider, ResponseMetadata};
use crate::upload::{UploadReceipt, UploadTarget, parse_uploader_url, plain_receipt};

const UPLOAD_URL: &str = "https://transfer.sh";

#[derive(Debug)]
pub(super) struct TransferSh;

impl Provider for TransferSh {
    const NAME: &'static str = "transfer.sh";
    const FIELD: &'static str = "file";

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn parse(
        body: &[u8],
        _target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        plain_receipt(
            body,
            metadata.filename,
            metadata.size_bytes,
            metadata.delete_header.to_owned(),
        )
    }
}
