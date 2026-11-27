//! Snapshot cache benchmark: request latency with the cache on vs. off,
//! through the real gRPC server, on the same tables. Ignored by default;
//! run in release mode:
//!
//!     cargo test --release --test bench_snapshot_cache -- --ignored --nocapture
//!
//! Uses a file:// tempdir unless AWS_ENDPOINT_URL and
//! DELTA_TXN_TEST_S3_BUCKET are set (same variables as e2e_s3.rs), in
//! which case it runs against that S3 endpoint. Optional:
//! DELTA_TXN_BENCH_ITERS (default 30), DELTA_TXN_BENCH_ONLY (run only the
//! table shapes whose name contains this).

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::{
    add_file_action, commit_request, create_table_actions, pb, TestServer, TestServerConfig,
};
use delta_txn_service::delta::cache::SnapshotCache;

/// A table shape: `commits` appends of `files_per_commit` files each,
/// after the create commit. Delta checkpoints every 100 versions, so the
/// number of JSON commits after the last checkpoint is `commits % 100`.
struct Shape {
    name: &'static str,
    commits: usize,
    files_per_commit: usize,
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "small, short tail",
        commits: 5,
        files_per_commit: 10,
    },
    Shape {
        name: "small, 99-commit tail",
        commits: 99,
        files_per_commit: 10,
    },
    Shape {
        name: "checkpointed + 50 tail",
        commits: 150,
        files_per_commit: 10,
    },
    Shape {
        name: "100k files, 20 commits",
        commits: 20,
        files_per_commit: 5_000,
    },
];

fn storage() -> (
    HashMap<String, String>,
    Option<String>,
    Option<tempfile::TempDir>,
) {
    match (
        std::env::var("AWS_ENDPOINT_URL").ok(),
        std::env::var("DELTA_TXN_TEST_S3_BUCKET").ok(),
    ) {
        (Some(endpoint), Some(bucket)) => {
            let mut opts = HashMap::new();
            opts.insert("AWS_ENDPOINT_URL".into(), endpoint);
            for key in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"] {
                if let Ok(v) = std::env::var(key) {
                    opts.insert(key.into(), v);
                }
            }
            opts.insert("AWS_ALLOW_HTTP".into(), "true".into());
            let run = std::process::id();
            (opts, Some(format!("s3://{bucket}/bench-{run}")), None)
        }
        _ => {
            let dir = tempfile::tempdir().unwrap();
            let base = format!("file://{}", dir.path().display());
            (HashMap::new(), Some(base), Some(dir))
        }
    }
}

struct Stats {
    p50: Duration,
    p95: Duration,
}

fn stats(mut samples: Vec<Duration>) -> Stats {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    Stats {
        p50: at(0.5),
        p95: at(0.95),
    }
}

fn ms(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1000.0)
}

async fn get_table(client: &mut common::Client, uri: &str) -> i64 {
    client
        .get_table(pb::GetTableRequest {
            table_uri: uri.to_string(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .version
}

async fn list_files(client: &mut common::Client, uri: &str) -> usize {
    let mut stream = client
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri: uri.to_string(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut n = 0;
    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(pb::list_active_files_response::Payload::Batch(b)) = msg.payload {
            n += b.files.len();
        }
    }
    n
}

async fn build_table(client: &mut common::Client, uri: &str, shape: &Shape) -> i64 {
    client
        .commit(commit_request(uri, None, create_table_actions("bench")))
        .await
        .unwrap();
    for c in 0..shape.commits {
        let actions = (0..shape.files_per_commit)
            .map(|f| add_file_action(&format!("c{c}/f{f}.parquet"), 100))
            .collect();
        client
            .commit(commit_request(uri, Some(c as i64), actions))
            .await
            .unwrap();
    }
    shape.commits as i64
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run with --release -- --ignored --nocapture"]
async fn snapshot_cache_benchmark() {
    let iters: usize = std::env::var("DELTA_TXN_BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    // Server warnings (e.g. a failed checkpoint) go to stderr.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
    let (storage_opts, base, _dir) = storage();
    let base = base.unwrap();
    let backend = if base.starts_with("s3://") {
        "S3"
    } else {
        "file://"
    };

    let start = |cache: SnapshotCache| {
        let storage_opts = storage_opts.clone();
        async move {
            TestServer::start(TestServerConfig {
                storage_opts,
                snapshot_cache: Some(cache),
                max_message_bytes: Some(64 * 1024 * 1024),
                ..Default::default()
            })
            .await
        }
    };
    let cached = start(SnapshotCache::new(32, Duration::from_secs(300))).await;
    let uncached = start(SnapshotCache::disabled()).await;
    let mut cached_client = cached.connect().await;
    let mut uncached_client = uncached.connect().await;

    println!("\nsnapshot cache benchmark ({backend}, {iters} iterations; ms, p50 / p95)\n");
    println!(
        "| table | files | GetTable off | GetTable on | ListActiveFiles off | ListActiveFiles on | Commit+GetTable off | Commit+GetTable on |"
    );
    println!("|---|---|---|---|---|---|---|---|");

    let only = std::env::var("DELTA_TXN_BENCH_ONLY").ok();
    for (i, shape) in SHAPES.iter().enumerate() {
        if only.as_deref().is_some_and(|o| !shape.name.contains(o)) {
            continue;
        }
        let uri = format!("{base}/t{i}");
        let mut version = build_table(&mut uncached_client, &uri, shape).await;
        let files = shape.commits * shape.files_per_commit;

        let mut row = vec![shape.name.to_string(), files.to_string()];

        // Read-only latency. One untimed warm-up each (fills the cache).
        let mut get = HashMap::new();
        let mut list = HashMap::new();
        for (label, client) in [("off", &mut uncached_client), ("on", &mut cached_client)] {
            assert_eq!(get_table(client, &uri).await, version);
            let mut samples = Vec::with_capacity(iters);
            for _ in 0..iters {
                let t = Instant::now();
                get_table(client, &uri).await;
                samples.push(t.elapsed());
            }
            get.insert(label, stats(samples));

            let list_iters = (iters / 3).max(5);
            let mut samples = Vec::with_capacity(list_iters);
            for _ in 0..list_iters {
                let t = Instant::now();
                assert_eq!(list_files(client, &uri).await, files);
                samples.push(t.elapsed());
            }
            list.insert(label, stats(samples));
        }

        // A writer and a reader on the same server: each iteration commits
        // one file, then reads the new version back. Both RPCs load the
        // table, so this is where catching up incrementally shows.
        let mut write_read = HashMap::new();
        for (label, client) in [("off", &mut uncached_client), ("on", &mut cached_client)] {
            let mut samples = Vec::with_capacity(iters);
            for n in 0..iters {
                let t = Instant::now();
                client
                    .commit(commit_request(
                        &uri,
                        Some(version),
                        vec![add_file_action(&format!("{label}/w{n}.parquet"), 1)],
                    ))
                    .await
                    .unwrap();
                version += 1;
                assert_eq!(get_table(client, &uri).await, version);
                samples.push(t.elapsed());
            }
            write_read.insert(label, stats(samples));
        }

        for map in [&get, &list, &write_read] {
            for label in ["off", "on"] {
                let s = &map[label];
                row.push(format!("{} / {}", ms(s.p50), ms(s.p95)));
            }
        }
        println!("| {} |", row.join(" | "));
    }
    println!();
}
