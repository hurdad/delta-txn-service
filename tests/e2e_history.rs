//! GetHistory: one entry per version, newest first, with commitInfo
//! fields, file counts, paging, validation, and history whose oldest
//! commit files were removed by log retention.

mod common;

use std::collections::HashMap;

use common::{
    add_file_action, commit_request, create_table_actions, pb, remove_file_action, sample_metadata,
    sample_protocol, TestServer, TestServerConfig,
};
use tonic::Code;

fn history_request(
    table_uri: &str,
    limit: Option<i32>,
    before_version: Option<i64>,
) -> pb::GetHistoryRequest {
    pb::GetHistoryRequest {
        table_uri: table_uri.to_string(),
        limit,
        before_version,
    }
}

fn commit_info(operation: pb::CommitOperation, note: &str) -> pb::Action {
    pb::Action {
        action: Some(pb::action::Action::CommitInfo(pb::CommitInfo {
            engine_name: "history-test".to_string(),
            engine_version: "1.0".to_string(),
            operation: operation as i32,
            operation_parameters: HashMap::from([("mode".to_string(), "Append".to_string())]),
            user_metadata: HashMap::from([("note".to_string(), note.to_string())]),
            timestamp: 0,
        })),
    }
}

/// Creates a table and appends `appends` commits of one file each.
async fn table_with_commits(
    client: &mut common::Client,
    server: &TestServer,
    name: &str,
    appends: i64,
) -> String {
    let uri = server.new_table_uri(name);
    client
        .commit(commit_request(&uri, None, create_table_actions(name)))
        .await
        .unwrap();
    for v in 0..appends {
        client
            .commit(commit_request(
                &uri,
                Some(v),
                vec![add_file_action(&format!("f{v}.parquet"), 1)],
            ))
            .await
            .unwrap();
    }
    uri
}

#[tokio::test]
async fn history_lists_every_version_newest_first_with_commit_details() {
    let server = TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("details");
    client
        .commit(commit_request(&uri, None, create_table_actions("details")))
        .await
        .unwrap();
    client
        .commit(commit_request(
            &uri,
            Some(0),
            vec![
                add_file_action("a.parquet", 1),
                add_file_action("b.parquet", 1),
                commit_info(pb::CommitOperation::Write, "first load"),
            ],
        ))
        .await
        .unwrap();
    client
        .commit(commit_request(
            &uri,
            Some(1),
            vec![
                remove_file_action("a.parquet"),
                add_file_action("c.parquet", 1),
            ],
        ))
        .await
        .unwrap();

    let page = client
        .get_history(history_request(&uri, None, None))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.latest_version, 2);
    assert_eq!(page.next_before_version, None);
    let versions: Vec<i64> = page.commits.iter().map(|c| c.version).collect();
    assert_eq!(versions, vec![2, 1, 0]);

    let [v2, v1, v0] = &page.commits[..] else {
        panic!("three entries")
    };
    assert!(page.commits.iter().all(|c| c.has_commit_info));
    assert!(page
        .commits
        .iter()
        .all(|c| c.timestamp_ms > 1_600_000_000_000));
    assert!(v0.timestamp_ms <= v1.timestamp_ms && v1.timestamp_ms <= v2.timestamp_ms);

    let v1_log = url::Url::parse(&uri)
        .unwrap()
        .to_file_path()
        .unwrap()
        .join("_delta_log/00000000000000000001.json");
    assert!(
        std::fs::read_to_string(v1_log)
            .unwrap()
            .contains(&format!("\"timestamp\":{}", v1.timestamp_ms)),
        "the server stamps the client CommitInfo's unset timestamp into the log"
    );

    assert_eq!(v0.operation, "CREATE TABLE");
    assert_eq!((v0.num_add_files, v0.num_remove_files), (0, 0));

    assert_eq!((v1.num_add_files, v1.num_remove_files), (2, 0));
    assert_eq!(v1.operation, "WRITE");
    // delta-rs records no readVersion / isolationLevel / isBlindAppend;
    // they're filled in for commits from writers that do (e.g. Spark).
    assert_eq!(v1.read_version, None);
    assert_eq!(v1.engine_info, "history-test/1.0");
    assert!(
        v1.user_metadata.contains("first load"),
        "{}",
        v1.user_metadata
    );

    assert_eq!((v2.num_add_files, v2.num_remove_files), (1, 1));
    assert_eq!(v2.operation, "WRITE");
}

