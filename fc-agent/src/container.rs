use anyhow::{Context, Result};
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::{sleep, Duration},
};

use crate::output::OutputHandle;
use crate::types::Plan;
use crate::vsock;

/// Mount a pre-built overlay storage image and configure as additionalImageStore.
///
/// The storage image is an ext4 filesystem containing a podman overlay storage tree
/// (overlay/, overlay-images/, overlay-layers/). It is mounted read-only and podman
/// finds the image there without needing `podman load`.
pub fn mount_overlay_image(
    device: &str,
    image_name: &str,
    username: Option<&str>,
) -> Result<String> {
    eprintln!("[fc-agent] mounting overlay storage image: {}", device);

    let mount_path = "/mnt/image-store";
    std::fs::create_dir_all(mount_path).context("creating image store mount point")?;

    wait_for_device(device)?;

    // Mount read-only
    let output = std::process::Command::new("mount")
        .args(["-o", "ro", device, mount_path])
        .output()
        .context("mounting storage image")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to mount overlay storage image {} at {}: {}",
            device,
            mount_path,
            stderr
        );
    }

    // Configure podman to use this as an additional image store.
    let (conf_path, runroot, graphroot) = storage_paths(username);

    let storage_conf = format!(
        "[storage]\ndriver = \"overlay\"\nrunroot = \"{runroot}\"\ngraphroot = \"{graphroot}\"\n\n[storage.options]\nadditionalimagestores = [\"{mount_path}\"]\n"
    );
    std::fs::write(&conf_path, &storage_conf).context("writing storage.conf")?;

    // Write containers.conf to disable netavark (VM uses --network=host)
    let containers_conf_path = if username.is_some() {
        // User-level containers.conf lives alongside storage.conf
        conf_path.replace("storage.conf", "containers.conf")
    } else {
        "/etc/containers/containers.conf".to_string()
    };
    write_containers_conf(&containers_conf_path);

    eprintln!(
        "[fc-agent] overlay image mounted at {}, configured as additional image store (conf: {})",
        mount_path, conf_path
    );

    Ok(image_name.to_string())
}

