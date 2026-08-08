use super::errors::DeltaTxnError;
use deltalake::kernel::transaction::{CommitBuilder, TableReference};
use deltalake::kernel::{Action, CommitInfo, Metadata, Protocol};
use deltalake::protocol::{DeltaOperation, SaveMode};
use deltalake::DeltaTableBuilder;
use std::collections::HashMap;
use url::Url;

/// The single place that scans an action list for its `Protocol` action --
/// shared by grpc::server::commit() (deciding whether a create-table
/// request is well-formed) and `create_table` below (building
/// `DeltaOperation::Create`), so there's exactly one definition of "does
/// this action list have a Protocol" for both to agree on.
pub fn find_protocol(actions: &[Action]) -> Option<&Protocol> {
    actions.iter().find_map(|a| match a {
        Action::Protocol(p) => Some(p),
        _ => None,
    })
}

/// See `find_protocol`'s doc comment -- same reasoning, for `Metadata`.
pub fn find_metadata(actions: &[Action]) -> Option<&Metadata> {
    actions.iter().find_map(|a| match a {
        Action::Metadata(m) => Some(m),
        _ => None,
    })
}

fn default_write_operation() -> DeltaOperation {
    DeltaOperation::Write {
        mode: SaveMode::Append,
        partition_by: None,
        predicate: None,
    }
}

/// A `String`-keyed lookup into a CommitInfo action's `operation_parameters`
/// bag (see grpc::mapping::map_json_map -- every value is a JSON string,
/// never a richer type, since the wire representation is a flat
/// `map<string, string>`), returning `None` for a missing key or a
/// present-but-non-string value.
fn string_param(
    params: &Option<std::collections::HashMap<String, serde_json::Value>>,
    key: &str,
) -> Option<String> {
    params.as_ref()?.get(key)?.as_str().map(|s| s.to_string())
}

fn int_param(
    params: &Option<std::collections::HashMap<String, serde_json::Value>>,
    key: &str,
) -> Option<i64> {
    params.as_ref()?.get(key)?.as_i64()
}

// `DeltaOperation::Restore`'s `version` field is u64 (delta-rs versions are
// always non-negative); every other int_param() call site below stays i64.
fn uint_param(
    params: &Option<std::collections::HashMap<String, serde_json::Value>>,
    key: &str,
) -> Option<u64> {
    params.as_ref()?.get(key)?.as_u64()
}

/// Maps a commit's own CommitInfo action (its `operation` string --
/// already produced from the client's CommitOperation enum by
/// grpc::mapping::map_commit_operation -- and `operation_parameters` bag)
/// to the DeltaOperation CommitBuilder needs for its conflict-detection
/// isolation-level choice (see conflict_checker.rs's
/// `can_downgrade_to_snapshot_isolation()`, which calls
/// `operation.changes_data()`).
///
/// FIXED (this was previously hardcoded to always report `Write`,
/// regardless of what the client actually requested -- see git history/
/// the project's own audit notes for the original finding). The practical
/// effect of getting this right is narrower than it might look: delta-rs's
/// own `changes_data()` returns `true` for Write, Merge, Update, Delete,
/// and Restore alike -- so for those five kinds, this mapping produces
/// *identical* conflict-checking behavior to the old hardcoded value; only
/// Optimize and Vacuum (`changes_data() == false`) actually unlock the
/// isolation-level downgrade this was missing before.
///
/// Residual imprecision, not attempted here: Merge's `matched_predicates`/
/// `not_matched_predicates`/`not_matched_by_source_predicates` (structured
/// per-clause data) have no representation in this service's flat
/// `operation_parameters` string map, so they're always empty; Vacuum has
/// no wire fields for its retention-policy parameters at all, so
/// `VacuumStart` always gets the same fixed defaults; CommitOperation's
/// `CONVERT` has no corresponding `DeltaOperation` variant in delta-rs at
/// all (it models conflict-relevant log operations, not table-migration
/// tooling), so it falls back to `Write`, same as an unset/unrecognized
/// operation.
fn build_operation(actions: &[Action]) -> DeltaOperation {
    let Some(commit_info) = actions.iter().find_map(|action| match action {
        Action::CommitInfo(ci) => Some(ci),
        _ => None,
    }) else {
        return default_write_operation();
    };
    let CommitInfo {
        operation,
        operation_parameters: params,
        ..
    } = commit_info;

    match operation.as_deref() {
        Some("MERGE") => DeltaOperation::Merge {
            predicate: string_param(params, "predicate"),
            merge_predicate: string_param(params, "merge_predicate"),
            matched_predicates: Vec::new(),
            not_matched_predicates: Vec::new(),
            not_matched_by_source_predicates: Vec::new(),
        },
        Some("UPDATE") => DeltaOperation::Update {
            predicate: string_param(params, "predicate"),
        },
        Some("DELETE") => DeltaOperation::Delete {
            predicate: string_param(params, "predicate"),
        },
        Some("OPTIMIZE") => DeltaOperation::Optimize {
            predicate: string_param(params, "predicate"),
            target_size: int_param(params, "target_size").unwrap_or(0),
        },
        Some("VACUUM") => DeltaOperation::VacuumStart {
            retention_check_enabled: true,
            specified_retention_millis: int_param(params, "specified_retention_millis"),
            default_retention_millis: int_param(params, "default_retention_millis").unwrap_or(0),
        },
        Some("RESTORE") => DeltaOperation::Restore {
            version: uint_param(params, "version"),
            datetime: int_param(params, "datetime"),
        },
        // "WRITE", CONVERT (no DeltaOperation equivalent), an unrecognized
        // string, or no CommitInfo/operation at all.
        _ => default_write_operation(),
    }
}

