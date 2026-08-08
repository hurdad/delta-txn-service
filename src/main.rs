// Process entry point: loads config, wires up telemetry + the auth
// interceptor + the DeltaTxnService implementation, and serves until a
// shutdown signal arrives (see shutdown_signal() below) or an
// unrecoverable startup error occurs.
use std::time::Duration;

use opentelemetry::global;
use tokio::net::TcpStream;
use tokio::signal;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing::info;

use delta_txn_service::config::grpc::load_grpc_config;
use delta_txn_service::grpc::auth::make_auth_interceptor;
use delta_txn_service::grpc::server::pb::delta_txn_service_server::DeltaTxnServiceServer;
use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use delta_txn_service::telemetry::metrics::GrpcMetricsLayer;
use delta_txn_service::telemetry::trace_context::TraceContextLayer;
use delta_txn_service::telemetry::tracing::init_tracing;

/// Resolves on SIGTERM (what Kubernetes/Docker send on pod/container
/// termination -- every rolling deploy, not just an operator-initiated
/// stop) or Ctrl+C/SIGINT (local `cargo run`/`docker run -it`), whichever
/// arrives first. Handed to tonic's own `serve_with_shutdown` (see below):
/// once this future resolves, tonic stops accepting *new* connections but
/// lets already-in-flight RPCs finish, instead of the previous behavior --
/// the process simply ending, mid-request, with every open connection reset
/// out from under whatever client was waiting on it. Delta's own atomic
/// commit protocol already protects table data from a request being killed
/// mid-write; what this actually buys is well-behaved clients getting a
/// real response (success or a clean error) on every request made before
/// shutdown began, instead of a connection reset on whichever ones lost
/// the race with the signal.
///
/// Also flips `DeltaTxnService`'s health status to `NOT_SERVING` as soon
/// as the signal arrives, before this future resolves and tonic stops
/// accepting new connections: otherwise the readiness probe (checked on
/// its own periodSeconds interval, not synchronously with shutdown) could
/// keep reporting this pod as ready for up to one more interval after it
/// has already stopped accepting connections, letting a load balancer
/// route brand-new requests to a pod that can no longer serve them --
/// exactly the connection-reset problem graceful shutdown exists to avoid.
async fn shutdown_signal(health_reporter: tonic_health::server::HealthReporter) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install a Ctrl+C (SIGINT) handler");
    };

    // SIGTERM has no cross-platform equivalent in tokio::signal -- only
    // Unix has the concept at all. Docker/Kubernetes only ever run this
    // service on Linux, but `cargo build`/`cargo test` still need to
    // compile on any platform a contributor develops on.
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install a SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    info!("shutdown signal received: draining in-flight requests before exiting");
    health_reporter
        .set_not_serving::<DeltaTxnServiceServer<DeltaTxnGrpcServer>>()
        .await;
}

