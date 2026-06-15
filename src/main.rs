// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

mod registry;
mod tar_builder;
mod telemetry;

use anyhow::Result;
use axum::{
    body::Body,
    extract::{ConnectInfo, Path},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use bytes::Bytes;
use futures::Stream;
use opentelemetry::{
    global,
    propagation::Extractor,
    trace::{Span, SpanKind, Status, TraceContextExt, Tracer},
    Context as OtelContext,
};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Instant,
};
use tokio_util::io::ReaderStream;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use crate::registry::{RegistryClient, RegistryConfig, RegistryCredentials};
use crate::tar_builder::ImageTarBuilder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientRegistryAuthMode {
    Off,
    Optional,
    Required,
}

impl ClientRegistryAuthMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum RegistryTargetSource {
    Environment,
    RequestUrl,
}

impl RegistryTargetSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::RequestUrl => "request_url",
        }
    }
}

struct DownloadRouteContext {
    registry_config: RegistryConfig,
    client_auth_mode: ClientRegistryAuthMode,
    request_path: String,
    route_template: &'static str,
    registry_target_source: RegistryTargetSource,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Initialize OpenTelemetry (conditional based on .env)
    let _telemetry_guard = telemetry::init_telemetry();

    let app = Router::new()
        .route("/docker/:filename", get(handle_docker_download))
        .route(
            "/registry/:scheme/:host/docker/:filename",
            get(handle_registry_download),
        )
        .route(
            "/registry-auth/:scheme/:host/docker/:filename",
            get(handle_registry_auth_download),
        )
        .route("/health", get(health_check));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    info!("Starting Docker HTTP Passthrough server on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

async fn health_check() -> &'static str {
    "OK"
}

fn client_registry_auth_mode() -> ClientRegistryAuthMode {
    match std::env::var("REGISTRY_CLIENT_AUTH")
        .unwrap_or_else(|_| "optional".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "required" | "require" | "challenge" | "on" | "true" | "1" => {
            ClientRegistryAuthMode::Required
        }
        "off" | "false" | "0" | "disabled" | "ignore" => ClientRegistryAuthMode::Off,
        _ => ClientRegistryAuthMode::Optional,
    }
}

fn registry_credentials_from_headers(
    headers: &HeaderMap,
) -> Result<Option<RegistryCredentials>, String> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };

    let value = value
        .to_str()
        .map_err(|_| "Invalid Authorization header".to_string())?
        .trim();

    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next().unwrap_or_default();
    if !scheme.eq_ignore_ascii_case("Basic") {
        return Ok(None);
    };
    let encoded = parts.next().unwrap_or_default().trim();

    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid Basic Authorization value".to_string())?;
    let decoded = String::from_utf8(decoded)
        .map_err(|_| "Basic Authorization value is not valid UTF-8".to_string())?;

    let Some((username, password)) = decoded.split_once(':') else {
        return Err("Basic Authorization value must contain username and password".to_string());
    };

    if username.trim().is_empty() {
        return Err("Basic Authorization username cannot be empty".to_string());
    }

    Ok(Some(RegistryCredentials::new(
        username.to_string(),
        Some(password.to_string()),
    )))
}

fn client_auth_challenge_response(status: StatusCode, message: impl Into<String>) -> Response {
    Response::builder()
        .status(status)
        .header(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"registry credentials\"",
        )
        .body(Body::from(message.into()))
        .unwrap()
        .into_response()
}

struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|name| name.as_str()).collect()
    }
}

fn start_request_span(
    headers: &HeaderMap,
    socket_addr: &SocketAddr,
    filename: &str,
    route: &DownloadRouteContext,
) -> OtelContext {
    let tracer = global::tracer("docker-http-passthrough");
    let remote_parent =
        global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)));
    let mut span = tracer
        .span_builder(format!("GET {}", route.route_template))
        .with_kind(SpanKind::Server)
        .start_with_context(&tracer, &remote_parent);

    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http".to_string());

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let forwarded_port = headers
        .get("x-forwarded-port")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u16>().ok());

    let (server_address, server_port) = match host.as_deref() {
        Some(h) => match h.rsplit_once(':') {
            Some((addr, port)) => {
                let port = port.parse::<u16>().ok();
                (addr.to_string(), port)
            }
            None => (h.to_string(), None),
        },
        None => ("0.0.0.0".to_string(), Some(8080)),
    };

    let server_port = server_port.or(forwarded_port).or(Some(8080));
    let url_authority = match (host.as_deref(), server_port) {
        (Some(h), Some(p)) if !h.contains(':') => format!("{}:{}", h, p),
        (Some(h), _) => h.to_string(),
        (None, Some(p)) => format!("localhost:{}", p),
        (None, None) => "localhost".to_string(),
    };
    let url_full = format!("{}://{}{}", scheme, url_authority, route.request_path);

    span.set_attribute(opentelemetry::KeyValue::new("http.request.method", "GET"));
    span.set_attribute(opentelemetry::KeyValue::new(
        "http.route",
        route.route_template,
    ));
    span.set_attribute(opentelemetry::KeyValue::new("url.scheme", scheme));
    span.set_attribute(opentelemetry::KeyValue::new(
        "url.path",
        route.request_path.clone(),
    ));
    span.set_attribute(opentelemetry::KeyValue::new("url.full", url_full));
    span.set_attribute(opentelemetry::KeyValue::new(
        "network.protocol.name",
        "http",
    ));
    span.set_attribute(opentelemetry::KeyValue::new("network.transport", "tcp"));
    span.set_attribute(opentelemetry::KeyValue::new(
        "server.address",
        server_address,
    ));
    if let Some(p) = server_port {
        span.set_attribute(opentelemetry::KeyValue::new("server.port", p as i64));
    }
    span.set_attribute(opentelemetry::KeyValue::new(
        "network.peer.address",
        socket_addr.ip().to_string(),
    ));
    span.set_attribute(opentelemetry::KeyValue::new(
        "network.peer.port",
        socket_addr.port() as i64,
    ));
    span.set_attribute(opentelemetry::KeyValue::new(
        "client.address",
        get_client_ip(headers, socket_addr),
    ));
    if let Some(ua) = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        span.set_attribute(opentelemetry::KeyValue::new(
            "user_agent.original",
            ua.to_string(),
        ));
    }

    span.set_attribute(opentelemetry::KeyValue::new(
        "file.name",
        filename.to_string(),
    ));
    span.set_attribute(opentelemetry::KeyValue::new(
        "registry.name",
        route.registry_config.registry_name().to_string(),
    ));
    span.set_attribute(opentelemetry::KeyValue::new(
        "registry.url",
        route.registry_config.registry_url().to_string(),
    ));
    span.set_attribute(opentelemetry::KeyValue::new(
        "registry.target.source",
        route.registry_target_source.as_str(),
    ));
    span.set_attribute(opentelemetry::KeyValue::new(
        "registry.auth.configured",
        route.registry_config.auth_url_configured(),
    ));
    if let Some(service) = route.registry_config.service() {
        span.set_attribute(opentelemetry::KeyValue::new(
            "registry.auth.service",
            service.to_string(),
        ));
    }
    if let Some(namespace) = route.registry_config.default_namespace() {
        span.set_attribute(opentelemetry::KeyValue::new(
            "registry.default_namespace",
            namespace.to_string(),
        ));
    }
    span.set_attribute(opentelemetry::KeyValue::new(
        "registry.client_auth.mode",
        route.client_auth_mode.as_str(),
    ));

    let container_name = std::env::var("CONTAINER_NAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "docker-http-passthrough".to_string());
    span.set_attribute(opentelemetry::KeyValue::new(
        "container.name",
        container_name,
    ));
    if let Ok(hostname) = std::env::var("HOSTNAME") {
        if !hostname.trim().is_empty() {
            span.set_attribute(opentelemetry::KeyValue::new("host.name", hostname.clone()));
            span.set_attribute(opentelemetry::KeyValue::new(
                "service.instance.id",
                hostname.clone(),
            ));
            span.set_attribute(opentelemetry::KeyValue::new("container.id", hostname));
        }
    }

    remote_parent.with_span(span)
}

