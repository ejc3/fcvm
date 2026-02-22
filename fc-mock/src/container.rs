//! Container lifecycle management.
//!
//! Launches a podman container based on the MMDS container-plan,
//! connects to the vsock status/output sockets, and manages the
//! container lifecycle.

use anyhow::{bail, Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Container name used by fcvm's health monitor (hardcoded in health.rs)
const CONTAINER_NAME: &str = "fcvm-container";

/// Network config parsed from kernel boot args.
struct NetworkConfig {
    guest_ip: String,
    netmask: String,
    gateway: String,
    dns_servers: Vec<String>,
}

/// Launch a container from the MMDS plan and manage its lifecycle.
///
/// 1. Parse MMDS container-plan
/// 2. Configure namespace networking (IP on br0, default route)
/// 3. Start vsock exec listener
/// 4. Launch container via podman with --network host
/// 5. Connect to status socket → send "ready"
/// 6. Connect to output socket → forward stdout/stderr
/// 7. Wait for container exit → send "exit:{code}"
/// 8. Signal shutdown so fc-mock exits
pub async fn launch_container(
    mmds_data: Option<serde_json::Value>,
    vsock_uds_path: Option<String>,
    boot_args: Option<String>,
    shutdown: Option<Arc<Notify>>,
) -> Result<()> {
    let mmds = mmds_data.context("no MMDS data stored (PUT /mmds not called)")?;
    let vsock_path = vsock_uds_path.context("no vsock config (PUT /vsock not called)")?;

    // Parse container plan from MMDS
    let plan = &mmds["latest"]["container-plan"];
    if plan.is_null() {
        bail!("MMDS data has no latest.container-plan");
    }

    let image = plan["image"]
        .as_str()
        .context("container-plan missing 'image'")?;

    let cmd: Option<Vec<String>> = plan["cmd"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let env: Vec<(String, String)> = plan["env"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();

    let privileged = plan["privileged"].as_bool().unwrap_or(false);
    let tty = plan["tty"].as_bool().unwrap_or(false);
    let user = plan["user"].as_str().map(String::from);

    info!(
        image = %image,
        cmd = ?cmd,
        env_count = env.len(),
        privileged,
        tty,
        "parsed container plan"
    );

    // Parse boot args and configure namespace networking
    let net_config = boot_args.as_deref().and_then(parse_boot_args);
    if let Some(ref nc) = net_config {
        if let Err(e) = configure_namespace_networking(nc).await {
            warn!(
                "failed to configure namespace networking: {} (container may lack internet)",
                e
            );
        }
    }

    // Start vsock exec listener BEFORE anything else so health checks can connect
    let exec_handle = crate::vsock_exec::start_exec_listener(&vsock_path).await?;

    // Remove any existing container with the same name
    cleanup_container().await;

    // Build podman command
    let mut podman_args = vec![
        "run".to_string(),
        "--name".to_string(),
        CONTAINER_NAME.to_string(),
        "--rm".to_string(),
        // Share the namespace's network stack (mirrors fc-agent's --network=host)
        "--network".to_string(),
        "host".to_string(),
    ];

    // DNS from boot args
    if let Some(ref nc) = net_config {
        for dns in &nc.dns_servers {
            podman_args.push("--dns".to_string());
            podman_args.push(dns.clone());
        }
    }

    // Rootless storage fix: use overlay+fuse-overlayfs on tmpfs to avoid btrfs
    // remount failure. fuse-overlayfs works in user namespaces and handles chown errors.
    let storage_args = rootless_storage_args();
    podman_args.extend(storage_args.clone());

    for (key, val) in &env {
        podman_args.push("-e".to_string());
        podman_args.push(format!("{}={}", key, val));
    }

    if privileged {
        podman_args.push("--privileged".to_string());
    }

    if tty {
        podman_args.push("-t".to_string());
    }

    if let Some(ref u) = user {
        podman_args.push("--user".to_string());
        podman_args.push(u.clone());
    }

    podman_args.push(image.to_string());

    if let Some(ref cmd_args) = cmd {
        podman_args.extend(cmd_args.clone());
    }

    info!(args = ?podman_args, "launching container");

    // Spawn podman with piped stdout/stderr
    let mut cmd = tokio::process::Command::new("podman");
    cmd.args(&podman_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // In user namespaces, HOME and XDG_RUNTIME_DIR may be inaccessible.
    apply_user_ns_env(&mut cmd);

    let mut child = cmd.spawn().context("spawning podman")?;

    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    // Connect to status socket (created by fcvm's run_status_listener)
    let status_socket_path = format!("{}_4999", vsock_path);
    let output_socket_path = format!("{}_4997", vsock_path);

    // Retry connecting to status socket (it might not be ready yet)
    let status_stream = connect_with_retry(&status_socket_path, 50, 100)
        .await
        .context("connecting to status socket")?;
    info!("connected to status socket");

    // Send "ready" immediately (container process is running)
    let mut status_writer = status_stream;
    status_writer
        .write_all(b"ready\n")
        .await
        .context("sending ready")?;
    status_writer.flush().await?;
    info!("sent ready to status socket");

    // Connect to output socket and forward container output
    let output_stream = connect_with_retry(&output_socket_path, 50, 100).await;
    let output_writer = match output_stream {
        Ok(s) => {
            info!("connected to output socket");
            Some(Arc::new(tokio::sync::Mutex::new(s)))
        }
        Err(e) => {
            warn!(
                "could not connect to output socket: {} (output won't be forwarded)",
                e
            );
            None
        }
    };

    // Spawn stdout forwarder
    let out_writer = output_writer.clone();
    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(ref w) = out_writer {
                let msg = format!("stdout:{}\n", line);
                let mut w = w.lock().await;
                let _ = w.write_all(msg.as_bytes()).await;
            }
        }
    });

    // Spawn stderr forwarder
    let err_writer = output_writer.clone();
    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(ref w) = err_writer {
                let msg = format!("stderr:{}\n", line);
                let mut w = w.lock().await;
                let _ = w.write_all(msg.as_bytes()).await;
            }
        }
    });

    // Wait for container to exit
    let exit_status = child.wait().await.context("waiting for container")?;
    let exit_code = exit_status.code().unwrap_or(1);

    info!(exit_code, "container exited");

    // Send exit code to status socket
    let exit_msg = format!("exit:{}\n", exit_code);
    let _ = status_writer.write_all(exit_msg.as_bytes()).await;
    let _ = status_writer.flush().await;

    // Wait for output tasks to finish
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = stdout_task.await;
        let _ = stderr_task.await;
    })
    .await;

    // Abort the exec listener
    exec_handle.abort();

    // Signal shutdown so fc-mock exits (like real Firecracker after PSCI shutdown)
    if let Some(shutdown) = shutdown {
        shutdown.notify_one();
    }

    Ok(())
}