#[tokio::test]
async fn paging_visits_every_version_exactly_once() {
    let server = TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let uri = table_with_commits(&mut client, &server, "paged", 6).await;

    let mut seen = Vec::new();
    let mut before = None;
    let mut pages = 0;
    loop {
        let page = client
            .get_history(history_request(&uri, Some(3), before))
            .await
            .unwrap()
            .into_inner();
        pages += 1;
        assert!(page.commits.len() <= 3);
        seen.extend(page.commits.iter().map(|c| c.version));
        match page.next_before_version {
            Some(next) => {
                assert_eq!(Some(next), page.commits.last().map(|c| c.version));
                before = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(seen, (0..=6).rev().collect::<Vec<i64>>());
    assert_eq!(pages, 3);

    // before_version = latest + 1 is the same as unset; 0 is an empty page.
    let all = client
        .get_history(history_request(&uri, None, Some(7)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(all.commits.len(), 7);
    let none = client
        .get_history(history_request(&uri, None, Some(0)))
        .await
        .unwrap()
        .into_inner();
    assert!(none.commits.is_empty());
    assert_eq!(none.next_before_version, None);
}

#[tokio::test]
async fn invalid_history_requests_are_rejected() {
    let server = TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let uri = table_with_commits(&mut client, &server, "invalid", 1).await;

    for (limit, before, code) in [
        (Some(0), None, Code::InvalidArgument),
        (Some(-1), None, Code::InvalidArgument),
        (Some(1001), None, Code::InvalidArgument),
        (None, Some(-1), Code::InvalidArgument),
        (None, Some(3), Code::NotFound),
    ] {
        let err = client
            .get_history(history_request(&uri, limit, before))
            .await
            .expect_err("invalid");
        assert_eq!(
            err.code(),
            code,
            "limit {limit:?} before {before:?}: {err:?}"
        );
    }
    client
        .get_history(history_request(&uri, Some(1000), None))
        .await
        .expect("limit 1000 is allowed");

    let err = client
        .get_history(history_request(
            &server.new_table_uri("missing"),
            None,
            None,
        ))
        .await
        .expect_err("missing table");
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn history_respects_the_table_allowlist() {
    let server = TestServer::start(TestServerConfig {
        allowed_table_prefixes: Some(vec!["file:///this-prefix-matches-nothing".to_string()]),
        ..Default::default()
    })
    .await;
    let mut client = server.connect().await;
    let err = client
        .get_history(history_request(&server.new_table_uri("t"), None, None))
        .await
        .expect_err("outside the allowlist");
    assert_eq!(err.code(), Code::PermissionDenied);
}

/// Log retention (by another writer; this service doesn't clean up logs)
/// can delete the oldest commit files once a checkpoint covers them.
/// History then ends at the oldest remaining commit, with no next page.
#[tokio::test]
async fn history_ends_at_the_oldest_commit_the_log_still_holds() {
    let server = TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("retention");
    let mut metadata = sample_metadata("retention");
    metadata
        .configuration
        .insert("delta.checkpointInterval".to_string(), "3".to_string());
    client
        .commit(commit_request(
            &uri,
            None,
            vec![
                pb::Action {
                    action: Some(pb::action::Action::Protocol(sample_protocol())),
                },
                pb::Action {
                    action: Some(pb::action::Action::MetaData(metadata)),
                },
            ],
        ))
        .await
        .unwrap();
    for v in 0..4 {
        client
            .commit(commit_request(
                &uri,
                Some(v),
                vec![add_file_action(&format!("f{v}.parquet"), 1)],
            ))
            .await
            .unwrap();
    }
    // Checkpoint at 3; remove commits 0-2 as log cleanup would.
    let log_dir = url::Url::parse(&uri)
        .unwrap()
        .to_file_path()
        .unwrap()
        .join("_delta_log");
    for v in 0..3 {
        std::fs::remove_file(log_dir.join(format!("{v:020}.json"))).unwrap();
    }

    let page = client
        .get_history(history_request(&uri, None, None))
        .await
        .unwrap()
        .into_inner();
    let versions: Vec<i64> = page.commits.iter().map(|c| c.version).collect();
    assert_eq!(versions, vec![4, 3]);
    assert_eq!(page.next_before_version, None);
}