fn finish_request_span(parent_cx: &OtelContext, status: StatusCode) {
    let span = parent_cx.span();
    span.set_attribute(opentelemetry::KeyValue::new(
        "http.response.status_code",
        status.as_u16() as i64,
    ));
    if status.is_server_error() {
        let status_code = status.as_u16().to_string();
        span.set_attribute(opentelemetry::KeyValue::new(
            "error.type",
            status_code.clone(),
        ));
        span.set_status(Status::error(status_code));
    }
    span.end();
}

/// Parse filename like "nginx_latest.tar" into ("library/nginx", "latest"),
/// "library_nginx_1.25.tar" into ("library/nginx", "1.25"), or
/// "team__platform__api_1.0.tar" into ("team/platform/api", "1.0").
fn parse_image_name(filename: &str, default_namespace: Option<&str>) -> Option<(String, String)> {
    // Remove .tar extension
    let name = filename.strip_suffix(".tar")?;

    // Find the last underscore which separates the tag
    let last_underscore = name.rfind('_')?;
    let (image_part, tag) = name.split_at(last_underscore);
    let tag = &tag[1..]; // Remove the leading underscore

    if image_part.is_empty() || tag.is_empty() {
        return None;
    }

    let image_name = if image_part.contains("__") {
        image_part.replace("__", "/")
    } else if image_part.contains('_') {
        // Preserve the historical namespace_image format.
        let first_underscore = image_part.find('_')?;
        let (namespace, rest) = image_part.split_at(first_underscore);
        format!("{}/{}", namespace, &rest[1..])
    } else if let Some(namespace) = default_namespace {
        format!("{}/{}", namespace, image_part)
    } else {
        image_part.to_string()
    };

    Some((image_name, tag.to_string()))
}

async fn handle_docker_download(
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(filename): Path<String>,
) -> Response {
    let registry_config = RegistryConfig::from_env();
    let request_path = format!("/docker/{}", filename);
    handle_image_download(
        socket_addr,
        headers,
        filename,
        DownloadRouteContext {
            registry_config,
            client_auth_mode: client_registry_auth_mode(),
            request_path,
            route_template: "/docker/{filename}",
            registry_target_source: RegistryTargetSource::Environment,
        },
    )
    .await
}

async fn handle_registry_download(
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((scheme, host, filename)): Path<(String, String, String)>,
) -> Response {
    let registry_config = match RegistryConfig::from_url_parts(&scheme, &host) {
        Ok(config) => config,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid registry target: {}", e),
            )
                .into_response()
        }
    };
    let request_path = format!("/registry/{}/{}/docker/{}", scheme, host, filename);
    handle_image_download(
        socket_addr,
        headers,
        filename,
        DownloadRouteContext {
            registry_config,
            client_auth_mode: ClientRegistryAuthMode::Optional,
            request_path,
            route_template: "/registry/{scheme}/{host}/docker/{filename}",
            registry_target_source: RegistryTargetSource::RequestUrl,
        },
    )
    .await
}

async fn handle_registry_auth_download(
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((scheme, host, filename)): Path<(String, String, String)>,
) -> Response {
    let registry_config = match RegistryConfig::from_url_parts(&scheme, &host) {
        Ok(config) => config,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid registry target: {}", e),
            )
                .into_response()
        }
    };
    let request_path = format!("/registry-auth/{}/{}/docker/{}", scheme, host, filename);
    handle_image_download(
        socket_addr,
        headers,
        filename,
        DownloadRouteContext {
            registry_config,
            client_auth_mode: ClientRegistryAuthMode::Required,
            request_path,
            route_template: "/registry-auth/{scheme}/{host}/docker/{filename}",
            registry_target_source: RegistryTargetSource::RequestUrl,
        },
    )
    .await
}

