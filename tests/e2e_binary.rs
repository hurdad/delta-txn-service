//! Runs the real `delta-txn-service` executable (not the library through
//! the harness) to cover what only the process does: env-var config
//! loading, the fail-closed startup check, tracing/OTLP initialization,
//! and SIGTERM-driven graceful shutdown. cargo-llvm-cov collects coverage
//! from these child processes too.

#![cfg(unix)]

mod common;

use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use common::pb;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::HealthCheckRequest;

const BIN: &str = env!("CARGO_BIN_EXE_delta-txn-service");

/// Picks a port that's free right now. Another process could take it
/// before the child binds; on a test machine that's rare enough to accept.
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The child's environment is built from scratch (env_clear) so whatever
/// the developer or CI has exported (AWS_*, OTEL_*, RUST_LOG) can't change
/// the outcome. PATH and LLVM_PROFILE_FILE are kept: the latter is how
/// cargo-llvm-cov collects the child's coverage.
fn command(envs: &[(&str, String)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env_clear();
    for key in ["PATH", "LLVM_PROFILE_FILE", "CARGO_LLVM_COV_TARGET_DIR"] {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

async fn wait_until_serving(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let attempt = async {
            let channel =
                tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
                    .unwrap()
                    .connect()
                    .await
                    .ok()?;
            let mut client = tonic_health::pb::health_client::HealthClient::new(channel);
            client
                .check(HealthCheckRequest {
                    service: String::new(),
                })
                .await
                .ok()
                .map(|r| r.into_inner().status)
        };
        if attempt.await == Some(ServingStatus::Serving as i32) {
            return;
        }
        assert!(Instant::now() < deadline, "binary never became healthy");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Async, not a blocking sleep loop: graceful shutdown waits for open
/// connections to close, and a client in this test can only react to the
/// server's GOAWAY if the test's runtime thread isn't blocked.
async fn wait_for_exit(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("binary did not exit within {limit:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Starts the binary, checks it serves, shuts it down with SIGTERM and
/// returns its captured output.
async fn run_and_terminate(envs: &[(&str, String)], port: u16) -> Output {
    let mut child = command(envs).spawn().expect("failed to spawn binary");
    wait_until_serving(port).await;

    let mut client = common::Client::connect(format!("http://127.0.0.1:{port}"))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = client
        .get_table(pb::GetTableRequest {
            table_uri: format!("file://{}/missing", dir.path().display()),
            ..Default::default()
        })
        .await
        .expect_err("table does not exist");
    assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");

    sigterm(&child);
    let status = wait_for_exit(&mut child, Duration::from_secs(30)).await;
    let output = child.wait_with_output().unwrap();
    assert!(
        status.success(),
        "graceful shutdown should exit 0, got {status:?}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[tokio::test]
async fn binary_serves_and_shuts_down_gracefully_on_sigterm() {
    let port = free_port();
    let output = run_and_terminate(
        &[
            ("DELTA_TXN_GRPC_ADDR", format!("127.0.0.1:{port}")),
            ("DELTA_TXN_ALLOW_INSECURE", "true".into()),
            ("DELTA_TXN_MAX_ACTIONS_PER_COMMIT", "10".into()),
        ],
        port,
    )
    .await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("DeltaTxnService listening on"), "{stdout}");
    assert!(stdout.contains("shutdown signal received"), "{stdout}");
    // Running with neither an API key nor an allowlist is allowed here
    // only because of DELTA_TXN_ALLOW_INSECURE, and must be warned about.
    assert!(stdout.contains("WARN"), "{stdout}");
}

#[tokio::test]
async fn binary_with_json_logs_otlp_export_and_storage_probe() {
    let port = free_port();
    // Nothing listens on these: the OTLP exporters and the storage probe
    // must tolerate an unreachable endpoint without affecting serving.
    let dead_port = free_port();
    let output = run_and_terminate(
        &[
            ("DELTA_TXN_GRPC_ADDR", format!("127.0.0.1:{port}")),
            ("DELTA_TXN_ALLOWED_TABLE_PREFIXES", "file:///".into()),
            ("DELTA_TXN_LOG_FORMAT", "json".into()),
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                format!("http://127.0.0.1:{dead_port}"),
            ),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf".into()),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "200".into()),
            ("OTEL_SERVICE_NAME", "delta-txn-binary-test".into()),
            ("AWS_ENDPOINT_URL", format!("http://127.0.0.1:{dead_port}")),
            ("AWS_REGION", "us-east-1".into()),
        ],
        port,
    )
    .await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf is not supported"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().expect("expected log output");
    let parsed: serde_json::Value =
        serde_json::from_str(first_line).expect("DELTA_TXN_LOG_FORMAT=json emits JSON lines");
    assert!(parsed.get("level").is_some(), "{parsed}");
}

/// GetTable needs the API key here; the request in run_and_terminate is
/// unauthenticated, so this variant checks the key is enforced instead.
#[tokio::test]
async fn binary_enforces_api_key_from_env() {
    let port = free_port();
    let mut child = command(&[
        ("DELTA_TXN_GRPC_ADDR", format!("127.0.0.1:{port}")),
        ("DELTA_TXN_GRPC_API_KEY", "binary-test-key".into()),
    ])
    .spawn()
    .unwrap();
    wait_until_serving(port).await;

    let mut client = common::Client::connect(format!("http://127.0.0.1:{port}"))
        .await
        .unwrap();
    let err = client
        .get_table(pb::GetTableRequest {
            table_uri: "file:///tmp/x".into(),
            ..Default::default()
        })
        .await
        .expect_err("no API key supplied");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    sigterm(&child);
    assert!(wait_for_exit(&mut child, Duration::from_secs(30))
        .await
        .success());
}

#[tokio::test]
async fn binary_refuses_to_start_without_auth_or_allowlist() {
    let port = free_port();
    let mut child = command(&[("DELTA_TXN_GRPC_ADDR", format!("127.0.0.1:{port}"))])
        .spawn()
        .unwrap();
    let status = wait_for_exit(&mut child, Duration::from_secs(30)).await;
    assert!(!status.success(), "startup must fail closed");
}

#[tokio::test]
async fn binary_rejects_invalid_config() {
    let mut child = command(&[("DELTA_TXN_GRPC_ADDR", "not-an-address".into())])
        .spawn()
        .unwrap();
    let status = wait_for_exit(&mut child, Duration::from_secs(30)).await;
    assert!(!status.success());
    let output = child.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("DELTA_TXN_GRPC_ADDR"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
