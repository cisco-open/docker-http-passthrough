// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::Stream;
use once_cell::sync::Lazy;
use opentelemetry::{
    global,
    propagation::Injector,
    trace::{Span, SpanKind, Status, TraceContextExt, Tracer},
    Context as OtelContext, KeyValue,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE},
    Client, RequestBuilder, Response, Url,
};
use serde::Deserialize;
use std::env;
use std::pin::Pin;
use tracing::{debug, info};

pub mod verified;

/// Base URL for the container registry V2 API.
/// Default: Docker Hub (registry-1.docker.io)
static REGISTRY_URL: Lazy<String> = Lazy::new(|| {
    env_value("REGISTRY_URL").unwrap_or_else(|| "https://registry-1.docker.io".to_string())
});

/// URL for the registry authentication endpoint.
/// Default: Docker Hub auth service
static AUTH_URL: Lazy<Option<String>> = Lazy::new(|| match env::var("REGISTRY_AUTH_URL") {
    Ok(value) => non_empty(value),
    Err(_) if registry_is_docker_hub() => Some("https://auth.docker.io/token".to_string()),
    Err(_) => None,
});

/// Service name used in authentication scope requests.
/// Default: Docker Hub service name
static REGISTRY_SERVICE: Lazy<Option<String>> = Lazy::new(|| match env::var("REGISTRY_SERVICE") {
    Ok(value) => non_empty(value),
    Err(_) if registry_is_docker_hub() => Some("registry.docker.io".to_string()),
    Err(_) => registry_host(),
});

/// Display name for the registry (used in telemetry/logging).
/// Default: docker.io
pub static REGISTRY_NAME: Lazy<String> =
    Lazy::new(|| env_value("REGISTRY_NAME").unwrap_or_else(|| "docker.io".to_string()));

static REGISTRY_USERNAME: Lazy<Option<String>> = Lazy::new(|| env_value("REGISTRY_USERNAME"));
static REGISTRY_PASSWORD: Lazy<Option<String>> = Lazy::new(|| env_value("REGISTRY_PASSWORD"));
static REGISTRY_BEARER_TOKEN: Lazy<Option<String>> =
    Lazy::new(|| env_value("REGISTRY_BEARER_TOKEN").or_else(|| env_value("REGISTRY_TOKEN")));

pub fn default_image_namespace() -> Option<String> {
    match env::var("REGISTRY_DEFAULT_NAMESPACE") {
        Ok(value) => non_empty(value),
        Err(_) if registry_is_docker_hub() => Some("library".to_string()),
        Err(_) => None,
    }
}

