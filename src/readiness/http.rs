use anyhow::{Context, Result};
use tokio::time::{sleep, Duration, timeout};
use tracing::{info, debug};

/// Wait for HTTP endpoint to return 200 OK
pub async fn wait_http(url: &str, timeout_secs: u64) -> Result<()> {
    info!(url = url, timeout_secs = timeout_secs, "waiting for HTTP readiness");

    let client = reqwest::Client::new();
    let start = std::time::Instant::now();
    let timeout_duration = Duration::from_secs(timeout_secs);

    timeout(timeout_duration, async {
        loop {
            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(url = url, "HTTP endpoint ready");
                    break;
                }
                Ok(resp) => {
                    debug!(url = url, status = %resp.status(), "HTTP endpoint not ready");
                }
                Err(e) => {
                    debug!(url = url, error = %e, "HTTP request failed");
                }
            }

            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("HTTP readiness timeout")?;

    let elapsed = start.elapsed();
    info!(url = url, elapsed_secs = elapsed.as_secs(), "HTTP endpoint became ready");

    Ok(())
}