async fn handle_image_download(
    socket_addr: SocketAddr,
    headers: HeaderMap,
    filename: String,
    route: DownloadRouteContext,
) -> Response {
    let client_auth_mode = route.client_auth_mode;
    let registry_config = &route.registry_config;

    // Extract real client IP from X-Forwarded-For or X-Real-IP headers, fallback to socket address
    let client_ip = get_client_ip(&headers, &socket_addr);

    info!("[{}] Received request for: {}", client_ip, filename);

    let parent_cx = start_request_span(&headers, &socket_addr, &filename, &route);
    let tracer = global::tracer("docker-http-passthrough");

    // Parse the filename to extract image name and tag
    let parse_span = tracer
        .span_builder("parse_image_request")
        .with_kind(SpanKind::Internal)
        .start_with_context(&tracer, &parent_cx);
    let parse_cx = parent_cx.with_span(parse_span);
    parse_cx
        .span()
        .set_attribute(opentelemetry::KeyValue::new("file.name", filename.clone()));
    if let Some(namespace) = registry_config.default_namespace() {
        parse_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "registry.default_namespace",
            namespace.to_string(),
        ));
    }
    let parsed_image = parse_image_name(&filename, registry_config.default_namespace());
    let (image_name, tag) = match parsed_image {
        Some(parsed) => parsed,
        None => {
            parse_cx.span().set_attribute(opentelemetry::KeyValue::new(
                "error.type",
                "invalid_image_filename",
            ));
            parse_cx
                .span()
                .set_status(Status::error("invalid image filename"));
            parse_cx.span().end();
            finish_request_span(&parent_cx, StatusCode::BAD_REQUEST);
            error!("Invalid filename format: {}", filename);
            return (
                StatusCode::BAD_REQUEST,
                "Invalid filename format. Expected: imagename_tag.tar, namespace_imagename_tag.tar, or nested__repo__image_tag.tar",
            )
                .into_response();
        }
    };
    parse_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "container.image.name",
        format!("{}/{}", registry_config.registry_name(), image_name),
    ));
    parse_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "container.image.tag",
        tag.clone(),
    ));
    parse_cx.span().set_status(Status::Ok);
    parse_cx.span().end();

    info!("[{}] Parsed image: {}:{}", client_ip, image_name, tag);

    parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "container.image.name",
        format!("{}/{}", registry_config.registry_name(), image_name),
    ));
    parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "container.image.tag",
        tag.clone(),
    ));

    let auth_span = tracer
        .span_builder("extract_registry_client_credentials")
        .with_kind(SpanKind::Internal)
        .start_with_context(&tracer, &parent_cx);
    let auth_cx = parent_cx.with_span(auth_span);
    auth_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "registry.client_auth.mode",
        client_auth_mode.as_str(),
    ));
    auth_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "http.request.header.authorization.present",
        headers.contains_key(header::AUTHORIZATION),
    ));

    let request_credentials = match registry_credentials_from_headers(&headers) {
        Ok(credentials) => credentials,
        Err(message) => {
            auth_cx.span().set_attribute(opentelemetry::KeyValue::new(
                "error.type",
                "invalid_authorization_header",
            ));
            auth_cx.span().set_status(Status::error(message.clone()));
            auth_cx.span().end();
            finish_request_span(&parent_cx, StatusCode::BAD_REQUEST);
            return client_auth_challenge_response(StatusCode::BAD_REQUEST, message);
        }
    };
    auth_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "registry.client_auth.credentials_present",
        request_credentials.is_some(),
    ));

    if client_auth_mode == ClientRegistryAuthMode::Required && request_credentials.is_none() {
        auth_cx.span().set_attribute(opentelemetry::KeyValue::new(
            "registry.client_auth.challenge_sent",
            true,
        ));
        auth_cx.span().set_status(Status::Ok);
        auth_cx.span().end();
        finish_request_span(&parent_cx, StatusCode::UNAUTHORIZED);
        return client_auth_challenge_response(
            StatusCode::UNAUTHORIZED,
            "Registry credentials are required for this passthrough endpoint",
        );
    }

    let request_credentials = if client_auth_mode == ClientRegistryAuthMode::Off {
        None
    } else {
        request_credentials
    };
    auth_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "registry.client_auth.credentials_used",
        request_credentials.is_some(),
    ));
    auth_cx.span().set_status(Status::Ok);
    auth_cx.span().end();

    // Create registry client
    let client = RegistryClient::with_config_and_request_credentials(
        route.registry_config,
        request_credentials,
    );

    // Prepare the image before committing a 200 response to the client.
    let tar_builder = match ImageTarBuilder::prepare_with_context(
        client,
        image_name.clone(),
        tag.clone(),
        parent_cx.clone(),
    )
    .await
    {
        Ok(builder) => builder,
        Err(e) => {
            let status = StatusCode::BAD_GATEWAY;
            let span = parent_cx.span();
            span.set_attribute(opentelemetry::KeyValue::new(
                "http.response.status_code",
                status.as_u16() as i64,
            ));
            span.set_attribute(opentelemetry::KeyValue::new(
                "error.type",
                "registry_prepare_failed",
            ));
            span.set_status(Status::error(e.to_string()));
            span.end();
            error!(
                "[{}] Error preparing tar for {}:{}: {}",
                client_ip, image_name, tag, e
            );
            return (
                status,
                format!("Failed to prepare image {}:{}: {}", image_name, tag, e),
            )
                .into_response();
        }
    };

    parent_cx.span().set_attribute(opentelemetry::KeyValue::new(
        "http.response.status_code",
        200_i64,
    ));

    // Create a pipe for streaming
    let (writer, reader) = tokio::io::duplex(64 * 1024); // 64KB buffer

    let transfer_state = Arc::new(TransferState::default());

    // Spawn task to build tar archive with parent context for tracing
    let image_name_clone = image_name.clone();
    let tag_clone = tag.clone();
    let client_ip_clone = client_ip.clone();
    let spawn_cx = parent_cx.clone();
    let spawn_transfer_state = Arc::clone(&transfer_state);
    tokio::spawn(async move {
        let result = tar_builder
            .build_tar_with_context(writer, spawn_cx.clone())
            .await;

        match &result {
            Ok(_) => {
                spawn_cx.span().set_status(Status::Ok);
            }
            Err(e) => {
                spawn_transfer_state.finish_error(
                    &spawn_cx,
                    "response_stream_failed",
                    e.to_string(),
                );
                error!(
                    "[{}] Error building tar for {}:{}: {}",
                    client_ip_clone, image_name_clone, tag_clone, e
                );
            }
        }
    });

    // Stream the response with logging wrapper
    let stream = ReaderStream::new(reader);
    let logged_stream = LoggingStream::new(
        stream,
        transfer_state,
        parent_cx.clone(),
        client_ip,
        filename.clone(),
        image_name.clone(),
        tag.clone(),
    );
    let body = Body::from_stream(logged_stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-tar")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .header(header::TRANSFER_ENCODING, "chunked")
        // Keep-alive headers to prevent timeout
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap()
        .into_response()
}

