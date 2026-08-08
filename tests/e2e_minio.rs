//! Runs a slice of the golden-path integration suite against a real
//! S3-compatible backend (MinIO) instead of the `file://` filesystem
//! backend the rest of `tests/` uses. This is the one place this repo
//! actually verifies `table_exists`/`create_table`/optimistic-concurrency
//! conflict detection against something S3-shaped -- `object_store`'s
//! backends don't all share identical semantics (conditional-put
//! behavior, listing consistency), and local-filesystem rename-based
//! atomicity passing doesn't prove S3's own conditional-put path does.
//!
//! Only runs when the environment supplies real MinIO connection info
//! (`AWS_ENDPOINT_URL` + `DELTA_TXN_TEST_S3_BUCKET`); every test here is
//! otherwise a silent no-op that still reports "ok". That's deliberate,
//! not sloppy: this file still compiles and gets picked up by a bare
//! `cargo test` -- including the Docker build's own `RUN cargo test
//! --release` (see Dockerfile), where no MinIO is reachable -- and
//! runtime-skipping is the standard way to make a Rust test conditional
//! without a heavier test harness/custom runner. `.github/workflows/ci.yml`'s
//! `minio-integration-tests` job is what actually sets these env vars and
//! gets real coverage out of this file.

mod common;

use common::{add_file_action, commit_request, create_table_actions, pb, TestServerConfig};
use std::collections::HashMap;
use tonic::Code;

/// `None` means "no MinIO configured for this test run" -- every test
/// below treats that as skip, not failure.
fn minio_storage_opts() -> Option<HashMap<String, String>> {
    let endpoint = std::env::var("AWS_ENDPOINT_URL").ok()?;
    let mut opts = HashMap::new();
    opts.insert("AWS_ENDPOINT_URL".to_string(), endpoint);
    for key in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_REGION",
        "AWS_ALLOW_HTTP",
    ] {
        if let Ok(value) = std::env::var(key) {
            opts.insert(key.to_string(), value);
        }
    }
    Some(opts)
}

fn minio_table_uri(label: &str) -> Option<String> {
    let bucket = std::env::var("DELTA_TXN_TEST_S3_BUCKET").ok()?;
    Some(format!("s3://{bucket}/e2e-minio-tests/{label}"))
}

/// `$label` is just a unique path segment for this test's table -- doesn't
/// need to match the test function's name, just be distinct from every
/// other test in this file so they don't collide on the same MinIO
/// bucket/key.
macro_rules! skip_unless_minio_configured {
    ($label:expr) => {
        match (minio_storage_opts(), minio_table_uri($label)) {
            (Some(opts), Some(uri)) => (opts, uri),
            _ => {
                eprintln!(
                    "skipping {}: AWS_ENDPOINT_URL/DELTA_TXN_TEST_S3_BUCKET not set \
                     (expected outside CI's minio-integration-tests job)",
                    $label
                );
                return;
            }
        }
    };
}

#[tokio::test]
async fn commit_creates_a_new_table_on_minio_and_get_table_reads_it_back() {
    let (storage_opts, table_uri) = skip_unless_minio_configured!("create-and-read");

    let server = common::TestServer::start(TestServerConfig {
        storage_opts,
        ..Default::default()
    })
    .await;
    let mut client = server.connect().await;

    let commit_response = client
        .commit(commit_request(
            &table_uri,
            None,
            create_table_actions("orders"),
        ))
        .await
        .expect("Commit that creates a new table on MinIO should succeed")
        .into_inner();
    assert_eq!(commit_response.committed_version, 0);

    let get_table_response = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
        })
        .await
        .expect("GetTable against MinIO should succeed")
        .into_inner();
    assert_eq!(get_table_response.version, 0);
    assert_eq!(
        get_table_response.metadata.expect("expected metadata").name,
        "orders"
    );
}

#[tokio::test]
async fn commit_append_and_version_conflict_detection_work_against_minio() {
    // The important case this proves that a file://-only suite can't:
    // delta-rs's optimistic-concurrency conflict detection depends on the
    // storage backend's own conditional-put semantics. Local filesystem
    // achieves atomicity via rename(); S3-compatible stores do it through
    // a genuinely different mechanism (conditional PUT). This test is what
    // actually exercises that second path.
    let (storage_opts, table_uri) = skip_unless_minio_configured!("version-conflict");

    let server = common::TestServer::start(TestServerConfig {
        storage_opts,
        ..Default::default()
    })
    .await;
    let mut client = server.connect().await;

    client
        .commit(commit_request(
            &table_uri,
            None,
            create_table_actions("orders"),
        ))
        .await
        .expect("create commit should succeed");

    let append_response = client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![add_file_action("part-00000.parquet", 3)],
        ))
        .await
        .expect("append with correct expected_version should succeed")
        .into_inner();
    assert_eq!(append_response.committed_version, 1);

    let err = client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![add_file_action("part-00001.parquet", 1)],
        ))
        .await
        .expect_err("stale expected_version must be rejected even against a real S3 backend");
    assert_eq!(err.code(), Code::Aborted);
}

#[tokio::test]
async fn list_active_files_round_trips_against_minio() {
    let (storage_opts, table_uri) = skip_unless_minio_configured!("list-active-files");

    let server = common::TestServer::start(TestServerConfig {
        storage_opts,
        ..Default::default()
    })
    .await;
    let mut client = server.connect().await;

    client
        .commit(commit_request(
            &table_uri,
            None,
            create_table_actions("orders"),
        ))
        .await
        .expect("create commit should succeed");
    client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![add_file_action("part-00000.parquet", 3)],
        ))
        .await
        .expect("append should succeed");

    let mut stream = client
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri: table_uri.clone(),
        })
        .await
        .expect("ListActiveFiles against MinIO should succeed")
        .into_inner();

    stream.message().await.unwrap(); // header
    let batch_msg = stream
        .message()
        .await
        .expect("stream should not error")
        .expect("stream should yield a batch message");
    let batch = match batch_msg.payload {
        Some(pb::list_active_files_response::Payload::Batch(b)) => b,
        other => panic!("expected a batch message, got {other:?}"),
    };
    assert_eq!(batch.files.len(), 1);
    assert_eq!(batch.files[0].path, "part-00000.parquet");
}
