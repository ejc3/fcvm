//! virtio-pmem integration tests (--pmem).
//!
//! Each test builds an ext4 image with 4 KiB blocks holding a random file and attaches it
//! with `--pmem IMAGE:/mnt/cache:ro`. The first checks in the guest that the mount has DAX
//! on, that the file reads back with the host's sha256, and that reading it does not grow
//! the guest's page cache by the file's size (DAX maps the data from the device, so it
//! stays out of guest RAM and the memory snapshot). fcvm maps a copy of the image from its
//! store under `<data_dir>/pmem`; three more check that writes to the image after a run
//! started reach neither that VM nor a restore of its snapshot, and that two runs of one
//! image map one copy.
//!
//! Two more boots must fail before the VM turns healthy: one where a second pmem image
//! covers the first through the guest's /var/run -> /run symlink, and one where a pmem
//! image covers an extra disk the same way.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{bail, ensure, Context, Result};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const FILE_MIB: u64 = 64;
/// Image length: room for the file plus filesystem metadata, a multiple of 2 MiB.
const IMAGE_MIB: u64 = 128;
const GUEST_MOUNT: &str = "/mnt/cache";

async fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Build `cache.ext4` under `dir` holding one random file. Returns the image path
/// and the file's sha256.
async fn build_image(dir: &Path) -> Result<(PathBuf, String)> {
    let content = dir.join("content");
    std::fs::create_dir(&content)?;
    let file = content.join("blob.bin");
    let mut random = Vec::with_capacity((FILE_MIB << 20) as usize);
    std::fs::File::open("/dev/urandom")?
        .take(FILE_MIB << 20)
        .read_to_end(&mut random)?;
    std::fs::write(&file, &random)?;
    let sha = run("sha256sum", &[file.to_str().unwrap()]).await?;
    let sha = sha.split_whitespace().next().context("sha256sum output")?;

    let image = dir.join("cache.ext4");
    std::fs::File::create(&image)?.set_len(IMAGE_MIB << 20)?;
    run(
        "mkfs.ext4",
        &[
            "-q",
            "-F",
            "-b",
            "4096",
            "-d",
            content.to_str().unwrap(),
            image.to_str().unwrap(),
        ],
    )
    .await?;
    ensure!(
        std::fs::metadata(&image)?.len() == IMAGE_MIB << 20,
        "mkfs.ext4 changed the image length"
    );
    Ok((image, sha.to_string()))
}

/// A temp directory for a test's pmem image under /mnt/fcvm-btrfs, which every test
/// environment mounts. fcvm refuses an image on tmpfs or overlayfs, and the system temp
/// directory can be either (the CI container's /tmp is overlayfs). Fails unless fcvm
/// accepts the directory's filesystem.
fn image_dir() -> Result<tempfile::TempDir> {
    let base = Path::new("/mnt/fcvm-btrfs");
    let kind = nix::sys::statfs::statfs(base)
        .with_context(|| format!("statfs {}", base.display()))?
        .filesystem_type();
    if let Some((name, _)) = fcvm::storage::pmem_store::unverifiable_filesystem(kind) {
        bail!(
            "{} is on {name}, where fcvm refuses a --pmem image",
            base.display()
        );
    }
    tempfile::tempdir_in(base)
        .with_context(|| format!("creating a temp directory in {}", base.display()))
}

/// The guest's `Cached:` in KiB.
async fn guest_cached_kib(pid: u32) -> Result<u64> {
    let out = common::exec_in_vm(pid, &["grep '^Cached:' /proc/meminfo"]).await?;
    out.split_whitespace()
        .nth(1)
        .context("Cached line")?
        .parse()
        .with_context(|| format!("parsing {out:?}"))
}

