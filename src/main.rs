use opentelemetry::global;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing::info;

use delta_txn_service::config::grpc::load_grpc_config;
use delta_txn_service::grpc::server::pb::delta_txn_service_server::DeltaTxnServiceServer;
use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use delta_txn_service::telemetry::metrics::GrpcMetricsLayer;
use delta_txn_service::telemetry::tracing::init_tracing;

fn make_auth_interceptor(
    api_key: Option<String>,
) -> impl Fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |req: tonic::Request<()>| {
        let Some(api_key) = api_key.as_deref() else {
            return Ok(req);
        };

        let metadata = req.metadata();
        let mut authorized = metadata
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(|value| value == api_key)
            .unwrap_or(false);

        if !authorized {
            authorized = metadata
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(|value| value == api_key)
                .unwrap_or(false);
        }

        if authorized {
            Ok(req)
        } else {
            Err(tonic::Status::unauthenticated("missing or invalid api key"))
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry_guard = init_tracing();

    let grpc_config = load_grpc_config()?;

    let svc = DeltaTxnGrpcServer::new();
    let svc = DeltaTxnServiceServer::with_interceptor(
        svc,
        make_auth_interceptor(grpc_config.api_key),
    );

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
