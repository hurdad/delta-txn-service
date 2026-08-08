//! End-to-end coverage of the "golden path" through all three RPCs against
//! a real (in-process, `file://`-backed) DeltaTxnGrpcServer: creating a
//! table via `Commit`, reading it back via `GetTable`, appending/removing
//! files, and streaming the active-file set via `ListActiveFiles` --
//! including the multi-batch case FILE_BATCH_SIZE is meant to handle.
//!
//! This is the suite that would have caught the table-creation gap this
//! service used to have (`open_table()` failing outright for a
//! `table_uri` with no existing `_delta_log`, so `Commit` could never
//! actually create a new table) -- none of the unit tests elsewhere in
//! this crate exercise a real, unopened storage location at all.

mod common;

use common::{add_file_action, commit_request, create_table_actions, pb, remove_file_action};
use tonic::Code;

#[tokio::test]
async fn commit_creates_a_new_table_and_get_table_reads_it_back() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    let commit_response = client
        .commit(commit_request(&table_uri, None, create_table_actions("orders")))
        .await
        .expect("Commit that creates a new table should succeed")
        .into_inner();
    assert_eq!(
        commit_response.committed_version, 0,
        "a table's creating commit should always land at version 0"
    );

    let get_table_response = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.clone(),
        })
        .await
        .expect("GetTable on a table that now exists should succeed")
        .into_inner();

    assert_eq!(get_table_response.version, 0);
    let metadata = get_table_response.metadata.expect("expected metadata");
    assert_eq!(metadata.name, "orders");
    assert_eq!(metadata.id, "test-table-orders");
    let protocol = get_table_response.protocol.expect("expected protocol");
    assert_eq!(protocol.min_reader_version, 1);
    assert_eq!(protocol.min_writer_version, 2);
}

#[tokio::test]
async fn commit_append_advances_version_and_checks_expected_version() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    client
        .commit(commit_request(&table_uri, None, create_table_actions("orders")))
        .await
        .expect("create commit should succeed");

    let append_response = client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![add_file_action("part-00000.parquet", 3)],
        ))
        .await
        .expect("append with correct expected_version should succeed")
        .into_inner();
    assert_eq!(append_response.committed_version, 1);

    // Same expected_version reused a second time -- the table has since
    // moved to version 1, so this must be rejected as a conflict, not
    // silently accepted.
    let stale_result = client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![add_file_action("part-00001.parquet", 1)],
        ))
        .await;
    let err = stale_result.expect_err("stale expected_version must be rejected");
    assert_eq!(err.code(), Code::Aborted);
    assert!(
        err.message().contains("expected 0") && err.message().contains("found 1"),
        "conflict message should name both the expected and actual versions, got: {}",
        err.message()
    );
}

#[tokio::test]
async fn list_active_files_streams_header_then_batch_with_stats() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    client
        .commit(commit_request(&table_uri, None, create_table_actions("orders")))
        .await
        .expect("create commit should succeed");
    client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![add_file_action("part-00000.parquet", 3)],
        ))
        .await
        .expect("append should succeed");

    let mut stream = client
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri: table_uri.clone(),
        })
        .await
        .expect("ListActiveFiles should succeed")
        .into_inner();

    let header_msg = stream
        .message()
        .await
        .expect("stream should not error")
        .expect("stream should yield a header message first");
    let header = match header_msg.payload {
        Some(pb::list_active_files_response::Payload::Header(h)) => h,
        other => panic!("expected header as the first message, got {other:?}"),
    };
    assert_eq!(header.version, 1);
    assert_eq!(header.metadata.expect("expected metadata").name, "orders");

    let batch_msg = stream
        .message()
        .await
        .expect("stream should not error")
        .expect("stream should yield a batch message after the header");
    let batch = match batch_msg.payload {
        Some(pb::list_active_files_response::Payload::Batch(b)) => b,
        other => panic!("expected a batch message, got {other:?}"),
    };
    assert_eq!(batch.files.len(), 1);
    let file = &batch.files[0];
    assert_eq!(file.path, "part-00000.parquet");
    assert_eq!(file.data_change, pb::DataChange::True as i32);
    let stats = file.stats.as_ref().expect("expected stats on the file");
    assert_eq!(stats.num_records, 3);
    let id_stats = stats.columns.get("id").expect("expected stats for column 'id'");
    assert_eq!(id_stats.min_value, Some(pb::column_stats::MinValue::MinInt(1)));
    assert_eq!(id_stats.max_value, Some(pb::column_stats::MaxValue::MaxInt(3)));

    assert!(
        stream.message().await.expect("stream should not error").is_none(),
        "stream should end after exactly one header and one batch for a single-file table"
    );
}

#[tokio::test]
async fn list_active_files_excludes_removed_files() {
    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("orders");
    let mut client = server.connect().await;

    client
        .commit(commit_request(&table_uri, None, create_table_actions("orders")))
        .await
        .expect("create commit should succeed");
    client
        .commit(commit_request(
            &table_uri,
            Some(0),
            vec![
                add_file_action("keep.parquet", 1),
                add_file_action("remove-me.parquet", 1),
            ],
        ))
        .await
        .expect("append of both files should succeed");
    client
        .commit(commit_request(
            &table_uri,
            Some(1),
            vec![remove_file_action("remove-me.parquet")],
        ))
        .await
        .expect("remove commit should succeed");

    let mut stream = client
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri: table_uri.clone(),
        })
        .await
        .expect("ListActiveFiles should succeed")
        .into_inner();

    stream.message().await.unwrap(); // header
    let mut paths = Vec::new();
    while let Some(msg) = stream.message().await.expect("stream should not error") {
        if let Some(pb::list_active_files_response::Payload::Batch(batch)) = msg.payload {
            paths.extend(batch.files.into_iter().map(|f| f.path));
        }
    }

    assert_eq!(
        paths,
        vec!["keep.parquet".to_string()],
        "the removed file must not appear in the active-file listing"
    );
}

#[tokio::test]
async fn list_active_files_streams_multiple_batches_for_a_large_file_set() {
    // FILE_BATCH_SIZE (server.rs) is 1000 -- committing more than that
    // many files in one table forces ListActiveFiles to actually exercise
    // its batching logic (a second, tail batch), not just the
    // single-batch path every other test in this file happens to hit.
    const FILE_COUNT: i64 = 1500;

    let server = common::TestServer::start(Default::default()).await;
    let table_uri = server.new_table_uri("wide");
    let mut client = server.connect().await;

    client
        .commit(commit_request(&table_uri, None, create_table_actions("wide")))
        .await
        .expect("create commit should succeed");

    let actions: Vec<pb::Action> = (0..FILE_COUNT)
        .map(|i| add_file_action(&format!("part-{i:05}.parquet"), 1))
        .collect();
    client
        .commit(commit_request(&table_uri, Some(0), actions))
        .await
        .expect("bulk append should succeed");

    let mut stream = client
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri: table_uri.clone(),
        })
        .await
        .expect("ListActiveFiles should succeed")
        .into_inner();

    stream.message().await.unwrap(); // header

    let mut total_files = 0usize;
    let mut batch_count = 0usize;
    while let Some(msg) = stream.message().await.expect("stream should not error") {
        if let Some(pb::list_active_files_response::Payload::Batch(batch)) = msg.payload {
            batch_count += 1;
            total_files += batch.files.len();
        }
    }

    assert_eq!(total_files, FILE_COUNT as usize);
    assert!(
        batch_count >= 2,
        "expected more than one batch message for {FILE_COUNT} files, got {batch_count}"
    );
}
