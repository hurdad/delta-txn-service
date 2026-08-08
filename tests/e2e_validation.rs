//! Error-path coverage: every deliberate rejection this service is
//! documented to perform, verified against a real server rather than the
//! pure-function unit tests elsewhere in this crate (which check the
//! mapping/validation logic in isolation, not that grpc::server actually
//! wires it up to the right gRPC status code end to end).

mod common;

use common::{add_file_action, commit_request, create_table_actions, pb};
use tonic::Code;

#[tokio::test]
async fn get_table_on_a_table_uri_that_does_not_exist_returns_not_found() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("never-created");
    let mut client = server.connect().await;

    let err = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
        })
        .await
        .expect_err("GetTable on a nonexistent table_uri must fail");

    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn list_active_files_on_a_table_uri_that_does_not_exist_returns_not_found() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("never-created");
    let mut client = server.connect().await;

    let mut stream = client
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri: table_uri.clone(),
        })
        .await
        .expect("the RPC call itself succeeds; the error arrives as the stream's first item")
        .into_inner();

    let err = stream
        .message()
        .await
        .expect_err("the stream's first (and only) item should be an error");
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn commit_that_creates_a_table_requires_protocol_and_metadata_actions() {
    let server = common::TestServer::start(Default::default()).await;
    let mut client = server.connect().await;

    // Protocol only, no TableMetadata.
    let table_uri = server.new_table_uri("missing-metadata");
    let protocol_only = vec![pb::Action {
        action: Some(pb::action::Action::Protocol(common::sample_protocol())),
    }];
    let err = client
        .commit(commit_request(&table_uri, None, protocol_only))
        .await
        .expect_err("creating a table without a TableMetadata action must be rejected");
    assert_eq!(err.code(), Code::FailedPrecondition);

    // Neither Protocol nor Metadata at all -- just an AddFile, as if the
    // caller mistakenly believed the table already existed.
    let table_uri = server.new_table_uri("missing-both");
    let err = client
        .commit(commit_request(
            &table_uri,
            None,
            vec![add_file_action("part-00000.parquet", 1)],
        ))
        .await
        .expect_err("appending to a table_uri that doesn't exist yet must be rejected");
    assert_eq!(err.code(), Code::FailedPrecondition);
}

#[tokio::test]
async fn commit_that_creates_a_table_rejects_expected_version_being_set() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("brand-new");
    let mut client = server.connect().await;

    let err = client
        .commit(commit_request(
            &table_uri,
            Some(0),
            create_table_actions("brand-new"),
        ))
        .await
        .expect_err("expected_version on a table-creating Commit must be rejected");
    assert_eq!(err.code(), Code::FailedPrecondition);

    // And the table must genuinely not have been created by the rejected
    // attempt above -- a subsequent, correctly-formed create should still
    // land at version 0, not find a table already there.
    let commit_response = client
        .commit(commit_request(&table_uri, None, create_table_actions("brand-new")))
        .await
        .expect("a correctly-formed create commit should still succeed afterwards")
        .into_inner();
    assert_eq!(commit_response.committed_version, 0);
}

#[tokio::test]
async fn commit_rejects_an_action_with_unspecified_data_change() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    client
        .commit(commit_request(&table_uri, None, create_table_actions("orders")))
        .await
        .expect("create commit should succeed");

    let bad_action = pb::Action {
        action: Some(pb::action::Action::Add(pb::AddFile {
            path: "part-00000.parquet".to_string(),
            size: 1,
            modification_time: 0,
            partition_values: Default::default(),
            data_change: pb::DataChange::Unspecified as i32,
            stats: None,
            tags: Default::default(),
        })),
    };

    let err = client
        .commit(commit_request(&table_uri, Some(0), vec![bad_action]))
        .await
        .expect_err("an AddFile with DATA_CHANGE_UNSPECIFIED must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument);
}
