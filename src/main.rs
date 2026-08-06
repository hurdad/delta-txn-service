// Process entry point: loads config, wires up telemetry + the auth
// interceptor + the DeltaTxnService implementation, and serves until the
// process is killed (no graceful-shutdown signal handling -- a SIGTERM/
// SIGINT just ends the process; any in-flight RPC is dropped, not drained).
use opentelemetry::global;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing::info;

use delta_txn_service::config::grpc::load_grpc_config;
use delta_txn_service::grpc::auth::make_auth_interceptor;
use delta_txn_service::grpc::server::pb::delta_txn_service_server::DeltaTxnServiceServer;
use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use delta_txn_service::telemetry::metrics::GrpcMetricsLayer;
use delta_txn_service::telemetry::trace_context::TraceContextLayer;
use delta_txn_service::telemetry::tracing::init_tracing;

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

    server.add_service(svc).serve(grpc_config.addr).await?;

    Ok(())
}
