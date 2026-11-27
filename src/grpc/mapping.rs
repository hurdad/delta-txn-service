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
use deltalake::kernel::{
    Action, Add, AddCDCFile, CommitInfo, DeletionVectorDescriptor, DomainMetadata, Metadata,
    Protocol, Remove, StorageType, Transaction,
};

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
//
// `deletion_vector` is passed through as-is: a file with one still
// physically contains the deleted rows, so a reader that never saw it
// would return them.
pub fn map_active_file_to_pb(
    path: String,
    size: i64,
    modification_time: i64,
    partition_values: std::collections::HashMap<String, Option<String>>,
    stats_json: Option<String>,
    deletion_vector: Option<DeletionVectorDescriptor>,
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
        deletion_vector: deletion_vector.map(map_deletion_vector_to_pb),
    }
}

fn map_deletion_vector_to_pb(dv: DeletionVectorDescriptor) -> pb::DeletionVector {
    pb::DeletionVector {
        storage_type: dv.storage_type.to_string(),
        path_or_inline_dv: dv.path_or_inline_dv,
        offset: dv.offset,
        size_in_bytes: dv.size_in_bytes,
        cardinality: dv.cardinality,
    }
}

/// Kernel Protocol -> wire Protocol, including table features. The
/// kernel's own feature accessors are crate-private, so this reads them
/// back through Protocol's Serialize impl (Delta log JSON: camelCase
/// `readerFeatures`/`writerFeatures` arrays of feature names), the same
/// log-JSON route map_protocol() uses in the other direction.
pub fn map_protocol_to_pb(protocol: &Protocol) -> pb::Protocol {
    let value = serde_json::to_value(protocol).unwrap_or(Value::Null);
    let features = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    pb::Protocol {
        min_reader_version: protocol.min_reader_version(),
        min_writer_version: protocol.min_writer_version(),
        reader_features: features("readerFeatures"),
        writer_features: features("writerFeatures"),
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
/// variant. The row-tracking/clustering fields on Add/Remove
/// (base_row_id, default_row_commit_version, clustering_provider) have no
/// wire fields and are always left unset: delta-rs assigns them itself
/// when the table enables those features.
fn map_action(action: pb::Action) -> Result<Action, String> {
    match action.action.ok_or("missing action")? {
        PbAction::Add(a) => {
            validate_file_path("add", &a.path)?;
            Ok(Action::Add(Add {
                path: a.path,
                size: a.size,
                modification_time: a.modification_time,
                partition_values: map_string_map(a.partition_values),
                data_change: map_data_change(a.data_change)?,
                stats: map_file_stats(a.stats),
                tags: map_optional_string_map(a.tags),
                deletion_vector: map_deletion_vector(a.deletion_vector)?,
                base_row_id: None,
                default_row_commit_version: None,
                clustering_provider: None,
            }))
        }

        PbAction::Remove(r) => {
            validate_file_path("remove", &r.path)?;
            // Delta's extendedFileMetadata flag means "size and
            // partitionValues are recorded on this remove".
            let extended = r.size.is_some() || !r.partition_values.is_empty();
            Ok(Action::Remove(Remove {
                path: r.path,
                deletion_timestamp: r.deletion_timestamp,
                data_change: map_data_change(r.data_change)?,
                extended_file_metadata: extended.then_some(true),
                partition_values: (!r.partition_values.is_empty())
                    .then(|| map_string_map(r.partition_values)),
                size: r.size,
                tags: None,
                deletion_vector: map_deletion_vector(r.deletion_vector)?,
                base_row_id: None,
                default_row_commit_version: None,
            }))
        }

        PbAction::Protocol(p) => Ok(Action::Protocol(map_protocol(p)?)),

        PbAction::MetaData(m) => Ok(Action::Metadata(map_metadata(m)?)),

        PbAction::CommitInfo(ci) => Ok(Action::CommitInfo(map_commit_info(ci)?)),

        PbAction::Txn(t) => {
            if t.app_id.is_empty() {
                return Err("txn.app_id must not be empty".to_string());
            }
            Ok(Action::Txn(Transaction::new_with_last_update(
                t.app_id,
                t.version,
                t.last_updated,
            )))
        }

        PbAction::Cdc(c) => {
            validate_file_path("cdc", &c.path)?;
            Ok(Action::Cdc(AddCDCFile {
                path: c.path,
                size: c.size,
                partition_values: map_string_map(c.partition_values),
                data_change: map_data_change(c.data_change)?,
                tags: map_optional_string_map(c.tags),
            }))
        }

        PbAction::DomainMetadata(d) => {
            if d.domain.is_empty() {
                return Err("domain_metadata.domain must not be empty".to_string());
            }
            Ok(Action::DomainMetadata(DomainMetadata {
                domain: d.domain,
                configuration: d.configuration,
                removed: d.removed,
            }))
        }
    }
}

/// Rejects a file path that could point outside the table root. Delta
/// paths are relative to the table root and percent-encoded; the spec
/// also allows absolute URIs (shallow clones), but accepting those would
/// let any writer register a file in another bucket or table, which every
/// reader then trusts -- sidestepping DELTA_TXN_ALLOWED_TABLE_PREFIXES.
/// So this rejects: empty paths, absolute paths and URIs (`/x`, `\x`,
/// `C:x`, `s3://...`), and any `..` segment, checked after decoding the
/// percent-encoded forms of `.`, `/` and `\`.
fn validate_file_path(kind: &str, path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(format!("{kind}.path must not be empty"));
    }
    let decoded = path
        .replace("%2e", ".")
        .replace("%2E", ".")
        .replace("%2f", "/")
        .replace("%2F", "/")
        .replace("%5c", "\\")
        .replace("%5C", "\\");
    let bytes = decoded.as_bytes();
    let has_drive_letter = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if decoded.contains("://")
        || decoded.starts_with('/')
        || decoded.starts_with('\\')
        || has_drive_letter
    {
        return Err(format!(
            "{kind}.path '{path}' must be relative to the table root, not an absolute path or URI"
        ));
    }
    if decoded.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(format!(
            "{kind}.path '{path}' must not contain '..' segments"
        ));
    }
    Ok(())
}

