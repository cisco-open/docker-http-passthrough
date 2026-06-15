// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

use anyhow::{anyhow, Result};
use flate2::read::GzDecoder;
use futures::StreamExt;
use opentelemetry::{
    global,
    trace::{Span, SpanKind, Status, TraceContextExt, Tracer},
    Context as OtelContext,
};
use serde::Serialize;
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use tar::Header;
use tempfile::NamedTempFile;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, info};

use crate::registry::{Layer, RegistryClient};

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

struct PreparedLayer {
    digest: String,
    dir: String,
    tar_size: u64,
    file: NamedTempFile,
}

struct PreparedImage {
    image: String,
    tag: String,
    config_filename: String,
    config_bytes: Vec<u8>,
    manifest_json_bytes: Vec<u8>,
    layers: Vec<PreparedLayer>,
}

pub struct ImageTarBuilder {
    image: PreparedImage,
}

impl ImageTarBuilder {
    pub async fn prepare_with_context(
        client: RegistryClient,
        image: String,
        tag: String,
        parent_cx: OtelContext,
    ) -> Result<Self> {
        info!("Preparing tar build for {}:{}", image, tag);

        let registry_name = client.registry_name().to_string();
        let tracer = global::tracer("docker-http-passthrough");

        let (manifest, _resolved_manifest_digest) = {
            let span = tracer
                .span_builder("resolve_manifest")
                .with_kind(SpanKind::Client)
                .start_with_context(&tracer, &parent_cx);
            let span_cx = parent_cx.with_span(span);
            span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                "container.image.name",
                format!("{}/{}", registry_name, image),
            ));
            span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                "container.image.tag",
                tag.clone(),
            ));

            let result = client.resolve_amd64_manifest(&image, &tag, &span_cx).await;
            match &result {
                Ok((_, digest_opt)) => {
                    if let Some(digest) = digest_opt {
                        span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                            "oci.manifest.digest",
                            digest.clone(),
                        ));
                        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
                            "oci.manifest.digest",
                            digest.clone(),
                        ));
                    }
                    span_cx.span().set_status(Status::Ok)
                }
                Err(e) => span_cx.span().set_status(Status::error(e.to_string())),
            }
            span_cx.span().end();
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

        let config_bytes = {
            let span = tracer
                .span_builder("fetch_config_blob")
                .with_kind(SpanKind::Client)
                .start_with_context(&tracer, &parent_cx);
            let span_cx = parent_cx.with_span(span);
            span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                "oci.image.config.digest",
                config_descriptor.digest.clone(),
            ));

            let result = client
                .get_config_with_context(&image, &config_descriptor.digest, &span_cx)
                .await;
            match &result {
                Ok(bytes) => {
                    span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                        "http.response.body.size",
                        bytes.len() as i64,
                    ));
                    span_cx.span().set_status(Status::Ok);
                }
                Err(e) => span_cx.span().set_status(Status::error(e.to_string())),
            }
            span_cx.span().end();
            result?
        };

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
            format!("{}/{}:{}", registry_name, image, tag),
        ));

        let mut prepared_layers = Vec::with_capacity(layers.len());
        let mut layers_decompressed_size = 0_u64;

        for (idx, layer) in layers.iter().enumerate() {
            info!(
                "Preparing layer {}/{}: {} ({} bytes)",
                idx + 1,
                layers.len(),
                layer.digest,
                layer.size
            );

            let span = tracer
                .span_builder(format!("download_layer_{}", idx + 1))
                .with_kind(SpanKind::Client)
                .start_with_context(&tracer, &parent_cx);
            let span_cx = parent_cx.with_span(span);
            span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                "oci.layer.index",
                (idx + 1) as i64,
            ));
            span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                "oci.layer.digest",
                layer.digest.clone(),
            ));
            span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                "oci.layer.compressed_size",
                layer.size as i64,
            ));

            let result = Self::download_and_prepare_layer(&client, &image, layer, &span_cx).await;
            match &result {
                Ok((_, tar_size)) => {
                    layers_decompressed_size = layers_decompressed_size.saturating_add(*tar_size);
                    span_cx.span().set_attribute(opentelemetry::KeyValue::new(
                        "oci.layer.decompressed_size",
                        *tar_size as i64,
                    ));
                    span_cx.span().set_status(Status::Ok);
                }
                Err(e) => span_cx.span().set_status(Status::error(e.to_string())),
            }
            span_cx.span().end();

            let (file, tar_size) = result?;
            let layer_hash = layer
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&layer.digest)
                .to_string();

            prepared_layers.push(PreparedLayer {
                digest: layer.digest.clone(),
                dir: layer_hash,
                tar_size,
                file,
            });
        }

        parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "oci.image.layers.decompressed_size",
            layers_decompressed_size as i64,
        ));

        let layer_filenames: Vec<String> = prepared_layers
            .iter()
            .map(|layer| format!("{}/layer.tar", layer.dir))
            .collect();

        let repo_tag = if image.starts_with("library/") {
            format!("{}:{}", image.strip_prefix("library/").unwrap(), tag)
        } else {
            format!("{}:{}", image, tag)
        };

        let manifest_json = vec![ManifestJson {
            config: config_filename.clone(),
            repo_tags: vec![repo_tag],
            layers: layer_filenames,
        }];

        let prepared = PreparedImage {
            image,
            tag,
            config_filename,
            config_bytes,
            manifest_json_bytes: serde_json::to_vec_pretty(&manifest_json)?,
            layers: prepared_layers,
        };

        info!("Prepared tar build for {}:{}", prepared.image, prepared.tag);
        Ok(Self { image: prepared })
    }

    pub async fn build_tar_with_context<W: AsyncWrite + Unpin + Send>(
        self,
        mut writer: W,
        parent_cx: OtelContext,
    ) -> Result<()> {
        let prepared = self.image;
        info!(
            "Starting tar stream for {}:{}",
            prepared.image, prepared.tag
        );

        write_tar_entry(&mut writer, "manifest.json", &prepared.manifest_json_bytes).await?;
        debug!("Added manifest.json to tar");

        write_tar_entry(
            &mut writer,
            &prepared.config_filename,
            &prepared.config_bytes,
        )
        .await?;
        debug!("Added config {} to tar", prepared.config_filename);

        writer.flush().await?;
        info!("Sent manifest.json and config to client");

        let tracer = global::tracer("docker-http-passthrough");

        for (idx, layer) in prepared.layers.iter().enumerate() {
            info!(
                "Streaming layer {}/{}: {}",
                idx + 1,
                prepared.layers.len(),
                layer.digest
            );

            write_tar_dir(&mut writer, &format!("{}/", layer.dir)).await?;
            write_tar_entry(&mut writer, &format!("{}/VERSION", layer.dir), b"1.0").await?;

            let layer_json = serde_json::json!({
                "id": layer.dir,
                "created": "1970-01-01T00:00:00Z"
            });
            let layer_json_bytes = serde_json::to_vec(&layer_json)?;
            write_tar_entry(
                &mut writer,
                &format!("{}/json", layer.dir),
                &layer_json_bytes,
            )
            .await?;

            let mut span = tracer
                .span_builder(format!("write_layer_tar_{}", idx + 1))
                .with_kind(SpanKind::Internal)
                .start_with_context(&tracer, &parent_cx);
            span.set_attribute(opentelemetry::KeyValue::new(
                "oci.layer.index",
                (idx + 1) as i64,
            ));
            span.set_attribute(opentelemetry::KeyValue::new(
                "oci.layer.tar_size",
                layer.tar_size as i64,
            ));

            let result = write_tar_file_entry(
                &mut writer,
                &format!("{}/layer.tar", layer.dir),
                layer.file.path(),
                layer.tar_size,
            )
            .await;
            match &result {
                Ok(_) => span.set_status(Status::Ok),
                Err(e) => span.set_status(Status::error(e.to_string())),
            }
            span.end();
            result?;

            info!("Completed layer {}/{}", idx + 1, prepared.layers.len());
        }

        let footer = [0_u8; 1024];
        writer.write_all(&footer).await?;
        writer.flush().await?;

        info!(
            "Tar stream complete for {}:{}",
            prepared.image, prepared.tag
        );
        Ok(())
    }

    async fn download_and_prepare_layer(
        client: &RegistryClient,
        image: &str,
        layer: &Layer,
        parent_cx: &OtelContext,
    ) -> Result<(NamedTempFile, u64)> {
        if layer.media_type.contains("zstd") {
            return Err(anyhow!(
                "Unsupported zstd-compressed layer {} ({})",
                layer.digest,
                layer.media_type
            ));
        }

        info!("Downloading layer: {}", layer.digest);

        let compressed_file = NamedTempFile::new()?;
        let compressed_path = compressed_file.path().to_path_buf();
        let mut compressed_writer = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&compressed_path)
            .await?;

        let mut stream = client
            .get_layer_stream_with_context(image, &layer.digest, parent_cx)
            .await?;
        let mut magic = [0_u8; 2];
        let mut magic_len = 0_usize;
        let mut compressed_size = 0_u64;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            if magic_len < magic.len() {
                let remaining = magic.len() - magic_len;
                let take = remaining.min(chunk.len());
                magic[magic_len..magic_len + take].copy_from_slice(&chunk[..take]);
                magic_len += take;
            }
            compressed_size = compressed_size.saturating_add(chunk.len() as u64);
            compressed_writer.write_all(&chunk).await?;
        }

        compressed_writer.flush().await?;
        drop(compressed_writer);

        info!(
            "Downloaded {} bytes for layer {}",
            compressed_size, layer.digest
        );

        let is_gzipped = layer.media_type.contains("gzip")
            || layer.media_type.contains("tar+gzip")
            || (magic_len == magic.len() && magic == [0x1f, 0x8b]);

        if !is_gzipped {
            info!("Layer {} is already an uncompressed tar", layer.digest);
            return Ok((compressed_file, compressed_size));
        }

        info!("Decompressing gzipped layer {}", layer.digest);
        let decompressed = decompress_temp_file(compressed_path).await?;
        drop(compressed_file);

        info!(
            "Layer {} decompressed to {} bytes",
            layer.digest, decompressed.1
        );
        Ok(decompressed)
    }
}

