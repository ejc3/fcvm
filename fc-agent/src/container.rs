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

/// Mount a pre-built podman storage image and configure as additionalImageStore.
///
/// The storage image is an ext4 filesystem containing a podman overlay storage tree
/// (overlay/, overlay-images/, overlay-layers/). It is mounted read-only and podman
/// finds the image there without needing `podman load`.
pub fn mount_storage_image(device: &str, image_name: &str) -> Result<String> {
    eprintln!("[fc-agent] mounting pre-built storage image: {}", device);

    let mount_path = "/mnt/image-store";
    std::fs::create_dir_all(mount_path).context("creating image store mount point")?;

    // Wait for device to appear
    let device_path = std::path::Path::new(device);
    for attempt in 1..=10 {
        if device_path.exists() {
            break;
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

    // Mount read-only
    let output = std::process::Command::new("mount")
        .args(["-o", "ro", device, mount_path])
        .output()
        .context("mounting storage image")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to mount storage image {} at {}: {}",
            device,
            mount_path,
            stderr
        );
    }

    // Configure podman to use this as an additional image store.
    // Write a complete storage.conf with runroot/graphroot (required when
    // storage.options is present) but omit "driver" to let podman auto-detect
    // — setting it explicitly causes "database graph driver mismatch" errors.
    let storage_conf = format!(
        "[storage]\nrunroot = \"/run/containers/storage\"\ngraphroot = \"/var/lib/containers/storage\"\n\n[storage.options]\nadditionalimagestores = [\"{mount_path}\"]\n"
    );
    std::fs::write("/etc/containers/storage.conf", storage_conf).context("writing storage.conf")?;

    eprintln!(
        "[fc-agent] storage image mounted at {}, configured as additional image store",
        mount_path
    );
    Ok(image_name.to_string())
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

    let mut load_child = Command::new(&cmd)
        .args(&args)
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

        let mut cmd = Command::new("podman");
        cmd.arg("pull").arg(&plan.image);
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
pub fn create_vm_user(user_spec: &str, desired_name: &str) -> (String, String, String) {
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

    let subuid_entry = format!("{}:100000:65536\n", username);
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
    let _ = std::process::Command::new("env")
        .args([
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

    (username, uid, runtime_dir)
}

/// Build the env + runuser prefix for running commands as the VM user.
pub fn run_as_user_prefix(username: &str, runtime_dir: &str) -> Vec<String> {
    vec![
        "env".to_string(),
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
