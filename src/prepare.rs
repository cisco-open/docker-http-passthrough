// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

//! Local, unsigned artifact preparation. This module does not approve, sign,
//! publish remotely, scan, or install an application.

use crate::registry::{
    verified::{decode_json, digest_bytes, validate_digest, validate_repository, VerifiedRegistry},
    Manifest,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use flate2::read::MultiGzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tar::{Builder, Header};

const RECIPE: &[u8] = b"{\"format\":\"docker-save-v1\",\"revision\":1,\"repoTag\":\"sha256-<manifest-hex>\",\"tarMetadata\":\"uid=0,gid=0,mtime=0,mode=0644\",\"layers\":\"ordered-uncompressed-tar\"}\n";
const MAX_LAYERS: usize = 128;
const HELP: &str = "Usage: docker-http-passthrough prepare --repository PATH --digest sha256:HEX --platform linux/ARCH[/VARIANT] --output-dir NEW_DIRECTORY [--registry https://HOST] [--auth-url https://HOST/PATH] [--ca-file CA.pem] [--blob-origin https://HOST] [--max-bytes N] [--timeout-seconds N]\n\nProduces package.tar, descriptor.json, provenance.json, recipe.json and checksums.json. Output is unsigned and unapproved. No arguments starts the unchanged HTTP server. Registry defaults to REGISTRY_URL or Docker Hub. Credentials use REGISTRY_USERNAME/REGISTRY_PASSWORD or REGISTRY_BEARER_TOKEN. Blob redirect origins must be explicitly allowlisted.\n";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Platform {
    os: String,
    architecture: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
}

impl Platform {
    fn parse(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split('/').collect();
        ensure!(
            (parts.len() == 2 || parts.len() == 3) && parts[0] == "linux",
            "platform must be linux/architecture[/variant]"
        );
        ensure!(
            !parts[1].is_empty()
                && parts[1].len() <= 32
                && parts[1].as_bytes()[0].is_ascii_alphanumeric()
                && parts[1]
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b)),
            "invalid platform architecture"
        );
        let variant = parts.get(2).map(|s| s.to_string());
        if let Some(v) = &variant {
            ensure!(
                !v.is_empty()
                    && v.len() <= 32
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                "invalid platform variant"
            );
        }
        Ok(Self {
            os: "linux".into(),
            architecture: parts[1].into(),
            variant,
        })
    }

    fn matches_json(&self, value: &Value) -> bool {
        value.get("os").and_then(Value::as_str) == Some(self.os.as_str())
            && value.get("architecture").and_then(Value::as_str) == Some(self.architecture.as_str())
            && value.get("variant").and_then(Value::as_str) == self.variant.as_deref()
    }
}

