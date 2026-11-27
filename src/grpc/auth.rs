use tonic::{Request, Status};

use crate::config::grpc::ApiKey;

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

/// The authenticated caller's API key name, attached to the request's
/// extensions by make_auth_interceptor so handlers can see who is calling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity(pub String);

/// Builds a tonic request interceptor that accepts a request carrying any
/// one of `api_keys`, sent either as an `x-api-key` header or as a
/// `Bearer <token>` `authorization` header. Every configured key is
/// compared (constant-time) against both headers, so the work done
/// doesn't depend on which key matched. An empty `api_keys` makes this a
/// pass-through: every request is accepted.
///
/// On success the matched key's name is recorded on the current
/// `grpc.request` span's `client` field and attached as a
/// ClientIdentity extension. Names are not secret; keys never leave this
/// function.
///
/// Runs as a tonic per-service interceptor (see main.rs's own comment on
/// how this composes with the tower-Layer-based
/// TraceContextLayer/GrpcMetricsLayer, which both still see -- and
/// record -- a request this rejects).
pub fn make_auth_interceptor(
    api_keys: Vec<ApiKey>,
) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |mut req: Request<()>| {
        if api_keys.is_empty() {
            return Ok(req);
        }

        let metadata = req.metadata();
        let header_key = metadata
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let bearer = metadata
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or("");

        let mut matched: Option<&ApiKey> = None;
        for candidate in &api_keys {
            let hit = constant_time_eq(header_key, &candidate.key)
                | constant_time_eq(bearer, &candidate.key);
            if hit && matched.is_none() {
                matched = Some(candidate);
            }
        }

        match matched {
            Some(key) => {
                tracing::Span::current().record("client", key.name.as_str());
                req.extensions_mut()
                    .insert(ClientIdentity(key.name.clone()));
                Ok(req)
            }
            None => Err(Status::unauthenticated("missing or invalid api key")),
        }
    }
}

/// Convenience for the common single-key case (and tests): `Some(key)`
/// becomes one key named `default`, `None` means no auth.
pub fn single_api_key(key: Option<String>) -> Vec<ApiKey> {
    key.map(|key| {
        vec![ApiKey {
            name: "default".to_string(),
            key,
        }]
    })
    .unwrap_or_default()
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
        let interceptor = make_auth_interceptor(Vec::new());
        let req = Request::new(());
        assert!(interceptor(req).is_ok());
    }

    #[test]
    fn interceptor_accepts_valid_x_api_key_header() {
        let interceptor = make_auth_interceptor(single_api_key(Some("secret".to_string())));
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("x-api-key", "secret".parse().unwrap());
        assert!(interceptor(req).is_ok());
    }

    #[test]
    fn interceptor_accepts_valid_bearer_token() {
        let interceptor = make_auth_interceptor(single_api_key(Some("secret".to_string())));
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("authorization", "Bearer secret".parse().unwrap());
        assert!(interceptor(req).is_ok());
    }

    #[test]
    fn interceptor_rejects_missing_or_invalid_credentials() {
        let interceptor = make_auth_interceptor(single_api_key(Some("secret".to_string())));
        let req = Request::new(());
        assert!(interceptor(req).is_err());

        let interceptor = make_auth_interceptor(single_api_key(Some("secret".to_string())));
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("x-api-key", "wrong".parse().unwrap());
        assert!(interceptor(req).is_err());
    }

    fn named_keys() -> Vec<ApiKey> {
        vec![
            ApiKey {
                name: "ingest".into(),
                key: "k-ingest".into(),
            },
            ApiKey {
                name: "reader".into(),
                key: "k-reader".into(),
            },
        ]
    }

    #[test]
    fn interceptor_accepts_any_configured_key_and_records_its_name() {
        let interceptor = make_auth_interceptor(named_keys());
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("authorization", "Bearer k-reader".parse().unwrap());
        let req = interceptor(req).expect("second key should be accepted");
        assert_eq!(
            req.extensions().get::<ClientIdentity>(),
            Some(&ClientIdentity("reader".to_string()))
        );
    }

    #[test]
    fn interceptor_rejects_unknown_key_and_non_bearer_scheme() {
        let interceptor = make_auth_interceptor(named_keys());
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("x-api-key", "k-other".parse().unwrap());
        assert!(interceptor(req).is_err());

        let interceptor = make_auth_interceptor(named_keys());
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("authorization", "Basic k-ingest".parse().unwrap());
        assert!(interceptor(req).is_err());
    }

    #[test]
    fn single_api_key_helper() {
        assert!(single_api_key(None).is_empty());
        let keys = single_api_key(Some("x".into()));
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "default");
    }
}
