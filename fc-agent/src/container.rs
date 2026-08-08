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
    reset_podman_state: bool,
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

    // Reset podman state after writing storage.conf to clear any stale db.sql
    // that may have been created by health monitor's `podman inspect` racing
    // with storage setup. Without this, the db.sql graph driver won't match
    // the new storage.conf, causing "database graph driver does not match".
    // NEVER on a provisioned re-mount (clone/reboot): the graphroot holds the
    // captured container — a reset would erase it.
    if reset_podman_state {
        let _ = std::process::Command::new("podman")
            .args(["system", "reset", "--force"])
            .output();
    } else {
        eprintln!("[fc-agent] skipping podman reset (provisioned overlay re-mount)");
    }

    eprintln!(
        "[fc-agent] overlay image mounted at {}, configured as additional image store (conf: {})",
        mount_path, conf_path
    );

    // Discover the actual image reference in the overlay store.
    // The overlay cache is keyed by digest (content-addressed), but the image
    // inside is tagged with the name from the original `podman save`. When the
    // same content is built under a different name, the cached overlay has the
    // old name. We ask podman what's actually there instead of trusting the
    // expected name from the Plan.
    let discover_output = if let Some(name) = username {
        let user_pw = nix::unistd::User::from_name(name).ok().flatten();
        let uid = user_pw.map(|u| u.uid.as_raw()).unwrap_or(0);
        std::process::Command::new("env")
            .args([
                &format!("HOME=/home/{}", name),
                &format!("XDG_RUNTIME_DIR=/run/user/{}", uid),
                "runuser",
                "-u",
                name,
                "--",
                "podman",
                "images",
                "--format",
                "{{.Repository}}:{{.Tag}}",
            ])
            .output()
    } else {
        std::process::Command::new("podman")
            .args(["images", "--format", "{{.Repository}}:{{.Tag}}"])
            .output()
    };

    if let Ok(output) = discover_output {
        if output.status.success() {
            let images_out = String::from_utf8_lossy(&output.stdout);
            // Find first image that isn't <none>
            if let Some(actual_ref) = images_out
                .lines()
                .find(|line| !line.contains("<none>") && !line.trim().is_empty())
            {
                let actual_ref = actual_ref.trim().to_string();
                if actual_ref != image_name {
                    eprintln!(
                        "[fc-agent] overlay store contains '{}' (expected '{}'), using discovered name",
                        actual_ref, image_name
                    );
                }
                return Ok(actual_ref);
            }
        }
    }

    // Fallback: return expected name (downstream will fail with clear error if wrong)
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

