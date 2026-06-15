// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace as sdktrace;
use opentelemetry_sdk::Resource;
use std::env;
use tracing::info;

const SERVICE_NAME: &str = "docker-http-passthrough";

pub struct TelemetryGuard {
    enabled: bool,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if self.enabled {
            global::shutdown_tracer_provider();
        }
    }
}

pub fn init_telemetry() -> TelemetryGuard {
    // Load .env file if present
    let _ = dotenvy::dotenv();
    global::set_text_map_propagator(TraceContextPropagator::new());

    let endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_default();

    if endpoint.is_empty() {
        info!("OTEL_EXPORTER_OTLP_ENDPOINT not set, OpenTelemetry export disabled");
        return TelemetryGuard { enabled: false };
    }

    match init_tracer(&endpoint) {
        Ok(_) => {
            info!("OpenTelemetry initialized, exporting to {}", endpoint);
            TelemetryGuard { enabled: true }
        }
        Err(e) => {
            tracing::error!("Failed to initialize OpenTelemetry: {}", e);
            TelemetryGuard { enabled: false }
        }
    }
}

fn init_tracer(endpoint: &str) -> Result<(), opentelemetry::trace::TraceError> {
    let service_name = env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| SERVICE_NAME.to_string());

    let container_name = env::var("CONTAINER_NAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let hostname = env::var("HOSTNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut resource_kvs = vec![KeyValue::new("service.name", service_name)];
    if let Some(h) = hostname.clone() {
        resource_kvs.push(KeyValue::new("service.instance.id", h.clone()));
        resource_kvs.push(KeyValue::new("host.name", h.clone()));
        resource_kvs.push(KeyValue::new("container.id", h));
    }
    if let Some(cn) = container_name {
        resource_kvs.push(KeyValue::new("container.name", cn));
    }

    if let Ok(attrs) = env::var("OTEL_RESOURCE_ATTRIBUTES") {
        for pair in attrs.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            if let Some((k, v)) = pair.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                if !k.is_empty() && !v.is_empty() {
                    resource_kvs.push(KeyValue::new(k.to_string(), v.to_string()));
                }
            }
        }
    }

    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(endpoint);

    let tracer_provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(sdktrace::Config::default().with_resource(Resource::new(resource_kvs)))
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;

    global::set_tracer_provider(tracer_provider.provider().unwrap());

    Ok(())
}