/// Parse kernel boot args for network configuration.
///
/// Extracts:
/// - `ip=GUEST_IP::GATEWAY:NETMASK::eth0:off` → guest_ip, gateway
/// - `fcvm_dns=X.X.X.X|Y.Y.Y.Y` → DNS servers
fn parse_boot_args(boot_args: &str) -> Option<NetworkConfig> {
    let mut guest_ip = None;
    let mut netmask = None;
    let mut gateway = None;
    let mut dns_servers = Vec::new();

    for arg in boot_args.split_whitespace() {
        if let Some(ip_arg) = arg.strip_prefix("ip=") {
            // Format: ip=CLIENT::GATEWAY:NETMASK::DEVICE:AUTOCONF
            let parts: Vec<&str> = ip_arg.split(':').collect();
            if !parts.is_empty() && !parts[0].is_empty() {
                guest_ip = Some(parts[0].to_string());
            }
            // Gateway is after two colons (empty server-ip field)
            // Parts: [CLIENT, "", GATEWAY, NETMASK, "", DEVICE, AUTOCONF]
            if parts.len() >= 3 && !parts[2].is_empty() {
                gateway = Some(parts[2].to_string());
            }
            if parts.len() >= 4 && !parts[3].is_empty() {
                netmask = Some(parts[3].to_string());
            }
        } else if let Some(dns_arg) = arg.strip_prefix("fcvm_dns=") {
            // Format: fcvm_dns=X.X.X.X|Y.Y.Y.Y (pipe-separated)
            dns_servers = dns_arg.split('|').map(String::from).collect();
        }
    }

    match (guest_ip, gateway) {
        (Some(ip), Some(gw)) => {
            let mask = netmask.unwrap_or_else(|| "255.255.255.0".to_string());
            info!(guest_ip = %ip, netmask = %mask, gateway = %gw, dns = ?dns_servers, "parsed network config from boot args");
            Some(NetworkConfig {
                guest_ip: ip,
                netmask: mask,
                gateway: gw,
                dns_servers,
            })
        }
        _ => {
            debug!("no network config found in boot args");
            None
        }
    }
}

