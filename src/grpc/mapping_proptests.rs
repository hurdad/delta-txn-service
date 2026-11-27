//! Property-based tests for grpc::mapping (a child module of it, so the
//! private mapping functions are reachable through `super::`). The
//! example-based tests in mapping.rs pin specific cases; these check the
//! invariants over generated inputs: the proto <-> Delta-log-JSON
//! conversions are exact inverses, `validate_file_path` accepts every safe
//! relative path and rejects every escape (in each encoding it decodes),
//! and nothing in the Commit mapping panics on arbitrary input.
//!
//! On failure proptest prints the shrunk input and records it under
//! `proptest-regressions/` so it is replayed first on the next run.

use std::collections::HashMap;

use proptest::prelude::*;

use super::*;
use pb::column_stats::{MaxValue, MinValue};

// ---------------------------------------------------------------- stats

fn finite_f64() -> impl Strategy<Value = f64> {
    prop::num::f64::NORMAL | prop::num::f64::ZERO | prop::num::f64::SUBNORMAL
}

fn min_value() -> impl Strategy<Value = MinValue> {
    prop_oneof![
        any::<i64>().prop_map(MinValue::MinInt),
        finite_f64().prop_map(MinValue::MinDouble),
        ".*".prop_map(MinValue::MinString),
        any::<bool>().prop_map(MinValue::MinBool),
    ]
}

fn max_value() -> impl Strategy<Value = MaxValue> {
    prop_oneof![
        any::<i64>().prop_map(MaxValue::MaxInt),
        finite_f64().prop_map(MaxValue::MaxDouble),
        ".*".prop_map(MaxValue::MaxString),
        any::<bool>().prop_map(MaxValue::MaxBool),
    ]
}

fn column_stats() -> impl Strategy<Value = pb::ColumnStats> {
    (
        prop::option::of(min_value()),
        prop::option::of(max_value()),
        any::<i64>(),
    )
        .prop_map(|(min_value, max_value, null_count)| pb::ColumnStats {
            min_value,
            max_value,
            null_count,
        })
}

fn file_stats() -> impl Strategy<Value = pb::FileStats> {
    (
        any::<i64>(),
        prop::collection::hash_map(".*", column_stats(), 0..6),
    )
        .prop_map(|(num_records, columns)| pb::FileStats {
            num_records,
            columns,
        })
}

proptest! {
    /// Write path then read path gives back exactly what the client sent:
    /// every column, every min/max with its type (an integral double like
    /// 3.0 must stay a double, not become an int), and every null count.
    #[test]
    fn stats_round_trip_through_delta_log_json(stats in file_stats()) {
        let json = map_file_stats(Some(stats.clone())).expect("stats serialize");
        let back = map_stats_json_to_pb(Some(json)).expect("stats parse back");
        prop_assert_eq!(back, stats);
    }

    /// JSON can't represent NaN or +-Infinity; such a min/max is dropped,
    /// and only that value -- the rest of the column survives.
    #[test]
    fn non_finite_doubles_are_dropped_without_losing_the_column(
        column in ".*",
        null_count in any::<i64>(),
        bad in prop_oneof![Just(f64::NAN), Just(f64::INFINITY), Just(f64::NEG_INFINITY)],
        max in prop::option::of(max_value()),
    ) {
        let stats = pb::FileStats {
            num_records: 1,
            columns: HashMap::from([(
                column.clone(),
                pb::ColumnStats { min_value: Some(MinValue::MinDouble(bad)), max_value: max.clone(), null_count },
            )]),
        };
        let back = map_stats_json_to_pb(map_file_stats(Some(stats))).unwrap();
        let col = &back.columns[&column];
        prop_assert_eq!(&col.min_value, &None);
        prop_assert_eq!(&col.max_value, &max);
        prop_assert_eq!(col.null_count, null_count);
    }

    /// The read path must never panic on whatever happens to be in a
    /// table's log (it may have been written by any Delta writer).
    #[test]
    fn stats_parser_never_panics(raw in ".*") {
        let _ = map_stats_json_to_pb(Some(raw));
    }
}

// ----------------------------------------------------------- file paths

/// One path segment with no separators, no '%', no ':' and never "..".
fn safe_segment() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_=.\\- ]{1,12}".prop_filter("not a parent segment", |s| s != "..")
}

fn safe_relative_path() -> impl Strategy<Value = String> {
    prop::collection::vec(safe_segment(), 1..5).prop_map(|segs| segs.join("/"))
}

/// Every spelling of ".." that validate_file_path decodes.
fn parent_segment() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["..", "%2e%2e", "%2E%2E", "%2e.", ".%2E", "%2E%2e"])
}

/// Every separator spelling it decodes.
fn separator() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["/", "\\", "%2f", "%2F", "%5c", "%5C"])
}

