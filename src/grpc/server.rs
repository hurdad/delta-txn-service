//! The DeltaTxnService gRPC implementation itself: GetTable (unary table
//! inspection), Commit (unary, optimistic-concurrency-checked writes), and
//! ListActiveFiles (server-streaming active-file listing). See each
//! method's own doc comment for the specifics; this file's shared state
//! (DeltaTxnGrpcServer) and helpers (build_metadata_and_protocol,
//! map_open_or_snapshot_error) are what all three have in common.

use std::collections::HashMap;
use std::pin::Pin;

use deltalake::kernel::scalars::ScalarExt;
use deltalake::table::state::DeltaTableState;
use deltalake::{ensure_table_uri, DeltaTableError};
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{Request, Response, Status};
use tracing::warn;
use url::Url;

use crate::config::storage::{
    is_table_uri_allowed, load_allowed_table_prefixes, load_storage_options,
};
use crate::delta::errors::DeltaTxnError;
use crate::delta::{
    cache::{snapshot_cache_from_env, SnapshotCache},
    commit::{commit_actions, create_table, find_metadata, find_protocol},
    features,
    table::{open_table_at, table_exists, ReadAt},
};
use crate::grpc::mapping::{map_actions, map_active_file_to_pb, map_protocol_to_pb};
use crate::locking::table_lock::TableLockManager;
use crate::telemetry::commit_metrics::{CommitMetrics, CommitOutcome};

// How many files each ListActiveFilesBatch message carries. Large enough
// that per-message gRPC framing overhead is negligible even for a
// million-file table, small enough that memory use per in-flight message
// stays modest and a client starts seeing files promptly instead of
// waiting for one giant batch.
const FILE_BATCH_SIZE: usize = 1000;

// Backpressure-bounded: the sender task in list_active_files() only gets
// this far ahead of whatever the client has actually consumed off the
// stream.
const STREAM_CHANNEL_CAPACITY: usize = 4;

/// Default cap on actions in one Commit (DELTA_TXN_MAX_ACTIONS_PER_COMMIT).
/// A commit's actions are all held in memory, mapped, and written while
/// the per-table lock is held, so an unbounded list lets one client stall
/// every other writer to that table. 100k covers a large OPTIMIZE.
pub const DEFAULT_MAX_ACTIONS_PER_COMMIT: usize = 100_000;

type ListActiveFilesResultStream =
    Pin<Box<dyn Stream<Item = Result<ListActiveFilesResponse, Status>> + Send>>;

// The tonic-prost-build-generated protobuf/gRPC types (request/response
// messages, the DeltaTxnService server trait, etc.) -- see build.rs for
// where this actually gets compiled from proto/delta_txn.proto.
// `pub` so grpc::mapping and main.rs can name these types too, without
// each needing to `include_proto!` its own (ODR-duplicate) copy.
pub mod pb {
    tonic::include_proto!("delta.txn.v1");
}

use pb::delta_txn_service_server::{DeltaTxnService, DeltaTxnServiceServer};
use pb::*;

/// The DeltaTxnService implementation. Cheap to clone (all three fields
/// are either already-Arc'd (TableLockManager) or small/immutable-after-
/// construction), which matters because tonic clones the service per
/// connection/request as needed.
#[derive(Clone)]
pub struct DeltaTxnGrpcServer {
    locks: TableLockManager,
    /// Loaded once at startup (see config::storage::load_storage_options)
    /// and cloned per-call into open_table() -- see that function's own
    /// signature. Not re-read from the environment after construction;
    /// changing AWS_* env vars at runtime has no effect until restart.
    storage_opts: HashMap<String, String>,
    allowed_table_prefixes: Option<Vec<String>>,
    max_actions_per_commit: usize,
    metrics: CommitMetrics,
    /// Loaded tables per table_uri, caught up incrementally on each
    /// latest-version access (see delta::cache). Disabled by default in
    /// with_config; new() enables it from the environment.
    snapshots: SnapshotCache,
}

