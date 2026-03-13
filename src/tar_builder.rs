// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

use anyhow::{anyhow, Result};
use flate2::read::GzDecoder;
use futures::StreamExt;
use opentelemetry::{
    global,
    trace::{Span, SpanKind, Status, Tracer, TraceContextExt},
    Context as OtelContext,
};
use serde::Serialize;
use std::io::Read;
use tar::Header;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

use crate::registry::{RegistryClient, REGISTRY_NAME};

const TAR_BLOCK_SIZE: usize = 512;

#[derive(Serialize)]
struct ManifestJson {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Vec<String>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

pub struct ImageTarBuilder {
    client: RegistryClient,
    image: String,
    tag: String,
}

impl ImageTarBuilder {
    pub fn new(client: RegistryClient, image: String, tag: String) -> Self {
        Self { client, image, tag }
    }

    pub async fn build_tar<W: tokio::io::AsyncWrite + Unpin + Send>(
        self,
        writer: W,
    ) -> Result<()> {
        self.build_tar_with_context(writer, OtelContext::current()).await
    }

    pub async fn build_tar_with_context<W: tokio::io::AsyncWrite + Unpin + Send>(
        self,
        mut writer: W,
        parent_cx: OtelContext,
    ) -> Result<()> {
        info!("Starting tar build for {}:{}", self.image, self.tag);
        
        let tracer = global::tracer("docker-http-passthrough");

        // Get the amd64 manifest
        let (manifest, _resolved_manifest_digest) = {
            let mut span = tracer
                .span_builder("resolve_manifest")
                .with_kind(SpanKind::Client)
                .start_with_context(&tracer, &parent_cx);
            span.set_attribute(opentelemetry::KeyValue::new(
                "container.image.name",
                format!("{}/{}", REGISTRY_NAME.as_str(), self.image),
            ));
            span.set_attribute(opentelemetry::KeyValue::new("container.image.tag", self.tag.clone()));
            
            let result = self
                .client
                .resolve_amd64_manifest(&self.image, &self.tag)
                .await;
            match &result {
                Ok((_, digest_opt)) => {
                    if let Some(digest) = digest_opt {
                        span.set_attribute(opentelemetry::KeyValue::new(
                            "oci.manifest.digest",
                            digest.clone(),
                        ));
                        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
                            "oci.manifest.digest",
                            digest.clone(),
                        ));
                    }
                    span.set_status(Status::Ok)
                }
                Err(e) => span.set_status(Status::error(e.to_string())),
            }
            span.end();
            result?
        };

        let config_descriptor = manifest
            .config
            .as_ref()
            .ok_or_else(|| anyhow!("Manifest missing config"))?;

