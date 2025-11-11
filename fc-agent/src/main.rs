use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::{io::{AsyncBufReadExt, BufReader}, process::Command, time::{sleep, Duration}};

#[derive(Debug, Deserialize)]
struct Plan {
    image: String,
    #[serde(default)]
    env: HashMap<String, String>,
    cmd: Option<Vec<String>>,
    #[serde(default)]
    volumes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LatestMetadata {
    #[serde(rename = "host-time")]
    host_time: String,
}

async fn fetch_plan() -> Result<Plan> {
    // MMDS V2 requires getting a session token first
    let client = reqwest::Client::new();

    // Step 1: Get session token
    eprintln!("[fc-agent] requesting MMDS V2 session token from http://169.254.169.254/latest/api/token");
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
                eprintln!("[fc-agent]   → TIMEOUT: MMDS not responding within 5 seconds");
            } else if e.is_connect() {
                eprintln!("[fc-agent]   → CONNECTION ERROR: Cannot reach 169.254.169.254");
            } else if e.is_request() {
                eprintln!("[fc-agent]   → REQUEST ERROR: Problem building request");
            }
            return Err(e).context("requesting MMDS session token");
        }
    };

    let token_status = token_response.status();
    eprintln!("[fc-agent] token response status: {} {}", token_status.as_u16(), token_status.canonical_reason().unwrap_or(""));

    let token = token_response.text().await.context("reading session token")?;
    eprintln!("[fc-agent] got token: {} bytes ({})", token.len(), if token.is_empty() { "EMPTY!" } else { "ok" });

    // Step 2: Fetch plan with token from /latest/container-plan
    // IMPORTANT: Must include Accept: application/json to get JSON response instead of IMDS key list
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
            eprintln!("[fc-agent] plan request FAILED - detailed error:");
            eprintln!("[fc-agent]   error type: {:?}", e);
            if e.is_timeout() {
                eprintln!("[fc-agent]   → TIMEOUT: MMDS not responding within 5 seconds");
            } else if e.is_connect() {
                eprintln!("[fc-agent]   → CONNECTION ERROR: Cannot reach 169.254.169.254");
            } else if e.is_request() {
                eprintln!("[fc-agent]   → REQUEST ERROR: Problem building request");
            }
            return Err(e).context("fetching from MMDS");
        }
    };

    let plan_status = plan_response.status();
    eprintln!("[fc-agent] plan response status: {} {}", plan_status.as_u16(), plan_status.canonical_reason().unwrap_or(""));

    if !plan_status.is_success() {
        eprintln!("[fc-agent] ERROR: HTTP {} - this is NOT a 2xx success code", plan_status.as_u16());
    }

    let body = plan_response.text().await.context("reading plan body")?;
    eprintln!("[fc-agent] plan response body ({} bytes): {}", body.len(), body);

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

/// Sync VM clock from host time provided via MMDS
/// This avoids the need to wait for slow NTP synchronization
async fn sync_clock_from_host() -> Result<()> {
    eprintln!("[fc-agent] syncing VM clock from host time via MMDS");

    let client = reqwest::Client::new();

    // Get session token
    let token_response = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("getting MMDS token for time sync")?;

    let token = token_response.text().await?;

    // Fetch host-time from /latest
    let metadata_response = client
        .get("http://169.254.169.254/latest")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("fetching host-time from MMDS")?;

    let body = metadata_response.text().await?;
    let metadata: LatestMetadata = serde_json::from_str(&body)
        .context("parsing host-time from MMDS")?;

    eprintln!("[fc-agent] received host time: {}", metadata.host_time);

    // Set system clock using `date` command with Unix timestamp
    // Format: @1731301800 (seconds since epoch)
    // BusyBox date supports this with -s @TIMESTAMP
    let output = Command::new("date")
        .arg("-u")
        .arg("-s")
        .arg(format!("@{}", metadata.host_time))
        .output()
        .await
        .context("setting system clock")?;

    if !output.status.success() {
        eprintln!("[fc-agent] WARNING: failed to set clock: {}", String::from_utf8_lossy(&output.stderr));
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    } else {
        eprintln!("[fc-agent] ✓ system clock synchronized from host");
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("[fc-agent] starting");

    // Wait for MMDS to be ready
    let plan = loop {
        match fetch_plan().await {
            Ok(p) => {
                eprintln!("[fc-agent] ✓ received container plan successfully");
                break p;
            }
            Err(e) => {
                eprintln!("[fc-agent] MMDS not ready - full error chain:");
                eprintln!("[fc-agent]   {:?}", e);
                eprintln!("[fc-agent] retrying in 500ms...");
                sleep(Duration::from_millis(500)).await;
            }
        }
    };

    // Sync VM clock from host before launching container
    // This ensures TLS certificate validation works immediately
    if let Err(e) = sync_clock_from_host().await {
        eprintln!("[fc-agent] WARNING: clock sync failed: {:?}", e);
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    }

    eprintln!("[fc-agent] launching container: {}", plan.image);

    // Build Podman command
    let mut cmd = Command::new("podman");
    cmd.arg("run")
        .arg("--rm")
        .arg("--network=host");

    // Add environment variables
    for (key, val) in &plan.env {
        cmd.arg("-e").arg(format!("{}={}", key, val));
    }

    // Add volume mounts
    for vol in &plan.volumes {
        cmd.arg("-v").arg(vol);
    }

    // Image
    cmd.arg(&plan.image);

    // Command override
    if let Some(cmd_args) = &plan.cmd {
        cmd.args(cmd_args);
    }

    // Spawn container
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()
        .context("spawning Podman container")?;

    // Stream stdout to serial console
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("[ctr:out] {}", line);
            }
        });
    }

    // Stream stderr to serial console
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[ctr:err] {}", line);
            }
        });
    }

    // Wait for container to exit
    let status = child.wait().await?;

    if status.success() {
        eprintln!("[fc-agent] container exited successfully");
        Ok(())
    } else {
        eprintln!("[fc-agent] container exited with error: {}", status);
        std::process::exit(status.code().unwrap_or(1))
    }
}
