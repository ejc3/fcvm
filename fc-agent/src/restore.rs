use std::thread;
use tokio::{
    process::Command,
    time::{sleep, Duration},
};

use crate::network;
use crate::output::OutputHandle;
use crate::types::VolumeMount;

/// Handle clone restore: kill stale sockets, flush ARP, reconnect output, remount volumes.
pub async fn handle_clone_restore(volumes: &[VolumeMount], output: &OutputHandle) {
    network::kill_stale_tcp_connections().await;
    network::flush_arp_cache().await;
    network::send_gratuitous_arp().await;

    // Reconnect output vsock (broken by snapshot vsock reset)
    output.reconnect();
    eprintln!("[fc-agent] signaled output vsock reconnect after restore");

    if !volumes.is_empty() {
        eprintln!(
            "[fc-agent] clone has {} volume(s) to remount",
            volumes.len()
        );
        remount_fuse_volumes(volumes).await;
    }
}

/// Remount FUSE volumes after clone restore.
pub async fn remount_fuse_volumes(volumes: &[VolumeMount]) {
    // Wait for vsock transport reset to complete
    sleep(Duration::from_millis(500)).await;

    for vol in volumes {
        for attempt in 0..3 {
            if attempt > 0 {
                eprintln!(
                    "[fc-agent] retrying remount of {} (attempt {})",
                    vol.guest_path,
                    attempt + 1
                );
                sleep(Duration::from_millis(500)).await;
            }

            eprintln!(
                "[fc-agent] remounting volume at {} (port {})",
                vol.guest_path, vol.vsock_port
            );

            let umount_output = Command::new("umount")
                .args(["-l", &vol.guest_path])
                .output()
                .await;

            match umount_output {
                Ok(o) if o.status.success() => {
                    eprintln!("[fc-agent] unmounted old FUSE mount at {}", vol.guest_path);
                }
                Ok(o) => {
                    eprintln!(
                        "[fc-agent] umount {} (may not be mounted): {}",
                        vol.guest_path,
                        String::from_utf8_lossy(&o.stderr).trim()
                    );
                }
                Err(e) => {
                    eprintln!("[fc-agent] umount error for {}: {}", vol.guest_path, e);
                }
            }

            sleep(Duration::from_millis(100)).await;

            if let Err(e) = std::fs::create_dir_all(&vol.guest_path) {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    eprintln!(
                        "[fc-agent] ERROR: cannot create mount point {}: {}",
                        vol.guest_path, e
                    );
                    break;
                }
            }

            let mount_path = vol.guest_path.clone();
            let port = vol.vsock_port;

            thread::spawn(move || {
                eprintln!("[fc-agent] fuse: starting remount at {}", mount_path);
                if let Err(e) = crate::fuse::mount_vsock(port, &mount_path) {
                    eprintln!("[fc-agent] FUSE remount error at {}: {}", mount_path, e);
                }
                eprintln!("[fc-agent] fuse: remount at {} exited", mount_path);
            });

            eprintln!("[fc-agent] volume {} remount initiated", vol.guest_path);

            sleep(Duration::from_millis(500)).await;

            if std::fs::metadata(&vol.guest_path).is_ok() {
                eprintln!("[fc-agent] volume {} remount verified", vol.guest_path);
                break;
            } else {
                eprintln!(
                    "[fc-agent] volume {} mount not accessible after remount",
                    vol.guest_path
                );
            }
        }
    }

    if volumes.is_empty() {
        return;
    }

    rebind_volumes_in_container(volumes).await;
    eprintln!("[fc-agent] volume remounts complete");
}

