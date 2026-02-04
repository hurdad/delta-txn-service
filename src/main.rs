use std::net::SocketAddr;

use tonic::transport::Server;
use tracing::info;

use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use delta_txn_service::telemetry::tracing::init_tracing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let addr: SocketAddr = "0.0.0.0:50051".parse()?;

    let svc = DeltaTxnGrpcServer::new();

    info!("DeltaTxnService listening on {}", addr);

    Server::builder()
        .add_service(svc.into_service())
        .serve(addr)
        .await?;

    Ok(())
}
