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

use crate::config::storage::{
    is_table_uri_allowed, load_allowed_table_prefixes, load_storage_options,
};
use crate::delta::errors::DeltaTxnError;
use crate::delta::{commit::commit_actions, table::open_table};
use crate::grpc::mapping::{map_actions, map_active_file_to_pb};
use crate::locking::table_lock::TableLockManager;

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
}

impl DeltaTxnGrpcServer {
    /// Reads DELTA_TXN_ALLOWED_TABLE_PREFIXES/AWS_* from the environment
    /// once (see config::storage) -- called exactly once in main.rs.
    pub fn new() -> Self {
        let allowed_table_prefixes = load_allowed_table_prefixes();
        if allowed_table_prefixes.is_none() {
            warn!(
                "DELTA_TXN_ALLOWED_TABLE_PREFIXES is not set: this service will open any \
                 table_uri supplied by a client. Set it to restrict which tables can be accessed."
            );
        }

        Self {
            locks: TableLockManager::default(),
            storage_opts: load_storage_options(),
            allowed_table_prefixes,
        }
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
            partition_columns: metadata.partition_columns().clone(),
            configuration: metadata.configuration().clone(),
            created_time: metadata.created_time().unwrap_or_default(),
        },
        Protocol {
            min_reader_version: protocol.min_reader_version(),
            min_writer_version: protocol.min_writer_version(),
        },
    ))
}

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
    table_uri: String,
    storage_opts: HashMap<String, String>,
    tx: tokio::sync::mpsc::Sender<Result<ListActiveFilesResponse, Status>>,
) {
    let result = stream_active_files_inner(table_uri, storage_opts, &tx).await;
    if let Err(status) = result {
        let _ = tx.send(Err(status)).await;
    }
}

async fn stream_active_files_inner(
    table_uri: String,
    storage_opts: HashMap<String, String>,
    tx: &tokio::sync::mpsc::Sender<Result<ListActiveFilesResponse, Status>>,
) -> Result<(), Status> {
    let table = open_table(table_uri.as_str(), storage_opts)
        .await
        .map_err(Status::from)?;

    let snapshot = table.snapshot().map_err(map_open_or_snapshot_error)?;
    let (metadata, protocol) = build_metadata_and_protocol(snapshot)?;

    let header = ListActiveFilesResponse {
        payload: Some(list_active_files_response::Payload::Header(
            ListActiveFilesHeader {
                version: snapshot.version(),
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
        let r = req.into_inner();
        let table_uri = r.table_uri;

        let normalized_table_uri =
            ensure_table_uri(&table_uri).map_err(|e| Status::invalid_argument(e.to_string()))?;

        self.check_table_uri_allowed(normalized_table_uri.as_str())?;

        // Validated before the lock is taken or the table is opened: a
        // malformed action list (e.g. an unspecified data_change) is a
        // pure client-input error that doesn't need either a network round
        // trip to storage or the per-table lock held while it's rejected.
        let actions = map_actions(r.actions).map_err(|e| Status::invalid_argument(e))?;

        // Held across the whole open-table -> version-check -> commit
        // sequence below, not just the commit call itself -- see
        // TableLockManager's own doc comment for why the version check has
        // to be inside the locked section too (otherwise two concurrent
        // commits could both read the same "current" version and both
        // pass their own expected_version check before either writes).
        let lock = self.locks.lock_for(normalized_table_uri.as_str());
        let _guard = lock.lock().await;

        let table = open_table(normalized_table_uri.as_str(), self.storage_opts.clone())
            .await
            .map_err(|e| Status::from(e))?;

        if let Some(expected) = r.expected_version {
            let current = table
                .version()
                .ok_or_else(|| Status::failed_precondition("table not initialized"))?;
            if current != expected {
                return Err(Status::from(DeltaTxnError::VersionConflict {
                    expected,
                    actual: current,
                }));
            }
        }

        let version = commit_actions(table, actions)
            .await
            .map_err(|e| Status::from(e))?;

        Ok(Response::new(CommitResponse {
            committed_version: version,
        }))
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

        let normalized_table_uri =
            ensure_table_uri(&table_uri).map_err(|e| Status::invalid_argument(e.to_string()))?;

        self.check_table_uri_allowed(normalized_table_uri.as_str())?;

        let table = open_table(normalized_table_uri.as_str(), self.storage_opts.clone())
            .await
            .map_err(|e| Status::from(e))?;

        let snapshot = table.snapshot().map_err(map_open_or_snapshot_error)?;
        let (metadata, protocol) = build_metadata_and_protocol(snapshot)?;

        Ok(Response::new(GetTableResponse {
            version: snapshot.version(),
            metadata: Some(metadata),
            protocol: Some(protocol),
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

        let normalized_table_uri =
            ensure_table_uri(&table_uri).map_err(|e| Status::invalid_argument(e.to_string()))?;

        self.check_table_uri_allowed(normalized_table_uri.as_str())?;

        let storage_opts = self.storage_opts.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_CHANNEL_CAPACITY);

        tokio::spawn(stream_active_files(
            normalized_table_uri.to_string(),
            storage_opts,
            tx,
        ));

        let stream: Self::ListActiveFilesStream = Box::pin(ReceiverStream::new(rx));
        Ok(Response::new(stream))
    }
}
