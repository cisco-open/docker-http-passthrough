// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

//! Digest-verified acquisition for the prepare command. The legacy HTTP routes
//! intentionally keep their existing client and authentication behavior.

use super::{parse_authenticate_header, AuthResponse, RegistryCredentials};
use anyhow::{anyhow, bail, ensure, Context, Result};
use futures::StreamExt;
use reqwest::{header, Client, Response, Url};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path, time::Duration};
use tempfile::NamedTempFile;

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json";
pub const METADATA_LIMIT: u64 = 4 * 1024 * 1024;

pub fn validate_digest(value: &str) -> Result<()> {
    let hex = value.strip_prefix("sha256:").unwrap_or_default();
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected lowercase sha256:<64 hex digits>"
    );
    Ok(())
}

pub fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn validate_repository(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 255,
        "invalid repository length"
    );
    for component in value.split('/') {
        let bytes = component.as_bytes();
        let alphanumeric = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let mut pos = 0;
        loop {
            let start = pos;
            while pos < bytes.len() && alphanumeric(bytes[pos]) {
                pos += 1;
            }
            ensure!(pos > start, "invalid canonical repository component");
            if pos == bytes.len() {
                break;
            }
            match bytes[pos] {
                b'.' => pos += 1,
                b'_' => { pos += 1; if pos < bytes.len() && bytes[pos] == b'_' { pos += 1; } },
                b'-' => { while pos < bytes.len() && bytes[pos] == b'-' { pos += 1; } },
                _ => bail!("repository must be a lowercase canonical path without a tag, digest, URL or credentials"),
            }
        }
    }
    Ok(())
}

fn endpoint(value: &str, allow_test_http: bool) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow!("invalid registry/auth endpoint"))?;
    ensure!(
        url.scheme() == "https"
            || (allow_test_http
                && url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))),
        "preparation requires HTTPS endpoints"
    );
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "endpoint must not contain credentials, query or fragment"
    );
    Ok(url)
}

pub struct VerifiedRegistry {
    client: Client,
    origin: Url,
    token_endpoint: Option<Url>,
    blob_origins: Vec<Url>,
    credentials: Option<RegistryCredentials>,
    bearer: Option<String>,
    allow_test_http: bool,
}

impl VerifiedRegistry {
    pub fn new(
        registry: &str,
        token_endpoint: Option<&str>,
        blob_origins: &[String],
        timeout: Duration,
    ) -> Result<Self> {
        Self::build(registry, token_endpoint, blob_origins, timeout, false, None)
    }

    pub fn new_with_ca(
        registry: &str,
        token_endpoint: Option<&str>,
        blob_origins: &[String],
        timeout: Duration,
        ca_pem: &[u8],
    ) -> Result<Self> {
        Self::build(
            registry,
            token_endpoint,
            blob_origins,
            timeout,
            false,
            Some(ca_pem),
        )
    }

    fn build(
        registry: &str,
        token_endpoint: Option<&str>,
        blob_origins: &[String],
        timeout: Duration,
        allow_test_http: bool,
        ca_pem: Option<&[u8]>,
    ) -> Result<Self> {
        let registry = match registry {
            "https://docker.io" | "https://index.docker.io" => "https://registry-1.docker.io",
            other => other,
        };
        let origin = endpoint(registry, allow_test_http)?;
        ensure!(
            origin.path() == "/",
            "registry must be an origin without a path"
        );
        ensure!(
            origin.origin().ascii_serialization().len() - origin.scheme().len() - 3 <= 253,
            "registry authority exceeds descriptor length limit"
        );
        let default_token = (origin.as_str() == "https://registry-1.docker.io/")
            .then_some("https://auth.docker.io/token");
        let token_endpoint = token_endpoint
            .or(default_token)
            .map(|s| endpoint(s, allow_test_http))
            .transpose()?;
        let blob_origins: Vec<_> = blob_origins
            .iter()
            .map(|s| endpoint(s, allow_test_http))
            .collect::<Result<_>>()?;
        ensure!(
            blob_origins.iter().all(|u| u.path() == "/"),
            "blob redirect allowlist entries must be origins"
        );
        let mut client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(timeout);
        if let Some(pem) = ca_pem {
            ensure!(
                !pem.is_empty() && pem.len() <= 1024 * 1024,
                "registry CA file must contain at most 1 MiB"
            );
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .context("invalid registry CA certificate")?;
            ensure!(
                certificates.len() == 1,
                "registry CA file must contain exactly one PEM certificate"
            );
            client = client.add_root_certificate(certificates.into_iter().next().unwrap());
        }
        Ok(Self {
            client: client.build()?,
            origin,
            token_endpoint,
            blob_origins,
            credentials: None,
            bearer: None,
            allow_test_http,
        })
    }

