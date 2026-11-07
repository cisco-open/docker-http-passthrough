use super::*;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
    Router,
};
use std::{collections::HashMap, sync::Arc};
use tokio::task::JoinHandle;

#[derive(Clone, Copy)]
enum Fault {
    None,
    Manifest,
    Config,
    Layer,
    LayerSize,
    DiffId,
    Platform,
    Zstd,
    IndexSize,
    NestedIndex,
    DuplicatePlatform,
    DuplicateConfig,
    DuplicateManifest,
}

struct Fixture {
    origin: String,
    digest: String,
    task: JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    async fn start(fault: Fault, index: bool, gzip: bool) -> Self {
        let mut inner_tar = Builder::new(Vec::new());
        inner_tar
            .append_data(&mut tar_header(6), "hello.txt", &b"hello\n"[..])
            .unwrap();
        let plain = inner_tar.into_inner().unwrap();
        let diff_id = if matches!(fault, Fault::DiffId) {
            digest_bytes(b"wrong layer")
        } else {
            digest_bytes(&plain)
        };
        let layer = if gzip {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&plain).unwrap();
            encoder.finish().unwrap()
        } else {
            plain
        };
        let config = serde_json::to_vec(&json!({"os": "linux", "architecture": if matches!(fault, Fault::Platform) { "arm64" } else { "amd64" }, "rootfs": {"type": "layers", "diff_ids": [diff_id]}})).unwrap();
        let config = if matches!(fault, Fault::DuplicateConfig) {
            [b"{\"architecture\":\"arm64\",".as_slice(), &config[1..]].concat()
        } else {
            config
        };
        let config_digest = digest_bytes(&config);
        let layer_digest = digest_bytes(&layer);
        let layer_type = if matches!(fault, Fault::Zstd) {
            "application/vnd.oci.image.layer.v1.tar+zstd"
        } else if gzip {
            "application/vnd.oci.image.layer.v1.tar+gzip"
        } else {
            "application/vnd.oci.image.layer.v1.tar"
        };
        let manifest = serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "config": {"digest": config_digest, "size": config.len(), "mediaType": "application/vnd.oci.image.config.v1+json"}, "layers": [{"digest": layer_digest, "size": layer.len() + usize::from(matches!(fault, Fault::LayerSize)), "mediaType": layer_type}]})).unwrap();
        let manifest = if matches!(fault, Fault::DuplicateManifest) {
            [b"{\"schemaVersion\":1,".as_slice(), &manifest[1..]].concat()
        } else {
            manifest
        };
        let manifest_digest = digest_bytes(&manifest);
        let mut objects = HashMap::new();
        objects.insert(
            format!("/v2/test/agent/manifests/{manifest_digest}"),
            if matches!(fault, Fault::Manifest) {
                b"tampered".to_vec()
            } else {
                manifest.clone()
            },
        );
        objects.insert(
            format!("/v2/test/agent/blobs/{config_digest}"),
            if matches!(fault, Fault::Config) {
                let mut changed = config.clone();
                changed[0] ^= 1;
                changed
            } else {
                config
            },
        );
        objects.insert(
            format!("/v2/test/agent/blobs/{layer_digest}"),
            if matches!(fault, Fault::Layer) {
                let mut changed = layer.clone();
                changed[0] ^= 1;
                changed
            } else {
                layer
            },
        );
        let digest = if index {
            let entry = json!({"digest": manifest_digest, "size": manifest.len() + usize::from(matches!(fault, Fault::IndexSize)), "mediaType": if matches!(fault, Fault::NestedIndex) {"application/vnd.oci.image.index.v1+json"} else {"application/vnd.oci.image.manifest.v1+json"}, "platform": {"os": "linux", "architecture": "amd64"}});
            let entries = if matches!(fault, Fault::DuplicatePlatform) {
                vec![entry.clone(), entry]
            } else {
                vec![entry]
            };
            let index = serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": entries})).unwrap();
            let digest = digest_bytes(&index);
            objects.insert(format!("/v2/test/agent/manifests/{digest}"), index);
            digest
        } else {
            manifest_digest
        };
        async fn serve(
            State(objects): State<Arc<HashMap<String, Vec<u8>>>>,
            request: Request,
        ) -> impl IntoResponse {
            match objects.get(request.uri().path()) {
                Some(bytes) => (StatusCode::OK, bytes.clone()),
                None => (StatusCode::NOT_FOUND, Vec::new()),
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(serve).with_state(Arc::new(objects));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            origin,
            digest,
            task,
        }
    }

    fn options(&self, output: &Path) -> Options {
        Options {
            repository: "test/agent".into(),
            digest: self.digest.clone(),
            platform: Platform::parse("linux/amd64").unwrap(),
            output: output.into(),
            registry: self.origin.clone(),
            auth_url: None,
            ca_file: None,
            blob_origins: vec![],
            max_bytes: 1024 * 1024,
            // Hashing the debug test executable can be slow on a busy CI host.
            // Deadline behavior is exercised separately with an expired clock.
            timeout: Duration::from_secs(60),
        }
    }
    fn client(&self) -> VerifiedRegistry {
        VerifiedRegistry::for_test(&self.origin).unwrap()
    }
}

