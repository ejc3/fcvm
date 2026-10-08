use anyhow::{Context, Result};
use std::thread;

use crate::types::{ExtraDiskMount, NfsMount, PmemMount, VolumeMount};

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
/// (checked_volume_mappings in src/commands/podman/types.rs).
///
/// Each volume goes into `record` once its mount is ready, which is after every volume
/// of its level has started, so a later volume of the same level can already cover it.
pub fn mount_fuse_volumes(
    volumes: &[VolumeMount],
    record: &mut MountRecord,
) -> Result<Vec<String>> {
    mount_in_levels(volumes, start_fuse_mount, |volume| {
        wait_for_fuse_mount(volume)?;
        record.record("FUSE volume", &volume.guest_path)
    })
}

/// Start the mounts of `volumes` one group of `mount_levels` at a time, and
/// wait for every mount of a group before the next group starts: the next
/// group creates its mount points inside this one's mounts. Returns the guest
/// paths in the order they were started.
fn mount_in_levels(
    volumes: &[VolumeMount],
    mut start: impl FnMut(&VolumeMount) -> Result<()>,
    mut wait_until_ready: impl FnMut(&VolumeMount) -> Result<()>,
) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();
    for level in mount_levels(volumes) {
        for &index in &level {
            start(&volumes[index])?;
            mounted_paths.push(volumes[index].guest_path.clone());
        }
        for &index in &level {
            wait_until_ready(&volumes[index])?;
        }
    }
    Ok(mounted_paths)
}

/// Create the volume's mount point and start its mount on a thread of its own.
fn start_fuse_mount(vol: &VolumeMount) -> Result<()> {
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
    let read_only = vol.read_only;

    thread::spawn(move || {
        eprintln!("[fc-agent] fuse: starting reconnectable mount at {}", path);
        if let Err(e) = crate::fuse::mount_vsock_reconnectable(port, &path, read_only) {
            eprintln!("[fc-agent] FUSE mount error at {}: {}", path, e);
        }
        eprintln!("[fc-agent] fuse: mount at {} exited", path);
    });
    Ok(())
}