#[tokio::test]
async fn test_pmem_dax_keeps_file_data_out_of_guest_ram() -> Result<()> {
    let dir = image_dir()?;
    let (image, host_sha) = build_image(dir.path()).await?;
    let spec = format!("{}:{GUEST_MOUNT}:ro", image.display());

    let (vm_name, _, _, _) = common::unique_names("pmem-dax");
    let (mut child, pid, log_path) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--pmem",
            &spec,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning fcvm podman run --pmem")?;

    let result = async {
        common::poll_health_by_pid(pid, 300).await?;

        let mounts = common::exec_in_vm(pid, &["cat /proc/mounts"]).await?;
        let line = mounts
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some(GUEST_MOUNT))
            .with_context(|| format!("{GUEST_MOUNT} not mounted:\n{mounts}"))?;
        let options = line.split_whitespace().nth(3).unwrap_or_default();
        ensure!(
            line.starts_with("/dev/pmem0 ") && options.split(',').any(|o| o == "dax=always"),
            "pmem mount is not /dev/pmem0 with dax=always: {line}"
        );

        let before = guest_cached_kib(pid).await?;
        let guest_sha =
            common::exec_in_vm(pid, &[&format!("sha256sum {GUEST_MOUNT}/blob.bin")]).await?;
        let after = guest_cached_kib(pid).await?;
        ensure!(
            guest_sha.split_whitespace().next() == Some(host_sha.as_str()),
            "guest sha256 {guest_sha:?} does not match the host's {host_sha}"
        );
        let grown_kib = after.saturating_sub(before);
        let file_kib = FILE_MIB << 10;
        ensure!(
            grown_kib < file_kib / 4,
            "reading the {FILE_MIB} MiB file grew the guest's Cached by {grown_kib} KiB \
             ({before} -> {after}); with DAX it should not enter the page cache"
        );
        println!("pmem DAX: Cached {before} -> {after} KiB after reading {file_kib} KiB");

        let in_container =
            common::exec_in_container(pid, &[&format!("sha256sum {GUEST_MOUNT}/blob.bin")]).await?;
        ensure!(
            in_container.split_whitespace().next() == Some(host_sha.as_str()),
            "container sha256 {in_container:?} does not match the host's {host_sha}"
        );
        Ok(())
    }
    .await;

    common::kill_process(pid).await;
    child.wait().await.ok();
    remove_logged_store_copies(&log_path).await;
    result
}

/// Run a VM that has to fail before it turns healthy, and require one line of its log
/// to hold every string in `wanted`.
async fn assert_boot_fails(args: &[&str], vm_name: &str, wanted: &[&str]) -> Result<()> {
    use std::time::Duration;
    let (mut child, pid, log_path) = common::spawn_fcvm_with_log_path(args, vm_name)
        .await
        .context("spawning fcvm")?;

    let result = async {
        // The run exits with an error before the VM turns healthy. A health poll that
        // errors (the process exited) disables its branch.
        let status = tokio::time::timeout(Duration::from_secs(300), async {
            tokio::select! {
                status = child.wait() => status.map_err(anyhow::Error::from),
                Ok(()) = common::poll_health_by_pid(pid, 280) => {
                    Err(anyhow::anyhow!("the VM turned healthy"))
                }
            }
        })
        .await
        .context("the run neither exited nor turned healthy within 300 s")??;
        ensure!(!status.success(), "the run exited successfully");
        common::wait_for_log_eof(&log_path, Duration::from_secs(30)).await?;
        let log = std::fs::read_to_string(&log_path)?;
        ensure!(
            log.lines()
                .any(|line| wanted.iter().all(|wanted| line.contains(wanted))),
            "no line of the log holds all of {wanted:?}; log {}",
            log_path.display()
        );
        Ok(())
    }
    .await;

    // Signal fcvm only while it is unreaped: once child.wait() has reaped it, the
    // kernel can give its PID to another process.
    if matches!(child.try_wait(), Ok(None)) {
        common::kill_process(pid).await;
        child.wait().await.ok();
    }
    remove_logged_store_copies(&log_path).await;
    result
}

/// fc-agent fails the boot when a later mount covers a pmem device through a symlink in
/// the guest rootfs. Ubuntu's /var/run is a symlink to /run, so a second pmem image at
/// /run/cache covers the first at /var/run/cache. The host compares the two guest paths
/// as text and lets the run start; after its last mount fc-agent finds that
/// /var/run/cache no longer reaches the first device's mount.
#[tokio::test]
async fn test_pmem_covered_through_a_guest_symlink_fails_the_boot() -> Result<()> {
    let (first_dir, second_dir) = (image_dir()?, image_dir()?);
    let (first, _) = build_image(first_dir.path()).await?;
    let (second, _) = build_image(second_dir.path()).await?;
    let first = format!("{}:/var/run/cache:ro", first.display());
    let second = format!("{}:/run/cache:ro", second.display());
    let (vm_name, _, _, _) = common::unique_names("pmem-covered");
    assert_boot_fails(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--pmem",
            &first,
            "--pmem",
            &second,
            common::TEST_IMAGE,
        ],
        &vm_name,
        &[
            "pmem /dev/pmem0 at /var/run/cache: a later mount now covers it",
            "ext4 /dev/pmem1 at /run/cache",
        ],
    )
    .await
}

