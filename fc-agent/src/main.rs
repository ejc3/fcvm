mod fuse;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Stdio;
use std::thread;
use fs2::FileExt;
use tokio::{io::{AsyncBufReadExt, BufReader}, process::Command, time::{sleep, Duration}};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Deserialize)]
struct Plan {
    image: String,
    #[serde(default)]
    env: HashMap<String, String>,
    cmd: Option<Vec<String>>,
    /// Volume mounts from host (FUSE-over-vsock)
    #[serde(default)]
    volumes: Vec<VolumeMount>,
}

/// Volume mount configuration from MMDS
#[derive(Debug, Clone, Deserialize)]
struct VolumeMount {
    /// Mount path inside guest
    guest_path: String,
    /// Vsock port to connect to host VolumeServer
    vsock_port: u32,
    /// Read-only flag
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Deserialize)]
struct LatestMetadata {
    #[serde(rename = "host-time")]
    host_time: String,
    #[serde(rename = "restore-epoch")]
    restore_epoch: Option<String>,
    /// Volume mounts for clone restore (provided by fcvm snapshot run)
    #[serde(default)]
    volumes: Vec<VolumeMount>,
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

/// Watch for restore-epoch changes in MMDS and handle clone restore
/// This runs as a background task to handle snapshot restore scenarios
async fn watch_restore_epoch() {
    let mut last_epoch: Option<String> = None;

    // Poll every 100ms - simple and fast enough to detect restores quickly
    // The CPU overhead is negligible (~0.1% of one core)
    loop {
        sleep(Duration::from_millis(100)).await;

        // Create a fresh client each time to handle snapshot restore
        // (TCP connections are invalidated after snapshot restore)
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        // Try to fetch current metadata (including restore-epoch and volumes)
        let metadata = match fetch_latest_metadata(&client).await {
            Ok(m) => m,
            Err(_) => continue, // Ignore errors, just keep polling
        };

        // Check if epoch changed or if this is the first time we see one
        if let Some(ref current) = metadata.restore_epoch {
            match &last_epoch {
                None => {
                    // First time seeing an epoch - THIS IS A CLONE RESTORE!
                    // On fresh boot, there is no restore-epoch in MMDS yet.
                    // If we see one, we were restored from a snapshot.
                    eprintln!("[fc-agent] detected restore-epoch: {} (clone restore detected)", current);
                    handle_clone_restore(&metadata.volumes).await;
                    last_epoch = metadata.restore_epoch;
                }
                Some(prev) if prev != current => {
                    // Epoch changed! This means we were restored from snapshot again
                    eprintln!("[fc-agent] restore-epoch changed: {} -> {}", prev, current);
                    handle_clone_restore(&metadata.volumes).await;
                    last_epoch = metadata.restore_epoch;
                }
                _ => {
                    // No change
                }
            }
        }
    }
}

/// Handle clone restore: flush ARP and remount volumes
async fn handle_clone_restore(volumes: &[VolumeMount]) {
    // 1. Flush ARP cache (network connections are broken after restore)
    flush_arp_cache().await;

    // 2. Remount FUSE volumes if any
    if !volumes.is_empty() {
        eprintln!("[fc-agent] clone has {} volume(s) to remount", volumes.len());
        remount_fuse_volumes(volumes).await;
    }
}

/// Remount FUSE volumes after clone restore.
/// The old vsock connections are broken, so we unmount and remount.
async fn remount_fuse_volumes(volumes: &[VolumeMount]) {
    for vol in volumes {
        eprintln!("[fc-agent] remounting volume at {} (port {})", vol.guest_path, vol.vsock_port);

        // First, try to unmount the old (broken) FUSE mount
        // Use lazy unmount (-l) in case there are open files
        let umount_output = Command::new("umount")
            .args(["-l", &vol.guest_path])
            .output()
            .await;

        match umount_output {
            Ok(o) if o.status.success() => {
                eprintln!("[fc-agent] unmounted old FUSE mount at {}", vol.guest_path);
            }
            Ok(o) => {
                // Not mounted or error - that's fine, we'll mount fresh
                eprintln!("[fc-agent] umount {} (may not be mounted): {}",
                    vol.guest_path, String::from_utf8_lossy(&o.stderr).trim());
            }
            Err(e) => {
                eprintln!("[fc-agent] umount error for {}: {}", vol.guest_path, e);
            }
        }

        // Small delay to ensure unmount completes
        sleep(Duration::from_millis(100)).await;

        // Create mount point directory (in case it doesn't exist)
        if let Err(e) = std::fs::create_dir_all(&vol.guest_path) {
            eprintln!("[fc-agent] ERROR: cannot create mount point {}: {}", vol.guest_path, e);
            continue;
        }

        // Mount FUSE filesystem in a background thread using fuse-pipe
        let mount_path = vol.guest_path.clone();
        let port = vol.vsock_port;

        thread::spawn(move || {
            eprintln!("[fc-agent] fuse: starting remount at {}", mount_path);
            if let Err(e) = fuse::mount_vsock(port, &mount_path) {
                eprintln!("[fc-agent] FUSE remount error at {}: {}", mount_path, e);
            }
            eprintln!("[fc-agent] fuse: remount at {} exited", mount_path);
        });

        eprintln!("[fc-agent] volume {} remount initiated", vol.guest_path);
    }

    // Give FUSE mounts time to initialize
    if !volumes.is_empty() {
        eprintln!("[fc-agent] waiting for FUSE remounts to initialize...");
        sleep(Duration::from_millis(500)).await;
        eprintln!("[fc-agent] ✓ volume remounts complete");
    }
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

async fn flush_arp_cache() {
    let output = Command::new("ip")
        .args(["neigh", "flush", "all"])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] ✓ ARP cache flushed successfully");
        }
        Ok(o) => {
            eprintln!("[fc-agent] WARNING: ARP flush failed: {}", String::from_utf8_lossy(&o.stderr));
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: ARP flush error: {}", e);
        }
    }
}