        let layers = manifest
            .layers
            .as_ref()
            .ok_or_else(|| anyhow!("Manifest missing layers"))?;

        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "oci.image.layers.count",
            layers.len() as i64,
        ));
        let layers_compressed_size: u64 = layers.iter().map(|l| l.size).sum();
        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "oci.image.layers.compressed_size",
            layers_compressed_size as i64,
        ));

        info!(
            "Manifest has {} layers, config: {}",
            layers.len(),
            config_descriptor.digest
        );

        // Fetch the config blob
        let config_bytes = {
            let mut span = tracer
                .span_builder("fetch_config_blob")
                .with_kind(SpanKind::Client)
                .start_with_context(&tracer, &parent_cx);
            span.set_attribute(opentelemetry::KeyValue::new(
                "oci.image.config.digest",
                config_descriptor.digest.clone(),
            ));
            
            let result = self.client.get_config(&self.image, &config_descriptor.digest).await;
            match &result {
                Ok(bytes) => {
                    span.set_attribute(opentelemetry::KeyValue::new("http.response.body.size", bytes.len() as i64));
                    span.set_status(Status::Ok);
                }
                Err(e) => span.set_status(Status::error(e.to_string())),
            }
            span.end();
            result?
        };

        // Calculate config filename (sha256 hash without prefix)
        let config_hash = config_descriptor
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&config_descriptor.digest);
        let config_filename = format!("{}.json", config_hash);
        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "container.image.id",
            config_descriptor.digest.clone(),
        ));
        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "oci.image.config.digest",
            config_descriptor.digest.clone(),
        ));

        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "oci.image.ref",
            format!("{}/{}:{}", REGISTRY_NAME.as_str(), self.image, self.tag),
        ));

        // Build layer filenames for manifest.json
        let layer_filenames: Vec<String> = layers
            .iter()
            .map(|l| {
                let hash = l.digest.strip_prefix("sha256:").unwrap_or(&l.digest);
                format!("{}/layer.tar", hash)
            })
            .collect();

        // Create manifest.json content
        let repo_tag = if self.image.starts_with("library/") {
            format!("{}:{}", self.image.strip_prefix("library/").unwrap(), self.tag)
        } else {
            format!("{}:{}", self.image, self.tag)
        };

        let manifest_json = vec![ManifestJson {
            config: config_filename.clone(),
            repo_tags: vec![repo_tag],
            layers: layer_filenames.clone(),
        }];

        let manifest_json_bytes = serde_json::to_vec_pretty(&manifest_json)?;

        // Write manifest.json entry
        write_tar_entry(&mut writer, "manifest.json", &manifest_json_bytes).await?;
        debug!("Added manifest.json to tar");

        // Write config file entry
        write_tar_entry(&mut writer, &config_filename, &config_bytes).await?;
        debug!("Added config {} to tar", config_filename);
        
        writer.flush().await?;
        info!("Sent manifest.json and config to client");

        let mut layers_decompressed_size: u64 = 0;

        // Now stream each layer
        for (idx, layer) in layers.iter().enumerate() {
            info!(
                "Processing layer {}/{}: {} ({} bytes)",
                idx + 1,
                layers.len(),
                layer.digest,
                layer.size
            );

            let layer_hash = layer
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&layer.digest);
            let layer_dir = layer_hash.to_string();

            // Create layer directory entry
            write_tar_dir(&mut writer, &format!("{}/", layer_dir)).await?;

            // Download and decompress layer with tracing
            let layer_data = {
                let mut span = tracer
                    .span_builder(format!("download_layer_{}", idx + 1))
                    .with_kind(SpanKind::Client)
                    .start_with_context(&tracer, &parent_cx);
                span.set_attribute(opentelemetry::KeyValue::new("oci.layer.index", (idx + 1) as i64));
                span.set_attribute(opentelemetry::KeyValue::new("oci.layer.digest", layer.digest.clone()));
                span.set_attribute(opentelemetry::KeyValue::new("oci.layer.compressed_size", layer.size as i64));
                
                let result = self.download_and_decompress_layer(&layer.digest, &layer.media_type).await;
                match &result {
                    Ok(data) => {
                        layers_decompressed_size = layers_decompressed_size.saturating_add(data.len() as u64);
                        span.set_attribute(opentelemetry::KeyValue::new("oci.layer.decompressed_size", data.len() as i64));
                        span.set_status(Status::Ok);
                    }
                    Err(e) => span.set_status(Status::error(e.to_string())),
                }
                span.end();
                result?
            };

            // Create VERSION file for the layer
            write_tar_entry(&mut writer, &format!("{}/VERSION", layer_dir), b"1.0").await?;

            // Create json file for the layer (minimal)
            let layer_json = serde_json::json!({
                "id": layer_hash,
                "created": "1970-01-01T00:00:00Z"
            });
            let layer_json_bytes = serde_json::to_vec(&layer_json)?;
            write_tar_entry(&mut writer, &format!("{}/json", layer_dir), &layer_json_bytes).await?;

            // Add layer.tar - stream in chunks with tracing
            {
                let mut span = tracer
                    .span_builder(format!("write_layer_tar_{}", idx + 1))
                    .with_kind(SpanKind::Internal)
                    .start_with_context(&tracer, &parent_cx);
                span.set_attribute(opentelemetry::KeyValue::new("oci.layer.index", (idx + 1) as i64));
                span.set_attribute(opentelemetry::KeyValue::new("oci.layer.tar_size", layer_data.len() as i64));
                
                let result = write_tar_entry_chunked(&mut writer, &format!("{}/layer.tar", layer_dir), &layer_data).await;
                match &result {
                    Ok(_) => span.set_status(Status::Ok),
                    Err(e) => span.set_status(Status::error(e.to_string())),
                }
                span.end();
                result?;
            }

            info!("Completed layer {}/{}", idx + 1, layers.len());
        }

        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "oci.image.layers.decompressed_size",
            layers_decompressed_size as i64,
        ));

        // Write tar footer (two 512-byte zero blocks)
        let footer = [0u8; 1024];
        writer.write_all(&footer).await?;
        writer.flush().await?;

        info!("Tar build complete for {}:{}", self.image, self.tag);
        Ok(())
    }

    async fn download_and_decompress_layer(
        &self,
        digest: &str,
        media_type: &str,
    ) -> Result<Vec<u8>> {
        info!("Downloading layer: {}", digest);

        let mut stream = self.client.get_layer_stream(&self.image, digest).await?;

        // Collect all bytes from the stream
        let mut compressed_data = Vec::new();
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            compressed_data.extend_from_slice(&chunk);
        }

        info!(
            "Downloaded {} bytes for layer {}",
            compressed_data.len(),
            digest
        );

        // Check if we need to decompress
        let is_gzipped = media_type.contains("gzip")
            || media_type.contains("tar+gzip")
            || compressed_data.starts_with(&[0x1f, 0x8b]);

        let decompressed = if is_gzipped {
            info!("Decompressing gzipped layer");
            let mut decoder = GzDecoder::new(&compressed_data[..]);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)?;
            decompressed
        } else {
            compressed_data
        };

        info!("Layer decompressed to {} bytes", decompressed.len());
        Ok(decompressed)
    }
}

/// Write a tar file entry header and data directly (no Builder)
async fn write_tar_entry<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    path: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_path(path)?;
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();

    // Write header (512 bytes)
    writer.write_all(header.as_bytes()).await?;
    
    // Write data
    writer.write_all(data).await?;
    
    // Pad to 512-byte boundary
    let remainder = data.len() % TAR_BLOCK_SIZE;
    if remainder > 0 {
        let padding = TAR_BLOCK_SIZE - remainder;
        writer.write_all(&vec![0u8; padding]).await?;
    }
    
    Ok(())
}

/// Write a tar file entry in chunks for large files
async fn write_tar_entry_chunked<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    path: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_path(path)?;
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();

    // Write header (512 bytes)
    writer.write_all(header.as_bytes()).await?;
    
    // Write data in chunks
    for chunk in data.chunks(64 * 1024) {
        writer.write_all(chunk).await?;
        writer.flush().await?;
    }
    
    // Pad to 512-byte boundary
    let remainder = data.len() % TAR_BLOCK_SIZE;
    if remainder > 0 {
        let padding = TAR_BLOCK_SIZE - remainder;
        writer.write_all(&vec![0u8; padding]).await?;
    }
    
    Ok(())
}

/// Write a tar directory entry
async fn write_tar_dir<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    path: &str,
) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_path(path)?;
    header.set_size(0);
    header.set_mode(0o755);
    header.set_mtime(0);
    header.set_cksum();

    // Write header (512 bytes)
    writer.write_all(header.as_bytes()).await?;
    
    Ok(())
}