/// Rebind new FUSE mounts into the container's mount namespace.
async fn rebind_volumes_in_container(volumes: &[VolumeMount]) {
    let pid_output = match Command::new("podman")
        .args(["inspect", "--format", "{{.State.Pid}}", "fcvm-container"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(_) => {
            eprintln!("[fc-agent] container not running, skipping mount rebind");
            return;
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] podman inspect failed: {}, skipping mount rebind",
                e
            );
            return;
        }
    };

    let container_pid = String::from_utf8_lossy(&pid_output.stdout)
        .trim()
        .to_string();
    if container_pid.is_empty() || container_pid == "0" {
        eprintln!("[fc-agent] container PID is 0, skipping mount rebind");
        return;
    }

    for vol in volumes {
        let pid = container_pid.clone();
        let path = vol.guest_path.clone();

        let result = tokio::task::spawn_blocking(move || rebind_mount_cross_ns(&pid, &path)).await;

        match result {
            Ok(Ok(())) => {
                eprintln!(
                    "[fc-agent] volume {} rebound in container namespace",
                    vol.guest_path
                );
            }
            Ok(Err(e)) => {
                eprintln!(
                    "[fc-agent] WARNING: rebind {} in container failed: {}",
                    vol.guest_path, e
                );
            }
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARNING: rebind task failed for {}: {}",
                    vol.guest_path, e
                );
            }
        }
    }
}

/// Rebind a FUSE mount from root namespace into container's mount namespace.
/// Uses fork + open_tree + move_mount (async-signal-safe).
fn rebind_mount_cross_ns(container_pid: &str, guest_path: &str) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;

    const SYS_OPEN_TREE: libc::c_long = libc::SYS_open_tree;
    const SYS_MOVE_MOUNT: libc::c_long = libc::SYS_move_mount;
    const OPEN_TREE_CLONE: libc::c_ulong = 1;
    const MOVE_MOUNT_F_EMPTY_PATH: libc::c_ulong = 4;

    let path_c = CString::new(guest_path).map_err(|e| format!("invalid path: {}", e))?;

    let tree_fd = unsafe {
        libc::syscall(
            SYS_OPEN_TREE,
            libc::AT_FDCWD,
            path_c.as_ptr(),
            OPEN_TREE_CLONE,
        )
    };
    if tree_fd < 0 {
        return Err(format!(
            "open_tree({}) failed: {}",
            guest_path,
            std::io::Error::last_os_error()
        ));
    }
    let tree_fd = tree_fd as libc::c_int;

    let ns_path = format!("/proc/{}/ns/mnt", container_pid);
    let root_path = format!("/proc/{}/root", container_pid);

    let ns_file = std::fs::File::open(&ns_path).map_err(|e| {
        unsafe { libc::close(tree_fd) };
        format!("open container mount ns: {}", e)
    })?;
    let root_file = std::fs::File::open(&root_path).map_err(|e| {
        unsafe { libc::close(tree_fd) };
        format!("open container root: {}", e)
    })?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe { libc::close(tree_fd) };
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }

    if pid == 0 {
        // Child: enter container namespace, move mount
        if unsafe { libc::setns(ns_file.as_raw_fd(), libc::CLONE_NEWNS) } != 0 {
            unsafe { libc::_exit(1) };
        }
        if unsafe { libc::fchdir(root_file.as_raw_fd()) } != 0 {
            unsafe { libc::_exit(2) };
        }
        if unsafe { libc::chroot(c".".as_ptr()) } != 0 {
            unsafe { libc::_exit(3) };
        }

        unsafe { libc::umount2(path_c.as_ptr(), libc::MNT_DETACH) };

        let empty = c"".as_ptr();
        let ret = unsafe {
            libc::syscall(
                SYS_MOVE_MOUNT,
                tree_fd,
                empty,
                libc::AT_FDCWD,
                path_c.as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH,
            )
        };

        unsafe { libc::_exit(if ret == 0 { 0 } else { 5 }) };
    }

    // Parent
    unsafe { libc::close(tree_fd) };

    let mut status: libc::c_int = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };

    let exited = (status & 0x7f) == 0;
    let exit_code = (status >> 8) & 0xff;

    if exited && exit_code == 0 {
        Ok(())
    } else {
        Err(format!("rebind child failed (exit code {})", exit_code))
    }
}