/// Configure the namespace's network so containers can use it.
///
/// Assigns guest IP to br0 and adds a default route via the gateway.
/// This makes the namespace's network stack usable with --network host.
async fn configure_namespace_networking(config: &NetworkConfig) -> Result<()> {
    let prefix_len = netmask_to_prefix(&config.netmask);

    // Assign guest IP to br0
    let output = tokio::process::Command::new("ip")
        .args([
            "addr",
            "add",
            &format!("{}/{}", config.guest_ip, prefix_len),
            "dev",
            "br0",
        ])
        .output()
        .await
        .context("running ip addr add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // RTNETLINK: File exists means IP already assigned — that's fine
        if !stderr.contains("File exists") {
            bail!("ip addr add failed: {}", stderr.trim());
        }
    }
    info!(ip = %config.guest_ip, "assigned IP to br0");

    // Add default route via gateway
    let output = tokio::process::Command::new("ip")
        .args([
            "route",
            "add",
            "default",
            "via",
            &config.gateway,
            "dev",
            "br0",
        ])
        .output()
        .await
        .context("running ip route add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("File exists") {
            bail!("ip route add failed: {}", stderr.trim());
        }
    }
    info!(gateway = %config.gateway, "added default route");

    // Fix DNS: In network namespaces, /etc/resolv.conf often points to
    // 127.0.0.53 (systemd-resolved) which is unreachable. If we have DNS
    // servers from boot args and are in a mount namespace (set up in main),
    // bind-mount a custom resolv.conf so podman can resolve registry names.
    if !config.dns_servers.is_empty() {
        fix_namespace_dns(&config.dns_servers);
    }

    Ok(())
}

/// Fix DNS in the namespace by bind-mounting a custom resolv.conf.
///
/// In network namespaces, /etc/resolv.conf typically points to 127.0.0.53
/// (systemd-resolved) which only listens in the initial network namespace.
/// This replaces it with the real DNS servers from boot args.
///
/// Requires a private mount namespace (set up in main.rs before tokio).
fn fix_namespace_dns(dns_servers: &[String]) {
    // Check if fix is needed
    if let Ok(content) = std::fs::read_to_string("/etc/resolv.conf") {
        if !content.contains("127.0.0.53") {
            debug!("resolv.conf doesn't use systemd-resolved stub, no fix needed");
            return;
        }
    }

    // Write custom resolv.conf
    let resolv_path = "/tmp/fc-mock-resolv.conf";
    let content: String = dns_servers
        .iter()
        .map(|s| format!("nameserver {}", s))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    if let Err(e) = std::fs::write(resolv_path, &content) {
        warn!("failed to write custom resolv.conf: {}", e);
        return;
    }

    // Bind mount over /etc/resolv.conf
    let source = std::ffi::CString::new(resolv_path).unwrap();
    let target = std::ffi::CString::new("/etc/resolv.conf").unwrap();
    let ret = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };

    if ret != 0 {
        warn!(
            "failed to bind mount resolv.conf: {} (DNS may not work for image pulls)",
            std::io::Error::last_os_error()
        );
    } else {
        info!(dns = ?dns_servers, "bind-mounted custom resolv.conf for namespace DNS");
    }
}

/// Convert a dotted-decimal netmask to CIDR prefix length.
/// Falls back to 24 if the netmask can't be parsed.
fn netmask_to_prefix(netmask: &str) -> u32 {
    netmask
        .parse::<std::net::Ipv4Addr>()
        .map(|ip| u32::from(ip).count_ones())
        .unwrap_or(24)
}