impl DeltaTxnGrpcServer {
    /// Reads DELTA_TXN_ALLOWED_TABLE_PREFIXES/AWS_* from the environment
    /// once (see config::storage) -- called exactly once in main.rs.
    // No Default impl on purpose: a Default that silently reads env vars
    // would be surprising. Use with_config for explicit configuration.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let allowed_table_prefixes = load_allowed_table_prefixes();
        if allowed_table_prefixes.is_none() {
            warn!(
                "DELTA_TXN_ALLOWED_TABLE_PREFIXES is not set: this service will open any \
                 table_uri supplied by a client. Set it to restrict which tables can be accessed."
            );
        }

        let max_actions_per_commit = std::env::var("DELTA_TXN_MAX_ACTIONS_PER_COMMIT")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_ACTIONS_PER_COMMIT);

        let snapshots = snapshot_cache_from_env(|k| std::env::var(k).ok())
            .with_metrics(&opentelemetry::global::meter("delta-txn-service"));

        Self::with_config(load_storage_options(), allowed_table_prefixes)
            .with_max_actions_per_commit(max_actions_per_commit)
            .with_snapshot_cache(snapshots)
    }

    /// Uses `cache` for latest-version table loads (Commit, GetTable,
    /// ListActiveFiles). Time-travel reads always load directly.
    pub fn with_snapshot_cache(mut self, cache: SnapshotCache) -> Self {
        self.snapshots = cache;
        self
    }

    /// Overrides DEFAULT_MAX_ACTIONS_PER_COMMIT.
    pub fn with_max_actions_per_commit(mut self, max: usize) -> Self {
        self.max_actions_per_commit = max.max(1);
        self
    }

    /// Whether a table_uri allowlist is configured -- main.rs's startup
    /// security check needs this (see config::grpc::check_security).
    pub fn has_allowlist(&self) -> bool {
        self.allowed_table_prefixes.is_some()
    }

    /// Constructs a server from already-resolved configuration rather than
    /// reading it from the environment -- what `new()` itself delegates to
    /// after doing that env read. Exists for the integration test suite
    /// (tests/): mutating process-global env vars (e.g.
    /// DELTA_TXN_ALLOWED_TABLE_PREFIXES) per test is fragile under Rust's
    /// default parallel test execution, since tests share one process; this
    /// gives tests an explicit config seam instead. No startup warning here
    /// (unlike `new()`) since a test passing `None` on purpose isn't an
    /// operator misconfiguration to flag.
    pub fn with_config(
        storage_opts: HashMap<String, String>,
        allowed_table_prefixes: Option<Vec<String>>,
    ) -> Self {
        Self {
            locks: TableLockManager::default(),
            storage_opts,
            allowed_table_prefixes,
            max_actions_per_commit: DEFAULT_MAX_ACTIONS_PER_COMMIT,
            snapshots: SnapshotCache::disabled(),
            // The global meter is a no-op until init_tracing installs an
            // OTLP meter provider, which main.rs does before this runs.
            metrics: CommitMetrics::new(&opentelemetry::global::meter("delta-txn-service")),
        }
    }

    /// The body of Commit; returns the response and whether this commit
    /// created the table (for CommitMetrics' outcome).
    async fn commit_inner(
        &self,
        req: Request<CommitRequest>,
    ) -> Result<(CommitResponse, bool), Status> {
        let r = req.into_inner();
        let table_uri = r.table_uri;

        let normalized_table_uri = self.normalize_and_check_table_uri(&table_uri)?;

        // Validated before the lock is taken or the table is opened: a
        // malformed action list (e.g. an unspecified data_change) is a
        // pure client-input error that doesn't need either a network round
        // trip to storage or the per-table lock held while it's rejected.
        if r.actions.is_empty() {
            return Err(Status::invalid_argument("actions must not be empty"));
        }
        if r.actions.len() > self.max_actions_per_commit {
            return Err(Status::invalid_argument(format!(
                "{} actions exceeds the limit of {} per commit (DELTA_TXN_MAX_ACTIONS_PER_COMMIT)",
                r.actions.len(),
                self.max_actions_per_commit
            )));
        }
        let actions = map_actions(r.actions).map_err(Status::invalid_argument)?;
        // A Protocol action (creating a table, or upgrading one) must be
        // one this service can keep writing to; see delta::features.
        if let Some(protocol) = find_protocol(&actions) {
            features::check_writable(protocol).map_err(Status::from)?;
        }

        // Held across the whole exists-check -> open-table -> version-check
        // -> commit sequence below, not just the commit call itself -- see
        // TableLockManager's own doc comment for why the version check has
        // to be inside the locked section too (otherwise two concurrent
        // commits could both read the same "current" version and both
        // pass their own expected_version check before either writes). The
        // exists-check has to be in here for the same reason: otherwise
        // two concurrent Commits to the same brand-new table_uri could
        // both see "doesn't exist yet" and both attempt to create it.
        let lock = self.locks.lock_for(normalized_table_uri.as_str());
        let wait_start = std::time::Instant::now();
        let _guard = lock.lock().await;
        self.metrics
            .record_lock_wait(wait_start.elapsed().as_secs_f64() * 1000.0);

        // table_exists() first, not a bare open_table(): open_table (via
        // delta-rs's own open_table_with_storage_options) fails outright
        // for a table_uri with no _delta_log at all, indistinguishable at
        // that point from a genuine storage error -- see table_exists's
        // own doc comment. A brand-new table_uri isn't a failure here,
        // it's this RPC's other job: create the table from `actions`'
        // own Protocol/Metadata, exactly like the very first commit any
        // Delta writer makes to a location nothing has written to yet.
        if !table_exists(&normalized_table_uri, self.storage_opts.clone())
            .await
            .map_err(Status::from)?
        {
            if r.expected_version.is_some() {
                return Err(Status::failed_precondition(
                    "expected_version was set but table_uri does not exist yet -- omit \
                     expected_version on the Commit that creates a new table",
                ));
            }
            // The single shared scan (find_protocol/find_metadata, also
            // used by create_table below) rather than a separate
            // has_protocol/has_metadata pass here plus create_table
            // re-deriving the same values itself -- one definition of
            // "does this action list have a Protocol/Metadata" for both
            // to agree on, and the values extracted here are handed
            // straight to create_table instead of it re-scanning.
            let protocol = find_protocol(&actions).cloned();
            let metadata = find_metadata(&actions).cloned();
            let (Some(protocol), Some(metadata)) = (protocol, metadata) else {
                return Err(Status::failed_precondition(
                    "table_uri does not exist yet -- the Commit that creates a new table must \
                     include both a Protocol and a TableMetadata action",
                ));
            };

            features::check_domain_metadata(&protocol, &actions).map_err(Status::from)?;

            let version = create_table(
                normalized_table_uri,
                self.storage_opts.clone(),
                actions,
                protocol,
                metadata,
            )
            .await
            .map_err(Status::from)?;

            return Ok((
                CommitResponse {
                    committed_version: version,
                },
                true,
            ));
        }

        // Caught up to the latest version, so expected_version and the
        // txn idempotency check below see the same state a fresh load would.
        let table = self
            .snapshots
            .latest(&normalized_table_uri, self.storage_opts.clone())
            .await
            .map_err(Status::from)?;

        {
            // The protocol after this commit: its own Protocol action if it
            // has one, otherwise the table's current one.
            let current = table.snapshot().map_err(map_open_or_snapshot_error)?;
            let protocol = find_protocol(&actions).unwrap_or_else(|| current.protocol());
            features::check_domain_metadata(protocol, &actions).map_err(Status::from)?;
        }

        if let Some(expected) = r.expected_version {
            // delta-rs's DeltaTable::version() returns u64; expected/actual
            // (and the wire contract, delta_txn.proto) are int64 -- see the
            // matching comment on snapshot.version() below.
            let current = table
                .version()
                .ok_or_else(|| Status::failed_precondition("table not initialized"))?
                as i64;
            if current != expected {
                return Err(Status::from(DeltaTxnError::VersionConflict {
                    expected,
                    actual: current,
                }));
            }
        }

        // Server-side idempotency for SetTransaction: inside the lock, so
        // two concurrent retries of the same (app_id, version) can't both
        // pass. Without this a client retrying a Commit whose outcome it
        // never saw (a timeout) would commit the same files twice.
        for action in &actions {
            if let deltalake::kernel::Action::Txn(txn) = action {
                let committed = table
                    .snapshot()
                    .map_err(map_open_or_snapshot_error)?
                    .transaction_version(table.log_store().as_ref(), &txn.app_id)
                    .await
                    .map_err(|e| {
                        tracing::error!(error = %e, "failed to read app transaction version");
                        Status::internal("internal error reading app transaction version")
                    })?;
                if let Some(committed) = committed {
                    if committed >= txn.version {
                        return Err(Status::from(DeltaTxnError::TransactionAlreadyCommitted {
                            app_id: txn.app_id.clone(),
                            committed,
                        }));
                    }
                }
            }
        }

        let version = commit_actions(table, actions).await.map_err(Status::from)?;

        Ok((
            CommitResponse {
                committed_version: version,
            },
            false,
        ))
    }

    pub fn into_service(self) -> DeltaTxnServiceServer<Self> {
        DeltaTxnServiceServer::new(self)
    }

    /// Checked at the top of every handler that takes a table_uri, before
    /// any I/O -- see config::storage::is_table_uri_allowed's own doc
    /// comment for the exact (character-prefix, not path-segment) matching
    /// rule and its operator-facing footgun.
    fn check_table_uri_allowed(&self, table_uri: &str) -> Result<(), Status> {
        if is_table_uri_allowed(table_uri, &self.allowed_table_prefixes) {
            Ok(())
        } else {
            Err(Status::permission_denied(format!(
                "table_uri '{table_uri}' is not in the configured allowlist"
            )))
        }
    }

    /// Allowlist-checks and normalizes a client-supplied table_uri --
    /// shared by all three handlers, which otherwise each needed the exact
    /// same three-step sequence.
    ///
    /// The raw, client-supplied string is checked first, but only when it
    /// already carries an explicit scheme (`"://"`): `ensure_table_uri()`
    /// below creates a local directory as a side effect for a bare/
    /// relative local path (delta-rs's own documented behavior for
    /// `ensure_table_uri`), unconditionally, even for a table_uri that's
    /// about to be rejected by the allowlist -- checking a scheme'd URI
    /// raw here means a disallowed absolute URI never reaches
    /// `ensure_table_uri()` at all, so no directory gets created for a
    /// request that was never going to be allowed to use it. A *relative*
    /// path can't be evaluated correctly before normalization, though:
    /// `ensure_table_uri()` rewrites it into an absolute `file://` URI,
    /// which is the only form `is_table_uri_allowed` can match against a
    /// configured `file://` prefix -- checking the bare relative string
    /// here would reject some allowlisted relative paths outright before
    /// they ever got the chance to normalize into an allowed form. So this
    /// raw check is skipped for anything without a scheme, and the
    /// post-normalization check right after remains the actual source of
    /// truth either way, since normalization can change which configured
    /// prefix a URI matches (trailing slashes, relative paths) in ways
    /// this raw check can't account for.
    fn normalize_and_check_table_uri(&self, table_uri: &str) -> Result<Url, Status> {
        if table_uri.contains("://") {
            self.check_table_uri_allowed(table_uri)?;
        }

        let normalized_table_uri =
            ensure_table_uri(table_uri).map_err(|e| Status::invalid_argument(e.to_string()))?;

        self.check_table_uri_allowed(normalized_table_uri.as_str())?;

        Ok(normalized_table_uri)
    }
}

