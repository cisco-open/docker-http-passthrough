// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::Stream;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::Deserialize;
use std::env;
use std::pin::Pin;
use tracing::{debug, info};

/// Base URL for the container registry V2 API.
/// Default: Docker Hub (registry-1.docker.io)
static REGISTRY_URL: Lazy<String> = Lazy::new(|| {
    env::var("REGISTRY_URL").unwrap_or_else(|_| "https://registry-1.docker.io".to_string())
});

/// URL for the registry authentication endpoint.
/// Default: Docker Hub auth service
static AUTH_URL: Lazy<String> = Lazy::new(|| {
    env::var("REGISTRY_AUTH_URL").unwrap_or_else(|_| "https://auth.docker.io/token".to_string())
});

/// Service name used in authentication scope requests.
/// Default: Docker Hub service name
static REGISTRY_SERVICE: Lazy<String> = Lazy::new(|| {
    env::var("REGISTRY_SERVICE").unwrap_or_else(|_| "registry.docker.io".to_string())
});

/// Display name for the registry (used in telemetry/logging).
/// Default: docker.io
pub static REGISTRY_NAME: Lazy<String> = Lazy::new(|| {
    env::var("REGISTRY_NAME").unwrap_or_else(|_| "docker.io".to_string())
});

#[derive(Debug, Deserialize)]
struct AuthResponse {
    token: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Manifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: Option<String>,
    pub config: Option<ManifestConfig>,
    pub layers: Option<Vec<Layer>>,
    pub manifests: Option<Vec<ManifestEntry>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ManifestConfig {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub size: u64,
    pub digest: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Layer {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub size: u64,
    pub digest: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ManifestEntry {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub size: u64,
    pub digest: String,
    pub platform: Option<Platform>,
}

#[derive(Debug, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
}

pub struct RegistryClient {
    client: Client,
}

impl RegistryClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    async fn get_token(&self, image: &str) -> Result<String> {
        let scope = format!("repository:{}:pull", image);
        let url = format!(
            "{}?service={}&scope={}",
            AUTH_URL.as_str(),
            REGISTRY_SERVICE.as_str(),
            scope
        );

        debug!("Fetching auth token for {}", image);
        let response: AuthResponse = self.client.get(&url).send().await?.json().await?;
        Ok(response.token)
    }

    pub async fn get_manifest(&self, image: &str, tag: &str) -> Result<(Manifest, Option<String>)> {
        let token = self.get_token(image).await?;
        let url = format!("{}/v2/{}/manifests/{}", REGISTRY_URL.as_str(), image, tag);

        info!("Fetching manifest for {}:{}", image, tag);
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header(
                "Accept",
                "application/vnd.docker.distribution.manifest.v2+json, \
                 application/vnd.docker.distribution.manifest.list.v2+json, \
                 application/vnd.oci.image.manifest.v1+json, \
                 application/vnd.oci.image.index.v1+json",
            )
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Failed to fetch manifest: {} - {}",
                status,
                body
            ));
        }

        let digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let manifest: Manifest = response.json().await?;
        Ok((manifest, digest))
    }

    pub async fn get_manifest_by_digest(
        &self,
        image: &str,
        digest: &str,
    ) -> Result<(Manifest, Option<String>)> {
        let token = self.get_token(image).await?;
        let url = format!("{}/v2/{}/manifests/{}", REGISTRY_URL.as_str(), image, digest);

        info!("Fetching manifest by digest for {}", image);
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header(
                "Accept",
                "application/vnd.docker.distribution.manifest.v2+json, \
                 application/vnd.oci.image.manifest.v1+json",
            )
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Failed to fetch manifest by digest: {} - {}",
                status,
                body
            ));
        }

        let digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let manifest: Manifest = response.json().await?;
        Ok((manifest, digest))
    }

    pub async fn get_config(&self, image: &str, digest: &str) -> Result<Vec<u8>> {
        let token = self.get_token(image).await?;
        let url = format!("{}/v2/{}/blobs/{}", REGISTRY_URL.as_str(), image, digest);

        info!("Fetching config blob for {}", image);
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Failed to fetch config: {} - {}", status, body));
        }

        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }

    pub async fn get_layer_stream(
        &self,
        image: &str,
        digest: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>> {
        let token = self.get_token(image).await?;
        let url = format!("{}/v2/{}/blobs/{}", REGISTRY_URL.as_str(), image, digest);

        info!("Streaming layer {} for {}", digest, image);
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Failed to fetch layer: {} - {}", status, body));
        }

        Ok(Box::pin(response.bytes_stream()))
    }

    /// Find the amd64/linux manifest from a manifest list
    pub async fn resolve_amd64_manifest(
        &self,
        image: &str,
        tag: &str,
    ) -> Result<(Manifest, Option<String>)> {
        let (manifest, manifest_digest) = self.get_manifest(image, tag).await?;

        // Check if this is a manifest list (multi-arch)
        if let Some(manifests) = &manifest.manifests {
            info!("Found manifest list, looking for amd64/linux");
            for entry in manifests {
                if let Some(platform) = &entry.platform {
                    if platform.architecture == "amd64" && platform.os == "linux" {
                        info!("Found amd64/linux manifest: {}", entry.digest);
                        let digest = entry.digest.clone();
                        let (resolved, _) = self.get_manifest_by_digest(image, &digest).await?;
                        return Ok((resolved, Some(digest)));
                    }
                }
            }
            return Err(anyhow!("No amd64/linux manifest found in manifest list"));
        }

        // Single manifest, return as-is
        Ok((manifest, manifest_digest))
    }
}