/// Wait for a block device to appear (up to 5 seconds).
fn wait_for_device(device: &str) -> Result<()> {
    let device_path = std::path::Path::new(device);
    for attempt in 1..=10 {
        if device_path.exists() {
            return Ok(());
        }
        if attempt == 10 {
            anyhow::bail!("Device {} not found after 10 attempts", device);
        }
        eprintln!(
            "[fc-agent] waiting for device {} (attempt {}/10)",
            device, attempt
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

/// Get storage.conf path, runroot, and graphroot for the given user.
fn storage_paths(username: Option<&str>) -> (String, String, String) {
    if let Some(name) = username {
        let home = format!("/home/{}", name);
        let config_dir = format!("{}/.config", home);
        let conf_dir = format!("{}/containers", config_dir);
        let _ = std::fs::create_dir_all(&conf_dir);
        // Look up user once and reuse for chown and UID extraction
        let user_pw = nix::unistd::User::from_name(name).ok().flatten();
        if let Some(ref pw) = user_pw {
            let _ = nix::unistd::chown(config_dir.as_str(), Some(pw.uid), Some(pw.gid));
            let _ = nix::unistd::chown(conf_dir.as_str(), Some(pw.uid), Some(pw.gid));
        }
        let uid = user_pw.map(|u| u.uid.as_raw()).unwrap_or(0);
        (
            format!("{}/storage.conf", conf_dir),
            format!("/run/user/{}/containers", uid),
            format!("{}/.local/share/containers/storage", home),
        )
    } else {
        (
            "/etc/containers/storage.conf".to_string(),
            "/run/containers/storage".to_string(),
            "/var/lib/containers/storage".to_string(),
        )
    }
}

/// Write containers.conf to disable netavark requirement.
///
/// The VM always uses `--network=host` for containers, so podman never needs
/// netavark for container networking. However, podman 4.x checks for netavark
/// at startup even with `--network=host`. The rootfs may not have netavark
/// installed (it's not required for our use case), so we configure podman to
/// skip the check by setting `network_backend = "cni"`.
fn write_containers_conf(conf_path: &str) {
    let containers_conf = "[network]\nnetwork_backend = \"cni\"\n";
    let conf_dir = std::path::Path::new(conf_path).parent();
    if let Some(dir) = conf_dir {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(conf_path, containers_conf) {
        eprintln!(
            "[fc-agent] WARNING: failed to write containers.conf at {}: {}",
            conf_path, e
        );
    }
}

/// Global command prefix for running podman commands as the target user.
/// Set once during setup, used by exec server and health checks.
/// Empty when running as root (no user mapping).
static PODMAN_CMD_PREFIX: OnceLock<Vec<String>> = OnceLock::new();

/// Store the command prefix for use by exec and other podman commands.
pub fn set_podman_cmd_prefix(prefix: Vec<String>) {
    let _ = PODMAN_CMD_PREFIX.set(prefix);
}

/// Get the command prefix (empty vec if running as root).
pub fn podman_cmd_prefix() -> &'static [String] {
    PODMAN_CMD_PREFIX.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// Write a btrfs storage.conf for root podman.
pub fn write_btrfs_storage_conf(conf_path: &str, graphroot: &str, runroot: &str) {
    let _ = std::fs::create_dir_all(
        std::path::Path::new(conf_path)
            .parent()
            .unwrap_or(std::path::Path::new("/etc/containers")),
    );
    let storage_conf = format!(
        "[storage]\ndriver = \"btrfs\"\nrunroot = \"{}\"\ngraphroot = \"{}\"\n",
        runroot, graphroot
    );
    if let Err(e) = std::fs::write(conf_path, &storage_conf) {
        eprintln!(
            "[fc-agent] WARNING: failed to write storage.conf at {}: {}",
            conf_path, e
        );
    }
    let containers_conf = conf_path.replace("storage.conf", "containers.conf");
    write_containers_conf(&containers_conf);
}

/// Get the total size of the filesystem containing `path` in bytes.
fn get_filesystem_size_bytes(path: &str) -> Option<u64> {
    let output = std::process::Command::new("df")
        .args(["--output=size", "-B1", path])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // df output: header line then value line with size in bytes
    stdout
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
        .filter(|&b: &u64| b > 0)
}

/// Set up btrfs storage if the kernel supports it.
/// Creates a sparse loopback btrfs filesystem sized to the root disk capacity
/// and configures podman to use it.
/// This avoids overlay's idmap issues that cause expensive chown-copy on rootless podman.
pub fn setup_btrfs_storage_if_available() {
    // Check if kernel has btrfs support via /proc/filesystems.
    // Note: /sys/fs/btrfs only appears after a btrfs filesystem is mounted,
    // so it can't detect built-in (CONFIG_BTRFS_FS=y) support before first mount.
    let has_btrfs = std::fs::read_to_string("/proc/filesystems")
        .map(|content| content.lines().any(|line| line.trim().ends_with("btrfs")))
        .unwrap_or(false);
    if !has_btrfs {
        eprintln!("[fc-agent] btrfs not available in kernel, using default storage driver");
        return;
    }

    // If root filesystem is natively btrfs, resize to fill disk and skip loopback.
    // The host may have expanded the sparse file for --rootfs-size.
    let root_is_btrfs = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "/"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "btrfs")
        .unwrap_or(false);

    if root_is_btrfs {
        match std::process::Command::new("btrfs")
            .args(["filesystem", "resize", "max", "/"])
            .output()
        {
            Ok(output) if output.status.success() => {
                eprintln!("[fc-agent] root filesystem is btrfs, resized to fill disk");
            }
            Ok(output) => {
                eprintln!(
                    "[fc-agent] WARNING: btrfs resize failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(e) => {
                eprintln!("[fc-agent] WARNING: btrfs resize command failed: {}", e);
            }
        }
        let storage_dir = "/var/lib/containers/storage";
        let _ = std::fs::create_dir_all(storage_dir);
        write_btrfs_storage_conf(
            "/etc/containers/storage.conf",
            storage_dir,
            "/run/containers/storage",
        );
        return;
    }

    let storage_dir = "/var/lib/containers/storage";
    let loopback_path = "/var/lib/containers/btrfs.img";

    // Skip if already btrfs (either native btrfs root or pre-existing loopback mount)
    let already_btrfs = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", storage_dir])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "btrfs")
        .unwrap_or(false);

    if already_btrfs {
        eprintln!(
            "[fc-agent] btrfs storage already mounted at {}",
            storage_dir
        );
        write_btrfs_storage_conf(
            "/etc/containers/storage.conf",
            storage_dir,
            "/run/containers/storage",
        );
        return;
    }

    // Size the sparse loopback to the root filesystem capacity.
    // Since it's sparse, only written blocks use real space — share full disk capacity.
    let loopback_size = get_filesystem_size_bytes("/")
        .map(|b| b.to_string())
        .unwrap_or_else(|| "8G".to_string());

    let _ = std::fs::create_dir_all(storage_dir);
    let truncate = std::process::Command::new("truncate")
        .args(["-s", &loopback_size, loopback_path])
        .output();
    match truncate {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: truncate failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to create btrfs loopback: {}", e);
            return;
        }
    }

    // Format as btrfs
    let mkfs = std::process::Command::new("mkfs.btrfs")
        .args(["-f", loopback_path])
        .output();
    match mkfs {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: mkfs.btrfs failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: mkfs.btrfs not found: {}", e);
            return;
        }
    }

    // Mount
    let mount = std::process::Command::new("mount")
        .args(["-o", "loop", loopback_path, storage_dir])
        .output();
    match mount {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: mount btrfs failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: mount failed: {}", e);
            return;
        }
    }

    // Make the btrfs mount and parent traversable by non-root users.
    // Rootless podman needs to traverse this path to reach its graphroot subdirectory.
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = std::path::Path::new(storage_dir).parent() {
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::set_permissions(storage_dir, std::fs::Permissions::from_mode(0o755));
    }

    // Reset podman state to avoid driver mismatch errors.
    // The rootfs may have been initialized with a different driver during setup.
    let _ = std::process::Command::new("podman")
        .args(["system", "reset", "--force"])
        .output();

    write_btrfs_storage_conf(
        "/etc/containers/storage.conf",
        storage_dir,
        "/run/containers/storage",
    );

    eprintln!(
        "[fc-agent] btrfs storage configured at {} ({} sparse loopback)",
        storage_dir, loopback_size
    );
}

/// Reset root podman state to match the current storage.conf.
///
/// Fixes "database graph driver does not match" errors caused by the health
/// monitor running `podman inspect` via exec before storage setup completes,
/// creating db.sql with an empty or wrong driver.
///
/// Only call for root podman (empty cmd_prefix). User-mode podman already
/// resets in create_vm_user(). A root reset would destroy the user's btrfs
/// storage subdirectory at /var/lib/containers/storage/user-{uid}.
pub fn reset_podman_state() {
    match std::process::Command::new("podman")
        .args(["system", "reset", "--force"])
        .output()
    {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] podman state reset to match storage.conf");
        }
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: podman system reset failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: podman system reset error: {}", e);
        }
    }
}

