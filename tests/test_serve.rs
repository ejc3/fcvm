//! Integration tests for `fcvm serve` HTTP API.
//!
//! Tests start the server on a random port, create sandboxes, run code/commands,
//! read/write files, and destroy sandboxes — exercising the full gateway + daemon pipeline.

mod common;

use std::time::Duration;

#[cfg(feature = "integration-slow")]
#[tokio::test]
async fn test_serve_create_run_destroy() {
    let logger = common::TestLogger::new("test_serve_create_run_destroy");

    // Find fcvm binary
    let fcvm_path = common::find_fcvm_binary().expect("find fcvm binary");
    logger.info(&format!("Using fcvm binary: {:?}", fcvm_path));

    // Start fcvm serve on a random port
    let port = 18090 + (std::process::id() % 1000) as u16;
    logger.info(&format!("Starting fcvm serve on port {}", port));

    let mut serve_child = tokio::process::Command::new(&fcvm_path)
        .args(["serve", "--port", &port.to_string()])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn fcvm serve");

    let serve_pid = serve_child.id().expect("serve process has PID");
    logger.info(&format!("fcvm serve started with PID {}", serve_pid));

    // Wait for server to be ready
    let base_url = format!("http://localhost:{}", port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build HTTP client");

    // Poll until server accepts connections (max 10s)
    let mut ready = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let url = format!("{}/v1/sandboxes", base_url);
        if client.get(url).send().await.is_ok() {
            ready = true;
            break;
        }
    }
    assert!(ready, "Server failed to start within 10s");
    logger.info("Server is ready");

    // ===== Test 1: List sandboxes (empty) =====
    logger.info("Test 1: List sandboxes (empty)");
    let resp = client
        .get(format!("{}/v1/sandboxes", base_url))
        .send()
        .await
        .expect("list sandboxes");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse JSON");
    assert_eq!(body["success"], true);
    assert_eq!(body["data"].as_array().expect("data is array").len(), 0);

    // ===== Test 2: Create sandbox =====
    logger.info("Test 2: Create sandbox with python runtime");
    let create_body = serde_json::json!({
        "runtime": "python",
        "name": "serve-test",
        "cpu": 1,
        "memory_mib": 512,
    });
    let resp = client
        .post(format!("{}/v1/sandboxes", base_url))
        .json(&create_body)
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .expect("create sandbox");

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("parse create response");
    logger.info(&format!(
        "Create response: {}",
        serde_json::to_string_pretty(&body).unwrap()
    ));
    assert_eq!(status, 200, "Create failed: {:?}", body);
    assert_eq!(body["success"], true);

    let sandbox_id = body["data"]["sandboxId"]
        .as_str()
        .expect("sandboxId in response")
        .to_string();
    let sandbox_url = body["data"]["url"]
        .as_str()
        .expect("url in response")
        .to_string();
    logger.info(&format!(
        "Created sandbox: {} at {}",
        sandbox_id, sandbox_url
    ));

    // ===== Test 3: Health check =====
    logger.info("Test 3: Health check");
    let resp = client
        .get(format!("{}/health", sandbox_url))
        .send()
        .await
        .expect("health check");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse health");
    assert_eq!(body["status"], "ok");
    logger.info(&format!("Health: {}", body));

    // ===== Test 4: Ready check =====
    logger.info("Test 4: Ready check");
    let resp = client
        .get(format!("{}/ready", sandbox_url))
        .send()
        .await
        .expect("ready check");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse ready");
    assert_eq!(body["ready"], true);
    assert_eq!(body["healthy"], true);

    // ===== Test 5: Run code =====
    logger.info("Test 5: Run code (python)");
    let code_body = serde_json::json!({
        "code": "print(40 + 2)",
    });
    let resp = client
        .post(format!("{}/run/code", sandbox_url))
        .json(&code_body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("run code");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse run code");
    logger.info(&format!(
        "Run code result: {}",
        serde_json::to_string_pretty(&body).unwrap()
    ));
    assert_eq!(body["data"]["exit_code"], 0);
    assert_eq!(body["data"]["output"].as_str().unwrap().trim(), "42");
    assert_eq!(body["data"]["language"], "python");

    // ===== Test 6: Run command =====
    logger.info("Test 6: Run command");
    let cmd_body = serde_json::json!({
        "command": "echo hello world",
    });
    let resp = client
        .post(format!("{}/run/command", sandbox_url))
        .json(&cmd_body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("run command");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse run command");
    logger.info(&format!(
        "Run command result: {}",
        serde_json::to_string_pretty(&body).unwrap()
    ));
    assert_eq!(body["data"]["exit_code"], 0);
    assert_eq!(
        body["data"]["stdout"].as_str().unwrap().trim(),
        "hello world"
    );
    assert!(body["data"]["duration_ms"].as_u64().unwrap() > 0);

    // ===== Test 7: Write file =====
    logger.info("Test 7: Write file");
    let write_body = serde_json::json!({
        "path": "/tmp/test-serve.txt",
        "content": "hello from serve test",
    });
    let resp = client
        .post(format!("{}/files", sandbox_url))
        .json(&write_body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("write file");
    assert_eq!(resp.status(), 201);

    // ===== Test 8: Read file =====
    logger.info("Test 8: Read file");
    let resp = client
        .get(format!("{}/files/tmp/test-serve.txt", sandbox_url))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("read file");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse read file");
    assert_eq!(
        body["content"].as_str().unwrap().trim(),
        "hello from serve test"
    );

    // ===== Test 9: File exists (HEAD) =====
    logger.info("Test 9: File exists check");
    let resp = client
        .head(format!("{}/files/tmp/test-serve.txt", sandbox_url))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("check file exists");
    assert_eq!(resp.status(), 200);

    // ===== Test 10: File not exists (HEAD) =====
    logger.info("Test 10: File not exists check");
    let resp = client
        .head(format!("{}/files/tmp/nonexistent-file.txt", sandbox_url))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("check file not exists");
    assert_eq!(resp.status(), 404);

    // ===== Test 11: Delete file =====
    logger.info("Test 11: Delete file");
    let resp = client
        .delete(format!("{}/files/tmp/test-serve.txt", sandbox_url))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("delete file");
    assert_eq!(resp.status(), 204);

    // Verify file is gone
    let resp = client
        .head(format!("{}/files/tmp/test-serve.txt", sandbox_url))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("verify file deleted");
    assert_eq!(resp.status(), 404);

    // ===== Test 12: List files =====
    logger.info("Test 12: List files");
    let resp = client
        .get(format!("{}/files?path=/tmp", sandbox_url))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("list files");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse list files");
    assert!(body["data"]["files"].is_array());

    // ===== Test 13: Get sandbox =====
    logger.info("Test 13: Get sandbox details");
    let resp = client
        .get(format!("{}/v1/sandboxes/{}", base_url, sandbox_id))
        .send()
        .await
        .expect("get sandbox");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse get sandbox");
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["sandboxId"], sandbox_id);

    // ===== Test 14: List sandboxes (1 present) =====
    logger.info("Test 14: List sandboxes (1 present)");
    let resp = client
        .get(format!("{}/v1/sandboxes", base_url))
        .send()
        .await
        .expect("list sandboxes");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse list");
    assert_eq!(body["data"].as_array().expect("data array").len(), 1);

    // ===== Test 15: Destroy sandbox =====
    logger.info("Test 15: Destroy sandbox");
    let resp = client
        .delete(format!("{}/v1/sandboxes/{}", base_url, sandbox_id))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("destroy sandbox");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse destroy");
    assert_eq!(body["success"], true);

    // ===== Test 16: Verify sandbox is gone =====
    logger.info("Test 16: Verify sandbox is gone");
    let resp = client
        .get(format!("{}/v1/sandboxes/{}", base_url, sandbox_id))
        .send()
        .await
        .expect("get deleted sandbox");
    assert_eq!(resp.status(), 404);

    // ===== Test 17: Destroy non-existent (404) =====
    logger.info("Test 17: Destroy non-existent sandbox");
    let resp = client
        .delete(format!("{}/v1/sandboxes/nonexistent", base_url))
        .send()
        .await
        .expect("destroy nonexistent");
    assert_eq!(resp.status(), 404);

    // Clean up: kill the serve process
    logger.info("Cleaning up: killing serve process");
    common::kill_process(serve_pid).await;
    let _ = serve_child.wait().await;

    logger.finish(true);
}