fn env_value(key: &str) -> Option<String> {
    env::var(key).ok().and_then(non_empty)
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn registry_is_docker_hub() -> bool {
    is_docker_hub_registry(REGISTRY_URL.as_str(), REGISTRY_NAME.as_str())
}

fn registry_host() -> Option<String> {
    Url::parse(REGISTRY_URL.as_str())
        .ok()
        .and_then(|url| url.host_str().map(ToString::to_string))
}

fn is_docker_hub_registry(registry_url: &str, registry_name: &str) -> bool {
    registry_name == "docker.io"
        || registry_url == "https://registry-1.docker.io"
        || registry_url == "https://index.docker.io"
}

fn static_registry_credentials() -> Option<RegistryCredentials> {
    REGISTRY_USERNAME
        .as_ref()
        .map(|username| RegistryCredentials {
            username: username.clone(),
            password: REGISTRY_PASSWORD.as_ref().cloned(),
        })
}

#[derive(Debug, Deserialize)]
struct AuthResponse {
    token: Option<String>,
    access_token: Option<String>,
}

impl AuthResponse {
    fn into_token(self) -> Result<String> {
        self.token
            .or(self.access_token)
            .ok_or_else(|| anyhow!("Registry auth response did not include a token"))
    }
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

#[derive(Clone)]
pub struct RegistryConfig {
    registry_url: String,
    auth_url: Option<String>,
    service: Option<String>,
    name: String,
    default_namespace: Option<String>,
}

impl RegistryConfig {
    pub fn from_env() -> Self {
        Self {
            registry_url: REGISTRY_URL.clone(),
            auth_url: AUTH_URL.clone(),
            service: REGISTRY_SERVICE.clone(),
            name: REGISTRY_NAME.clone(),
            default_namespace: default_image_namespace(),
        }
    }

    pub fn from_url_parts(scheme: &str, host: &str) -> Result<Self> {
        let scheme = scheme.trim().to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(anyhow!("Registry URL scheme must be http or https"));
        }

        let host = host.trim();
        if host.is_empty() || host.contains('/') {
            return Err(anyhow!("Registry host must be a hostname or host:port"));
        }

        let is_docker_hub = matches!(
            host,
            "docker.io" | "index.docker.io" | "registry-1.docker.io"
        );
        if is_docker_hub {
            return Ok(Self {
                registry_url: "https://registry-1.docker.io".to_string(),
                auth_url: Some("https://auth.docker.io/token".to_string()),
                service: Some("registry.docker.io".to_string()),
                name: "docker.io".to_string(),
                default_namespace: Some("library".to_string()),
            });
        }

        let registry_url = format!("{}://{}", scheme, host);
        Url::parse(&registry_url)?;

        Ok(Self {
            registry_url,
            auth_url: None,
            service: Some(host.to_string()),
            name: host.to_string(),
            default_namespace: None,
        })
    }

    pub fn registry_name(&self) -> &str {
        &self.name
    }

    pub fn registry_url(&self) -> &str {
        &self.registry_url
    }

    pub fn auth_url_configured(&self) -> bool {
        self.auth_url.is_some()
    }

    pub fn service(&self) -> Option<&str> {
        self.service.as_deref()
    }

    pub fn default_namespace(&self) -> Option<&str> {
        self.default_namespace.as_deref()
    }
}

pub struct RegistryClient {
    client: Client,
    config: RegistryConfig,
    request_credentials: Option<RegistryCredentials>,
}

#[derive(Clone)]
pub struct RegistryCredentials {
    username: String,
    password: Option<String>,
}

impl RegistryCredentials {
    pub fn new(username: String, password: Option<String>) -> Self {
        Self { username, password }
    }
}

#[derive(Clone)]
enum RegistryRequestAuth {
    None,
    Bearer(String),
    Basic(RegistryCredentials),
}

#[derive(Debug, PartialEq, Eq)]
struct AuthChallenge {
    scheme: String,
    realm: Option<String>,
    service: Option<String>,
    scope: Option<String>,
}

struct RegistryHttpAttempt<'a> {
    operation: &'static str,
    url: &'a str,
    url_template: &'static str,
    resend_count: i64,
    auth: &'a RegistryRequestAuth,
    parent_cx: &'a OtelContext,
}

struct HeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        let Ok(name) = HeaderName::from_bytes(key.as_bytes()) else {
            return;
        };
        let Ok(value) = HeaderValue::from_str(&value) else {
            return;
        };
        self.0.insert(name, value);
    }
}

impl RegistryClient {
    pub fn with_config_and_request_credentials(
        config: RegistryConfig,
        request_credentials: Option<RegistryCredentials>,
    ) -> Self {
        Self {
            client: Client::new(),
            config,
            request_credentials,
        }
    }

    pub fn registry_name(&self) -> &str {
        self.config.registry_name()
    }

    async fn initial_auth(
        &self,
        image: &str,
        parent_cx: &OtelContext,
    ) -> Result<RegistryRequestAuth> {
        if let Some(token) = REGISTRY_BEARER_TOKEN.as_ref() {
            return Ok(RegistryRequestAuth::Bearer(token.clone()));
        }

        if let Some(auth_url) = self.config.auth_url.as_ref() {
            return Ok(RegistryRequestAuth::Bearer(
                self.fetch_token_with_context(
                    "registry.auth.token.initial",
                    auth_url,
                    self.config.service.as_deref(),
                    Some(&Self::repository_scope(image)),
                    parent_cx,
                )
                .await?,
            ));
        }

        Ok(self
            .credentials()
            .map(RegistryRequestAuth::Basic)
            .unwrap_or(RegistryRequestAuth::None))
    }

