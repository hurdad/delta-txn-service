use std::net::SocketAddr;

use tonic::transport::Server;
use tracing::info;

use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use delta_txn_service::telemetry::tracing::init_tracing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry_guard = init_tracing();

    let addr_env = std::env::var("DELTA_TXN_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".to_string());
    let addr: SocketAddr = addr_env.parse()?;

    let svc = DeltaTxnGrpcServer::new();

    info!("DeltaTxnService listening on {}", addr);

    Server::builder()
        .add_service(svc.into_service())
        .serve(addr)
        .await?;

    Ok(())
}
