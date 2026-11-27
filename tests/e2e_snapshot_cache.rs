//! The snapshot cache (delta::cache): cached reads must be exactly as
//! current as uncached ones -- including after commits made by another
//! replica that this server never saw -- and the safety valves (TTL,
//! fallback reload, eviction, disabling) must hold.

mod common;

use std::time::Duration;

use common::{
    add_file_action, commit_request, create_table_actions, pb, TestServer, TestServerConfig,
};
use delta_txn_service::delta::cache::{LookupResult, SnapshotCache};
use url::Url;

async fn get_version(client: &mut common::Client, table_uri: &str) -> pb::GetTableResponse {
    client
        .get_table(pb::GetTableRequest {
            table_uri: table_uri.to_string(),
            ..Default::default()
        })
        .await
        .expect("GetTable")
        .into_inner()
}

async fn list_paths(client: &mut common::Client, table_uri: &str) -> Vec<String> {
    let mut stream = client
        .list_active_files(pb::ListActiveFilesRequest {
            table_uri: table_uri.to_string(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut paths = Vec::new();
    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(pb::list_active_files_response::Payload::Batch(b)) = msg.payload {
            paths.extend(b.files.into_iter().map(|f| f.path));
        }
    }
    paths.sort();
    paths
}

async fn server_with(cache: SnapshotCache) -> TestServer {
    TestServer::start(TestServerConfig {
        snapshot_cache: Some(cache),
        ..Default::default()
    })
    .await
}

#[tokio::test]
async fn cached_reads_follow_this_servers_commits() {
    let cache = SnapshotCache::new(8, Duration::from_secs(300));
    let server = server_with(cache.clone()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("t");

    client
        .commit(commit_request(&uri, None, create_table_actions("t")))
        .await
        .unwrap();
    assert_eq!(get_version(&mut client, &uri).await.version, 0);
    assert_eq!(cache.len(), 1, "the read should have populated the cache");

    for v in 0..3 {
        client
            .commit(commit_request(
                &uri,
                Some(v),
                vec![add_file_action(&format!("f{v}.parquet"), 1)],
            ))
            .await
            .unwrap();
        assert_eq!(get_version(&mut client, &uri).await.version, v + 1);
    }
    assert_eq!(
        list_paths(&mut client, &uri).await,
        vec!["f0.parquet", "f1.parquet", "f2.parquet"]
    );
}

/// The case that matters most: another replica commits, and this
/// server's cached entry must still catch up to it.
#[tokio::test]
async fn cached_reads_see_commits_from_another_replica() {
    let cache = SnapshotCache::new(8, Duration::from_secs(300));
    let reader = server_with(cache.clone()).await;
    let writer = TestServer::start(Default::default()).await;
    let mut reader_client = reader.connect().await;
    let mut writer_client = writer.connect().await;
    // Both replicas address the same table.
    let uri = reader.new_table_uri("shared");

    writer_client
        .commit(commit_request(&uri, None, create_table_actions("shared")))
        .await
        .unwrap();
    assert_eq!(get_version(&mut reader_client, &uri).await.version, 0);

    writer_client
        .commit(commit_request(
            &uri,
            Some(0),
            vec![add_file_action("a.parquet", 1)],
        ))
        .await
        .unwrap();
    assert_eq!(get_version(&mut reader_client, &uri).await.version, 1);
    assert_eq!(
        list_paths(&mut reader_client, &uri).await,
        vec!["a.parquet"]
    );

    // The reader's own commit path uses the cache too: expected_version
    // must be checked against the latest version, not the cached one.
    let stale = reader_client
        .commit(commit_request(
            &uri,
            Some(0),
            vec![add_file_action("b.parquet", 1)],
        ))
        .await
        .expect_err("expected_version 0 is stale");
    assert_eq!(stale.code(), tonic::Code::Aborted);
    reader_client
        .commit(commit_request(
            &uri,
            Some(1),
            vec![add_file_action("b.parquet", 1)],
        ))
        .await
        .expect("expected_version 1 is current");
}

/// Deleting a table's _delta_log and recreating it at the same location.
/// The cache must not serve, or build on, the old table's state.
#[tokio::test]
async fn recreated_table_is_not_served_from_the_old_cache_entry() {
    let cache = SnapshotCache::new(8, Duration::from_secs(300));
    let server = server_with(cache.clone()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("recreated");

    client
        .commit(commit_request(&uri, None, create_table_actions("old")))
        .await
        .unwrap();
    for v in 0..2 {
        client
            .commit(commit_request(
                &uri,
                Some(v),
                vec![add_file_action(&format!("old{v}.parquet"), 1)],
            ))
            .await
            .unwrap();
    }
    assert_eq!(get_version(&mut client, &uri).await.version, 2);

    let log_dir = Url::parse(&uri)
        .unwrap()
        .to_file_path()
        .unwrap()
        .join("_delta_log");
    std::fs::remove_dir_all(&log_dir).unwrap();
    client
        .commit(commit_request(&uri, None, create_table_actions("new")))
        .await
        .expect("recreate");

    let table = get_version(&mut client, &uri).await;
    assert_eq!(table.version, 0);
    assert_eq!(table.metadata.unwrap().name, "new");
    assert!(list_paths(&mut client, &uri).await.is_empty());
}

#[tokio::test]
async fn expired_entries_are_reloaded_from_scratch() {
    let server = TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("ttl");
    client
        .commit(commit_request(&uri, None, create_table_actions("ttl")))
        .await
        .unwrap();
    let url = Url::parse(&uri).unwrap();

    let warm = SnapshotCache::new(4, Duration::from_secs(300));
    assert_eq!(
        warm.latest_with_result(&url, Default::default())
            .await
            .unwrap()
            .1,
        LookupResult::Miss
    );
    assert_eq!(
        warm.latest_with_result(&url, Default::default())
            .await
            .unwrap()
            .1,
        LookupResult::Hit
    );

    let expiring = SnapshotCache::new(4, Duration::ZERO);
    assert_eq!(
        expiring
            .latest_with_result(&url, Default::default())
            .await
            .unwrap()
            .1,
        LookupResult::Miss
    );
    assert_eq!(
        expiring
            .latest_with_result(&url, Default::default())
            .await
            .unwrap()
            .1,
        LookupResult::Miss
    );
}

#[tokio::test]
async fn least_recently_used_entries_are_evicted() {
    let cache = SnapshotCache::new(1, Duration::from_secs(300));
    let server = server_with(cache.clone()).await;
    let mut client = server.connect().await;
    let (a, b) = (server.new_table_uri("a"), server.new_table_uri("b"));
    for (uri, name) in [(&a, "a"), (&b, "b")] {
        client
            .commit(commit_request(uri, None, create_table_actions(name)))
            .await
            .unwrap();
    }
    for _ in 0..3 {
        assert_eq!(
            get_version(&mut client, &a).await.metadata.unwrap().name,
            "a"
        );
        assert_eq!(
            get_version(&mut client, &b).await.metadata.unwrap().name,
            "b"
        );
        assert_eq!(cache.len(), 1);
    }
}

#[tokio::test]
async fn a_disabled_cache_holds_nothing_and_reads_still_work() {
    let cache = SnapshotCache::disabled();
    let server = server_with(cache.clone()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("off");
    client
        .commit(commit_request(&uri, None, create_table_actions("off")))
        .await
        .unwrap();
    client
        .commit(commit_request(
            &uri,
            Some(0),
            vec![add_file_action("x.parquet", 1)],
        ))
        .await
        .unwrap();
    assert_eq!(get_version(&mut client, &uri).await.version, 1);
    assert!(cache.is_empty());
}

/// Time travel bypasses the cache and is unaffected by it.
#[tokio::test]
async fn time_travel_reads_bypass_the_cache() {
    let cache = SnapshotCache::new(8, Duration::from_secs(300));
    let server = server_with(cache.clone()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("tt");
    client
        .commit(commit_request(&uri, None, create_table_actions("tt")))
        .await
        .unwrap();
    client
        .commit(commit_request(
            &uri,
            Some(0),
            vec![add_file_action("x.parquet", 1)],
        ))
        .await
        .unwrap();
    assert_eq!(get_version(&mut client, &uri).await.version, 1);

    let old = client
        .get_table(pb::GetTableRequest {
            table_uri: uri.clone(),
            version: Some(0),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(old.version, 0);
    assert_eq!(get_version(&mut client, &uri).await.version, 1);
}

/// Drives the cache directly (no RPC) so each lookup's result can be
/// checked: catching up after new commits is a hit, and a rewritten log is
/// detected and reloaded, never reused.
#[tokio::test]
async fn lookups_hit_while_the_log_grows_and_reload_when_it_is_rewritten() {
    let server = TestServer::start(Default::default()).await;
    let mut client = server.connect().await;
    let uri = server.new_table_uri("direct");
    let url = Url::parse(&uri).unwrap();
    client
        .commit(commit_request(&uri, None, create_table_actions("direct")))
        .await
        .unwrap();

    let cache = SnapshotCache::new(4, Duration::from_secs(300));
    let lookup = || async {
        cache
            .latest_with_result(&url, Default::default())
            .await
            .unwrap()
    };

    let (table, result) = lookup().await;
    assert_eq!((table.version(), result), (Some(0), LookupResult::Miss));
    let (table, result) = lookup().await;
    assert_eq!((table.version(), result), (Some(0), LookupResult::Hit));

    for v in 0..3 {
        client
            .commit(commit_request(
                &uri,
                Some(v),
                vec![add_file_action(&format!("f{v}.parquet"), 1)],
            ))
            .await
            .unwrap();
        let (table, result) = lookup().await;
        assert_eq!(
            (table.version(), result),
            (Some(v as u64 + 1), LookupResult::Hit)
        );
    }

    let log_dir = url.to_file_path().unwrap().join("_delta_log");
    std::fs::remove_dir_all(&log_dir).unwrap();
    client
        .commit(commit_request(
            &uri,
            None,
            create_table_actions("rewritten"),
        ))
        .await
        .unwrap();
    let (table, result) = lookup().await;
    assert_eq!(
        (table.version(), result),
        (Some(0), LookupResult::RefreshFailed)
    );
    let (table, result) = lookup().await;
    assert_eq!((table.version(), result), (Some(0), LookupResult::Hit));
}
