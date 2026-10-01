//! Integration tests for read-only maps (`--map HOST:GUEST:ro`).
//!
//! fc-agent mounts a read-only map read-only and without the FUSE writeback
//! cache. The guest then reports the host's size and mtime for a file the host
//! changes, in a running VM and in a clone restored from a snapshot, and a
//! write from the guest OS fails with EROFS.
//!
//! Every VM here uses --portable-volumes, as the consumer that restores clones
//! hours after its snapshot does. A plain volume goes through the same guest
//! mount; only the host's server differs.
//!
//! Run with: make test-all FILTER="--test test_read_only_map" STREAM=1

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the guest gets to show a change the host made. The attribute
/// timeout is 1 s; the rest is exec round trips on a busy host.
const FOLLOW_LIMIT: Duration = Duration::from_secs(10);

/// An empty host directory for one test.
fn fresh_dir(name: &str) -> Result<PathBuf> {
    let dir = PathBuf::from(format!("/tmp/fcvm-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Start a VM with --portable-volumes and the given maps, and wait until it is healthy.
async fn start_vm(
    vm_name: &str,
    maps: &[String],
    started: &mut Vec<u32>,
) -> Result<(tokio::process::Child, u32)> {
    let mut args = vec![
        "podman",
        "run",
        "--name",
        vm_name,
        "--network",
        "rootless",
        "--portable-volumes",
    ];
    for map in maps {
        args.push("--map");
        args.push(map);
    }
    args.push(common::TEST_IMAGE);
    let (child, pid) = common::spawn_fcvm_with_logs(&args, vm_name)
        .await
        .context("spawning the VM")?;
    started.push(pid);
    common::poll_health_by_pid(pid, 180).await?;
    Ok((child, pid))
}

/// Kill every fcvm process a test started, newest first.
async fn stop_all(started: &[u32]) {
    for pid in started.iter().rev() {
        common::kill_process(*pid).await;
    }
}

/// A file as one side sees it.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    size: u64,
    /// Seconds and nanoseconds, as `stat -c %.9Y` prints them.
    mtime: String,
    content: String,
}

impl std::fmt::Display for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let start: String = self.content.chars().take(24).collect();
        write!(
            f,
            "size={} mtime={} read {} bytes starting {:?}",
            self.size,
            self.mtime,
            self.content.len(),
            start
        )
    }
}

fn host_view(path: &Path) -> Result<Seen> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    Ok(Seen {
        size: meta.len(),
        mtime: format!("{}.{:09}", meta.mtime(), meta.mtime_nsec()),
        content: std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?,
    })
}

/// The same file as the guest OS sees it. One exec, so the stat and the read
/// are of the same moment.
async fn guest_view(pid: u32, path: &str) -> Result<Seen> {
    let out =
        common::exec_in_vm(pid, &[&format!("stat -c '%s %.9Y' {path} && cat {path}")]).await?;
    let (stat, content) = out
        .split_once('\n')
        .with_context(|| format!("no stat line for {path} in {out:?}"))?;
    let (size, mtime) = stat
        .split_once(' ')
        .with_context(|| format!("unexpected stat line for {path}: {stat:?}"))?;
    Ok(Seen {
        size: size
            .parse()
            .with_context(|| format!("size of {path} in {stat:?}"))?,
        mtime: mtime.to_string(),
        content: content.to_string(),
    })
}

fn show(seen: &Result<Seen>) -> String {
    match seen {
        Ok(seen) => seen.to_string(),
        Err(e) => format!("error: {e:#}"),
    }
}

/// Each named file whose guest view differs from the host's, worded for an
/// assertion message.
async fn files_not_following(
    pid: u32,
    host_dir: &Path,
    guest_dir: &str,
    names: &[&str],
) -> Vec<String> {
    let mut differing = Vec::new();
    for name in names {
        let host = host_view(&host_dir.join(name));
        let guest = guest_view(pid, &format!("{guest_dir}/{name}")).await;
        let same = matches!((&host, &guest), (Ok(host), Ok(guest)) if host == guest);
        if !same {
            differing.push(format!(
                "{name}: host {}; guest {}",
                show(&host),
                show(&guest)
            ));
        }
    }
    differing
}

