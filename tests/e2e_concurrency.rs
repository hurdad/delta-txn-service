//! Exercises TableLockManager's real effect through the actual gRPC/
//! network boundary -- locking::table_lock's own unit tests call
//! `lock_for` directly; this drives the same "many concurrent writers,
//! one table" scenario through real Commit RPCs instead, using the
//! read-then-commit-with-expected_version retry pattern the README/proto
//! document as the intended client usage for concurrent writers.

mod common;

use common::{add_file_action, commit_request, create_table_actions, pb};
use std::collections::HashSet;
use std::sync::Arc;

#[tokio::test]
async fn concurrent_writers_each_land_their_own_file_with_no_lost_updates() {
    const WRITER_COUNT: usize = 20;

    let server = common::TestServer::start(Default::default()).await;
    let table_uri: Arc<str> = Arc::from(server.new_table_uri("orders"));
    let mut client = server.connect().await;

    client
        .commit(commit_request(
            &table_uri,
            None,
            create_table_actions("orders"),
        ))
        .await
        .expect("create commit should succeed");

    let mut handles = Vec::with_capacity(WRITER_COUNT);
    for i in 0..WRITER_COUNT {
        let mut client = client.clone();
        let table_uri = Arc::clone(&table_uri);
        handles.push(tokio::spawn(async move {
            let path = format!("writer-{i}.parquet");
            // Classic optimistic-concurrency retry loop: read the current
            // version, try to commit against it, retry on ABORTED with a
            // freshly-read version. Any other error is this test's own
            // bug, not a race to retry past.
            loop {
                let current_version = client
                    .get_table(pb::GetTableRequest {
                        table_uri: table_uri.to_string(),
                    })
                    .await
                    .expect("GetTable should succeed")
                    .into_inner()
                    .version;

                let result = client
                    .commit(commit_request(
                        &table_uri,
                        Some(current_version),
                        vec![add_file_action(&path, 1)],
                    ))
                    .await;

                match result {
                    Ok(_) => break,
                    Err(status) if status.code() == tonic::Code::Aborted => continue,
                    Err(status) => panic!("unexpected error committing {path}: {status}"),
                }
            }
        }));
    }

    for handle in handles {
        handle.await.expect("writer task panicked");
    }

    let final_version = client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.to_string(),
        })
        .await
        .expect("final GetTable should succeed")
        .into_inner()
        .version;
    assert_eq!(
        final_version, WRITER_COUNT as i64,
        "table started at version 0; each of {WRITER_COUNT} writers should have landed \
         exactly one commit with no lost updates"
    );

    let mut stream = client
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri: table_uri.to_string(),
        })
        .await
        .expect("ListActiveFiles should succeed")
        .into_inner();
    stream.message().await.unwrap(); // header

    let mut paths = HashSet::new();
    while let Some(msg) = stream.message().await.expect("stream should not error") {
        if let Some(pb::list_active_files_response::Payload::Batch(batch)) = msg.payload {
            paths.extend(batch.files.into_iter().map(|f| f.path));
        }
    }

    assert_eq!(paths.len(), WRITER_COUNT);
    for i in 0..WRITER_COUNT {
        let path = format!("writer-{i}.parquet");
        assert!(
            paths.contains(&path),
            "expected {path} in the active-file set, found {paths:?}"
        );
    }
}