#[tokio::test]
async fn verified_index_gzip_preparation_is_deterministic_and_descriptor_binds_bytes() {
    let registry = Fixture::start(Fault::None, true, true).await;
    let temp = tempfile::tempdir().unwrap();
    let output_a = temp.path().join("a");
    let output_b = temp.path().join("b");
    let a = prepare(&registry.options(&output_a), registry.client())
        .await
        .unwrap();
    let b = prepare(&registry.options(&output_b), registry.client())
        .await
        .unwrap();
    let bytes = fs::read(output_a.join("package.tar")).unwrap();
    assert_eq!(bytes, fs::read(output_b.join("package.tar")).unwrap());
    assert_eq!(a["package"]["digest"], digest_bytes(&bytes));
    assert_eq!(a["package"]["sizeBytes"], bytes.len());
    assert_eq!(a["package"], b["package"]);
    assert_eq!(a["source"]["indexDigest"], registry.digest);
    assert_eq!(
        a["provenance"]["digest"],
        digest_bytes(&fs::read(output_a.join("provenance.json")).unwrap())
    );
    assert_eq!(
        a["producer"]["recipeDigest"],
        digest_bytes(&fs::read(output_a.join("recipe.json")).unwrap())
    );
    assert!(a.get("approved").is_none());
    let checksums: Value =
        serde_json::from_slice(&fs::read(output_a.join("checksums.json")).unwrap()).unwrap();
    assert_eq!(
        checksums["descriptor.json"],
        digest_bytes(&fs::read(output_a.join("descriptor.json")).unwrap())
    );
    let mut tar = tar::Archive::new(bytes.as_slice());
    let mut saw_manifest = false;
    for entry in tar.entries().unwrap() {
        let mut entry = entry.unwrap();
        assert_eq!(entry.header().mtime().unwrap(), 0);
        assert_eq!(entry.header().uid().unwrap(), 0);
        if entry.path().unwrap() == Path::new("manifest.json") {
            let manifest: Value = serde_json::from_reader(&mut entry).unwrap();
            let repo_tag = manifest[0]["RepoTags"][0].as_str().unwrap();
            assert!(repo_tag.starts_with("test/agent:sha256-"));
            assert_eq!(repo_tag.matches(':').count(), 1);
            saw_manifest = true;
        }
    }
    assert!(saw_manifest);
}