/// fc-agent fails the boot when a pmem device covers an extra disk mounted before it
/// through a symlink in the guest rootfs. Extra disks mount before pmem devices, so a
/// pmem image at /var/run/data covers a disk at /run/data, while /var/run/data itself
/// still reaches the pmem device. The host compares the guest paths as text and lets
/// the run start.
#[tokio::test]
async fn test_pmem_covering_an_earlier_disk_through_a_guest_symlink_fails_the_boot() -> Result<()> {
    let (pmem_dir, disk_dir) = (image_dir()?, tempfile::tempdir()?);
    let (image, _) = build_image(pmem_dir.path()).await?;
    let (disk, _) = build_image(disk_dir.path()).await?;
    let pmem = format!("{}:/var/run/data:ro", image.display());
    let disk = format!("{}:/run/data:ro", disk.display());
    let (vm_name, _, _, _) = common::unique_names("pmem-over-disk");
    assert_boot_fails(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--disk",
            &disk,
            "--pmem",
            &pmem,
            common::TEST_IMAGE,
        ],
        &vm_name,
        &[
            "extra disk /dev/",
            " at /run/data: a later mount now covers it",
            "ext4 /dev/pmem0 at /run/data",
        ],
    )
    .await
}

/// Byte offset in `image` of the 4 KiB block holding the first 4 KiB of `file`, which
/// `build_image` filled with random bytes, so exactly one block of the image matches.
fn block_offset_of(image: &Path, file: &Path) -> Result<usize> {
    let mut needle = vec![0u8; 4096];
    std::fs::File::open(file)?.read_exact(&mut needle)?;
    let bytes = std::fs::read(image)?;
    bytes
        .chunks_exact(4096)
        .position(|block| block == needle.as_slice())
        .map(|index| index * 4096)
        .with_context(|| {
            format!(
                "the first block of {} is not in {}",
                file.display(),
                image.display()
            )
        })
}

/// Overwrite the 4 KiB block at `offset` of `image` through a host MAP_SHARED mapping.
fn overwrite_through_shared_mapping(image: &Path, offset: usize) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)?;
    // SAFETY: the test owns the image and nothing truncates it while it is mapped.
    let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
    map[offset..offset + 4096].fill(0x5a);
    Ok(())
}

/// The sha256 of the random file as the guest reads it through the pmem mount.
async fn guest_sha(pid: u32) -> Result<String> {
    let out = common::exec_in_vm(pid, &[&format!("sha256sum {GUEST_MOUNT}/blob.bin")]).await?;
    Ok(out
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

/// The pmem devices `fcvm ls` records for the VM of fcvm process `pid`.
async fn pmem_devices(pid: u32) -> Result<Vec<serde_json::Value>> {
    let out = tokio::process::Command::new(common::find_fcvm_binary()?)
        .args(["ls", "--json", "--pid", &pid.to_string()])
        .output()
        .await
        .context("running fcvm ls")?;
    ensure!(
        out.status.success(),
        "fcvm ls failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let vms: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout)?;
    let vm = vms
        .first()
        .with_context(|| format!("fcvm ls lists no VM for pid {pid}"))?;
    Ok(vm["config"]["pmem_devices"]
        .as_array()
        .with_context(|| format!("no pmem_devices for pid {pid}: {vm}"))?
        .clone())
}

/// Whether `path` is shaped like a pmem store entry, `<data_dir>/pmem/<64 hex>.img`.
fn is_store_entry(path: &Path) -> bool {
    path.parent().and_then(Path::file_name) == Some("pmem".as_ref())
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".img"))
            .is_some_and(|hex| {
                hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
}

/// Remove the store copies the VMs mapped, so the tests do not leave them in the data
/// directory: nothing collects store entries yet. Only a path shaped like an entry is
/// removed, so the image under a test's own directory never is.
fn remove_store_copies(copies: &[String]) {
    for copy in copies {
        if is_store_entry(Path::new(copy)) {
            std::fs::remove_file(copy).ok();
        }
    }
}

/// Remove the store copies a run's debug log names once both of the log's streams have
/// ended, so a run that fails before it turns healthy, or before `fcvm ls` lists its
/// devices, does not leave its copy behind.
async fn remove_logged_store_copies(log_path: &Path) {
    if let Err(error) = common::wait_for_log_eof(log_path, std::time::Duration::from_secs(30)).await
    {
        println!("  reading the store copies from a log that may be incomplete: {error:#}");
    }
    remove_store_copies(&store_copies_logged(log_path));
}

/// The store entries a run's debug log says it copied an image into or found there.
fn store_copies_logged(log_path: &Path) -> Vec<String> {
    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    let mut copies: Vec<String> = log
        .lines()
        .filter(|line| line.contains("pmem store"))
        .flat_map(str::split_whitespace)
        .filter_map(|field| field.strip_prefix("entry="))
        .map(str::to_string)
        .collect();
    copies.sort();
    copies.dedup();
    copies
}

/// The Firecracker processes descended from fcvm process `pid`.
fn firecracker_pids_under(pid: u32) -> Result<Vec<u32>> {
    let mut children: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    for entry in std::fs::read_dir("/proc")? {
        let Some(child) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // A process can exit between the readdir and the read.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else {
            continue;
        };
        // The fields after the command name, which ends at the last ')': state, then ppid.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|ppid| ppid.parse::<u32>().ok());
        if let Some(ppid) = ppid {
            children.entry(ppid).or_default().push(child);
        }
    }
    let mut found = Vec::new();
    let mut queue = vec![pid];
    while let Some(parent) = queue.pop() {
        for &child in children.get(&parent).map(Vec::as_slice).unwrap_or_default() {
            let comm = std::fs::read_to_string(format!("/proc/{child}/comm")).unwrap_or_default();
            if comm.starts_with("firecracker") {
                found.push(child);
            }
            queue.push(child);
        }
    }
    Ok(found)
}

/// The device and inode of every mapping of `path` in /proc/<pid>/maps.
fn mappings_of(pid: u32, path: &str) -> Result<Vec<(String, u64)>> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .with_context(|| format!("reading /proc/{pid}/maps"))?;
    let mut found = Vec::new();
    for line in maps.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 6 && fields[5..].join(" ") == path {
            found.push((fields[3].to_string(), fields[4].parse()?));
        }
    }
    Ok(found)
}

