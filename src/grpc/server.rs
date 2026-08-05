use std::collections::HashMap;

use deltalake::{ensure_table_uri, DeltaTableError};
use tonic::{Request, Response, Status};
use tracing::warn;

use crate::config::storage::{
    is_table_uri_allowed, load_allowed_table_prefixes, load_storage_options,
};
use crate::delta::errors::DeltaTxnError;
use crate::delta::{commit::commit_actions, table::open_table};
use crate::grpc::mapping::map_actions;
use crate::locking::table_lock::TableLockManager;

pub mod pb {
    tonic::include_proto!("delta.txn.v1");
}

use pb::delta_txn_service_server::{DeltaTxnService, DeltaTxnServiceServer};
use pb::*;

#[derive(Clone)]
pub struct DeltaTxnGrpcServer {
    locks: TableLockManager,
    storage_opts: HashMap<String, String>,
    allowed_table_prefixes: Option<Vec<String>>,
}

impl DeltaTxnGrpcServer {
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

#[tonic::async_trait]
impl DeltaTxnService for DeltaTxnGrpcServer {
    async fn commit(
        &self,
        req: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        let r = req.into_inner();
        let table_uri = r.table_uri;

        let normalized_table_uri =
            ensure_table_uri(&table_uri).map_err(|e| Status::invalid_argument(e.to_string()))?;

        self.check_table_uri_allowed(normalized_table_uri.as_str())?;

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

        let actions = map_actions(r.actions).map_err(|e| Status::invalid_argument(e))?;

        let version = commit_actions(table, actions)
            .await
            .map_err(|e| Status::from(e))?;

        Ok(Response::new(CommitResponse {
            committed_version: version,
        }))
    }

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

        let snapshot = table.snapshot().map_err(|e| match e {
            DeltaTableError::NotInitialized => {
                Status::failed_precondition("table not initialized")
            }
            _ => {
                tracing::error!(error = %e, "failed to load table snapshot");
                Status::internal("internal error loading table snapshot")
            }
        })?;

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

        Ok(Response::new(GetTableResponse {
            version: snapshot.version(),
            metadata: Some(TableMetadata {
                id: metadata.id().to_string(),
                name: metadata.name().unwrap_or_default().to_string(),
                description: metadata.description().unwrap_or_default().to_string(),
                schema_string,
                partition_columns: metadata.partition_columns().clone(),
                configuration: metadata.configuration().clone(),
                created_time: metadata.created_time().unwrap_or_default(),
            }),
            protocol: Some(Protocol {
                min_reader_version: protocol.min_reader_version(),
                min_writer_version: protocol.min_writer_version(),
            }),
        }))
    }
}
