//! Shared setup for the gRPC integration test suite: spins up a real
//! `DeltaTxnGrpcServer` through `delta_txn_service::app::serve` -- the same
//! function main.rs calls, so layers, auth, health, TLS and limits are the
//! production wiring, not a copy of it -- bound to an OS-assigned
//! ephemeral localhost port, and hands out throwaway `file://` table_uris
//! under a fresh tempdir per server -- no S3 dependency, so this
//! suite runs anywhere `cargo test` does, including inside the Docker
//! build (see Dockerfile's own `RUN cargo test --release` step).
//!
//! Each file directly under tests/ is its own separate test binary
//! (Cargo's own convention); `mod common;` in each pulls this in without
//! this file itself being treated as a test target (only direct children
//! of tests/ are).
//!
//! Because this module is recompiled fresh into every one of those
//! binaries, each binary's dead-code pass only sees the subset of these
//! helpers *it* happens to call (e.g. `health_client` is used only by
//! `e2e_health.rs`) -- every other binary reports the rest as unused, even
//! though the suite as a whole uses all of them. Suppressed wholesale
//! rather than per-item since which helpers trip it depends on which test
//! binary is compiling, not on anything actually dead.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;

use delta_txn_service::app::{self, BoxError, ServeOptions};
use delta_txn_service::config::grpc::{parse_grpc_config, TlsConfig};
use delta_txn_service::delta::cache::{
    SnapshotCache, DEFAULT_SNAPSHOT_CACHE_SIZE, DEFAULT_SNAPSHOT_CACHE_TTL,
};
use delta_txn_service::grpc::auth::single_api_key;
// `pub use`, not `use`: test files reference these as `common::pb::...`
// rather than each needing their own `use delta_txn_service::grpc::...`.
pub use delta_txn_service::grpc::server::pb;
use delta_txn_service::grpc::server::pb::delta_txn_service_client::DeltaTxnServiceClient;
use delta_txn_service::grpc::server::DeltaTxnGrpcServer;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::transport::Channel;

/// The generated gRPC client type every test uses.
pub type Client = DeltaTxnServiceClient<Channel>;

/// What to configure the server under test with -- everything defaults to
/// "off" (no auth, no allowlist, no object-store credentials), matching an
/// operator who hasn't set any of the corresponding env vars. Individual
/// tests opt into `api_key`/`allowed_table_prefixes`/`storage_opts` only
/// when that's specifically what they're exercising.
#[derive(Default)]
pub struct TestServerConfig {
    pub api_key: Option<String>,
    pub allowed_table_prefixes: Option<Vec<String>>,
    /// Forwarded to `DeltaTxnGrpcServer::with_config` verbatim -- empty by
    /// default, which is all every `file://`-backed test (the overwhelming
    /// majority of this suite) needs. Only `tests/e2e_s3.rs` sets this,
    /// to supply `AWS_ENDPOINT_URL`/credentials for a real S3 backend.
    pub storage_opts: HashMap<String, String>,
    /// Overrides DEFAULT_MAX_ACTIONS_PER_COMMIT when set.
    pub max_actions_per_commit: Option<usize>,
    /// Serve over TLS (tests connect with their own TLS client, not
    /// `connect()`).
    pub tls: Option<TlsConfig>,
    /// Overrides DEFAULT_MAX_MESSAGE_BYTES when set.
    pub max_message_bytes: Option<usize>,
    /// Overrides DEFAULT_REQUEST_TIMEOUT when set.
    pub request_timeout: Option<Duration>,
    /// Enables the storage readiness probe against this endpoint.
    pub storage_probe_endpoint: Option<String>,
    pub storage_probe_interval: Option<Duration>,
    /// The snapshot cache to use; a clone stays with the test so it can
    /// inspect it. None gets a default-sized cache, as in production, so
    /// the whole suite runs through it.
    pub snapshot_cache: Option<SnapshotCache>,
}