/// Import a Docker archive into podman storage. Returns image reference.
/// If cmd_prefix is provided, prepend it to the podman command (e.g., for runuser).
pub async fn import_image(
    archive_path: &str,
    image_name: &str,
    output: &OutputHandle,
    cmd_prefix: &[String],
) -> Result<String> {
    eprintln!("[fc-agent] importing Docker archive: {}", archive_path);

    if archive_path.starts_with("/dev/") {
        let _ = std::process::Command::new("chmod")
            .args(["444", archive_path])
            .output();
    }

    let (cmd, args) = if cmd_prefix.is_empty() {
        (
            "podman".to_string(),
            vec![
                "load".to_string(),
                "-i".to_string(),
                archive_path.to_string(),
            ],
        )
    } else {
        let mut all_args: Vec<String> = cmd_prefix[1..].to_vec();
        all_args.extend([
            "podman".to_string(),
            "load".to_string(),
            "-i".to_string(),
            archive_path.to_string(),
        ]);
        (cmd_prefix[0].clone(), all_args)
    };

    // Use btrfs tmpdir if available so podman extracts layers on the same
    // filesystem (avoids ext4→btrfs cross-fs copy during podman load).
    // For rootless: extract uid from cmd_prefix (runuser -u <username>).
    // For root: use uid 0.
    let target_uid = cmd_prefix
        .windows(2)
        .find(|w| w[0] == "-u")
        .and_then(|w| {
            nix::unistd::User::from_name(&w[1])
                .ok()
                .flatten()
                .map(|u| u.uid.as_raw())
        })
        .unwrap_or(0);
    let btrfs_tmpdir = format!("/var/lib/containers/storage/tmp-{}", target_uid);
    let mut cmd_builder = Command::new(&cmd);
    cmd_builder.args(&args);
    if std::path::Path::new(&btrfs_tmpdir).is_dir() {
        eprintln!("[fc-agent] using btrfs tmpdir: {}", btrfs_tmpdir);
        cmd_builder.env("TMPDIR", &btrfs_tmpdir);
    }
    let mut load_child = cmd_builder
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning podman load")?;

    let status = loop {
        tokio::select! {
            result = load_child.wait() => {
                break result.context("waiting for podman load")?;
            }
            _ = sleep(Duration::from_secs(30)) => {
                output.try_send_line("heartbeat", "importing image");
                eprintln!("[fc-agent] heartbeat: still importing image...");
            }
        }
    };

    if !status.success() {
        let stderr = if let Some(mut se) = load_child.stderr.take() {
            let mut buf = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut se, &mut buf).await;
            buf
        } else {
            String::new()
        };
        anyhow::bail!("podman load failed: {}", stderr);
    }

    let loaded_output = if let Some(mut so) = load_child.stdout.take() {
        let mut buf = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut so, &mut buf).await;
        buf
    } else {
        String::new()
    };
    eprintln!("[fc-agent] podman load: {}", loaded_output.trim());
    eprintln!("[fc-agent] image imported as: {}", image_name);
    Ok(image_name.to_string())
}

