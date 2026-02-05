use std::time::Instant;

use std::future::Future;
use std::pin::Pin;

use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::KeyValue;
use tonic::body::BoxBody;
use tonic::{Code, Request, Response};
use tower::{Layer, Service};

#[derive(Clone)]
pub struct GrpcMetricsLayer {
    meter: Meter,
}

impl GrpcMetricsLayer {
    pub fn new(meter: Meter) -> Self {
        Self { meter }
    }
}

impl<S> Layer<S> for GrpcMetricsLayer {
    type Service = GrpcMetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcMetricsService::new(inner, self.meter.clone())
    }
}

#[derive(Clone)]
pub struct GrpcMetricsService<S> {
    inner: S,
    request_counter: Counter<u64>,
    error_counter: Counter<u64>,
    latency_histogram: Histogram<f64>,
}

impl<S> GrpcMetricsService<S> {
    fn new(inner: S, meter: Meter) -> Self {
        let request_counter = meter
            .u64_counter("grpc.server.requests")
            .with_description("Total gRPC requests received.")
            .init();
        let error_counter = meter
            .u64_counter("grpc.server.errors")
            .with_description("Total gRPC requests that returned non-OK status.")
            .init();
        let latency_histogram = meter
            .f64_histogram("grpc.server.latency_ms")
            .with_description("gRPC server latency in milliseconds.")
            .with_unit("ms")
            .init();

        Self {
            inner,
            request_counter,
            error_counter,
            latency_histogram,
        }
    }
}

impl<S, B> Service<Request<B>> for GrpcMetricsService<S>
where
    S: Service<Request<B>, Response = Response<BoxBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
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

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let mut inner = self.inner.clone();
        let request_counter = self.request_counter.clone();
        let error_counter = self.error_counter.clone();
        let latency_histogram = self.latency_histogram.clone();
        let path = req.uri().path();
        let method = path
            .rsplit('/')
            .next()
            .unwrap_or("unknown")
            .to_string();
        let service = path
            .rsplitn(2, '/')
            .last()
            .unwrap_or("unknown")
            .trim_start_matches('/')
            .to_string();
        let start = Instant::now();

        Box::pin(async move {
            let response = inner.call(req).await;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1_000.0;

            let (status_code, is_error) = match &response {
                Ok(resp) => {
                    let code = Code::from(resp.status());
                    (code, code != Code::Ok)
                }
                Err(_err) => (Code::Unknown, true),
            };

            let attributes = [
                KeyValue::new("rpc.system", "grpc"),
                KeyValue::new("rpc.service", service),
                KeyValue::new("rpc.method", method),
                KeyValue::new("rpc.grpc.status_code", status_code.to_string()),
            ];

            request_counter.add(1, &attributes);
            if is_error {
                error_counter.add(1, &attributes);
            }
            latency_histogram.record(elapsed_ms, &attributes);

            response
        })
    }
}