    async fn retry_auth_from_challenge(
        &self,
        image: &str,
        challenge: &AuthChallenge,
        parent_cx: &OtelContext,
    ) -> Result<Option<RegistryRequestAuth>> {
        match challenge.scheme.as_str() {
            "bearer" => {
                let Some(realm) = challenge
                    .realm
                    .as_deref()
                    .or(self.config.auth_url.as_deref())
                else {
                    return Ok(None);
                };
                let service = challenge
                    .service
                    .as_deref()
                    .or(self.config.service.as_deref());
                let default_scope = Self::repository_scope(image);
                let scope = challenge.scope.as_deref().unwrap_or(default_scope.as_str());
                let token = self
                    .fetch_token_with_context(
                        "registry.auth.token.challenge",
                        realm,
                        service,
                        Some(scope),
                        parent_cx,
                    )
                    .await?;
                Ok(Some(RegistryRequestAuth::Bearer(token)))
            }
            "basic" => Ok(self.credentials().map(RegistryRequestAuth::Basic)),
            _ => Ok(None),
        }
    }

    async fn send_registry_request<F>(
        &self,
        image: &str,
        operation: &'static str,
        url: &str,
        url_template: &'static str,
        parent_cx: &OtelContext,
        build_request: F,
    ) -> Result<Response>
    where
        F: Fn() -> RequestBuilder,
    {
        let initial_auth = self.initial_auth(image, parent_cx).await?;
        let response = self
            .send_registry_attempt(
                RegistryHttpAttempt {
                    operation,
                    url,
                    url_template,
                    resend_count: 0,
                    auth: &initial_auth,
                    parent_cx,
                },
                &build_request,
            )
            .await?;

        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let challenge = auth_challenge(response.headers());
        let Some(challenge) = challenge else {
            return Ok(response);
        };

        let Some(retry_auth) = self
            .retry_auth_from_challenge(image, &challenge, parent_cx)
            .await?
        else {
            return Ok(response);
        };

        debug!(
            "Retrying registry request for {} using {} auth challenge",
            image, challenge.scheme
        );
        self.send_registry_attempt(
            RegistryHttpAttempt {
                operation,
                url,
                url_template,
                resend_count: 1,
                auth: &retry_auth,
                parent_cx,
            },
            build_request,
        )
        .await
    }