    pub fn with_environment_credentials(mut self) -> Self {
        self.credentials = super::env_value("REGISTRY_USERNAME").map(|username| {
            RegistryCredentials::new(username, super::env_value("REGISTRY_PASSWORD"))
        });
        self.bearer = super::env_value("REGISTRY_BEARER_TOKEN")
            .or_else(|| super::env_value("REGISTRY_TOKEN"));
        self
    }

    pub fn authority(&self) -> String {
        self.origin
            .origin()
            .ascii_serialization()
            .split_once("://")
            .unwrap()
            .1
            .to_string()
    }

    async fn request(&self, repository: &str, kind: &str, digest: &str) -> Result<Response> {
        validate_repository(repository)?;
        validate_digest(digest)?;
        let url = self
            .origin
            .join(&format!("v2/{repository}/{kind}/{digest}"))?;
        let mut request = self.client.get(url.clone()).header(header::ACCEPT, ACCEPT);
        if let Some(token) = &self.bearer {
            request = request.bearer_auth(token);
        } else if let Some(credentials) = &self.credentials {
            request = request.basic_auth(&credentials.username, credentials.password.as_deref());
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| anyhow!("registry request failed or timed out"))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|h| h.to_str().ok())
                .and_then(parse_authenticate_header)
                .ok_or_else(|| {
                    anyhow!("registry authentication challenge missing or unsupported")
                })?;
            ensure!(
                challenge.scheme == "bearer",
                "registry rejected configured credentials"
            );
            let realm = endpoint(
                challenge
                    .realm
                    .as_deref()
                    .ok_or_else(|| anyhow!("token challenge has no realm"))?,
                self.allow_test_http,
            )?;
            match &self.token_endpoint {
                Some(allowed) => ensure!(
                    realm == *allowed,
                    "registry requested an unapproved token endpoint"
                ),
                None => ensure!(
                    realm.origin() == self.origin.origin(),
                    "cross-origin token endpoint requires explicit --auth-url"
                ),
            }
            let mut token_url = realm;
            {
                let mut query = token_url.query_pairs_mut();
                if let Some(service) = challenge.service {
                    query.append_pair("service", &service);
                }
                // Never accept broader registry-supplied scopes.
                query.append_pair("scope", &format!("repository:{repository}:pull"));
            }
            let mut token_request = self.client.get(token_url);
            if let Some(credentials) = &self.credentials {
                token_request = token_request
                    .basic_auth(&credentials.username, credentials.password.as_deref());
            }
            let response_token = token_request
                .send()
                .await
                .map_err(|_| anyhow!("token request failed or timed out"))?;
            ensure!(
                response_token.status().is_success(),
                "token endpoint rejected request (status {})",
                response_token.status()
            );
            let token: AuthResponse =
                serde_json::from_slice(&read_bounded(response_token, 64 * 1024).await?)
                    .context("invalid token response")?;
            response = self
                .client
                .get(url)
                .header(header::ACCEPT, ACCEPT)
                .bearer_auth(token.into_token()?)
                .send()
                .await
                .map_err(|_| anyhow!("authenticated registry request failed or timed out"))?;
        }
        for _ in 0..3 {
            if !response.status().is_redirection() {
                break;
            }
            ensure!(kind == "blobs", "manifest redirects are not supported");
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|h| h.to_str().ok())
                .ok_or_else(|| anyhow!("blob redirect has no location"))?;
            let target = response
                .url()
                .join(location)
                .map_err(|_| anyhow!("invalid blob redirect"))?;
            ensure!(
                target.username().is_empty()
                    && target.password().is_none()
                    && target.fragment().is_none(),
                "invalid blob redirect authority"
            );
            ensure!(
                target.scheme() == "https"
                    || (self.allow_test_http && target.origin() == self.origin.origin()),
                "blob redirect would downgrade HTTPS"
            );
            ensure!(
                target.origin() == self.origin.origin()
                    || self
                        .blob_origins
                        .iter()
                        .any(|u| u.origin() == target.origin()),
                "blob redirect origin is not allowlisted; configure --blob-origin"
            );
            // Signed CDN query strings are allowed, but never logged. No registry
            // credentials are attached to any redirect request.
            response = self
                .client
                .get(target)
                .send()
                .await
                .map_err(|_| anyhow!("blob redirect request failed or timed out"))?;
        }
        ensure!(
            response.status().is_success(),
            "registry request failed (status {})",
            response.status()
        );
        Ok(response)
    }

    pub async fn metadata(
        &self,
        repository: &str,
        digest: &str,
        expected_size: Option<u64>,
        manifest: bool,
    ) -> Result<Vec<u8>> {
        if let Some(size) = expected_size {
            ensure!(size <= METADATA_LIMIT, "metadata exceeds configured limit");
        }
        let response = self
            .request(
                repository,
                if manifest { "manifests" } else { "blobs" },
                digest,
            )
            .await?;
        let bytes = read_bounded(response, expected_size.unwrap_or(METADATA_LIMIT)).await?;
        verify_bytes(&bytes, digest, expected_size)?;
        Ok(bytes)
    }

    pub async fn blob(
        &self,
        repository: &str,
        digest: &str,
        size: u64,
        directory: &Path,
        limit: u64,
    ) -> Result<NamedTempFile> {
        ensure!(
            size <= limit,
            "compressed layer exceeds remaining byte budget"
        );
        let mut response = self
            .request(repository, "blobs", digest)
            .await?
            .bytes_stream();
        let mut output = NamedTempFile::new_in(directory)?;
        let mut hash = Sha256::new();
        let mut received = 0_u64;
        while let Some(chunk) = response.next().await {
            let chunk = chunk.map_err(|_| anyhow!("layer download failed or timed out"))?;
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| anyhow!("layer size overflow"))?;
            ensure!(received <= size, "layer exceeds descriptor size");
            hash.update(&chunk);
            output.write_all(&chunk)?;
        }
        ensure!(received == size, "layer size differs from descriptor");
        ensure!(
            format!("sha256:{:x}", hash.finalize()) == digest,
            "layer digest mismatch"
        );
        output.flush()?;
        Ok(output)
    }

    #[cfg(test)]
    pub fn for_test(registry: &str) -> Result<Self> {
        Self::build(registry, None, &[], Duration::from_secs(2), true, None)
    }
}