/// Return extra podman args for rootless storage if in a user namespace.
///
/// In user namespaces, podman can't remount btrfs storage and native overlay
/// fails. Use overlay driver with fuse-overlayfs on tmpfs instead.
/// fuse-overlayfs works in user namespaces and the overlay driver properly
/// handles chown errors during image layer extraction (unlike vfs).
pub fn rootless_storage_args() -> Vec<String> {
    if !in_user_namespace() {
        return Vec::new();
    }
    let storage_root = format!("/tmp/fc-mock-podman-{}", std::process::id());
    debug!(storage_root = %storage_root, "using overlay+fuse-overlayfs storage (user namespace)");
    vec![
        "--root".to_string(),
        storage_root,
        "--storage-driver".to_string(),
        "overlay".to_string(),
        "--storage-opt".to_string(),
        "overlay.mount_program=/usr/bin/fuse-overlayfs".to_string(),
        // In user namespaces with limited UID/GID mappings (e.g., only 0→0),
        // image layer extraction fails on lchown for unmapped IDs (like GID 42).
        // This tells the overlay driver to silently ignore chown errors.
        "--storage-opt".to_string(),
        "overlay.ignore_chown_errors=true".to_string(),
    ]
}

/// Apply environment fixes needed for podman in user namespaces.
///
/// In user namespaces, HOME and XDG_RUNTIME_DIR may point to directories
/// with restrictive permissions (e.g., /home/ubuntu with 750 or /run/user/1000),
/// causing podman to fail reading config and auth files.
///
/// Also sets _CONTAINERS_USERNS_CONFIGURED so containers/storage treats the
/// process as rootless. This enables automatic chown error tolerance during
/// image layer extraction, since the user namespace only maps UID/GID 0.
pub fn apply_user_ns_env(cmd: &mut tokio::process::Command) {
    if !in_user_namespace() {
        return;
    }
    let runtime_dir = format!("/tmp/fc-mock-runtime-{}", std::process::id());
    let _ = std::fs::create_dir_all(&runtime_dir);
    cmd.env("HOME", "/tmp");
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    // Tell containers/storage we're in a user namespace so it ignores
    // chown errors during image layer extraction (only UID/GID 0 is mapped).
    cmd.env("_CONTAINERS_USERNS_CONFIGURED", "1");
}

/// Detect if we're running inside a user namespace.
///
/// Checks /proc/self/uid_map: identity mapping (0 0 4294967295) means
/// no user namespace. Any other mapping means we're in one.
pub fn in_user_namespace() -> bool {
    match std::fs::read_to_string("/proc/self/uid_map") {
        Ok(content) => {
            let line = content.trim();
            // Identity mapping = not in user namespace
            // Format: "         0          0 4294967295" (with variable whitespace)
            let parts: Vec<&str> = line.split_whitespace().collect();
            !(parts.len() == 3 && parts[0] == "0" && parts[1] == "0" && parts[2] == "4294967295")
        }
        Err(_) => false,
    }
}

/// Clean up any existing container with our name.
pub async fn cleanup_container() {
    debug!("cleaning up container {}", CONTAINER_NAME);
    let storage_args = rootless_storage_args();
    let mut args = storage_args;
    args.extend(
        ["rm", "-f", "-t", "0", CONTAINER_NAME]
            .iter()
            .map(|s| s.to_string()),
    );

    let mut cmd = tokio::process::Command::new("podman");
    cmd.args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    apply_user_ns_env(&mut cmd);

    let output = cmd.output().await;
    if let Ok(o) = output {
        if o.status.success() {
            debug!("removed existing container {}", CONTAINER_NAME);
        }
    }
}

/// Connect to a Unix socket with retries.
async fn connect_with_retry(
    path: &str,
    max_attempts: u32,
    delay_ms: u64,
) -> Result<tokio::net::UnixStream> {
    for attempt in 1..=max_attempts {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(e) if attempt < max_attempts => {
                if attempt == 1 || attempt % 10 == 0 {
                    debug!(attempt, path, "socket not ready, retrying: {}", e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            Err(e) => bail!(
                "failed to connect to {} after {} attempts: {}",
                path,
                max_attempts,
                e
            ),
        }
    }
    unreachable!()
}
