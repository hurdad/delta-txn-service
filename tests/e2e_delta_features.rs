//! End-to-end coverage of the Delta features beyond the basic add/remove
//! lifecycle: time travel on the read RPCs, SetTransaction idempotency,
//! protocol table features, deletion-vector pass-through, and rejection
//! of file paths that escape the table root.

mod common;

use std::collections::HashMap;

use common::{
    add_file_action, commit_request, create_table_actions, pb, sample_metadata, sample_protocol,
};
use tokio_stream::StreamExt;
use tonic::Code;

async fn create_table(client: &mut common::Client, table_uri: &str, name: &str) {
    client
        .commit(commit_request(table_uri, None, create_table_actions(name)))
        .await
        .expect("create should succeed");
}

async fn list_files(
    client: &mut common::Client,
    req: pb::ListActiveFilesRequest,
) -> Result<(i64, Vec<pb::AddFile>), tonic::Status> {
    let mut stream = client.list_active_files(req).await?.into_inner();
    let mut version = -1;
    let mut files = Vec::new();
    while let Some(msg) = stream.next().await {
        match msg?.payload.expect("payload") {
            pb::list_active_files_response::Payload::Header(h) => version = h.version,
            pb::list_active_files_response::Payload::Batch(b) => files.extend(b.files),
        }
    }
    Ok((version, files))
}

#[tokio::test]
async fn time_travel_reads_an_earlier_version() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("tt");
    let mut client = server.connect().await;
    create_table(&mut client, &table_uri, "tt").await;
    for (v, path) in [(0, "a.parquet"), (1, "b.parquet")] {
        client
            .commit(commit_request(
                &table_uri,
                Some(v),
                vec![add_file_action(path, 1)],
            ))
            .await
            .expect("append should succeed");
    }

    let v1 = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            version: Some(1),
            ..Default::default()
        })
        .await
        .expect("GetTable at version 1")
        .into_inner();
    assert_eq!(v1.version, 1);

    let (version, files) = list_files(
        &mut client,
        pb::ListActiveFilesRequest {
            table_uri: table_uri.clone(),
            version: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("ListActiveFiles at version 1");
    assert_eq!(version, 1);
    assert_eq!(files.len(), 1, "only a.parquet existed at version 1");
    assert_eq!(files[0].path, "a.parquet");

    // A timestamp far in the future resolves to the latest version.
    let latest = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            timestamp_ms: Some(4_000_000_000_000),
            ..Default::default()
        })
        .await
        .expect("GetTable by future timestamp")
        .into_inner();
    assert_eq!(latest.version, 2);

    let missing = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            version: Some(99),
            ..Default::default()
        })
        .await
        .expect_err("version 99 doesn't exist");
    assert_eq!(missing.code(), Code::NotFound, "{missing:?}");

    let before_creation = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            timestamp_ms: Some(0),
            ..Default::default()
        })
        .await
        .expect_err("no version exists at the Unix epoch");
    assert_eq!(
        before_creation.code(),
        Code::NotFound,
        "{before_creation:?}"
    );

    let both = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            version: Some(1),
            timestamp_ms: Some(0),
            ..Default::default()
        })
        .await
        .expect_err("version and timestamp together are ambiguous");
    assert_eq!(both.code(), Code::InvalidArgument);

    let negative = client
        .get_table(pb::GetTableRequest {
            table_uri,
            version: Some(-1),
            ..Default::default()
        })
        .await
        .expect_err("negative version");
    assert_eq!(negative.code(), Code::InvalidArgument);
}

fn txn_action(app_id: &str, version: i64) -> pb::Action {
    pb::Action {
        action: Some(pb::action::Action::Txn(pb::SetTransaction {
            app_id: app_id.to_string(),
            version,
            last_updated: None,
        })),
    }
}

