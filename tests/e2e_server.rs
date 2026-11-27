//! Server-level behavior of `app::serve` that isn't specific to any one
//! RPC: TLS and mutual TLS, startup failure on bad TLS files, graceful
//! shutdown, the storage readiness probe, and the request-size limit.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{commit_request, create_table_actions, pb, TestServer, TestServerConfig};
use delta_txn_service::config::grpc::TlsConfig;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::HealthCheckRequest;

const SERVICE_NAME: &str = "delta.txn.v1.DeltaTxnService";

/// A throwaway CA plus a server certificate for `localhost` and a client
/// certificate, all written as PEM files under `dir`.
struct TestPki {
    ca_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
    tls: TlsConfig,
}

fn make_pki(dir: &Path, require_client_cert: bool) -> TestPki {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();

    let server_key = KeyPair::generate().unwrap();
    let server_cert = CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .signed_by(&server_key, &ca)
        .unwrap();
    let client_key = KeyPair::generate().unwrap();
    let client_cert = CertificateParams::new(vec!["client".to_string()])
        .unwrap()
        .signed_by(&client_key, &ca)
        .unwrap();

    let write = |name: &str, contents: &str| {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    };
    let ca_path = write("ca.pem", &ca.pem());
    let tls = TlsConfig {
        cert_path: write("server.pem", &server_cert.pem()),
        key_path: write("server.key", &server_key.serialize_pem()),
        client_ca_path: require_client_cert.then_some(ca_path),
    };
    TestPki {
        ca_pem: ca.pem(),
        client_cert_pem: client_cert.pem(),
        client_key_pem: client_key.serialize_pem(),
        tls,
    }
}

async fn tls_client(
    server: &TestServer,
    pki: &TestPki,
    with_client_cert: bool,
) -> Result<common::Client, tonic::transport::Error> {
    let mut tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&pki.ca_pem))
        .domain_name("localhost");
    if with_client_cert {
        tls = tls.identity(Identity::from_pem(
            &pki.client_cert_pem,
            &pki.client_key_pem,
        ));
    }
    let channel = Endpoint::from_shared(format!("https://localhost:{}", server.addr().port()))
        .unwrap()
        .tls_config(tls)?
        .connect()
        .await?;
    Ok(common::Client::new(channel))
}

async fn create_table(client: &mut common::Client, table_uri: &str) -> Result<i64, tonic::Status> {
    client
        .commit(commit_request(table_uri, None, create_table_actions("t")))
        .await
        .map(|r| r.into_inner().committed_version)
}

#[tokio::test]
async fn tls_server_accepts_a_client_that_trusts_its_ca() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make_pki(dir.path(), false);
    let server = TestServer::start(TestServerConfig {
        tls: Some(pki.tls.clone()),
        ..Default::default()
    })
    .await;

    let mut client = tls_client(&server, &pki, false).await.expect("TLS connect");
    let version = create_table(&mut client, &server.new_table_uri("tls"))
        .await
        .expect("commit over TLS");
    assert_eq!(version, 0);

    // A plaintext client can't talk to a TLS server.
    let mut plain = server.connect().await;
    assert!(create_table(&mut plain, &server.new_table_uri("plain"))
        .await
        .is_err());
}

#[tokio::test]
async fn mutual_tls_requires_a_client_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make_pki(dir.path(), true);
    let server = TestServer::start(TestServerConfig {
        tls: Some(pki.tls.clone()),
        ..Default::default()
    })
    .await;

    // Without a client certificate the handshake (or the first RPC on the
    // connection, depending on when rustls reports it) fails.
    let rejected = match tls_client(&server, &pki, false).await {
        Err(_) => true,
        Ok(mut client) => create_table(&mut client, &server.new_table_uri("no-cert"))
            .await
            .is_err(),
    };
    assert!(rejected, "a client without a certificate must be rejected");

    let mut client = tls_client(&server, &pki, true).await.expect("mTLS connect");
    create_table(&mut client, &server.new_table_uri("with-cert"))
        .await
        .expect("commit with a client certificate");
}

