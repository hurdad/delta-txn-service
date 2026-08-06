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

/// Holds the real SdkTracerProvider/SdkMeterProvider (when OTLP export is
/// enabled -- see otel_export_enabled()) so their Drop impls run at
/// process shutdown, flushing any buffered spans/metrics before exit.
/// Both are None when export is disabled (or failed to construct -- see
/// build_tracer()'s own comment on that silent-failure gap); dropping a
/// None TelemetryGuard is a no-op either way. Held as `let _guard = ...`
/// in main() for the whole process lifetime, never inspected otherwise.
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

/// Initializes the global `tracing` subscriber (always: a plain stdout
/// `fmt` layer, filtered by `RUST_LOG`/`EnvFilter` default "info") and,
/// when `otel_export_enabled()`, also OTLP trace + metric export via
/// `tracing_opentelemetry`'s bridging layer -- must be called exactly
/// once, before anything else in the process calls `tracing::info!`/etc.
/// (main.rs calls this first thing). Returns a guard whose Drop flushes
/// and shuts down whatever providers were actually constructed; hold it
/// for the process's whole lifetime.
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

/// Builds the OTLP span exporter for `protocol` and wraps it in a real
/// SdkTracerProvider, registering it as the process-global tracer provider
/// (`global::set_tracer_provider`) and stashing it in `guard` so it gets
/// shut down/flushed at process exit.
///
/// KNOWN GAP (found auditing this file, not yet fixed): `.build().ok()`
/// silently discards the exporter-construction error and returns `None`
/// on failure (a malformed OTEL_EXPORTER_OTLP_ENDPOINT, unreachable DNS
/// name at construction time if the SDK validates that eagerly, etc.) --
/// init_tracing() then just falls back to no OTel layer at all, with zero
/// indication to the operator that anything went wrong; `otel_export_enabled()`
/// having returned `true` (an endpoint env var *was* set) makes this a
/// real "silently produced no traces despite being configured to" trap,
/// not just a theoretical one. Not fixed here: this function runs *before*
/// `tracing_subscriber`'s global subscriber is installed (see
/// init_tracing()), so a `tracing::warn!` call here would itself go
/// nowhere -- surfacing this properly needs either reordering
/// initialization or falling back to a raw `eprintln!` for this one
/// bootstrap-time diagnostic, a real (if small) design choice, not made
/// unilaterally in this audit pass.
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

/// Same shape (and the same silent-failure-on-bad-config gap, see
/// build_tracer()'s doc comment) as build_tracer(), for metrics instead of
/// traces -- registered as the process-global meter provider by
/// init_tracing() itself (not here), since GrpcMetricsLayer's own
/// `global::meter("delta-txn-service")` call (main.rs) needs that to have
/// already happened.
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

/// Reads OTEL_EXPORTER_OTLP_PROTOCOL, defaulting to (and falling back to,
/// for any unrecognized value) gRPC -- a deliberate choice, not an
/// arbitrary one: this service already speaks gRPC for its own primary
/// API, so exporting telemetry the same way avoids adding an HTTP
/// dependency (and a second port/endpoint shape to operate) purely for
/// observability. See README.md's "Configuration" section for the
/// documented env var list this reads.
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

/// Export is opt-in: any of the three endpoint env vars being set is
/// enough (mirrors the OTel spec's own per-signal endpoint override
/// convention -- an operator who only wants metrics, say, can set just
/// OTEL_EXPORTER_OTLP_METRICS_ENDPOINT). Absent all three, init_tracing()
/// only installs the plain stdout `fmt` layer -- logging always works,
/// export is the opt-in part.
fn otel_export_enabled() -> bool {
    env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
        || env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_ok()
        || env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_ok()
}