    async fn send_registry_attempt<F>(
        &self,
        attempt: RegistryHttpAttempt<'_>,
        build_request: F,
    ) -> Result<Response>
    where
        F: Fn() -> RequestBuilder,
    {
        let parsed_url = Url::parse(attempt.url)?;
        let tracer = global::tracer("docker-http-passthrough");
        let mut span = tracer
            .span_builder(format!("GET {}", attempt.url_template))
            .with_kind(SpanKind::Client)
            .start_with_context(&tracer, attempt.parent_cx);

        set_http_client_span_attributes(
            &mut span,
            "GET",
            &parsed_url,
            Some(attempt.url_template),
            attempt.resend_count,
        );
        span.set_attribute(KeyValue::new("registry.operation", attempt.operation));
        span.set_attribute(KeyValue::new(
            "registry.name",
            self.config.registry_name().to_string(),
        ));
        span.set_attribute(KeyValue::new("registry.auth.type", attempt.auth.kind()));
        let attempt_cx = attempt.parent_cx.with_span(span);

        let request = Self::inject_trace_context(
            Self::apply_auth(build_request(), attempt.auth),
            &attempt_cx,
        );
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                let span = attempt_cx.span();
                span.set_attribute(KeyValue::new("error.type", "reqwest.error"));
                span.set_status(Status::error(e.to_string()));
                span.end();
                return Err(e.into());
            }
        };

        let span = attempt_cx.span();
        let status = response.status();
        span.set_attribute(KeyValue::new(
            "http.response.status_code",
            status.as_u16() as i64,
        ));
        if status.is_client_error() || status.is_server_error() {
            let status_code = status.as_u16().to_string();
            span.set_attribute(KeyValue::new("error.type", status_code.clone()));
            span.set_status(Status::error(status_code));
        }
        span.end();
        Ok(response)
    }

    fn apply_auth(request: RequestBuilder, auth: &RegistryRequestAuth) -> RequestBuilder {
        match auth {
            RegistryRequestAuth::None => request,
            RegistryRequestAuth::Bearer(token) => request.bearer_auth(token),
            RegistryRequestAuth::Basic(credentials) => {
                request.basic_auth(&credentials.username, credentials.password.as_deref())
            }
        }
    }

    fn inject_trace_context(request: RequestBuilder, context: &OtelContext) -> RequestBuilder {
        let mut headers = HeaderMap::new();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(context, &mut HeaderInjector(&mut headers));
        });

        if headers.is_empty() {
            request
        } else {
            request.headers(headers)
        }
    }

    fn credentials(&self) -> Option<RegistryCredentials> {
        self.request_credentials
            .clone()
            .or_else(static_registry_credentials)
    }

    fn repository_scope(image: &str) -> String {
        format!("repository:{}:pull", image)
    }

    async fn fetch_token_with_context(
        &self,
        operation: &'static str,
        auth_url: &str,
        service: Option<&str>,
        scope: Option<&str>,
        parent_cx: &OtelContext,
    ) -> Result<String> {
        let mut url = Url::parse(auth_url)?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = service {
                query.append_pair("service", service);
            }
            if let Some(scope) = scope {
                query.append_pair("scope", scope);
            }
        }

        debug!("Fetching registry auth token");
        let tracer = global::tracer("docker-http-passthrough");
        let mut span = tracer
            .span_builder("GET {registry_token_endpoint}")
            .with_kind(SpanKind::Client)
            .start_with_context(&tracer, parent_cx);
        set_http_client_span_attributes(
            &mut span,
            "GET",
            &url,
            Some("{registry_token_endpoint}"),
            0,
        );
        span.set_attribute(KeyValue::new("registry.operation", operation));
        span.set_attribute(KeyValue::new(
            "registry.name",
            self.config.registry_name().to_string(),
        ));
        if let Some(service) = service {
            span.set_attribute(KeyValue::new("registry.auth.service", service.to_string()));
        }
        if scope.is_some() {
            span.set_attribute(KeyValue::new("registry.auth.scope_present", true));
        }
        let token_cx = parent_cx.with_span(span);

        let mut request = self.client.get(url);
        if let Some(credentials) = self.credentials() {
            token_cx
                .span()
                .set_attribute(KeyValue::new("registry.auth.type", "basic"));
            request = request.basic_auth(credentials.username, credentials.password);
        } else {
            token_cx
                .span()
                .set_attribute(KeyValue::new("registry.auth.type", "none"));
        }

        let response = match Self::inject_trace_context(request, &token_cx).send().await {
            Ok(response) => response,
            Err(e) => {
                let span = token_cx.span();
                span.set_attribute(KeyValue::new("error.type", "reqwest.error"));
                span.set_status(Status::error(e.to_string()));
                span.end();
                return Err(e.into());
            }
        };
        let status = response.status();
        let span = token_cx.span();
        span.set_attribute(KeyValue::new(
            "http.response.status_code",
            status.as_u16() as i64,
        ));
        if status.is_client_error() || status.is_server_error() {
            let status_code = status.as_u16().to_string();
            span.set_attribute(KeyValue::new("error.type", status_code.clone()));
            span.set_status(Status::error(status_code));
        }
        if !status.is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            span.end();
            return Err(anyhow!(
                "Failed to fetch registry auth token: {} - {}",
                status,
                body
            ));
        }

        let body = response.bytes().await?;
        span.set_attribute(KeyValue::new("http.response.body.size", body.len() as i64));
        span.end();
        serde_json::from_slice::<AuthResponse>(&body)?.into_token()
    }

    #[allow(dead_code)]
    async fn get_token(&self, image: &str) -> Result<String> {
        let scope = format!("repository:{}:pull", image);
        let auth_url = self
            .config
            .auth_url
            .as_deref()
            .ok_or_else(|| anyhow!("REGISTRY_AUTH_URL is not configured"))?;
        let cx = OtelContext::current();
        self.fetch_token_with_context(
            "registry.auth.token.manual",
            auth_url,
            self.config.service.as_deref(),
            Some(&scope),
            &cx,
        )
        .await
    }

    pub async fn get_manifest_with_context(
        &self,
        image: &str,
        tag: &str,
        parent_cx: &OtelContext,
    ) -> Result<(Manifest, Option<String>)> {
        let url = format!(
            "{}/v2/{}/manifests/{}",
            self.config.registry_url, image, tag
        );

        info!("Fetching manifest for {}:{}", image, tag);
        let response = self
            .send_registry_request(
                image,
                "registry.manifest.fetch",
                &url,
                "/v2/{repository}/manifests/{reference}",
                parent_cx,
                || {
                    self.client.get(&url).header(
                        "Accept",
                        "application/vnd.docker.distribution.manifest.v2+json, \
                         application/vnd.docker.distribution.manifest.list.v2+json, \
                         application/vnd.oci.image.manifest.v1+json, \
                         application/vnd.oci.image.index.v1+json",
                    )
                },
            )
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Failed to fetch manifest: {} - {}", status, body));
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

    pub async fn get_manifest_by_digest_with_context(
        &self,
        image: &str,
        digest: &str,
        parent_cx: &OtelContext,
    ) -> Result<(Manifest, Option<String>)> {
        let url = format!(
            "{}/v2/{}/manifests/{}",
            self.config.registry_url, image, digest
        );

        info!("Fetching manifest by digest for {}", image);
        let response = self
            .send_registry_request(
                image,
                "registry.manifest.fetch_by_digest",
                &url,
                "/v2/{repository}/manifests/{reference}",
                parent_cx,
                || {
                    self.client.get(&url).header(
                        "Accept",
                        "application/vnd.docker.distribution.manifest.v2+json, \
                         application/vnd.oci.image.manifest.v1+json",
                    )
                },
            )
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

    pub async fn get_config_with_context(
        &self,
        image: &str,
        digest: &str,
        parent_cx: &OtelContext,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/v2/{}/blobs/{}", self.config.registry_url, image, digest);

        info!("Fetching config blob for {}", image);
        let response = self
            .send_registry_request(
                image,
                "registry.config.fetch",
                &url,
                "/v2/{repository}/blobs/{digest}",
                parent_cx,
                || self.client.get(&url),
            )
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Failed to fetch config: {} - {}", status, body));
        }

        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }

    pub async fn get_layer_stream_with_context(
        &self,
        image: &str,
        digest: &str,
        parent_cx: &OtelContext,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>> {
        let url = format!("{}/v2/{}/blobs/{}", self.config.registry_url, image, digest);

        info!("Streaming layer {} for {}", digest, image);
        let response = self
            .send_registry_request(
                image,
                "registry.layer.fetch",
                &url,
                "/v2/{repository}/blobs/{digest}",
                parent_cx,
                || self.client.get(&url),
            )
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
        parent_cx: &OtelContext,
    ) -> Result<(Manifest, Option<String>)> {
        let (manifest, manifest_digest) = self
            .get_manifest_with_context(image, tag, parent_cx)
            .await?;

        // Check if this is a manifest list (multi-arch)
        if let Some(manifests) = &manifest.manifests {
            info!("Found manifest list, looking for amd64/linux");
            for entry in manifests {
                if let Some(platform) = &entry.platform {
                    if platform.architecture == "amd64" && platform.os == "linux" {
                        info!("Found amd64/linux manifest: {}", entry.digest);
                        let digest = entry.digest.clone();
                        let (resolved, _) = self
                            .get_manifest_by_digest_with_context(image, &digest, parent_cx)
                            .await?;
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

impl RegistryRequestAuth {
    fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer(_) => "bearer",
            Self::Basic(_) => "basic",
        }
    }
}

fn set_http_client_span_attributes(
    span: &mut impl Span,
    method: &'static str,
    url: &Url,
    url_template: Option<&'static str>,
    resend_count: i64,
) {
    span.set_attribute(KeyValue::new("http.request.method", method));
    span.set_attribute(KeyValue::new("url.full", url.as_str().to_string()));
    span.set_attribute(KeyValue::new("url.scheme", url.scheme().to_string()));
    span.set_attribute(KeyValue::new("network.protocol.name", "http"));
    span.set_attribute(KeyValue::new("network.transport", "tcp"));

    if let Some(host) = url.host_str() {
        span.set_attribute(KeyValue::new("server.address", host.to_string()));
    }
    if let Some(port) = url.port_or_known_default() {
        span.set_attribute(KeyValue::new("server.port", port as i64));
    }
    if let Some(url_template) = url_template {
        span.set_attribute(KeyValue::new("url.template", url_template));
    }
    if resend_count > 0 {
        span.set_attribute(KeyValue::new("http.request.resend_count", resend_count));
    }
}

fn auth_challenge(headers: &HeaderMap) -> Option<AuthChallenge> {
    headers
        .get(WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_authenticate_header)
}

fn parse_authenticate_header(value: &str) -> Option<AuthChallenge> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let (scheme, params) = value
        .split_once(' ')
        .map(|(scheme, params)| (scheme, params.trim()))
        .unwrap_or((value, ""));

    let mut challenge = AuthChallenge {
        scheme: scheme.to_ascii_lowercase(),
        realm: None,
        service: None,
        scope: None,
    };

    for param in split_auth_params(params) {
        let Some((key, raw_value)) = param.split_once('=') else {
            continue;
        };
        let parsed_value = raw_value.trim().trim_matches('"').to_string();
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => challenge.realm = Some(parsed_value),
            "service" => challenge.service = Some(parsed_value),
            "scope" => challenge.scope = Some(parsed_value),
            _ => {}
        }
    }

    Some(challenge)
}

fn split_auth_params(params: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in params.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ',' if !in_quotes => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }

    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_authenticate_header() {
        let challenge = parse_authenticate_header(
            r#"Bearer realm="https://registry.example.com/token",service="registry.example.com",scope="repository:team/app:pull""#,
        )
        .unwrap();

        assert_eq!(challenge.scheme, "bearer");
        assert_eq!(
            challenge.realm,
            Some("https://registry.example.com/token".to_string())
        );
        assert_eq!(challenge.service, Some("registry.example.com".to_string()));
        assert_eq!(
            challenge.scope,
            Some("repository:team/app:pull".to_string())
        );
    }

    #[test]
    fn parses_basic_authenticate_header() {
        let challenge = parse_authenticate_header(r#"Basic realm="private registry""#).unwrap();

        assert_eq!(challenge.scheme, "basic");
        assert_eq!(challenge.realm, Some("private registry".to_string()));
        assert_eq!(challenge.service, None);
        assert_eq!(challenge.scope, None);
    }

    #[test]
    fn splits_quoted_commas_in_auth_params() {
        let parts = split_auth_params(r#"realm="a,b",service="registry.example.com""#);

        assert_eq!(
            parts,
            vec!["realm=\"a,b\"", "service=\"registry.example.com\""]
        );
    }

    #[test]
    fn docker_hub_aliases_use_canonical_registry_settings() {
        for host in ["docker.io", "index.docker.io", "registry-1.docker.io"] {
            let config = RegistryConfig::from_url_parts("http", host).unwrap();

            assert_eq!(config.registry_url(), "https://registry-1.docker.io");
            assert_eq!(config.registry_name(), "docker.io");
            assert_eq!(config.service(), Some("registry.docker.io"));
            assert_eq!(config.default_namespace(), Some("library"));
            assert!(config.auth_url_configured());
        }
    }

    #[test]
    fn request_selected_registries_accept_https_http_and_ports() {
        let ghcr = RegistryConfig::from_url_parts("https", "ghcr.io").unwrap();
        assert_eq!(ghcr.registry_url(), "https://ghcr.io");
        assert_eq!(ghcr.registry_name(), "ghcr.io");
        assert_eq!(ghcr.service(), Some("ghcr.io"));
        assert_eq!(ghcr.default_namespace(), None);
        assert!(!ghcr.auth_url_configured());

        let local = RegistryConfig::from_url_parts("http", "registry2.example.com:5000").unwrap();
        assert_eq!(local.registry_url(), "http://registry2.example.com:5000");
        assert_eq!(local.registry_name(), "registry2.example.com:5000");
        assert_eq!(local.service(), Some("registry2.example.com:5000"));
        assert_eq!(local.default_namespace(), None);
        assert!(!local.auth_url_configured());
    }

    #[test]
    fn request_selected_registries_reject_invalid_targets() {
        assert!(RegistryConfig::from_url_parts("ftp", "registry.example.com").is_err());
        assert!(RegistryConfig::from_url_parts("https", "").is_err());
        assert!(RegistryConfig::from_url_parts("https", "registry.example.com/path").is_err());
    }
}