/// If `AWS_ENDPOINT_URL` is set -- the self-hosted/MinIO-style deployment
/// shape, where every `table_uri` this service ever opens ultimately
/// routes through one fixed object-store endpoint -- spawns a background
/// task that periodically probes that endpoint's TCP reachability and
/// reflects it in the health service's status for
/// `delta.txn.v1.DeltaTxnService` specifically (`SERVING` when reachable,
/// `NOT_SERVING` when not). The Helm chart's readiness probe checks that
/// exact service name (see deploy/helm's own `deployment.yaml`), so a
/// storage outage now actually shows up as "not ready" instead of the
/// health check meaning nothing beyond "the process started."
///
/// Deliberately only ever updates `DeltaTxnService`'s own status, never the
/// overall-server (`""`) status the *liveness* probe checks: a downstream
/// storage outage isn't a reason to *restart* this process -- restarting
/// doesn't fix MinIO being down, and cycling pods repeatedly during an
/// outage is its own antipattern -- it's a reason to stop routing *new*
/// traffic here until it recovers, which is exactly what failing
/// *readiness* (pulling the pod out of Service endpoints, no restart) does.
///
/// Deliberately does nothing at all when `AWS_ENDPOINT_URL` is unset (real
/// AWS S3): there's no single fixed endpoint to probe here -- this service
/// accepts an arbitrary `table_uri` per request, and S3 itself is
/// IAM/region-routed rather than reached through one host -- so
/// `DeltaTxnService`'s status just stays `SERVING` from startup onward,
/// same as before this existed.
///
/// A malformed `AWS_ENDPOINT_URL` (unparseable, no host, no resolvable
/// port), on the other hand, is an operator misconfiguration this service
/// *can* detect -- so each of those cases sets `DeltaTxnService` to
/// `NOT_SERVING` in addition to logging, rather than silently leaving it
/// `SERVING` (unverified) for the rest of the process's life. Readiness
/// failing loudly and immediately, in a way the deployment's own readiness
/// probe surfaces, is far more actionable than a startup log line no one
/// may be watching for.
fn spawn_storage_health_probe(
    health_reporter: tonic_health::server::HealthReporter,
    endpoint: String,
) {
    let Ok(parsed) = url::Url::parse(&endpoint) else {
        tracing::warn!(
            endpoint = %endpoint,
            "AWS_ENDPOINT_URL is not a valid URL -- marking DeltaTxnService not-serving"
        );
        tokio::spawn(async move {
            health_reporter
                .set_not_serving::<DeltaTxnServiceServer<DeltaTxnGrpcServer>>()
                .await;
        });
        return;
    };
    let Some(host) = parsed.host_str().map(str::to_string) else {
        tracing::warn!(
            endpoint = %endpoint,
            "AWS_ENDPOINT_URL has no host -- marking DeltaTxnService not-serving"
        );
        tokio::spawn(async move {
            health_reporter
                .set_not_serving::<DeltaTxnServiceServer<DeltaTxnGrpcServer>>()
                .await;
        });
        return;
    };
    let Some(port) = parsed.port_or_known_default() else {
        tracing::warn!(
            endpoint = %endpoint,
            "AWS_ENDPOINT_URL has no resolvable port -- marking DeltaTxnService not-serving"
        );
        tokio::spawn(async move {
            health_reporter
                .set_not_serving::<DeltaTxnServiceServer<DeltaTxnGrpcServer>>()
                .await;
        });
        return;
    };

    const PROBE_INTERVAL: Duration = Duration::from_secs(15);
    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

    tokio::spawn(async move {
        // interval(), ticked at the top of the loop, rather than
        // sleep(PROBE_INTERVAL) at the bottom: sleeping after each check
        // completes adds that check's own duration (up to PROBE_TIMEOUT)
        // on top of PROBE_INTERVAL every time, so a slow or timed-out
        // probe drifts the real cadence past what's documented -- worst
        // during exactly the degraded periods where prompt readiness
        // updates matter most. interval() anchors ticks to a fixed
        // schedule instead, so cadence stays PROBE_INTERVAL regardless of
        // how long any individual probe took.
        let mut ticker = tokio::time::interval(PROBE_INTERVAL);
        loop {
            ticker.tick().await;

            let reachable = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((host.as_str(), port)))
                .await
                .map(|connect_result| connect_result.is_ok())
                .unwrap_or(false);

            if reachable {
                health_reporter
                    .set_serving::<DeltaTxnServiceServer<DeltaTxnGrpcServer>>()
                    .await;
            } else {
                health_reporter
                    .set_not_serving::<DeltaTxnServiceServer<DeltaTxnGrpcServer>>()
                    .await;
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Must run before anything that might tracing::info!/error! -- it's
    // what makes those calls actually go anywhere useful (see
    // telemetry::tracing::init_tracing's own doc comment for exactly what
    // "enabled" means; a plain stdout `fmt` layer is always installed
    // regardless, so logging works even with OTLP export off).
    let _telemetry_guard = init_tracing();

    let grpc_config = load_grpc_config()?;

    let svc = DeltaTxnGrpcServer::new();
    // Tonic's per-service interceptor (metadata-only, runs after tower's
    // own Layer stack below has already routed the request to this
    // service) -- not a tower Layer itself, so it composes with
    // TraceContextLayer/GrpcMetricsLayer below rather than needing to be
    // one of them. Runs *after* those two (they wrap the whole HTTP/2
    // request before tonic's per-service dispatch even happens), so an
    // unauthorized request still gets a trace span and shows up in
    // grpc.server.requests/errors -- deliberately observable, not silently
    // invisible, even though it's ultimately rejected.
    let svc =
        DeltaTxnServiceServer::with_interceptor(svc, make_auth_interceptor(grpc_config.api_key));

    // The standard grpc.health.v1.Health service (tonic-health), reported
    // as SERVING for DeltaTxnService as soon as the process is ready to
    // accept requests. Deliberately added outside make_auth_interceptor: an
    // orchestrator's liveness/readiness probe has no practical way to
    // supply DELTA_TXN_GRPC_API_KEY, so gating health checks behind it
    // would make the probe itself the thing misconfigured auth breaks.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<DeltaTxnServiceServer<DeltaTxnGrpcServer>>()
        .await;
    // Cloned before any potential move into spawn_storage_health_probe
    // below: shutdown_signal() needs its own handle on the same reporter
    // to flip DeltaTxnService to NOT_SERVING when a shutdown signal
    // arrives, regardless of whether the storage health probe is active.
    let shutdown_health_reporter = health_reporter.clone();

    // From this point on, DeltaTxnService's status may keep changing --
    // see spawn_storage_health_probe's own doc comment for exactly when
    // and why -- so "SERVING for the rest of the process's life" is no
    // longer the whole story the way it used to be.
    if let Ok(endpoint) = std::env::var("AWS_ENDPOINT_URL") {
        spawn_storage_health_probe(health_reporter, endpoint);
    }

    let meter = global::meter("delta-txn-service");
    let metrics_layer = GrpcMetricsLayer::new(meter);

    info!("DeltaTxnService listening on {}", grpc_config.addr);

    // TraceContextLayer first (outermost): it establishes the "grpc.request"
    // span -- parented to the caller's own trace when a traceparent header
    // is present -- before anything else runs, so metrics_layer's recording
    // (and every tracing::info!/error! log call anywhere under this
    // request) happens inside that span's scope rather than alongside it.
    let mut server = Server::builder()
        .layer(TraceContextLayer::new())
        .layer(metrics_layer);

    if let Some(tls_config) = grpc_config.tls {
        let cert = std::fs::read(tls_config.cert_path)?;
        let key = std::fs::read(tls_config.key_path)?;
        let identity = Identity::from_pem(cert, key);
        server = server.tls_config(ServerTlsConfig::new().identity(identity))?;
    }

    server
        .add_service(svc)
        .add_service(health_service)
        .serve_with_shutdown(grpc_config.addr, shutdown_signal(shutdown_health_reporter))
        .await?;

    Ok(())
}
