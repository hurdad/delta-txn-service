//! Bidirectional translation between the wire protobuf types (pb::*, see
//! grpc::server::pb) and delta-rs's own kernel::Action/Add/Remove/
//! Protocol/Metadata/CommitInfo types.
//!
//! Write direction (Commit, `map_actions`/`map_action` and everything they
//! call): proto -> kernel, so grpc::server::commit() can hand the result
//! straight to delta-rs's CommitBuilder.
//!
//! Read direction (ListActiveFiles, `map_active_file_to_pb` and
//! `map_stats_json_to_pb`): the reverse, taking plain fields read off a
//! LogicalFileView (see grpc::server::stream_active_files_inner) rather
//! than a kernel::Add itself, since LogicalFileView's public API doesn't
//! expose one (see map_active_file_to_pb's own doc comment).
//!
//! Several kernel types (Metadata, Protocol, CommitInfo) are built here by
//! constructing a serde_json::Value in Delta's own log JSON shape
//! (camelCase keys matching `_delta_log/*.json` exactly -- "schemaString",
//! "partitionColumns", etc.) and deserializing it via delta-rs's existing
//! Deserialize impls, rather than constructing the Rust structs field-by-
//! field -- reuses delta-rs's own parsing/validation instead of
//! duplicating it, at the cost of the mapping being one step more
//! indirect than a plain struct literal would be.
use deltalake::kernel::{Action, Add, CommitInfo, Metadata, Protocol, Remove};

use serde_json::Value;

use crate::grpc::server::pb;
use pb::action::Action as PbAction;

/// Top-level entry point for Commit: every action in the request, in
/// order (order matters -- delta-rs's CommitBuilder applies them as a
/// single ordered transaction, e.g. a Remove before an Add for the same
/// logical file is a different outcome than the reverse).
pub fn map_actions(pb_actions: Vec<pb::Action>) -> Result<Vec<Action>, String> {
    pb_actions.into_iter().map(map_action).collect()
}

// The read-side counterpart to map_action's AddFile arm above: one active
// file (as read back off a table's log via
// EagerSnapshot::file_views()/LogicalFileView, see server.rs's
// list_active_files) converted to its wire form for ListActiveFiles.
// Takes plain fields rather than a LogicalFileView itself so this stays a
// pure data transformation with no deltalake-core snapshot-iterator type
// in its signature, matching every other function in this file -- the
// (non-trivial, kernel::StructData-walking) extraction of
// `partition_values` off a LogicalFileView happens in server.rs, right
// where the borrow lives.
//
// `data_change` and `tags` have no equivalent on LogicalFileView's public
// API: both are write-time-only concepts (data_change distinguishes a
// CDC-relevant write from a metadata-only one *at commit time*; tags are
// writer-supplied and not surfaced by the snapshot read API) that don't
// apply to "this file is part of the table's current snapshot," which is
// all a read needs to say -- so data_change is always reported True (this
// file has real data, whatever wrote it) and tags always empty.
pub fn map_active_file_to_pb(
    path: String,
    size: i64,
    modification_time: i64,
    partition_values: std::collections::HashMap<String, Option<String>>,
    stats_json: Option<String>,
) -> pb::AddFile {
    pb::AddFile {
        path,
        size,
        modification_time,
        partition_values: partition_values
            .into_iter()
            .map(|(k, v)| (k, v.unwrap_or_default()))
            .collect(),
        data_change: pb::DataChange::True as i32,
        stats: map_stats_json_to_pb(stats_json),
        tags: std::collections::HashMap::new(),
    }
}