struct Options {
    repository: String,
    digest: String,
    platform: Platform,
    output: PathBuf,
    registry: String,
    auth_url: Option<String>,
    ca_file: Option<PathBuf>,
    blob_origins: Vec<String>,
    max_bytes: u64,
    timeout: Duration,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        ensure!(
            args.first().map(String::as_str) == Some("prepare"),
            "unknown command; use --help"
        );
        let mut values = std::collections::HashMap::new();
        let mut blob_origins = Vec::new();
        let mut args = args[1..].iter();
        while let Some(flag) = args.next() {
            ensure!(
                [
                    "--repository",
                    "--digest",
                    "--platform",
                    "--output-dir",
                    "--registry",
                    "--auth-url",
                    "--ca-file",
                    "--blob-origin",
                    "--max-bytes",
                    "--timeout-seconds"
                ]
                .contains(&flag.as_str()),
                "unknown preparation option"
            );
            let value = args
                .next()
                .ok_or_else(|| anyhow!("missing value for {flag}"))?;
            if flag == "--blob-origin" {
                blob_origins.push(value.clone());
            } else {
                ensure!(
                    values.insert(flag.as_str(), value.clone()).is_none(),
                    "duplicate option {flag}"
                );
            }
        }
        let required = |key| {
            values
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow!("missing {key}"))
        };
        let repository = required("--repository")?;
        validate_repository(&repository)?;
        let digest = required("--digest")?;
        validate_digest(&digest)?;
        let platform = Platform::parse(&required("--platform")?)?;
        let max_bytes = values
            .get("--max-bytes")
            .map(|s| s.parse::<u64>())
            .transpose()?
            .unwrap_or(2 * 1024 * 1024 * 1024);
        ensure!(
            (1024..=1_u64 << 36).contains(&max_bytes),
            "max-bytes must be between 1024 and 68719476736"
        );
        let seconds = values
            .get("--timeout-seconds")
            .map(|s| s.parse::<u64>())
            .transpose()?
            .unwrap_or(1800);
        ensure!(
            (1..=86400).contains(&seconds),
            "timeout-seconds must be between 1 and 86400"
        );
        Ok(Self {
            repository,
            digest,
            platform,
            output: required("--output-dir")?.into(),
            registry: values
                .remove("--registry")
                .or_else(|| std::env::var("REGISTRY_URL").ok())
                .unwrap_or_else(|| "https://registry-1.docker.io".into()),
            auth_url: values.remove("--auth-url").or_else(|| {
                std::env::var("REGISTRY_AUTH_URL")
                    .ok()
                    .filter(|s| !s.is_empty())
            }),
            ca_file: values.remove("--ca-file").map(PathBuf::from),
            blob_origins,
            max_bytes,
            timeout: Duration::from_secs(seconds),
        })
    }
}

pub async fn run_cli(args: &[String]) -> Result<()> {
    if args == ["--help"] || args == ["-h"] || args == ["prepare", "--help"] {
        print!("{HELP}");
        return Ok(());
    }
    // Load the same environment file as server mode, before reading options.
    let _ = dotenvy::dotenv();
    let options = Options::parse(args)?;
    let ca = options
        .ca_file
        .as_ref()
        .map(|path| -> Result<Vec<u8>> {
            let meta = std::fs::metadata(path).context("read registry CA file metadata")?;
            ensure!(
                meta.is_file() && meta.len() > 0 && meta.len() <= 1024 * 1024,
                "registry CA file must be a regular file of at most 1 MiB"
            );
            let mut raw = Vec::new();
            std::io::Read::take(std::fs::File::open(path)?, 1024 * 1024 + 1)
                .read_to_end(&mut raw)?;
            ensure!(
                raw.len() <= 1024 * 1024,
                "registry CA file exceeds size limit"
            );
            Ok(raw)
        })
        .transpose()?;
    let registry = if let Some(ca) = ca.as_ref() {
        VerifiedRegistry::new_with_ca(
            &options.registry,
            options.auth_url.as_deref(),
            &options.blob_origins,
            options.timeout.min(Duration::from_secs(120)),
            ca,
        )
    } else {
        VerifiedRegistry::new(
            &options.registry,
            options.auth_url.as_deref(),
            &options.blob_origins,
            options.timeout.min(Duration::from_secs(120)),
        )
    }?
    .with_environment_credentials();
    let descriptor = tokio::time::timeout(options.timeout, prepare(&options, registry))
        .await
        .context("preparation exceeded total timeout")??;
    println!("{}", serde_json::to_string(&descriptor)?);
    Ok(())
}

struct SelectedManifest {
    bytes: Vec<u8>,
    digest: String,
    index: Option<String>,
}

