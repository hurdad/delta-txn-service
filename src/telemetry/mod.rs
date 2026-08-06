//! Observability: `tracing` sets up the process-wide tracing_subscriber
//! registry plus (when OTEL_EXPORTER_OTLP_* env vars are set) OTLP
//! trace/metric export -- see init_tracing()'s own doc comment for exactly
//! what "enabled" means. `metrics`/`trace_context` are both tower Layers
//! wrapping every gRPC request (see main.rs for the order they're
//! applied in): `metrics` records request/error counts and latency;
//! `trace_context` extracts an incoming W3C traceparent header (if any)
//! and creates the actual per-request tracing span everything else
//! (including `metrics`'s own recording) then runs inside.

pub mod metrics;
pub mod trace_context;
pub mod tracing;
