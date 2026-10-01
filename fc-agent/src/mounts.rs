use anyhow::{Context, Result};
use std::thread;

use crate::types::{ExtraDiskMount, NfsMount, VolumeMount};

/// Check if a path has an active FUSE mount by examining /proc/self/mountinfo.
fn is_fuse_mounted(path: &str) -> bool {
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    // mountinfo format: 36 35 98:0 /mnt1 /mnt2 rw,... - type source options
    // Field 5 (0-indexed: 4) is the mount point. Match exactly to avoid
    // partial path matches (e.g., "/mnt/data" matching "/mnt/data2").
    mounts.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.get(4) == Some(&path) && (line.contains("fuse") || line.contains("FUSE"))
    })
}

/// Whether `inner` lies inside `outer`, compared path component by component.
fn is_inside(inner: &str, outer: &str) -> bool {
    let (inner, outer) = (std::path::Path::new(inner), std::path::Path::new(outer));
    inner != outer && inner.starts_with(outer)
}

/// The order to mount `volumes` in: groups of indexes into `volumes`, each
/// group mounted and ready before the next one starts.
///
/// A volume whose guest path lies inside another volume's comes in a later
/// group than that volume. Its mount point is a directory of the outer
/// volume, so the outer one has to be mounted first: mounted second, it would
/// cover the inner mount. Volumes in one group do not contain each other and
/// are mounted together.
fn mount_levels(volumes: &[VolumeMount]) -> Vec<Vec<usize>> {
    let mut levels: Vec<Vec<usize>> = Vec::new();
    for (index, volume) in volumes.iter().enumerate() {
        let depth = volumes
            .iter()
            .filter(|outer| is_inside(&volume.guest_path, &outer.guest_path))
            .count();
        if levels.len() <= depth {
            levels.resize_with(depth + 1, Vec::new);
        }
        levels[depth].push(index);
    }
    levels.retain(|level| !level.is_empty());
    levels
}

/// Mount FUSE volumes from host via vsock. Returns list of mounted paths.
///
/// Uses reconnectable mounts: when vsock connections die (e.g., after
/// snapshot), the multiplexer automatically reconnects and re-sends
/// pending requests. The kernel FUSE session stays alive — no remount needed.
///
/// A read-only volume is mounted read-only, so a mount point inside one cannot
/// be created here. fcvm refuses such a plan before it boots the VM unless the
/// mount point is already a directory of that volume
/// (check_mount_points_inside_read_only_maps).
pub fn mount_fuse_volumes(volumes: &[VolumeMount]) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();

    for level in mount_levels(volumes) {
        for &index in &level {
            let vol = &volumes[index];
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
                    return Err(e)
                        .with_context(|| format!("creating mount point: {}", vol.guest_path));
                }
            }

            let path = vol.guest_path.clone();
            let port = vol.vsock_port;
            let read_only = vol.read_only;

            thread::spawn(move || {
                eprintln!("[fc-agent] fuse: starting reconnectable mount at {}", path);
                if let Err(e) = crate::fuse::mount_vsock_reconnectable(port, &path, read_only) {
                    eprintln!("[fc-agent] FUSE mount error at {}: {}", path, e);
                }
                eprintln!("[fc-agent] fuse: mount at {} exited", path);
            });

            mounted_paths.push(vol.guest_path.clone());
        }

        // Wait for each FUSE mount of this group to become accessible (up to
        // 30s per mount) before the next group creates its mount points
        // inside them.
        for &index in &level {
            let vol = &volumes[index];
            let path = std::path::Path::new(&vol.guest_path);
            let mut ready = false;
            for attempt in 1..=60 {
                // Check both read_dir (mount is functional) AND mountinfo (mount is FUSE,
                // not just an empty directory after a failed mount attempt)
                if is_fuse_mounted(&vol.guest_path) && std::fs::read_dir(path).is_ok() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn volumes(guest_paths: &[&str]) -> Vec<VolumeMount> {
        guest_paths
            .iter()
            .enumerate()
            .map(|(index, guest_path)| VolumeMount {
                guest_path: guest_path.to_string(),
                vsock_port: 5000 + index as u32,
                read_only: false,
            })
            .collect()
    }

    /// The outer volume is mounted, and ready, before a volume inside it,
    /// whatever order the plan lists them in.
    ///
    /// RED BEFORE THE FIX: every volume was started at once in plan order.
    /// Which of two nested mounts landed first was a race between their
    /// threads, and when the outer one landed second it covered the inner one.
    #[test]
    fn a_volume_inside_another_is_mounted_after_it() {
        let cases: Vec<(Vec<&str>, Vec<Vec<usize>>)> = vec![
            (vec!["/a", "/b"], vec![vec![0, 1]]),
            (vec!["/a", "/a/b"], vec![vec![0], vec![1]]),
            (vec!["/a/b", "/a"], vec![vec![1], vec![0]]),
            (
                vec!["/a/b/c", "/x", "/a/b", "/a"],
                vec![vec![1, 3], vec![2], vec![0]],
            ),
            // Compared by path component: /ab is not inside /a, and a trailing
            // slash changes nothing.
            (vec!["/a", "/ab"], vec![vec![0, 1]]),
            (vec!["/a/b/", "/a/"], vec![vec![1], vec![0]]),
        ];
        for (guest_paths, want) in cases {
            assert_eq!(
                mount_levels(&volumes(&guest_paths)),
                want,
                "the groups {guest_paths:?} are mounted in"
            );
        }
    }
}
