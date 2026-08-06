use std::net::SocketAddr;
use std::path::PathBuf;

/// Process-lifetime gRPC server configuration, loaded once in main() before
/// the server starts listening -- see load_grpc_config() below for the
/// exact environment variables each field comes from.
#[derive(Debug, Clone)]
pub struct GrpcConfig {
    pub addr: SocketAddr,
    /// None means plaintext (no TLS) -- the historical default, still the
    /// default today unless both DELTA_TXN_GRPC_TLS_CERT and
    /// DELTA_TXN_GRPC_TLS_KEY are set.
    pub tls: Option<TlsConfig>,
    /// None means no auth: every request is accepted regardless of
    /// credentials (see grpc::auth::make_auth_interceptor, which is a
    /// no-op pass-through in that case). Operators are warned about this
    /// at startup -- see grpc::server::DeltaTxnGrpcServer::new().
    pub api_key: Option<String>,
}

/// PEM-encoded server certificate and private key file paths, read and
/// handed to tonic's ServerTlsConfig at startup (main.rs) -- not
/// reloaded/watched for rotation; a certificate renewal needs a process
/// restart.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Reads all gRPC server configuration from environment variables (see
/// README.md's "Configuration" section for the full documented list):
/// - `DELTA_TXN_GRPC_ADDR` (default "0.0.0.0:50051")
/// - `DELTA_TXN_GRPC_TLS_CERT` / `DELTA_TXN_GRPC_TLS_KEY` (both-or-neither)
/// - `DELTA_TXN_GRPC_API_KEY` (optional; empty/whitespace-only treated as
///   unset, matching how an operator might accidentally set `API_KEY=""`
///   in a Compose/Helm values file without meaning to enable a real check)
pub fn load_grpc_config() -> Result<GrpcConfig, Box<dyn std::error::Error>> {
    let addr_env =
        std::env::var("DELTA_TXN_GRPC_ADDR").unwrap_or_else(|_| "0.0.0.0:50051".to_string());
    let addr: SocketAddr = addr_env.parse()?;

    let cert_path = std::env::var("DELTA_TXN_GRPC_TLS_CERT").ok();
    let key_path = std::env::var("DELTA_TXN_GRPC_TLS_KEY").ok();
    let tls = match (cert_path, key_path) {
        (Some(cert_path), Some(key_path)) => Some(TlsConfig {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
        }),
        (None, None) => None,
        // Exactly one of the pair set is almost certainly a misconfiguration
        // (a typo'd env var name, a half-finished TLS setup) rather than an
        // intentional choice -- failing loudly here beats silently falling
        // back to plaintext with an operator believing TLS is on.
        _ => {
            return Err(
                "DELTA_TXN_GRPC_TLS_CERT and DELTA_TXN_GRPC_TLS_KEY must both be set".into(),
            )
        }
    };

    let api_key = std::env::var("DELTA_TXN_GRPC_API_KEY")
        .ok()
        .and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });

    Ok(GrpcConfig { addr, tls, api_key })
}