async fn decompress_temp_file(path: PathBuf) -> Result<(NamedTempFile, u64)> {
    tokio::task::spawn_blocking(move || -> Result<(NamedTempFile, u64)> {
        let input = std::fs::File::open(path)?;
        let mut decoder = GzDecoder::new(input);
        let mut output = NamedTempFile::new()?;
        let size = std::io::copy(&mut decoder, output.as_file_mut())?;
        output.as_file_mut().flush()?;
        Ok((output, size))
    })
    .await?
}

async fn write_tar_entry<W: AsyncWrite + Unpin>(
    writer: &mut W,
    path: &str,
    data: &[u8],
) -> Result<()> {
    let size = data.len() as u64;
    write_tar_header(writer, path, size, 0o644, tar::EntryType::Regular).await?;
    writer.write_all(data).await?;
    write_tar_padding(writer, size).await?;
    Ok(())
}

async fn write_tar_file_entry<W: AsyncWrite + Unpin>(
    writer: &mut W,
    path: &str,
    file_path: &Path,
    size: u64,
) -> Result<()> {
    write_tar_header(writer, path, size, 0o644, tar::EntryType::Regular).await?;

    let mut file = tokio::fs::File::open(file_path).await?;
    let copied = tokio::io::copy(&mut file, writer).await?;
    if copied != size {
        return Err(anyhow!(
            "Layer temp file changed while streaming: expected {} bytes, copied {} bytes",
            size,
            copied
        ));
    }

    write_tar_padding(writer, size).await?;
    Ok(())
}

async fn write_tar_dir<W: AsyncWrite + Unpin>(writer: &mut W, path: &str) -> Result<()> {
    write_tar_header(writer, path, 0, 0o755, tar::EntryType::Directory).await
}

async fn write_tar_header<W: AsyncWrite + Unpin>(
    writer: &mut W,
    path: &str,
    size: u64,
    mode: u32,
    entry_type: tar::EntryType,
) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_path(path)?;
    header.set_size(size);
    header.set_mode(mode);
    header.set_mtime(0);
    header.set_cksum();
    writer.write_all(header.as_bytes()).await?;
    Ok(())
}

async fn write_tar_padding<W: AsyncWrite + Unpin>(writer: &mut W, size: u64) -> Result<()> {
    let remainder = (size % TAR_BLOCK_SIZE as u64) as usize;
    if remainder > 0 {
        let padding = TAR_BLOCK_SIZE - remainder;
        writer.write_all(&[0_u8; TAR_BLOCK_SIZE][..padding]).await?;
    }
    Ok(())
}
