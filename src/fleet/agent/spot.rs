//! EC2 spot interruption notices, read from instance metadata (IMDSv2).

use std::time::Duration;

/// Poll `<base>/latest/meta-data/spot/instance-action` every `every` until it exists, then
/// return its body. The path answers 404 until EC2 schedules the interruption.
pub async fn wait_for_notice(http: &reqwest::Client, base: &str, every: Duration) -> String {
    let base = base.trim_end_matches('/');
    loop {
        if let Some(body) = check(http, base).await {
            return body;
        }
        tokio::time::sleep(every).await;
    }
}

async fn check(http: &reqwest::Client, base: &str) -> Option<String> {
    let token = http
        .put(format!("{base}/latest/api/token"))
        .header("X-aws-ec2-metadata-token-ttl-seconds", "300")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    let r = http
        .get(format!("{base}/latest/meta-data/spot/instance-action"))
        .header("X-aws-ec2-metadata-token", token)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.text().await.ok()
}