async fn select_manifest(
    registry: &VerifiedRegistry,
    options: &Options,
) -> Result<SelectedManifest> {
    let bytes = registry
        .metadata(&options.repository, &options.digest, None, true)
        .await?;
    let value: Value = decode_json(&bytes)?;
    ensure!(
        value.get("schemaVersion").and_then(Value::as_u64) == Some(2),
        "unsupported manifest schema version"
    );
    let media_type = value.get("mediaType").and_then(Value::as_str).unwrap_or("");
    if matches!(
        media_type,
        "application/vnd.oci.image.index.v1+json"
            | "application/vnd.docker.distribution.manifest.list.v2+json"
    ) {
        let entries = value
            .get("manifests")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("index has no manifest descriptors"))?;
        ensure!(entries.len() <= 1024, "index has too many entries");
        let selected: Vec<_> = entries
            .iter()
            .filter(|e| {
                e.get("platform")
                    .is_some_and(|p| options.platform.matches_json(p))
            })
            .collect();
        ensure!(
            selected.len() == 1,
            "index must contain exactly one matching platform"
        );
        let selected = selected[0];
        ensure!(
            is_manifest_type(
                selected
                    .get("mediaType")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ),
            "nested indexes or unsupported child media types are not supported"
        );
        let digest = selected
            .get("digest")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("child manifest has no digest"))?;
        let size = selected
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("child manifest has no size"))?;
        let bytes = registry
            .metadata(&options.repository, digest, Some(size), true)
            .await?;
        Ok(SelectedManifest {
            bytes,
            digest: digest.into(),
            index: Some(options.digest.clone()),
        })
    } else {
        ensure!(
            is_manifest_type(media_type),
            "unsupported manifest media type"
        );
        Ok(SelectedManifest {
            bytes,
            digest: options.digest.clone(),
            index: None,
        })
    }
}

fn is_manifest_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/vnd.oci.image.manifest.v1+json"
            | "application/vnd.docker.distribution.manifest.v2+json"
    )
}