/// Latest-version reads go through the snapshot cache; time-travel reads
/// load the requested version directly.
async fn load_for_read(
    snapshots: &SnapshotCache,
    table_uri: &Url,
    storage_opts: HashMap<String, String>,
    at: ReadAt,
) -> Result<deltalake::DeltaTable, DeltaTxnError> {
    match at {
        ReadAt::Latest => snapshots.latest(table_uri, storage_opts).await,
        _ => open_table_at(table_uri, storage_opts, at).await,
    }
}

/// Turns a read request's optional time-travel fields into a ReadAt.
/// Setting both is ambiguous, and negative values can't name a version or
/// a representable instant, so all of those are INVALID_ARGUMENT.
fn parse_read_at(version: Option<i64>, timestamp_ms: Option<i64>) -> Result<ReadAt, Status> {
    match (version, timestamp_ms) {
        (Some(_), Some(_)) => Err(Status::invalid_argument(
            "set at most one of version and timestamp_ms",
        )),
        (Some(v), None) => u64::try_from(v)
            .map(ReadAt::Version)
            .map_err(|_| Status::invalid_argument("version must be >= 0")),
        (None, Some(ms)) => chrono::DateTime::from_timestamp_millis(ms)
            .filter(|_| ms >= 0)
            .map(ReadAt::Timestamp)
            .ok_or_else(|| Status::invalid_argument("timestamp_ms must be a valid time >= 0")),
        (None, None) => Ok(ReadAt::Latest),
    }
}

