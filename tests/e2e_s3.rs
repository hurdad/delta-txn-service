//! Runs a slice of the integration suite against a real S3-compatible
//! backend (SeaweedFS in CI and in deploy/docker-compose.yaml) instead of
//! the `file://` filesystem backend the rest of `tests/` uses. This is the
//! one place this repo verifies `table_exists`/`create_table`/optimistic
//! concurrency against something S3-shaped: `object_store`'s backends
//! don't share identical semantics, and local-filesystem rename-based
//! atomicity passing doesn't prove S3's conditional-put path does.
//!
//! Only runs when the environment supplies S3 connection info
//! (`AWS_ENDPOINT_URL` + `DELTA_TXN_TEST_S3_BUCKET`); every test here is
//! otherwise a silent no-op that still reports "ok". That's deliberate:
//! this file still compiles and runs under a bare `cargo test` -- including
//! the Docker build's own `RUN cargo test` step, where no S3 endpoint is
//! reachable. `.github/workflows/ci.yml`'s `s3-integration-tests` job sets
//! the env vars and gets real coverage out of this file.
//!
//! The backend must support conditional writes (`If-None-Match: *` on
//! PutObject) -- delta-rs's S3 commit protocol depends on them.

mod common;

use common::{add_file_action, commit_request, create_table_actions, pb, TestServerConfig};
use delta_txn_service::delta::cache::{LookupResult, SnapshotCache};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use tonic::Code;

/// `None` means "no S3 backend configured for this test run" -- every test
/// below treats that as skip, not failure.
fn s3_storage_opts() -> Option<HashMap<String, String>> {
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

/// One prefix per test-binary run, so rerunning against the same bucket
/// (common locally) never finds tables a previous run left behind.
fn run_id() -> &'static str {
    static RUN_ID: OnceLock<String> = OnceLock::new();
    RUN_ID.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("run-{nanos}")
    })
}

fn s3_table_uri(label: &str) -> Option<String> {
    let bucket = std::env::var("DELTA_TXN_TEST_S3_BUCKET").ok()?;
    Some(format!("s3://{bucket}/e2e-s3-tests/{}/{label}", run_id()))
}

/// `$label` is a unique path segment for this test's table, distinct from
/// every other test in this file so they don't collide in the bucket.
macro_rules! skip_unless_s3_configured {
    ($label:expr) => {
        match (s3_storage_opts(), s3_table_uri($label)) {
            (Some(opts), Some(uri)) => (opts, uri),
            _ => {
                eprintln!(
                    "skipping {}: AWS_ENDPOINT_URL/DELTA_TXN_TEST_S3_BUCKET not set \
                     (expected outside CI's s3-integration-tests job)",
                    $label
                );
                return;
            }
        }
    };
}

async fn start_server(storage_opts: HashMap<String, String>) -> common::TestServer {
    common::TestServer::start(TestServerConfig {
        storage_opts,
        ..Default::default()
    })
    .await
}

#[tokio::test]
async fn commit_creates_a_new_table_on_s3_and_get_table_reads_it_back() {
    let (storage_opts, table_uri) = skip_unless_s3_configured!("create-and-read");
    let server = start_server(storage_opts).await;
    let mut client = server.connect().await;

    let commit_response = client
        .commit(commit_request(
            &table_uri,
            None,
            create_table_actions("orders"),
        ))
        .await
        .expect("Commit that creates a new table on S3 should succeed")
        .into_inner();
    assert_eq!(commit_response.committed_version, 0);

    let get_table_response = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            ..Default::default()
        })
        .await
        .expect("GetTable against S3 should succeed")
        .into_inner();
    assert_eq!(get_table_response.version, 0);
    assert_eq!(
        get_table_response.metadata.expect("expected metadata").name,
        "orders"
    );
}