/// Watch for lock test trigger file and run lock tests when it appears
/// The trigger file contains the number of iterations to run
/// This runs in clones that have a shared volume mounted at /mnt/shared
async fn watch_for_lock_test(clone_id: String) {
    let trigger_path = "/mnt/shared/run-lock-test";
    let counter_path = "/mnt/shared/counter.txt";
    let append_path = "/mnt/shared/append.log";

    eprintln!("[fc-agent] watching for lock test trigger at {}", trigger_path);

    // Poll for trigger file
    loop {
        sleep(Duration::from_millis(500)).await;

        // Check if trigger file exists
        if std::path::Path::new(trigger_path).exists() {
            // Read iterations count
            let iterations: usize = match std::fs::read_to_string(trigger_path) {
                Ok(content) => content.trim().parse().unwrap_or(100),
                Err(_) => continue,
            };

            eprintln!("[fc-agent] lock test triggered! clone={} iterations={}", clone_id, iterations);

            // Run lock tests
            run_lock_tests(&clone_id, iterations, counter_path, append_path);

            // Write done file
            let done_path = format!("/mnt/shared/done-{}", clone_id);
            if let Err(e) = std::fs::write(&done_path, "done") {
                eprintln!("[fc-agent] ERROR writing done file: {}", e);
            } else {
                eprintln!("[fc-agent] ✓ lock test complete, wrote {}", done_path);
            }

            // Only run once per trigger
            break;
        }
    }
}

