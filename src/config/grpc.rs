use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct GrpcConfig {
    pub addr: SocketAddr,
    pub tls: Option<TlsConfig>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

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
        _ => {
            return Err("DELTA_TXN_GRPC_TLS_CERT and DELTA_TXN_GRPC_TLS_KEY must both be set"
                .into())
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