// Shared by get_table() and list_active_files()'s header: converts a
// loaded snapshot's metadata/protocol into their wire form, including the
// schema_string parse-and-reserialize step both need identically.
fn build_metadata_and_protocol(
    snapshot: &DeltaTableState,
) -> Result<(TableMetadata, Protocol), Status> {
    let metadata = snapshot.metadata();
    let protocol = snapshot.protocol();

    let schema_string = metadata
        .parse_schema()
        .map_err(|e| {
            tracing::error!(error = %e, "failed to parse table schema");
            Status::internal("internal error parsing table schema")
        })
        .and_then(|schema| {
            serde_json::to_string(&schema).map_err(|e| {
                tracing::error!(error = %e, "failed to serialize table schema");
                Status::internal("internal error serializing table schema")
            })
        })?;

    Ok((
        TableMetadata {
            id: metadata.id().to_string(),
            name: metadata.name().unwrap_or_default().to_string(),
            description: metadata.description().unwrap_or_default().to_string(),
            schema_string,
            partition_columns: metadata.partition_columns().to_vec(),
            configuration: metadata.configuration().clone(),
            created_time: metadata.created_time().unwrap_or_default(),
        },
        map_protocol_to_pb(protocol),
    ))
}

// The "table_uri doesn't exist at all" case is caught earlier by every
// caller's own table_exists() check (returning Status::not_found before
// this is ever reached) -- NotInitialized is left handled here defensively
// for whatever narrower "log exists but couldn't be loaded into a usable
// snapshot" state delta-rs might still produce that isn't captured by
// table_exists's own is_delta_table_location() probe, not because it's the
// primary path anymore.
fn map_open_or_snapshot_error(e: DeltaTableError) -> Status {
    match e {
        DeltaTableError::NotInitialized => Status::failed_precondition("table not initialized"),
        _ => {
            tracing::error!(error = %e, "failed to load table snapshot");
            Status::internal("internal error loading table snapshot")
        }
    }
}

