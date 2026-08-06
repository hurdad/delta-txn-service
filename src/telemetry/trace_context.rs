use std::future::Future;
use std::pin::Pin;

use http_body::Body;
use opentelemetry::propagation::{Extractor, TextMapPropagator};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tonic::body::Body as TonicBody;
use tonic::codegen::http::{HeaderMap, Request, Response};
use tower::{Layer, Service};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

// Wraps every request in a "grpc.request" tracing span, made a *child* of
// whatever W3C traceparent/tracestate headers the caller sent (if any) --
// this is what was actually missing before: init_tracing() (see
// telemetry/tracing.rs) already installs tracing_opentelemetry's layer and
// a live OTLP TracerProvider, but with no span ever created around a
// request, it had nothing to export; and with no incoming-context
// extraction, a caller's own trace (e.g. a client-side query span) could
// never have linked up here even once a span existed. GrpcMetricsLayer
// (telemetry/metrics.rs) is the shape this mirrors -- a tower Layer/Service
// wrapping every RPC uniformly rather than hand-instrumenting each of
// get_table/commit/list_active_files individually.
struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|key| key.as_str()).collect()
    }
}

#[derive(Clone)]
pub struct TraceContextLayer {
    propagator: TraceContextPropagator,
}

impl TraceContextLayer {
    pub fn new() -> Self {
        Self {
            propagator: TraceContextPropagator::new(),
        }
    }
}

impl Default for TraceContextLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Layer<S> for TraceContextLayer {
    type Service = TraceContextService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TraceContextService {
            inner,
            propagator: self.propagator.clone(),
        }
    }
}

#[derive(Clone)]
pub struct TraceContextService<S> {
    inner: S,
    propagator: TraceContextPropagator,
}

impl<S, R> Service<Request<TonicBody>> for TraceContextService<S>
where
    S: Service<Request<TonicBody>, Response = Response<R>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    R: Body + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<TonicBody>) -> Self::Future {
        let parent_cx = self.propagator.extract(&HeaderExtractor(req.headers()));

        // Same path -> service/method split GrpcMetricsService already
        // does for its own rpc.service/rpc.method attributes -- gRPC's
        // wire path is always "/{package}.{Service}/{Method}".
        let path = req.uri().path();
        let method = path.rsplit('/').next().unwrap_or("unknown").to_string();
        let service = path
            .rsplitn(2, '/')
            .last()
            .unwrap_or("unknown")
            .trim_start_matches('/')
            .to_string();

        let span = tracing::info_span!(
            "grpc.request",
            "rpc.system" = "grpc",
            "rpc.service" = %service,
            "rpc.method" = %method,
        );
        // Errors only when the span was already started/consumed elsewhere
        // (see OpenTelemetrySpanExt::set_parent's own doc comment) -- can't
        // happen for a span created fresh right above, but set_parent
        // still returns a Result rather than panicking, so this has to be
        // handled either way.
        if let Err(err) = span.set_parent(parent_cx) {
            tracing::warn!(error = %err, "failed to set span parent from incoming trace context");
        }

        let mut inner = self.inner.clone();
        let future = async move { inner.call(req).await };
        Box::pin(future.instrument(span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_extractor_reads_present_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );
        let extractor = HeaderExtractor(&headers);
        assert_eq!(
            extractor.get("traceparent"),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        );
        assert_eq!(extractor.get("missing"), None);
    }

    #[test]
    fn header_extractor_lists_keys() {
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", "value".parse().unwrap());
        headers.insert("x-api-key", "value".parse().unwrap());
        let extractor = HeaderExtractor(&headers);
        let mut keys = extractor.keys();
        keys.sort_unstable();
        assert_eq!(keys, vec!["traceparent", "x-api-key"]);
    }

    #[test]
    fn propagator_extracts_valid_traceparent_into_a_sampled_remote_context() {
        use opentelemetry::trace::TraceContextExt;

        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );
        let propagator = TraceContextPropagator::new();
        let cx = propagator.extract(&HeaderExtractor(&headers));
        let span_context = cx.span().span_context().clone();
        assert!(span_context.is_valid());
        assert!(span_context.is_remote());
        assert_eq!(
            span_context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[test]
    fn propagator_returns_empty_context_when_traceparent_is_absent() {
        use opentelemetry::trace::TraceContextExt;

        let headers = HeaderMap::new();
        let propagator = TraceContextPropagator::new();
        let cx = propagator.extract(&HeaderExtractor(&headers));
        assert!(!cx.span().span_context().is_valid());
    }
}
