use std::time::Instant;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http_body::{Body, Frame, SizeHint};
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::KeyValue;
use pin_project_lite::pin_project;
use tonic::body::Body as TonicBody;
use tonic::codegen::http::{Request, Response};
use tonic::Code;
use tower::{Layer, Service};

/// A tower Layer recording `grpc.server.requests`/`grpc.server.errors`
/// (Counters) and `grpc.server.latency_ms` (Histogram) for every request,
/// tagged with rpc.system/rpc.service/rpc.method/rpc.grpc.status_code --
/// see README.md's "Metrics" section for the exact names/descriptions
/// these are documented under.
///
/// The real gRPC status for most calls arrives in HTTP/2 *trailers*, sent
/// only after the response body (if any) has been fully written -- never
/// in the initial headers alone. There are two distinct cases, handled
/// differently below:
/// - "Trailers-Only": a call that fails before writing any message at all
///   (e.g. grpc::auth's interceptor rejecting a request) gets its status
///   folded into the *initial* headers frame instead of a separate
///   trailers frame -- `call()` below still detects this directly via
///   `tonic::Status::from_header_map(response.headers())`, exactly as
///   before.
/// - Everything else (a normal successful response, an application error
///   returned after writing some data, or a `ListActiveFiles` stream that
///   fails partway through after already writing several batches) needs
///   the response *body* itself observed as it streams, since that's the
///   only place the real trailers frame ever appears -- `MetricsBody`
///   below wraps the body for exactly this, deferring recording until
///   `poll_frame` yields a trailers frame (or the body ends/errors
///   without ever producing one, treated as `Code::Unknown`/an error
///   rather than silently never recording anything).
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
            .build();
        let error_counter = meter
            .u64_counter("grpc.server.errors")
            .with_description("Total gRPC requests that returned non-OK status.")
            .build();
        let latency_histogram = meter
            .f64_histogram("grpc.server.latency_ms")
            .with_description("gRPC server latency in milliseconds.")
            .with_unit("ms")
            .build();

        Self {
            inner,
            request_counter,
            error_counter,
            latency_histogram,
        }
    }
}

fn record(
    request_counter: &Counter<u64>,
    error_counter: &Counter<u64>,
    latency_histogram: &Histogram<f64>,
    service: String,
    method: String,
    start: Instant,
    status_code: Code,
) {
    let elapsed_ms = start.elapsed().as_secs_f64() * 1_000.0;
    let attributes = [
        KeyValue::new("rpc.system", "grpc"),
        KeyValue::new("rpc.service", service),
        KeyValue::new("rpc.method", method),
        KeyValue::new("rpc.grpc.status_code", status_code.to_string()),
    ];
    request_counter.add(1, &attributes);
    if status_code != Code::Ok {
        error_counter.add(1, &attributes);
    }
    latency_histogram.record(elapsed_ms, &attributes);
}

impl<S, R> Service<Request<TonicBody>> for GrpcMetricsService<S>
where
    S: Service<Request<TonicBody>, Response = Response<R>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    R: Body + Send + 'static,
{
    type Response = Response<MetricsBody<R>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<TonicBody>) -> Self::Future {
        let mut inner = self.inner.clone();
        let request_counter = self.request_counter.clone();
        let error_counter = self.error_counter.clone();
        let latency_histogram = self.latency_histogram.clone();
        let path = req.uri().path();
        let method = path.rsplit('/').next().unwrap_or("unknown").to_string();
        let service = path
            .rsplitn(2, '/')
            .last()
            .unwrap_or("unknown")
            .trim_start_matches('/')
            .to_string();
        let start = Instant::now();

        Box::pin(async move {
            let response = match inner.call(req).await {
                Ok(response) => response,
                Err(err) => {
                    record(
                        &request_counter,
                        &error_counter,
                        &latency_histogram,
                        service,
                        method,
                        start,
                        Code::Unknown,
                    );
                    return Err(err);
                }
            };

            // Trailers-Only fast path: the real status is already fully
            // known from the initial headers (see this module's own top
            // comment) -- record immediately rather than waiting on a body
            // that, for this specific case, carries no separate trailers
            // frame at all.
            if let Some(status) = tonic::Status::from_header_map(response.headers()) {
                record(
                    &request_counter,
                    &error_counter,
                    &latency_histogram,
                    service,
                    method,
                    start,
                    status.code(),
                );
                let (parts, body) = response.into_parts();
                return Ok(Response::from_parts(
                    parts,
                    MetricsBody {
                        inner: body,
                        recorder: None,
                    },
                ));
            }

            // Otherwise: defer recording until the body's real trailers
            // frame (or an abnormal end/error with no trailers at all) is
            // observed -- see MetricsBody::poll_frame below.
            let recorder = Some(MetricsRecorder {
                request_counter,
                error_counter,
                latency_histogram,
                service,
                method,
                start,
            });
            let (parts, body) = response.into_parts();
            Ok(Response::from_parts(
                parts,
                MetricsBody {
                    inner: body,
                    recorder,
                },
            ))
        })
    }
}

struct MetricsRecorder {
    request_counter: Counter<u64>,
    error_counter: Counter<u64>,
    latency_histogram: Histogram<f64>,
    service: String,
    method: String,
    start: Instant,
}

impl MetricsRecorder {
    fn record(self, status_code: Code) {
        record(
            &self.request_counter,
            &self.error_counter,
            &self.latency_histogram,
            self.service,
            self.method,
            self.start,
            status_code,
        );
    }
}

pin_project! {
    /// Wraps a response body so metrics are recorded once the real gRPC
    /// status is known, from a `Frame::trailers()` the *inner* body
    /// yields -- not the response's initial headers (already handled, for
    /// the one case where a status legitimately appears there instead, in
    /// GrpcMetricsService::call() above). `recorder` is `None` from
    /// construction for the Trailers-Only case (already recorded, nothing
    /// left to do here but pass frames through transparently) and is
    /// `take()`n exactly once -- on the first trailers frame seen, or on
    /// stream end/error if no trailers frame ever arrived -- so a
    /// (spec-compliant) stream that happens to poll past its own end
    /// never double-records.
    pub struct MetricsBody<B> {
        #[pin]
        inner: B,
        recorder: Option<MetricsRecorder>,
    }
}

impl<B> Body for MetricsBody<B>
where
    B: Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let poll = this.inner.poll_frame(cx);

        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(trailers) = frame.trailers_ref() {
                    if let Some(recorder) = this.recorder.take() {
                        let status_code = tonic::Status::from_header_map(trailers)
                            .map(|status| status.code())
                            .unwrap_or(Code::Unknown);
                        recorder.record(status_code);
                    }
                }
            }
            // Stream ended, or errored, without ever yielding a trailers
            // frame -- an abnormal close (dropped connection, or a body
            // implementation that doesn't send trailers). Recorded as an
            // error rather than silently never recording anything at all.
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => {
                if let Some(recorder) = this.recorder.take() {
                    recorder.record(Code::Unknown);
                }
            }
            Poll::Pending => {}
        }

        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