/// Wait for the volume's FUSE mount to become accessible (up to 30s).
fn wait_for_fuse_mount(vol: &VolumeMount) -> Result<()> {
    let path = std::path::Path::new(&vol.guest_path);
    for attempt in 1..=60 {
        // Check both read_dir (mount is functional) AND mountinfo (mount is FUSE,
        // not just an empty directory after a failed mount attempt)
        if is_fuse_mounted(&vol.guest_path) && std::fs::read_dir(path).is_ok() {
            eprintln!(
                "[fc-agent] mount {} ready ({}ms)",
                vol.guest_path,
                (attempt - 1) * 500
            );
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(anyhow::anyhow!(
        "mount {} not accessible after 30s",
        vol.guest_path
    ))
}

/// Wait about 5 s (10 checks, 500 ms apart) for a device node to appear.
fn wait_for_device(device: &str) -> Result<()> {
    let device_path = std::path::Path::new(device);
    for attempt in 1..=10 {
        if device_path.exists() {
            return Ok(());
        }
        if attempt == 10 {
            break;
        }
        eprintln!(
            "[fc-agent] waiting for device {} (attempt {}/10)",
            device, attempt
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    anyhow::bail!("Device {} not found after 10 attempts", device)
}

/// Mount options for a pmem device. `noload` keeps ext4 from replaying a journal
/// on a device the host mapped read-only: on aarch64 a guest write to it stops the
/// VM. `dax=always` maps file data straight from the device, so it never enters the
/// guest page cache.
const PMEM_MOUNT_OPTIONS: &str = "ro,noload,dax=always";

/// Arguments to `mount` for one pmem device.
fn pmem_mount_args(pmem: &PmemMount) -> Vec<String> {
    vec![
        "-t".to_string(),
        "ext4".to_string(),
        "-o".to_string(),
        PMEM_MOUNT_OPTIONS.to_string(),
        pmem.device.clone(),
        pmem.mount_path.clone(),
    ]
}

/// Undo the octal escapes /proc/mounts uses for space, tab, newline and backslash.
fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = (bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b)))
        .then(|| {
            u8::try_from(
                bytes[i + 1..i + 4]
                    .iter()
                    .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0')),
            )
            .ok()
        })
        .flatten();
        if let Some(byte) = octal {
            out.push(byte);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Fail unless the mount at `mount_path` in a /proc/mounts listing has DAX on.
/// Without DAX, file pages land in guest RAM and the memory snapshot, which is
/// what --pmem exists to avoid, so a mount that does not show it is an error
/// whatever the reason. The last entry for the path is the one in effect.
fn check_dax_mount(proc_mounts: &str, mount_path: &str) -> Result<()> {
    // /proc/mounts records the target with symlinks resolved, so compare the resolved
    // path; a path that does not resolve is compared as given.
    let resolved =
        std::fs::canonicalize(mount_path).unwrap_or_else(|_| std::path::PathBuf::from(mount_path));
    let wanted = resolved.as_path();
    let options = proc_mounts
        .lines()
        .rev()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let _source = fields.next()?;
            let target = unescape_mount_field(fields.next()?);
            let _fstype = fields.next()?;
            let options = fields.next()?;
            (std::path::Path::new(&target) == wanted).then(|| options.to_string())
        })
        .with_context(|| format!("{mount_path} is not in /proc/mounts"))?;
    anyhow::ensure!(
        options.split(',').any(|o| o == "dax" || o == "dax=always"),
        "pmem mount {mount_path} does not have DAX on (options: {options}); the image \
         must be ext4 with 4 KiB blocks (mkfs.ext4 -b 4096)"
    );
    Ok(())
}

/// The mounts fc-agent makes before the container starts, each with the mount its path
/// reaches when it is recorded. A FUSE volume is recorded once its mount is ready, after
/// every volume of its level has started; every other mount is recorded right after it
/// is made. `check_none_covered` runs after the last one and consumes the record.
#[derive(Debug, Default)]
pub struct MountRecord {
    mounts: Vec<RecordedMount>,
}

/// One mount fc-agent made.
#[derive(Debug)]
struct RecordedMount {
    /// What was mounted, as an error names it, such as "pmem /dev/pmem0".
    what: String,
    path: String,
    /// The ID of the mount `path` reached when it was recorded, which is the first
    /// field of that mount's line in /proc/self/mountinfo.
    mount_id: u64,
    /// The O_PATH descriptor `mount_id` was read from. It holds the mount, so the kernel
    /// cannot give its ID to another mount before the check, and a plain umount of it
    /// fails with EBUSY until the check drops the record.
    _pin: std::fs::File,
}

impl MountRecord {
    /// Record the mount fc-agent made at `path`. `what` names it in an error.
    pub fn record(&mut self, what: impl Into<String>, path: &str) -> Result<()> {
        let pinned = open_path(path).and_then(|file| file_mount_id(&file).map(|id| (file, id)));
        let (pin, mount_id) =
            pinned.with_context(|| format!("reading which mount {path} reaches"))?;
        self.mounts.push(RecordedMount {
            what: what.into(),
            path: path.to_string(),
            mount_id,
            _pin: pin,
        });
        Ok(())
    }

    /// Fail if a mount fc-agent made is covered or gone. A symlink in the guest, such as
    /// Ubuntu's /var/run -> /run, lets two guest paths that differ as text name one
    /// directory, which the host's check of the guest paths cannot see. Consuming the
    /// record closes the descriptors that hold its mounts, so the plain umount of the
    /// extra disks at shutdown does not fail with EBUSY.
    pub fn check_none_covered(self) -> Result<()> {
        mounts_visible(&self.mounts, path_mount_id, || {
            std::fs::read_to_string("/proc/self/mountinfo")
        })
    }
}

/// The ID of the mount `path` reaches. It follows symlinks, as the mount and the
/// container's bind mount of the path do. A mount ID names one mount, while st_dev names
/// a filesystem, which several mounts can show. It is read from the mnt_id line of
/// /proc/self/fdinfo for an O_PATH descriptor, because the libc crate does not expose
/// statx to the musl build.
fn path_mount_id(path: &str) -> std::io::Result<u64> {
    file_mount_id(&open_path(path)?)
}

/// An O_PATH descriptor for `path`, following symlinks. While it is open it holds the
/// mount it reaches.
fn open_path(path: &str) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(path)
}