#[tokio::test]
async fn corrupt_content_and_unsupported_sources_never_publish_or_leave_scratch() {
    for (fault, indexed, message) in [
        (Fault::Manifest, false, "metadata digest mismatch"),
        (Fault::Config, false, "metadata digest mismatch"),
        (Fault::Layer, false, "layer digest mismatch"),
        (Fault::LayerSize, false, "layer size differs"),
        (Fault::DiffId, false, "diff ID mismatch"),
        (Fault::Platform, false, "platform does not match"),
        (Fault::Zstd, false, "unsupported layer media type"),
        (Fault::IndexSize, true, "metadata size differs"),
        (Fault::NestedIndex, true, "nested indexes"),
        (Fault::DuplicateConfig, false, "ambiguous source JSON"),
        (Fault::DuplicateManifest, false, "ambiguous source JSON"),
        (
            Fault::DuplicatePlatform,
            true,
            "exactly one matching platform",
        ),
    ] {
        let registry = Fixture::start(fault, indexed, true).await;
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("artifact");
        let error = prepare(&registry.options(&output), registry.client())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(message),
            "expected {message}, got {error:#}"
        );
        assert!(!output.exists());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn direct_manifest_and_plain_layer_work_and_existing_outputs_are_untouched() {
    let registry = Fixture::start(Fault::None, false, false).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("artifact");
    let descriptor = prepare(&registry.options(&output), registry.client())
        .await
        .unwrap();
    assert!(descriptor["source"].get("indexDigest").is_none());
    assert_eq!(
        descriptor["source"]["requestedDigest"],
        descriptor["source"]["manifestDigest"]
    );
    let before = fs::read(output.join("descriptor.json")).unwrap();
    assert!(prepare(&registry.options(&output), registry.client())
        .await
        .is_err());
    assert_eq!(before, fs::read(output.join("descriptor.json")).unwrap());
}

#[tokio::test]
async fn package_budget_and_missing_index_platform_fail_closed() {
    let registry = Fixture::start(Fault::None, true, true).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("artifact");
    let mut options = registry.options(&output);
    options.max_bytes = 1024;
    assert!(prepare(&options, registry.client()).await.is_err());
    assert!(!output.exists());
    options.max_bytes = 1024 * 1024;
    options.platform = Platform::parse("linux/arm64").unwrap();
    assert!(prepare(&options, registry.client())
        .await
        .unwrap_err()
        .to_string()
        .contains("exactly one"));
}

#[test]
fn expansion_budget_diff_id_and_deadline_are_enforced() {
    let temp = tempfile::tempdir().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let expanded = vec![0; 1024 * 1024];
    encoder.write_all(&expanded).unwrap();
    let path = temp.path().join("layer.gz");
    fs::write(&path, encoder.finish().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    assert!(expand_layer(
        &path,
        true,
        &digest_bytes(&expanded),
        temp.path(),
        1024,
        deadline
    )
    .unwrap_err()
    .to_string()
    .contains("byte budget"));
    assert!(expand_layer(
        &path,
        true,
        &digest_bytes(b"wrong"),
        temp.path(),
        expanded.len() as u64,
        deadline
    )
    .unwrap_err()
    .to_string()
    .contains("diff ID mismatch"));
    assert!(expand_layer(
        &path,
        true,
        &digest_bytes(&expanded),
        temp.path(),
        expanded.len() as u64,
        Instant::now()
    )
    .is_err());
}

#[test]
fn atomic_publish_refuses_existing_directory_and_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("data"), b"new").unwrap();
    let destination = temp.path().join("destination");
    fs::create_dir(&destination).unwrap();
    assert!(publish_directory(&source, &destination).is_err());
    assert!(source.join("data").exists());
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    #[cfg(unix)]
    {
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&destination, &link).unwrap();
        assert!(publish_directory(&source, &link).is_err());
    }
}

#[test]
fn command_inputs_and_timestamp_are_strict() {
    for invalid in [
        "agent:latest",
        "agent@sha256:abc",
        "../agent",
        "https://host/image",
        "Agent",
        "agent?x=1",
        "a..b",
        "a._b",
        "a___b",
    ] {
        assert!(validate_repository(invalid).is_err());
    }
    assert!(validate_repository("team/agent_name.v2").is_ok());
    assert!(validate_repository("team__name/agent--name.v2").is_ok());
    assert!(validate_digest("sha256:abc").is_err());
    assert!(validate_digest(&digest_bytes(b"test")).is_ok());
    assert!(Platform::parse("amd64").is_err());
    assert!(Platform::parse("windows/amd64").is_err());
    assert!(Platform::parse("linux/_amd64").is_err());
    assert!(Platform::parse("linux/-amd64").is_err());
    assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
    assert_eq!(utc_timestamp(1709251199), "2024-02-29T23:59:59Z");
    assert!(
        Options::parse(&["prepare".into(), "--repository".into(), "test/agent".into()]).is_err()
    );
}
