// Process entry point: initializes tracing, loads config from the
// environment, applies the fail-closed security check, and hands off to
// delta_txn_service::app::serve (shared with the integration test harness)
// until a shutdown signal arrives.
use tokio::net::TcpListener;
use tokio::signal;
use tracing::warn;

use delta_txn_service::app::{install_crypto_provider, serve, BoxError, ServeOptions};
use delta_txn_service::config::grpc::{check_security, load_grpc_config};
use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use delta_txn_service::telemetry::tracing::init_tracing;

// Static musl builds (the Docker image) only: musl's allocator serializes
// heavily under concurrent allocation. See Cargo.toml.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Resolves on SIGTERM (what Kubernetes/Docker send on every rolling
/// deploy) or Ctrl+C/SIGINT, whichever arrives first. `serve` then stops
/// accepting new connections and lets in-flight RPCs finish, so clients
/// get a real response instead of a connection reset.
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install a Ctrl+C (SIGINT) handler");
    };

    // SIGTERM only exists on Unix; keep `cargo build` working elsewhere.
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
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    // Before anything that might make a TLS connection (OTLP export is set
    // up inside init_tracing). See install_crypto_provider's doc comment.
    install_crypto_provider();

    // Must run before anything logs: it installs the global subscriber
    // (and OTLP export when configured). Held for the process lifetime so
    // its Drop flushes buffered spans/metrics.
    let _telemetry_guard = init_tracing();

    let grpc_config = load_grpc_config()?;
    let svc = DeltaTxnGrpcServer::new();

    // Fail closed: see check_security's doc comment for the policy.
    for warning in check_security(&grpc_config, svc.has_allowlist())? {
        warn!("{warning}");
    }

    let listener = TcpListener::bind(grpc_config.addr).await?;
    serve(
        grpc_config,
        svc,
        listener,
        ServeOptions::from_env(),
        shutdown_signal(),
    )
    .await
}
