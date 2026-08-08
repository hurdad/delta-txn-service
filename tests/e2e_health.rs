//! Coverage for the grpc.health.v1.Health service wired into main.rs (see
//! that file's own comment on why it's unauthenticated and served
//! alongside, not through, DeltaTxnServiceServer's own interceptor).

mod common;

use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::HealthCheckRequest;

#[tokio::test]
async fn overall_server_health_reports_serving() {
    let server = common::TestServer::start(Default::default()).await;
    let mut client = server.health_client().await;

    let response = client
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .expect("health check for the overall server should succeed")
        .into_inner();

    assert_eq!(
        ServingStatus::try_from(response.status).expect("valid ServingStatus"),
        ServingStatus::Serving
    );
}

#[tokio::test]
async fn delta_txn_service_health_reports_serving() {
    let server = common::TestServer::start(Default::default()).await;
    let mut client = server.health_client().await;

    let response = client
        .check(HealthCheckRequest {
            service: "delta.txn.v1.DeltaTxnService".to_string(),
        })
        .await
        .expect("health check for delta.txn.v1.DeltaTxnService should succeed")
        .into_inner();

    assert_eq!(
        ServingStatus::try_from(response.status).expect("valid ServingStatus"),
        ServingStatus::Serving
    );
}

#[tokio::test]
async fn health_check_is_not_gated_behind_the_api_key() {
    // Mirrors main.rs's own comment on why the health service is added
    // outside make_auth_interceptor: an orchestrator's liveness/readiness
    // probe has no practical way to supply an API key.
    let server = common::TestServer::start(common::TestServerConfig {
        api_key: Some("super-secret".to_string()),
        ..Default::default()
    })
    .await;
    let mut client = server.health_client().await;

    let response = client
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .expect("health check must succeed even with no credentials supplied")
        .into_inner();

    assert_eq!(
        ServingStatus::try_from(response.status).expect("valid ServingStatus"),
        ServingStatus::Serving
    );
}

#[tokio::test]
async fn health_check_for_an_unknown_service_name_is_not_serving() {
    let server = common::TestServer::start(Default::default()).await;
    let mut client = server.health_client().await;

    let err = client
        .check(HealthCheckRequest {
            service: "not.a.real.Service".to_string(),
        })
        .await
        .expect_err("checking an unregistered service name should not report SERVING");
    assert_eq!(err.code(), tonic::Code::NotFound);
}
