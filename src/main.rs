use std::net::SocketAddr;

use opentelemetry::global;
use tonic::transport::Server;
use tracing::info;

use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use delta_txn_service::telemetry::grpc_metrics::GrpcMetricsLayer;
use delta_txn_service::telemetry::tracing::init_tracing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry_guard = init_tracing();

    let addr_env =
        std::env::var("DELTA_TXN_GRPC_ADDR").unwrap_or_else(|_| "0.0.0.0:50051".to_string());
    let addr: SocketAddr = addr_env.parse()?;

    let svc = DeltaTxnGrpcServer::new();
    let meter = global::meter("delta-txn-service");
    let metrics_layer = GrpcMetricsLayer::new(meter);

    info!("DeltaTxnService listening on {}", addr);

    Server::builder()
        .layer(metrics_layer)
        .add_service(svc.into_service())
        .serve(addr)
        .await?;

    Ok(())
}