#[tokio::test]
async fn set_transaction_makes_retries_idempotent() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("txn");
    let mut client = server.connect().await;
    create_table(&mut client, &table_uri, "txn").await;

    let before = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            app_id: Some("ingest".to_string()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(before.app_transaction_version, None);

    let batch = || {
        vec![
            txn_action("ingest", 7),
            add_file_action("batch-7.parquet", 1),
        ]
    };
    client
        .commit(commit_request(&table_uri, None, batch()))
        .await
        .expect("first commit of batch 7");

    // A blind retry of the same batch (e.g. after a timeout) is refused.
    let retry = client
        .commit(commit_request(&table_uri, None, batch()))
        .await
        .expect_err("retry of an already-committed txn version");
    assert_eq!(retry.code(), Code::AlreadyExists, "{retry:?}");

    let after = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            app_id: Some("ingest".to_string()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(after.app_transaction_version, Some(7));

    let (_, files) = list_files(
        &mut client,
        pb::ListActiveFilesRequest {
            table_uri: table_uri.clone(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(files.len(), 1, "the retry must not add the file twice");

    // The next batch for the same app id goes through.
    client
        .commit(commit_request(
            &table_uri,
            None,
            vec![
                txn_action("ingest", 8),
                add_file_action("batch-8.parquet", 1),
            ],
        ))
        .await
        .expect("batch 8 should commit");
}

fn dv_table_actions(name: &str) -> Vec<pb::Action> {
    let mut metadata = sample_metadata(name);
    metadata.configuration = HashMap::from([(
        "delta.enableDeletionVectors".to_string(),
        "true".to_string(),
    )]);
    vec![
        pb::Action {
            action: Some(pb::action::Action::Protocol(pb::Protocol {
                min_reader_version: 3,
                min_writer_version: 7,
                reader_features: vec!["deletionVectors".to_string()],
                writer_features: vec!["deletionVectors".to_string()],
            })),
        },
        pb::Action {
            action: Some(pb::action::Action::MetaData(metadata)),
        },
    ]
}

#[tokio::test]
async fn table_features_and_deletion_vectors_round_trip() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("dv");
    let mut client = server.connect().await;
    client
        .commit(commit_request(&table_uri, None, dv_table_actions("dv")))
        .await
        .expect("creating a v3/v7 table with deletionVectors should succeed");

    let table = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let protocol = table.protocol.expect("protocol");
    assert_eq!(protocol.min_reader_version, 3);
    assert_eq!(protocol.min_writer_version, 7);
    assert!(protocol
        .reader_features
        .contains(&"deletionVectors".to_string()));
    assert!(protocol
        .writer_features
        .contains(&"deletionVectors".to_string()));

    // Inline deletion vector: 2 rows deleted from this file.
    let dv = pb::DeletionVector {
        storage_type: "i".to_string(),
        path_or_inline_dv: "wi5b=000010000siXQKl0rr91000f55c8Xg0@@D72lkbi5=-{L".to_string(),
        offset: None,
        size_in_bytes: 40,
        cardinality: 2,
    };
    let mut add = add_file_action("part-0.parquet", 10);
    if let Some(pb::action::Action::Add(ref mut a)) = add.action {
        a.deletion_vector = Some(dv.clone());
    }
    client
        .commit(commit_request(&table_uri, Some(0), vec![add]))
        .await
        .expect("add with a deletion vector should commit");

    let (_, files) = list_files(
        &mut client,
        pb::ListActiveFilesRequest {
            table_uri,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(files.len(), 1);
    let got = files[0]
        .deletion_vector
        .as_ref()
        .expect("ListActiveFiles must report the file's deletion vector");
    assert_eq!(got.storage_type, "i");
    assert_eq!(got.cardinality, 2);
    assert_eq!(got.path_or_inline_dv, dv.path_or_inline_dv);
}

#[tokio::test]
async fn commit_rejects_file_paths_outside_the_table() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("paths");
    let mut client = server.connect().await;
    create_table(&mut client, &table_uri, "paths").await;

    for bad in [
        "s3://other-bucket/secret.parquet",
        "/etc/passwd",
        "../other_table/part-0.parquet",
        "",
    ] {
        let err = client
            .commit(commit_request(
                &table_uri,
                Some(0),
                vec![add_file_action(bad, 1)],
            ))
            .await
            .expect_err("escaping path must be rejected");
        assert_eq!(err.code(), Code::InvalidArgument, "{bad:?}: {err:?}");
    }
}

/// Partition values written on Commit come back from ListActiveFiles as
/// Delta's string encoding, including for a non-string partition column.
#[tokio::test]
async fn partition_values_round_trip_through_list_active_files() {
    let server = common::TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let table_uri = server.new_table_uri("partitioned");

    let mut metadata = sample_metadata("partitioned");
    metadata.schema_string = serde_json::json!({
        "type": "struct",
        "fields": [
            {"name": "id", "type": "long", "nullable": false, "metadata": {}},
            {"name": "region", "type": "string", "nullable": true, "metadata": {}},
            {"name": "year", "type": "integer", "nullable": true, "metadata": {}}
        ]
    })
    .to_string();
    metadata.partition_columns = vec!["region".to_string(), "year".to_string()];
    client
        .commit(commit_request(
            &table_uri,
            None,
            vec![
                pb::Action {
                    action: Some(pb::action::Action::Protocol(common::sample_protocol())),
                },
                pb::Action {
                    action: Some(pb::action::Action::MetaData(metadata)),
                },
            ],
        ))
        .await
        .expect("create partitioned table");

    let partitioned_add = |path: &str, region: &str, year: &str| {
        let mut action = add_file_action(path, 1);
        if let Some(pb::action::Action::Add(add)) = action.action.as_mut() {
            add.partition_values = HashMap::from([
                ("region".to_string(), region.to_string()),
                ("year".to_string(), year.to_string()),
            ]);
        }
        action
    };
    client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![
                partitioned_add("region=us/year=2026/a.parquet", "us", "2026"),
                partitioned_add("region=eu/year=2025/b.parquet", "eu", "2025"),
            ],
        ))
        .await
        .expect("append partitioned files");

    let (_, files) = list_files(
        &mut client,
        pb::ListActiveFilesRequest {
            table_uri,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let by_path: HashMap<_, _> = files
        .into_iter()
        .map(|f| (f.path, f.partition_values))
        .collect();
    assert_eq!(
        by_path["region=us/year=2026/a.parquet"],
        HashMap::from([
            ("region".to_string(), "us".to_string()),
            ("year".to_string(), "2026".to_string()),
        ])
    );
    assert_eq!(
        by_path["region=eu/year=2025/b.parquet"]["region"],
        "eu".to_string()
    );
}

/// Commits write a checkpoint every `delta.checkpointInterval` versions
/// (delta-rs 1.1 doesn't by default), and the table reads correctly from
/// it: latest state and time travel to before the checkpoint.
#[tokio::test]
async fn commits_write_checkpoints_at_the_table_interval() {
    let server = common::TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("checkpointed");

    let mut metadata = sample_metadata("checkpointed");
    metadata
        .configuration
        .insert("delta.checkpointInterval".to_string(), "5".to_string());
    let create = vec![
        pb::Action {
            action: Some(pb::action::Action::Protocol(sample_protocol())),
        },
        pb::Action {
            action: Some(pb::action::Action::MetaData(metadata)),
        },
    ];
    client
        .commit(commit_request(&uri, None, create))
        .await
        .unwrap();
    for v in 0..12 {
        client
            .commit(commit_request(
                &uri,
                Some(v),
                vec![add_file_action(&format!("f{v}.parquet"), 1)],
            ))
            .await
            .unwrap();
    }

    let log_dir = url::Url::parse(&uri)
        .unwrap()
        .to_file_path()
        .unwrap()
        .join("_delta_log");
    let mut checkpoints: Vec<String> = std::fs::read_dir(&log_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains(".checkpoint."))
        .collect();
    checkpoints.sort();
    assert_eq!(
        checkpoints,
        vec![
            "00000000000000000005.checkpoint.parquet",
            "00000000000000000010.checkpoint.parquet"
        ]
    );
    assert!(log_dir.join("_last_checkpoint").exists());

    let count_files = |version: Option<i64>| {
        let mut client = client.clone();
        let uri = uri.clone();
        async move {
            let mut stream = client
                .list_active_files(pb::ListActiveFilesRequest {
                    table_uri: uri,
                    version,
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_inner();
            let mut n = 0;
            while let Some(msg) = stream.next().await {
                if let Some(pb::list_active_files_response::Payload::Batch(b)) =
                    msg.unwrap().payload
                {
                    n += b.files.len();
                }
            }
            n
        }
    };
    assert_eq!(count_files(None).await, 12);
    assert_eq!(count_files(Some(3)).await, 3);
    assert_eq!(count_files(Some(10)).await, 10);
}
