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

async fn fetch_plan() -> Result<Plan> {
    let url = std::env::var("FC_MMDS_URI").unwrap_or_else(|_| "http://169.254.169.254/".to_string());
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
    eprintln!("[fc-agent] starting");

    // Wait for MMDS to be ready
    let plan = loop {
        match fetch_plan().await {
            Ok(p) => {
                eprintln!("[fc-agent] received container plan");
                break p;
            }
            Err(e) => {
                eprintln!("[fc-agent] MMDS not ready: {}", e);
                sleep(Duration::from_millis(500)).await;
            }
        }
    };

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
