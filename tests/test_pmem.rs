//! virtio-pmem integration test (--pmem).
//!
//! Builds an ext4 image with 4 KiB blocks holding a random file, attaches it with
//! `--pmem IMAGE:/mnt/cache:ro`, and checks in the guest that the mount has DAX on,
//! that the file reads back with the host's sha256, and that reading it does not
//! grow the guest's page cache by the file's size (DAX maps the data from the
//! device, so it stays out of guest RAM and the memory snapshot).
//!
//! Two more boots must fail before the VM turns healthy: one where a second pmem image
//! covers the first through the guest's /var/run -> /run symlink, and one where a pmem
//! image covers an extra disk the same way.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{bail, ensure, Context, Result};
use std::io::Read;
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
    let dir = tempfile::tempdir()?;
    let (image, host_sha) = build_image(dir.path()).await?;
    let spec = format!("{}:{GUEST_MOUNT}:ro", image.display());

    let (vm_name, _, _, _) = common::unique_names("pmem-dax");
    let (mut child, pid) = common::spawn_fcvm_with_logs(
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
    result
}

/// fc-agent fails the boot when a later mount covers a pmem device through a symlink in
/// the guest rootfs. Ubuntu's /var/run is a symlink to /run, so a second pmem image at
/// /run/cache covers the first at /var/run/cache. The host compares the two guest paths
/// as text and lets the run start; after its last mount fc-agent finds that
/// /var/run/cache no longer reaches the first device's mount.
#[tokio::test]
async fn test_pmem_covered_through_a_guest_symlink_fails_the_boot() -> Result<()> {
    let (first_dir, second_dir) = (tempfile::tempdir()?, tempfile::tempdir()?);
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
    let (pmem_dir, disk_dir) = (tempfile::tempdir()?, tempfile::tempdir()?);
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

/// A snapshot of a VM with a pmem image restores with the image mounted with DAX and
/// its file intact, and a restore refuses the image once it is rewritten in place at
/// the same length: the guest's cached view of the filesystem in the memory image
/// describes the old contents.
#[tokio::test]
async fn test_pmem_restores_and_refuses_a_rewritten_image() -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use std::time::Duration;
    let dir = tempfile::tempdir()?;
    let (image, host_sha) = build_image(dir.path()).await?;
    let spec = format!("{}:{GUEST_MOUNT}:ro", image.display());
    let (baseline, clone, snapshot, _) = common::unique_names("pmem-restore");
    let refused_name = format!("{clone}-rewritten");
    let fcvm_path = common::find_fcvm_binary()?;

    // VM processes still running, killed afterwards whatever the outcome.
    let mut running: Vec<u32> = Vec::new();
    let result = async {
        let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
            &[
                "podman",
                "run",
                "--name",
                &baseline,
                "--pmem",
                &spec,
                common::TEST_IMAGE,
            ],
            &baseline,
        )
        .await
        .context("spawning the baseline")?;
        running.push(baseline_pid);
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

        let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
            &["snapshot", "run", "--snapshot", &snapshot, "--name", &clone],
            &clone,
        )
        .await
        .context("spawning the clone")?;
        running.push(clone_pid);
        common::poll_health_by_pid(clone_pid, 120).await?;
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
        let guest_sha =
            common::exec_in_vm(clone_pid, &[&format!("sha256sum {GUEST_MOUNT}/blob.bin")]).await?;
        ensure!(
            guest_sha.split_whitespace().next() == Some(host_sha.as_str()),
            "the clone read {guest_sha:?}, the host wrote {host_sha}"
        );
        common::kill_process(clone_pid).await;
        running.retain(|pid| *pid != clone_pid);

        // Rewrite one block in place: the same inode and length, new contents and mtime.
        {
            let mut file = std::fs::OpenOptions::new().write(true).open(&image)?;
            file.seek(SeekFrom::Start(IMAGE_MIB << 19))?;
            file.write_all(&[0x5a; 4096])?;
            file.sync_all()?;
        }
        ensure!(
            std::fs::metadata(&image)?.len() == IMAGE_MIB << 20,
            "the rewrite changed the image length"
        );
        let (mut refused, refused_pid, refused_log) = common::spawn_fcvm_with_log_path(
            &["snapshot", "run", "--snapshot", &snapshot, "--name", &refused_name],
            &refused_name,
        )
        .await
        .context("spawning the restore over the rewritten image")?;
        running.push(refused_pid);
        // The restore has to refuse: exit with an error before any clone turns healthy. A
        // health poll that errors (the process exited) disables its branch.
        let status = tokio::time::timeout(Duration::from_secs(150), async {
            tokio::select! {
                status = refused.wait() => status.map_err(anyhow::Error::from),
                Ok(()) = common::poll_health_by_pid(refused_pid, 120) => Err(anyhow::anyhow!(
                    "a restore over a pmem image rewritten at the same length started a healthy clone"
                )),
            }
        })
        .await
        .context("the restore over the rewritten image neither exited nor turned healthy within 150 s")??;
        running.retain(|pid| *pid != refused_pid);
        ensure!(
            !status.success(),
            "a restore over a pmem image rewritten at the same length exited successfully"
        );
        common::wait_for_log_eof(&refused_log, Duration::from_secs(30)).await?;
        let log = std::fs::read_to_string(&refused_log)?;
        ensure!(
            log.contains("snapshot pmem image changed"),
            "the refused restore does not name the changed image; log {}",
            refused_log.display()
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
    result
}