/// Run lock tests: counter increment + append to file
/// Uses POSIX file locking (flock) to ensure no corruption
fn run_lock_tests(clone_id: &str, iterations: usize, counter_path: &str, append_path: &str) {
    eprintln!("[fc-agent] running {} lock iterations", iterations);

    for i in 0..iterations {
        // Test 1: Counter increment with lock
        if let Err(e) = increment_counter_with_lock(counter_path) {
            eprintln!("[fc-agent] ERROR incrementing counter (iter {}): {}", i, e);
        }

        // Test 2: Append to log with lock
        if let Err(e) = append_with_lock(append_path, clone_id, i) {
            eprintln!("[fc-agent] ERROR appending to log (iter {}): {}", i, e);
        }

        // Small delay between iterations to increase chance of contention
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    eprintln!("[fc-agent] completed {} lock iterations", iterations);
}

/// Increment a counter file with exclusive lock
/// Uses flock for POSIX advisory locking
fn increment_counter_with_lock(path: &str) -> Result<()> {
    // Open file for read+write
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("opening counter file")?;

    // Acquire exclusive lock (blocking)
    file.lock_exclusive().context("acquiring exclusive lock on counter")?;

    // Read current value
    let mut content = String::new();
    file.read_to_string(&mut content).context("reading counter")?;
    let current: i64 = content.trim().parse().unwrap_or(0);

    // Increment
    let new_value = current + 1;

    // Write new value (truncate and rewrite)
    file.seek(SeekFrom::Start(0)).context("seeking to start")?;
    file.set_len(0).context("truncating file")?;
    write!(file, "{}", new_value).context("writing new counter value")?;
    file.sync_all().context("syncing counter file")?;

    // Lock is automatically released when file is dropped
    Ok(())
}

/// Append a line to a log file with exclusive lock
fn append_with_lock(path: &str, clone_id: &str, iteration: usize) -> Result<()> {
    // Open file for append
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("opening append file")?;

    // Acquire exclusive lock (blocking)
    file.lock_exclusive().context("acquiring exclusive lock on append file")?;

    // Write line with clone ID, iteration, and timestamp
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let line = format!("{}:{}:{}\n", clone_id, iteration, timestamp);

    // Use BufWriter for atomic-ish write
    let mut writer = std::io::BufWriter::new(&file);
    writer.write_all(line.as_bytes()).context("writing append line")?;
    writer.flush().context("flushing append file")?;

    // Lock is automatically released when file is dropped
    Ok(())
}

/// Extract clone ID from MMDS or hostname
/// Clones are named "clone-lock-{N}" so we extract the number
async fn get_clone_id() -> String {
    // Try to get from hostname first
    if let Ok(output) = Command::new("hostname").output().await {
        let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // Clone VMs have names like "clone-lock-0", extract just the number
        if hostname.starts_with("clone-lock-") {
            if let Some(id) = hostname.strip_prefix("clone-lock-") {
                return id.to_string();
            }
        }
        // Return hostname if it looks like a clone ID
        if hostname.chars().all(|c| c.is_ascii_digit()) {
            return hostname;
        }
    }

    // Fallback: use process ID as clone ID (unique per VM)
    std::process::id().to_string()
}

/// Mount FUSE volumes from host via vsock.
/// Returns list of mount points that need to be cleaned up on exit.
fn mount_fuse_volumes(volumes: &[VolumeMount]) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();

    for vol in volumes {
        eprintln!("[fc-agent] mounting FUSE volume at {} via vsock port {}",
            vol.guest_path, vol.vsock_port);

        // Create mount point directory
        std::fs::create_dir_all(&vol.guest_path)
            .with_context(|| format!("creating mount point: {}", vol.guest_path))?;

        // Mount FUSE filesystem in a background thread using fuse-pipe
        // fuse-pipe's mount_vsock blocks, so we run it in a dedicated thread
        let mount_path = vol.guest_path.clone();
        let port = vol.vsock_port;

        thread::spawn(move || {
            eprintln!("[fc-agent] fuse: starting mount at {}", mount_path);
            if let Err(e) = fuse::mount_vsock(port, &mount_path) {
                eprintln!("[fc-agent] FUSE mount error at {}: {}", mount_path, e);
            }
            eprintln!("[fc-agent] fuse: mount at {} exited", mount_path);
        });

        mounted_paths.push(vol.guest_path.clone());
    }

    // Give FUSE mounts time to initialize
    if !volumes.is_empty() {
        eprintln!("[fc-agent] waiting for FUSE mounts to initialize...");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    Ok(mounted_paths)
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
    // Initialize tracing (fuse-pipe uses tracing for logging)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,fuse_pipe=debug"))
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

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

    // Start background task to watch for restore-epoch changes
    // This handles ARP cache flushing when VM is restored from snapshot
    tokio::spawn(async {
        eprintln!("[fc-agent] starting restore-epoch watcher for ARP flush");
        watch_restore_epoch().await;
    });

    // Mount FUSE volumes from host before launching container
    // Note: mounted_volumes tracks which mounts succeeded, but we bind from plan.volumes
    // since they use the same guest_path for both FUSE mount and container bind
    let has_shared_volume = if !plan.volumes.is_empty() {
        eprintln!("[fc-agent] mounting {} FUSE volume(s) from host", plan.volumes.len());
        match mount_fuse_volumes(&plan.volumes) {
            Ok(paths) => {
                eprintln!("[fc-agent] ✓ FUSE volumes mounted successfully");
                // Check if we have a /mnt/shared volume for lock testing
                paths.iter().any(|p| p == "/mnt/shared")
            }
            Err(e) => {
                eprintln!("[fc-agent] ERROR: failed to mount FUSE volumes: {:?}", e);
                // Continue without volumes - container can still run
                false
            }
        }
    } else {
        false
    };

    // If we have a shared volume, start lock test watcher
    // This allows clones to run POSIX lock tests on demand
    if has_shared_volume {
        let clone_id = get_clone_id().await;
        eprintln!("[fc-agent] starting lock test watcher (clone_id={})", clone_id);
        tokio::spawn(async move {
            watch_for_lock_test(clone_id).await;
        });
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

    // Add FUSE-mounted volumes as bind mounts to container
    // The FUSE mount is already at guest_path, so we bind it to same path in container
    for vol in &plan.volumes {
        let mount_spec = if vol.read_only {
            format!("{}:{}:ro", vol.guest_path, vol.guest_path)
        } else {
            format!("{}:{}", vol.guest_path, vol.guest_path)
        };
        cmd.arg("-v").arg(mount_spec);
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
