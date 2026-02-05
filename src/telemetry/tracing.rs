use std::env;

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::resource::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const DEFAULT_SERVICE_NAME: &str = "delta-txn-service";

pub struct TelemetryGuard {
    meter_provider: Option<SdkMeterProvider>,
    tracer_provider: Option<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }

        if let Some(provider) = self.meter_provider.take() {
            let _ = provider.shutdown();
        }
    }
}

pub fn init_tracing() -> TelemetryGuard {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer();

    let mut guard = TelemetryGuard {
        meter_provider: None,
        tracer_provider: None,
    };

    if otel_export_enabled() {
        let service_name =
            env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| DEFAULT_SERVICE_NAME.to_string());
        let resource = Resource::builder_empty()
            .with_attributes([KeyValue::new("service.name", service_name)])
            .build();

        let protocol = otel_protocol();
        let tracer = build_tracer(protocol, resource.clone(), &mut guard);
        let otel_layer = tracer
            .as_ref()
            .map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer.clone()));

        let meter_provider = build_meter_provider(protocol, resource);

        if let Some(provider) = meter_provider.clone() {
            global::set_meter_provider(provider.clone());
            guard.meter_provider = Some(provider);
        }

        let registry = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer);
        if let Some(layer) = otel_layer {
            registry.with(layer).init();
        } else {
            registry.init();
        }
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .init();
    }

    guard
}

fn build_tracer(
    protocol: Protocol,
    resource: Resource,
    guard: &mut TelemetryGuard,
) -> Option<opentelemetry_sdk::trace::Tracer> {
    let exporter = match protocol {
        Protocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_protocol(Protocol::Grpc)
            .build()
            .ok(),
        Protocol::HttpBinary => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
            .ok(),
        Protocol::HttpJson => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpJson)
            .build()
            .ok(),
    };

    exporter.map(|exporter| {
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build();
        let tracer = provider.tracer(DEFAULT_SERVICE_NAME);
        global::set_tracer_provider(provider.clone());
        guard.tracer_provider = Some(provider);
        tracer
    })
}

fn build_meter_provider(protocol: Protocol, resource: Resource) -> Option<SdkMeterProvider> {
    let exporter = match protocol {
        Protocol::Grpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_protocol(Protocol::Grpc)
            .build()
            .ok(),
        Protocol::HttpBinary => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
            .ok(),
        Protocol::HttpJson => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpJson)
            .build()
            .ok(),
    };

    exporter.map(|exporter| {
        SdkMeterProvider::builder()
            .with_periodic_exporter(exporter)
            .with_resource(resource)
            .build()
    })
}

fn otel_protocol() -> Protocol {
    let protocol = env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
        .unwrap_or_else(|_| "grpc".to_string())
        .to_lowercase();

    match protocol.as_str() {
        "grpc" => Protocol::Grpc,
        "http/protobuf" | "http-protobuf" | "http" => Protocol::HttpBinary,
        "http/json" | "http-json" => Protocol::HttpJson,
        _ => Protocol::Grpc,
    }
}

fn otel_export_enabled() -> bool {
    env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
        || env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_ok()
        || env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_ok()
}