/// Pull image from registry with retries.
pub async fn pull_image(plan: &Plan) -> Result<String> {
    const MAX_RETRIES: u32 = 3;
    const RETRY_DELAY_SECS: u64 = 2;

    let mut last_error = String::new();

    for attempt in 1..=MAX_RETRIES {
        eprintln!(
            "[fc-agent] PULLING IMAGE: {} (attempt {}/{})",
            plan.image, attempt, MAX_RETRIES
        );

        let prefix = podman_cmd_prefix();
        let mut cmd = if prefix.is_empty() {
            let mut c = Command::new("podman");
            c.arg("pull");
            c
        } else {
            let mut c = Command::new(&prefix[0]);
            c.args(&prefix[1..]);
            c.arg("podman").arg("pull");
            c
        };
        cmd.arg(&plan.image);
        if let Some(ref proxy) = plan.http_proxy {
            cmd.env("http_proxy", proxy).env("HTTP_PROXY", proxy);
        }
        if let Some(ref proxy) = plan.https_proxy {
            cmd.env("https_proxy", proxy).env("HTTPS_PROXY", proxy);
        }
        if let Some(ref no_proxy) = plan.no_proxy {
            cmd.env("no_proxy", no_proxy).env("NO_PROXY", no_proxy);
        }

        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning podman pull")?;

        let stdout_task = child.stdout.take().map(|stdout| {
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[fc-agent] [podman] {}", line);
                }
            })
        });

        let stderr_task = child.stderr.take().map(|stderr| {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut captured = Vec::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[fc-agent] [podman] {}", line);
                    captured.push(line);
                }
                captured
            })
        });

        let status = child.wait().await.context("waiting for podman pull")?;

        if let Some(task) = stdout_task {
            let _ = task.await;
        }
        let stderr_lines = if let Some(task) = stderr_task {
            task.await.unwrap_or_default()
        } else {
            Vec::new()
        };

        if status.success() {
            eprintln!("[fc-agent] image pulled successfully");
            return Ok(plan.image.clone());
        }

        last_error = stderr_lines.join("\n");
        eprintln!(
            "[fc-agent] IMAGE PULL FAILED (attempt {}/{}), exit code: {:?}",
            attempt,
            MAX_RETRIES,
            status.code()
        );

        if attempt < MAX_RETRIES {
            eprintln!("[fc-agent] retrying in {} seconds...", RETRY_DELAY_SECS);
            sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
        }
    }

    anyhow::bail!(
        "Failed to pull image after {} attempts:\n{}",
        MAX_RETRIES,
        last_error
    )
}

