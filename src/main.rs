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
use bytes::Bytes;
use futures::Stream;
use opentelemetry::{
    global,
    trace::{Span, SpanKind, Status, Tracer, TraceContextExt},
    Context as OtelContext,
};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{atomic::{AtomicU64, Ordering}, Arc},
    task::{Context, Poll},
    time::Instant,
};
use tokio_util::io::ReaderStream;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::registry::{RegistryClient, REGISTRY_NAME};
use crate::tar_builder::ImageTarBuilder;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Initialize OpenTelemetry (conditional based on .env)
    let _telemetry_guard = telemetry::init_telemetry();

    let app = Router::new()
        .route("/docker/:filename", get(handle_docker_download))
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

/// Parse filename like "nginx_latest.tar" into ("nginx", "latest")
/// or "library_nginx_1.25.tar" into ("library/nginx", "1.25")
fn parse_image_name(filename: &str) -> Option<(String, String)> {
    // Remove .tar extension
    let name = filename.strip_suffix(".tar")?;
    
    // Find the last underscore which separates the tag
    let last_underscore = name.rfind('_')?;
    let (image_part, tag) = name.split_at(last_underscore);
    let tag = &tag[1..]; // Remove the leading underscore
    
    if image_part.is_empty() || tag.is_empty() {
        return None;
    }
    
    // Check if there's a namespace (another underscore)
    // e.g., "library_nginx" -> "library/nginx"
    // e.g., "nginx" -> "library/nginx" (default to library for official images)
    let image_name = if image_part.contains('_') {
        // Replace first underscore with slash for namespace
        let first_underscore = image_part.find('_')?;
        let (namespace, rest) = image_part.split_at(first_underscore);
        format!("{}/{}", namespace, &rest[1..])
    } else {
        // Official image, prepend library/
        format!("library/{}", image_part)
    };
    
    Some((image_name, tag.to_string()))
}

