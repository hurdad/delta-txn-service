use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Default per-request timeout (DELTA_TXN_GRPC_REQUEST_TIMEOUT_SECS).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Default concurrent requests per HTTP/2 connection
/// (DELTA_TXN_GRPC_MAX_CONCURRENT_REQUESTS_PER_CONNECTION).
pub const DEFAULT_MAX_CONCURRENT_REQUESTS_PER_CONNECTION: usize = 64;
/// Default largest request message accepted, in bytes
/// (DELTA_TXN_GRPC_MAX_MESSAGE_BYTES) -- tonic's own default.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Process-lifetime gRPC server configuration, loaded once in main() before
/// the server starts listening -- see parse_grpc_config() below for the
/// exact environment variables each field comes from.
#[derive(Debug, Clone)]
pub struct GrpcConfig {
    pub addr: SocketAddr,
    /// None means plaintext (no TLS) unless both DELTA_TXN_GRPC_TLS_CERT
    /// and DELTA_TXN_GRPC_TLS_KEY are set.
    pub tls: Option<TlsConfig>,
    /// Empty means no auth: every request is accepted regardless of
    /// credentials (see grpc::auth::make_auth_interceptor). Startup refuses
    /// that combined with no table allowlist unless `allow_insecure` (see
    /// check_security()).
    pub api_keys: Vec<ApiKey>,
    /// None disables the per-request timeout.
    pub request_timeout: Option<Duration>,
    pub max_concurrent_requests_per_connection: usize,
    pub max_message_bytes: usize,
    /// DELTA_TXN_ALLOW_INSECURE=true: start even with no API key and no
    /// table allowlist (local development only).
    pub allow_insecure: bool,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// When set, clients must present a certificate signed by this CA
    /// (mutual TLS).
    pub client_ca_path: Option<PathBuf>,
}

/// One accepted API key. `name` identifies the caller in traces and logs
/// (the request span's `client` field) and is never secret; `key` is.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey {
    pub name: String,
    pub key: String,
}

// Hand-written so a key never ends up in a Debug log line.
impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKey")
            .field("name", &self.name)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Reads the configuration from the process environment. See
/// parse_grpc_config() for the variables.
pub fn load_grpc_config() -> Result<GrpcConfig, String> {
    parse_grpc_config(|name| std::env::var(name).ok())
}