/// A running `DeltaTxnGrpcServer` plus the tempdir its `file://` tables
/// live under. Keep this alive for as long as the test needs the server or
/// its tables to exist -- dropping it stops the server and deletes the
/// tempdir (see `Drop` impl below).
pub struct TestServer {
    addr: SocketAddr,
    tmp_dir: TempDir,
    server_task: tokio::task::JoinHandle<Result<(), BoxError>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // JoinHandle::drop alone only detaches, it doesn't stop the task --
        // each test's server would otherwise keep accepting connections on
        // its ephemeral port for the rest of the test binary process's
        // lifetime. Aborting is safe specifically because nothing outside
        // this process could depend on this server staying up: it's bound
        // to 127.0.0.1 on a port only this test process ever learned.
        self.server_task.abort();
    }
}

impl TestServer {
    /// Binds the listener and spawns the server *before* returning -- by
    /// the time `.await` on this resolves, the OS has already bound and is
    /// listening on `addr` (TcpListener::bind does that synchronously with
    /// respect to this function's own caller), so a client connecting the
    /// moment this returns will queue in the kernel's own accept backlog
    /// even if tonic's accept loop hasn't polled yet -- no separate
    /// "wait until ready" poll loop needed.
    pub async fn start(config: TestServerConfig) -> Self {
        let tmp_dir = tempfile::tempdir().expect("failed to create tempdir for test tables");

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test server listener");
        let addr = listener
            .local_addr()
            .expect("failed to read test server listener's bound address");

        let mut svc =
            DeltaTxnGrpcServer::with_config(config.storage_opts, config.allowed_table_prefixes);
        if let Some(max) = config.max_actions_per_commit {
            svc = svc.with_max_actions_per_commit(max);
        }
        svc = svc.with_snapshot_cache(config.snapshot_cache.unwrap_or_else(|| {
            SnapshotCache::new(DEFAULT_SNAPSHOT_CACHE_SIZE, DEFAULT_SNAPSHOT_CACHE_TTL)
        }));

        // Start from the same defaults an operator with no env vars set
        // gets, then apply this test's overrides.
        let mut grpc_config = parse_grpc_config(|_| None).expect("default config must parse");
        grpc_config.addr = addr;
        grpc_config.api_keys = single_api_key(config.api_key);
        grpc_config.tls = config.tls;
        if let Some(max) = config.max_message_bytes {
            grpc_config.max_message_bytes = max;
        }
        if let Some(timeout) = config.request_timeout {
            grpc_config.request_timeout = Some(timeout);
        }
        let options = ServeOptions {
            storage_endpoint: config.storage_probe_endpoint,
            storage_probe_interval: config
                .storage_probe_interval
                .unwrap_or(app::DEFAULT_STORAGE_PROBE_INTERVAL),
        };

        // serve() installs it too, but it runs on a spawned task; install
        // here so a TLS test client connecting right away has it already.
        app::install_crypto_provider();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_task = tokio::spawn(app::serve(
            grpc_config,
            svc,
            listener,
            options,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        Self {
            addr,
            tmp_dir,
            server_task,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Triggers graceful shutdown and returns what `app::serve` returned.
    pub async fn shutdown(mut self) -> Result<(), BoxError> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        (&mut self.server_task).await.expect("server task panicked")
    }

    /// Waits for `app::serve` to return on its own (e.g. a startup error).
    pub async fn join(mut self) -> Result<(), BoxError> {
        (&mut self.server_task).await.expect("server task panicked")
    }

    /// A fresh, never-yet-used `file://` table_uri under this server's own
    /// tempdir -- give each test (or each table within a test) its own
    /// label so concurrent tests never collide on the same Delta log.
    pub fn new_table_uri(&self, label: &str) -> String {
        format!("file://{}", self.tmp_dir.path().join(label).display())
    }

    /// A plain, unauthenticated client -- what most tests want. For a
    /// request that needs an API key, attach one to that one request via
    /// `with_api_key` below rather than standing up a second,
    /// interceptor-wrapped client type just for the handful of tests that
    /// exercise auth.
    pub async fn connect(&self) -> DeltaTxnServiceClient<Channel> {
        DeltaTxnServiceClient::connect(format!("http://{}", self.addr))
            .await
            .expect("failed to connect test gRPC client")
    }

    /// Unlike `DeltaTxnServiceClient` (generated fresh by this crate's own
    /// `build.rs`), `tonic_health::pb::health_client::HealthClient` is
    /// pre-built/checked-in code shipped by the `tonic-health` crate
    /// itself, and it has no `connect()` convenience associated function
    /// -- only `new`/`with_origin`/`with_interceptor`, all of which take an
    /// already-connected transport. So this builds the `Channel` by hand
    /// via `tonic::transport::Endpoint`, the same thing `connect()` would
    /// have done internally if it existed here.
    pub async fn health_client(&self) -> tonic_health::pb::health_client::HealthClient<Channel> {
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", self.addr))
            .expect("failed to build test health client endpoint")
            .connect()
            .await
            .expect("failed to connect test health client");
        tonic_health::pb::health_client::HealthClient::new(channel)
    }
}

/// Attaches an `x-api-key` metadata header to a request -- the test-side
/// equivalent of what a real client's own interceptor would do, without
/// needing a second generic client type just for the handful of tests
/// that exercise auth.
pub fn with_api_key<T>(mut req: tonic::Request<T>, api_key: &str) -> tonic::Request<T> {
    req.metadata_mut().insert(
        "x-api-key",
        api_key
            .parse()
            .expect("api key must be a valid metadata value"),
    );
    req
}

// ---------------------------------------------------------------------
// Request-payload builders: every field a test doesn't specifically care
// about gets a fixed, valid-looking default (id/name/schema below), so
// each test file only has to spell out what it's actually testing.
// ---------------------------------------------------------------------

pub fn sample_protocol() -> pb::Protocol {
    pb::Protocol {
        min_reader_version: 1,
        min_writer_version: 2,
        ..Default::default()
    }
}

fn sample_schema_string() -> String {
    serde_json::json!({
        "type": "struct",
        "fields": [
            {"name": "id", "type": "long", "nullable": false, "metadata": {}},
            {"name": "amount", "type": "double", "nullable": true, "metadata": {}}
        ]
    })
    .to_string()
}

pub fn sample_metadata(table_name: &str) -> pb::TableMetadata {
    pb::TableMetadata {
        id: format!("test-table-{table_name}"),
        name: table_name.to_string(),
        description: String::new(),
        schema_string: sample_schema_string(),
        partition_columns: Vec::new(),
        configuration: HashMap::new(),
        created_time: 0,
    }
}

/// The two actions (`Protocol` + `TableMetadata`) required to create a new
/// table -- see `grpc::server::commit()`'s own validation of exactly this
/// requirement.
pub fn create_table_actions(table_name: &str) -> Vec<pb::Action> {
    vec![
        pb::Action {
            action: Some(pb::action::Action::Protocol(sample_protocol())),
        },
        pb::Action {
            action: Some(pb::action::Action::MetaData(sample_metadata(table_name))),
        },
    ]
}

pub fn add_file_action(path: &str, num_records: i64) -> pb::Action {
    let mut columns = HashMap::new();
    columns.insert(
        "id".to_string(),
        pb::ColumnStats {
            min_value: Some(pb::column_stats::MinValue::MinInt(1)),
            max_value: Some(pb::column_stats::MaxValue::MaxInt(num_records)),
            null_count: 0,
        },
    );
    pb::Action {
        action: Some(pb::action::Action::Add(pb::AddFile {
            path: path.to_string(),
            size: 1024,
            modification_time: 1_700_000_000_000,
            partition_values: HashMap::new(),
            data_change: pb::DataChange::True as i32,
            stats: Some(pb::FileStats {
                num_records,
                columns,
            }),
            tags: HashMap::new(),
            deletion_vector: None,
        })),
    }
}

pub fn remove_file_action(path: &str) -> pb::Action {
    pb::Action {
        action: Some(pb::action::Action::Remove(pb::RemoveFile {
            path: path.to_string(),
            deletion_timestamp: Some(1_700_000_002_000),
            data_change: pb::DataChange::True as i32,
            ..Default::default()
        })),
    }
}

pub fn commit_request(
    table_uri: &str,
    expected_version: Option<i64>,
    actions: Vec<pb::Action>,
) -> pb::CommitRequest {
    pb::CommitRequest {
        table_uri: table_uri.to_string(),
        expected_version,
        actions,
        app_metadata: HashMap::new(),
    }
}