#[tokio::test]
async fn missing_tls_files_fail_startup() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(TestServerConfig {
        tls: Some(TlsConfig {
            cert_path: dir.path().join("missing.pem"),
            key_path: dir.path().join("missing.key"),
            client_ca_path: None,
        }),
        ..Default::default()
    })
    .await;
    assert!(server.join().await.is_err());
}

#[tokio::test]
async fn missing_client_ca_file_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let mut pki = make_pki(dir.path(), true);
    pki.tls.client_ca_path = Some(dir.path().join("missing-ca.pem"));
    let server = TestServer::start(TestServerConfig {
        tls: Some(pki.tls),
        ..Default::default()
    })
    .await;
    assert!(server.join().await.is_err());
}

#[tokio::test]
async fn graceful_shutdown_returns_cleanly() {
    let server = TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    create_table(&mut client, &server.new_table_uri("before-shutdown"))
        .await
        .unwrap();
    drop(client);
    server.shutdown().await.expect("serve should return Ok");
}

async fn service_status(server: &TestServer) -> ServingStatus {
    let mut health = server.health_client().await;
    let status = health
        .check(HealthCheckRequest {
            service: SERVICE_NAME.to_string(),
        })
        .await
        .unwrap()
        .into_inner()
        .status;
    ServingStatus::try_from(status).unwrap()
}

/// Polls until DeltaTxnService reports `want`, failing after 10s.
async fn wait_for_status(server: &TestServer, want: ServingStatus) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while service_status(server).await != want {
        assert!(Instant::now() < deadline, "status never became {want:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn probe_server(endpoint: String) -> TestServer {
    TestServer::start(TestServerConfig {
        storage_probe_endpoint: Some(endpoint),
        storage_probe_interval: Some(Duration::from_millis(20)),
        ..Default::default()
    })
    .await
}

#[tokio::test]
async fn storage_probe_tracks_endpoint_reachability() {
    let storage = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = storage.local_addr().unwrap().port();
    let server = probe_server(format!("http://127.0.0.1:{port}")).await;
    wait_for_status(&server, ServingStatus::Serving).await;

    // Once nothing listens there any more, readiness must drop.
    drop(storage);
    wait_for_status(&server, ServingStatus::NotServing).await;

    // Liveness (the overall-server status) is never affected.
    let mut health = server.health_client().await;
    let overall = health
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .unwrap()
        .into_inner()
        .status;
    assert_eq!(overall, ServingStatus::Serving as i32);
}

#[tokio::test]
async fn malformed_storage_endpoints_mark_the_service_not_serving() {
    for endpoint in ["not a url", "file:///no/host", "custom://host-without-port"] {
        let server = probe_server(endpoint.to_string()).await;
        assert_eq!(
            service_status(&server).await,
            ServingStatus::NotServing,
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn requests_over_the_message_size_limit_are_rejected() {
    let server = TestServer::start(TestServerConfig {
        max_message_bytes: Some(4096),
        ..Default::default()
    })
    .await;
    let mut client = server.connect().await;

    let mut request = commit_request(
        &server.new_table_uri("big"),
        None,
        create_table_actions("t"),
    );
    request
        .app_metadata
        .insert("padding".to_string(), "x".repeat(8192));
    let err = client.commit(request).await.expect_err("too large");
    assert_eq!(err.code(), tonic::Code::OutOfRange, "{err:?}");
}

#[tokio::test]
async fn incoming_traceparent_is_accepted() {
    let server = TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let mut request = tonic::Request::new(pb::GetTableRequest {
        table_uri: server.new_table_uri("traced"),
        ..Default::default()
    });
    request.metadata_mut().insert(
        "traceparent",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            .parse()
            .unwrap(),
    );
    let err = client.get_table(request).await.expect_err("no such table");
    assert_eq!(err.code(), tonic::Code::NotFound);
}