/// The ID of the mount `file` is on, from the mnt_id line of its /proc/self/fdinfo entry.
fn file_mount_id(file: &std::fs::File) -> std::io::Result<u64> {
    use std::os::fd::AsRawFd;
    let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "fdinfo has no mnt_id line")
        })
}

/// Fail if two records reached one mount when they were recorded, or a recorded path no
/// longer reaches the mount it reached then, or can no longer be read. `mount_of` reads
/// the ID of the mount a path reaches, and `mountinfo` reads /proc/self/mountinfo, which
/// names a mount by its ID.
fn mounts_visible(
    mounts: &[RecordedMount],
    mount_of: impl Fn(&str) -> std::io::Result<u64>,
    mountinfo: impl Fn() -> std::io::Result<String>,
) -> Result<()> {
    // Mount `id` as an error names it.
    let describe = |id: u64| match mountinfo() {
        Ok(listing) => mountinfo_entry(&listing, id)
            .unwrap_or_else(|| format!("mount {id}, which /proc/self/mountinfo does not list")),
        Err(error) => format!("mount {id} (/proc/self/mountinfo cannot be read: {error})"),
    };
    // Each mount fc-agent makes is a new mount, so two records on one mount mean that one
    // of them is covered or was never mounted.
    for (at, first) in mounts.iter().enumerate() {
        if let Some(second) = mounts[at + 1..]
            .iter()
            .find(|later| later.mount_id == first.mount_id)
        {
            anyhow::bail!(
                "{} at {} and {} at {} both reached {} when fc-agent recorded them, so one \
                 of them is covered or was never mounted. Two mounts at one guest path, or \
                 at two paths a symlink in the guest joins, cannot both be reached. Mount \
                 one of them somewhere else.",
                first.what,
                first.path,
                second.what,
                second.path,
                describe(first.mount_id)
            );
        }
    }
    for mount in mounts {
        let now = match mount_of(&mount.path) {
            Ok(id) if id == mount.mount_id => continue,
            Ok(id) => id,
            Err(error) => anyhow::bail!(
                "{} at {} cannot be read after fc-agent's last mount: {error}. A mount made \
                 after it may cover a directory on the way to it, possibly through a symlink \
                 in the guest such as Ubuntu's /var/run -> /run.",
                mount.what,
                mount.path
            ),
        };
        let reached = describe(now);
        let gone =
            mountinfo().is_ok_and(|listing| mountinfo_entry(&listing, mount.mount_id).is_none());
        if gone {
            anyhow::bail!(
                "{} at {} is no longer mounted: /proc/self/mountinfo no longer lists mount \
                 {}, and the path now reaches {reached}.",
                mount.what,
                mount.path,
                mount.mount_id
            );
        }
        anyhow::bail!(
            "{} at {}: a later mount now covers it: the path reaches {reached}. A mount \
             made after this one is at or above this path, possibly through a symlink in \
             the guest such as Ubuntu's /var/run -> /run. Mount one of them somewhere else.",
            mount.what,
            mount.path
        );
    }
    Ok(())
}

