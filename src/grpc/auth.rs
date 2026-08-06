use tonic::{Request, Status};

/// Constant-time string comparison to avoid leaking the configured API key
/// one byte at a time through response-timing side channels (an attacker
/// measuring how long rejection takes for a guessed key could otherwise
/// learn how many leading bytes matched).
///
/// The early `a.len() != b.len()` return *is* a timing leak of the key's
/// length specifically -- a deliberate, standard tradeoff shared by
/// essentially every constant-time-compare implementation (including
/// well-reviewed ones like the `subtle` crate's), not an oversight: byte
/// *content* is what actually needs protecting (it's the secret), and an
/// API key's length is rarely treated as sensitive on its own (often
/// fixed/predictable anyway, e.g. a UUID).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Builds a tonic request interceptor checking `api_key` against every
/// request's metadata -- either the `x-api-key` header directly, or a
/// `Bearer <token>` `authorization` header (checked in that order; the
/// first match wins, but both are always attempted if the first is
/// absent/wrong, not short-circuited on presence alone). `api_key: None`
/// makes this a pure pass-through -- every request accepted regardless of
/// credentials -- matching the "empty/whitespace-only DELTA_TXN_GRPC_API_KEY
/// treated as unset" behavior in config::grpc::load_grpc_config().
///
/// Runs as a tonic per-service interceptor (see main.rs's own comment on
/// how this composes with the tower-Layer-based
/// TraceContextLayer/GrpcMetricsLayer, which both still see -- and
/// record -- a request this rejects).
pub fn make_auth_interceptor(
    api_key: Option<String>,
) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |req: Request<()>| {
        let Some(api_key) = api_key.as_deref() else {
            return Ok(req);
        };

        let metadata = req.metadata();
        let mut authorized = metadata
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(|value| constant_time_eq(value, api_key))
            .unwrap_or(false);

        if !authorized {
            authorized = metadata
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(|value| constant_time_eq(value, api_key))
                .unwrap_or(false);
        }

        if authorized {
            Ok(req)
        } else {
            Err(Status::unauthenticated("missing or invalid api key"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_equal_strings() {
        assert!(constant_time_eq("super-secret", "super-secret"));
    }

    #[test]
    fn constant_time_eq_rejects_different_strings() {
        assert!(!constant_time_eq("super-secret", "super-secre0"));
        assert!(!constant_time_eq("short", "longer-value"));
        assert!(!constant_time_eq("", "nonempty"));
    }

    #[test]
    fn interceptor_allows_when_no_api_key_configured() {
        let interceptor = make_auth_interceptor(None);
        let req = Request::new(());
        assert!(interceptor(req).is_ok());
    }

    #[test]
    fn interceptor_accepts_valid_x_api_key_header() {
        let interceptor = make_auth_interceptor(Some("secret".to_string()));
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("x-api-key", "secret".parse().unwrap());
        assert!(interceptor(req).is_ok());
    }

    #[test]
    fn interceptor_accepts_valid_bearer_token() {
        let interceptor = make_auth_interceptor(Some("secret".to_string()));
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("authorization", "Bearer secret".parse().unwrap());
        assert!(interceptor(req).is_ok());
    }

    #[test]
    fn interceptor_rejects_missing_or_invalid_credentials() {
        let interceptor = make_auth_interceptor(Some("secret".to_string()));
        let req = Request::new(());
        assert!(interceptor(req).is_err());

        let interceptor = make_auth_interceptor(Some("secret".to_string()));
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("x-api-key", "wrong".parse().unwrap());
        assert!(interceptor(req).is_err());
    }
}