/// A write to the image made after the VM started does not reach the guest: the guest
/// maps fcvm's copy of the image, not the image itself.
#[tokio::test]
async fn test_pmem_guest_does_not_see_writes_to_the_source() -> Result<()> {
    let dir = image_dir()?;
    let (image, host_sha) = build_image(dir.path()).await?;
    let spec = format!("{}:{GUEST_MOUNT}:ro", image.display());
    let (vm_name, _, _, _) = common::unique_names("pmem-source-write");
    let (mut child, pid, log_path) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--no-snapshot",
            "--pmem",
            &spec,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning fcvm podman run --pmem")?;

    let result = async {
        common::poll_health_by_pid(pid, 300).await?;
        let before = guest_sha(pid).await?;
        ensure!(
            before == host_sha,
            "the guest read {before}, the host wrote {host_sha}"
        );

        let offset = block_offset_of(&image, &dir.path().join("content/blob.bin"))?;
        overwrite_through_shared_mapping(&image, offset)?;
        let mut block = vec![0u8; 4096];
        {
            use std::os::unix::fs::FileExt;
            std::fs::File::open(&image)?.read_exact_at(&mut block, offset as u64)?;
        }
        ensure!(
            block.iter().all(|byte| *byte == 0x5a),
            "the host write did not land in the image"
        );

        let after = guest_sha(pid).await?;
        ensure!(
            after == host_sha,
            "the guest read a write made to the image after it started: sha256 {after}, \
             the image held {host_sha} when the run started"
        );
        Ok(())
    }
    .await;

    common::kill_process(pid).await;
    child.wait().await.ok();
    remove_logged_store_copies(&log_path).await;
    result
}