// The actual open-table/build-header/stream-files work for
// list_active_files(), run inside the spawned task (see that method's own
// comment on why): every failure here becomes the one `Err` item sent
// before the stream ends, since by the time this runs the RPC has already
// returned `Ok(Response::new(stream))` to the client.
async fn stream_active_files(
    table_uri: Url,
    storage_opts: HashMap<String, String>,
    snapshots: SnapshotCache,
    at: ReadAt,
    tx: tokio::sync::mpsc::Sender<Result<ListActiveFilesResponse, Status>>,
) {
    let result = stream_active_files_inner(table_uri, storage_opts, &snapshots, at, &tx).await;
    if let Err(status) = result {
        let _ = tx.send(Err(status)).await;
    }
}

async fn stream_active_files_inner(
    table_uri: Url,
    storage_opts: HashMap<String, String>,
    snapshots: &SnapshotCache,
    at: ReadAt,
    tx: &tokio::sync::mpsc::Sender<Result<ListActiveFilesResponse, Status>>,
) -> Result<(), Status> {
    if !table_exists(&table_uri, storage_opts.clone())
        .await
        .map_err(Status::from)?
    {
        return Err(Status::not_found(format!(
            "table_uri '{table_uri}' does not exist"
        )));
    }

    let table = load_for_read(snapshots, &table_uri, storage_opts, at)
        .await
        .map_err(Status::from)?;

    let snapshot = table.snapshot().map_err(map_open_or_snapshot_error)?;
    let (metadata, protocol) = build_metadata_and_protocol(snapshot)?;

    let header = ListActiveFilesResponse {
        payload: Some(list_active_files_response::Payload::Header(
            ListActiveFilesHeader {
                // delta-rs's Snapshot::version() returns u64; the wire
                // contract (delta_txn.proto) uses int64 throughout, and a
                // real table version never approaches i64::MAX.
                version: snapshot.version() as i64,
                metadata: Some(metadata),
                protocol: Some(protocol),
            },
        )),
    };
    if tx.send(Ok(header)).await.is_err() {
        return Ok(()); // client already gone; nothing left to report.
    }

    let log_store = table.log_store();
    let eager_snapshot = snapshot.snapshot();
    let mut file_stream = eager_snapshot.file_views(log_store.as_ref(), None);

    let mut batch: Vec<AddFile> = Vec::with_capacity(FILE_BATCH_SIZE);
    while let Some(file_result) = file_stream.next().await {
        let file_view = file_result.map_err(|e| {
            tracing::error!(error = %e, "failed while streaming active files");
            Status::internal("internal error streaming active files")
        })?;

        // LogicalFileView::partition_values() -> Option<StructData> has no
        // convenient accessor of its own (deltalake-core's maintainers say
        // as much in their own internal StructDataExt doc comment) --
        // walking .fields()/.values() in lockstep and serializing each
        // non-null Scalar is exactly what deltalake-core's own (private)
        // LogicalFileView::partition_values_map() helper does internally;
        // replicated here since that helper isn't public.
        let partition_values = file_view
            .partition_values()
            .map(|data| {
                data.fields()
                    .iter()
                    .zip(data.values().iter())
                    .map(|(field, value)| {
                        (
                            field.name().to_string(),
                            if value.is_null() {
                                None
                            } else {
                                Some(value.serialize())
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        batch.push(map_active_file_to_pb(
            file_view.path().to_string(),
            file_view.size(),
            file_view.modification_time(),
            partition_values,
            file_view.stats(),
            file_view.deletion_vector_descriptor(),
        ));

        if batch.len() >= FILE_BATCH_SIZE {
            let message = ListActiveFilesResponse {
                payload: Some(list_active_files_response::Payload::Batch(
                    ListActiveFilesBatch {
                        files: std::mem::take(&mut batch),
                    },
                )),
            };
            if tx.send(Ok(message)).await.is_err() {
                return Ok(());
            }
        }
    }
    if !batch.is_empty() {
        let message = ListActiveFilesResponse {
            payload: Some(list_active_files_response::Payload::Batch(
                ListActiveFilesBatch { files: batch },
            )),
        };
        let _ = tx.send(Ok(message)).await;
    }

    Ok(())
}

#[tonic::async_trait]
impl DeltaTxnService for DeltaTxnGrpcServer {
    /// Atomically applies `req.actions` to the table at `req.table_uri`,
    /// serialized against every other concurrent Commit for the *same*
    /// table_uri by TableLockManager (see that type's own doc comment --
    /// an optimization, not a correctness requirement) and, once past
    /// that, checked against `expected_version` (this service's own
    /// pre-commit optimistic-concurrency check) before delta-rs's
    /// CommitBuilder (see delta::commit::commit_actions, including its
    /// own doc comment on the operation-type limitation) does the actual
    /// atomic write with its own independent conflict retry.
    async fn commit(
        &self,
        req: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        let start = std::time::Instant::now();
        let result = self.commit_inner(req).await;
        let outcome = match &result {
            Ok((_, true)) => CommitOutcome::Created,
            Ok((_, false)) => CommitOutcome::Committed,
            Err(status) => CommitOutcome::from_code(status.code()),
        };
        self.metrics
            .record_commit(outcome, start.elapsed().as_secs_f64() * 1000.0);
        result.map(|(response, _)| Response::new(response))
    }

    /// Returns a table's current version, metadata (including its schema),
    /// and protocol -- no file listing (see list_active_files() for that).
    /// A plain read: opens the table fresh, takes no lock (Delta readers
    /// never need to block on writers -- see locking::mod's own doc
    /// comment), and reflects whatever snapshot happens to be visible at
    /// the moment it's called.
    async fn get_table(
        &self,
        req: Request<GetTableRequest>,
    ) -> Result<Response<GetTableResponse>, Status> {
        let r = req.into_inner();
        let table_uri = r.table_uri;

        let normalized_table_uri = self.normalize_and_check_table_uri(&table_uri)?;
        let at = parse_read_at(r.version, r.timestamp_ms)?;
        if r.app_id.as_deref() == Some("") {
            return Err(Status::invalid_argument(
                "app_id must not be empty when set",
            ));
        }

        if !table_exists(&normalized_table_uri, self.storage_opts.clone())
            .await
            .map_err(Status::from)?
        {
            return Err(Status::not_found(format!(
                "table_uri '{normalized_table_uri}' does not exist"
            )));
        }

        let table = load_for_read(
            &self.snapshots,
            &normalized_table_uri,
            self.storage_opts.clone(),
            at,
        )
        .await
        .map_err(Status::from)?;

        let snapshot = table.snapshot().map_err(map_open_or_snapshot_error)?;
        let (metadata, protocol) = build_metadata_and_protocol(snapshot)?;

        let app_transaction_version = match r.app_id {
            Some(app_id) => snapshot
                .transaction_version(table.log_store().as_ref(), app_id)
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "failed to read app transaction version");
                    Status::internal("internal error reading app transaction version")
                })?,
            None => None,
        };

        Ok(Response::new(GetTableResponse {
            // See stream_active_files_inner's matching cast comment above.
            version: snapshot.version() as i64,
            metadata: Some(metadata),
            protocol: Some(protocol),
            app_transaction_version,
        }))
    }

    type ListActiveFilesStream = ListActiveFilesResultStream;

    // Server-streaming: table_uri validation happens synchronously here
    // (so a bad request still fails the RPC call itself, same as every
    // other handler), but the actual table open + snapshot + file
    // enumeration all happen inside a spawned task instead of before this
    // function returns. That's a real ownership constraint, not just
    // style: EagerSnapshot::file_views()'s returned stream borrows from
    // the snapshot, which borrows from the DeltaTable -- none of which can
    // be moved across the spawn boundary as a borrow, so the whole
    // open-through-stream sequence has to live inside one async block that
    // owns the DeltaTable itself. See stream_active_files_inner() for
    // where any failure in that sequence actually surfaces (as the
    // stream's one Err item, not this function's own Result).
    async fn list_active_files(
        &self,
        req: Request<ListActiveFilesRequest>,
    ) -> Result<Response<Self::ListActiveFilesStream>, Status> {
        let r = req.into_inner();
        let table_uri = r.table_uri;

        let normalized_table_uri = self.normalize_and_check_table_uri(&table_uri)?;
        let at = parse_read_at(r.version, r.timestamp_ms)?;

        let storage_opts = self.storage_opts.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_CHANNEL_CAPACITY);

        tokio::spawn(stream_active_files(
            normalized_table_uri,
            storage_opts,
            self.snapshots.clone(),
            at,
            tx,
        ));

        let stream: Self::ListActiveFilesStream = Box::pin(ReceiverStream::new(rx));
        Ok(Response::new(stream))
    }
}