proptest! {
    #[test]
    fn safe_relative_paths_are_accepted(path in safe_relative_path()) {
        prop_assert!(validate_file_path("add", &path).is_ok(), "{path}");
    }

    /// A ".." segment anywhere, in any encoding, with any separator.
    #[test]
    fn parent_segments_are_rejected_in_every_encoding(
        before in prop::collection::vec(safe_segment(), 0..3),
        after in prop::collection::vec(safe_segment(), 1..3),
        dots in parent_segment(),
        sep in separator(),
    ) {
        let mut segs: Vec<String> = before;
        segs.push(dots.to_string());
        segs.extend(after);
        let path = segs.join(sep);
        prop_assert!(validate_file_path("add", &path).is_err(), "{path}");
    }

    #[test]
    fn absolute_paths_and_uris_are_rejected(
        rest in safe_relative_path(),
        prefix in prop::sample::select(vec![
            "/", "\\", "%2f", "%2F", "%5c", "%5C", "C:", "z:", "s3://bucket/", "file:///", "gs://b/",
        ]),
    ) {
        let path = format!("{prefix}{rest}");
        prop_assert!(validate_file_path("add", &path).is_err(), "{path}");
    }

    #[test]
    fn path_validation_never_panics(path in ".*") {
        let _ = validate_file_path("add", &path);
    }
}

// ------------------------------------------------------ deletion vectors

fn valid_deletion_vector() -> impl Strategy<Value = pb::DeletionVector> {
    let uuid_relative = (
        "[A-Za-z0-9]{1,30}",
        prop::option::of(any::<i32>()),
        any::<i32>(),
        any::<i64>(),
    )
        .prop_map(|(path, offset, size, card)| pb::DeletionVector {
            storage_type: "u".into(),
            path_or_inline_dv: path,
            offset,
            size_in_bytes: size,
            cardinality: card,
        });
    let inline =
        ("[A-Za-z0-9]{1,30}", any::<i32>(), any::<i64>()).prop_map(|(data, size, card)| {
            pb::DeletionVector {
                storage_type: "i".into(),
                path_or_inline_dv: data,
                offset: None,
                size_in_bytes: size,
                cardinality: card,
            }
        });
    prop_oneof![uuid_relative, inline]
}

proptest! {
    #[test]
    fn deletion_vectors_round_trip(dv in valid_deletion_vector()) {
        let kernel = map_deletion_vector(Some(dv.clone()))
            .expect("valid descriptor")
            .expect("present");
        prop_assert_eq!(map_deletion_vector_to_pb(kernel), dv);
    }

    /// Absolute-path vectors could point outside the table, like absolute
    /// file paths; anything that isn't a known storage type is invalid.
    #[test]
    fn absolute_and_unknown_deletion_vectors_are_rejected(
        mut dv in valid_deletion_vector(),
        storage_type in prop_oneof![Just("p".to_string()), "[a-z]{0,3}".prop_filter("unknown", |s| s != "u" && s != "i" && s != "p")],
    ) {
        dv.storage_type = storage_type;
        prop_assert!(map_deletion_vector(Some(dv)).is_err());
    }
}

// --------------------------------------------------------------- protocol

const READER_FEATURES: &[&str] = &[
    "columnMapping",
    "deletionVectors",
    "timestampNtz",
    "v2Checkpoint",
    "vacuumProtocolCheck",
];
const WRITER_FEATURES: &[&str] = &[
    "appendOnly",
    "invariants",
    "checkConstraints",
    "changeDataFeed",
    "generatedColumns",
    "columnMapping",
    "identityColumns",
    "deletionVectors",
    "rowTracking",
    "timestampNtz",
    "domainMetadata",
    "v2Checkpoint",
    "clustering",
    "vacuumProtocolCheck",
];

fn feature_list(names: &'static [&'static str]) -> impl Strategy<Value = Vec<String>> {
    prop::sample::subsequence(names, 0..=names.len())
        .prop_map(|v| v.into_iter().map(str::to_string).collect())
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

proptest! {
    /// Whatever the kernel accepts comes back unchanged (feature order
    /// aside); whatever it rejects is an Err, never a panic.
    #[test]
    fn accepted_protocols_round_trip(
        min_reader_version in 0i32..=4,
        min_writer_version in 0i32..=8,
        reader_features in feature_list(READER_FEATURES),
        writer_features in feature_list(WRITER_FEATURES),
    ) {
        let sent = pb::Protocol { min_reader_version, min_writer_version, reader_features, writer_features };
        if let Ok(kernel) = map_protocol(sent.clone()) {
            let back = map_protocol_to_pb(&kernel);
            prop_assert_eq!(back.min_reader_version, sent.min_reader_version);
            prop_assert_eq!(back.min_writer_version, sent.min_writer_version);
            prop_assert_eq!(sorted(back.reader_features), sorted(sent.reader_features));
            prop_assert_eq!(sorted(back.writer_features), sorted(sent.writer_features));
        }
    }
}

// ------------------------------------------------------- whole actions

fn string_map() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map(".{0,8}", ".{0,8}", 0..3)
}

/// Mostly-valid paths with some garbage mixed in, so both outcomes occur.
fn any_path() -> impl Strategy<Value = String> {
    prop_oneof![3 => safe_relative_path(), 1 => ".{0,20}"]
}