/// A run under sudo with the default rootless networking hands its store entry to the
/// user who ran sudo, mode 0400, and Firecracker, which runs inside the holder's user
/// namespace, can still open it. Inside a user namespace the capability that overrides
/// file permissions covers only a file whose owner and group are mapped there, so the
/// namespace has to map that user. The root test runner unsets SUDO_USER, so this test
/// sets it to the user who owns the checkout, as `sudo fcvm` run by that user would.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_pmem_sudo_run_with_rootless_networking_maps_its_entry() -> Result<()> {
    ensure!(
        nix::unistd::geteuid().is_root(),
        "this test needs root: run it with make test-root"
    );
    let owner = std::fs::metadata(env!("CARGO_MANIFEST_DIR"))?.uid();
    let invoker = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(owner))?
        .with_context(|| format!("no passwd entry for uid {owner}, the checkout's owner"))?;
    ensure!(
        !invoker.uid.is_root(),
        "the checkout is owned by root, so no user can stand in for the one who ran sudo"
    );
    let dir = image_dir()?;
    let (image, host_sha) = build_image(dir.path()).await?;
    let spec = format!("{}:{GUEST_MOUNT}:ro", image.display());
    let (vm_name, _, _, _) = common::unique_names("pmem-sudo");
    let (mut child, pid, log_path) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--no-snapshot",
            "--pmem",
            &spec,
            common::TEST_IMAGE,
        ],
        &[("SUDO_USER", invoker.name.as_str())],
    )
    .await
    .context("spawning fcvm podman run --pmem with SUDO_USER set")?;

    let mut copies = Vec::new();
    let result = async {
        common::poll_health_by_pid(pid, 300).await?;
        for device in pmem_devices(pid).await? {
            copies.extend(device["path"].as_str().map(str::to_string));
        }
        ensure!(copies.len() == 1, "fcvm ls lists the copies {copies:?}");
        let metadata = std::fs::metadata(&copies[0])?;
        ensure!(
            (metadata.uid(), metadata.gid()) == (invoker.uid.as_raw(), invoker.gid.as_raw()),
            "{} is owned by {}:{}, not by {} ({}:{})",
            copies[0],
            metadata.uid(),
            metadata.gid(),
            invoker.name,
            invoker.uid,
            invoker.gid
        );
        ensure!(
            metadata.mode() & 0o7777 == 0o400,
            "{} has mode {:o}",
            copies[0],
            metadata.mode() & 0o7777
        );
        let sha = guest_sha(pid).await?;
        ensure!(
            sha == host_sha,
            "the guest read {sha}, the host wrote {host_sha}"
        );
        Ok(())
    }
    .await;

    common::kill_process(pid).await;
    child.wait().await.ok();
    remove_logged_store_copies(&log_path).await;
    result
}

/// Two runs of one image map one file, and it is not the image: the same device and inode
/// in both Firecracker processes' /proc/<pid>/maps, an inode other than the image's, and
/// `fcvm ls` names the image as the device's source.
#[tokio::test]
async fn test_pmem_runs_of_one_source_map_one_store_entry() -> Result<()> {
    let dir = image_dir()?;
    let (image, _) = build_image(dir.path()).await?;
    let image = image.canonicalize()?;
    let image_text = image.display().to_string();
    let image_inode = std::fs::metadata(&image)?.ino();
    let spec = format!("{image_text}:{GUEST_MOUNT}:ro");
    let names = [
        common::unique_names("pmem-share-a").0,
        common::unique_names("pmem-share-b").0,
    ];

    let mut running: Vec<(tokio::process::Child, u32)> = Vec::new();
    let mut logs: Vec<PathBuf> = Vec::new();
    let result = async {
        for name in &names {
            let (child, pid, log_path) = common::spawn_fcvm_with_log_path(
                &[
                    "podman",
                    "run",
                    "--name",
                    name,
                    "--no-snapshot",
                    "--pmem",
                    &spec,
                    common::TEST_IMAGE,
                ],
                name,
            )
            .await
            .with_context(|| format!("spawning {name}"))?;
            running.push((child, pid));
            logs.push(log_path);
        }
        let pids: Vec<u32> = running.iter().map(|(_, pid)| *pid).collect();
        for pid in &pids {
            common::poll_health_by_pid(*pid, 300).await?;
        }

        let mut mapped = Vec::new();
        for pid in &pids {
            let firecracker = firecracker_pids_under(*pid)?;
            ensure!(
                firecracker.len() == 1,
                "fcvm {pid} has Firecracker processes {firecracker:?}, expected one"
            );
            let firecracker = firecracker[0];
            let of_image = mappings_of(firecracker, &image_text)?;
            ensure!(
                of_image.is_empty(),
                "Firecracker {firecracker} maps the image {image_text} itself: {of_image:?}"
            );
            let devices = pmem_devices(*pid).await?;
            ensure!(devices.len() == 1, "fcvm ls lists {devices:?}");
            let path = devices[0]["path"]
                .as_str()
                .context("pmem device without a path")?
                .to_string();
            ensure!(
                devices[0]["source"].as_str() == Some(image_text.as_str()),
                "the device's source is not the image: {}",
                devices[0]
            );
            let of_copy = mappings_of(firecracker, &path)?;
            ensure!(
                !of_copy.is_empty(),
                "Firecracker {firecracker} does not map {path}"
            );
            mapped.push((path, of_copy[0].clone()));
        }
        ensure!(
            mapped[0] == mapped[1],
            "two runs of one image map different files: {mapped:?}"
        );
        ensure!(
            mapped[0].1 .1 != image_inode,
            "the mapped file has the image's inode {image_inode}"
        );
        println!(
            "pmem store: both runs map {} ({}:{})",
            mapped[0].0, mapped[0].1 .0, mapped[0].1 .1
        );
        Ok(())
    }
    .await;

    for (child, pid) in running.iter_mut().rev() {
        common::kill_process(*pid).await;
        child.wait().await.ok();
    }
    for log_path in &logs {
        remove_logged_store_copies(log_path).await;
    }
    result
}

