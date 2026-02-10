use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::{sleep, Duration},
};

use crate::output::OutputHandle;
use crate::types::Plan;
use crate::vsock;

/// Import a Docker archive into podman storage. Returns image reference.
pub async fn import_image(
    archive_path: &str,
    image_name: &str,
    output: &OutputHandle,
) -> Result<String> {
    eprintln!("[fc-agent] importing Docker archive: {}", archive_path);

    if archive_path.starts_with("/dev/") {
        let _ = std::process::Command::new("chmod")
            .args(["444", archive_path])
            .output();
    }

    let mut load_child = Command::new("podman")
        .args(["load", "-i", archive_path])
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
pub async fn get_image_digest(image: &str) -> Result<String> {
    let output = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Digest}}", image])
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
pub fn build_podman_args(plan: &Plan, image_ref: &str) -> Vec<String> {
    let mut args = vec![
        "podman".to_string(),
        "run".to_string(),
        "--name".to_string(),
        "fcvm-container".to_string(),
        "--network=host".to_string(),
        "--cgroups=split".to_string(),
        "--ulimit".to_string(),
        "nofile=65536:65536".to_string(),
    ];

    if let Some(ref user_spec) = plan.user {
        setup_user_mapping(&mut args, user_spec);
    }

    if plan.privileged {
        eprintln!("[fc-agent] privileged mode enabled");
        args.push("--device-cgroup-rule=b *:* rwm".to_string());
        args.push("--device-cgroup-rule=c *:* rwm".to_string());
        args.push("--privileged".to_string());
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

fn setup_user_mapping(args: &mut Vec<String>, user_spec: &str) {
    let parts: Vec<&str> = user_spec.split(':').collect();
    let uid = parts[0];
    let gid = parts.get(1).unwrap_or(&"100");
    let username = "fcvm-user".to_string();

    eprintln!(
        "[fc-agent] setting up user mapping: uid={} gid={}",
        uid, gid
    );

    let _ = std::process::Command::new("groupadd")
        .args(["-g", gid, &username])
        .output();
    let _ = std::process::Command::new("useradd")
        .args(["-u", uid, "-g", gid, "-m", "-s", "/bin/sh", &username])
        .output();

    let subuid_entry = format!("{}:100000:65536\n", username);
    let _ = std::fs::write("/etc/subuid", &subuid_entry);
    let _ = std::fs::write("/etc/subgid", &subuid_entry);

    let runtime_dir = format!("/run/user/{}", uid);
    let _ = std::fs::create_dir_all(&runtime_dir);
    let _ = std::process::Command::new("chown")
        .args([&format!("{}:{}", uid, gid), &runtime_dir])
        .output();

    let cgroup_dir = format!("/sys/fs/cgroup/user.slice/user-{}.slice", uid);
    let _ = std::fs::create_dir_all(&cgroup_dir);
    let _ = std::process::Command::new("chown")
        .args(["-R", &format!("{}:{}", uid, gid), &cgroup_dir])
        .output();
    for path in &[
        "/sys/fs/cgroup/cgroup.subtree_control",
        &format!("{}/cgroup.subtree_control", cgroup_dir),
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

    // Rootless podman: remove split, add keep-id, wrap with runuser
    args.retain(|a| a != "--cgroups=split");
    args.push("--userns=keep-id".to_string());
    args.insert(0, "--".to_string());
    args.insert(0, username);
    args.insert(0, "-u".to_string());
    args.insert(0, "runuser".to_string());
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

        // Capture podman logs on failure
        eprintln!("[fc-agent] capturing podman logs for failed container...");
        match std::process::Command::new("podman")
            .args(["logs", "fcvm-container"])
            .output()
        {
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

    // Clean up the container
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", "fcvm-container"])
        .output();

    Ok(exit_code)
}