/// The environment-independent core of load_grpc_config(): `lookup` maps a
/// variable name to its value, so tests can pass a plain map.
///
/// - `DELTA_TXN_GRPC_ADDR` (default `0.0.0.0:50051`)
/// - `DELTA_TXN_GRPC_TLS_CERT` / `DELTA_TXN_GRPC_TLS_KEY` (both-or-neither)
/// - `DELTA_TXN_GRPC_TLS_CLIENT_CA` (optional, requires TLS; enables mTLS)
/// - `DELTA_TXN_GRPC_API_KEY` (optional single key, named `default`;
///   empty/whitespace-only is treated as unset)
/// - `DELTA_TXN_GRPC_API_KEYS` (optional comma-separated `name=key` list;
///   list both the old and new key while rotating)
/// - `DELTA_TXN_GRPC_REQUEST_TIMEOUT_SECS` (default 60; 0 disables)
/// - `DELTA_TXN_GRPC_MAX_CONCURRENT_REQUESTS_PER_CONNECTION` (default 64)
/// - `DELTA_TXN_GRPC_MAX_MESSAGE_BYTES` (default 4 MiB)
/// - `DELTA_TXN_ALLOW_INSECURE` (`true`/`1` to allow running with neither
///   auth nor a table allowlist)
pub fn parse_grpc_config(lookup: impl Fn(&str) -> Option<String>) -> Result<GrpcConfig, String> {
    let non_empty = |name: &str| {
        lookup(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    let addr_env = non_empty("DELTA_TXN_GRPC_ADDR").unwrap_or_else(|| "0.0.0.0:50051".to_string());
    let addr: SocketAddr = addr_env
        .parse()
        .map_err(|e| format!("DELTA_TXN_GRPC_ADDR '{addr_env}' is not a valid address: {e}"))?;

    let client_ca_path = non_empty("DELTA_TXN_GRPC_TLS_CLIENT_CA").map(PathBuf::from);
    let tls = match (
        non_empty("DELTA_TXN_GRPC_TLS_CERT"),
        non_empty("DELTA_TXN_GRPC_TLS_KEY"),
    ) {
        (Some(cert_path), Some(key_path)) => Some(TlsConfig {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            client_ca_path,
        }),
        (None, None) => {
            if client_ca_path.is_some() {
                return Err(
                    "DELTA_TXN_GRPC_TLS_CLIENT_CA requires DELTA_TXN_GRPC_TLS_CERT and \
                     DELTA_TXN_GRPC_TLS_KEY"
                        .to_string(),
                );
            }
            None
        }
        _ => {
            return Err(
                "DELTA_TXN_GRPC_TLS_CERT and DELTA_TXN_GRPC_TLS_KEY must both be set".to_string(),
            )
        }
    };

    let mut api_keys = Vec::new();
    if let Some(key) = non_empty("DELTA_TXN_GRPC_API_KEY") {
        api_keys.push(ApiKey {
            name: "default".to_string(),
            key,
        });
    }
    if let Some(list) = non_empty("DELTA_TXN_GRPC_API_KEYS") {
        for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (name, key) = entry.split_once('=').ok_or_else(|| {
                "DELTA_TXN_GRPC_API_KEYS entries must be name=key (one entry has no '=')"
                    .to_string()
            })?;
            let (name, key) = (name.trim(), key.trim());
            if name.is_empty() || key.is_empty() {
                return Err(
                    "DELTA_TXN_GRPC_API_KEYS entries need a non-empty name and key".to_string(),
                );
            }
            if api_keys.iter().any(|k: &ApiKey| k.name == name) {
                return Err(format!(
                    "API key name '{name}' is configured more than once"
                ));
            }
            api_keys.push(ApiKey {
                name: name.to_string(),
                key: key.to_string(),
            });
        }
    }

    let number = |name: &str, default: usize| -> Result<usize, String> {
        match non_empty(name) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| format!("{name} must be a non-negative integer, got '{v}'")),
        }
    };
    let timeout_secs = number(
        "DELTA_TXN_GRPC_REQUEST_TIMEOUT_SECS",
        DEFAULT_REQUEST_TIMEOUT.as_secs() as usize,
    )?;
    let request_timeout = (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs as u64));
    let max_concurrent_requests_per_connection = number(
        "DELTA_TXN_GRPC_MAX_CONCURRENT_REQUESTS_PER_CONNECTION",
        DEFAULT_MAX_CONCURRENT_REQUESTS_PER_CONNECTION,
    )?;
    if max_concurrent_requests_per_connection == 0 {
        return Err("DELTA_TXN_GRPC_MAX_CONCURRENT_REQUESTS_PER_CONNECTION must be > 0".into());
    }
    let max_message_bytes = number(
        "DELTA_TXN_GRPC_MAX_MESSAGE_BYTES",
        DEFAULT_MAX_MESSAGE_BYTES,
    )?;
    if max_message_bytes == 0 {
        return Err("DELTA_TXN_GRPC_MAX_MESSAGE_BYTES must be > 0".into());
    }

    let allow_insecure = matches!(
        non_empty("DELTA_TXN_ALLOW_INSECURE")
            .map(|v| v.to_ascii_lowercase())
            .as_deref(),
        Some("true" | "1" | "yes")
    );

    Ok(GrpcConfig {
        addr,
        tls,
        api_keys,
        request_timeout,
        max_concurrent_requests_per_connection,
        max_message_bytes,
        allow_insecure,
    })
}