/// Poll until the guest shows every named file as the host has it. Returns
/// what still differs when `FOLLOW_LIMIT` runs out, which is nothing when the
/// guest follows the host.
async fn wait_for_guest_to_follow(
    pid: u32,
    host_dir: &Path,
    guest_dir: &str,
    names: &[&str],
) -> Vec<String> {
    let deadline = Instant::now() + FOLLOW_LIMIT;
    loop {
        let differing = files_not_following(pid, host_dir, guest_dir, names).await;
        if differing.is_empty() || Instant::now() >= deadline {
            return differing;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `len` bytes that name their version, so a stale or cut-off read shows
/// which version it is.
fn body(version: &str, len: usize) -> String {
    let mut text = format!("{version}:");
    assert!(text.len() <= len, "{version} does not fit in {len} bytes");
    text.push_str(&".".repeat(len - text.len()));
    text
}

/// Rewrite a file through its existing inode.
fn rewrite_in_place(path: &Path, content: &str) -> Result<()> {
    let before = std::fs::metadata(path)?.ino();
    std::fs::write(path, content)?;
    let after = std::fs::metadata(path)?.ino();
    anyhow::ensure!(
        before == after,
        "{} was not rewritten in place: inode {before} became {after}",
        path.display()
    );
    Ok(())
}

/// Put a new file in a path's place by rename.
fn replace_by_rename(path: &Path, content: &str) -> Result<()> {
    let before = std::fs::metadata(path)?.ino();
    let staged = path.with_extension("staged");
    std::fs::write(&staged, content)?;
    std::fs::rename(&staged, path)?;
    let after = std::fs::metadata(path)?.ino();
    anyhow::ensure!(
        before != after,
        "{} still has inode {before} after the rename",
        path.display()
    );
    Ok(())
}

const FOLLOWED_FILES: [&str; 3] = ["grows.txt", "shrinks.txt", "replaced.txt"];

/// Write version `n` of the three files. Version 0 creates them. Each later
/// version rewrites grows.txt longer in place, rewrites shrinks.txt shorter in
/// place, and replaces replaced.txt by rename with another length.
fn write_version(host_dir: &Path, n: usize) -> Result<()> {
    const LENGTHS: [(usize, usize, usize); 3] = [(8, 200, 8), (92, 20, 57), (134, 5, 301)];
    let (grows, shrinks, replaced) = LENGTHS[n];
    let grows = (host_dir.join("grows.txt"), body(&format!("g{n}"), grows));
    let shrinks = (
        host_dir.join("shrinks.txt"),
        body(&format!("s{n}"), shrinks),
    );
    let replaced = (
        host_dir.join("replaced.txt"),
        body(&format!("r{n}"), replaced),
    );
    if n == 0 {
        for (path, content) in [&grows, &shrinks, &replaced] {
            std::fs::write(path, content)?;
        }
        return Ok(());
    }
    rewrite_in_place(&grows.0, &grows.1)?;
    rewrite_in_place(&shrinks.0, &shrinks.1)?;
    replace_by_rename(&replaced.0, &replaced.1)
}

/// The host rewrites one file longer in place, one shorter in place, and
/// replaces one by rename. The guest's stat and content match the host's within
/// `FOLLOW_LIMIT`, in the running VM and then, after the host changes all three
/// again, in a clone restored from a snapshot taken before that second change.
#[tokio::test]
async fn test_read_only_map_follows_host_in_vm_and_clone() -> Result<()> {
    const GUEST_DIR: &str = "/mnt/ro";
    let (vm_name, clone_name, snap_name, _) = common::unique_names("ro-follow");
    let host_dir = fresh_dir("ro-follow")?;
    write_version(&host_dir, 0)?;

    let mut started: Vec<u32> = Vec::new();
    let result = async {
        let map = format!("{}:{GUEST_DIR}:ro", host_dir.display());
        let (_child, pid) = start_vm(&vm_name, &[map], &mut started).await?;

        // The guest reads each file once, so it holds a size, an mtime and
        // pages for it before the host changes anything.
        let before = wait_for_guest_to_follow(pid, &host_dir, GUEST_DIR, &FOLLOWED_FILES).await;
        anyhow::ensure!(
            before.is_empty(),
            "the guest does not show the files as the host wrote them before boot:\n{}",
            before.join("\n")
        );

        write_version(&host_dir, 1)?;
        let in_vm = wait_for_guest_to_follow(pid, &host_dir, GUEST_DIR, &FOLLOWED_FILES).await;

        // The snapshot holds what the guest cached of version 1. The host
        // writes version 2 before any clone exists.
        common::create_snapshot_by_pid(pid, &snap_name).await?;
        common::kill_process(pid).await;
        started.retain(|p| *p != pid);
        write_version(&host_dir, 2)?;

        let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
        started.push(serve_pid);
        let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name).await?;
        started.push(clone_pid);
        common::poll_health_by_pid(clone_pid, 180).await?;
        let in_clone =
            wait_for_guest_to_follow(clone_pid, &host_dir, GUEST_DIR, &FOLLOWED_FILES).await;

        anyhow::ensure!(
            in_vm.is_empty() && in_clone.is_empty(),
            "a read-only map does not follow the host.\n\
             running VM, {FOLLOW_LIMIT:?} after the host changed the files:\n{}\n\
             clone restored from a snapshot taken before the host changed them again:\n{}",
            in_vm.join("\n"),
            in_clone.join("\n")
        );
        Ok(())
    }
    .await;

    stop_all(&started).await;
    common::delete_snapshot(&snap_name).await.ok();
    std::fs::remove_dir_all(&host_dir).ok();
    result
}

/// The per-mount options and the superblock options of the mount at
/// `mount_point`, from the text of /proc/self/mountinfo.
fn mount_options_of(mountinfo: &str, mount_point: &str) -> Result<(Vec<String>, Vec<String>)> {
    let options = |text: &str| text.split(',').map(str::to_string).collect::<Vec<_>>();
    mountinfo
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(4) != Some(&mount_point) {
                return None;
            }
            // id parent major:minor root mount-point options [tags] - type source super-options
            let separator = fields.iter().position(|field| *field == "-")?;
            Some((options(fields[5]), options(fields.get(separator + 3)?)))
        })
        .next_back()
        .with_context(|| {
            format!("no mount at {mount_point} in the guest's mountinfo:\n{mountinfo}")
        })
}