/// A snapshot of a VM with a pmem image restores after the image is rewritten in place,
/// with the image mounted with DAX and the file as it was when the snapshot was taken:
/// the snapshot maps fcvm's copy of the image.
#[tokio::test]
async fn test_pmem_restores_the_snapshot_image_after_the_source_is_rewritten() -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let dir = image_dir()?;
    let (image, host_sha) = build_image(dir.path()).await?;
    let spec = format!("{}:{GUEST_MOUNT}:ro", image.display());
    let (baseline, clone, snapshot, _) = common::unique_names("pmem-restore");
    let fcvm_path = common::find_fcvm_binary()?;

    // VM processes still running, killed afterwards whatever the outcome.
    let mut running: Vec<u32> = Vec::new();
    let mut baseline_log = None;
    let result = async {
        let (_baseline_child, baseline_pid, log_path) = common::spawn_fcvm_with_log_path(
            &[
                "podman",
                "run",
                "--name",
                &baseline,
                "--no-snapshot",
                "--pmem",
                &spec,
                common::TEST_IMAGE,
            ],
            &baseline,
        )
        .await
        .context("spawning the baseline")?;
        running.push(baseline_pid);
        baseline_log = Some(log_path);
        common::poll_health_by_pid(baseline_pid, 300).await?;
        let created = tokio::process::Command::new(&fcvm_path)
            .args([
                "snapshot",
                "create",
                "--pid",
                &baseline_pid.to_string(),
                "--tag",
                &snapshot,
            ])
            .output()
            .await
            .context("running snapshot create")?;
        ensure!(
            created.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        common::kill_process(baseline_pid).await;
        running.retain(|pid| *pid != baseline_pid);

        // Rewrite the random file's first block in place: the same inode and length, new
        // contents, modification and change time.
        let offset = block_offset_of(&image, &dir.path().join("content/blob.bin"))?;
        {
            let mut file = std::fs::OpenOptions::new().write(true).open(&image)?;
            file.seek(SeekFrom::Start(offset as u64))?;
            file.write_all(&[0x5a; 4096])?;
            file.sync_all()?;
        }
        ensure!(
            std::fs::metadata(&image)?.len() == IMAGE_MIB << 20,
            "the rewrite changed the image length"
        );

        let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
            &["snapshot", "run", "--snapshot", &snapshot, "--name", &clone],
            &clone,
        )
        .await
        .context("spawning the clone")?;
        running.push(clone_pid);
        common::poll_health_by_pid(clone_pid, 120)
            .await
            .context("the restore after the image was rewritten did not start a healthy clone")?;
        let mounts = common::exec_in_vm(clone_pid, &["cat /proc/mounts"]).await?;
        let line = mounts
            .lines()
            .find(|line| line.split_whitespace().nth(1) == Some(GUEST_MOUNT))
            .with_context(|| format!("{GUEST_MOUNT} is not mounted in the clone:\n{mounts}"))?;
        let options = line.split_whitespace().nth(3).unwrap_or("");
        ensure!(
            line.starts_with("/dev/pmem0 ") && options.split(',').any(|o| o == "dax=always"),
            "the clone's pmem mount is not /dev/pmem0 with dax=always: {line}"
        );
        let clone_sha = guest_sha(clone_pid).await?;
        ensure!(
            clone_sha == host_sha,
            "the clone read {clone_sha}; the image held {host_sha} when the snapshot was taken"
        );
        Ok(())
    }
    .await;

    for pid in running.into_iter().rev() {
        common::kill_process(pid).await;
    }
    if let Err(error) = common::delete_snapshot(&snapshot).await {
        println!("  could not delete snapshot {snapshot}: {error:#}");
    }
    if let Some(log_path) = &baseline_log {
        remove_logged_store_copies(log_path).await;
    }
    result
}