async fn prepare(options: &Options, registry: VerifiedRegistry) -> Result<Value> {
    ensure!(
        fs::symlink_metadata(&options.output)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "output directory already exists or is inaccessible"
    );
    let parent = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(
        parent.is_dir(),
        "output parent directory must already exist"
    );
    let staging = tempfile::Builder::new()
        .prefix(".dhp-prepare-")
        .tempdir_in(parent)?;
    let deadline = Instant::now() + options.timeout;
    let selected = select_manifest(&registry, options).await?;
    let manifest: Manifest = decode_json(&selected.bytes)?;
    ensure!(
        manifest.schema_version == 2
            && manifest.manifests.is_none()
            && is_manifest_type(manifest.media_type.as_deref().unwrap_or("")),
        "unsupported selected manifest"
    );
    let config = manifest
        .config
        .ok_or_else(|| anyhow!("manifest has no config"))?;
    ensure!(
        matches!(
            config.media_type.as_str(),
            "application/vnd.oci.image.config.v1+json"
                | "application/vnd.docker.container.image.v1+json"
        ),
        "unsupported config media type"
    );
    let layers = manifest
        .layers
        .ok_or_else(|| anyhow!("manifest has no layers"))?;
    ensure!(
        layers.len() <= MAX_LAYERS,
        "image exceeds layer count limit"
    );
    let config_bytes = registry
        .metadata(
            &options.repository,
            &config.digest,
            Some(config.size),
            false,
        )
        .await?;
    let config_json: Value = decode_json(&config_bytes)?;
    ensure!(
        options.platform.matches_json(&config_json),
        "image config platform does not match requested platform"
    );
    ensure!(
        config_json.pointer("/rootfs/type").and_then(Value::as_str) == Some("layers"),
        "unsupported rootfs type"
    );
    let diff_ids = config_json
        .pointer("/rootfs/diff_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("config has no rootfs diff IDs"))?;
    ensure!(
        diff_ids.len() == layers.len(),
        "layer count differs from config diff IDs"
    );
    let mut compressed_total = 0_u64;
    for layer in &layers {
        validate_digest(&layer.digest)?;
        compressed_total = compressed_total
            .checked_add(layer.size)
            .ok_or_else(|| anyhow!("compressed size overflow"))?;
        ensure!(
            compressed_total <= options.max_bytes,
            "compressed image exceeds byte budget"
        );
        layer_encoding(&layer.media_type)?;
    }
    for diff_id in diff_ids {
        validate_digest(diff_id.as_str().ok_or_else(|| anyhow!("invalid diff ID"))?)?;
    }

    let package_path = staging.path().join("package.tar");
    let writer = LimitedWriter {
        file: File::create(&package_path)?,
        hash: Sha256::new(),
        size: 0,
        limit: options.max_bytes,
        deadline,
    };
    let mut archive = Builder::new(writer);
    let config_name = format!("{}.json", &config.digest[7..]);
    let repo_tag = format!("{}:sha256-{}", options.repository, &selected.digest[7..]);
    let names: Vec<_> = layers
        .iter()
        .map(|l| format!("{}/layer.tar", &l.digest[7..]))
        .collect();
    let archive_manifest =
        json!([{ "Config": config_name, "RepoTags": [repo_tag], "Layers": names }]);
    append_bytes(
        &mut archive,
        "manifest.json",
        &serde_json::to_vec(&archive_manifest)?,
    )?;
    append_bytes(&mut archive, &config_name, &config_bytes)?;
    let mut expanded_total = 0_u64;
    let mut written = HashSet::new();
    for (layer, diff_id) in layers.iter().zip(diff_ids) {
        ensure!(Instant::now() < deadline, "preparation deadline exceeded");
        let compressed = registry
            .blob(
                &options.repository,
                &layer.digest,
                layer.size,
                staging.path(),
                options.max_bytes,
            )
            .await?;
        let (expanded, size) = expand_layer(
            compressed.path(),
            layer_encoding(&layer.media_type)?,
            diff_id.as_str().unwrap(),
            staging.path(),
            options.max_bytes - expanded_total,
            deadline,
        )?;
        expanded_total += size;
        // Repeated descriptors retain their positions in manifest.json but need
        // only one tar entry. Every occurrence's expected diff ID is checked.
        if written.insert(layer.digest.clone()) {
            let base = &layer.digest[7..];
            append_bytes(&mut archive, &format!("{base}/VERSION"), b"1.0")?;
            append_bytes(
                &mut archive,
                &format!("{base}/json"),
                &serde_json::to_vec(&json!({"id": base, "created": "1970-01-01T00:00:00Z"}))?,
            )?;
            let mut header = tar_header(size);
            archive.append_data(
                &mut header,
                format!("{base}/layer.tar"),
                File::open(expanded.path())?,
            )?;
        }
    }
    archive.finish()?;
    let writer = archive.into_inner()?;
    writer.file.sync_all()?;
    let package_digest = format!("sha256:{:x}", writer.hash.finalize());
    let package_size = writer.size;
    let source = json!({ "registry": registry.authority(), "repository": options.repository, "requestedDigest": options.digest, "manifestDigest": selected.digest, "configDigest": config.digest, "platform": options.platform });
    let mut source = source;
    if let Some(index) = selected.index {
        source["indexDigest"] = index.into();
    }
    let build_digest = hash_file(&std::env::current_exe()?)?;
    let producer = json!({ "name": "docker-http-passthrough", "version": env!("CARGO_PKG_VERSION"), "buildDigest": build_digest, "recipeDigest": digest_bytes(RECIPE) });
    let provenance = json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{"name": "package.tar", "digest": {"sha256": &package_digest[7..]}}],
        "predicateType": "urn:cisco:docker-http-passthrough:verified-preparation:v1",
        "predicate": { "source": source, "producer": producer, "verification": { "compressedSizeBytes": compressed_total, "uncompressedSizeBytes": expanded_total, "manifestConfigLayersAndDiffIDs": true } }
    });
    let provenance_bytes = json_bytes(&provenance)?;
    let descriptor = json!({
        "apiVersion": "apphosting.cisco.com/v1alpha1", "kind": "AppHostingArtifact",
        "source": source,
        "package": {"format": "docker-save-v1", "mediaType": "application/x-tar", "digest": package_digest, "sizeBytes": package_size},
        "producer": producer,
        "createdAt": utc_timestamp(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()),
        "provenance": {"digest": digest_bytes(&provenance_bytes), "mediaType": "application/vnd.in-toto+json"}
    });
    let descriptor_bytes = json_bytes(&descriptor)?;
    write_synced(staging.path().join("descriptor.json"), &descriptor_bytes)?;
    write_synced(staging.path().join("provenance.json"), &provenance_bytes)?;
    write_synced(staging.path().join("recipe.json"), RECIPE)?;
    write_synced(
        staging.path().join("checksums.json"),
        &json_bytes(
            &json!({"package.tar": descriptor["package"]["digest"], "descriptor.json": digest_bytes(&descriptor_bytes), "provenance.json": digest_bytes(&provenance_bytes), "recipe.json": digest_bytes(RECIPE)}),
        )?,
    )?;
    File::open(staging.path())?.sync_all()?;
    ensure!(
        Instant::now() < deadline,
        "preparation deadline exceeded before publication"
    );
    publish_directory(staging.path(), &options.output)?;
    File::open(parent)?.sync_all()?;
    Ok(descriptor)
}