/// Run a shell command in the guest OS or in the container and return what it
/// printed, with its exit status appended, whether or not it failed.
async fn attempt(pid: u32, in_container: bool, command: &str) -> Result<String> {
    let script = format!("sh -c '{command}' 2>&1; echo status=$?");
    let out = if in_container {
        common::exec_in_container(pid, &[&script]).await?
    } else {
        common::exec_in_vm(pid, &[&script]).await?
    };
    Ok(out.trim().to_string())
}

/// A read-only map is mounted read-only in the guest: its mount table says so,
/// a write from the guest OS fails with EROFS and nothing changes on the host.
/// The container, which had podman's read-only bind already, still cannot write.
#[tokio::test]
async fn test_read_only_map_refuses_guest_writes() -> Result<()> {
    const GUEST_DIR: &str = "/mnt/ro";
    const HOST_CONTENT: &str = "written by the host";
    let (vm_name, _, _, _) = common::unique_names("ro-write");
    let host_dir = fresh_dir("ro-write")?;
    std::fs::write(host_dir.join("existing.txt"), HOST_CONTENT)?;

    let mut started: Vec<u32> = Vec::new();
    let result = async {
        let map = format!("{}:{GUEST_DIR}:ro", host_dir.display());
        let (_child, pid) = start_vm(&vm_name, &[map], &mut started).await?;
        let mut wrong: Vec<String> = Vec::new();

        let mountinfo = common::exec_in_vm(pid, &["cat /proc/self/mountinfo"]).await?;
        let (per_mount, superblock) = mount_options_of(&mountinfo, GUEST_DIR)?;
        if !per_mount.iter().any(|o| o == "ro") || !superblock.iter().any(|o| o == "ro") {
            wrong.push(format!(
                "the guest's mount table has {GUEST_DIR} as {} (superblock {}), not ro",
                per_mount.join(","),
                superblock.join(",")
            ));
        }

        let created = attempt(pid, false, &format!("touch {GUEST_DIR}/created-by-guest")).await?;
        if !created.contains("Read-only file system") {
            wrong.push(format!(
                "creating a file from the guest OS did not fail with EROFS: {created:?}"
            ));
        }
        let overwritten = attempt(
            pid,
            false,
            &format!("echo overwritten by the guest > {GUEST_DIR}/existing.txt"),
        )
        .await?;
        if !overwritten.contains("Read-only file system") {
            wrong.push(format!(
                "overwriting a file from the guest OS did not fail with EROFS: {overwritten:?}"
            ));
        }
        let in_container = attempt(
            pid,
            true,
            &format!("touch {GUEST_DIR}/created-by-container"),
        )
        .await?;
        if !in_container.contains("Read-only file system") {
            wrong.push(format!(
                "creating a file from the container did not fail with EROFS: {in_container:?}"
            ));
        }

        let mut on_host: Vec<String> = std::fs::read_dir(&host_dir)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_>>()?;
        on_host.sort();
        if on_host != ["existing.txt"] {
            wrong.push(format!(
                "the host directory holds {on_host:?} after the guest's writes, not only existing.txt"
            ));
        }
        let existing = std::fs::read_to_string(host_dir.join("existing.txt"))?;
        if existing != HOST_CONTENT {
            wrong.push(format!(
                "existing.txt on the host now reads {existing:?}"
            ));
        }

        anyhow::ensure!(
            wrong.is_empty(),
            "a read-only map is not read-only in the guest:\n{}",
            wrong.join("\n")
        );
        Ok(())
    }
    .await;

    stop_all(&started).await;
    std::fs::remove_dir_all(&host_dir).ok();
    result
}

