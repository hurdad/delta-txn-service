//! Which Delta protocols this service can write, checked before anything
//! is committed.
//!
//! delta-rs checks a table's protocol only when *appending* to it, against
//! the table's current protocol. Two gaps follow, both closed here:
//! - Creating a table (no current protocol yet) accepted any protocol,
//!   even an unknown feature, producing a table no later Commit could
//!   append to.
//! - A Commit carrying a new Protocol action could add such a feature to
//!   a writable table, with the same result.
//!
//! Reader features are checked with delta-rs's own checker. Its writer
//! check needs a loaded snapshot, so the writer side mirrors delta-rs's
//! supported list for this build (deltalake with only the `s3` feature,
//! no `datafusion`). `tests/e2e_delta_features.rs` commits to a table
//! with each listed feature and checks one past the list is refused, so a
//! delta-rs upgrade that changes the list fails a test instead of drifting.

use deltalake::kernel::transaction::PROTOCOL;
use deltalake::kernel::{Action, Metadata, Protocol};

use super::errors::DeltaTxnError;

/// Writer features delta-rs 1.1 can commit to without its `datafusion`
/// feature (kernel/transaction/protocol.rs, `INSTANCE`), by their Delta
/// protocol names. Legacy writer versions 3-6 imply features from the
/// `datafusion`-only set (invariants, check constraints, change data
/// feed, generated columns, column mapping, identity columns), so only
/// writer versions 1, 2 and 7 work.
pub const WRITABLE_FEATURES: &[&str] = &[
    "appendOnly",
    "timestampNtz",
    "variantType",
    "variantType-preview",
    "v2Checkpoint",
    "deletionVectors",
    "checkpointProtection",
];

