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
            data_change: map_data_change(a.data_change)?,
            stats: map_file_stats(a.stats),
            tags: map_optional_string_map(a.tags),
            deletion_vector: None,
            base_row_id: None,
            default_row_commit_version: None,
            clustering_provider: None,
        })),

        PbAction::Remove(r) => Ok(Action::Remove(Remove {
            path: r.path,
            deletion_timestamp: r.deletion_timestamp,
            data_change: map_data_change(r.data_change)?,
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

fn map_data_change(dc: i32) -> Result<bool, String> {
    match pb::DataChange::try_from(dc) {
        Ok(pb::DataChange::True) => Ok(true),
        Ok(pb::DataChange::False) => Ok(false),
        Ok(pb::DataChange::Unspecified) => Err("data_change is unspecified".to_string()),
        Err(_) => Err(format!("invalid data_change value: {dc}")),
    }
}

fn map_file_stats(stats: Option<pb::FileStats>) -> Option<String> {
    let stats = stats?;

    let mut min_values = serde_json::Map::new();
    let mut max_values = serde_json::Map::new();
    let mut null_count = serde_json::Map::new();

    for (column, column_stats) in stats.columns {
        if let Some(min) = map_column_min_value(column_stats.min_value) {
            min_values.insert(column.clone(), min);
        }
        if let Some(max) = map_column_max_value(column_stats.max_value) {
            max_values.insert(column.clone(), max);
        }
        null_count.insert(column, Value::from(column_stats.null_count));
    }

    let json = serde_json::json!({
        "numRecords": stats.num_records,
        "minValues": min_values,
        "maxValues": max_values,
        "nullCount": null_count,
    });

    serde_json::to_string(&json).ok()
}

fn map_column_min_value(value: Option<pb::column_stats::MinValue>) -> Option<Value> {
    use pb::column_stats::MinValue;
    match value? {
        MinValue::MinInt(v) => Some(Value::from(v)),
        MinValue::MinDouble(v) => serde_json::Number::from_f64(v).map(Value::Number),
        MinValue::MinString(v) => Some(Value::String(v)),
        MinValue::MinBool(v) => Some(Value::Bool(v)),
    }
}

fn map_column_max_value(value: Option<pb::column_stats::MaxValue>) -> Option<Value> {
    use pb::column_stats::MaxValue;
    match value? {
        MaxValue::MaxInt(v) => Some(Value::from(v)),
        MaxValue::MaxDouble(v) => serde_json::Number::from_f64(v).map(Value::Number),
        MaxValue::MaxString(v) => Some(Value::String(v)),
        MaxValue::MaxBool(v) => Some(Value::Bool(v)),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn map_data_change_maps_valid_values() {
        assert_eq!(
            map_data_change(pb::DataChange::True as i32).unwrap(),
            true
        );
        assert_eq!(
            map_data_change(pb::DataChange::False as i32).unwrap(),
            false
        );
    }

    #[test]
    fn map_data_change_rejects_unspecified_and_invalid() {
        let err = map_data_change(pb::DataChange::Unspecified as i32).unwrap_err();
        assert_eq!(err, "data_change is unspecified");

        let err = map_data_change(99).unwrap_err();
        assert_eq!(err, "invalid data_change value: 99");
    }

    #[test]
    fn map_optional_string_map_respects_empty_inputs() {
        let empty = HashMap::<String, String>::new();
        assert!(map_optional_string_map(empty).is_none());

        let mut values = HashMap::new();
        values.insert("region".to_string(), "us-east-1".to_string());
        let mapped = map_optional_string_map(values).expect("expected map");
        assert_eq!(mapped.get("region"), Some(&Some("us-east-1".to_string())));
    }

    #[test]
    fn map_engine_info_formats_engine_name_and_version() {
        assert_eq!(
            map_engine_info("".to_string(), "".to_string()),
            None
        );
        assert_eq!(
            map_engine_info("delta".to_string(), "".to_string()),
            Some("delta".to_string())
        );
        assert_eq!(
            map_engine_info("".to_string(), "1.2.3".to_string()),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            map_engine_info("delta".to_string(), "1.2.3".to_string()),
            Some("delta/1.2.3".to_string())
        );
    }

    #[test]
    fn map_commit_operation_handles_known_values() {
        assert_eq!(
            map_commit_operation(pb::CommitOperation::Write as i32),
            Some("WRITE".to_string())
        );
        assert_eq!(
            map_commit_operation(pb::CommitOperation::Unspecified as i32),
            None
        );
        assert_eq!(map_commit_operation(99), None);
    }

    #[test]
    fn map_file_stats_returns_none_when_absent() {
        assert!(map_file_stats(None).is_none());
    }

    #[test]
    fn map_file_stats_serializes_min_max_and_null_counts() {
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            pb::ColumnStats {
                min_value: Some(pb::column_stats::MinValue::MinInt(1)),
                max_value: Some(pb::column_stats::MaxValue::MaxInt(100)),
                null_count: 0,
            },
        );
        columns.insert(
            "name".to_string(),
            pb::ColumnStats {
                min_value: Some(pb::column_stats::MinValue::MinString("a".to_string())),
                max_value: Some(pb::column_stats::MaxValue::MaxString("z".to_string())),
                null_count: 3,
            },
        );

        let stats = pb::FileStats {
            num_records: 42,
            columns,
        };

        let json = map_file_stats(Some(stats)).expect("expected stats json");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["numRecords"], 42);
        assert_eq!(value["minValues"]["id"], 1);
        assert_eq!(value["maxValues"]["id"], 100);
        assert_eq!(value["minValues"]["name"], "a");
        assert_eq!(value["maxValues"]["name"], "z");
        assert_eq!(value["nullCount"]["name"], 3);
        assert_eq!(value["nullCount"]["id"], 0);
    }

    #[test]
    fn map_user_metadata_serializes_to_json() {
        let empty = HashMap::<String, String>::new();
        assert!(map_user_metadata(empty).unwrap().is_none());

        let mut input = HashMap::new();
        input.insert("source".to_string(), "unit-test".to_string());
        let json = map_user_metadata(input).unwrap().expect("expected json");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value, serde_json::json!({ "source": "unit-test" }));
    }
}