/// A map whose guest path lies inside a read-only map is mounted over the
/// directory the outer map already has there, whatever order the maps are
/// given in: the guest sees the inner map's files, writes to the inner map
/// reach its own host directory, and the outer map stays read-only.
#[tokio::test]
async fn test_map_inside_read_only_map_is_mounted_over_it() -> Result<()> {
    const OUTER: &str = "/mnt/outer";
    const INNER: &str = "/mnt/outer/inner";
    let (vm_name, _, _, _) = common::unique_names("ro-nested");
    let outer_host = fresh_dir("ro-nested-outer")?;
    let inner_host = fresh_dir("ro-nested-inner")?;
    std::fs::write(outer_host.join("outer.txt"), "outer")?;
    // The inner map's mount point, with a file the inner map must hide.
    std::fs::create_dir(outer_host.join("inner"))?;
    std::fs::write(outer_host.join("inner/under-the-inner-map.txt"), "hidden")?;
    std::fs::write(inner_host.join("inner.txt"), "inner")?;

    let mut started: Vec<u32> = Vec::new();
    let result = async {
        // Inner first: the order on the command line must not decide the order of the mounts.
        let maps = [
            format!("{}:{INNER}", inner_host.display()),
            format!("{}:{OUTER}:ro", outer_host.display()),
        ];
        let (_child, pid) = start_vm(&vm_name, &maps, &mut started).await?;
        let mut wrong: Vec<String> = Vec::new();

        for in_container in [false, true] {
            let listing = attempt(pid, in_container, &format!("ls {INNER}")).await?;
            if listing != "inner.txt\nstatus=0" {
                wrong.push(format!(
                    "{INNER} in the {} does not list the inner map's one file: {listing:?}",
                    if in_container { "container" } else { "guest OS" }
                ));
            }
        }

        let written = attempt(
            pid,
            false,
            &format!("echo from the guest > {INNER}/from-guest.txt"),
        )
        .await?;
        let on_inner_host = std::fs::read_to_string(inner_host.join("from-guest.txt"));
        if written != "status=0" || on_inner_host.as_deref().ok() != Some("from the guest\n") {
            wrong.push(format!(
                "a write to the inner map did not reach its host directory: guest said {written:?}, \
                 the host file reads {on_inner_host:?}"
            ));
        }
        if outer_host.join("inner/from-guest.txt").exists() {
            wrong.push("a write to the inner map landed in the outer map's host directory".into());
        }

        let created = attempt(pid, false, &format!("touch {OUTER}/created-by-guest")).await?;
        if !created.contains("Read-only file system") || outer_host.join("created-by-guest").exists()
        {
            wrong.push(format!(
                "the outer map is writable from the guest OS: {created:?}"
            ));
        }

        anyhow::ensure!(
            wrong.is_empty(),
            "a map inside a read-only map is not mounted over it:\n{}",
            wrong.join("\n")
        );
        Ok(())
    }
    .await;

    stop_all(&started).await;
    std::fs::remove_dir_all(&outer_host).ok();
    std::fs::remove_dir_all(&inner_host).ok();
    result
}