/// The protocol's `writerFeatures` by name. delta-rs doesn't re-export
/// the kernel's feature type, so this reads the protocol's Delta log JSON
/// form, as grpc::mapping does.
fn writer_features(protocol: &Protocol) -> Vec<String> {
    serde_json::to_value(protocol)
        .ok()
        .and_then(|v| v.get("writerFeatures").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Ok if this service can commit to a table with `protocol`.
pub fn check_writable(protocol: &Protocol) -> Result<(), DeltaTxnError> {
    PROTOCOL
        .can_read_from_protocol(protocol)
        .map_err(|e| DeltaTxnError::NotWritable(e.to_string()))?;

    match protocol.min_writer_version() {
        1 | 2 => Ok(()),
        7 => {
            let mut unsupported: Vec<String> = writer_features(protocol)
                .into_iter()
                .filter(|f| !WRITABLE_FEATURES.contains(&f.as_str()))
                .collect();
            if unsupported.is_empty() {
                Ok(())
            } else {
                unsupported.sort();
                Err(DeltaTxnError::NotWritable(format!(
                    "this service can't write tables with these writer features: {}",
                    unsupported.join(", ")
                )))
            }
        }
        v => Err(DeltaTxnError::NotWritable(format!(
            "writer version {v} isn't supported: it implies features (check constraints, \
             change data feed, generated or identity columns, column mapping) this service \
             can't write; use writer version 2, or 7 with supported features"
        ))),
    }
}

/// Table properties that turn a table feature on, and the feature each
/// needs (Delta protocol; Spark upgrades the protocol or refuses when one
/// is set). `None` value: any value other than "none" turns it on.
const FEATURE_PROPERTIES: &[(&str, Option<&str>, &str)] = &[
    ("delta.appendOnly", Some("true"), "appendOnly"),
    ("delta.enableChangeDataFeed", Some("true"), "changeDataFeed"),
    (
        "delta.enableDeletionVectors",
        Some("true"),
        "deletionVectors",
    ),
    ("delta.columnMapping.mode", None, "columnMapping"),
    ("delta.enableRowTracking", Some("true"), "rowTracking"),
    (
        "delta.enableInCommitTimestamps",
        Some("true"),
        "inCommitTimestamp",
    ),
    ("delta.checkpointPolicy", Some("v2"), "v2Checkpoint"),
    ("delta.enableTypeWidening", Some("true"), "typeWidening"),
];

/// Whether `protocol` has writer feature `feature`: listed, for writer
/// version 7, or implied by a legacy writer version.
fn supports(protocol: &Protocol, feature: &str) -> bool {
    match protocol.min_writer_version() {
        7 => writer_features(protocol).iter().any(|f| f == feature),
        v => {
            let since = match feature {
                "appendOnly" | "invariants" => 2,
                "checkConstraints" => 3,
                "changeDataFeed" | "generatedColumns" => 4,
                "columnMapping" => 5,
                "identityColumns" => 6,
                _ => return false,
            };
            v >= since
        }
    }
}

/// Ok unless `metadata`'s properties turn on a feature `protocol` lacks,
/// e.g. `delta.enableChangeDataFeed=true` on a writer-version-2 table.
/// Such a table claims a feature no writer maintains: readers that trust
/// the property (expecting change data files, column-mapped names, ...)
/// misread it.
pub fn check_feature_properties(
    protocol: &Protocol,
    metadata: &Metadata,
) -> Result<(), DeltaTxnError> {
    let config = metadata.configuration();
    let mut missing = Vec::new();
    for (key, on_value, feature) in FEATURE_PROPERTIES {
        let Some(value) = config.get(*key) else {
            continue;
        };
        let enabled = match on_value {
            Some(on) => value.eq_ignore_ascii_case(on),
            None => !value.eq_ignore_ascii_case("none"),
        };
        if enabled && !supports(protocol, feature) {
            missing.push(format!("{key}={value} needs the {feature} table feature"));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(DeltaTxnError::NotWritable(format!(
            "table properties enable features the protocol doesn't have: {}",
            missing.join("; ")
        )))
    }
}

/// Domain metadata actions require the `domainMetadata` writer feature
/// (Delta protocol, "Domain Metadata"). That feature isn't writable by
/// this delta-rs build, so in practice every DomainMetadata action is
/// refused -- but checking the feature keeps this right once it is.
pub fn check_domain_metadata(protocol: &Protocol, actions: &[Action]) -> Result<(), DeltaTxnError> {
    let has_domain_metadata = actions
        .iter()
        .any(|a| matches!(a, Action::DomainMetadata(_)));
    if has_domain_metadata
        && !writer_features(protocol)
            .iter()
            .any(|f| f == "domainMetadata")
    {
        return Err(DeltaTxnError::NotWritable(
            "domain_metadata actions require the table's domainMetadata writer feature, \
             which this service can't write yet"
                .to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocol(reader: i32, writer: i32, rf: &[&str], wf: &[&str]) -> Protocol {
        let mut v = serde_json::json!({
            "minReaderVersion": reader,
            "minWriterVersion": writer,
        });
        if reader == 3 {
            v["readerFeatures"] = serde_json::json!(rf);
        }
        if writer == 7 {
            v["writerFeatures"] = serde_json::json!(wf);
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn legacy_writer_versions() {
        assert!(check_writable(&protocol(1, 1, &[], &[])).is_ok());
        assert!(check_writable(&protocol(1, 2, &[], &[])).is_ok());
        for v in 3..=6 {
            assert!(
                check_writable(&protocol(1, v, &[], &[])).is_err(),
                "writer v{v}"
            );
        }
    }

    #[test]
    fn table_features() {
        assert!(check_writable(&protocol(
            3,
            7,
            &["deletionVectors", "timestampNtz"],
            &[
                "deletionVectors",
                "timestampNtz",
                "appendOnly",
                "checkpointProtection"
            ]
        ))
        .is_ok());
        let err = check_writable(&protocol(1, 7, &[], &["rowTracking", "notARealFeature"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("notARealFeature, rowTracking"), "{err}");
        assert!(check_writable(&protocol(3, 7, &["columnMapping"], &["columnMapping"])).is_err());
    }

    fn metadata(config: &[(&str, &str)]) -> Metadata {
        let config: std::collections::HashMap<_, _> = config.iter().copied().collect();
        serde_json::from_value(serde_json::json!({
            "id": "t",
            "format": {"provider": "parquet", "options": {}},
            "schemaString": r#"{"type":"struct","fields":[]}"#,
            "partitionColumns": [],
            "configuration": config,
        }))
        .unwrap()
    }

    #[test]
    fn feature_properties_need_their_features() {
        let v12 = protocol(1, 2, &[], &[]);
        assert!(check_feature_properties(&v12, &metadata(&[])).is_ok());
        assert!(check_feature_properties(&v12, &metadata(&[("delta.appendOnly", "true")])).is_ok());
        assert!(check_feature_properties(
            &v12,
            &metadata(&[
                ("delta.enableChangeDataFeed", "false"),
                ("delta.columnMapping.mode", "none"),
                ("delta.checkpointInterval", "10"),
            ])
        )
        .is_ok());
        let v11 = protocol(1, 1, &[], &[]);
        assert!(
            check_feature_properties(&v11, &metadata(&[("delta.appendOnly", "true")])).is_err()
        );

        let err = check_feature_properties(
            &v12,
            &metadata(&[
                ("delta.enableChangeDataFeed", "TRUE"),
                ("delta.columnMapping.mode", "name"),
            ]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("changeDataFeed") && err.contains("columnMapping"),
            "{err}"
        );

        let dv = protocol(3, 7, &["deletionVectors"], &["deletionVectors"]);
        let dv_on = metadata(&[("delta.enableDeletionVectors", "true")]);
        assert!(check_feature_properties(&dv, &dv_on).is_ok());
        assert!(check_feature_properties(&v12, &dv_on).is_err());
        assert!(check_feature_properties(
            &protocol(1, 4, &[], &[]),
            &metadata(&[("delta.enableChangeDataFeed", "true")])
        )
        .is_ok());
    }

    #[test]
    fn domain_metadata_needs_its_feature() {
        let dm = Action::DomainMetadata(deltalake::kernel::DomainMetadata {
            domain: "app".into(),
            configuration: "{}".into(),
            removed: false,
        });
        assert!(check_domain_metadata(&protocol(1, 2, &[], &[]), &[]).is_ok());
        assert!(
            check_domain_metadata(&protocol(1, 2, &[], &[]), std::slice::from_ref(&dm)).is_err()
        );
        assert!(check_domain_metadata(&protocol(1, 7, &[], &["domainMetadata"]), &[dm]).is_ok());
    }
}
