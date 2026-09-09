use anyhow::{Context, Result};
use reqwest::{Url, header::HeaderMap};
use serde::Deserialize;

use super::{Provider, ResponseMetadata};
use crate::upload::{
    UploadReceipt, UploadTarget, custom_header_map, endpoint_with_lifetime, parse_uploader_url,
};

#[derive(Debug)]
pub(super) struct Custom;

impl Provider for Custom {
    const NAME: &'static str = "Custom";
    const FIELD: &'static str = "file";
    const SUPPORTS_LIFETIME: bool = true;

    fn endpoint(target: &UploadTarget, lifetime_seconds: u32) -> Result<Url> {
        let endpoint = parse_uploader_url(&target.custom_url)?;

        Ok(endpoint_with_lifetime(&endpoint, lifetime_seconds))
    }

    fn headers(target: &UploadTarget) -> Result<HeaderMap> {
        if target.header_name.is_empty() {
            Ok(HeaderMap::new())
        } else {
            custom_header_map(&target.header_name, &target.header_value)
        }
    }

    fn parse(
        body: &[u8],
        _target: &UploadTarget,
        _metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        Ok(serde_json::from_slice::<UploadEnvelope>(body)
            .context("uploader returned invalid JSON")?
            .data)
    }
}

#[derive(Debug, Deserialize)]
struct UploadEnvelope {
    data: UploadReceipt,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_contract_reads_data_link() {
        let response: UploadEnvelope = serde_json::from_str(
            r#"{"data":{"originalName":"a.png","size":123,"link":"https://files.example/a.png","deleteLink":"https://files.example/delete/a.png/token","expiresAt":"2026-09-09T00:00:00Z"}}"#,
        )
        .unwrap();

        assert_eq!(response.data.link, "https://files.example/a.png");
        assert_eq!(
            response.data.delete_url,
            "https://files.example/delete/a.png/token"
        );
    }
}