/// Startup security policy, given the gRPC config and whether a table
/// allowlist (DELTA_TXN_ALLOWED_TABLE_PREFIXES) is set. Returns the
/// warnings to log, or an error if the server must not start:
///
/// - No API key *and* no allowlist means any client that can reach the
///   port can read and write any table the storage credentials reach.
///   That's refused unless DELTA_TXN_ALLOW_INSECURE is set.
/// - API keys without TLS travel in cleartext: warned, not refused (a
///   service mesh or private network may already encrypt the hop).
pub fn check_security(config: &GrpcConfig, has_allowlist: bool) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    if config.api_keys.is_empty() && !has_allowlist {
        if !config.allow_insecure {
            return Err(
                "refusing to start: no API key (DELTA_TXN_GRPC_API_KEY / DELTA_TXN_GRPC_API_KEYS) \
                 and no table allowlist (DELTA_TXN_ALLOWED_TABLE_PREFIXES) are configured, so any \
                 client could read and write any table. Configure one of them, or set \
                 DELTA_TXN_ALLOW_INSECURE=true for local development."
                    .to_string(),
            );
        }
        warnings.push(
            "DELTA_TXN_ALLOW_INSECURE is set: running with no API key and no table allowlist"
                .to_string(),
        );
    }
    if !config.api_keys.is_empty() && config.tls.is_none() {
        warnings.push(
            "API keys are configured but TLS is not: keys are sent in cleartext unless the \
             network path is encrypted some other way"
                .to_string(),
        );
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse(vars: &[(&str, &str)]) -> Result<GrpcConfig, String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        parse_grpc_config(|name| map.get(name).cloned())
    }

    #[test]
    fn defaults() {
        let c = parse(&[]).unwrap();
        assert_eq!(c.addr.to_string(), "0.0.0.0:50051");
        assert!(c.tls.is_none());
        assert!(c.api_keys.is_empty());
        assert_eq!(c.request_timeout, Some(DEFAULT_REQUEST_TIMEOUT));
        assert_eq!(
            c.max_concurrent_requests_per_connection,
            DEFAULT_MAX_CONCURRENT_REQUESTS_PER_CONNECTION
        );
        assert_eq!(c.max_message_bytes, DEFAULT_MAX_MESSAGE_BYTES);
        assert!(!c.allow_insecure);
    }

    #[test]
    fn invalid_addr_is_an_error() {
        assert!(parse(&[("DELTA_TXN_GRPC_ADDR", "not-an-addr")]).is_err());
    }

    #[test]
    fn tls_needs_both_cert_and_key() {
        assert!(parse(&[("DELTA_TXN_GRPC_TLS_CERT", "/c")]).is_err());
        assert!(parse(&[("DELTA_TXN_GRPC_TLS_KEY", "/k")]).is_err());
        let c = parse(&[
            ("DELTA_TXN_GRPC_TLS_CERT", "/c"),
            ("DELTA_TXN_GRPC_TLS_KEY", "/k"),
            ("DELTA_TXN_GRPC_TLS_CLIENT_CA", "/ca"),
        ])
        .unwrap();
        let tls = c.tls.unwrap();
        assert_eq!(tls.cert_path, PathBuf::from("/c"));
        assert_eq!(tls.client_ca_path, Some(PathBuf::from("/ca")));
    }

    #[test]
    fn client_ca_without_tls_is_an_error() {
        assert!(parse(&[("DELTA_TXN_GRPC_TLS_CLIENT_CA", "/ca")]).is_err());
    }

    #[test]
    fn api_keys_single_and_named() {
        let c = parse(&[
            ("DELTA_TXN_GRPC_API_KEY", "  k0  "),
            ("DELTA_TXN_GRPC_API_KEYS", "ingest=k1, reader = k2 ,"),
        ])
        .unwrap();
        let names: Vec<_> = c.api_keys.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["default", "ingest", "reader"]);
        assert_eq!(c.api_keys[0].key, "k0");
        assert_eq!(c.api_keys[2].key, "k2");
    }

    #[test]
    fn blank_api_key_is_unset() {
        assert!(parse(&[("DELTA_TXN_GRPC_API_KEY", "   ")])
            .unwrap()
            .api_keys
            .is_empty());
    }

    #[test]
    fn malformed_api_keys_are_errors() {
        assert!(parse(&[("DELTA_TXN_GRPC_API_KEYS", "nokey")]).is_err());
        assert!(parse(&[("DELTA_TXN_GRPC_API_KEYS", "=k")]).is_err());
        assert!(parse(&[("DELTA_TXN_GRPC_API_KEYS", "a=")]).is_err());
        assert!(parse(&[("DELTA_TXN_GRPC_API_KEYS", "a=1,a=2")]).is_err());
        assert!(parse(&[
            ("DELTA_TXN_GRPC_API_KEY", "x"),
            ("DELTA_TXN_GRPC_API_KEYS", "default=y")
        ])
        .is_err());
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let k = ApiKey {
            name: "ingest".into(),
            key: "super-secret".into(),
        };
        let shown = format!("{k:?}");
        assert!(shown.contains("ingest"));
        assert!(!shown.contains("super-secret"));
    }

    #[test]
    fn limits_parse_and_validate() {
        let c = parse(&[
            ("DELTA_TXN_GRPC_REQUEST_TIMEOUT_SECS", "0"),
            ("DELTA_TXN_GRPC_MAX_CONCURRENT_REQUESTS_PER_CONNECTION", "8"),
            ("DELTA_TXN_GRPC_MAX_MESSAGE_BYTES", "1024"),
        ])
        .unwrap();
        assert_eq!(c.request_timeout, None);
        assert_eq!(c.max_concurrent_requests_per_connection, 8);
        assert_eq!(c.max_message_bytes, 1024);

        assert!(parse(&[("DELTA_TXN_GRPC_REQUEST_TIMEOUT_SECS", "soon")]).is_err());
        assert!(parse(&[("DELTA_TXN_GRPC_MAX_CONCURRENT_REQUESTS_PER_CONNECTION", "0")]).is_err());
        assert!(parse(&[("DELTA_TXN_GRPC_MAX_MESSAGE_BYTES", "0")]).is_err());
    }

    #[test]
    fn allow_insecure_values() {
        for v in ["true", "TRUE", "1", "yes"] {
            assert!(
                parse(&[("DELTA_TXN_ALLOW_INSECURE", v)])
                    .unwrap()
                    .allow_insecure
            );
        }
        for v in ["false", "0", "no", ""] {
            assert!(
                !parse(&[("DELTA_TXN_ALLOW_INSECURE", v)])
                    .unwrap()
                    .allow_insecure
            );
        }
    }

    #[test]
    fn security_policy() {
        let open = parse(&[]).unwrap();
        assert!(
            check_security(&open, false).is_err(),
            "no auth, no allowlist"
        );
        assert!(
            check_security(&open, true).unwrap().is_empty(),
            "allowlist only"
        );

        let insecure = parse(&[("DELTA_TXN_ALLOW_INSECURE", "true")]).unwrap();
        assert_eq!(check_security(&insecure, false).unwrap().len(), 1);

        let keyed_plaintext = parse(&[("DELTA_TXN_GRPC_API_KEY", "k")]).unwrap();
        let warnings = check_security(&keyed_plaintext, false).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("cleartext"));

        let keyed_tls = parse(&[
            ("DELTA_TXN_GRPC_API_KEY", "k"),
            ("DELTA_TXN_GRPC_TLS_CERT", "/c"),
            ("DELTA_TXN_GRPC_TLS_KEY", "/k"),
        ])
        .unwrap();
        assert!(check_security(&keyed_tls, false).unwrap().is_empty());
    }
}
