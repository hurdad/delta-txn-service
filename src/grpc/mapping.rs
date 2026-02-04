use deltalake::kernel::{Action, Add, CommitInfo, Metadata, Protocol, Remove};

use serde_json::Value;

use crate::grpc::server::pb;
use pb::action::Action as PbAction;

pub fn map_actions(pb_actions: Vec<pb::Action>) -> Result<Vec<Action>, String> {
    pb_actions.into_iter().map(map_action).collect()
}

fn map_action(action: pb::Action) -> Result<Action, String> {
    match action.action.ok_or("missing action")? {
        PbAction::Add(a) => Ok(Action::Add(Add {
            path: a.path,
            size: a.size,
            modification_time: a.modification_time,
            partition_values: map_string_map(a.partition_values),
            data_change: map_data_change(a.data_change),
            stats: None,
            tags: map_optional_string_map(a.tags),
            deletion_vector: None,
            base_row_id: None,
            default_row_commit_version: None,
            clustering_provider: None,
        })),

        PbAction::Remove(r) => Ok(Action::Remove(Remove {
            path: r.path,
            deletion_timestamp: r.deletion_timestamp,
            data_change: map_data_change(r.data_change),
            extended_file_metadata: None,
            partition_values: None,
            size: None,
            tags: None,
            deletion_vector: None,
            base_row_id: None,
            default_row_commit_version: None,
        })),

        PbAction::Protocol(p) => Ok(Action::Protocol(map_protocol(p)?)),

        PbAction::MetaData(m) => Ok(Action::Metadata(map_metadata(m)?)),

        PbAction::CommitInfo(ci) => Ok(Action::CommitInfo(map_commit_info(ci)?)),
    }
}

fn map_data_change(dc: i32) -> bool {
    matches!(pb::DataChange::try_from(dc), Ok(pb::DataChange::True))
}

fn map_string_map(
    input: std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, Option<String>> {
    input.into_iter().map(|(k, v)| (k, Some(v))).collect()
}

fn map_optional_string_map(
    input: std::collections::HashMap<String, String>,
) -> Option<std::collections::HashMap<String, Option<String>>> {
    if input.is_empty() {
        None
    } else {
        Some(map_string_map(input))
    }
}

fn map_protocol(protocol: pb::Protocol) -> Result<Protocol, String> {
    let value = serde_json::json!({
        "minReaderVersion": protocol.min_reader_version,
        "minWriterVersion": protocol.min_writer_version,
    });
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn map_metadata(metadata: pb::TableMetadata) -> Result<Metadata, String> {
    let name = if metadata.name.is_empty() {
        None
    } else {
        Some(metadata.name)
    };
    let description = if metadata.description.is_empty() {
        None
    } else {
        Some(metadata.description)
    };
    let created_time = if metadata.created_time == 0 {
        None
    } else {
        Some(metadata.created_time)
    };
    let value = serde_json::json!({
        "id": metadata.id,
        "name": name,
        "description": description,
        "format": { "provider": "parquet", "options": {} },
        "schemaString": metadata.schema_string,
        "partitionColumns": metadata.partition_columns,
        "configuration": metadata.configuration,
        "createdTime": created_time,
    });
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn map_commit_info(commit_info: pb::CommitInfo) -> Result<CommitInfo, String> {
    let operation = map_commit_operation(commit_info.operation);
    let operation_parameters = map_json_map(commit_info.operation_parameters);
    let user_metadata = map_user_metadata(commit_info.user_metadata)?;
    let engine_info = map_engine_info(commit_info.engine_name, commit_info.engine_version);
    let timestamp = if commit_info.timestamp == 0 {
        None
    } else {
        Some(commit_info.timestamp)
    };

    Ok(CommitInfo {
        timestamp,
        operation,
        operation_parameters,
        engine_info,
        user_metadata,
        ..Default::default()
    })
}

fn map_json_map(
    input: std::collections::HashMap<String, String>,
) -> Option<std::collections::HashMap<String, Value>> {
    if input.is_empty() {
        None
    } else {
        Some(
            input
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        )
    }
}

fn map_user_metadata(
    input: std::collections::HashMap<String, String>,
) -> Result<Option<String>, String> {
    if input.is_empty() {
        Ok(None)
    } else {
        serde_json::to_string(&input)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

fn map_engine_info(name: String, version: String) -> Option<String> {
    match (name.is_empty(), version.is_empty()) {
        (true, true) => None,
        (false, true) => Some(name),
        (true, false) => Some(version),
        (false, false) => Some(format!("{}/{}", name, version)),
    }
}

fn map_commit_operation(operation: i32) -> Option<String> {
    match pb::CommitOperation::try_from(operation).ok()? {
        pb::CommitOperation::Write => Some("WRITE".to_string()),
        pb::CommitOperation::Merge => Some("MERGE".to_string()),
        pb::CommitOperation::Update => Some("UPDATE".to_string()),
        pb::CommitOperation::Delete => Some("DELETE".to_string()),
        pb::CommitOperation::Optimize => Some("OPTIMIZE".to_string()),
        pb::CommitOperation::Vacuum => Some("VACUUM".to_string()),
        pb::CommitOperation::Restore => Some("RESTORE".to_string()),
        pb::CommitOperation::Convert => Some("CONVERT".to_string()),
        pb::CommitOperation::Unspecified => None,
    }
}
