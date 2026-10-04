//! Wire contracts for the Unix HTTP clients, without starting a VMM.

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::Router;
use fcvm::firecracker::api::BalloonStats;
use fcvm::firecracker::FirecrackerClient;
use fcvm::hypervisor::cloud_hypervisor::api::ChClient;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
async fn firecracker_balloon_statistics_request_and_reply() {
    // Firecracker's reply has more members than fcvm reads.
    let mut server = ApiServer::start(Some((
        StatusCode::OK,
        r#"{"target_pages":16384,"actual_pages":8192,"target_mib":64,"free_memory":1024}"#,
    )))
    .await;
    let client = FirecrackerClient::new(server.path.clone()).unwrap();
    assert_eq!(
        client.balloon_stats().await.unwrap(),
        BalloonStats { target_mib: 64 }
    );
    server
        .assert_request(Method::GET, "/balloon/statistics", None)
        .await;

    // A VM with no balloon device answers 400, and the caller gets the reason.
    let server = ApiServer::start(Some((StatusCode::BAD_REQUEST, "no balloon device"))).await;
    let client = FirecrackerClient::new(server.path.clone()).unwrap();
    assert_eq!(
        client.balloon_stats().await.unwrap_err().to_string(),
        "Firecracker API error: 400 Bad Request - no balloon device"
    );

    let server = ApiServer::start(None).await;
    let client = FirecrackerClient::new(server.path.clone())
        .unwrap()
        .with_timeout(Duration::from_millis(10));
    assert_eq!(
        client.balloon_stats().await.unwrap_err().to_string(),
        "Firecracker API GET /balloon/statistics timed out after 10ms"
    );
}

/// The request deadline covers the reply's body. Only the wait for the response head
/// was timed, so a VMM that sent the head and then stalled hung `balloon_stats()`
/// for good.
#[tokio::test]
async fn firecracker_get_deadline_covers_a_reply_body_that_stalls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    // Answers each request with a head that promises a body, sends none of the body,
    // and keeps the connection open.
    let server = tokio::spawn(async move {
        let mut open = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "the client closed before sending a request head");
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 17\r\n\r\n",
                )
                .await
                .unwrap();
            open.push(stream);
        }
    });

    // The head is written as soon as the request is read, so it reaches the client
    // long before the 200ms deadline. What the deadline has to end is the body.
    let client = FirecrackerClient::new(path)
        .unwrap()
        .with_timeout(Duration::from_millis(200));
    let error = tokio::time::timeout(Duration::from_secs(10), client.balloon_stats())
        .await
        .expect("balloon_stats() was still waiting 10s after its 200ms deadline")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Firecracker API GET /balloon/statistics timed out after 200ms"
    );
    server.abort();
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
