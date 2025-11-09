use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Stdio;
use tokio::{io::{AsyncBufReadExt, BufReader}, process::Command, time::{sleep, Duration}};

#[derive(Debug, Deserialize)]
struct Plan {
    image: String,
    #[allow(dead_code)]
    env: Option<serde_json::Value>,
    #[allow(dead_code)]
    cmd: Option<Vec<String>>,
    #[allow(dead_code)]
    logs: Option<serde_json::Value>,
    #[allow(dead_code)]
    readiness: Option<serde_json::Value>,
    #[allow(dead_code)]
    podman: Option<serde_json::Value>,
    #[allow(dead_code)]
    volumes: Option<serde_json::Value>,
}

async fn fetch_plan() -> Result<Plan> {
    let url = std::env::var("FC_MMDs_URI").unwrap_or_else(|_| "http://169.254.169.254/".to_string());
    let plan: Plan = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .json()
        .await
        .context("parsing MMDS plan")?;
    Ok(plan)
}

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("[agent] starting");
    let plan = loop {
        match fetch_plan().await {
            Ok(p) => break p,
            Err(e) => { eprintln!("[agent] MMDS not ready: {e}"); sleep(Duration::from_millis(500)).await; }
        }
    };

    let mut cmd = Command::new("podman");
    cmd.arg("run").arg("--rm").arg("--network=host");
    cmd.arg(&plan.image);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

    // Stream logs to serial
    if let Some(out) = child.stdout.take() {
        let mut r = BufReader::new(out).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = r.next_line().await {
                println!("[ctr] {line}");
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let mut r = BufReader::new(err).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = r.next_line().await {
                eprintln!("[ctr] {line}");
            }
        });
    }

    let status = child.wait().await?;
    eprintln!("[agent] container exited with {status}");
    Ok(())
}