/// Commits `actions` to `table` via delta-rs's own `CommitBuilder`, which
/// handles the actual atomic-log-write protocol (conditional-put on the
/// next `_delta_log/N.json`, retried against a fresh snapshot on a losing
/// race) and, before that, delta-rs's own optimistic-concurrency conflict
/// check against whatever committed between our own snapshot read (in the
/// gRPC handler, before this function was called) and this write actually
/// landing. See build_operation()'s own doc comment for how `actions`'
/// own CommitInfo (if any) determines the DeltaOperation passed to
/// CommitBuilder.
pub async fn commit_actions(
    table: deltalake::DeltaTable,
    actions: Vec<Action>,
) -> Result<i64, DeltaTxnError> {
    let table_state = table
        .snapshot()
        .map_err(|e| DeltaTxnError::CommitFailed(e.to_string()))?;
    let operation = build_operation(&actions);

    let result = CommitBuilder::default()
        .with_actions(actions)
        .build(
            Some(table_state as &dyn TableReference),
            table.log_store(),
            operation,
        )
        .await
        .map_err(|e| DeltaTxnError::CommitFailed(e.to_string()))?;

    Ok(result.version() as i64)
}

/// Bootstraps a brand-new Delta table at `table_url` by committing
/// `actions` as its version-0 commit -- the create-path counterpart to
/// `commit_actions` above, used by grpc::server::commit() when
/// `delta::table::table_exists` has already confirmed there's no
/// `_delta_log` at this location yet (see that function's own doc
/// comment for why that check has to happen first).
///
/// Two real differences from `commit_actions`, not just a different entry
/// point for the same thing:
/// - There is no existing `DeltaTableState` to hand `CommitBuilder` as a
///   conflict-check baseline (`table_data: None`) -- there is nothing yet
///   to conflict with.
/// - The operation is always `DeltaOperation::Create`, built from
///   `actions`' own `Protocol`/`Metadata` actions (mirroring what
///   delta-rs's own `operations::create::CreateBuilder` does internally),
///   not whatever `build_operation()` would infer from the client's
///   `CommitInfo` -- a fresh table has no prior operation history for
///   conflict-isolation purposes to get right either way.
///
/// Requires `actions` to contain exactly one `Protocol` and one
/// `Metadata` action; the caller (grpc::server::commit()) validates this
/// up front via `find_protocol`/`find_metadata` -- the same two functions
/// this signature requires it to have already extracted -- so it can
/// return a client-friendly `Status::failed_precondition` naming exactly
/// what's missing, and this function never has to re-scan `actions` (or
/// fail on their absence) itself.
///
/// Same cross-replica caveat as everywhere else optimistic concurrency is
/// involved in this service: two different replicas racing to create the
/// *same* new table_uri at the same time aren't serialized by anything in
/// this process (TableLockManager is per-process only, see its own doc
/// comment) -- delta-rs's own atomic conditional-put still prevents actual
/// corruption. But with no baseline `DeltaTableState` to conflict-check
/// against (`table_data: None` below), the loser doesn't necessarily just
/// get an error back: `CommitBuilder` can retry its write against the
/// snapshot the winner just landed, succeeding at a *later* version with
/// the loser's own Protocol/Metadata actions silently duplicated into the
/// table's history instead. A genuine first commit to a brand-new table
/// is always version 0, so the check below turns "landed at some other
/// version" into an explicit conflict error instead of a silent, corrupt-
/// looking success.
pub async fn create_table(
    table_url: Url,
    storage_options: HashMap<String, String>,
    actions: Vec<Action>,
    protocol: Protocol,
    metadata: Metadata,
) -> Result<i64, DeltaTxnError> {
    let table = DeltaTableBuilder::from_url(table_url.clone())
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?
        .with_storage_options(storage_options)
        .build()
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?;

    let operation = DeltaOperation::Create {
        mode: SaveMode::ErrorIfExists,
        location: table_url,
        protocol,
        metadata,
    };

    let result = CommitBuilder::default()
        .with_actions(actions)
        .build(None, table.log_store(), operation)
        .await
        .map_err(|e| DeltaTxnError::CommitFailed(e.to_string()))?;

    let version = result.version() as i64;
    if version != 0 {
        return Err(DeltaTxnError::VersionConflict {
            expected: 0,
            actual: version,
        });
    }

    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltalake::kernel::Protocol;
    use std::collections::HashMap;

    fn commit_info_action(
        operation: Option<&str>,
        params: Vec<(&str, serde_json::Value)>,
    ) -> Action {
        Action::CommitInfo(CommitInfo {
            operation: operation.map(|s| s.to_string()),
            operation_parameters: if params.is_empty() {
                None
            } else {
                Some(
                    params
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect::<HashMap<_, _>>(),
                )
            },
            ..Default::default()
        })
    }

    #[test]
    fn build_operation_defaults_to_write_with_no_actions() {
        assert!(matches!(build_operation(&[]), DeltaOperation::Write { .. }));
    }

    #[test]
    fn build_operation_defaults_to_write_with_no_commit_info() {
        let actions = vec![Action::Protocol(Protocol::default())];
        assert!(matches!(
            build_operation(&actions),
            DeltaOperation::Write { .. }
        ));
    }

    #[test]
    fn build_operation_maps_optimize_with_predicate_and_target_size() {
        let actions = vec![commit_info_action(
            Some("OPTIMIZE"),
            vec![
                (
                    "predicate",
                    serde_json::Value::String("region = 'US'".into()),
                ),
                ("target_size", serde_json::json!(134217728)),
            ],
        )];
        match build_operation(&actions) {
            DeltaOperation::Optimize {
                predicate,
                target_size,
            } => {
                assert_eq!(predicate.as_deref(), Some("region = 'US'"));
                assert_eq!(target_size, 134217728);
            }
            other => panic!("expected Optimize, got {other:?}"),
        }
    }

    #[test]
    fn build_operation_maps_delete_with_predicate() {
        let actions = vec![commit_info_action(
            Some("DELETE"),
            vec![("predicate", serde_json::Value::String("id = 1".into()))],
        )];
        match build_operation(&actions) {
            DeltaOperation::Delete { predicate } => {
                assert_eq!(predicate.as_deref(), Some("id = 1"));
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn build_operation_maps_vacuum_with_defaults_when_no_params() {
        let actions = vec![commit_info_action(Some("VACUUM"), vec![])];
        match build_operation(&actions) {
            DeltaOperation::VacuumStart {
                retention_check_enabled,
                specified_retention_millis,
                default_retention_millis,
            } => {
                assert!(retention_check_enabled);
                assert_eq!(specified_retention_millis, None);
                assert_eq!(default_retention_millis, 0);
            }
            other => panic!("expected VacuumStart, got {other:?}"),
        }
    }

    #[test]
    fn build_operation_falls_back_to_write_for_convert_and_unknown() {
        assert!(matches!(
            build_operation(&[commit_info_action(Some("CONVERT"), vec![])]),
            DeltaOperation::Write { .. }
        ));
        assert!(matches!(
            build_operation(&[commit_info_action(Some("SOMETHING_NEW"), vec![])]),
            DeltaOperation::Write { .. }
        ));
        assert!(matches!(
            build_operation(&[commit_info_action(None, vec![])]),
            DeltaOperation::Write { .. }
        ));
    }

    #[test]
    fn build_operation_maps_merge_leaving_predicate_vectors_empty() {
        let actions = vec![commit_info_action(
            Some("MERGE"),
            vec![("predicate", serde_json::Value::String("t.id = s.id".into()))],
        )];
        match build_operation(&actions) {
            DeltaOperation::Merge {
                predicate,
                matched_predicates,
                not_matched_predicates,
                not_matched_by_source_predicates,
                ..
            } => {
                assert_eq!(predicate.as_deref(), Some("t.id = s.id"));
                assert!(matched_predicates.is_empty());
                assert!(not_matched_predicates.is_empty());
                assert!(not_matched_by_source_predicates.is_empty());
            }
            other => panic!("expected Merge, got {other:?}"),
        }
    }
}