/// The filesystem type, source and mount point of mount `id` in a /proc/self/mountinfo
/// listing, as "ext4 /dev/pmem1 at /run/cache".
fn mountinfo_entry(mountinfo: &str, id: u64) -> Option<String> {
    mountinfo.lines().find_map(|line| {
        // The mount ID, parent ID, major:minor, root, mount point, options and optional
        // fields, then "-", the filesystem type, the source and the superblock options.
        let (mount, filesystem) = line.split_once(" - ")?;
        let mut fields = mount.split_whitespace();
        if fields.next()?.parse::<u64>().ok()? != id {
            return None;
        }
        let mount_point = unescape_mount_field(fields.nth(3)?);
        let mut filesystem = filesystem.split_whitespace();
        let fstype = filesystem.next()?;
        let source = unescape_mount_field(filesystem.next()?);
        Some(format!("{fstype} {source} at {mount_point}"))
    })
}

/// Mount read-only pmem devices with DAX and check /proc/mounts shows it. Returns
/// the mounted paths. Each device goes into `record` once it is mounted.
pub fn mount_pmem_devices(devices: &[PmemMount], record: &mut MountRecord) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();
    for pmem in devices {
        if let Err(error) = mount_pmem_device(pmem, &mut mounted_paths, record) {
            // The boot fails; leave none of the devices mounted so far behind.
            unmount_paths(&mounted_paths, "pmem device");
            return Err(error);
        }
    }
    Ok(mounted_paths)
}

/// Mount one pmem device with DAX and check /proc/mounts shows it. The path is
/// recorded as soon as the device is mounted, so a failed check unmounts it too.
fn mount_pmem_device(
    pmem: &PmemMount,
    mounted_paths: &mut Vec<String>,
    record: &mut MountRecord,
) -> Result<()> {
    eprintln!(
        "[fc-agent] mounting pmem {} at {} ({PMEM_MOUNT_OPTIONS})",
        pmem.device, pmem.mount_path
    );
    std::fs::create_dir_all(&pmem.mount_path)
        .with_context(|| format!("creating mount point: {}", pmem.mount_path))?;
    wait_for_device(&pmem.device)?;
    let output = std::process::Command::new("mount")
        .args(pmem_mount_args(pmem))
        .output()
        .with_context(|| format!("mounting {} at {}", pmem.device, pmem.mount_path))?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to mount {} at {}: {}",
            pmem.device,
            pmem.mount_path,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    mounted_paths.push(pmem.mount_path.clone());
    let proc_mounts = std::fs::read_to_string("/proc/mounts").context("reading /proc/mounts")?;
    check_dax_mount(&proc_mounts, &pmem.mount_path)?;
    record.record(format!("pmem {}", pmem.device), &pmem.mount_path)?;
    eprintln!(
        "[fc-agent] pmem {} mounted at {} with DAX",
        pmem.device, pmem.mount_path
    );
    Ok(())
}

/// Mount extra block devices. Returns list of mounted paths. Each disk goes into
/// `record` once it is mounted.
pub fn mount_extra_disks(
    disks: &[ExtraDiskMount],
    record: &mut MountRecord,
) -> Result<Vec<String>> {
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

        wait_for_device(&disk.device)?;

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
        record.record(format!("extra disk {}", disk.device), &disk.mount_path)?;
    }

    Ok(mounted_paths)
}