#[tokio::test]
async fn commit_append_and_version_conflict_detection_work_against_s3() {
    let (storage_opts, table_uri) = skip_unless_s3_configured!("version-conflict");
    let server = start_server(storage_opts).await;
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
async fn list_active_files_round_trips_against_s3() {
    let (storage_opts, table_uri) = skip_unless_s3_configured!("list-active-files");
    let server = start_server(storage_opts).await;
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
            ..Default::default()
        })
        .await
        .expect("ListActiveFiles against S3 should succeed")
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

/// The case the per-table in-process lock can't help with: two replicas
/// (two servers, two independent lock managers) racing on one table. Only
/// the storage backend's conditional put keeps commits from overwriting
/// each other, and a writer that loses at the storage level must get
/// ABORTED (retryable), not INTERNAL.
#[tokio::test]
async fn concurrent_writers_across_two_replicas_lose_no_updates_on_s3() {
    const WRITERS_PER_REPLICA: usize = 5;
    let (storage_opts, table_uri) = skip_unless_s3_configured!("two-replicas");
    let replica_a = start_server(storage_opts.clone()).await;
    let replica_b = start_server(storage_opts).await;
    let mut client_a = replica_a.connect().await;
    let client_b = replica_b.connect().await;

    client_a
        .commit(commit_request(
            &table_uri,
            None,
            create_table_actions("orders"),
        ))
        .await
        .expect("create commit should succeed");

    let mut handles = Vec::new();
    for i in 0..WRITERS_PER_REPLICA * 2 {
        let mut client = if i % 2 == 0 {
            client_a.clone()
        } else {
            client_b.clone()
        };
        let table_uri = table_uri.clone();
        handles.push(tokio::spawn(async move {
            let path = format!("writer-{i}.parquet");
            for _attempt in 0..50 {
                let version = client
                    .get_table(pb::GetTableRequest {
                        table_uri: table_uri.clone(),
                        ..Default::default()
                    })
                    .await
                    .expect("GetTable should succeed")
                    .into_inner()
                    .version;
                match client
                    .commit(commit_request(
                        &table_uri,
                        Some(version),
                        vec![add_file_action(&path, 1)],
                    ))
                    .await
                {
                    Ok(_) => return,
                    Err(status) if status.code() == Code::Aborted => continue,
                    Err(status) => panic!("{path}: non-retryable error {status:?}"),
                }
            }
            panic!("{path}: gave up after 50 attempts");
        }));
    }
    for handle in handles {
        handle.await.expect("writer task panicked");
    }

    let final_version = client_a
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .version;
    assert_eq!(final_version, (WRITERS_PER_REPLICA * 2) as i64);

    let mut stream = client_a
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut paths = HashSet::new();
    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(pb::list_active_files_response::Payload::Batch(batch)) = msg.payload {
            paths.extend(batch.files.into_iter().map(|f| f.path));
        }
    }
    assert_eq!(paths.len(), WRITERS_PER_REPLICA * 2, "{paths:?}");
}

/// The snapshot cache's commit-file check goes through the table-scoped
/// object store, whose rooting differs per backend; make sure lookups
/// actually hit (and stay current) on S3, not just on file://.
#[tokio::test]
async fn snapshot_cache_hits_and_stays_current_on_s3() {
    let (storage_opts, table_uri) = skip_unless_s3_configured!("snapshot-cache");
    let server = start_server(storage_opts.clone()).await;
    let mut client = server.connect().await;
    client
        .commit(commit_request(
            &table_uri,
            None,
            create_table_actions("orders"),
        ))
        .await
        .expect("create commit should succeed");

    let url = url::Url::parse(&table_uri).unwrap();
    let cache = SnapshotCache::new(4, std::time::Duration::from_secs(300));
    let (_, first) = cache
        .latest_with_result(&url, storage_opts.clone())
        .await
        .unwrap();
    assert_eq!(first, LookupResult::Miss);

    client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![add_file_action("a.parquet", 1)],
        ))
        .await
        .expect("append should succeed");
    let (table, second) = cache.latest_with_result(&url, storage_opts).await.unwrap();
    assert_eq!((table.version(), second), (Some(1), LookupResult::Hit));
}