// The inverse of map_file_stats() below: parses the same
// numRecords/minValues/maxValues/nullCount JSON shape that function
// produces (the Delta log's own `add.stats` encoding, unchanged by this
// service either way) back into typed pb::FileStats. A column is included
// if it appears in any of minValues/maxValues/nullCount -- a column with
// only e.g. a null count and no min/max (all-null column) still gets an
// entry, just with min_value/max_value left unset.
fn map_stats_json_to_pb(stats_json: Option<String>) -> Option<pb::FileStats> {
    let stats_json = stats_json?;
    let value: Value = serde_json::from_str(&stats_json).ok()?;

    let num_records = value.get("numRecords").and_then(Value::as_i64).unwrap_or(0);
    let min_values = value.get("minValues").and_then(Value::as_object);
    let max_values = value.get("maxValues").and_then(Value::as_object);
    let null_counts = value.get("nullCount").and_then(Value::as_object);

    let mut column_names: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
    if let Some(m) = min_values {
        column_names.extend(m.keys());
    }
    if let Some(m) = max_values {
        column_names.extend(m.keys());
    }
    if let Some(m) = null_counts {
        column_names.extend(m.keys());
    }

    let mut columns = std::collections::HashMap::new();
    for column in column_names {
        let min_value = min_values
            .and_then(|m| m.get(column))
            .and_then(value_to_min);
        let max_value = max_values
            .and_then(|m| m.get(column))
            .and_then(value_to_max);
        let null_count = null_counts
            .and_then(|m| m.get(column))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        columns.insert(
            column.clone(),
            pb::ColumnStats {
                min_value,
                max_value,
                null_count,
            },
        );
    }

    Some(pb::FileStats {
        num_records,
        columns,
    })
}

fn value_to_min(v: &Value) -> Option<pb::column_stats::MinValue> {
    use pb::column_stats::MinValue;
    match v {
        Value::String(s) => Some(MinValue::MinString(s.clone())),
        Value::Bool(b) => Some(MinValue::MinBool(*b)),
        Value::Number(n) => n
            .as_i64()
            .map(MinValue::MinInt)
            .or_else(|| n.as_f64().map(MinValue::MinDouble)),
        _ => None,
    }
}

fn value_to_max(v: &Value) -> Option<pb::column_stats::MaxValue> {
    use pb::column_stats::MaxValue;
    match v {
        Value::String(s) => Some(MaxValue::MaxString(s.clone())),
        Value::Bool(b) => Some(MaxValue::MaxBool(*b)),
        Value::Number(n) => n
            .as_i64()
            .map(MaxValue::MaxInt)
            .or_else(|| n.as_f64().map(MaxValue::MaxDouble)),
        _ => None,
    }
}

/// Dispatches on the request Action's oneof to the matching kernel::Action
/// variant. `Add`/`Remove` fields not listed here at all (deletion_vector,
/// base_row_id, default_row_commit_version, clustering_provider,
/// extended_file_metadata) aren't oversights -- the AddFile/RemoveFile
/// proto messages simply don't have wire fields for them yet (newer Delta
/// features this service's schema hasn't been extended to carry); every
/// commit through this service always sets them to their "not present"
/// value (None), regardless of what a more modern writer might set.
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

/// `DataChange::Unspecified` (proto3's implicit zero-value default for an
/// enum field a caller never explicitly set) is rejected, not defaulted to
/// either true or false -- whether a file change is data-relevant is
/// semantically meaningful (affects Delta's own conflict-checking and
/// change-data-feed behavior, see delta::commit's own doc comment on
/// operation types), so a caller must say one or the other explicitly
/// rather than this silently guessing.
fn map_data_change(dc: i32) -> Result<bool, String> {
    match pb::DataChange::try_from(dc) {
        Ok(pb::DataChange::True) => Ok(true),
        Ok(pb::DataChange::False) => Ok(false),
        Ok(pb::DataChange::Unspecified) => Err("data_change is unspecified".to_string()),
        Err(_) => Err(format!("invalid data_change value: {dc}")),
    }
}

/// The write-side counterpart to map_stats_json_to_pb (this file's
/// read-side inverse of this exact function): typed pb::FileStats ->
/// Delta's own numRecords/minValues/maxValues/nullCount log JSON
/// encoding, which becomes kernel::Add's `stats: Option<String>` field
/// verbatim.
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

// `serde_json::Number::from_f64` returns `None` for NaN/+-Infinity (JSON
// has no representation for either) -- a MinDouble/MaxDouble stat of NaN
// or Infinity is silently dropped from the resulting stats JSON rather
// than erroring the whole commit over what's normally meaningless input
// anyway (a real column min/max is never NaN/Infinite in practice).
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

