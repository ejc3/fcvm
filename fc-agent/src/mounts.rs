use anyhow::{Context, Result};
use std::thread;

use crate::types::{ExtraDiskMount, NfsMount, VolumeMount};

/// Mount FUSE volumes from host via vsock. Returns list of mounted paths.
pub fn mount_fuse_volumes(volumes: &[VolumeMount]) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();

    for vol in volumes {
        eprintln!(
            "[fc-agent] mounting FUSE volume at {} via vsock port {}",
            vol.guest_path, vol.vsock_port
        );

        let mount_path = std::path::Path::new(&vol.guest_path);
        if mount_path.exists() {
            eprintln!("[fc-agent] mount point exists, attempting to unmount stale mount...");
            let _ = std::process::Command::new("umount")
                .arg("-l")
                .arg(&vol.guest_path)
                .output();
        }

        if let Err(e) = std::fs::create_dir_all(&vol.guest_path) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e).with_context(|| format!("creating mount point: {}", vol.guest_path));
            }
        }

        let path = vol.guest_path.clone();
        let port = vol.vsock_port;

        thread::spawn(move || {
            eprintln!("[fc-agent] fuse: starting mount at {}", path);
            if let Err(e) = crate::fuse::mount_vsock(port, &path) {
                eprintln!("[fc-agent] FUSE mount error at {}: {}", path, e);
            }
            eprintln!("[fc-agent] fuse: mount at {} exited", path);
        });

        mounted_paths.push(vol.guest_path.clone());
    }

    // Wait for each FUSE mount to become accessible (up to 30s per mount)
    for vol in volumes {
        let path = std::path::Path::new(&vol.guest_path);
        let mut ready = false;
        for attempt in 1..=60 {
            if std::fs::read_dir(path).is_ok() {
                eprintln!(
                    "[fc-agent] mount {} ready ({}ms)",
                    vol.guest_path,
                    (attempt - 1) * 500
                );
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if !ready {
            return Err(anyhow::anyhow!(
                "mount {} not accessible after 30s",
                vol.guest_path
            ));
        }
    }

    Ok(mounted_paths)
}

/// Mount extra block devices. Returns list of mounted paths.
pub fn mount_extra_disks(disks: &[ExtraDiskMount]) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();

    for disk in disks {
        eprintln!(
            "[fc-agent] mounting extra disk {} at {} ({})",
            disk.device,
            disk.mount_path,
            if disk.read_only { "ro" } else { "rw" }
        );

        if let Err(e) = std::fs::create_dir_all(&disk.mount_path) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e)
                    .with_context(|| format!("creating mount point: {}", disk.mount_path));
            }
        }

        let device_path = std::path::Path::new(&disk.device);
        for attempt in 1..=10 {
            if device_path.exists() {
                break;
            }
            if attempt == 10 {
                anyhow::bail!("Device {} not found after 10 attempts", disk.device);
            }
            eprintln!(
                "[fc-agent] waiting for device {} (attempt {}/10)",
                disk.device, attempt
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        let mut mount_cmd = std::process::Command::new("mount");
        if disk.read_only {
            mount_cmd.arg("-o").arg("ro");
        }
        mount_cmd.arg(&disk.device).arg(&disk.mount_path);

        let output = mount_cmd
            .output()
            .with_context(|| format!("mounting {} at {}", disk.device, disk.mount_path))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Failed to mount {} at {}: {}",
                disk.device,
                disk.mount_path,
                stderr
            );
        }

        eprintln!(
            "[fc-agent] extra disk {} mounted at {}",
            disk.device, disk.mount_path
        );
        mounted_paths.push(disk.mount_path.clone());
    }

    Ok(mounted_paths)
}

/// Mount NFS shares from host. Returns list of mounted paths.
pub fn mount_nfs_shares(shares: &[NfsMount]) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();

    for share in shares {
        let nfs_source = format!("{}:{}", share.host_ip, share.host_path);
        eprintln!(
            "[fc-agent] mounting NFS {} at {} ({})",
            nfs_source,
            share.mount_path,
            if share.read_only { "ro" } else { "rw" }
        );

        if let Err(e) = std::fs::create_dir_all(&share.mount_path) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e)
                    .with_context(|| format!("creating NFS mount point: {}", share.mount_path));
            }
        }

        let mut mount_cmd = std::process::Command::new("mount");
        mount_cmd.arg("-t").arg("nfs");

        let opts = if share.read_only {
            "ro,nfsvers=4,nolock"
        } else {
            "rw,nfsvers=4,nolock"
        };
        mount_cmd.arg("-o").arg(opts);
        mount_cmd.arg(&nfs_source).arg(&share.mount_path);

        let output = mount_cmd
            .output()
            .with_context(|| format!("mounting NFS {} at {}", nfs_source, share.mount_path))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Failed to mount NFS {} at {}: {}",
                nfs_source,
                share.mount_path,
                stderr
            );
        }

        eprintln!(
            "[fc-agent] NFS {} mounted at {}",
            nfs_source, share.mount_path
        );
        mounted_paths.push(share.mount_path.clone());
    }

    Ok(mounted_paths)
}

/// Unmount a list of paths with lazy unmount.
pub fn unmount_paths(paths: &[String], label: &str) {
    if paths.is_empty() {
        return;
    }
    eprintln!(
        "[fc-agent] unmounting {} {}(s) before shutdown",
        paths.len(),
        label
    );
    for path in paths {
        eprintln!("[fc-agent] unmounting {} at {}", label, path);
        match std::process::Command::new("umount")
            .arg("-l")
            .arg(path)
            .output()
        {
            Ok(output) => {
                if output.status.success() {
                    eprintln!("[fc-agent] unmounted {}", path);
                } else {
                    eprintln!(
                        "[fc-agent] umount {} failed: {}",
                        path,
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
            Err(e) => {
                eprintln!("[fc-agent] umount {} error: {}", path, e);
            }
        }
    }
}

/// Unmount disk paths (non-lazy).
pub fn unmount_disks(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    eprintln!(
        "[fc-agent] unmounting {} extra disk(s) before shutdown",
        paths.len()
    );
    for path in paths {
        eprintln!("[fc-agent] unmounting extra disk at {}", path);
        match std::process::Command::new("umount").arg(path).output() {
            Ok(output) => {
                if output.status.success() {
                    eprintln!("[fc-agent] unmounted {}", path);
                } else {
                    eprintln!(
                        "[fc-agent] umount {} failed: {}",
                        path,
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
            Err(e) => {
                eprintln!("[fc-agent] umount {} error: {}", path, e);
            }
        }
    }
}

/// Check if FUSE mounts are still healthy after a potential snapshot.
pub async fn check_and_remount_fuse(volumes: &[VolumeMount], mounted_paths: &[String]) {
    if mounted_paths.is_empty() {
        return;
    }
    let mut broken = false;
    for path in mounted_paths {
        if std::fs::metadata(path).is_err() {
            eprintln!(
                "[fc-agent] FUSE mount at {} broken after snapshot (vsock reset), will remount",
                path
            );
            broken = true;
            break;
        }
    }
    if broken {
        crate::restore::remount_fuse_volumes(volumes).await;
    }
}