async fn read_bounded(response: Response, limit: u64) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|size| size > limit) {
        bail!("metadata response exceeds byte limit");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow!("metadata download failed or timed out"))?;
        ensure!(
            bytes.len() as u64 + chunk.len() as u64 <= limit,
            "metadata response exceeds byte limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub fn verify_bytes(bytes: &[u8], digest: &str, size: Option<u64>) -> Result<()> {
    validate_digest(digest)?;
    ensure!(
        size.is_none_or(|size| size == bytes.len() as u64),
        "metadata size differs from descriptor"
    );
    ensure!(digest_bytes(bytes) == digest, "metadata digest mismatch");
    Ok(())
}

/// Reject ambiguous JSON before model decoding: the raw config will also be
/// parsed by the downstream container runtime.
pub fn decode_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    struct UniqueKeys;
    impl<'de> serde::Deserialize<'de> for UniqueKeys {
        fn deserialize<D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = UniqueKeys;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("JSON without duplicate object keys")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> std::result::Result<UniqueKeys, A::Error> {
                    let mut keys = std::collections::HashSet::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if !keys.insert(key) {
                            return Err(serde::de::Error::custom("duplicate JSON object key"));
                        }
                        map.next_value::<UniqueKeys>()?;
                    }
                    Ok(UniqueKeys)
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> std::result::Result<UniqueKeys, A::Error> {
                    while seq.next_element::<UniqueKeys>()?.is_some() {}
                    Ok(UniqueKeys)
                }
                fn visit_bool<E: serde::de::Error>(
                    self,
                    _: bool,
                ) -> std::result::Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_i64<E: serde::de::Error>(
                    self,
                    _: i64,
                ) -> std::result::Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_u64<E: serde::de::Error>(
                    self,
                    _: u64,
                ) -> std::result::Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_f64<E: serde::de::Error>(
                    self,
                    _: f64,
                ) -> std::result::Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_str<E: serde::de::Error>(
                    self,
                    _: &str,
                ) -> std::result::Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }
    serde_json::from_slice::<UniqueKeys>(bytes).context("invalid or ambiguous source JSON")?;
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::Request,
        http::StatusCode,
        response::{IntoResponse, Response as AxumResponse},
        Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    struct Server {
        origin: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn server<F>(handler: F) -> Server
    where
        F: Fn(Request) -> AxumResponse + Clone + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(move |r: Request| {
            let response = handler(r);
            async move { response }
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Server { origin, task }
    }

    #[test]
    fn verification_rejects_same_size_tampering_and_unsupported_digests() {
        let digest = digest_bytes(b"abcd");
        assert!(verify_bytes(b"abcd", &digest, Some(4)).is_ok());
        assert!(verify_bytes(b"abce", &digest, Some(4))
            .unwrap_err()
            .to_string()
            .contains("digest mismatch"));
        assert!(verify_bytes(b"abcd", &digest, Some(5)).is_err());
        assert!(verify_bytes(b"abcd", "sha512:abcd", None).is_err());
    }

    #[test]
    fn endpoint_validation_rejects_plaintext_userinfo_and_query() {
        for value in [
            "http://registry.example.com",
            "https://user:password@registry.example.com",
            "https://registry.example.com?token=secret",
            "https://registry.example.com/path",
            "https://registry.example.com#fragment",
        ] {
            assert!(VerifiedRegistry::new(value, None, &[], Duration::from_secs(1)).is_err());
        }
        assert!(VerifiedRegistry::new(
            "https://registry.example.com",
            None,
            &[],
            Duration::from_secs(1)
        )
        .is_ok());
    }

    #[tokio::test]
    async fn unapproved_token_realm_is_never_contacted() {
        let contacted = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&contacted);
        let token_server = server(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
            "unexpected".into_response()
        })
        .await;
        let realm = format!("{}/token", token_server.origin);
        let registry = server(move |_| {
            (
                StatusCode::UNAUTHORIZED,
                [("www-authenticate", format!("Bearer realm=\"{realm}\""))],
                "",
            )
                .into_response()
        })
        .await;
        let mut client = VerifiedRegistry::for_test(&registry.origin).unwrap();
        client.credentials = Some(RegistryCredentials::new(
            "user".into(),
            Some("secret".into()),
        ));
        let error = client
            .metadata("test/agent", &digest_bytes(b"data"), None, true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cross-origin token endpoint"));
        assert_eq!(contacted.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn allowed_token_endpoint_gets_only_requested_pull_scope() {
        let token_requests = Arc::new(Mutex::new(Vec::new()));
        let records = Arc::clone(&token_requests);
        let token_server = server(move |request| {
            records.lock().unwrap().push((
                request.uri().to_string(),
                request
                    .headers()
                    .get("authorization")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
            ));
            "{\"token\":\"issued-token\"}".into_response()
        })
        .await;
        let realm = format!("{}/token", token_server.origin);
        let challenge_realm = realm.clone();
        let registry = server(move |request| {
            if request.headers().get("authorization").is_some_and(|v| v == "Bearer issued-token") { return "data".into_response(); }
            (StatusCode::UNAUTHORIZED, [("www-authenticate", format!("Bearer realm=\"{challenge_realm}\",service=\"test\",scope=\"repository:other/private:pull,push\""))], "").into_response()
        }).await;
        let mut client = VerifiedRegistry::build(
            &registry.origin,
            Some(&realm),
            &[],
            Duration::from_secs(2),
            true,
            None,
        )
        .unwrap();
        client.credentials = Some(RegistryCredentials::new(
            "user".into(),
            Some("secret".into()),
        ));
        assert_eq!(
            client
                .metadata("test/agent", &digest_bytes(b"data"), Some(4), true)
                .await
                .unwrap(),
            b"data"
        );
        let records = token_requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        let query = Url::parse(&format!("http://test{}", records[0].0)).unwrap();
        assert_eq!(
            query
                .query_pairs()
                .find(|(key, _)| key == "scope")
                .unwrap()
                .1,
            "repository:test/agent:pull"
        );
        assert!(records[0].1.starts_with("Basic "));
    }

    #[tokio::test]
    async fn disallowed_blob_redirect_is_never_contacted() {
        let contacted = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&contacted);
        let blob_server = server(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
            "data".into_response()
        })
        .await;
        let target = format!("{}/blob?signature=secret", blob_server.origin);
        let registry = server(move |_| {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [("location", target.clone())],
                "",
            )
                .into_response()
        })
        .await;
        let client = VerifiedRegistry::for_test(&registry.origin).unwrap();
        assert!(client
            .metadata("test/agent", &digest_bytes(b"data"), Some(4), false)
            .await
            .is_err());
        assert_eq!(contacted.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn same_origin_blob_redirect_strips_registry_credentials() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let records = Arc::clone(&observed);
        let registry = server(move |request| {
            records.lock().unwrap().push((
                request.uri().path().to_string(),
                request
                    .headers()
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string()),
            ));
            if request.uri().path() == "/download" {
                return "data".into_response();
            }
            (
                StatusCode::TEMPORARY_REDIRECT,
                [("location", "/download?signature=secret")],
                "",
            )
                .into_response()
        })
        .await;
        let mut client = VerifiedRegistry::for_test(&registry.origin).unwrap();
        client.credentials = Some(RegistryCredentials::new(
            "user".into(),
            Some("secret".into()),
        ));
        assert_eq!(
            client
                .metadata("test/agent", &digest_bytes(b"data"), Some(4), false)
                .await
                .unwrap(),
            b"data"
        );
        let records = observed.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records[0].1.as_deref().unwrap().starts_with("Basic "));
        assert!(records[1].1.is_none());
    }
}

#[cfg(test)]
#[test]
fn custom_registry_ca_rejects_invalid_material_and_plaintext_origin() {
    assert!(VerifiedRegistry::new_with_ca(
        "https://registry.example.test",
        None,
        &[],
        Duration::from_secs(2),
        b"not a certificate"
    )
    .is_err());
    assert!(VerifiedRegistry::new_with_ca(
        "https://registry.example.test",
        None,
        &[],
        Duration::from_secs(2),
        b""
    )
    .is_err());
    assert!(VerifiedRegistry::new_with_ca(
        "http://127.0.0.1",
        None,
        &[],
        Duration::from_secs(2),
        b"not a certificate"
    )
    .is_err());
}