/// The proto's `map<string, string>` (no null values possible on the
/// wire -- protobuf map values are never optional) widened to kernel::
/// Add's own `HashMap<String, Option<String>>` partition_values shape,
/// which *does* need to represent a null partition value (a row whose
/// partition-key source column was itself null). Every value coming
/// through this specific path is `Some` -- there is no wire encoding for
/// "this partition value is null" in AddFile.partition_values today, only
/// in whatever a *read* (ListActiveFiles) might report back (see
/// grpc::server's own partition_values extraction, which does handle a
/// genuinely-null Scalar).
fn map_string_map(
    input: std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, Option<String>> {
    input.into_iter().map(|(k, v)| (k, Some(v))).collect()
}

/// AddFile.tags -> kernel::Add.tags: `None` for an empty map rather than
/// `Some(empty map)`, matching kernel::Add's own convention that "no tags
/// at all" and "an empty tags map" are the same state, not two distinct
/// ones worth preserving the distinction between.
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

/// Builds a Delta table's Metadata action from a client-supplied
/// TableMetadata. `metadata.id` is used as-is with no validation --
/// Delta's spec expects a table's `id` to be a GUID, but neither this
/// function nor (as far as this audit checked) delta-rs's own Metadata
/// deserialization enforces that; a client sending an empty or
/// non-GUID id gets exactly that written into the table's log, not
/// rejected up front.
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

/// Builds the CommitInfo *action* (one row of ordinary commit-history
/// metadata written into the log alongside the Add/Remove/etc. actions --
/// what shows up in Delta's own commit-history API/tooling) from the
/// client's request.
///
/// NOTE (relevant to the operation-type gap documented in delta::commit's
/// own doc comment): `map_commit_operation` below *does* faithfully
/// translate the client's CommitOperation enum into this action's
/// `operation` string field (e.g. "DELETE", "MERGE") -- so a client
/// request that says DELETE really does get "DELETE" recorded in this
/// CommitInfo action, and would show up correctly in commit-history
/// tooling. What's still wrong is a *separate* value: CommitBuilder's own
/// `operation: DeltaOperation` parameter (set in delta::commit::
/// commit_actions, unrelated to this CommitInfo action despite the
/// similar name) is hardcoded to Write regardless of what's recorded
/// here, and that's the one delta-rs's conflict-checker actually consults.
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

/// CommitInfo.operation_parameters: proto's string-only map widened to
/// JSON `Value`s (every value is still just a JSON string here -- there's
/// no wire encoding for a richer type), `None` for empty rather than
/// `Some(empty map)`, same convention as map_optional_string_map above.
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

/// CommitInfo.user_metadata: unlike operation_parameters above, delta-rs's
/// own kernel::CommitInfo represents this as a single pre-serialized JSON
/// *string* (not a structured map) -- matching Delta's log format, where
/// commitInfo.userMetadata is an opaque string the writer chose the
/// encoding for. This service always serializes the client's map as JSON;
/// a client wanting some other encoding there has no way to ask for one
/// through this API today.
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

/// CommitInfo.engineInfo: Delta's log convention is a single free-text
/// "name/version" string (there's no structured engineName/engineVersion
/// pair in the log format itself), so both proto fields get joined here.
/// Either half alone is used bare (no dangling "/"); both empty is `None`
/// (no engineInfo recorded at all) rather than an empty string.
fn map_engine_info(name: String, version: String) -> Option<String> {
    match (name.is_empty(), version.is_empty()) {
        (true, true) => None,
        (false, true) => Some(name),
        (true, false) => Some(version),
        (false, false) => Some(format!("{}/{}", name, version)),
    }
}

/// Proto CommitOperation -> Delta's own log convention for commitInfo's
/// `operation` string (upper-case, space-free, matching what Delta/Spark's
/// own writers emit -- see delta::commit's doc comment for how this
/// relates to, and is distinct from, CommitBuilder's own operation-type
/// parameter). `Unspecified` (the proto3 default a caller who never set
/// this field would get) maps to `None`, i.e. "no operation recorded" --
/// not an error, since CommitInfo itself is optional metadata a client
/// isn't required to send an operation for at all.
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
        assert_eq!(map_data_change(pb::DataChange::True as i32).unwrap(), true);
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
        assert_eq!(map_engine_info("".to_string(), "".to_string()), None);
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

    #[test]
    fn map_stats_json_to_pb_returns_none_when_absent() {
        assert!(map_stats_json_to_pb(None).is_none());
    }

    #[test]
    fn map_stats_json_to_pb_returns_none_on_malformed_json() {
        assert!(map_stats_json_to_pb(Some("not json".to_string())).is_none());
    }

    #[test]
    fn map_stats_json_to_pb_parses_every_value_shape() {
        let json = serde_json::json!({
            "numRecords": 42,
            "minValues": {"id": 1, "price": 1.5, "name": "a", "active": true},
            "maxValues": {"id": 100, "price": 9.5, "name": "z", "active": false},
            "nullCount": {"id": 0, "price": 2, "name": 3, "active": 4}
        })
        .to_string();

        let stats = map_stats_json_to_pb(Some(json)).expect("expected stats");
        assert_eq!(stats.num_records, 42);
        assert_eq!(stats.columns.len(), 4);

        let id = &stats.columns["id"];
        assert_eq!(id.min_value, Some(pb::column_stats::MinValue::MinInt(1)));
        assert_eq!(id.max_value, Some(pb::column_stats::MaxValue::MaxInt(100)));
        assert_eq!(id.null_count, 0);

        let price = &stats.columns["price"];
        assert_eq!(
            price.min_value,
            Some(pb::column_stats::MinValue::MinDouble(1.5))
        );
        assert_eq!(
            price.max_value,
            Some(pb::column_stats::MaxValue::MaxDouble(9.5))
        );

        let name = &stats.columns["name"];
        assert_eq!(
            name.min_value,
            Some(pb::column_stats::MinValue::MinString("a".to_string()))
        );
        assert_eq!(
            name.max_value,
            Some(pb::column_stats::MaxValue::MaxString("z".to_string()))
        );

        let active = &stats.columns["active"];
        assert_eq!(
            active.min_value,
            Some(pb::column_stats::MinValue::MinBool(true))
        );
        assert_eq!(
            active.max_value,
            Some(pb::column_stats::MaxValue::MaxBool(false))
        );
        assert_eq!(active.null_count, 4);
    }

    #[test]
    fn map_stats_json_to_pb_includes_a_column_with_only_a_null_count() {
        // An all-null column has no min/max to report but Delta still
        // records its null count -- that column must still produce an
        // entry (with min_value/max_value left unset), not be dropped.
        let json = serde_json::json!({
            "numRecords": 10,
            "minValues": {},
            "maxValues": {},
            "nullCount": {"always_null": 10}
        })
        .to_string();

        let stats = map_stats_json_to_pb(Some(json)).expect("expected stats");
        let column = &stats.columns["always_null"];
        assert_eq!(column.min_value, None);
        assert_eq!(column.max_value, None);
        assert_eq!(column.null_count, 10);
    }

    #[test]
    fn map_file_stats_and_map_stats_json_to_pb_round_trip() {
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            pb::ColumnStats {
                min_value: Some(pb::column_stats::MinValue::MinInt(1)),
                max_value: Some(pb::column_stats::MaxValue::MaxInt(100)),
                null_count: 5,
            },
        );
        let original = pb::FileStats {
            num_records: 7,
            columns,
        };

        let json = map_file_stats(Some(original.clone())).expect("expected json");
        let round_tripped = map_stats_json_to_pb(Some(json)).expect("expected stats");

        assert_eq!(round_tripped.num_records, original.num_records);
        assert_eq!(round_tripped.columns["id"], original.columns["id"]);
    }

    #[test]
    fn map_active_file_to_pb_maps_every_field() {
        let mut partition_values = HashMap::new();
        partition_values.insert("region".to_string(), Some("US".to_string()));

        let pb_add = map_active_file_to_pb(
            "part-0.parquet".to_string(),
            1234,
            1_700_000_000_000,
            partition_values,
            Some(
                serde_json::json!({"numRecords": 5, "minValues": {}, "maxValues": {}, "nullCount": {}})
                    .to_string(),
            ),
        );

        assert_eq!(pb_add.path, "part-0.parquet");
        assert_eq!(pb_add.size, 1234);
        assert_eq!(pb_add.modification_time, 1_700_000_000_000);
        assert_eq!(
            pb_add.partition_values.get("region"),
            Some(&"US".to_string())
        );
        assert_eq!(pb_add.data_change, pb::DataChange::True as i32);
        assert!(pb_add.tags.is_empty());
        assert_eq!(pb_add.stats.expect("expected stats").num_records, 5);
    }

    #[test]
    fn map_active_file_to_pb_treats_null_partition_value_as_empty_string() {
        let mut partition_values = HashMap::new();
        partition_values.insert("region".to_string(), None);

        let pb_add =
            map_active_file_to_pb("part-0.parquet".to_string(), 1, 0, partition_values, None);

        assert_eq!(pb_add.partition_values.get("region"), Some(&String::new()));
        assert!(pb_add.stats.is_none());
    }
}