/// Get the digest of a pulled image.
pub async fn get_image_digest(image: &str, cmd_prefix: &[String]) -> Result<String> {
    let (cmd, mut args) = if cmd_prefix.is_empty() {
        ("podman".to_string(), vec![])
    } else {
        let mut a: Vec<String> = cmd_prefix[1..].to_vec();
        a.push("podman".to_string());
        (cmd_prefix[0].clone(), a)
    };
    args.extend([
        "image".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        "{{.Digest}}".to_string(),
        image.to_string(),
    ]);

    let output = Command::new(&cmd)
        .args(&args)
        .output()
        .await
        .context("running podman image inspect")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("podman image inspect failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Notify host that image is cached, wait for snapshot ack.
pub fn notify_cache_ready_and_wait(digest: &str) -> bool {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use nix::unistd::{read, write};
    use std::os::fd::{AsFd, AsRawFd};

    let sock = match socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to create vsock socket for cache: {}",
                e
            );
            return false;
        }
    };

    let addr = VsockAddr::new(vsock::HOST_CID, vsock::STATUS_PORT);
    if let Err(e) = connect(sock.as_raw_fd(), &addr) {
        eprintln!(
            "[fc-agent] WARNING: failed to connect vsock for cache: {}",
            e
        );
        return false;
    }

    let msg = format!("cache-ready:{}\n", digest);
    match write(&sock, msg.as_bytes()) {
        Ok(n) if n == msg.len() => {}
        Ok(_) => {
            eprintln!("[fc-agent] WARNING: failed to send complete cache-ready message");
            return false;
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to send cache-ready message: {}",
                e
            );
            return false;
        }
    }

    eprintln!("[fc-agent] sent cache-ready:{}, waiting for ack...", digest);

    if let Ok(flags) = fcntl(sock.as_raw_fd(), FcntlArg::F_GETFL) {
        let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        let _ = fcntl(sock.as_raw_fd(), FcntlArg::F_SETFL(new_flags));
    }

    let mut buf = [0u8; 64];
    let mut total_read = 0;

    loop {
        let mut poll_fds = [PollFd::new(sock.as_fd(), PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::from(100u16)) {
            Err(e) => {
                eprintln!("[fc-agent] cache-ack poll error: {}", e);
                return false;
            }
            Ok(0) => {
                eprintln!("[fc-agent] cache-ack poll timeout (restored from snapshot?)");
                return false;
            }
            Ok(_) => {}
        }

        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLHUP) || revents.contains(PollFlags::POLLERR) {
                eprintln!("[fc-agent] cache-ack connection closed or error");
                return false;
            }
        }

        match read(sock.as_raw_fd(), &mut buf[total_read..]) as Result<usize, nix::errno::Errno> {
            Err(nix::errno::Errno::EAGAIN) => {
                eprintln!("[fc-agent] cache-ack read would block (likely restored from snapshot)");
                return false;
            }
            Err(e) => {
                eprintln!("[fc-agent] cache-ack read error: {}", e);
                return false;
            }
            Ok(0) => {
                eprintln!("[fc-agent] cache-ack connection closed");
                return false;
            }
            Ok(n) => {
                total_read += n;
            }
        }

        let received = std::str::from_utf8(&buf[..total_read]).unwrap_or("");
        if received.contains("cache-ack") {
            eprintln!("[fc-agent] received cache-ack from host");
            return true;
        }

        if total_read >= buf.len() {
            eprintln!("[fc-agent] cache-ack buffer overflow, giving up");
            return false;
        }
    }
}