/// Mount NFS shares from host. Returns list of mounted paths. On a boot each share goes
/// into `record` once it is mounted. A restore remounts the shares with no record: it
/// reproduces the mounts the boot's check passed.
pub fn mount_nfs_shares(
    shares: &[NfsMount],
    mut record: Option<&mut MountRecord>,
) -> Result<Vec<String>> {
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
        if let Some(record) = record.as_deref_mut() {
            record.record(format!("NFS share {nfs_source}"), &share.mount_path)?;
        }
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

    #[test]
    fn pmem_mount_args_are_read_only_ext4_with_dax() {
        let args = pmem_mount_args(&PmemMount {
            device: "/dev/pmem1".to_string(),
            mount_path: "/mnt/cache".to_string(),
        });
        assert_eq!(
            args,
            [
                "-t",
                "ext4",
                "-o",
                "ro,noload,dax=always",
                "/dev/pmem1",
                "/mnt/cache"
            ]
        );
    }

    /// /proc/mounts records a mount target with symlinks resolved, so a mount path through
    /// a symlinked directory (Ubuntu's /var/run is /run) has to match its resolved form.
    #[test]
    fn pmem_dax_check_follows_a_symlinked_mount_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mounts = format!(
            "/dev/pmem0 {} ext4 ro,relatime,dax=always 0 0\n",
            real.canonicalize().unwrap().display()
        );
        check_dax_mount(&mounts, link.to_str().unwrap()).unwrap();
    }

    /// Lines from a guest's /proc/mounts: the DAX pmem mount, the same image
    /// mounted without DAX, and a mount point with a space in it.
    #[test]
    fn pmem_dax_check_reads_the_mount_options() {
        let mounts = "\
/dev/vda / ext4 rw,relatime 0 0
/dev/pmem0 /mnt/cache ext4 ro,relatime,norecovery,dax=always 0 0
/dev/pmem1 /mnt/plain ext4 ro,relatime,norecovery 0 0
/dev/pmem2 /mnt/never ext4 ro,relatime,dax=never 0 0
/dev/pmem3 /mnt/with\\040space ext4 ro,relatime,dax=always 0 0
";
        check_dax_mount(mounts, "/mnt/cache").unwrap();
        check_dax_mount(mounts, "/mnt/cache/").unwrap();
        check_dax_mount(mounts, "/mnt/with space").unwrap();
        for path in ["/mnt/plain", "/mnt/never"] {
            let error = format!("{:#}", check_dax_mount(mounts, path).unwrap_err());
            assert!(error.contains("does not have DAX on"), "{path}: {error}");
        }
        let error = format!("{:#}", check_dax_mount(mounts, "/mnt/absent").unwrap_err());
        assert!(error.contains("not in /proc/mounts"), "{error}");

        // A later mount over the same path is the one in effect.
        let covered = format!("{mounts}tmpfs /mnt/cache tmpfs rw 0 0\n");
        assert!(check_dax_mount(&covered, "/mnt/cache").is_err());
    }

    fn recorded(what: &str, path: &str, mount_id: u64) -> RecordedMount {
        RecordedMount {
            what: what.to_string(),
            path: path.to_string(),
            mount_id,
            // Any descriptor: these records are checked against made-up mount IDs.
            _pin: std::fs::File::open("/").unwrap(),
        }
    }

    /// The mounts the tests below name, as /proc/self/mountinfo lists them.
    const MOUNTINFO: &str = "\
1 0 254:0 / / rw,relatime shared:1 - ext4 /dev/vda rw
30 1 254:16 / /run/data ro,relatime shared:5 - ext4 /dev/vdb ro
31 30 259:0 / /run/data ro,relatime shared:6 - ext4 /dev/pmem0 ro,dax=always
32 1 259:1 / /mnt/tools ro,relatime shared:7 - ext4 /dev/pmem1 ro,dax=always
40 1 0:45 / /run/cache rw,relatime - nfs4 10.0.2.2:/srv/my\\040share rw
50 1 0:52 / /data rw,nosuid,nodev,relatime - fuse fuse-pipe rw,user_id=0,group_id=0
";

    /// Run the check with each recorded path reaching the mount `now` gives it, or its
    /// recorded mount when `now` does not name the path: `Some(id)` reaches mount `id`,
    /// `None` cannot be read.
    fn check_with(mounts: &[RecordedMount], now: &[(&str, Option<u64>)]) -> Result<()> {
        mounts_visible(
            mounts,
            |path| match now.iter().find(|(changed, _)| *changed == path) {
                Some((_, Some(id))) => Ok(*id),
                Some((_, None)) => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                None => Ok(mounts
                    .iter()
                    .find(|mount| mount.path == path)
                    .expect("the check reads only recorded paths")
                    .mount_id),
            },
            || Ok(MOUNTINFO.to_string()),
        )
    }

    fn assert_names(error: &str, wanted: &[&str]) {
        for wanted in wanted {
            assert!(
                error.contains(wanted),
                "{wanted:?} is missing from: {error}"
            );
        }
    }

    /// A recorded path that reaches another mount after fc-agent's last mount is covered
    /// by a later one. The error names what was covered, its path, and the filesystem
    /// type, source and mount point that cover it. A path that cannot be read is reported
    /// as that, and a recorded mount that /proc/self/mountinfo no longer lists as gone,
    /// not as covered.
    #[test]
    fn a_mount_covered_by_a_later_mount_fails_the_boot() {
        let mounted = [
            recorded("pmem /dev/pmem0", "/var/run/cache", 31),
            recorded("pmem /dev/pmem1", "/mnt/tools", 32),
        ];
        check_with(&mounted, &[]).expect("no later mount covers either device");

        // An NFS share mounted at /run/cache reaches /var/run/cache through Ubuntu's
        // /var/run -> /run symlink and covers the pmem device there.
        let error = format!(
            "{:#}",
            check_with(&mounted, &[("/var/run/cache", Some(40))]).unwrap_err()
        );
        assert_names(
            &error,
            &[
                "pmem /dev/pmem0 at /var/run/cache: a later mount now covers it",
                "nfs4 10.0.2.2:/srv/my share at /run/cache",
            ],
        );

        let error = format!(
            "{:#}",
            check_with(&mounted, &[("/mnt/tools", Some(99))]).unwrap_err()
        );
        assert_names(
            &error,
            &[
                "pmem /dev/pmem1 at /mnt/tools: a later mount now covers it",
                "mount 99, which /proc/self/mountinfo does not list",
            ],
        );

        let error = format!(
            "{:#}",
            check_with(&mounted, &[("/mnt/tools", None)]).unwrap_err()
        );
        assert_names(&error, &["pmem /dev/pmem1 at /mnt/tools cannot be read"]);
        assert!(!error.contains("now covers it"), "{error}");

        // Mount 35 is unmounted and the path falls through to the root filesystem.
        let gone = [recorded("extra disk /dev/vdc", "/srv/scratch", 35)];
        let error = format!(
            "{:#}",
            check_with(&gone, &[("/srv/scratch", Some(1))]).unwrap_err()
        );
        assert_names(
            &error,
            &[
                "extra disk /dev/vdc at /srv/scratch is no longer mounted",
                "no longer lists mount 35",
                "the path now reaches ext4 /dev/vda at /",
            ],
        );
        assert!(!error.contains("covers it"), "{error}");

        mounts_visible(
            &[],
            |path| panic!("read {path} with nothing mounted"),
            || panic!("read /proc/self/mountinfo with nothing mounted"),
        )
        .expect("with nothing mounted there is nothing to check");
    }

    /// fc-agent mounts extra disks before pmem devices, so a pmem device at /var/run/data
    /// covers a disk at /run/data through the symlink, while /var/run/data itself still
    /// reaches the pmem device. Only the disk's own path shows it.
    #[test]
    fn an_earlier_disk_covered_by_a_later_pmem_device_fails_the_boot() {
        let mounted = [
            recorded("extra disk /dev/vdb", "/run/data", 30),
            recorded("pmem /dev/pmem0", "/var/run/data", 31),
        ];
        let error = format!(
            "{:#}",
            check_with(&mounted, &[("/run/data", Some(31))]).unwrap_err()
        );
        assert_names(
            &error,
            &[
                "extra disk /dev/vdb at /run/data: a later mount now covers it",
                "ext4 /dev/pmem0 at /run/data",
            ],
        );
    }

    /// A mount inside an earlier one covers nothing: with a disk at /data and a pmem
    /// device at /data/cache, /data still reaches the disk, and the boot goes on.
    #[test]
    fn a_mount_inside_an_earlier_one_still_boots() {
        let mounted = [
            recorded("extra disk /dev/vdb", "/data", 30),
            recorded("pmem /dev/pmem0", "/data/cache", 31),
        ];
        check_with(&mounted, &[]).expect("a pmem device inside a disk covers nothing");
    }

    /// Two mounts fc-agent made never reach one mount. Two FUSE volumes at /data start in
    /// one level and are recorded once both have started, so both can record the second
    /// one's mount while the first is covered or detached, and every path still reaches
    /// what it recorded.
    #[test]
    fn two_records_on_one_mount_fail_the_boot() {
        let mounted = [
            recorded("FUSE volume", "/data", 50),
            recorded("FUSE volume", "/data", 50),
        ];
        let error = format!("{:#}", check_with(&mounted, &[]).unwrap_err());
        assert_names(
            &error,
            &["FUSE volume at /data and FUSE volume at /data both reached fuse fuse-pipe at /data"],
        );
    }

    /// The check reads a recorded path through its symlinks, as the container's bind
    /// mount does, so a path whose symlink now leads to another mount is covered, and
    /// the error names that mount from the real /proc/self/mountinfo.
    #[test]
    fn the_check_reads_a_recorded_path_through_its_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let link = link.to_str().unwrap();
        assert_ne!(
            path_mount_id("/proc").unwrap(),
            path_mount_id(real.to_str().unwrap()).unwrap(),
            "the test needs /proc on another mount than the temporary directory"
        );

        let mut record = MountRecord::default();
        record.record("pmem /dev/pmem0", link).unwrap();
        record
            .check_none_covered()
            .expect("the path still reaches the same mount");

        let mut record = MountRecord::default();
        record.record("pmem /dev/pmem0", link).unwrap();
        std::fs::remove_file(link).unwrap();
        std::os::unix::fs::symlink("/proc", link).unwrap();
        let error = format!("{:#}", record.check_none_covered().unwrap_err());
        assert_names(
            &error,
            &[
                "a later mount now covers it",
                "the path reaches proc proc at /proc",
            ],
        );
    }

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

    /// What `mount_in_levels` did with `guest_paths`: every start and every
    /// wait, in order, and what it returned.
    fn events_of(
        guest_paths: &[&str],
        failing_start: Option<&str>,
    ) -> (Vec<String>, Result<Vec<String>>) {
        let events = std::cell::RefCell::new(Vec::new());
        let result = mount_in_levels(
            &volumes(guest_paths),
            |volume| {
                events
                    .borrow_mut()
                    .push(format!("start {}", volume.guest_path));
                anyhow::ensure!(
                    failing_start != Some(volume.guest_path.as_str()),
                    "cannot start {}",
                    volume.guest_path
                );
                Ok(())
            },
            |volume| {
                events
                    .borrow_mut()
                    .push(format!("ready {}", volume.guest_path));
                Ok(())
            },
        );
        (events.into_inner(), result)
    }

    /// The outer volume is started and ready before the volume inside it is
    /// started, whatever order the plan lists them in. Volumes that do not
    /// contain each other are started together and then waited for. A start
    /// that fails stops everything after it.
    ///
    /// RED BEFORE THE SPLIT: that a group is ready before the next one starts
    /// was pinned only by a VM test whose red depended on which mount thread
    /// won. With every volume started before any wait, the first case reads
    /// start /a, start /a/b, ready /a, ready /a/b.
    #[test]
    fn a_group_of_volumes_is_ready_before_the_next_group_starts() {
        let (events, result) = events_of(&["/a/b", "/a"], None);
        assert_eq!(events, ["start /a", "ready /a", "start /a/b", "ready /a/b"]);
        assert_eq!(result.unwrap(), ["/a", "/a/b"]);

        let (events, result) = events_of(&["/a", "/b"], None);
        assert_eq!(events, ["start /a", "start /b", "ready /a", "ready /b"]);
        assert_eq!(result.unwrap(), ["/a", "/b"]);

        let (events, result) = events_of(&["/a/b", "/a", "/c"], Some("/c"));
        assert_eq!(events, ["start /a", "start /c"]);
        assert_eq!(result.unwrap_err().to_string(), "cannot start /c");
    }
}
