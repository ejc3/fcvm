use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::time::{sleep, Duration};

use crate::output::OutputHandle;
use crate::types::{LatestMetadata, Plan, VolumeMount};

/// Fetch the container plan from MMDS with retry.
pub async fn fetch_plan() -> Result<Plan> {
    let client = reqwest::Client::new();

    eprintln!(
        "[fc-agent] requesting MMDS V2 session token from http://169.254.169.254/latest/api/token"
    );
    let token_response = match client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            eprintln!("[fc-agent] token request succeeded");
            resp
        }
        Err(e) => {
            eprintln!("[fc-agent] token request FAILED - detailed error:");
            eprintln!("[fc-agent]   error type: {:?}", e);
            if e.is_timeout() {
                eprintln!("[fc-agent]   TIMEOUT: MMDS not responding within 5 seconds");
            } else if e.is_connect() {
                eprintln!("[fc-agent]   CONNECTION ERROR: Cannot reach 169.254.169.254");
            }
            return Err(e).context("requesting MMDS session token");
        }
    };

    let token_status = token_response.status();
    eprintln!(
        "[fc-agent] token response status: {} {}",
        token_status.as_u16(),
        token_status.canonical_reason().unwrap_or("")
    );

    let token = token_response
        .text()
        .await
        .context("reading session token")?;
    eprintln!(
        "[fc-agent] got token: {} bytes ({})",
        token.len(),
        if token.is_empty() { "EMPTY!" } else { "ok" }
    );

    eprintln!("[fc-agent] fetching plan from http://169.254.169.254/latest/container-plan");
    let plan_response = match client
        .get("http://169.254.169.254/latest/container-plan")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            eprintln!("[fc-agent] plan request succeeded");
            resp
        }
        Err(e) => {
            eprintln!("[fc-agent] plan request FAILED: {:?}", e);
            return Err(e).context("fetching from MMDS");
        }
    };

    let plan_status = plan_response.status();
    eprintln!(
        "[fc-agent] plan response status: {} {}",
        plan_status.as_u16(),
        plan_status.canonical_reason().unwrap_or("")
    );

    if !plan_status.is_success() {
        eprintln!(
            "[fc-agent] ERROR: HTTP {} - this is NOT a 2xx success code",
            plan_status.as_u16()
        );
    }

    let body = plan_response.text().await.context("reading plan body")?;
    eprintln!(
        "[fc-agent] plan response body ({} bytes): {}",
        body.len(),
        body
    );

    let plan: Plan = match serde_json::from_str(&body) {
        Ok(p) => {
            eprintln!("[fc-agent] successfully parsed JSON into Plan struct");
            p
        }
        Err(e) => {
            eprintln!("[fc-agent] JSON PARSING FAILED:");
            eprintln!("[fc-agent]   parse error: {}", e);
            eprintln!("[fc-agent]   body was: {}", body);
            return Err(e.into());
        }
    };

    Ok(plan)
}

async fn fetch_latest_metadata(client: &reqwest::Client) -> Result<LatestMetadata> {
    let token_response = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_millis(500))
        .send()
        .await?;
    let token = token_response.text().await?;

    let response = client
        .get("http://169.254.169.254/latest")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(500))
        .send()
        .await?;

    let body = response.text().await?;
    let metadata: LatestMetadata = serde_json::from_str(&body)?;
    Ok(metadata)
}

/// Watch for restore-epoch changes in MMDS and handle clone restore.
pub async fn watch_restore_epoch(boot_volumes: Vec<VolumeMount>, output: OutputHandle) {
    let mut last_epoch: Option<String> = None;

    loop {
        sleep(Duration::from_millis(100)).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let metadata = match fetch_latest_metadata(&client).await {
            Ok(m) => m,
            Err(_) => continue,
        };

        if let Some(ref current) = metadata.restore_epoch {
            match &last_epoch {
                None => {
                    eprintln!(
                        "[fc-agent] detected restore-epoch: {} (clone restore detected, volumes: {})",
                        current,
                        boot_volumes.len()
                    );
                    crate::restore::handle_clone_restore(&boot_volumes, &output).await;
                    last_epoch = metadata.restore_epoch;
                }
                Some(prev) if prev != current => {
                    eprintln!(
                        "[fc-agent] restore-epoch changed: {} -> {} (volumes: {})",
                        prev,
                        current,
                        boot_volumes.len()
                    );
                    crate::restore::handle_clone_restore(&boot_volumes, &output).await;
                    last_epoch = metadata.restore_epoch;
                }
                _ => {}
            }
        }
    }
}

/// Sync VM clock from host time via MMDS.
pub async fn sync_clock_from_host() -> Result<()> {
    eprintln!("[fc-agent] syncing VM clock from host time via MMDS");

    let client = reqwest::Client::new();

    let token_response = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("getting MMDS token for time sync")?;

    let token = token_response.text().await?;

    let metadata_response = client
        .get("http://169.254.169.254/latest")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("fetching host-time from MMDS")?;

    let body = metadata_response.text().await?;
    let metadata: LatestMetadata =
        serde_json::from_str(&body).context("parsing host-time from MMDS")?;

    eprintln!("[fc-agent] received host time: {}", metadata.host_time);

    let output = Command::new("date")
        .arg("-u")
        .arg("-s")
        .arg(format!("@{}", metadata.host_time))
        .output()
        .await
        .context("setting system clock")?;

    if !output.status.success() {
        eprintln!(
            "[fc-agent] WARNING: failed to set clock: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    } else {
        eprintln!("[fc-agent] system clock synchronized from host");
    }

    Ok(())
}