fn layer_encoding(media_type: &str) -> Result<bool> {
    match media_type {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar" => Ok(false),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Ok(true),
        _ => bail!("unsupported layer media type (only tar and gzip tar are supported)"),
    }
}

fn expand_layer(
    path: &Path,
    gzip: bool,
    diff_id: &str,
    directory: &Path,
    budget: u64,
    deadline: Instant,
) -> Result<(tempfile::NamedTempFile, u64)> {
    let input = File::open(path)?;
    let mut input: Box<dyn Read> = if gzip {
        Box::new(MultiGzDecoder::new(input))
    } else {
        Box::new(input)
    };
    let mut output = tempfile::NamedTempFile::new_in(directory)?;
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        ensure!(
            Instant::now() < deadline,
            "preparation deadline exceeded during decompression"
        );
        let read = input
            .read(&mut buffer)
            .context("invalid or truncated compressed layer")?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| anyhow!("expanded size overflow"))?;
        ensure!(size <= budget, "expanded image exceeds byte budget");
        hash.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    ensure!(
        format!("sha256:{:x}", hash.finalize()) == diff_id,
        "uncompressed layer diff ID mismatch"
    );
    output.flush()?;
    Ok((output, size))
}

struct LimitedWriter {
    file: File,
    hash: Sha256,
    size: u64,
    limit: u64,
    deadline: Instant,
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "preparation deadline exceeded",
            ));
        }
        if bytes.len() as u64 > self.limit.saturating_sub(self.size) {
            return Err(std::io::Error::other("package exceeds byte budget"));
        }
        let size = self.file.write(bytes)?;
        self.hash.update(&bytes[..size]);
        self.size += size as u64;
        Ok(size)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn tar_header(size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    header
}
fn append_bytes(archive: &mut Builder<LimitedWriter>, name: &str, bytes: &[u8]) -> Result<()> {
    archive.append_data(&mut tar_header(bytes.len() as u64), name, bytes)?;
    Ok(())
}
fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}
fn write_synced(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

fn publish_directory(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
        // Both APIs atomically publish the whole directory and refuse to replace
        // any existing destination, including an empty directory or a symlink.
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("atomic publication failed; output was not replaced");
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (source, destination);
        bail!("atomic preparation publication is supported on Linux and macOS only")
    }
}

// Gregorian civil date conversion from Unix days; no shell command or new date
// runtime dependency. The descriptor's timestamp is historical, not approval.
fn utc_timestamp(seconds: u64) -> String {
    let days = (seconds / 86400) as i64 + 719468;
    let era = days / 146097;
    let day_of_era = days - era * 146097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds % 86400 / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests;
