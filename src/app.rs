//! Server assembly: everything between "config is loaded" and "serving
//! until shutdown". main.rs is only the process shell around this (tracing
//! init, env config, signal handling); the integration test harness
//! (tests/common) calls `serve` too, so tests exercise the exact production
//! wiring: layer order, auth interceptor, health service, TLS and limits.
use std::future::Future;
use std::time::Duration;

use opentelemetry::global;
use tokio::net::{TcpListener, TcpStream};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic_health::server::HealthReporter;
use tracing::info;

use crate::config::grpc::GrpcConfig;
use crate::grpc::auth::make_auth_interceptor;
use crate::grpc::server::pb::delta_txn_service_server::DeltaTxnServiceServer;
use crate::grpc::server::DeltaTxnGrpcServer;
use crate::telemetry::metrics::GrpcMetricsLayer;
use crate::telemetry::trace_context::TraceContextLayer;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How often the storage reachability probe runs (see
/// `spawn_storage_health_probe`).
pub const DEFAULT_STORAGE_PROBE_INTERVAL: Duration = Duration::from_secs(15);
const STORAGE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Settings for `serve` that aren't part of the gRPC config proper.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The fixed S3-compatible endpoint (`AWS_ENDPOINT_URL`) to probe for
    /// readiness, if any. None disables the probe.
    pub storage_endpoint: Option<String>,
    pub storage_probe_interval: Duration,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            storage_endpoint: None,
            storage_probe_interval: DEFAULT_STORAGE_PROBE_INTERVAL,
        }
    }
}

impl ServeOptions {
    pub fn from_env() -> Self {
        Self {
            storage_endpoint: std::env::var("AWS_ENDPOINT_URL").ok(),
            ..Default::default()
        }
    }
}

type ServiceHealth = DeltaTxnServiceServer<DeltaTxnGrpcServer>;

/// Installs ring as rustls's process-wide default crypto provider. The
/// dependency graph enables both rustls backends (ring via tonic and the
/// OTLP exporter, aws-lc-rs via delta-rs's HTTP client and the AWS SDK);
/// with both compiled in, rustls has no default and panics on the first
/// TLS handshake -- the gRPC server's own, S3 over HTTPS, or OTLP over
/// TLS. Must run before any of those. Idempotent: a provider that's
/// already installed is left as-is.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Serves `svc` on `listener` until `shutdown` resolves, then drains
/// in-flight requests and returns.
///
/// Layer order: TraceContextLayer (outermost: establishes the
/// `grpc.request` span, parented to an incoming traceparent) ->
/// GrpcMetricsLayer -> tonic's per-service auth interceptor. The
/// interceptor runs after both layers, so a rejected request still gets a
/// span and shows up in metrics. The grpc.health.v1.Health service sits
/// outside the interceptor entirely: an orchestrator's probe has no way to
/// supply an API key.
///
/// When `shutdown` resolves, DeltaTxnService's health status flips to
/// NOT_SERVING before tonic stops accepting connections, so the readiness
/// probe stops routing new traffic here as early as possible.
pub async fn serve(
    config: GrpcConfig,
    svc: DeltaTxnGrpcServer,
    listener: TcpListener,
    options: ServeOptions,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), BoxError> {
    install_crypto_provider();

    // max_decoding_message_size bounds a single request message.
    let svc = DeltaTxnServiceServer::new(svc).max_decoding_message_size(config.max_message_bytes);
    let svc = tonic::service::interceptor::InterceptedService::new(
        svc,
        make_auth_interceptor(config.api_keys.clone()),
    );

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter.set_serving::<ServiceHealth>().await;
    if let Some(endpoint) = options.storage_endpoint {
        spawn_storage_health_probe(
            health_reporter.clone(),
            endpoint,
            options.storage_probe_interval,
        )
        .await;
    }

    let metrics_layer = GrpcMetricsLayer::new(global::meter("delta-txn-service"));

    // timeout() bounds how long a handler may run before the client gets
    // DEADLINE_EXCEEDED (including time waiting on the per-table commit
    // lock); for ListActiveFiles it covers producing the response, not
    // draining the stream. concurrency_limit_per_connection keeps one
    // client connection from monopolizing the server.
    let mut builder = Server::builder()
        .concurrency_limit_per_connection(config.max_concurrent_requests_per_connection);
    if let Some(timeout) = config.request_timeout {
        builder = builder.timeout(timeout);
    }

    if let Some(tls_config) = &config.tls {
        let cert = std::fs::read(&tls_config.cert_path)?;
        let key = std::fs::read(&tls_config.key_path)?;
        let mut tls = ServerTlsConfig::new().identity(Identity::from_pem(cert, key));
        if let Some(ca_path) = &tls_config.client_ca_path {
            // Mutual TLS: only clients with a certificate signed by this
            // CA can connect at all.
            let ca = std::fs::read(ca_path)?;
            tls = tls.client_ca_root(Certificate::from_pem(ca));
            info!("mutual TLS enabled: client certificates are required");
        }
        builder = builder.tls_config(tls)?;
    }

    let local_addr = listener.local_addr()?;
    info!("DeltaTxnService listening on {local_addr}");

    let shutdown_reporter = health_reporter;
    let shutdown = async move {
        shutdown.await;
        info!("shutdown signal received: draining in-flight requests before exiting");
        shutdown_reporter.set_not_serving::<ServiceHealth>().await;
    };

    builder
        .layer(TraceContextLayer::new())
        .layer(metrics_layer)
        .add_service(svc)
        .add_service(health_service)
        .serve_with_incoming_shutdown(
            TcpIncoming::from(listener).with_nodelay(Some(true)),
            shutdown,
        )
        .await?;

    Ok(())
}

