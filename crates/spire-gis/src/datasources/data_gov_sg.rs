// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! data.gov.sg data-source driver.
//!
//! Ports the download flow from `ImportActor::import_datagov` (poll-download →
//! signed URL → GeoJSON text) behind the [`DataSourceDriver`] trait so it can
//! be configured, discovered, and fetched without importing.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::{analyze_geojson, DataSourceDriver, DatasetInfo, DatasetPayload};

/// Stable driver kind for data.gov.sg.
pub const KIND: &str = "data-gov-sg";

const POLL_TEMPLATE: &str =
    "https://api-open.data.gov.sg/v1/public/api/datasets/{id}/poll-download";
const MAX_POLLS: u32 = 8;
const POLL_DELAY_SECS: u64 = 3;

/// Extracts `config.dataset_id` (the only required setting for this provider).
pub fn dataset_id(config: &Value) -> Result<String, String> {
    config
        .get("dataset_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| "data.gov.sg requires config.dataset_id".to_string())
}

/// The data.gov.sg driver (stateless; config carries the dataset id).
pub struct DataGovSgDriver;

impl DataGovSgDriver {
    /// Poll for a ready download URL (data.gov.sg prepares exports async).
    async fn poll_download_url(
        &self,
        client: &reqwest::Client,
        dataset_id: &str,
    ) -> Result<String, String> {
        let poll_url = POLL_TEMPLATE.replace("{id}", dataset_id);
        for _ in 0..MAX_POLLS {
            let resp = client
                .get(&poll_url)
                .send()
                .await
                .map_err(|e| format!("data.gov.sg poll failed: {e}"))?;
            let body: Value = resp
                .json()
                .await
                .map_err(|e| format!("data.gov.sg poll body: {e}"))?;
            if let Some(u) = body
                .get("data")
                .and_then(|d| d.get("url"))
                .and_then(|u| u.as_str())
            {
                return Ok(u.to_string());
            }
            tokio::time::sleep(Duration::from_secs(POLL_DELAY_SECS)).await;
        }
        Err("data.gov.sg poll-download never became ready".to_string())
    }

    /// Download the dataset as UTF-8 GeoJSON text.
    async fn download_geojson_text(&self, config: &Value) -> Result<String, String> {
        let id = dataset_id(config)?;
        let client = reqwest::Client::new();
        let url = self.poll_download_url(&client, &id).await?;
        let bytes = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("data.gov.sg download failed: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("data.gov.sg download body: {e}"))?;
        std::str::from_utf8(&bytes)
            .map(|s| s.to_string())
            .map_err(|_| {
                "dataset is not utf-8 text (zip/shapefile?); GeoJSON text only for now".to_string()
            })
    }
}

#[async_trait]
impl DataSourceDriver for DataGovSgDriver {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn discover(&self, config: &Value) -> Result<DatasetInfo, String> {
        let id = dataset_id(config)?;
        let text = self.download_geojson_text(config).await?;
        let label = config
            .get("label")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("data.gov.sg dataset {id}"));
        // Full parse for the prototype: discovery is offline-only (no write).
        analyze_geojson(&text, label, format!("data.gov.sg dataset {id}"))
    }

    async fn fetch(&self, config: &Value) -> Result<DatasetPayload, String> {
        let text = self.download_geojson_text(config).await?;
        Ok(DatasetPayload::GeoJsonText(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_id_required_and_read() {
        assert!(dataset_id(&Value::Null).is_err());
        assert!(dataset_id(&serde_json::json!({})).is_err());
        assert_eq!(
            dataset_id(&serde_json::json!({ "dataset_id": "d_abc" })).unwrap(),
            "d_abc"
        );
    }
}