/// Build podman run args from the plan.
/// If user_info is Some, the container runs as the specified user (rootless podman).
pub fn build_podman_args(
    plan: &Plan,
    image_ref: &str,
    user_info: Option<(&str, &str)>, // (username, runtime_dir)
) -> Vec<String> {
    let mut args = vec![
        "podman".to_string(),
        "run".to_string(),
        "--name".to_string(),
        "fcvm-container".to_string(),
    ];

    // Always use host networking inside the VM. The VM already has its own
    // network namespace (via slirp4netns). Pasta inside the VM would create
    // double-NAT that breaks port forwarding and health checks.
    args.push("--network=host".to_string());

    args.extend([
        "--cgroups=split".to_string(),
        "--ulimit".to_string(),
        "nofile=65536:65536".to_string(),
        "--pids-limit=-1".to_string(),
    ]);

    if let Some((username, runtime_dir)) = user_info {
        setup_user_mapping(&mut args, username, runtime_dir);
    }

    if plan.privileged {
        eprintln!("[fc-agent] privileged mode enabled");
        if user_info.is_some() {
            // Rootless podman: --privileged gives zero effective capabilities with
            // --userns=keep-id. Instead, use explicit --cap-add for the caps we need.
            // These match the host podman's cap-add list exactly.
            for cap in &[
                "net_bind_service",
                "net_admin",
                "sys_nice",
                "sys_resource",
                "sys_ptrace",
                "sys_admin",
            ] {
                args.push(format!("--cap-add={}", cap));
            }
            args.push("--security-opt".to_string());
            args.push("seccomp=unconfined".to_string());
            args.push("--device".to_string());
            args.push("/dev/fuse".to_string());
        } else {
            // Root podman: full device cgroup access + privileged.
            args.push("--device-cgroup-rule=b *:* rwm".to_string());
            args.push("--device-cgroup-rule=c *:* rwm".to_string());
            args.push("--privileged".to_string());
        }
    }

    if plan.interactive {
        args.push("-i".to_string());
    }
    if plan.tty {
        args.push("-t".to_string());
    }

    for (key, val) in &plan.env {
        args.push("-e".to_string());
        args.push(format!("{}={}", key, val));
    }

    // Add FUSE/disk/NFS mounts as bind mounts
    for vol in &plan.volumes {
        let spec = if vol.read_only {
            format!("{}:{}:ro", vol.guest_path, vol.guest_path)
        } else {
            format!("{}:{}", vol.guest_path, vol.guest_path)
        };
        args.push("-v".to_string());
        args.push(spec);
    }
    for disk in &plan.extra_disks {
        let spec = if disk.read_only {
            format!("{}:{}:ro", disk.mount_path, disk.mount_path)
        } else {
            format!("{}:{}", disk.mount_path, disk.mount_path)
        };
        args.push("-v".to_string());
        args.push(spec);
    }
    for share in &plan.nfs_mounts {
        let spec = if share.read_only {
            format!("{}:{}:ro", share.mount_path, share.mount_path)
        } else {
            format!("{}:{}", share.mount_path, share.mount_path)
        };
        args.push("-v".to_string());
        args.push(spec);
    }

    args.push(image_ref.to_string());

    if let Some(cmd_args) = &plan.cmd {
        args.extend(cmd_args.iter().cloned());
    }

    args
}

/// Create the VM user and set up rootless podman prerequisites.
/// Call this BEFORE importing images so podman load runs as the target user.
/// Returns (username, uid, runtime_dir) for use by run_as_user_prefix().
///
/// `subuid_range`: optional (start, count) from the host's /etc/subuid.
/// When the storage image is built by rootless podman on the host, it contains
/// files with UIDs from the host's subuid range. The VM must use the same range
/// so those UIDs are valid in the VM's user namespace.
///
pub fn create_vm_user(
    user_spec: &str,
    desired_name: &str,
    subuid_range: Option<(u64, u64)>,
) -> (String, String, String) {
    let parts: Vec<&str> = user_spec.split(':').collect();
    let uid = parts[0].to_string();
    let gid = parts.get(1).unwrap_or(&"100").to_string();
    let username = desired_name.to_string();

    eprintln!(
        "[fc-agent] setting up user mapping: uid={} gid={}",
        uid, gid
    );

    let _ = std::process::Command::new("groupadd")
        .args(["-g", &gid, &username])
        .output();
    let _ = std::process::Command::new("useradd")
        .args(["-u", &uid, "-g", &gid, "-m", "-s", "/bin/sh", &username])
        .output();

    // Use host's subuid range if provided (matches storage image UIDs),
    // otherwise fall back to a default range.
    let (subuid_start, subuid_count) = subuid_range.unwrap_or((100000, 65536));
    let subuid_entry = format!("{}:{}:{}\n", username, subuid_start, subuid_count);
    eprintln!(
        "[fc-agent] subuid/subgid: {}:{}",
        subuid_start, subuid_count
    );
    let _ = std::fs::write("/etc/subuid", &subuid_entry);
    let _ = std::fs::write("/etc/subgid", &subuid_entry);

    let runtime_dir = format!("/run/user/{}", uid);
    let _ = std::fs::create_dir_all(&runtime_dir);
    let _ = std::process::Command::new("chown")
        .args([&format!("{}:{}", uid, gid), &runtime_dir])
        .output();

    // Clean stale podman storage from previous VM runs. The run root may have
    // changed between runs (e.g., /tmp vs /run/user), causing "database
    // configuration mismatch" errors.
    // HOME is set so podman finds user-level config (if it exists at this point).
    let _ = std::process::Command::new("env")
        .args([
            &format!("HOME=/home/{}", username),
            &format!("XDG_RUNTIME_DIR={}", runtime_dir),
            "runuser",
            "-u",
            &username,
            "--",
            "podman",
            "system",
            "reset",
            "--force",
        ])
        .output();

    let cgroup_dir = format!("/sys/fs/cgroup/user.slice/user-{}.slice", uid);
    let _ = std::fs::create_dir_all(&cgroup_dir);
    let _ = std::process::Command::new("chown")
        .args(["-R", &format!("{}:{}", uid, gid), &cgroup_dir])
        .output();
    for path in &[
        "/sys/fs/cgroup/cgroup.subtree_control".to_string(),
        format!("{}/cgroup.subtree_control", cgroup_dir),
    ] {
        let _ = std::fs::write(path, "+cpu +memory +pids");
    }

    if let Ok(cgroup_path) = std::fs::read_to_string("/proc/self/cgroup") {
        if let Some(path) = cgroup_path.trim().strip_prefix("0::") {
            let full_path = format!("/sys/fs/cgroup{}", path);
            let _ = std::process::Command::new("chown")
                .args(["-R", &format!("{}:{}", uid, gid), &full_path])
                .output();
            eprintln!("[fc-agent] delegated cgroup {} to user {}", full_path, uid);
        }
    }

    // Set up user-level btrfs storage if root btrfs is available
    setup_user_btrfs_storage(&uid, &gid, &username);

    (username, uid, runtime_dir)
}