async fn handle_docker_download(
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(filename): Path<String>,
) -> Response {
    // Extract real client IP from X-Forwarded-For or X-Real-IP headers, fallback to socket address
    let client_ip = get_client_ip(&headers, &socket_addr);
    
    info!("[{}] Received request for: {}", client_ip, filename);
    
    // Parse the filename to extract image name and tag
    let (image_name, tag) = match parse_image_name(&filename) {
        Some(parsed) => parsed,
        None => {
            error!("Invalid filename format: {}", filename);
            return (
                StatusCode::BAD_REQUEST,
                "Invalid filename format. Expected: imagename_tag.tar or namespace_imagename_tag.tar",
            )
                .into_response();
        }
    };
    
    info!("[{}] Parsed image: {}:{}", client_ip, image_name, tag);
    
    // Start parent span for the entire request
    let tracer = global::tracer("docker-http-passthrough");
    let mut span = tracer
        .span_builder("GET /docker/{filename}")
        .with_kind(SpanKind::Server)
        .start(&tracer);
    
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

    let path = format!("/docker/{}", filename);
    let url_authority = match (host.as_deref(), server_port) {
        (Some(h), Some(p)) if !h.contains(':') => format!("{}:{}", h, p),
        (Some(h), _) => h.to_string(),
        (None, Some(p)) => format!("localhost:{}", p),
        (None, None) => "localhost".to_string(),
    };
    let url_full = format!("{}://{}{}", scheme, url_authority, path);

    span.set_attribute(opentelemetry::KeyValue::new("http.request.method", "GET"));
    span.set_attribute(opentelemetry::KeyValue::new("http.route", "/docker/{filename}"));
    span.set_attribute(opentelemetry::KeyValue::new("url.scheme", scheme.clone()));
    span.set_attribute(opentelemetry::KeyValue::new("url.path", path));
    span.set_attribute(opentelemetry::KeyValue::new("url.full", url_full));
    span.set_attribute(opentelemetry::KeyValue::new("server.address", server_address));
    if let Some(p) = server_port {
        span.set_attribute(opentelemetry::KeyValue::new("server.port", p as i64));
    }
    span.set_attribute(opentelemetry::KeyValue::new("network.peer.address", socket_addr.ip().to_string()));
    span.set_attribute(opentelemetry::KeyValue::new("network.peer.port", socket_addr.port() as i64));
    span.set_attribute(opentelemetry::KeyValue::new("client.address", client_ip.clone()));
    if let Some(ua) = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        span.set_attribute(opentelemetry::KeyValue::new("user_agent.original", ua.to_string()));
    }
    span.set_attribute(opentelemetry::KeyValue::new("http.response.status_code", 200_i64));
    span.set_attribute(opentelemetry::KeyValue::new("file.name", filename.clone()));
    let image_name_full = format!("{}/{}", REGISTRY_NAME.as_str(), image_name);
    span.set_attribute(opentelemetry::KeyValue::new("container.image.name", image_name_full));
    span.set_attribute(opentelemetry::KeyValue::new("container.image.tag", tag.clone()));
    let container_name = std::env::var("CONTAINER_NAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "docker-http-passthrough".to_string());
    span.set_attribute(opentelemetry::KeyValue::new("container.name", container_name));
    if let Ok(hostname) = std::env::var("HOSTNAME") {
        if !hostname.trim().is_empty() {
            span.set_attribute(opentelemetry::KeyValue::new("host.name", hostname.clone()));
            span.set_attribute(opentelemetry::KeyValue::new("service.instance.id", hostname.clone()));
            span.set_attribute(opentelemetry::KeyValue::new("container.id", hostname));
        }
    }
    
    let parent_cx = OtelContext::current_with_span(span);
    
    // Create registry client
    let client = RegistryClient::new();
    
    // Create the tar builder which will stream the image
    let tar_builder = ImageTarBuilder::new(client, image_name.clone(), tag.clone());
    
    // Create a pipe for streaming
    let (writer, reader) = tokio::io::duplex(64 * 1024); // 64KB buffer
    
    // Spawn task to build tar archive with parent context for tracing
    let image_name_clone = image_name.clone();
    let tag_clone = tag.clone();
    let client_ip_clone = client_ip.clone();
    let spawn_cx = parent_cx.clone();
    tokio::spawn(async move {
        let result = tar_builder.build_tar_with_context(writer, spawn_cx.clone()).await;
        
        // End the parent span when tar building completes
        let parent_span = spawn_cx.span();
        match &result {
            Ok(_) => {
                parent_span.set_status(Status::Ok);
            }
            Err(e) => {
                parent_span.set_status(Status::error(e.to_string()));
                error!("[{}] Error building tar for {}:{}: {}", client_ip_clone, image_name_clone, tag_clone, e);
            }
        }
    });
    
    // Stream the response with logging wrapper
    let stream = ReaderStream::new(reader);
    let logged_stream = LoggingStream::new(
        stream,
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
    bytes_sent: Arc<AtomicU64>,
    otel_cx: OtelContext,
    client_ip: String,
    filename: String,
    image: String,
    tag: String,
    start_time: Instant,
    last_log_bytes: u64,
}

impl<S> LoggingStream<S> {
    fn new(
        inner: S,
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
            bytes_sent: Arc::new(AtomicU64::new(0)),
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
                let total = self.bytes_sent.fetch_add(chunk_len, Ordering::Relaxed) + chunk_len;
                
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
                let total = self.bytes_sent.load(Ordering::Relaxed);
                let span = self.otel_cx.span();
                span.set_attribute(opentelemetry::KeyValue::new(
                    "http.response.body.size",
                    total as i64,
                ));
                span.set_status(Status::error(e.to_string()));
                span.end();
                error!(
                    "[{}] Transfer error after {}: {} - {}",
                    self.client_ip,
                    Self::format_bytes(self.bytes_sent.load(Ordering::Relaxed)),
                    e,
                    self.filename
                );
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                let total = self.bytes_sent.load(Ordering::Relaxed);
                let elapsed = self.start_time.elapsed();
                let span = self.otel_cx.span();
                span.set_attribute(opentelemetry::KeyValue::new(
                    "http.response.body.size",
                    total as i64,
                ));
                span.end();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_image_name_simple() {
        let result = parse_image_name("nginx_latest.tar");
        assert_eq!(result, Some(("library/nginx".to_string(), "latest".to_string())));
    }

    #[test]
    fn test_parse_image_name_with_namespace() {
        let result = parse_image_name("myrepo_myimage_v1.0.tar");
        assert_eq!(result, Some(("myrepo/myimage".to_string(), "v1.0".to_string())));
    }

    #[test]
    fn test_parse_image_name_library() {
        let result = parse_image_name("library_nginx_1.25.tar");
        assert_eq!(result, Some(("library/nginx".to_string(), "1.25".to_string())));
    }

    #[test]
    fn test_parse_image_name_invalid() {
        assert_eq!(parse_image_name("invalid.tar"), None);
        assert_eq!(parse_image_name("noextension"), None);
        assert_eq!(parse_image_name("_tag.tar"), None);
    }
}
