//! Wire contracts for the Unix HTTP clients, without starting a VMM.

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::Router;
use fcvm::firecracker::FirecrackerClient;
use fcvm::hypervisor::cloud_hypervisor::api::ChClient;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

struct ApiServer {
    _dir: tempfile::TempDir,
    path: PathBuf,
    requests: mpsc::UnboundedReceiver<(Method, Uri, HeaderMap, Bytes)>,
    task: tokio::task::JoinHandle<()>,
}

impl ApiServer {
    async fn start(response: Option<(StatusCode, &'static str)>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (sender, requests) = mpsc::unbounded_channel();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let sender = sender.clone();
                async move {
                    sender.send((method, uri, headers, body)).unwrap();
                    match response {
                        Some(reply) => reply,
                        None => std::future::pending().await,
                    }
                }
            },
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            _dir: dir,
            path,
            requests,
            task,
        }
    }

    async fn assert_request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) {
        let (actual_method, uri, headers, bytes) =
            tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(actual_method, method);
        assert_eq!(uri.path(), path);
        if let Some(body) = body {
            assert_eq!(headers["content-type"], "application/json");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                body
            );
        } else {
            assert!(bytes.is_empty(), "body must be empty: {bytes:?}");
        }
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn firecracker_unix_api_preserves_requests_responses_and_deadlines() {
    let data = json!({"latest": {"message": "snowman ☃"}});
    for status in [StatusCode::OK, StatusCode::NO_CONTENT] {
        let mut server = ApiServer::start(Some((status, ""))).await;
        let client = FirecrackerClient::new(server.path.clone()).unwrap();
        client.put_mmds(data.clone()).await.unwrap();
        server
            .assert_request(Method::PUT, "/mmds", Some(data.clone()))
            .await;
        client.patch_mmds(data.clone()).await.unwrap();
        server
            .assert_request(Method::PATCH, "/mmds", Some(data.clone()))
            .await;
    }

    let server = ApiServer::start(Some((StatusCode::BAD_REQUEST, "invalid config"))).await;
    let client = FirecrackerClient::new(server.path.clone()).unwrap();
    for error in [
        client.put_mmds(data.clone()).await.unwrap_err(),
        client.patch_mmds(data.clone()).await.unwrap_err(),
    ] {
        assert_eq!(
            error.to_string(),
            "Firecracker API error: 400 Bad Request - invalid config"
        );
    }

    let server = ApiServer::start(None).await;
    let client = FirecrackerClient::new(server.path.clone())
        .unwrap()
        .with_timeout(Duration::from_millis(10));
    assert_eq!(
        client.put_mmds(data.clone()).await.unwrap_err().to_string(),
        "Firecracker API PUT /mmds timed out after 10ms"
    );
    assert_eq!(
        client.patch_mmds(data).await.unwrap_err().to_string(),
        "Firecracker API PATCH /mmds timed out after 10ms"
    );
}

#[tokio::test]
async fn cloud_hypervisor_unix_api_preserves_requests_responses_and_deadlines() {
    for status in [StatusCode::OK, StatusCode::NO_CONTENT] {
        let mut server = ApiServer::start(Some((status, ""))).await;
        let client = ChClient::new(server.path.clone());
        client.ping().await.unwrap();
        server
            .assert_request(Method::GET, "/api/v1/vmm.ping", None)
            .await;
        client.boot_vm().await.unwrap();
        server
            .assert_request(Method::PUT, "/api/v1/vm.boot", None)
            .await;
        client.snapshot_vm("file:///snapshot").await.unwrap();
        server
            .assert_request(
                Method::PUT,
                "/api/v1/vm.snapshot",
                Some(json!({"destination_url": "file:///snapshot"})),
            )
            .await;
    }

    let server = ApiServer::start(Some((StatusCode::BAD_REQUEST, "invalid config"))).await;
    let client = ChClient::new(server.path.clone());
    assert_eq!(
        client.boot_vm().await.unwrap_err().to_string(),
        "Cloud Hypervisor API error: /api/v1/vm.boot 400 Bad Request - invalid config"
    );

    let server = ApiServer::start(None).await;
    let client = ChClient::new(server.path.clone()).with_timeout(Duration::from_millis(10));
    assert_eq!(
        client.boot_vm().await.unwrap_err().to_string(),
        "Cloud Hypervisor API /api/v1/vm.boot timed out after 10ms"
    );
}