/// Extract real client IP from headers or fallback to socket address
fn get_client_ip(headers: &HeaderMap, socket_addr: &SocketAddr) -> String {
    // Try X-Forwarded-For header first (may contain multiple IPs, take the first one)
    if let Some(forwarded_for) = headers.get("x-forwarded-for") {
        if let Ok(value) = forwarded_for.to_str() {
            // X-Forwarded-For can be comma-separated, take the first (original client)
            if let Some(first_ip) = value.split(',').next() {
                let ip = first_ip.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }

    // Try X-Real-IP header
    if let Some(real_ip) = headers.get("x-real-ip") {
        if let Ok(value) = real_ip.to_str() {
            let ip = value.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
    }

    // Fallback to socket address (just the IP, not the port)
    socket_addr.ip().to_string()
}

/// A wrapper stream that logs bytes sent to the client
struct LoggingStream<S> {
    inner: S,
    transfer_state: Arc<TransferState>,
    otel_cx: OtelContext,
    client_ip: String,
    filename: String,
    image: String,
    tag: String,
    start_time: Instant,
    last_log_bytes: u64,
}

#[derive(Default)]
struct TransferState {
    bytes_sent: AtomicU64,
    span_ended: AtomicBool,
}

impl TransferState {
    fn add_bytes(&self, bytes: u64) -> u64 {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed) + bytes
    }

    fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    fn is_finished(&self) -> bool {
        self.span_ended.load(Ordering::Acquire)
    }

    fn finish_ok(&self, otel_cx: &OtelContext) {
        if self.span_ended.swap(true, Ordering::AcqRel) {
            return;
        }

        let span = otel_cx.span();
        span.set_attribute(opentelemetry::KeyValue::new(
            "http.response.body.size",
            self.bytes_sent() as i64,
        ));
        span.set_status(Status::Ok);
        span.end();
    }

    fn finish_error(&self, otel_cx: &OtelContext, error_type: &'static str, message: String) {
        if self.span_ended.swap(true, Ordering::AcqRel) {
            return;
        }

        let span = otel_cx.span();
        span.set_attribute(opentelemetry::KeyValue::new(
            "http.response.body.size",
            self.bytes_sent() as i64,
        ));
        span.set_attribute(opentelemetry::KeyValue::new("error.type", error_type));
        span.set_status(Status::error(message));
        span.end();
    }
}

impl<S> LoggingStream<S> {
    fn new(
        inner: S,
        transfer_state: Arc<TransferState>,
        otel_cx: OtelContext,
        client_ip: String,
        filename: String,
        image: String,
        tag: String,
    ) -> Self {
        info!(
            "[{}] Starting transfer of {} ({}:{})",
            client_ip, filename, image, tag
        );
        Self {
            inner,
            transfer_state,
            otel_cx,
            client_ip,
            filename,
            image,
            tag,
            start_time: Instant::now(),
            last_log_bytes: 0,
        }
    }

    fn format_bytes(bytes: u64) -> String {
        if bytes >= 1024 * 1024 * 1024 {
            format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        } else if bytes >= 1024 * 1024 {
            format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
        } else if bytes >= 1024 {
            format!("{:.2} KB", bytes as f64 / 1024.0)
        } else {
            format!("{} bytes", bytes)
        }
    }
}

impl<S> Stream for LoggingStream<S>
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let inner = Pin::new(&mut self.inner);
        match inner.poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let chunk_len = chunk.len() as u64;
                let total = self.transfer_state.add_bytes(chunk_len);

                // Log progress every 1MB
                if total - self.last_log_bytes >= 1024 * 1024 {
                    let elapsed = self.start_time.elapsed().as_secs_f64();
                    let speed = if elapsed > 0.0 {
                        Self::format_bytes((total as f64 / elapsed) as u64)
                    } else {
                        "N/A".to_string()
                    };
                    info!(
                        "[{}] Transfer progress: {} sent ({}/s) - {}",
                        self.client_ip,
                        Self::format_bytes(total),
                        speed,
                        self.filename
                    );
                    self.last_log_bytes = total;
                }

                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                let total = self.transfer_state.bytes_sent();
                self.transfer_state.finish_error(
                    &self.otel_cx,
                    "response_stream_error",
                    e.to_string(),
                );
                error!(
                    "[{}] Transfer error after {}: {} - {}",
                    self.client_ip,
                    Self::format_bytes(total),
                    e,
                    self.filename
                );
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                let total = self.transfer_state.bytes_sent();
                let elapsed = self.start_time.elapsed();
                self.transfer_state.finish_ok(&self.otel_cx);
                let speed = if elapsed.as_secs_f64() > 0.0 {
                    Self::format_bytes((total as f64 / elapsed.as_secs_f64()) as u64)
                } else {
                    "N/A".to_string()
                };
                info!(
                    "[{}] Transfer complete: {} sent in {:.2}s ({}/s) - {} ({}:{})",
                    self.client_ip,
                    Self::format_bytes(total),
                    elapsed.as_secs_f64(),
                    speed,
                    self.filename,
                    self.image,
                    self.tag
                );
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for LoggingStream<S> {
    fn drop(&mut self) {
        if self.transfer_state.is_finished() {
            return;
        }

        let total = self.transfer_state.bytes_sent();
        self.transfer_state.finish_error(
            &self.otel_cx,
            "response_stream_aborted",
            "response stream dropped before completion".to_string(),
        );
        info!(
            "[{}] Transfer aborted after {} - {} ({}:{})",
            self.client_ip,
            Self::format_bytes(total),
            self.filename,
            self.image,
            self.tag
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use flate2::{write::GzEncoder, Compression};
    use std::{
        collections::HashMap,
        io::{Cursor, Write as _},
        sync::{Arc, Mutex},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
    };

    const TEST_IMAGE: &str = "hello";
    const TEST_FILENAME: &str = "hello_latest.tar";
    const TEST_TAG: &str = "latest";
    const TEST_USER: &str = "testuser";
    const TEST_PASSWORD: &str = "testpass";
    const TEST_TOKEN: &str = "issued-registry-token";
    const CONFIG_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LAYER_DIGEST: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const MANIFEST_DIGEST: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RouteCase {
        DockerOff,
        DockerOptional,
        DockerRequired,
        Registry,
        RegistryAuth,
    }

    impl RouteCase {
        const ALL: [Self; 5] = [
            Self::DockerOff,
            Self::DockerOptional,
            Self::DockerRequired,
            Self::Registry,
            Self::RegistryAuth,
        ];

        fn name(self) -> &'static str {
            match self {
                Self::DockerOff => "docker/off",
                Self::DockerOptional => "docker/optional",
                Self::DockerRequired => "docker/required",
                Self::Registry => "registry/optional",
                Self::RegistryAuth => "registry-auth/required",
            }
        }

        fn auth_mode(self) -> ClientRegistryAuthMode {
            match self {
                Self::DockerOff => ClientRegistryAuthMode::Off,
                Self::DockerOptional | Self::Registry => ClientRegistryAuthMode::Optional,
                Self::DockerRequired | Self::RegistryAuth => ClientRegistryAuthMode::Required,
            }
        }

        fn route_template(self) -> &'static str {
            match self {
                Self::DockerOff | Self::DockerOptional | Self::DockerRequired => {
                    "/docker/{filename}"
                }
                Self::Registry => "/registry/{scheme}/{host}/docker/{filename}",
                Self::RegistryAuth => "/registry-auth/{scheme}/{host}/docker/{filename}",
            }
        }

        fn target_source(self) -> RegistryTargetSource {
            match self {
                Self::DockerOff | Self::DockerOptional | Self::DockerRequired => {
                    RegistryTargetSource::Environment
                }
                Self::Registry | Self::RegistryAuth => RegistryTargetSource::RequestUrl,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ClientCredentialCase {
        Missing,
        Valid,
        Invalid,
    }

    impl ClientCredentialCase {
        fn name(self) -> &'static str {
            match self {
                Self::Missing => "missing-client-basic",
                Self::Valid => "valid-client-basic",
                Self::Invalid => "invalid-client-basic",
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum UpstreamAuthPolicy {
        Anonymous,
        DirectBasic,
        BearerChallenge,
    }

    impl UpstreamAuthPolicy {
        const ALL: [Self; 3] = [Self::Anonymous, Self::DirectBasic, Self::BearerChallenge];

        fn name(self) -> &'static str {
            match self {
                Self::Anonymous => "anonymous-registry",
                Self::DirectBasic => "direct-basic-registry",
                Self::BearerChallenge => "bearer-challenge-registry",
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ManifestMode {
        Single,
        ListThenSingle,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum LayerEncoding {
        PlainTar,
        GzipTar,
    }

    #[derive(Clone, Debug)]
    struct RegistryRequestRecord {
        path: String,
        authorization: Option<String>,
    }

    struct FakeRegistryData {
        policy: UpstreamAuthPolicy,
        mode: ManifestMode,
        layer_media_type: &'static str,
        layer_bytes: Vec<u8>,
        config_bytes: Vec<u8>,
        manifest_bytes: Vec<u8>,
        manifest_list_bytes: Vec<u8>,
        basic_header: String,
        token: String,
        base_url: String,
    }

    struct FakeRegistry {
        host: String,
        records: Arc<Mutex<Vec<RegistryRequestRecord>>>,
        task: JoinHandle<()>,
    }

    impl FakeRegistry {
        async fn start(policy: UpstreamAuthPolicy) -> Self {
            Self::start_with_options(policy, ManifestMode::Single, LayerEncoding::PlainTar).await
        }

        async fn start_with_options(
            policy: UpstreamAuthPolicy,
            mode: ManifestMode,
            encoding: LayerEncoding,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let host = addr.to_string();
            let base_url = format!("http://{}", host);
            let layer_tar = make_layer_tar();
            let (layer_media_type, layer_bytes) = match encoding {
                LayerEncoding::PlainTar => ("application/vnd.oci.image.layer.v1.tar", layer_tar),
                LayerEncoding::GzipTar => (
                    "application/vnd.docker.image.rootfs.diff.tar.gzip",
                    gzip_bytes(&layer_tar),
                ),
            };
            let config_bytes = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec();
            let manifest_bytes =
                manifest_bytes(layer_media_type, config_bytes.len(), layer_bytes.len());
            let manifest_list_bytes = manifest_list_bytes();
            let records = Arc::new(Mutex::new(Vec::new()));
            let data = Arc::new(FakeRegistryData {
                policy,
                mode,
                layer_media_type,
                layer_bytes,
                config_bytes,
                manifest_bytes,
                manifest_list_bytes,
                basic_header: expected_basic_header(),
                token: TEST_TOKEN.to_string(),
                base_url,
            });

            let task_records = Arc::clone(&records);
            let task_data = Arc::clone(&data);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let connection_records = Arc::clone(&task_records);
                    let connection_data = Arc::clone(&task_data);
                    tokio::spawn(async move {
                        handle_fake_registry_connection(
                            stream,
                            connection_data,
                            connection_records,
                        )
                        .await;
                    });
                }
            });

            Self {
                host,
                records,
                task,
            }
        }

        fn records(&self) -> Vec<RegistryRequestRecord> {
            self.records.lock().unwrap().clone()
        }
    }

    impl Drop for FakeRegistry {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct CapturedResponse {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
    }

    #[test]
    fn test_parse_image_name_simple() {
        let result = parse_image_name("nginx_latest.tar", Some("library"));
        assert_eq!(
            result,
            Some(("library/nginx".to_string(), "latest".to_string()))
        );
    }

    #[test]
    fn test_parse_image_name_without_default_namespace() {
        let result = parse_image_name("hello-app_latest.tar", None);
        assert_eq!(
            result,
            Some(("hello-app".to_string(), "latest".to_string()))
        );
    }

    #[test]
    fn test_parse_image_name_with_namespace() {
        let result = parse_image_name("myrepo_myimage_v1.0.tar", Some("library"));
        assert_eq!(
            result,
            Some(("myrepo/myimage".to_string(), "v1.0".to_string()))
        );
    }

    #[test]
    fn test_parse_image_name_with_nested_repository() {
        let result = parse_image_name("team__platform__myimage_v1.0.tar", Some("library"));
        assert_eq!(
            result,
            Some(("team/platform/myimage".to_string(), "v1.0".to_string()))
        );
    }

    #[test]
    fn test_parse_image_name_library() {
        let result = parse_image_name("library_nginx_1.25.tar", Some("library"));
        assert_eq!(
            result,
            Some(("library/nginx".to_string(), "1.25".to_string()))
        );
    }

    #[test]
    fn test_parse_image_name_invalid() {
        assert_eq!(parse_image_name("invalid.tar", Some("library")), None);
        assert_eq!(parse_image_name("noextension", Some("library")), None);
        assert_eq!(parse_image_name("_tag.tar", Some("library")), None);
    }

    #[test]
    fn test_registry_credentials_from_basic_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Basic dGVzdHVzZXI6dGVzdHBhc3M=".parse().unwrap(),
        );

        let credentials = registry_credentials_from_headers(&headers).unwrap();
        assert!(credentials.is_some());
    }

    #[test]
    fn test_registry_credentials_accept_case_insensitive_basic_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "basic dGVzdHVzZXI6dGVzdHBhc3M=".parse().unwrap(),
        );

        let credentials = registry_credentials_from_headers(&headers).unwrap();
        assert!(credentials.is_some());
    }

    #[test]
    fn test_registry_credentials_ignore_non_basic_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());

        assert!(registry_credentials_from_headers(&headers)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_registry_credentials_reject_invalid_basic_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Basic not-valid".parse().unwrap());

        assert!(registry_credentials_from_headers(&headers).is_err());
    }

    #[test]
    fn test_registry_credentials_accept_empty_password() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", BASE64_STANDARD.encode("testuser:"))
                .parse()
                .unwrap(),
        );

        assert!(registry_credentials_from_headers(&headers)
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_registry_credentials_reject_missing_password_separator() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", BASE64_STANDARD.encode("testuser"))
                .parse()
                .unwrap(),
        );

        assert!(registry_credentials_from_headers(&headers).is_err());
    }

    #[test]
    fn test_registry_credentials_reject_empty_username() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", BASE64_STANDARD.encode(":testpass"))
                .parse()
                .unwrap(),
        );

        assert!(registry_credentials_from_headers(&headers).is_err());
    }

    #[test]
    fn test_client_registry_auth_mode_values() {
        let original = std::env::var("REGISTRY_CLIENT_AUTH").ok();

        for value in ["required", "require", "challenge", "on", "true", "1"] {
            std::env::set_var("REGISTRY_CLIENT_AUTH", value);
            assert_eq!(
                client_registry_auth_mode(),
                ClientRegistryAuthMode::Required
            );
        }

        for value in ["off", "false", "0", "disabled", "ignore"] {
            std::env::set_var("REGISTRY_CLIENT_AUTH", value);
            assert_eq!(client_registry_auth_mode(), ClientRegistryAuthMode::Off);
        }

        for value in ["", "optional", "unexpected"] {
            std::env::set_var("REGISTRY_CLIENT_AUTH", value);
            assert_eq!(
                client_registry_auth_mode(),
                ClientRegistryAuthMode::Optional
            );
        }

        match original {
            Some(value) => std::env::set_var("REGISTRY_CLIENT_AUTH", value),
            None => std::env::remove_var("REGISTRY_CLIENT_AUTH"),
        }
    }

    #[tokio::test]
    async fn download_matrix_covers_routes_client_credentials_and_registry_auth() {
        let mut checked = 0_usize;

        for route in RouteCase::ALL {
            for credential in [ClientCredentialCase::Missing, ClientCredentialCase::Valid] {
                for policy in UpstreamAuthPolicy::ALL {
                    let registry = FakeRegistry::start(policy).await;
                    let response = call_download(route, credential, &registry).await;
                    let expected = expected_matrix_status(route, credential, policy);
                    assert_eq!(
                        response.status,
                        expected,
                        "route={}, credential={}, upstream={}",
                        route.name(),
                        credential.name(),
                        policy.name()
                    );

                    assert_response_matches_expectation(&response, route, credential, policy);
                    assert_registry_records_match(
                        &registry.records(),
                        route,
                        credential,
                        policy,
                        expected,
                    );
                    checked += 1;
                }
            }
        }

        assert_eq!(checked, 30);
    }

    #[tokio::test]
    async fn invalid_basic_header_is_rejected_before_registry_access_for_every_route() {
        for route in RouteCase::ALL {
            let registry = FakeRegistry::start(UpstreamAuthPolicy::Anonymous).await;
            let response = call_download(route, ClientCredentialCase::Invalid, &registry).await;

            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "route={}",
                route.name()
            );
            assert_eq!(
                response
                    .headers
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok()),
                Some("Basic realm=\"registry credentials\"")
            );
            assert!(
                registry.records().is_empty(),
                "invalid Basic header should not contact registry for route={}",
                route.name()
            );
        }
    }

    #[tokio::test]
    async fn gzipped_layers_are_decompressed_into_docker_save_archive() {
        let registry = FakeRegistry::start_with_options(
            UpstreamAuthPolicy::Anonymous,
            ManifestMode::Single,
            LayerEncoding::GzipTar,
        )
        .await;
        let response = call_download(
            RouteCase::Registry,
            ClientCredentialCase::Missing,
            &registry,
        )
        .await;

        assert_eq!(response.status, StatusCode::OK);
        assert_valid_docker_save_archive(&response.body);
    }

    #[tokio::test]
    async fn manifest_lists_resolve_to_linux_amd64_manifest_before_download() {
        let registry = FakeRegistry::start_with_options(
            UpstreamAuthPolicy::BearerChallenge,
            ManifestMode::ListThenSingle,
            LayerEncoding::PlainTar,
        )
        .await;
        let response = call_download(
            RouteCase::RegistryAuth,
            ClientCredentialCase::Valid,
            &registry,
        )
        .await;

        assert_eq!(response.status, StatusCode::OK);
        assert_valid_docker_save_archive(&response.body);

        let records = registry.records();
        assert!(
            records.iter().any(|record| record
                .path
                .contains(&format!("/manifests/{}", MANIFEST_DIGEST))),
            "manifest list should be followed by digest-specific manifest request: {:?}",
            records
        );
    }

    fn expected_matrix_status(
        route: RouteCase,
        credential: ClientCredentialCase,
        policy: UpstreamAuthPolicy,
    ) -> StatusCode {
        if credential == ClientCredentialCase::Invalid {
            return StatusCode::BAD_REQUEST;
        }

        if route.auth_mode() == ClientRegistryAuthMode::Required
            && credential == ClientCredentialCase::Missing
        {
            return StatusCode::UNAUTHORIZED;
        }

        if policy == UpstreamAuthPolicy::Anonymous {
            return StatusCode::OK;
        }

        if route.auth_mode() == ClientRegistryAuthMode::Off {
            return StatusCode::BAD_GATEWAY;
        }

        if credential == ClientCredentialCase::Valid {
            StatusCode::OK
        } else {
            StatusCode::BAD_GATEWAY
        }
    }

    fn assert_response_matches_expectation(
        response: &CapturedResponse,
        route: RouteCase,
        credential: ClientCredentialCase,
        policy: UpstreamAuthPolicy,
    ) {
        match response.status {
            StatusCode::OK => {
                assert_eq!(
                    response
                        .headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok()),
                    Some("application/x-tar")
                );
                assert_valid_docker_save_archive(&response.body);
            }
            StatusCode::UNAUTHORIZED | StatusCode::BAD_REQUEST => {
                assert_eq!(
                    response
                        .headers
                        .get(header::WWW_AUTHENTICATE)
                        .and_then(|value| value.to_str().ok()),
                    Some("Basic realm=\"registry credentials\""),
                    "route={}, credential={}, upstream={}",
                    route.name(),
                    credential.name(),
                    policy.name()
                );
            }
            StatusCode::BAD_GATEWAY => {
                let body = std::str::from_utf8(&response.body).unwrap();
                assert!(
                    body.contains("Failed to prepare image"),
                    "unexpected bad gateway body: {}",
                    body
                );
            }
            status => panic!("unexpected status in matrix: {}", status),
        }
    }

    fn assert_registry_records_match(
        records: &[RegistryRequestRecord],
        route: RouteCase,
        credential: ClientCredentialCase,
        policy: UpstreamAuthPolicy,
        expected: StatusCode,
    ) {
        if expected == StatusCode::UNAUTHORIZED || expected == StatusCode::BAD_REQUEST {
            assert!(
                records.is_empty(),
                "route={}, credential={}, upstream={} should not contact registry: {:?}",
                route.name(),
                credential.name(),
                policy.name(),
                records
            );
            return;
        }

        assert!(
            records
                .iter()
                .any(|record| record.path == format!("/v2/{}/manifests/{}", TEST_IMAGE, TEST_TAG)),
            "route={}, credential={}, upstream={} did not request manifest: {:?}",
            route.name(),
            credential.name(),
            policy.name(),
            records
        );

        if expected == StatusCode::OK {
            assert!(
                records
                    .iter()
                    .any(|record| record.path
                        == format!("/v2/{}/blobs/{}", TEST_IMAGE, CONFIG_DIGEST)),
                "successful download did not request config blob: {:?}",
                records
            );
            assert!(
                records
                    .iter()
                    .any(|record| record.path
                        == format!("/v2/{}/blobs/{}", TEST_IMAGE, LAYER_DIGEST)),
                "successful download did not request layer blob: {:?}",
                records
            );
        }

        match (expected, route.auth_mode(), credential, policy) {
            (StatusCode::OK, ClientRegistryAuthMode::Off, ClientCredentialCase::Valid, _) => {
                assert!(
                    records.iter().all(|record| record.authorization.is_none()),
                    "client credentials should be ignored when auth mode is off: {:?}",
                    records
                );
            }
            (StatusCode::OK, _, ClientCredentialCase::Valid, UpstreamAuthPolicy::DirectBasic) => {
                assert!(
                    records.iter().all(|record| {
                        record
                            .authorization
                            .as_deref()
                            .is_some_and(|value| value == expected_basic_header())
                    }),
                    "direct Basic registry should receive client Basic credentials: {:?}",
                    records
                );
            }
            (
                StatusCode::OK,
                _,
                ClientCredentialCase::Valid,
                UpstreamAuthPolicy::BearerChallenge,
            ) => {
                assert!(
                    records
                        .iter()
                        .any(|record| record.path.starts_with("/token?")
                            && record.authorization.as_deref()
                                == Some(expected_basic_header().as_str())),
                    "bearer challenge should exchange client Basic credentials for a token: {:?}",
                    records
                );
                assert!(
                    records.iter().any(|record| {
                        record
                            .authorization
                            .as_deref()
                            .is_some_and(|value| value == format!("Bearer {}", TEST_TOKEN))
                    }),
                    "bearer challenge should retry registry request with issued token: {:?}",
                    records
                );
            }
            (StatusCode::OK, _, ClientCredentialCase::Missing, UpstreamAuthPolicy::Anonymous) => {
                assert!(
                    records.iter().all(|record| record.authorization.is_none()),
                    "anonymous registry should not need Authorization without client credentials: {:?}",
                    records
                );
            }
            _ => {}
        }
    }

    async fn call_download(
        route: RouteCase,
        credential: ClientCredentialCase,
        registry: &FakeRegistry,
    ) -> CapturedResponse {
        let mut headers = HeaderMap::new();
        match credential {
            ClientCredentialCase::Missing => {}
            ClientCredentialCase::Valid => {
                headers.insert(
                    header::AUTHORIZATION,
                    expected_basic_header().parse().unwrap(),
                );
            }
            ClientCredentialCase::Invalid => {
                headers.insert(header::AUTHORIZATION, "Basic not-valid".parse().unwrap());
            }
        }

        let registry_config = RegistryConfig::from_url_parts("http", &registry.host).unwrap();
        let request_path = match route {
            RouteCase::DockerOff | RouteCase::DockerOptional | RouteCase::DockerRequired => {
                format!("/docker/{}", TEST_FILENAME)
            }
            RouteCase::Registry => {
                format!("/registry/http/{}/docker/{}", registry.host, TEST_FILENAME)
            }
            RouteCase::RegistryAuth => {
                format!(
                    "/registry-auth/http/{}/docker/{}",
                    registry.host, TEST_FILENAME
                )
            }
        };
        let response = handle_image_download(
            "127.0.0.1:33000".parse().unwrap(),
            headers,
            TEST_FILENAME.to_string(),
            DownloadRouteContext {
                registry_config,
                client_auth_mode: route.auth_mode(),
                request_path,
                route_template: route.route_template(),
                registry_target_source: route.target_source(),
            },
        )
        .await;

        capture_response(response).await
    }

    async fn capture_response(response: Response) -> CapturedResponse {
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap();
        CapturedResponse {
            status,
            headers,
            body,
        }
    }

    async fn handle_fake_registry_connection(
        mut stream: TcpStream,
        data: Arc<FakeRegistryData>,
        records: Arc<Mutex<Vec<RegistryRequestRecord>>>,
    ) {
        let Some((path, headers)) = read_http_request(&mut stream).await else {
            return;
        };
        let authorization = headers.get("authorization").cloned();
        records.lock().unwrap().push(RegistryRequestRecord {
            path: path.clone(),
            authorization: authorization.clone(),
        });

        if path.starts_with("/token?") || path == "/token" {
            handle_token_request(&mut stream, &data, authorization.as_deref()).await;
            return;
        }

        if !registry_request_authorized(&data, authorization.as_deref()) {
            write_registry_auth_challenge(&mut stream, &data).await;
            return;
        }

        write_registry_content_response(&mut stream, &data, &path).await;
    }

    async fn read_http_request(
        stream: &mut TcpStream,
    ) -> Option<(String, HashMap<String, String>)> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if buffer.len() > 16 * 1024 {
                return None;
            }
        }

        let request = String::from_utf8_lossy(&buffer);
        let mut lines = request.split("\r\n");
        let request_line = lines.next()?;
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next()?;
        let path = request_parts.next()?.to_string();
        if method != "GET" {
            return None;
        }

        let mut headers = HashMap::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }

        Some((path, headers))
    }

    async fn handle_token_request(
        stream: &mut TcpStream,
        data: &FakeRegistryData,
        authorization: Option<&str>,
    ) {
        if data.policy != UpstreamAuthPolicy::BearerChallenge {
            write_http_response(stream, 404, &[], b"not found").await;
            return;
        }

        if authorization != Some(data.basic_header.as_str()) {
            write_http_response(
                stream,
                401,
                &[(
                    "WWW-Authenticate",
                    "Basic realm=\"token service\"".to_string(),
                )],
                b"token credentials required",
            )
            .await;
            return;
        }

        write_http_response(
            stream,
            200,
            &[("Content-Type", "application/json".to_string())],
            format!(r#"{{"token":"{}"}}"#, data.token).as_bytes(),
        )
        .await;
    }

    fn registry_request_authorized(data: &FakeRegistryData, authorization: Option<&str>) -> bool {
        match data.policy {
            UpstreamAuthPolicy::Anonymous => true,
            UpstreamAuthPolicy::DirectBasic => authorization == Some(data.basic_header.as_str()),
            UpstreamAuthPolicy::BearerChallenge => {
                authorization == Some(format!("Bearer {}", data.token).as_str())
            }
        }
    }

    async fn write_registry_auth_challenge(stream: &mut TcpStream, data: &FakeRegistryData) {
        let challenge = match data.policy {
            UpstreamAuthPolicy::Anonymous => return,
            UpstreamAuthPolicy::DirectBasic => "Basic realm=\"private registry\"".to_string(),
            UpstreamAuthPolicy::BearerChallenge => format!(
                "Bearer realm=\"{}/token\",service=\"registry.test\",scope=\"repository:{}:pull\"",
                data.base_url, TEST_IMAGE
            ),
        };
        write_http_response(
            stream,
            401,
            &[("WWW-Authenticate", challenge)],
            b"registry auth required",
        )
        .await;
    }

    async fn write_registry_content_response(
        stream: &mut TcpStream,
        data: &FakeRegistryData,
        path: &str,
    ) {
        let single_manifest_path = format!("/v2/{}/manifests/{}", TEST_IMAGE, TEST_TAG);
        let digest_manifest_path = format!("/v2/{}/manifests/{}", TEST_IMAGE, MANIFEST_DIGEST);
        let config_path = format!("/v2/{}/blobs/{}", TEST_IMAGE, CONFIG_DIGEST);
        let layer_path = format!("/v2/{}/blobs/{}", TEST_IMAGE, LAYER_DIGEST);

        if path == single_manifest_path {
            let body = match data.mode {
                ManifestMode::Single => &data.manifest_bytes,
                ManifestMode::ListThenSingle => &data.manifest_list_bytes,
            };
            write_http_response(
                stream,
                200,
                &[
                    (
                        "Content-Type",
                        "application/vnd.oci.image.manifest.v1+json".to_string(),
                    ),
                    ("Docker-Content-Digest", MANIFEST_DIGEST.to_string()),
                ],
                body,
            )
            .await;
        } else if path == digest_manifest_path {
            write_http_response(
                stream,
                200,
                &[
                    (
                        "Content-Type",
                        "application/vnd.oci.image.manifest.v1+json".to_string(),
                    ),
                    ("Docker-Content-Digest", MANIFEST_DIGEST.to_string()),
                ],
                &data.manifest_bytes,
            )
            .await;
        } else if path == config_path {
            write_http_response(
                stream,
                200,
                &[(
                    "Content-Type",
                    "application/vnd.oci.image.config.v1+json".to_string(),
                )],
                &data.config_bytes,
            )
            .await;
        } else if path == layer_path {
            write_http_response(
                stream,
                200,
                &[("Content-Type", data.layer_media_type.to_string())],
                &data.layer_bytes,
            )
            .await;
        } else {
            write_http_response(stream, 404, &[], b"not found").await;
        }
    }

    async fn write_http_response(
        stream: &mut TcpStream,
        status: u16,
        headers: &[(&str, String)],
        body: &[u8],
    ) {
        let reason = match status {
            200 => "OK",
            401 => "Unauthorized",
            404 => "Not Found",
            _ => "Error",
        };
        let mut response = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            status,
            reason,
            body.len()
        );
        for (name, value) in headers {
            response.push_str(name);
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");

        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.shutdown().await;
    }

    fn expected_basic_header() -> String {
        format!(
            "Basic {}",
            BASE64_STANDARD.encode(format!("{}:{}", TEST_USER, TEST_PASSWORD))
        )
    }

    fn make_layer_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let content = b"hello from layer\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("hello.txt").unwrap();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &content[..]).unwrap();
        builder.finish().unwrap();
        builder.into_inner().unwrap()
    }

    fn gzip_bytes(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn manifest_bytes(layer_media_type: &str, config_size: usize, layer_size: usize) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "size": config_size,
                "digest": CONFIG_DIGEST
            },
            "layers": [
                {
                    "mediaType": layer_media_type,
                    "size": layer_size,
                    "digest": LAYER_DIGEST
                }
            ]
        }))
        .unwrap()
    }

    fn manifest_list_bytes() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "size": 1234,
                    "digest": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                    "platform": {
                        "architecture": "arm64",
                        "os": "linux"
                    }
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "size": 1234,
                    "digest": MANIFEST_DIGEST,
                    "platform": {
                        "architecture": "amd64",
                        "os": "linux"
                    }
                }
            ]
        }))
        .unwrap()
    }

    fn assert_valid_docker_save_archive(bytes: &[u8]) {
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let mut paths = Vec::new();
        let mut manifest_json = None;

        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path == "manifest.json" {
                let mut body = String::new();
                std::io::Read::read_to_string(&mut entry, &mut body).unwrap();
                manifest_json = Some(body);
            }
            paths.push(path);
        }

        assert!(
            paths.iter().any(|path| path == "manifest.json"),
            "{:?}",
            paths
        );
        assert!(
            paths.iter().any(
                |path| path == &format!("{}.json", CONFIG_DIGEST.trim_start_matches("sha256:"))
            ),
            "{:?}",
            paths
        );
        assert!(
            paths.iter().any(|path| path.ends_with("/VERSION")),
            "{:?}",
            paths
        );
        assert!(
            paths.iter().any(|path| path.ends_with("/json")),
            "{:?}",
            paths
        );
        assert!(
            paths.iter().any(|path| path.ends_with("/layer.tar")),
            "{:?}",
            paths
        );

        let manifest_json = manifest_json.expect("manifest.json should be present");
        let manifest: serde_json::Value = serde_json::from_str(&manifest_json).unwrap();
        assert_eq!(manifest[0]["RepoTags"][0], "hello:latest");
        assert_eq!(
            manifest[0]["Config"],
            format!("{}.json", CONFIG_DIGEST.trim_start_matches("sha256:"))
        );
        assert_eq!(manifest[0]["Layers"].as_array().unwrap().len(), 1);
    }
}