fn any_action() -> impl Strategy<Value = pb::Action> {
    use pb::action::Action as A;
    let add = (
        any_path(),
        any::<i64>(),
        any::<i32>(),
        string_map(),
        string_map(),
        prop::option::of(file_stats()),
    )
        .prop_map(|(path, size, data_change, partition_values, tags, stats)| {
            A::Add(pb::AddFile {
                path,
                size,
                data_change,
                partition_values,
                tags,
                stats,
                ..Default::default()
            })
        });
    let remove = (
        any_path(),
        any::<i32>(),
        prop::option::of(any::<i64>()),
        string_map(),
        prop::option::of(any::<i64>()),
    )
        .prop_map(
            |(path, data_change, size, partition_values, deletion_timestamp)| {
                A::Remove(pb::RemoveFile {
                    path,
                    data_change,
                    size,
                    partition_values,
                    deletion_timestamp,
                    ..Default::default()
                })
            },
        );
    let txn = (".{0,10}", any::<i64>(), prop::option::of(any::<i64>())).prop_map(
        |(app_id, version, last_updated)| {
            A::Txn(pb::SetTransaction {
                app_id,
                version,
                last_updated,
            })
        },
    );
    let cdc = (
        any_path(),
        any::<i64>(),
        any::<i32>(),
        string_map(),
        string_map(),
    )
        .prop_map(|(path, size, data_change, partition_values, tags)| {
            A::Cdc(pb::AddCdcFile {
                path,
                size,
                data_change,
                partition_values,
                tags,
            })
        });
    let domain =
        (".{0,10}", ".{0,20}", any::<bool>()).prop_map(|(domain, configuration, removed)| {
            A::DomainMetadata(pb::DomainMetadata {
                domain,
                configuration,
                removed,
            })
        });
    prop_oneof![
        add,
        remove,
        txn,
        cdc,
        domain,
        Just(A::Add(Default::default()))
    ]
    .prop_map(|a| pb::Action { action: Some(a) })
}

proptest! {
    /// map_actions returns Ok or Err for any input -- never panics -- and
    /// whatever it accepts upholds the invariants the server relies on.
    #[test]
    fn mapped_actions_uphold_invariants(actions in prop::collection::vec(any_action(), 0..6)) {
        let sent = actions.clone();
        let Ok(mapped) = map_actions(actions) else { return Ok(()) };
        prop_assert_eq!(mapped.len(), sent.len());
        for (pb_action, action) in sent.into_iter().zip(mapped) {
            match (pb_action.action.unwrap(), action) {
                (pb::action::Action::Add(a), Action::Add(add)) => {
                    prop_assert!(validate_file_path("add", &add.path).is_ok());
                    prop_assert!(a.data_change == pb::DataChange::True as i32 || a.data_change == pb::DataChange::False as i32);
                    prop_assert_eq!(add.partition_values.len(), a.partition_values.len());
                    prop_assert!(add.partition_values.values().all(Option::is_some));
                    prop_assert_eq!(add.tags.is_none(), a.tags.is_empty());
                }
                (pb::action::Action::Remove(r), Action::Remove(remove)) => {
                    prop_assert!(validate_file_path("remove", &remove.path).is_ok());
                    // Delta's extendedFileMetadata means size/partitionValues are recorded.
                    let extended = r.size.is_some() || !r.partition_values.is_empty();
                    prop_assert_eq!(remove.extended_file_metadata, extended.then_some(true));
                    prop_assert_eq!(remove.size, r.size);
                }
                (pb::action::Action::Txn(t), Action::Txn(txn)) => {
                    prop_assert!(!t.app_id.is_empty());
                    prop_assert_eq!(txn.app_id, t.app_id);
                    prop_assert_eq!(txn.version, t.version);
                }
                (pb::action::Action::Cdc(_), Action::Cdc(cdc)) => {
                    prop_assert!(validate_file_path("cdc", &cdc.path).is_ok());
                }
                (pb::action::Action::DomainMetadata(d), Action::DomainMetadata(dm)) => {
                    prop_assert!(!d.domain.is_empty());
                    prop_assert_eq!(dm.domain, d.domain);
                }
                (sent, got) => prop_assert!(false, "action kind changed: {sent:?} -> {got:?}"),
            }
        }
    }

    /// The JSON written to commitInfo.userMetadata decodes back to the
    /// exact map the client sent.
    #[test]
    fn user_metadata_round_trips(map in prop::collection::hash_map(".*", ".*", 1..6)) {
        let json = map_user_metadata(map.clone()).unwrap().expect("non-empty map");
        let back: HashMap<String, String> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, map);
    }

    /// engineInfo never has a dangling or doubled separator.
    #[test]
    fn engine_info_shape(name in "[a-z]{0,6}", version in "[0-9.]{0,6}") {
        match map_engine_info(name.clone(), version.clone()) {
            None => prop_assert!(name.is_empty() && version.is_empty()),
            Some(info) => {
                prop_assert!(!info.starts_with('/') && !info.ends_with('/'));
                if !name.is_empty() && !version.is_empty() {
                    prop_assert_eq!(info, format!("{name}/{version}"));
                }
            }
        }
    }
}
