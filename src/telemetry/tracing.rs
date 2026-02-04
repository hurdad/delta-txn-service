use std::env;

use opentelemetry::global;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::resource::Resource;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const DEFAULT_SERVICE_NAME: &str = "delta-txn-service";

pub struct TelemetryGuard {
    meter_provider: Option<SdkMeterProvider>,
    tracer_enabled: bool,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if self.tracer_enabled {
            global::shutdown_tracer_provider();
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
        tracer_enabled: false,
    };

    if otel_export_enabled() {
        let service_name =
            env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| DEFAULT_SERVICE_NAME.to_string());
        let resource = Resource::new(vec![KeyValue::new("service.name", service_name)]);

        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(opentelemetry_otlp::new_exporter().tonic())
            .with_trace_config(
                opentelemetry_sdk::trace::Config::default().with_resource(resource.clone()),
            )
            .install_batch(opentelemetry_sdk::runtime::Tokio)
            .ok();
        let otel_layer = tracer
            .as_ref()
            .map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer.clone()));
        guard.tracer_enabled = tracer.is_some();

        let meter_provider = opentelemetry_otlp::new_pipeline()
            .metrics()
            .with_exporter(opentelemetry_otlp::new_exporter().tonic())
            .with_resource(resource)
            .build()
            .ok();

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

fn otel_export_enabled() -> bool {
    env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
        || env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_ok()
        || env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_ok()
}