/// Wire DeletionVector -> kernel descriptor. Absolute-path ("p") vectors
/// are rejected for the same reason absolute file paths are (see
/// validate_file_path).
fn map_deletion_vector(
    dv: Option<pb::DeletionVector>,
) -> Result<Option<DeletionVectorDescriptor>, String> {
    let Some(dv) = dv else {
        return Ok(None);
    };
    let storage_type: StorageType = dv
        .storage_type
        .parse()
        .map_err(|_| format!("invalid deletion_vector.storage_type '{}'", dv.storage_type))?;
    match storage_type {
        StorageType::AbsolutePath => {
            return Err(
                "deletion_vector.storage_type 'p' (absolute path) is not accepted".to_string(),
            )
        }
        StorageType::Inline if dv.offset.is_some() => {
            return Err("deletion_vector.offset must be unset for inline ('i') vectors".to_string())
        }
        _ => {}
    }
    if dv.path_or_inline_dv.is_empty() {
        return Err("deletion_vector.path_or_inline_dv must not be empty".to_string());
    }
    Ok(Some(DeletionVectorDescriptor {
        storage_type,
        path_or_inline_dv: dv.path_or_inline_dv,
        offset: dv.offset,
        size_in_bytes: dv.size_in_bytes,
        cardinality: dv.cardinality,
    }))
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

/// Wire Protocol -> kernel Protocol. Feature lists are included exactly
/// when Delta expects them (`readerFeatures` with reader version 3,
/// `writerFeatures` with writer version 7) or when the client sent some
/// anyway; the kernel's own validation (Protocol::try_new, which its
/// Deserialize impl runs) then rejects any inconsistent combination, e.g.
/// features listed under a legacy version.
fn map_protocol(protocol: pb::Protocol) -> Result<Protocol, String> {
    let mut value = serde_json::json!({
        "minReaderVersion": protocol.min_reader_version,
        "minWriterVersion": protocol.min_writer_version,
    });
    if protocol.min_reader_version == 3 || !protocol.reader_features.is_empty() {
        value["readerFeatures"] = serde_json::json!(protocol.reader_features);
    }
    if protocol.min_writer_version == 7 || !protocol.writer_features.is_empty() {
        value["writerFeatures"] = serde_json::json!(protocol.writer_features);
    }
    serde_json::from_value(value).map_err(|e| format!("invalid protocol: {e}"))
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
/// `map_commit_operation` below records the client's CommitOperation as
/// this action's `operation` string (e.g. "DELETE", "MERGE"). That same
/// string is what delta::commit::build_operation reads to choose the
/// DeltaOperation CommitBuilder uses for conflict detection.
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
#[path = "mapping_proptests.rs"]
mod proptests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn validate_file_path_accepts_relative_paths() {
        for ok in [
            "part-0.parquet",
            "year=2026/part-0.parquet",
            "a/b/c%20d.parquet",
            "deletion_vector_1.bin",
            "..hidden-but-not-a-segment.parquet",
        ] {
            assert!(
                validate_file_path("add", ok).is_ok(),
                "{ok} should be accepted"
            );
        }
    }

    #[test]
    fn validate_file_path_rejects_paths_outside_the_table_root() {
        for bad in [
            "",
            "/etc/passwd",
            "\\\\server\\share\\x.parquet",
            "C:x.parquet",
            "s3://other-bucket/secret.parquet",
            "file:///tmp/x.parquet",
            "../other_table/part-0.parquet",
            "a/../../x.parquet",
            "a\\..\\x.parquet",
            "%2e%2e/x.parquet",
            "a%2F..%2Fx.parquet",
            "%2Fetc%2Fpasswd",
        ] {
            assert!(
                validate_file_path("add", bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    fn add_action(path: &str) -> pb::Action {
        pb::Action {
            action: Some(PbAction::Add(pb::AddFile {
                path: path.to_string(),
                data_change: pb::DataChange::True as i32,
                ..Default::default()
            })),
        }
    }

    #[test]
    fn map_action_rejects_escaping_paths_for_add_remove_and_cdc() {
        assert!(map_action(add_action("../x.parquet")).is_err());
        assert!(map_action(pb::Action {
            action: Some(PbAction::Remove(pb::RemoveFile {
                path: "s3://elsewhere/x.parquet".to_string(),
                data_change: pb::DataChange::True as i32,
                ..Default::default()
            })),
        })
        .is_err());
        assert!(map_action(pb::Action {
            action: Some(PbAction::Cdc(pb::AddCdcFile {
                path: "/abs/cdc.parquet".to_string(),
                data_change: pb::DataChange::False as i32,
                ..Default::default()
            })),
        })
        .is_err());
    }

    #[test]
    fn map_action_maps_deletion_vectors_and_rejects_absolute_ones() {
        let dv = |storage_type: &str, offset: Option<i32>| pb::DeletionVector {
            storage_type: storage_type.to_string(),
            path_or_inline_dv: "vBn[lx{q8@P<9BNH/isA".to_string(),
            offset,
            size_in_bytes: 36,
            cardinality: 2,
        };
        let with_dv = |d| pb::Action {
            action: Some(PbAction::Add(pb::AddFile {
                path: "part-0.parquet".to_string(),
                data_change: pb::DataChange::True as i32,
                deletion_vector: Some(d),
                ..Default::default()
            })),
        };

        let Action::Add(add) = map_action(with_dv(dv("u", Some(1)))).unwrap() else {
            panic!("expected Add");
        };
        let mapped = add.deletion_vector.expect("deletion vector should be kept");
        assert_eq!(mapped.storage_type, StorageType::UuidRelativePath);
        assert_eq!(mapped.offset, Some(1));
        assert_eq!(mapped.cardinality, 2);

        assert!(
            map_action(with_dv(dv("p", Some(1)))).is_err(),
            "absolute DV"
        );
        assert!(
            map_action(with_dv(dv("i", Some(1)))).is_err(),
            "inline DV with offset"
        );
        assert!(
            map_action(with_dv(dv("x", None))).is_err(),
            "unknown storage type"
        );
    }

    #[test]
    fn map_deletion_vector_to_pb_round_trips() {
        let dv = DeletionVectorDescriptor {
            storage_type: StorageType::Inline,
            path_or_inline_dv: "wi5b=000010000siXQKl0rr91000f55c8Xg0@@D72lkbi5=-{L".to_string(),
            offset: None,
            size_in_bytes: 40,
            cardinality: 6,
        };
        let pb_dv = map_deletion_vector_to_pb(dv.clone());
        assert_eq!(pb_dv.storage_type, "i");
        assert_eq!(map_deletion_vector(Some(pb_dv)).unwrap(), Some(dv));
    }

    #[test]
    fn map_protocol_round_trips_table_features() {
        let wire = pb::Protocol {
            min_reader_version: 3,
            min_writer_version: 7,
            reader_features: vec!["deletionVectors".to_string()],
            writer_features: vec!["deletionVectors".to_string(), "appendOnly".to_string()],
        };
        let kernel = map_protocol(wire).expect("valid v3/v7 protocol");
        let back = map_protocol_to_pb(&kernel);
        assert_eq!(back.min_reader_version, 3);
        assert_eq!(back.min_writer_version, 7);
        assert_eq!(back.reader_features, vec!["deletionVectors".to_string()]);
        let mut writer = back.writer_features.clone();
        writer.sort();
        assert_eq!(
            writer,
            vec!["appendOnly".to_string(), "deletionVectors".to_string()]
        );
    }

    #[test]
    fn map_protocol_legacy_versions_have_no_features() {
        let kernel = map_protocol(pb::Protocol {
            min_reader_version: 1,
            min_writer_version: 2,
            ..Default::default()
        })
        .unwrap();
        let back = map_protocol_to_pb(&kernel);
        assert!(back.reader_features.is_empty());
        assert!(back.writer_features.is_empty());
    }

    #[test]
    fn map_protocol_rejects_features_on_legacy_versions() {
        assert!(map_protocol(pb::Protocol {
            min_reader_version: 1,
            min_writer_version: 2,
            reader_features: vec!["deletionVectors".to_string()],
            writer_features: Vec::new(),
        })
        .is_err());
    }

    #[test]
    fn map_action_maps_txn_cdc_and_domain_metadata() {
        let txn = map_action(pb::Action {
            action: Some(PbAction::Txn(pb::SetTransaction {
                app_id: "ingest-1".to_string(),
                version: 42,
                last_updated: Some(1_700_000_000_000),
            })),
        })
        .unwrap();
        assert_eq!(
            txn,
            Action::Txn(Transaction::new_with_last_update(
                "ingest-1",
                42,
                Some(1_700_000_000_000)
            ))
        );

        assert!(
            map_action(pb::Action {
                action: Some(PbAction::Txn(pb::SetTransaction::default())),
            })
            .is_err(),
            "empty app_id"
        );

        let cdc = map_action(pb::Action {
            action: Some(PbAction::Cdc(pb::AddCdcFile {
                path: "_change_data/cdc-0.parquet".to_string(),
                size: 10,
                data_change: pb::DataChange::False as i32,
                ..Default::default()
            })),
        })
        .unwrap();
        assert!(
            matches!(cdc, Action::Cdc(ref c) if c.path == "_change_data/cdc-0.parquet" && !c.data_change)
        );

        let domain = map_action(pb::Action {
            action: Some(PbAction::DomainMetadata(pb::DomainMetadata {
                domain: "delta.clustering".to_string(),
                configuration: "{\"clusteringColumns\":[]}".to_string(),
                removed: false,
            })),
        })
        .unwrap();
        assert!(matches!(domain, Action::DomainMetadata(ref d) if d.domain == "delta.clustering"));
    }

    #[test]
    fn map_action_records_extended_file_metadata_on_remove() {
        let Action::Remove(remove) = map_action(pb::Action {
            action: Some(PbAction::Remove(pb::RemoveFile {
                path: "part-0.parquet".to_string(),
                data_change: pb::DataChange::True as i32,
                size: Some(100),
                ..Default::default()
            })),
        })
        .unwrap() else {
            panic!("expected Remove");
        };
        assert_eq!(remove.extended_file_metadata, Some(true));
        assert_eq!(remove.size, Some(100));
    }

    #[test]
    fn map_data_change_maps_valid_values() {
        assert!(map_data_change(pb::DataChange::True as i32).unwrap());
        assert!(!map_data_change(pb::DataChange::False as i32).unwrap());
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
            None,
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

        let pb_add = map_active_file_to_pb(
            "part-0.parquet".to_string(),
            1,
            0,
            partition_values,
            None,
            None,
        );

        assert_eq!(pb_add.partition_values.get("region"), Some(&String::new()));
        assert!(pb_add.stats.is_none());
    }
}