/// Set up btrfs storage for a rootless user (loopback mode).
///
/// Creates a user-specific subdirectory on the loopback btrfs mount
/// so rootless podman has its own btrfs graphroot.
fn setup_user_btrfs_storage(uid: &str, gid: &str, username: &str) {
    let root_mnt = "/var/lib/containers/storage";

    // Check if root btrfs storage is mounted (loopback from setup_btrfs_storage_if_available)
    let is_btrfs = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", root_mnt])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "btrfs")
        .unwrap_or(false);

    if !is_btrfs {
        return;
    }

    // Loopback mode: create user-specific subdirectory and temp dir.
    // The temp dir is on btrfs so podman extracts layers on the same filesystem
    // (avoids ext4→btrfs cross-filesystem copy during podman load).
    let user_graphroot = format!("{}/user-{}", root_mnt, uid);
    let user_tmpdir = format!("{}/tmp-{}", root_mnt, uid);
    for dir in [&user_graphroot, &user_tmpdir] {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::process::Command::new("chown")
            .args([&format!("{}:{}", uid, gid), dir.as_str()])
            .output();
    }

    // Create user-level runroot
    let user_runroot = format!("/run/user/{}/containers", uid);
    let _ = std::fs::create_dir_all(&user_runroot);
    let _ = std::process::Command::new("chown")
        .args([&format!("{}:{}", uid, gid), &user_runroot])
        .output();

    // Write user-level storage.conf
    let user_config_dir = format!("/home/{}/.config/containers", username);
    let _ = std::fs::create_dir_all(&user_config_dir);
    let storage_conf = format!(
        "[storage]\ndriver = \"btrfs\"\ngraphroot = \"{}\"\nrunroot = \"{}\"\n",
        user_graphroot, user_runroot
    );
    if let Err(e) = std::fs::write(format!("{}/storage.conf", user_config_dir), &storage_conf) {
        eprintln!(
            "[fc-agent] WARNING: failed to write user storage.conf: {}",
            e
        );
        return;
    }
    write_containers_conf(&format!("{}/containers.conf", user_config_dir));
    // Ensure the user owns their config
    let _ = std::process::Command::new("chown")
        .args([
            "-R",
            &format!("{}:{}", uid, gid),
            &format!("/home/{}/.config", username),
        ])
        .output();

    eprintln!(
        "[fc-agent] user btrfs storage configured at {}",
        user_graphroot
    );
}

/// Build the env + runuser prefix for running commands as the VM user.
///
/// Sets HOME so podman finds user-level config at ~/.config/containers/,
/// and XDG_RUNTIME_DIR for the user's runtime state.
pub fn run_as_user_prefix(username: &str, runtime_dir: &str) -> Vec<String> {
    vec![
        "env".to_string(),
        format!("HOME=/home/{}", username),
        format!("XDG_RUNTIME_DIR={}", runtime_dir),
        "runuser".to_string(),
        "-u".to_string(),
        username.to_string(),
        "--".to_string(),
    ]
}