/// Write a minimal storage.conf with the correct graph driver.
///
/// Must be called BEFORE the exec server starts. The health monitor runs
/// `podman inspect` inside the VM as soon as the exec server accepts connections.
/// Any `podman` invocation initializes the BoltDB with whatever driver is in
/// storage.conf. If the default storage.conf has `driver = ""` (auto-detect),
/// the BoltDB may be initialized with an empty or wrong driver. When
/// `mount_overlay_image()` later writes `driver = "overlay"`, podman sees a
/// mismatch: "database graph driver '' does not match our graph driver 'overlay'".
///
/// By writing `driver = "overlay"` early, all podman invocations before the full
/// storage setup create a BoltDB that matches the final config.
pub fn write_early_storage_conf() {
    let (conf_path, runroot, graphroot) = storage_paths(None);
    let conf = format!(
        "[storage]\ndriver = \"overlay\"\nrunroot = \"{runroot}\"\ngraphroot = \"{graphroot}\"\n"
    );
    if let Err(e) = std::fs::write(&conf_path, conf) {
        eprintln!(
            "[fc-agent] WARNING: failed to write early storage.conf: {}",
            e
        );
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
/// Errors only when a PROVISIONED disk's existing storage loopback fails to mount —
/// continuing would silently detach the captured container (the boot would fall back
/// to an empty store and `podman run` a fresh container). Fresh-boot setup problems
/// remain best-effort warnings (the VM can still run with the default driver).
pub fn setup_btrfs_storage_if_available() -> anyhow::Result<()> {
    // Check if kernel has btrfs support via /proc/filesystems.
    // Note: /sys/fs/btrfs only appears after a btrfs filesystem is mounted,
    // so it can't detect built-in (CONFIG_BTRFS_FS=y) support before first mount.
    let has_btrfs = std::fs::read_to_string("/proc/filesystems")
        .map(|content| content.lines().any(|line| line.trim().ends_with("btrfs")))
        .unwrap_or(false);
    if !has_btrfs {
        eprintln!("[fc-agent] btrfs not available in kernel, using default storage driver");
        return Ok(());
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
        return Ok(());
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
        return Ok(());
    }

    // Disk-only clone: the loopback was formatted and populated on the source's
    // first boot and reflinked into this disk. Mount it as-is — NEVER mkfs, which
    // would erase the captured image + container. Mounts don't survive a reboot,
    // so the file exists but isn't mounted yet.
    if is_provisioned() && std::path::Path::new(loopback_path).exists() {
        match std::process::Command::new("mount")
            .args(["-o", "loop", loopback_path, storage_dir])
            .output()
        {
            Ok(o) if o.status.success() => {
                use std::os::unix::fs::PermissionsExt;
                if let Some(parent) = std::path::Path::new(storage_dir).parent() {
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
                }
                let _ =
                    std::fs::set_permissions(storage_dir, std::fs::Permissions::from_mode(0o755));
                write_btrfs_storage_conf(
                    "/etc/containers/storage.conf",
                    storage_dir,
                    "/run/containers/storage",
                );
                eprintln!("[fc-agent] mounted existing btrfs storage loopback (clone)");
                return Ok(());
            }
            Ok(o) => {
                anyhow::bail!(
                    "mounting existing provisioned btrfs loopback failed: {} — refusing \
                     to continue (the captured container storage would be silently lost)",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "mount of existing provisioned btrfs loopback failed: {e} — refusing \
                     to continue (the captured container storage would be silently lost)"
                );
            }
        }
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
            return Ok(());
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to create btrfs loopback: {}", e);
            return Ok(());
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
            return Ok(());
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: mkfs.btrfs not found: {}", e);
            return Ok(());
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
            return Ok(());
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: mount failed: {}", e);
            return Ok(());
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
    Ok(())
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

/// Marker written on the rootfs once first-boot provisioning (storage + image +
/// container) completes. A disk-only clone cold-boots from a reflink of that
/// rootfs, so the marker is already present — the signal that says "the work is
/// already here, don't redo (or destroy) it; just regenerate the identity."
const PROVISIONED_MARKER: &str = "/var/lib/fcvm/provisioned";

/// True if this boot is from an already-provisioned disk (a disk-only clone).
pub fn is_provisioned() -> bool {
    std::path::Path::new(PROVISIONED_MARKER).exists()
}

/// Record that first-boot provisioning completed. Idempotent.
pub fn write_provisioned_marker() {
    if let Some(parent) = std::path::Path::new(PROVISIONED_MARKER).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(PROVISIONED_MARKER, b"1\n") {
        eprintln!("[fc-agent] WARNING: failed to write provisioned marker: {e}");
    }
}

/// Whether a container named `fcvm-container` already exists in podman storage.
/// On a clone boot the container is already created — we start it rather than
/// `podman run` a fresh one, preserving its writable layer (the captured work).
pub fn container_exists(cmd_prefix: &[String]) -> bool {
    let mut cmd = if cmd_prefix.is_empty() {
        std::process::Command::new("podman")
    } else {
        let mut c = std::process::Command::new(&cmd_prefix[0]);
        c.args(&cmd_prefix[1..]);
        c.arg("podman");
        c
    };
    cmd.args(["container", "exists", "fcvm-container"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the argv to start (and attach to) the captured `fcvm-container`.
/// Mirrors `build_podman_args`' rootless prefixing so the same run_tty/run_async
/// path drives it.
pub fn build_start_args(plan: &Plan, user_info: Option<(&str, &str)>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some((username, runtime_dir)) = user_info {
        args.extend(run_as_user_prefix(username, runtime_dir));
    }
    args.push("podman".to_string());
    args.push("start".to_string());
    // Attach stdout/stderr (and stdin when interactive) so fc-agent forwards
    // the container's output exactly as it does for `podman run`.
    if plan.interactive {
        args.push("--attach".to_string());
        args.push("--interactive".to_string());
    } else {
        args.push("--attach".to_string());
    }
    args.push("fcvm-container".to_string());
    args
}

/// Regenerate per-machine identity on a clone so concurrently-running clones of
/// the same disk don't collide. Hostname is set separately from the Plan.
pub fn regenerate_identity() {
    // machine-id: a fresh random id (systemd/dbus and many apps key off it).
    if let Ok(uuid) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let id = format!("{}\n", uuid.trim().replace('-', ""));
        let _ = std::fs::write("/etc/machine-id", &id);
        if std::path::Path::new("/var/lib/dbus").is_dir() {
            let _ = std::fs::write("/var/lib/dbus/machine-id", &id);
        }
        eprintln!("[fc-agent] regenerated machine-id for clone");
    }
    // SSH host keys: regenerate if sshd is present (best-effort).
    if std::path::Path::new("/etc/ssh").is_dir() {
        if let Ok(entries) = std::fs::read_dir("/etc/ssh") {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("ssh_host_") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let _ = std::process::Command::new("ssh-keygen").arg("-A").output();
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

    // Drain stdout/stderr concurrently while waiting (same pattern as pull_image).
    // podman load can emit per-layer progress lines; if neither pipe is read until
    // after exit, the child blocks once the 64KB pipe buffer fills and wait() never
    // returns — the import hangs forever with only heartbeats.
    let stdout_task = load_child.stdout.take().map(|stdout| {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut captured = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                captured.push(line);
            }
            captured
        })
    });

    let stderr_task = load_child.stderr.take().map(|stderr| {
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

    let stdout_lines = if let Some(task) = stdout_task {
        task.await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let stderr_lines = if let Some(task) = stderr_task {
        task.await.unwrap_or_default()
    } else {
        Vec::new()
    };

    if !status.success() {
        anyhow::bail!("podman load failed: {}", stderr_lines.join("\n"));
    }

    eprintln!("[fc-agent] podman load: {}", stdout_lines.join(" "));
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

/// Result of notify_cache_ready_and_wait: distinguishes cold start from warm start.
#[derive(Debug, PartialEq)]
pub enum CacheResult {
    /// Host sent "cache-ack" — cold start, vsock connections are alive.
    ColdStart,
    /// POLLHUP, write-probe, or restore-epoch detected — warm start,
    /// vsock connections are dead (VIRTIO_VSOCK_EVENT_TRANSPORT_RESET).
    WarmStart,
    /// Handshake failed (console quiesce failure — cache-ready deliberately
    /// not sent so the host cannot snapshot a mid-transmit UART — or connect
    /// error, send error, timeout, etc.). The VM continues cold.
    Failed,
}

/// Notify host that image is cached, wait for snapshot ack.
///
/// The `restore_flag` is set by the restore-epoch watcher when it detects
/// a snapshot restore. This breaks the poll loop early when POLLHUP is not
/// detected (e.g., after pre-start snapshot restore in rootless mode).
/// Compact the cache-handshake read buffer after checking for "cache-ack":
/// drop all COMPLETE lines (host "cache-wait" keepalives — the only complete
/// lines the host sends before the ack) and keep any partial tail, so repeated
/// keepalives can't overflow the small buffer while a "cache-ack" split across
/// reads (e.g. "cache-a" + "ck\n") still assembles correctly from the tail.
///
/// Returns (new_total_read, saw_cache_wait_keepalive).
fn consume_cache_wait_lines(buf: &mut [u8; 64], total_read: usize) -> (usize, bool) {
    let received = std::str::from_utf8(&buf[..total_read]).unwrap_or("");
    let saw = received.contains("cache-wait");
    if !saw {
        return (total_read, false);
    }
    match received.rfind('\n') {
        Some(pos) => {
            let tail_start = pos + 1;
            let tail_len = total_read - tail_start;
            buf.copy_within(tail_start..total_read, 0);
            (tail_len, true)
        }
        None => (total_read, true),
    }
}

pub fn notify_cache_ready_and_wait(
    digest: &str,
    restore_flag: &std::sync::atomic::AtomicBool,
) -> CacheResult {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use nix::unistd::{read, write};
    use std::os::fd::{AsFd, AsRawFd};

    // Receiving "cache-ready" makes the host PAUSE this VM for the pre-start
    // snapshot. A snapshot that captures the UART mid-transmit is poisoned:
    // EVERY restore of it has a dead serial console (the guest 8250 driver
    // waits forever for a TX interrupt the re-created serial device never
    // delivers). So: announce FIRST, then make the console provably quiet
    // (flush + gate + TIOCOUTQ drain — structural, not probabilistic), and
    // only THEN let the host know we are ready to be paused. On success the
    // gate holds until this function returns (every path: ack, restore,
    // failure), so no concurrent task can put a byte in UART TX while the
    // pause can happen — lines logged meanwhile are buffered and flushed when
    // the guard drops. If the console CANNOT be proven quiet, the handshake
    // is aborted BEFORE cache-ready is sent: the host never pauses us, no
    // poisoned snapshot artifact can be created, and the VM continues cold.
    eprintln!(
        "[fc-agent] image digest {} loaded; quiescing console before cache-ready notification",
        digest
    );
    let _console_quiesce = match crate::console::quiesce_for_snapshot() {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: {}; not sending cache-ready — continuing cold \
                 without a pre-start snapshot",
                e
            );
            return CacheResult::Failed;
        }
    };

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
            return CacheResult::Failed;
        }
    };

    let addr = VsockAddr::new(vsock::HOST_CID, vsock::STATUS_PORT);
    if let Err(e) = connect(sock.as_raw_fd(), &addr) {
        eprintln!(
            "[fc-agent] WARNING: failed to connect vsock for cache: {}",
            e
        );
        return CacheResult::Failed;
    }

    // failpoint: hold AFTER the console quiesce completed and immediately BEFORE
    // announcing cache-ready (which triggers the host's pre-start snapshot pause) —
    // proves the quiesce gate keeps the UART idle across an arbitrarily long
    // pre-pause window. Sync context (this fn is blocking). Note: the marker line
    // is buffered by the quiesced console and reaches the host log only after the
    // guard drops — a hold, not a host-visible sync point.
    failpoint::hit("cache_ready.pre_send");

    let msg = format!("cache-ready:{}\n", digest);
    match write(&sock, msg.as_bytes()) {
        Ok(n) if n == msg.len() => {}
        Ok(_) => {
            eprintln!("[fc-agent] WARNING: failed to send complete cache-ready message");
            return CacheResult::Failed;
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to send cache-ready message: {}",
                e
            );
            return CacheResult::Failed;
        }
    }

    eprintln!("[fc-agent] sent cache-ready:{}, waiting for ack...", digest);

    if let Ok(flags) = fcntl(sock.as_raw_fd(), FcntlArg::F_GETFL) {
        let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        let _ = fcntl(sock.as_raw_fd(), FcntlArg::F_SETFL(new_flags));
    }

    let mut buf = [0u8; 64];
    let mut total_read = 0;

    // Use a 30s deadline with 500ms poll intervals.
    // The host takes ~740ms between receiving cache-ready and reaching the
    // Pause API (directory creation, flock acquisition, setup). A 100ms
    // timeout caused fc-agent to give up before the snapshot could start,
    // leading to the container launching and exiting while the host was
    // still setting up the snapshot — killing the VM mid-pause.
    //
    // Valid exit conditions:
    // 1. "cache-ack" received — host says no snapshot needed (cold start)
    // 2. POLLHUP / read returns 0 — vsock reset from snapshot RESTORE
    //    (warm start: VM was restored from cached pre-start snapshot)
    // 3. restore_flag set by restore-epoch watcher (warm start, POLLHUP not delivered)
    // 4. 30s since the last sign of host liveness — failsafe timeout. The host
    //    emits a "cache-wait" keepalive every 5s while it is queued on the global
    //    snapshot semaphore (and while the snapshot runs), and each keepalive
    //    extends this deadline (#627): under the SnapshotEnabled CI matrix the
    //    semaphore queue alone can exceed a fixed 30s while the host is perfectly
    //    alive, and abandoning the wait launches the container out of step with
    //    the host's pause. The deadline expires only when the host has gone
    //    genuinely silent — bounded by an absolute 10-minute cap so a wedged host
    //    that keeps ticking keepalives can't stall container launch forever.
    //
    //    Ordering matters: the deadline is checked AFTER each poll/drain cycle,
    //    never before. Keepalives can queue while the VM is paused for the
    //    snapshot, and the guest clock can jump forward across the pause (the ARM
    //    virtual counter keeps running) — checking expiry before draining would
    //    discard extensions that are already sitting in the socket buffer.
    let started = std::time::Instant::now();
    let absolute_deadline = started + std::time::Duration::from_secs(600);
    let mut deadline = started + std::time::Duration::from_secs(30);

    loop {
        // Check if the restore-epoch watcher detected a snapshot restore.
        // This breaks the loop when POLLHUP is not delivered (observed in
        // rootless mode after pre-start snapshot restore).
        if restore_flag.load(std::sync::atomic::Ordering::Acquire) {
            eprintln!("[fc-agent] cache-ack: restore detected via epoch watcher, skipping wait");
            return CacheResult::WarmStart;
        }

        // Fixed 500ms poll; expiry is evaluated after the poll/drain below.
        let poll_ms = 500u16;
        let mut poll_fds = [PollFd::new(sock.as_fd(), PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::from(poll_ms)) {
            Err(e) => {
                eprintln!("[fc-agent] cache-ack poll error: {}", e);
                return CacheResult::Failed;
            }
            Ok(0) => {
                // Poll timeout — actively probe the vsock connection.
                // After snapshot restore, the vsock transport is reset but the kernel
                // may not deliver POLLHUP on the restored fd (observed in rootless mode).
                // A write to a dead connection fails immediately with EPIPE or ECONNRESET,
                // which reliably detects that a snapshot was taken and we've been restored.
                match write(&sock, b"\n") {
                    Err(nix::errno::Errno::EPIPE)
                    | Err(nix::errno::Errno::ECONNRESET)
                    | Err(nix::errno::Errno::ENOTCONN)
                    | Err(nix::errno::Errno::ECONNREFUSED) => {
                        eprintln!("[fc-agent] cache-ack connection dead (write probe), snapshot was taken");
                        return CacheResult::WarmStart;
                    }
                    Err(nix::errno::Errno::EAGAIN) => {
                        // Connection alive but can't write now — continue polling
                    }
                    _ => {
                        // Write succeeded or other error — connection still alive, continue
                    }
                }
                // No data arrived this cycle — judge expiry now (never before a
                // drain: queued keepalives must be consumed first).
                let now = std::time::Instant::now();
                if now >= absolute_deadline {
                    eprintln!("[fc-agent] cache-ack absolute deadline expired (10 min)");
                    return CacheResult::Failed;
                }
                if now >= deadline {
                    eprintln!("[fc-agent] cache-ack deadline expired (host silent for 30s)");
                    return CacheResult::Failed;
                }
                continue;
            }
            Ok(_) => {}
        }

        // Drain readable data BEFORE acting on POLLHUP/POLLERR. Linux can report
        // POLLIN|POLLHUP together when the host writes "cache-ack" and then
        // immediately closes the connection — which the host status listener does
        // after a cold-start cache handshake. Treating POLLHUP as EOF before
        // reading would discard a buffered cache-ack and misclassify the cold
        // start as a warm restore, sending it through the restore-wait path.
        let hung_up = poll_fds[0]
            .revents()
            .is_some_and(|r| r.contains(PollFlags::POLLHUP) || r.contains(PollFlags::POLLERR));

        match read(sock.as_raw_fd(), &mut buf[total_read..]) as Result<usize, nix::errno::Errno> {
            Ok(n) if n > 0 => {
                total_read += n;
                let received = std::str::from_utf8(&buf[..total_read]).unwrap_or("");
                if received.contains("cache-ack") {
                    // Positive close handshake: close our end FIRST, before any
                    // logging, so the host's drain sees EOF as early as possible.
                    // The host holds this connection open until then precisely so
                    // it never closes with one of our 500ms liveness probes still
                    // unread — an unread byte at close becomes a vsock RST that
                    // flushes this receive buffer, and the ack we just read would
                    // have been lost to a spurious WarmStart (#627). Our close is
                    // what tells the host the ack landed.
                    //
                    // `poll_fds` borrowed `sock`, but its last use was the
                    // `hung_up` read above, so the borrow has already ended and
                    // the socket can be dropped — which closes the fd.
                    drop(sock);
                    eprintln!("[fc-agent] received cache-ack from host (handshake closed)");
                    return CacheResult::ColdStart;
                }
                // Host keepalive: it is alive but still queued on the snapshot
                // semaphore / writing the snapshot. Extend the deadline (bounded
                // by the absolute cap) and compact the buffer so repeated
                // keepalives can't overflow it.
                let (new_total, saw_keepalive) = consume_cache_wait_lines(&mut buf, total_read);
                total_read = new_total;
                if saw_keepalive {
                    deadline = (std::time::Instant::now() + std::time::Duration::from_secs(30))
                        .min(absolute_deadline);
                    eprintln!("[fc-agent] cache-wait keepalive from host, extending deadline");
                }
                if total_read >= buf.len() {
                    eprintln!("[fc-agent] cache-ack buffer overflow, giving up");
                    return CacheResult::Failed;
                }
                // Partial data and the peer has hung up: no cache-ack is coming.
                if hung_up {
                    eprintln!(
                        "[fc-agent] cache-ack connection reset after partial read (warm start)"
                    );
                    return CacheResult::WarmStart;
                }
                // Otherwise keep reading for the rest of the message.
            }
            Ok(_) => {
                // Orderly EOF with no buffered cache-ack — vsock reset from a
                // snapshot RESTORE (warm start).
                eprintln!("[fc-agent] cache-ack connection closed (snapshot taken)");
                return CacheResult::WarmStart;
            }
            Err(nix::errno::Errno::EAGAIN) => {
                // No data pending. If the peer hung up, it's a genuine reset.
                if hung_up {
                    eprintln!("[fc-agent] cache-ack connection reset (snapshot restore detected)");
                    return CacheResult::WarmStart;
                }
                // Spurious wakeup, continue polling.
                continue;
            }
            // A reset/torn-down connection (no buffered cache-ack recovered above)
            // means the vsock transport went away — either a genuine snapshot
            // RESTORE, or a cold-start handshake where the host closed the
            // connection with our write-probe bytes still unread, turning its
            // close into a RST that flushes our receive buffer. Either way the
            // correct fallback is a warm start, matching the write-probe
            // classification above; returning Failed here would abort container
            // launch on every reset. Only genuinely unexpected errors are fatal.
            Err(nix::errno::Errno::ECONNRESET)
            | Err(nix::errno::Errno::EPIPE)
            | Err(nix::errno::Errno::ENOTCONN)
            | Err(nix::errno::Errno::ECONNREFUSED) => {
                eprintln!("[fc-agent] cache-ack connection reset on read (warm start)");
                return CacheResult::WarmStart;
            }
            Err(e) => {
                eprintln!("[fc-agent] cache-ack read error: {}", e);
                return CacheResult::Failed;
            }
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
    // network namespace (via pasta). A second pasta inside the VM would create
    // double-NAT that breaks port forwarding and health checks.
    args.push("--network=host".to_string());

    args.extend([
        "--cgroups=split".to_string(),
        "--ulimit".to_string(),
        "nofile=65536:65536".to_string(),
        "--pids-limit=-1".to_string(),
        // Disable conmon log capture. fc-agent captures container output through
        // podman's stdout/stderr pipes directly. conmon's default k8s-file log
        // driver adds a redundant buffering layer that blocks under burst output
        // with rootless podman (--user mode), causing the container to deadlock
        // on stdout/stderr pipe write.
        "--log-driver=none".to_string(),
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
    provisioned: bool,
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
    // NEVER on a provisioned boot (disk-only clone / in-place reboot): the user's
    // storage holds the captured container — a reset would erase it.
    if provisioned {
        eprintln!(
            "[fc-agent] skipping user podman reset (provisioned disk — captured storage preserved)"
        );
    } else {
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
    }

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
pub async fn run_async(
    podman_args: &[String],
    output: &OutputHandle,
    non_blocking_output: bool,
) -> Result<i32> {
    let mut cmd = Command::new(&podman_args[0]);
    cmd.args(&podman_args[1..]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning Podman container")?;

    vsock::notify_container_started();

    // Stream stdout via OutputHandle.
    // In non-blocking mode, use try_send_line (drops on full) to prevent
    // backpressure from cascading into the container and deadlocking
    // FUSE-based services like configerator_fuse.
    let out = output.clone();
    let nb = non_blocking_output;
    let stdout_task = child.stdout.take().map(|stdout| {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if nb {
                    out.try_send_line("stdout", &line);
                } else {
                    out.send_line("stdout", &line).await;
                }
            }
        })
    });

    let out = output.clone();
    let nb = non_blocking_output;
    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if nb {
                    out.try_send_line("stderr", &line);
                } else {
                    out.send_line("stderr", &line).await;
                }
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

        // Note: podman logs are unavailable because we use --log-driver=none
        // (required to prevent conmon deadlock under burst output).
        // Container output was already captured through fc-agent's pipe-based
        // output forwarding and sent to the host via vsock.
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

#[cfg(test)]
mod tests {
    use super::consume_cache_wait_lines;

    fn buf_with(data: &[u8]) -> ([u8; 64], usize) {
        let mut buf = [0u8; 64];
        buf[..data.len()].copy_from_slice(data);
        (buf, data.len())
    }

    #[test]
    fn keepalive_lines_are_consumed_and_flagged() {
        let (mut buf, n) = buf_with(b"cache-wait\ncache-wait\n");
        let (rest, saw) = consume_cache_wait_lines(&mut buf, n);
        assert!(saw);
        assert_eq!(rest, 0, "complete keepalive lines must be drained");
    }

    #[test]
    fn split_cache_ack_tail_survives_compaction() {
        // "cache-ack" arriving split across reads must not be destroyed by the
        // keepalive drain: the partial tail is preserved so the next read
        // completes it.
        let (mut buf, n) = buf_with(b"cache-wait\ncache-a");
        let (rest, saw) = consume_cache_wait_lines(&mut buf, n);
        assert!(saw);
        assert_eq!(&buf[..rest], b"cache-a");
        // Simulate the next read appending the remainder.
        buf[rest..rest + 3].copy_from_slice(b"ck\n");
        let total = rest + 3;
        let received = std::str::from_utf8(&buf[..total]).unwrap();
        assert!(
            received.contains("cache-ack"),
            "split ack must assemble: {received}"
        );
    }

    #[test]
    fn no_keepalive_means_no_compaction() {
        let (mut buf, n) = buf_with(b"cache-a");
        let (rest, saw) = consume_cache_wait_lines(&mut buf, n);
        assert!(!saw);
        assert_eq!(rest, n, "partial ack untouched when no keepalive present");
        assert_eq!(&buf[..rest], b"cache-a");
    }

    #[test]
    fn repeated_keepalives_never_overflow_buffer() {
        let mut buf = [0u8; 64];
        let mut total = 0usize;
        for _ in 0..50 {
            let msg = b"cache-wait\n";
            assert!(
                total + msg.len() <= buf.len(),
                "buffer overflow before drain"
            );
            buf[total..total + msg.len()].copy_from_slice(msg);
            total += msg.len();
            let (rest, saw) = consume_cache_wait_lines(&mut buf, total);
            assert!(saw);
            total = rest;
            assert_eq!(total, 0);
        }
    }
}
