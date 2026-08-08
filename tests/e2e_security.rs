//! API-key auth and table_uri-allowlist coverage against a real server --
//! grpc::auth's own unit tests check `make_auth_interceptor` as a bare
//! function; this checks it's actually wired into the served RPCs the way
//! main.rs wires it (and that a rejected request never reaches a handler
//! at all, rather than, say, being rejected only after touching storage).

mod common;

use common::{pb, TestServerConfig};
use tonic::Code;

#[tokio::test]
async fn requests_without_an_api_key_are_rejected_when_one_is_configured() {
    let server = common::TestServer::start(TestServerConfig {
        api_key: Some("super-secret".to_string()),
        ..Default::default()
    })
    .await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    let err = client
        .get_table(pb::GetTableRequest { table_uri })
        .await
        .expect_err("a request with no credentials must be rejected when an API key is set");
    assert_eq!(err.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn requests_with_the_wrong_api_key_are_rejected() {
    let server = common::TestServer::start(TestServerConfig {
        api_key: Some("super-secret".to_string()),
        ..Default::default()
    })
    .await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    let req = common::with_api_key(
        tonic::Request::new(pb::GetTableRequest { table_uri }),
        "wrong-key",
    );
    let err = client
        .get_table(req)
        .await
        .expect_err("a request with the wrong API key must be rejected");
    assert_eq!(err.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn requests_with_the_correct_api_key_are_accepted() {
    let server = common::TestServer::start(TestServerConfig {
        api_key: Some("super-secret".to_string()),
        ..Default::default()
    })
    .await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    // A missing table_uri would still surface as NotFound past the auth
    // check -- what matters here is that it's NotFound, not Unauthenticated,
    // proving the correct key got the request past the interceptor.
    let req = common::with_api_key(
        tonic::Request::new(pb::GetTableRequest { table_uri }),
        "super-secret",
    );
    let err = client
        .get_table(req)
        .await
        .expect_err("the table itself still doesn't exist");
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn table_uri_outside_the_allowlist_is_rejected() {
    // The configured prefix deliberately doesn't correspond to any real
    // directory -- is_table_uri_allowed() is a pure string comparison
    // against the *configured* prefix, which never itself goes through
    // ensure_table_uri()/local-directory creation. The requested table_uri
    // below, on the other hand, does: `ensure_table_uri` creates a local
    // path's directory as a side effect *before* the allowlist check even
    // runs (see deltalake's own `ensure_table_uri` doc comment), so it has
    // to be a path under this test's own managed tempdir (via
    // `server.new_table_uri`) -- not some arbitrary real-filesystem path
    // like "/not-allowed/orders" -- or this test would actually create
    // that directory on whatever machine runs it.
    let server = common::TestServer::start(TestServerConfig {
        allowed_table_prefixes: Some(vec![
            "file:///this-prefix-matches-nothing-the-test-server-creates".to_string(),
        ]),
        ..Default::default()
    })
    .await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    let err = client
        .get_table(pb::GetTableRequest { table_uri })
        .await
        .expect_err("a table_uri outside the configured allowlist must be rejected");
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn table_uri_outside_the_allowlist_never_creates_a_local_directory() {
    // Regression test for the fix to a real gap: ensure_table_uri()
    // creates a local `file://`/bare-path table_uri's directory on disk as
    // a side effect, unconditionally -- grpc::server's three handlers used
    // to call it *before* checking the allowlist at all, so an
    // authenticated-but-not-authorized-for-this-path client could still
    // cause directory creation outside their allowed scope even though the
    // request itself was correctly rejected. Every handler now checks the
    // allowlist against the raw table_uri first, before ensure_table_uri()
    // ever runs.
    let tmp_dir = tempfile::tempdir().expect("failed to create tempdir");
    let disallowed_path = tmp_dir.path().join("should-never-be-created");
    let disallowed_uri = format!("file://{}", disallowed_path.display());

    let server = common::TestServer::start(TestServerConfig {
        allowed_table_prefixes: Some(vec![
            "file:///this-prefix-matches-nothing".to_string(),
        ]),
        ..Default::default()
    })
    .await;
    let mut client = server.connect().await;

    let err = client
        .get_table(pb::GetTableRequest {
            table_uri: disallowed_uri,
        })
        .await
        .expect_err("a table_uri outside the configured allowlist must be rejected");
    assert_eq!(err.code(), Code::PermissionDenied);
    assert!(
        !disallowed_path.exists(),
        "a rejected table_uri must not cause local directory creation as a side effect"
    );
}

#[tokio::test]
async fn table_uri_inside_the_allowlist_passes_the_check() {
    let tmp_dir = tempfile::tempdir().expect("failed to create tempdir");
    let allowed_root = format!("file://{}", tmp_dir.path().display());

    let server = common::TestServer::start(TestServerConfig {
        allowed_table_prefixes: Some(vec![allowed_root.clone()]),
        ..Default::default()
    })
    .await;
    let mut client = server.connect().await;

    // Past the allowlist check, the table still doesn't exist -- NotFound,
    // not PermissionDenied, is exactly the signal that the allowlist check
    // itself passed.
    let err = client
        .get_table(pb::GetTableRequest {
            table_uri: format!("{allowed_root}/orders"),
        })
        .await
        .expect_err("the table itself still doesn't exist");
    assert_eq!(err.code(), Code::NotFound);
}
