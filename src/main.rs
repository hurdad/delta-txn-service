use opentelemetry::global;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing::info;

use delta_txn_service::config::grpc::load_grpc_config;
use delta_txn_service::grpc::auth::make_auth_interceptor;
use delta_txn_service::grpc::server::pb::delta_txn_service_server::DeltaTxnServiceServer;
use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use delta_txn_service::telemetry::metrics::GrpcMetricsLayer;
use delta_txn_service::telemetry::tracing::init_tracing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry_guard = init_tracing();

    let grpc_config = load_grpc_config()?;

    let svc = DeltaTxnGrpcServer::new();
    let svc =
        DeltaTxnServiceServer::with_interceptor(svc, make_auth_interceptor(grpc_config.api_key));

    let meter = global::meter("delta-txn-service");
    let metrics_layer = GrpcMetricsLayer::new(meter);

    info!("DeltaTxnService listening on {}", grpc_config.addr);

    let mut server = Server::builder().layer(metrics_layer);

    if let Some(tls_config) = grpc_config.tls {
        let cert = std::fs::read(tls_config.cert_path)?;
        let key = std::fs::read(tls_config.key_path)?;
        let identity = Identity::from_pem(cert, key);
        server = server.tls_config(ServerTlsConfig::new().identity(identity))?;
    }

    server.add_service(svc).serve(grpc_config.addr).await?;

    Ok(())
}