/// Probes the configured S3-compatible endpoint's TCP reachability every
/// `interval` and reflects it in DeltaTxnService's health status, which the
/// Helm chart's readiness probe checks. Never touches the overall-server
/// (`""`) status the liveness probe checks: a storage outage should pull
/// the pod out of Service endpoints, not restart it (restarting doesn't
/// fix the object store being down).
///
/// Only used when AWS_ENDPOINT_URL is set: real AWS S3 has no single fixed
/// endpoint to probe. A malformed endpoint (unparseable, no host, no
/// resolvable port) is an operator error this service can detect, so it
/// marks the service NOT_SERVING (and logs) instead of leaving it
/// unverified-SERVING forever.
pub async fn spawn_storage_health_probe(
    health_reporter: HealthReporter,
    endpoint: String,
    interval: Duration,
) {
    let target = url::Url::parse(&endpoint)
        .map_err(|_| "is not a valid URL")
        .and_then(|parsed| {
            let host = parsed.host_str().ok_or("has no host")?.to_string();
            let port = parsed
                .port_or_known_default()
                .ok_or("has no resolvable port")?;
            Ok((host, port))
        });
    let (host, port) = match target {
        Ok(target) => target,
        Err(problem) => {
            tracing::warn!(
                endpoint = %endpoint,
                "AWS_ENDPOINT_URL {problem} -- marking DeltaTxnService not-serving"
            );
            health_reporter.set_not_serving::<ServiceHealth>().await;
            return;
        }
    };

    tokio::spawn(async move {
        // interval() rather than sleep() after each check: ticks stay on a
        // fixed schedule however long an individual probe takes.
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            let reachable = tokio::time::timeout(
                STORAGE_PROBE_TIMEOUT,
                TcpStream::connect((host.as_str(), port)),
            )
            .await
            .is_ok_and(|connect_result| connect_result.is_ok());

            if reachable {
                health_reporter.set_serving::<ServiceHealth>().await;
            } else {
                health_reporter.set_not_serving::<ServiceHealth>().await;
            }
        }
    });
}