fn setup_user_mapping(args: &mut Vec<String>, username: &str, runtime_dir: &str) {
    // Rootless podman: remove split cgroups, add keep-id + cgroupfs manager.
    // No systemd user session in the VM, so use cgroupfs directly.
    args.retain(|a| a != "--cgroups=split");
    args.push("--userns=keep-id".to_string());
    args.push("--cgroup-manager=cgroupfs".to_string());

    // Wrap with env + runuser to set XDG_RUNTIME_DIR (rootless podman needs it)
    let prefix = run_as_user_prefix(username, runtime_dir);
    for (i, arg) in prefix.into_iter().enumerate() {
        args.insert(i, arg);
    }
}

/// Run container in TTY mode (blocks until exit).
pub fn run_tty(podman_args: &[String], plan: &Plan, mounted_fuse_paths: &[String]) -> ! {
    vsock::notify_container_started();

    let exit_code = crate::tty::run_with_pty(podman_args, plan.tty, plan.interactive);

    vsock::notify_container_exit(exit_code);

    crate::mounts::unmount_paths(mounted_fuse_paths, "FUSE volume");

    eprintln!("[fc-agent] powering off VM");
    let _ = std::process::Command::new("poweroff").arg("-f").spawn();

    std::process::exit(exit_code);
}

/// Run container in non-TTY async mode. Returns exit code.
pub async fn run_async(podman_args: &[String], output: &OutputHandle) -> Result<i32> {
    let mut cmd = Command::new(&podman_args[0]);
    cmd.args(&podman_args[1..]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning Podman container")?;

    vsock::notify_container_started();

    // Stream stdout via OutputHandle
    let out = output.clone();
    let stdout_task = child.stdout.take().map(|stdout| {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                out.send_line("stdout", &line).await;
            }
        })
    });

    let out = output.clone();
    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                out.send_line("stderr", &line).await;
            }
        })
    });

    let status = child.wait().await?;
    let exit_code = status.code().unwrap_or(1);

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }

    if status.success() {
        eprintln!("[fc-agent] container exited successfully");
    } else {
        eprintln!(
            "[fc-agent] container exited with error: {} (code {})",
            status, exit_code
        );

        // Capture podman logs on failure (use user prefix for rootless podman)
        eprintln!("[fc-agent] capturing podman logs for failed container...");
        let prefix = podman_cmd_prefix();
        let logs_result = if prefix.is_empty() {
            std::process::Command::new("podman")
                .args(["logs", "fcvm-container"])
                .output()
        } else {
            let mut c = std::process::Command::new(&prefix[0]);
            c.args(&prefix[1..]);
            c.args(["podman", "logs", "fcvm-container"]);
            c.output()
        };
        match logs_result {
            Ok(logs) => {
                let stdout = String::from_utf8_lossy(&logs.stdout);
                let stderr = String::from_utf8_lossy(&logs.stderr);
                if !stdout.is_empty() {
                    eprintln!("[fc-agent] === podman logs (stdout) ===");
                    for line in stdout.lines() {
                        eprintln!("[fc-agent] {}", line);
                        output.try_send_line("stdout", line);
                    }
                }
                if !stderr.is_empty() {
                    eprintln!("[fc-agent] === podman logs (stderr) ===");
                    for line in stderr.lines() {
                        eprintln!("[fc-agent] {}", line);
                        output.try_send_line("stderr", line);
                    }
                }
                if stdout.is_empty() && stderr.is_empty() {
                    eprintln!("[fc-agent] (no podman logs captured)");
                }
            }
            Err(e) => {
                eprintln!("[fc-agent] failed to get podman logs: {}", e);
            }
        }
    }

    // Clean up the container (use user prefix for rootless podman)
    let prefix = podman_cmd_prefix();
    if prefix.is_empty() {
        let _ = std::process::Command::new("podman")
            .args(["rm", "-f", "fcvm-container"])
            .output();
    } else {
        let _ = std::process::Command::new(&prefix[0])
            .args(&prefix[1..])
            .args(["podman", "rm", "-f", "fcvm-container"])
            .output();
    }

    Ok(exit_code)
}
