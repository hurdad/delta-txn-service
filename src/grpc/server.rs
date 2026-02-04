use tonic::{Request, Response, Status};

use crate::config::storage::load_storage_options;
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
}

impl DeltaTxnGrpcServer {
    pub fn new() -> Self {
        Self {
            locks: TableLockManager::default(),
        }
    }

    pub fn into_service(self) -> DeltaTxnServiceServer<Self> {
        DeltaTxnServiceServer::new(self)
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

        let lock = self.locks.lock_for(&table_uri);
        let _guard = lock.lock().await;

        let storage_opts = load_storage_options();
        let table = open_table(&table_uri, storage_opts)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        if let Some(expected) = r.expected_version {
            let current = table
                .version()
                .ok_or_else(|| Status::failed_precondition("table not initialized"))?;
            if current != expected {
                return Err(Status::aborted(format!(
                    "version conflict: expected {}, found {}",
                    expected, current
                )));
            }
        }

        let actions = map_actions(r.actions)
            .map_err(|e| Status::invalid_argument(e))?;

        let version = commit_actions(table, actions)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(CommitResponse {
            committed_version: version,
        }))
    }

    async fn get_table(
        &self,
        _req: Request<GetTableRequest>,
    ) -> Result<Response<GetTableResponse>, Status> {
        Err(Status::unimplemented("GetTable not implemented yet"))
    }
}
