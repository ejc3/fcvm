//! `make cargo-target-link` must leave `target/` resolving to a real directory.
//!
//! Every build and test recipe runs cargo with `CARGO_TARGET_DIR=target`, where
//! `target` is a symlink onto btrfs (the root filesystem is small and a link step
//! dies with "No space left on device" mid-build). `/mnt/fcvm-btrfs` on the CI
//! runners is EPHEMERAL and gets reset out from under a checkout that persists
//! across jobs. A dangling link cannot be repaired by Cargo: it builds a tempdir
//! and `rename()`s it onto the path, which fails on an existing symlink with
//! ENOTDIR. Idle-cache reclamation also rotates this symlink to a fresh physical
//! generation so arbitrary build-script caches never reuse retained zero names.
//!
//! That is not a hypothetical. Host-arm64 on #771/#772 (2026-08-08) died with
//!
//! ```text
//! error: failed to create directory `/opt/actions-runner/_work/fcvm/fcvm/fcvm/target`
//! Caused by:
//!   Not a directory (os error 20)
//! ```
//!
//! reproduced exactly by pointing `target` at a path that does not exist. The
//! recipe already self-heals `$HOME/.cargo` for this very reason ("the btrfs above
//! is ephemeral and can be reset out from under us, leaving ~/.cargo a dangling
//! symlink") — `target/` was left without the same treatment.
//!
//! These run the REAL recipe out of the repo's Makefile, with `BTRFS_ROOT` pointed
//! at a temp dir, and assert the postcondition callers depend on: after the target
//! runs, `target/` is a directory cargo can write into. Asserting on the recipe's
//! text instead would pass on a Makefile whose comments merely mention healing.

use std::collections::HashSet;
use std::fs::{FileTimes, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Run the real script with cwd `dir` and the btrfs root redirected.
/// Returns (success, combined output).
///
/// Invoked directly rather than through `make`: the privileged suites run the
/// test binary under `sudo` (CARGO_TARGET_*_RUNNER), so a `make` subprocess would
/// be root and the Makefile refuses that outright ("Do not run make as root"),
/// failing every test here for a reason that has nothing to do with the code.
/// `makefile_delegates_to_the_script` covers the wiring.
fn run_link(dir: &Path, btrfs_root: &Path) -> (bool, String) {
    let out = Command::new(repo_root().join("scripts/cargo-target-link.sh"))
        .env("BTRFS_ROOT", btrfs_root)
        .env_remove("CARGO_TARGET_LINK_LOCKED")
        .current_dir(dir)
        .output()
        .expect("run scripts/cargo-target-link.sh");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// The postcondition every cargo invocation depends on.
fn assert_target_usable(dir: &Path, ctx: &str) {
    let target = dir.join("target");
    assert!(
        target.is_dir(),
        "{ctx}: `target` does not resolve to a directory, so `CARGO_TARGET_DIR=target cargo \
         build` fails with `failed to create directory ... Not a directory (os error 20)` — \
         cargo renames a tempdir onto the path and cannot do that through a symlink. \
         symlink_metadata={:?} readlink={:?}",
        std::fs::symlink_metadata(&target).map(|m| m.file_type()),
        std::fs::read_link(&target)
    );
    // Prove it is writable, not merely stat-able.
    let probe = target.join(".cargo-target-link-probe");
    std::fs::write(&probe, b"x").unwrap_or_else(|e| panic!("{ctx}: target/ is not writable: {e}"));
    let _ = std::fs::remove_file(&probe);
}

fn age_file(path: &Path) {
    set_file_time(path, 946_684_800);
}

fn set_file_time(path: &Path, unix_seconds: u64) {
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(unix_seconds);
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path:?} to age it: {e}"));
    file.set_times(FileTimes::new().set_accessed(old).set_modified(old))
        .unwrap_or_else(|e| panic!("age {path:?}: {e}"));
}

fn age_regular_tree(path: &Path, unix_seconds: u64) {
    for entry in std::fs::read_dir(path).unwrap_or_else(|error| panic!("read {path:?}: {error}")) {
        let entry = entry.unwrap_or_else(|error| panic!("read entry in {path:?}: {error}"));
        let entry_path = entry.path();
        let metadata = std::fs::symlink_metadata(&entry_path)
            .unwrap_or_else(|error| panic!("stat {entry_path:?}: {error}"));
        if metadata.file_type().is_dir() {
            age_regular_tree(&entry_path, unix_seconds);
        } else if metadata.file_type().is_file() {
            set_file_time(&entry_path, unix_seconds);
        }
    }
}

fn unique_regular_blocks(path: &Path) -> u64 {
    fn visit(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
        let mut blocks = 0;
        for entry in
            std::fs::read_dir(path).unwrap_or_else(|error| panic!("read {path:?}: {error}"))
        {
            let entry = entry.unwrap_or_else(|error| panic!("read entry in {path:?}: {error}"));
            let entry_path = entry.path();
            let metadata = std::fs::symlink_metadata(&entry_path)
                .unwrap_or_else(|error| panic!("stat {entry_path:?}: {error}"));
            if metadata.file_type().is_dir() {
                blocks += visit(&entry_path, seen);
            } else if metadata.file_type().is_file()
                && seen.insert((metadata.dev(), metadata.ino()))
            {
                blocks += metadata.blocks() * 512;
            }
        }
        blocks
    }

    visit(path, &mut HashSet::new())
}

fn run_target_pruner(btrfs_root: &Path, runner_work_root: &Path) -> (bool, String) {
    let out = Command::new(repo_root().join("scripts/runner-disk-preflight.sh"))
        .arg("--target-dirs-only")
        .env("BTRFS_ROOT", btrfs_root)
        .env("RUNNER_WORK_ROOT", runner_work_root)
        .env("TARGET_AGE_DAYS", "1")
        .output()
        .expect("run target-dir preflight");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already reaped")
    }

    fn wait(mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child_mut().wait()?;
        self.0 = None;
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for_path(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Fresh checkout: the link gets created, per-worktree.
#[test]
fn cargo_target_link_creates_a_per_worktree_link() {
    let work = tempfile::tempdir().expect("tempdir");
    let btrfs = tempfile::tempdir().expect("tempdir");
    let (ok, out) = run_link(work.path(), btrfs.path());
    assert!(ok, "cargo-target-link.sh failed:\n{out}");
    assert_target_usable(work.path(), "fresh checkout");

    let link = std::fs::read_link(work.path().join("target")).expect("target should be a symlink");
    assert!(
        link.starts_with(btrfs.path().join("cargo-target")),
        "target/ points at {link:?}, not under the btrfs cargo-target root — build artifacts \
         would land on the small root filesystem"
    );
}

/// Two checkouts must not share one target dir: cargo's test-binary filename hash
/// omits the checkout path, so a shared dir lets one worktree run another's test
/// binary (observed 2026-08-08 — a run listed a test that existed only in a
/// different worktree, which makes red/green verification meaningless).
#[test]
fn cargo_target_link_separates_two_worktrees() {
    let btrfs = tempfile::tempdir().expect("tempdir");
    let a = tempfile::tempdir().expect("tempdir");
    let b = tempfile::tempdir().expect("tempdir");

    for d in [a.path(), b.path()] {
        let (ok, out) = run_link(d, btrfs.path());
        assert!(ok, "cargo-target-link.sh failed in {d:?}:\n{out}");
    }
    let la = std::fs::read_link(a.path().join("target")).unwrap();
    let lb = std::fs::read_link(b.path().join("target")).unwrap();
    assert_ne!(
        la, lb,
        "two checkouts resolved to the SAME cargo target dir; each worktree's test binaries \
         would overwrite the other's"
    );
}

/// THE REGRESSION. The btrfs root is wiped after the link exists — exactly what an
/// ephemeral runner volume does between jobs.
#[test]
fn cargo_target_link_heals_a_dangling_link_after_btrfs_is_wiped() {
    let work = tempfile::tempdir().expect("tempdir");
    let btrfs = tempfile::tempdir().expect("tempdir");

    let (ok, out) = run_link(work.path(), btrfs.path());
    assert!(ok, "first run failed:\n{out}");
    let link = std::fs::read_link(work.path().join("target")).expect("symlink");

    // The volume is reset; the symlink survives in the persistent checkout.
    std::fs::remove_dir_all(btrfs.path()).expect("wipe btrfs root");
    assert!(
        !link.exists(),
        "precondition: the link must now be dangling"
    );
    std::fs::create_dir_all(btrfs.path()).expect("volume comes back, empty");

    let (ok, out) = run_link(work.path(), btrfs.path());
    assert!(ok, "second run failed:\n{out}");
    assert_target_usable(work.path(), "after the btrfs root was wiped");
}

/// The harder variant: the volume does not come back at all, so the `-d $(BTRFS_ROOT)`
/// branch is skipped entirely and nothing touches the stale link. A build must still
/// work — falling back to a local `target/` beats failing every job.
#[test]
fn cargo_target_link_heals_when_btrfs_is_gone_entirely() {
    let work = tempfile::tempdir().expect("tempdir");
    let btrfs = tempfile::tempdir().expect("tempdir");

    let (ok, out) = run_link(work.path(), btrfs.path());
    assert!(ok, "first run failed:\n{out}");

    let gone = btrfs.path().to_path_buf();
    std::fs::remove_dir_all(&gone).expect("wipe btrfs root");
    assert!(
        !gone.exists(),
        "precondition: the btrfs root must be absent"
    );

    let (ok, out) = run_link(work.path(), &gone);
    assert!(
        ok,
        "cargo-target-link.sh failed when the btrfs root was absent; every build and test \
         recipe depends on it:\n{out}"
    );
    assert_target_usable(work.path(), "btrfs root absent");

    // And it must be a REAL local directory, not the stale link with its target
    // recreated. `$BTRFS_ROOT` absent means the volume is unmounted; recreating
    // the path underneath a mountpoint writes build artifacts to the small root
    // filesystem while still looking like btrfs — the exact failure this whole
    // indirection exists to avoid.
    assert!(
        std::fs::symlink_metadata(work.path().join("target"))
            .expect("target must exist")
            .file_type()
            .is_dir(),
        "target is still a symlink after the btrfs volume disappeared; artifacts would be \
         written under the unmounted mountpoint {gone:?} — i.e. onto the root filesystem"
    );
}

/// A different textual spelling can still resolve to the same directory inode. Opening that
/// candidate a second time and taking EX flock while already holding the old target EX lock
/// deadlocks the process against itself; fd identity must select the already-held lease.
#[test]
fn cargo_target_link_reuses_the_lease_for_a_same_inode_path_alias() {
    let work = tempfile::tempdir().expect("worktree");
    let btrfs = tempfile::tempdir().expect("btrfs root");
    let (ok, output) = run_link(work.path(), btrfs.path());
    assert!(ok, "initial target link failed:\n{output}");
    let target = work.path().join("target");
    let physical = std::fs::read_link(&target).expect("read initial target link");
    std::fs::remove_file(&target).expect("remove initial target link");
    let aliased = format!(
        "{}//{}",
        physical.parent().expect("target parent").display(),
        physical.file_name().expect("target name").to_string_lossy()
    );
    std::os::unix::fs::symlink(&aliased, &target).expect("link same inode through path alias");

    let output = Command::new("timeout")
        .args(["--kill-after=1s", "5s"])
        .arg(repo_root().join("scripts/cargo-target-link.sh"))
        .env("BTRFS_ROOT", btrfs.path())
        .current_dir(work.path())
        .output()
        .expect("run target link against same-inode alias");
    assert!(
        output.status.success(),
        "target link self-deadlocked or failed on a same-inode alias: {:?}\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::canonicalize(&target).expect("resolve normalized target"),
        physical,
        "same-inode alias was repointed to a different physical target"
    );
}

/// `make clean` must not recursively unlink target dentries outside the lease protocol. Logical
/// cleanup durably retires the current generation and atomically publishes an empty sibling;
/// the disk guard later reclaims the old payload without deleting its names.
#[test]
fn cargo_target_link_force_rotate_provides_a_fresh_logically_clean_target() {
    let work = tempfile::tempdir().expect("worktree");
    let btrfs = tempfile::tempdir().expect("btrfs root");
    let (ok, output) = run_link(work.path(), btrfs.path());
    assert!(ok, "initial target link failed:\n{output}");
    let old_generation =
        std::fs::read_link(work.path().join("target")).expect("read initial target generation");
    let old_payload = old_generation.join("build-output");
    std::fs::write(&old_payload, b"old bytes").expect("write old build output");

    let output = Command::new(repo_root().join("scripts/cargo-target-link.sh"))
        .arg("--rotate")
        .env("BTRFS_ROOT", btrfs.path())
        .current_dir(work.path())
        .output()
        .expect("force target generation rotation");
    assert!(
        output.status.success(),
        "logical clean failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh_generation =
        std::fs::read_link(work.path().join("target")).expect("read fresh target generation");
    assert_ne!(
        fresh_generation, old_generation,
        "clean reused old generation"
    );
    assert!(
        old_payload.exists() && !fresh_generation.join("build-output").exists(),
        "clean deleted a physical old dentry or exposed old cache names in the fresh generation"
    );
    assert_target_usable(work.path(), "after logical target clean");

    let local_work = tempfile::tempdir().expect("local-only worktree");
    let missing_btrfs = local_work.path().join("missing-btrfs");
    let local_target = local_work.path().join("target");
    std::fs::create_dir(&local_target).expect("create unrotatable local target");
    let local_payload = local_target.join("payload");
    std::fs::write(&local_payload, b"local bytes").expect("write local payload");
    let rejected = Command::new(repo_root().join("scripts/cargo-target-link.sh"))
        .arg("--rotate")
        .env("BTRFS_ROOT", &missing_btrfs)
        .current_dir(local_work.path())
        .output()
        .expect("reject local-only target rotation");
    assert!(
        !rejected.status.success()
            && String::from_utf8_lossy(&rejected.stderr).contains("refusing unsafe clean"),
        "local-only clean silently succeeded without rotating or reclaiming: {:?}\n{}{}",
        rejected.status,
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(
        std::fs::read(local_payload).expect("read local payload after rejected clean"),
        b"local bytes"
    );
}

/// A generation name becomes enumerable at mkdir, before link setup can lease it. Force the
/// systemd pruner to win that interval: link setup must observe the durable retirement xattr,
/// abandon that immutable sibling, and publish a different freshly leased generation.
#[test]
fn cargo_target_link_never_publishes_a_generation_retired_before_lease() {
    let work = tempfile::tempdir().expect("worktree");
    let btrfs = tempfile::tempdir().expect("btrfs root");
    let (ok, output) = run_link(work.path(), btrfs.path());
    assert!(ok, "initial target link failed:\n{output}");
    let old_generation =
        std::fs::read_link(work.path().join("target")).expect("read initial generation link");
    let old_payload = old_generation.join("old-payload");
    std::fs::write(&old_payload, b"old").expect("write old generation payload");
    age_regular_tree(&old_generation, 946_684_800);
    let managed_root = btrfs.path().join("cargo-target");
    let retired = run_direct_target_pruner(&managed_root, false);
    assert!(
        retired.status.success(),
        "retire initial generation failed:\n{}{}",
        String::from_utf8_lossy(&retired.stdout),
        String::from_utf8_lossy(&retired.stderr)
    );

    let signals = tempfile::tempdir().expect("generation race signals");
    let ready = signals.path().join("ready.fifo");
    let release = signals.path().join("release.fifo");
    assert!(Command::new("mkfifo")
        .args([ready.as_os_str(), release.as_os_str()])
        .status()
        .expect("create generation race FIFOs")
        .success());
    let ready_reader = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&ready)
        .expect("open generation ready FIFO");
    let mut release_writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&release)
        .expect("open generation release FIFO");

    let shim_dir = tempfile::tempdir().expect("mktemp shim directory");
    let shim = shim_dir.path().join("mktemp");
    let once = signals.path().join("intercepted");
    std::fs::write(
        &shim,
        "#!/bin/bash\n\
         if [[ $* == *generation-XXXXXXXX* && ! -e $ONCE ]]; then\n\
           generation=$(\"$REAL_MKTEMP\" \"$@\") || exit $?\n\
           : >\"$ONCE\"\n\
           printf G >\"$READY\"\n\
           IFS= read -r _ <\"$RELEASE\"\n\
           printf '%s\\n' \"$generation\"\n\
         else\n\
           exec \"$REAL_MKTEMP\" \"$@\"\n\
         fi\n",
    )
    .expect("write mktemp shim");
    let mut permissions = std::fs::metadata(&shim)
        .expect("stat mktemp shim")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&shim, permissions).expect("make mktemp shim executable");
    let original_path = std::env::var_os("PATH").expect("PATH");
    let real_mktemp = std::env::split_paths(&original_path)
        .map(|directory| directory.join("mktemp"))
        .find(|candidate| candidate.is_file())
        .expect("find real mktemp");
    let shim_path = std::env::join_paths(
        std::iter::once(shim_dir.path().to_path_buf()).chain(std::env::split_paths(&original_path)),
    )
    .expect("build mktemp shim PATH");

    let link = ChildGuard(Some(
        Command::new(repo_root().join("scripts/cargo-target-link.sh"))
            .env("BTRFS_ROOT", btrfs.path())
            .env("PATH", shim_path)
            .env("REAL_MKTEMP", real_mktemp)
            .env("READY", &ready)
            .env("RELEASE", &release)
            .env("ONCE", &once)
            .current_dir(work.path())
            .spawn()
            .expect("spawn target link with generation race"),
    ));
    assert_eq!(
        read_marker_with_timeout(ready_reader, "fresh generation mkdir"),
        b'G'
    );
    let raced_generation = std::fs::read_dir(&managed_root)
        .expect("enumerate managed generations during race")
        .map(|entry| entry.expect("read managed generation").path())
        .find(|path| path != &old_generation && path.is_dir())
        .expect("find unleased fresh generation");

    let won_race = run_direct_target_pruner(&managed_root, false);
    assert!(
        won_race.status.success(),
        "pruner did not retire the unleased generation:\n{}{}",
        String::from_utf8_lossy(&won_race.stdout),
        String::from_utf8_lossy(&won_race.stderr)
    );
    release_writer
        .write_all(b"continue\n")
        .expect("release generation creator");
    let status = link
        .wait()
        .expect("wait for target link after generation race");
    assert!(
        status.success(),
        "target link failed after lost lease race: {status:?}"
    );
    let published =
        std::fs::read_link(work.path().join("target")).expect("read published generation");
    assert_ne!(
        published, raced_generation,
        "link published a generation the pruner had already retired"
    );
    assert_eq!(
        std::fs::read(published.join(".fcvm-generation")).expect("read fresh generation sentinel"),
        b"v1\n"
    );
}

/// `target` occupied by a regular file cannot be silently ignored: cargo would
/// fail later with the same opaque `Not a directory (os error 20)` and no hint of
/// why. Fail here, loudly, where the message can name the cause.
///
/// Both branches matter, and only the second pins the explicit guard. With the
/// btrfs root PRESENT the script dies earlier anyway, when `ln -s` hits the
/// existing file under `set -e` — so that case alone passes even with the guard
/// deleted (verified by mutation). With the root ABSENT nothing else touches
/// `target`, and the guard is the only thing standing between a silent exit 0 and
/// a build that fails much later somewhere else.
#[test]
fn cargo_target_link_fails_loudly_on_a_non_directory_target() {
    for btrfs_present in [true, false] {
        let work = tempfile::tempdir().expect("tempdir");
        let btrfs = tempfile::tempdir().expect("tempdir");
        let root = btrfs.path().to_path_buf();
        if !btrfs_present {
            std::fs::remove_dir_all(&root).expect("wipe btrfs root");
        }
        std::fs::write(work.path().join("target"), b"not a directory").expect("write file");

        let (ok, out) = run_link(work.path(), &root);
        assert!(
            !ok,
            "btrfs_present={btrfs_present}: reported success while `target` is a regular file. \
             The build then dies inside cargo with `Not a directory (os error 20)` and nothing \
             points at the cause. Output:\n{out}"
        );
        assert!(
            out.contains("target"),
            "btrfs_present={btrfs_present}: the failure must name `target` so it is \
             actionable. Output:\n{out}"
        );
    }
}

/// The Makefile must actually call the script. Every build and test recipe
/// depends on `cargo-target-link`, and the tests above drive the script directly
/// — so if the recipe stopped invoking it (or grew a second, divergent copy of
/// the logic inline) nothing else here would notice.
///
/// This reads the recipe's COMMAND lines only, not the whole file: a match
/// anywhere in the Makefile would also be satisfied by a comment mentioning the
/// script, which is the failure mode that made an earlier version of the AMI-hash
/// test pass with its subject deleted.
#[test]
fn makefile_delegates_to_the_script() {
    let mk = std::fs::read_to_string(repo_root().join("Makefile")).expect("read Makefile");
    let mut lines = mk
        .lines()
        .skip_while(|l| !l.starts_with("cargo-target-link:"));
    assert!(
        lines.next().is_some(),
        "Makefile has no `cargo-target-link:` target; every build recipe depends on it"
    );
    let recipe: Vec<&str> = lines
        .take_while(|l| l.starts_with('\t'))
        .map(|l| l.trim_start_matches('\t'))
        .filter(|l| !l.trim_start_matches('@').starts_with('#'))
        .collect();
    assert!(
        !recipe.is_empty(),
        "`cargo-target-link` has no commands, so nothing sets up target/"
    );
    assert!(
        recipe
            .iter()
            .any(|l| l.contains("scripts/cargo-target-link.sh")),
        "the cargo-target-link recipe does not invoke scripts/cargo-target-link.sh, so the \
         behaviour every other test in this file verifies is not what `make` runs. Commands \
         found: {recipe:?}"
    );
}

/// The disk preflight used to glob `cargo-target*`, which selected the parent
/// `cargo-target/` directory instead of its per-worktree children.  One idle
/// sibling could therefore never be reclaimed independently, and deleting the
/// parent left every checkout's target symlink dangling.
///
/// This also exercises the lease rather than merely inspecting shell text: an
/// old-looking target stays intact while a fake Cargo process holds the shared
/// directory lease, while its idle sibling payload is reclaimed in the same preflight.
#[test]
fn target_pruner_separates_idle_and_concurrently_active_worktrees() {
    let btrfs = tempfile::tempdir().expect("btrfs root");
    let runner_work = tempfile::tempdir().expect("runner work root");
    let active_work = tempfile::tempdir().expect("active worktree");
    let idle_work = tempfile::tempdir().expect("idle worktree");

    for work in [active_work.path(), idle_work.path()] {
        let (ok, out) = run_link(work, btrfs.path());
        assert!(ok, "cargo-target-link.sh failed in {work:?}:\n{out}");
    }
    let active_target = std::fs::read_link(active_work.path().join("target")).unwrap();
    let idle_target = std::fs::read_link(idle_work.path().join("target")).unwrap();
    let active_payload = active_target.join("old-active-artifact");
    let idle_payload = idle_target.join("old-idle-artifact");
    std::fs::write(&active_payload, b"active").unwrap();
    std::fs::write(&idle_payload, b"idle").unwrap();
    age_file(&active_payload);
    age_file(&idle_payload);

    let signals = tempfile::tempdir().expect("signals");
    let ready = signals.path().join("ready");
    let release = signals.path().join("release");
    let build = ChildGuard(Some(
        Command::new(repo_root().join("scripts/cargo-target-run.sh"))
            .args([
                "/bin/bash",
                "-c",
                "printf ready >\"$READY\"; while [[ ! -e $RELEASE ]]; do sleep 0.01; done; printf built >target/concurrent-build-output",
            ])
            .env("CARGO_TARGET_DIR", "target")
            .env("READY", &ready)
            .env("RELEASE", &release)
            .current_dir(active_work.path())
            .spawn()
            .expect("spawn leased fake Cargo process"),
    ));
    wait_for_path(&ready);

    let (ok, out) = run_target_pruner(btrfs.path(), runner_work.path());
    assert!(ok, "target-dir preflight failed:\n{out}");
    assert!(
        out.contains("concurrent cargo holds target lease"),
        "preflight did not report the held shared lease:\n{out}"
    );
    assert!(
        active_payload.exists(),
        "pruner deleted artifacts while the Cargo shared lease was held"
    );
    assert!(
        idle_payload.metadata().ok().map(|metadata| metadata.len()) == Some(0),
        "idle sibling payload was not reclaimed independently:\n{out}"
    );
    assert!(
        active_target.is_dir() && idle_target.is_dir(),
        "pruner removed a leased directory inode and dangled a worktree symlink"
    );
    let status = Command::new(repo_root().join("scripts/cargo-target-run.sh"))
        .args(["/bin/sh", "-c", "printf rebuilt >target/after-prune"])
        .env("BTRFS_ROOT", btrfs.path())
        .env("CARGO_TARGET_DIR", "target")
        .current_dir(idle_work.path())
        .status()
        .expect("run Cargo stand-in after retiring idle generation");
    assert!(
        status.success(),
        "Cargo wrapper did not rotate retired target"
    );
    let fresh_idle_target =
        std::fs::read_link(idle_work.path().join("target")).expect("read fresh idle target link");
    assert_ne!(
        fresh_idle_target, idle_target,
        "Cargo wrapper reused the retired physical generation"
    );
    assert_eq!(
        std::fs::read(fresh_idle_target.join("after-prune")).expect("read fresh target output"),
        b"rebuilt"
    );

    std::fs::write(&release, b"release").unwrap();
    let status = build.wait().expect("wait for fake Cargo process");
    assert!(status.success(), "fake Cargo process failed: {status}");
    assert_eq!(
        std::fs::read(active_target.join("concurrent-build-output")).unwrap(),
        b"built",
        "concurrent build did not complete after retaining its target"
    );
}

/// A real checkout-local `target/` cannot be atomically replaced by a fresh generation without
/// renaming its dentry, which can move a bind mount visible only in another mount namespace.
/// Such pre-protocol targets are retained; the final disk hard floor quarantines the runner if
/// managed btrfs generations do not recover enough space.
#[test]
fn target_pruner_retains_an_unrotatable_local_target() {
    let runner_root = tempfile::tempdir().expect("runner root");
    let target = runner_root.path().join("repo/checkout/target");
    std::fs::create_dir_all(&target).expect("create local runner target");
    let payload = target.join("old-payload");
    std::fs::write(&payload, b"must remain").expect("write local target payload");
    age_file(&payload);

    let output = Command::new(repo_root().join("scripts/prune-cargo-target.sh"))
        .args([
            std::ffi::OsStr::new("1 day ago"),
            std::ffi::OsStr::new("0"),
            runner_root.path().as_os_str(),
            std::ffi::OsStr::new(""),
        ])
        .output()
        .expect("run helper against unmanaged local target");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success() && text.contains("local target has no rotatable generation"),
        "local target was not explicitly retained:\n{text}"
    );
    assert_eq!(
        std::fs::read(payload).expect("read retained local payload"),
        b"must remain"
    );
}

/// Both target roots must be completely enumerated before cleanup begins.
/// A symlinked root is rejected by component-wise O_NOFOLLOW regardless of uid;
/// surviving artifacts prove the helper retains candidates but performs no
/// deletion until every supplied root has been traversed successfully.
#[test]
fn target_pruner_fails_closed_when_either_target_root_cannot_be_enumerated() {
    for fail_runner_root in [true, false] {
        let namespace = tempfile::tempdir().expect("root namespace");
        let runner_real = tempfile::tempdir().expect("real runner root");
        let btrfs_real = tempfile::tempdir().expect("real btrfs target root");
        let runner_link = namespace.path().join("runner-link");
        let btrfs_link = namespace.path().join("btrfs-link");
        std::os::unix::fs::symlink(runner_real.path(), &runner_link)
            .expect("create runner root symlink");
        std::os::unix::fs::symlink(btrfs_real.path(), &btrfs_link)
            .expect("create btrfs root symlink");

        let runner_target = runner_real.path().join("project/project/target");
        let btrfs_target = btrfs_real.path().join("worktree");
        std::fs::create_dir_all(&runner_target).expect("create runner target");
        std::fs::create_dir_all(&btrfs_target).expect("create btrfs target");

        let runner_payload = runner_target.join("old-runner-artifact");
        let btrfs_payload = btrfs_target.join("old-btrfs-artifact");
        std::fs::write(&runner_payload, b"runner").expect("write runner artifact");
        std::fs::write(&btrfs_payload, b"btrfs").expect("write btrfs artifact");
        age_file(&runner_payload);
        age_file(&btrfs_payload);

        let (runner_arg, btrfs_arg, failed_root) = if fail_runner_root {
            (
                runner_link.as_path(),
                btrfs_real.path(),
                runner_link.as_path(),
            )
        } else {
            (
                runner_real.path(),
                btrfs_link.as_path(),
                btrfs_link.as_path(),
            )
        };

        let out = Command::new(repo_root().join("scripts/prune-cargo-target.sh"))
            .args([
                std::ffi::OsStr::new("1 day ago"),
                std::ffi::OsStr::new("0"),
                runner_arg.as_os_str(),
                btrfs_arg.as_os_str(),
            ])
            .output()
            .expect("run target helper with an untrusted symlink root");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        assert!(
            !out.status.success(),
            "reported success after target enumeration failed for {failed_root:?}:\n{text}"
        );
        assert!(
            out.status.code() == Some(51)
                && text.contains("cannot enumerate every cargo target root"),
            "failure did not identify the no-follow root rejection {failed_root:?}:\n{text}"
        );
        assert!(
            runner_payload.exists() && btrfs_payload.exists(),
            "pruner deleted from a partial target view after {failed_root:?} could not be \
             enumerated: runner_exists={} btrfs_exists={}\n{text}",
            runner_payload.exists(),
            btrfs_payload.exists()
        );
    }
}
#[test]
fn target_pruner_owns_discovery_and_cleanup_at_one_privilege() {
    let script = std::fs::read_to_string(repo_root().join("scripts/runner-disk-preflight.sh"))
        .expect("read runner-disk-preflight.sh");
    let helper = std::fs::read_to_string(repo_root().join("scripts/prune-cargo-target.sh"))
        .expect("read privileged target-pruning helper");

    assert!(
        script.contains("\"${SUDO[@]}\" env")
            && script.contains("\"$TARGET_PRUNE_HELPER\"")
            && script.contains(
                "\"$TARGET_CUTOFF\" \"$DRY_RUN\" \"$runner_root\" \"$btrfs_target_root\""
            ),
        "target roots are not handed to one helper through the cleanup privilege boundary"
    );
    assert!(
        !script.contains("-printf '%p\\0%D:%i\\0'")
            && !script.contains("expected_identity")
            && !helper.contains("expected_identity"),
        "the cleanup still relies on a reusable cross-process dev:ino identity token"
    );

    let open = helper
        .find("return open_beneath(parent_fd, name, OPEN_FLAGS)")
        .expect("helper does not open candidates relative to the retained parent fd");
    let lock = helper
        .find("fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)")
        .expect("helper does not acquire the exclusive target lease");
    let reclaim = helper
        .find("def reclaim_target(fd, census):")
        .expect("helper reclaim is not rooted at the locked fd with a mount boundary");
    assert!(
        open < lock && lock < reclaim,
        "candidate open, exclusive lock, and fd-rooted reclaim are not one helper critical section"
    );
    assert!(
        helper.contains("os.O_DIRECTORY | os.O_NOFOLLOW")
            && helper.contains("open_absolute_directory(path)")
            && helper.contains("RESOLVE_NO_XDEV")
            && helper.contains("RESOLVE_NO_SYMLINKS")
            && helper.contains("validate_fingerprint_payloads(fd, census)")
            && helper.contains("os.ftruncate(writer, 0)")
            && helper.contains("os.fsync(writer)")
            && !helper.contains("os.rmdir(")
            && !helper.contains("os.unlink(")
            && !helper.contains("os.rename(")
            && !helper.contains("rm -rf"),
        "helper does not no-follow components, durably invalidate fingerprints first, or retain every dentry"
    );
}

fn spawn_synchronized_helper(
    btrfs_target_root: &Path,
) -> (
    ChildGuard,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let signals = tempfile::tempdir().expect("sync socket directory");
    let socket_path = signals.path().join("enumerated.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind sync socket");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    let sync = std::thread::spawn(move || {
        let _signals = signals;
        let (mut channel, _) = listener.accept().expect("accept helper sync");
        let mut marker = [0_u8; 1];
        channel
            .read_exact(&mut marker)
            .expect("read enumeration marker");
        assert_eq!(marker, [b'E']);
        ready_tx.send(()).expect("signal enumeration ready");
        continue_rx.recv().expect("wait for test mutation");
        channel.write_all(b"C").expect("release helper");
    });

    let helper = ChildGuard(Some(
        Command::new(repo_root().join("scripts/prune-cargo-target.sh"))
            .args([
                std::ffi::OsStr::new("1 day ago"),
                std::ffi::OsStr::new("0"),
                std::ffi::OsStr::new(""),
                btrfs_target_root.as_os_str(),
            ])
            .env("FCVM_PRUNE_TEST_SYNC_SOCKET", &socket_path)
            .spawn()
            .expect("spawn synchronized target helper"),
    ));
    (helper, ready_rx, continue_tx, sync)
}

/// A fresh real directory can replace a candidate after discovery. The retained locked fd is the
/// object authorized for cleanup: its old payload is removed even after rename, while the new
/// pathname occupant is never opened or touched. Reopening the path would invert both results.
#[test]
fn target_pruner_cleans_only_the_locked_object_after_directory_replacement() {
    let root = tempfile::tempdir().expect("target root");
    let target = root.path().join("candidate");
    let original = root.path().join("original-candidate");
    std::fs::create_dir(&target).expect("create candidate");
    let original_sentinel = target.join("original-must-survive");
    std::fs::write(&original_sentinel, b"original data").expect("write original sentinel");
    age_file(&original_sentinel);

    let (helper, ready, release, sync) = spawn_synchronized_helper(root.path());
    ready
        .recv_timeout(Duration::from_secs(5))
        .expect("helper did not retain the discovered target fd");

    std::fs::rename(&target, &original).expect("rename candidate after discovery");
    std::fs::create_dir(&target).expect("install fresh replacement directory");
    let replacement_sentinel = target.join("replacement-must-survive");
    std::fs::write(&replacement_sentinel, b"replacement data").expect("write replacement sentinel");
    age_file(&replacement_sentinel);
    release.send(()).expect("release helper after replacement");
    let status = helper.wait().expect("wait for synchronized helper");
    sync.join().expect("join synchronization peer");

    assert!(
        status.success(),
        "replacement-safe helper failed: {status:?}"
    );
    assert!(
        original
            .join("original-must-survive")
            .metadata()
            .ok()
            .map(|metadata| metadata.len())
            == Some(0)
            && replacement_sentinel.exists(),
        "helper did not reclaim only the retained locked object: original_size={:?} replacement_exists={}",
        original
            .join("original-must-survive")
            .metadata()
            .map(|metadata| metadata.len()),
        replacement_sentinel.exists()
    );
}

/// A FIFO in the candidate namespace is not a directory and must be skipped without ever
/// blocking in open(2). The outer timeout is an assertion boundary: rc=124 would be a failure,
/// not a retry or an accepted outcome.
#[test]
fn target_pruner_does_not_block_on_a_candidate_fifo() {
    let root = tempfile::tempdir().expect("target root");
    let target = root.path().join("candidate");
    let mkfifo = Command::new("mkfifo")
        .arg(&target)
        .status()
        .expect("run mkfifo");
    assert!(mkfifo.success(), "mkfifo failed: {mkfifo:?}");

    let helper = repo_root().join("scripts/prune-cargo-target.sh");
    let status = Command::new("/usr/bin/timeout")
        .args([
            std::ffi::OsStr::new("--kill-after=1s"),
            std::ffi::OsStr::new("5s"),
            helper.as_os_str(),
            std::ffi::OsStr::new("1 day ago"),
            std::ffi::OsStr::new("0"),
            std::ffi::OsStr::new(""),
            root.path().as_os_str(),
        ])
        .status()
        .expect("run target helper against replacement FIFO");
    assert_eq!(
        status.code(),
        Some(0),
        "FIFO discovery blocked or failed instead of skipping a non-directory: {status:?}"
    );
}

/// A final-component symlink must be skipped rather than followed into an unrelated directory.
/// The timeout is an assertion boundary, not a retry: timeout status is never accepted.
#[test]
fn target_pruner_never_follows_a_candidate_symlink() {
    let root = tempfile::tempdir().expect("target root");
    let victim = tempfile::tempdir().expect("symlink victim");
    let sentinel = victim.path().join("must-survive");
    std::fs::write(&sentinel, b"unrelated data").expect("write victim sentinel");
    age_file(&sentinel);
    std::os::unix::fs::symlink(victim.path(), root.path().join("candidate"))
        .expect("create candidate symlink");

    let helper = repo_root().join("scripts/prune-cargo-target.sh");
    let status = Command::new("/usr/bin/timeout")
        .args([
            std::ffi::OsStr::new("--kill-after=1s"),
            std::ffi::OsStr::new("5s"),
            helper.as_os_str(),
            std::ffi::OsStr::new("1 day ago"),
            std::ffi::OsStr::new("0"),
            std::ffi::OsStr::new(""),
            root.path().as_os_str(),
        ])
        .status()
        .expect("run target helper against candidate symlink");
    assert_eq!(
        status.code(),
        Some(0),
        "candidate symlink was followed, blocked, or failed instead of being skipped: {status:?}"
    );
    assert!(
        sentinel.exists(),
        "helper followed a candidate symlink and deleted unrelated data"
    );
}

/// O_NOFOLLOW on only the final component still lets an ancestor symlink redirect root open.
/// Component-by-component opening must reject the root before touching the real target.
#[test]
fn target_pruner_rejects_a_symlinked_ancestor() {
    let parent = tempfile::tempdir().expect("root parent");
    let real_parent = parent.path().join("real-parent");
    let linked_parent = parent.path().join("linked-parent");
    let real_root = real_parent.join("target-root");
    let linked_root = linked_parent.join("target-root");
    let target = real_root.join("candidate");
    std::fs::create_dir_all(&target).expect("create real candidate");
    let sentinel = target.join("must-survive");
    std::fs::write(&sentinel, b"data").expect("write sentinel");
    age_file(&sentinel);
    std::os::unix::fs::symlink(&real_parent, &linked_parent).expect("create ancestor symlink");

    let status = Command::new(repo_root().join("scripts/prune-cargo-target.sh"))
        .args([
            std::ffi::OsStr::new("1 day ago"),
            std::ffi::OsStr::new("0"),
            std::ffi::OsStr::new(""),
            linked_root.as_os_str(),
        ])
        .status()
        .expect("run helper through symlinked root");
    assert_eq!(
        status.code(),
        Some(51),
        "symlinked root was not rejected: {status:?}"
    );
    assert!(
        sentinel.exists(),
        "helper followed an ancestor symlink into the real target"
    );
}

#[cfg(feature = "privileged-tests")]
struct BindMountGuard(PathBuf);

#[cfg(feature = "privileged-tests")]
impl BindMountGuard {
    fn unmount(mut self) {
        let status = Command::new("umount").arg(&self.0).status();
        assert!(
            matches!(status, Ok(status) if status.success()),
            "failed to unmount test bind mount {:?}: {status:?}",
            self.0
        );
        self.0.clear();
    }
}

#[cfg(feature = "privileged-tests")]
impl Drop for BindMountGuard {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = Command::new("umount").arg(&self.0).status();
        }
    }
}

/// `rm -rf` and `find -xdev` both cross a bind mount whose source has the same device number as
/// the target. An aborted test can leave exactly such a mount under a Cargo target. Privileged
/// cleanup must detect the mount ID boundary and fail before touching the mounted filesystem.
#[cfg(feature = "privileged-tests")]
#[test]
fn target_pruner_never_deletes_through_a_descendant_bind_mount() {
    let root = tempfile::tempdir().expect("target root");
    let source = tempfile::tempdir().expect("bind source");
    let target = root.path().join("candidate");
    let mountpoint = target.join("mounted-source");
    std::fs::create_dir_all(&mountpoint).expect("create target and mountpoint");

    let sentinel = source.path().join("must-survive");
    std::fs::write(&sentinel, b"mounted data").expect("write mounted sentinel");
    age_file(&sentinel);
    let status = Command::new("mount")
        .args([
            std::ffi::OsStr::new("--bind"),
            source.path().as_os_str(),
            mountpoint.as_os_str(),
        ])
        .status()
        .expect("run bind mount");
    assert!(status.success(), "bind mount failed: {status:?}");
    let mount = BindMountGuard(mountpoint);

    let status = Command::new(repo_root().join("scripts/prune-cargo-target.sh"))
        .args([
            std::ffi::OsStr::new("1 day ago"),
            std::ffi::OsStr::new("0"),
            std::ffi::OsStr::new(""),
            root.path().as_os_str(),
        ])
        .status()
        .expect("run target helper with descendant bind mount");

    assert!(
        sentinel.exists(),
        "target cleanup crossed the bind mount and deleted unrelated mounted data"
    );
    assert_eq!(
        status.code(),
        Some(50),
        "helper did not reject a target containing a descendant mount: {status:?}"
    );
    mount.unmount();
}

/// A mount at the target itself is not a descendant, so a prefix-only mountinfo check misses it.
/// The helper must reject equality too, before touching the mounted source.
#[cfg(feature = "privileged-tests")]
#[test]
fn target_pruner_rejects_an_exact_target_bind_mount() {
    let root = tempfile::tempdir().expect("target root");
    let source = tempfile::tempdir().expect("bind source");
    let target = root.path().join("candidate");
    std::fs::create_dir(&target).expect("create target mountpoint");
    let sentinel = source.path().join("must-survive");
    std::fs::write(&sentinel, b"mounted data").expect("write mounted sentinel");
    age_file(&sentinel);

    let status = Command::new("mount")
        .args([
            std::ffi::OsStr::new("--bind"),
            source.path().as_os_str(),
            target.as_os_str(),
        ])
        .status()
        .expect("run exact-target bind mount");
    assert!(status.success(), "bind mount failed: {status:?}");
    let mount = BindMountGuard(target);

    let status = Command::new(repo_root().join("scripts/prune-cargo-target.sh"))
        .args([
            std::ffi::OsStr::new("1 day ago"),
            std::ffi::OsStr::new("0"),
            std::ffi::OsStr::new(""),
            root.path().as_os_str(),
        ])
        .status()
        .expect("run helper with exact target bind mount");
    assert_eq!(
        status.code(),
        Some(50),
        "helper did not reject an exact-target mount: {status:?}"
    );
    assert!(
        sentinel.exists(),
        "helper deleted through an exact-target bind mount"
    );
    mount.unmount();
}

/// Mount safety is tied to the retained target fd, not its mutable cleanup name. Renaming a
/// mounted candidate away and installing a fresh directory at the old path must still reject
/// the mount in the locked original; a pathname-only validator silently skips this case.
#[cfg(feature = "privileged-tests")]
#[test]
fn target_pruner_rejects_a_mounted_candidate_renamed_after_discovery() {
    let root = tempfile::tempdir().expect("target root");
    let source = tempfile::tempdir().expect("bind source");
    let target = root.path().join("candidate");
    let renamed = root.path().join("renamed-candidate");
    let mountpoint = target.join("mounted-source");
    std::fs::create_dir_all(&mountpoint).expect("create candidate mountpoint");
    let sentinel = source.path().join("must-survive");
    std::fs::write(&sentinel, b"mounted data").expect("write mounted sentinel");
    age_file(&sentinel);
    let status = Command::new("mount")
        .args([
            std::ffi::OsStr::new("--bind"),
            source.path().as_os_str(),
            mountpoint.as_os_str(),
        ])
        .status()
        .expect("run descendant bind mount");
    assert!(status.success(), "bind mount failed: {status:?}");
    let mut mount = BindMountGuard(mountpoint);

    let (helper, ready, release, sync) = spawn_synchronized_helper(root.path());
    ready
        .recv_timeout(Duration::from_secs(5))
        .expect("helper did not retain mounted candidate fd");
    std::fs::rename(&target, &renamed).expect("rename mounted candidate after discovery");
    mount.0 = renamed.join("mounted-source");
    std::fs::create_dir(&target).expect("install fresh path occupant");

    release
        .send(())
        .expect("release helper after candidate rename");
    let status = helper.wait().expect("wait for mounted target helper");
    sync.join().expect("join synchronization peer");
    assert_eq!(
        status.code(),
        Some(50),
        "helper did not reject the mount through its retained target fd: {status:?}"
    );
    assert!(
        sentinel.exists(),
        "helper crossed a mount after the candidate pathname changed"
    );
    mount.unmount();
}

/// A host bind mount inserted after discovery must remain visible to the deleting helper. Hiding
/// it in a private namespace is unsafe: VFS unlink/rmdir can detach a mount that exists in another
/// namespace after the local mountpoint check passes. The dynamic openat2 traversal must reject
/// the late descendant before any target payload is reclaimed, and the host mount must remain
/// attached.
#[cfg(feature = "privileged-tests")]
#[test]
fn target_pruner_rejects_a_host_bind_mount_added_after_discovery() {
    let root = tempfile::tempdir().expect("target root");
    let source = tempfile::tempdir().expect("bind source");
    let target = root.path().join("candidate");
    let late_mountpoint = target.join("late-mount");
    std::fs::create_dir_all(&late_mountpoint).expect("create candidate mountpoint");
    let old_payload = target.join("old-payload");
    std::fs::write(&old_payload, b"old").expect("write old payload");
    age_file(&old_payload);
    let mounted_sentinel = source.path().join("must-survive");
    std::fs::write(&mounted_sentinel, b"unrelated mounted data").expect("write mounted sentinel");

    let (helper, ready, release, sync) = spawn_synchronized_helper(root.path());
    ready
        .recv_timeout(Duration::from_secs(5))
        .expect("helper did not reach post-enumeration synchronization");

    let status = Command::new("mount")
        .args([
            std::ffi::OsStr::new("--bind"),
            source.path().as_os_str(),
            late_mountpoint.as_os_str(),
        ])
        .status()
        .expect("insert host bind mount after helper discovery");
    assert!(status.success(), "bind mount failed: {status:?}");
    let mount = BindMountGuard(late_mountpoint.clone());

    release.send(()).expect("release helper after host mount");
    let status = helper.wait().expect("wait for mount-aware helper");
    sync.join().expect("join synchronization peer");
    assert_eq!(
        status.code(),
        Some(50),
        "helper did not reject the bind mount added after discovery: {status:?}"
    );
    assert!(
        mounted_sentinel.exists(),
        "helper traversed the host bind mount and deleted unrelated data"
    );

    mount.unmount();
    assert!(
        old_payload.exists() && target.is_dir(),
        "helper partially reclaimed a target after detecting the late mount"
    );
}

/// A mount can exist only in another mount namespace. `openat2(RESOLVE_NO_XDEV)` in the
/// helper's namespace cannot see it, but unlink/rmdir of the underlying dentry calls the VFS
/// global `detach_mounts()` path and silently tears the foreign mount down. Cleanup must reclaim
/// the stale target without unlinking any dentry that another namespace can have mounted.
#[cfg(feature = "privileged-tests")]
#[test]
fn target_pruner_never_detaches_a_foreign_namespace_mount() {
    let root = tempfile::tempdir().expect("target root");
    let source = tempfile::tempdir().expect("bind source");
    let target = root.path().join("candidate");
    let mountpoint = target.join("foreign-mount");
    std::fs::create_dir_all(&mountpoint).expect("create foreign mountpoint");
    let old_payload = mountpoint.join("old-underlying-payload");
    std::fs::write(&old_payload, b"old").expect("write underlying target payload");
    age_file(&old_payload);
    let mounted_sentinel = source.path().join("must-survive");
    std::fs::write(&mounted_sentinel, b"foreign mounted data")
        .expect("write foreign mounted sentinel");

    let mut holder = ChildGuard(Some(
        Command::new("unshare")
            .args([
                std::ffi::OsStr::new("--mount"),
                std::ffi::OsStr::new("--propagation"),
                std::ffi::OsStr::new("private"),
                std::ffi::OsStr::new("/bin/bash"),
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new(
                    "set -euo pipefail\n\
                     mount --bind -- \"$SOURCE\" \"$MOUNTPOINT\"\n\
                     printf M\n\
                     IFS= read -r _\n\
                     mountpoint -q -- \"$MOUNTPOINT\" || exit 90\n\
                     umount -- \"$MOUNTPOINT\"",
                ),
            ])
            .env("SOURCE", source.path())
            .env("MOUNTPOINT", &mountpoint)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn foreign mount-namespace holder"),
    ));
    let holder_stdout = holder
        .child_mut()
        .stdout
        .take()
        .expect("foreign holder stdout");
    assert_eq!(
        read_marker_with_timeout(holder_stdout, "foreign bind mount"),
        b'M'
    );

    let status = Command::new(repo_root().join("scripts/prune-cargo-target.sh"))
        .args([
            std::ffi::OsStr::new("1 day ago"),
            std::ffi::OsStr::new("0"),
            std::ffi::OsStr::new(""),
            root.path().as_os_str(),
        ])
        .status()
        .expect("run helper while a foreign namespace holds a descendant mount");
    assert!(
        status.success(),
        "foreign-mount-safe cleanup failed: {status:?}"
    );

    holder
        .child_mut()
        .stdin
        .take()
        .expect("foreign holder stdin")
        .write_all(b"release\n")
        .expect("release foreign mount holder");
    let status = holder.wait().expect("wait for foreign mount holder");
    assert!(
        status.success(),
        "cleanup detached a bind mount that existed only in another namespace: {status:?}"
    );
    assert!(
        mounted_sentinel.exists(),
        "cleanup modified data behind a foreign namespace mount"
    );
    assert!(
        old_payload.metadata().ok().map(|metadata| metadata.len()) == Some(0),
        "cleanup did not reclaim the stale underlying target payload"
    );
}

fn run_cargo_fixture_build(project: &Path, btrfs_root: &Path) -> std::process::Output {
    if !project.join("target").exists() {
        let link = Command::new(repo_root().join("scripts/cargo-target-link.sh"))
            .env("BTRFS_ROOT", btrfs_root)
            .current_dir(project)
            .output()
            .expect("prepare initial managed Cargo cache generation");
        assert!(
            link.status.success(),
            "prepare initial managed Cargo cache generation failed:\n{}{}",
            String::from_utf8_lossy(&link.stdout),
            String::from_utf8_lossy(&link.stderr)
        );
    }
    Command::new(repo_root().join("scripts/cargo-target-run.sh"))
        .args(["cargo", "build", "--offline", "--verbose"])
        .env("BTRFS_ROOT", btrfs_root)
        .env("CARGO_TARGET_DIR", "target")
        .current_dir(project)
        .output()
        .expect("build Cargo cache fixture through target lease")
}

fn run_direct_target_pruner(
    target_root: &Path,
    fail_after_fingerprints: bool,
) -> std::process::Output {
    let mut command = Command::new(repo_root().join("scripts/prune-cargo-target.sh"));
    command.args([
        std::ffi::OsStr::new("1 day ago"),
        std::ffi::OsStr::new("0"),
        std::ffi::OsStr::new(""),
        target_root.as_os_str(),
    ]);
    if fail_after_fingerprints {
        command.env("FCVM_PRUNE_TEST_FAIL_AFTER_FINGERPRINTS", "1");
    } else {
        command.env_remove("FCVM_PRUNE_TEST_FAIL_AFTER_FINGERPRINTS");
    }
    command.output().expect("run target pruner directly")
}

fn find_fixture_rlib(target: &Path) -> PathBuf {
    std::fs::read_dir(target.join("debug/deps"))
        .expect("read Cargo fixture deps")
        .map(|entry| entry.expect("read Cargo fixture dep entry").path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libcache_helper-") && name.ends_with(".rlib"))
        })
        .expect("find path-dependency rlib")
}

fn find_fixture_generated_source(target: &Path) -> PathBuf {
    std::fs::read_dir(target.join("debug/build"))
        .expect("read Cargo fixture build directories")
        .map(|entry| entry.expect("read Cargo fixture build entry").path())
        .map(|path| path.join("out/generated.rs"))
        .find(|path| path.exists())
        .expect("find build-script generated source")
}

/// Fingerprints are the commit record for Cargo's artifact cache. They must all become durably
/// invalid before any output payload is reclaimed, so process failure or power loss cannot leave
/// a valid fingerprint beside a zero-length binary. Internal Cargo hardlinks are counted once and
/// reclaimed; the next build must compile despite source mtimes being older than every target
/// entry, proving this is fingerprint invalidation rather than ordinary mtime invalidation.
#[test]
fn target_pruner_invalidates_cargo_before_reclaiming_hardlinked_payloads() {
    let project = tempfile::tempdir().expect("Cargo fixture project");
    let target_root = tempfile::tempdir().expect("Cargo fixture target root");
    let managed_root = target_root.path().join("cargo-target");
    std::fs::create_dir_all(project.path().join("src")).expect("create app source directory");
    std::fs::create_dir_all(project.path().join("helper/src"))
        .expect("create helper source directory");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"cache-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\
         [dependencies]\ncache-helper = { path = \"helper\" }\n",
    )
    .expect("write app manifest");
    std::fs::write(
        project.path().join("build.rs"),
        "use std::{env, fs, path::PathBuf};\n\
         fn main() {\n\
             let generated = PathBuf::from(env::var_os(\"OUT_DIR\").unwrap()).join(\"generated.rs\");\n\
             if !generated.exists() {\n\
                 fs::write(generated, \"fn generated_message() -> &'static str { \\\"generated-value\\\" }\\n\").unwrap();\n\
             }\n\
         }\n",
    )
    .expect("write build script");
    std::fs::write(
        project.path().join("src/main.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n\
         fn main() { println!(\"{} {}\", cache_helper::message(), generated_message()); }\n",
    )
    .expect("write app source");
    std::fs::write(
        project.path().join("helper/Cargo.toml"),
        "[package]\nname = \"cache-helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write helper manifest");
    std::fs::write(
        project.path().join("helper/src/lib.rs"),
        "pub fn message() -> &'static str { \"dependency-value\" }\n",
    )
    .expect("write helper source");

    let first = run_cargo_fixture_build(project.path(), target_root.path());
    assert!(
        first.status.success(),
        "initial Cargo fixture build failed:\n{}{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first_generation = std::fs::canonicalize(project.path().join("target"))
        .expect("resolve first managed Cargo generation");
    let binary = first_generation.join("debug/cache-fixture");
    let rlib = find_fixture_rlib(&first_generation);
    let binary_metadata = binary.metadata().expect("stat root Cargo binary");
    assert!(
        binary_metadata.nlink() >= 2,
        "fixture did not exercise Cargo's deps-to-root hardlink: {binary_metadata:?}"
    );
    assert!(
        rlib.metadata().expect("stat helper rlib").blocks() > 0,
        "path-dependency rlib has no allocated payload to reclaim"
    );

    // Source is deliberately older than target. A rebuild after pruning cannot be explained by
    // Cargo's normal source-newer-than-output mtime rule.
    age_regular_tree(project.path(), 915_148_800);
    age_regular_tree(&first_generation, 946_684_800);
    let before_blocks = unique_regular_blocks(&first_generation);
    assert!(
        before_blocks > 0,
        "Cargo fixture target has no allocated blocks"
    );

    let interrupted = run_direct_target_pruner(&managed_root, true);
    let interrupted_text = format!(
        "{}{}",
        String::from_utf8_lossy(&interrupted.stdout),
        String::from_utf8_lossy(&interrupted.stderr)
    );
    assert_eq!(
        interrupted.status.code(),
        Some(49),
        "forced post-fingerprint failure did not stop before payload reclaim:\n{interrupted_text}"
    );
    assert!(
        interrupted_text.contains("injected failure after durable fingerprint invalidation"),
        "forced failure did not cross the durable fingerprint boundary:\n{interrupted_text}"
    );
    assert!(
        binary
            .metadata()
            .expect("stat binary after forced failure")
            .len()
            > 0
            && rlib
                .metadata()
                .expect("stat rlib after forced failure")
                .len()
                > 0,
        "payload was truncated before every fingerprint became durable"
    );

    let rebuilt_after_interruption = run_cargo_fixture_build(project.path(), target_root.path());
    let rebuilt_text = format!(
        "{}{}",
        String::from_utf8_lossy(&rebuilt_after_interruption.stdout),
        String::from_utf8_lossy(&rebuilt_after_interruption.stderr)
    );
    assert!(
        rebuilt_after_interruption.status.success()
            && rebuilt_text.contains("Compiling cache-helper")
            && rebuilt_text.contains("Compiling cache-fixture"),
        "Cargo accepted a cache after its durable fingerprints were invalidated:\n{rebuilt_text}"
    );

    let second_generation = std::fs::canonicalize(project.path().join("target"))
        .expect("resolve second managed Cargo generation");
    assert_ne!(
        second_generation, first_generation,
        "retired target generation was reused after interrupted reclaim"
    );
    age_regular_tree(&second_generation, 946_684_800);
    let before_blocks = unique_regular_blocks(&second_generation);
    let binary = second_generation.join("debug/cache-fixture");
    let rlib = find_fixture_rlib(&second_generation);
    let generated = find_fixture_generated_source(&second_generation);
    let completed = run_direct_target_pruner(&managed_root, false);
    let completed_text = format!(
        "{}{}",
        String::from_utf8_lossy(&completed.stdout),
        String::from_utf8_lossy(&completed.stderr)
    );
    assert!(
        completed.status.success(),
        "completed Cargo target reclaim failed:\n{completed_text}"
    );
    assert_eq!(
        binary.metadata().expect("stat reclaimed root binary").len(),
        0,
        "internally hardlinked root binary payload was skipped"
    );
    assert_eq!(
        rlib.metadata().expect("stat reclaimed helper rlib").len(),
        0,
        "path-dependency rlib payload was skipped"
    );
    assert_eq!(
        generated
            .metadata()
            .expect("stat reclaimed build-script output")
            .len(),
        0,
        "build-script output name was removed or its payload was exempted"
    );
    let after_blocks = unique_regular_blocks(&second_generation);
    assert!(
        after_blocks < before_blocks / 10,
        "reclaim left most Cargo payload allocated: before={before_blocks} after={after_blocks}\n{completed_text}"
    );

    let immediate_resume = run_direct_target_pruner(&managed_root, false);
    let immediate_text = format!(
        "{}{}",
        String::from_utf8_lossy(&immediate_resume.stdout),
        String::from_utf8_lossy(&immediate_resume.stderr)
    );
    assert!(
        immediate_resume.status.success() && !immediate_text.contains("keeping (active"),
        "reclaim refreshed its own idle files and blocked immediate resume:\n{immediate_text}"
    );

    let rebuilt = run_cargo_fixture_build(project.path(), target_root.path());
    let rebuilt_text = format!(
        "{}{}",
        String::from_utf8_lossy(&rebuilt.stdout),
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    assert!(
        rebuilt.status.success()
            && rebuilt_text.contains("Compiling cache-helper")
            && rebuilt_text.contains("Compiling cache-fixture"),
        "Cargo did not rebuild reclaimed hardlinked outputs:\n{rebuilt_text}"
    );
    let third_generation = std::fs::canonicalize(project.path().join("target"))
        .expect("resolve third managed Cargo generation");
    assert_ne!(
        third_generation, second_generation,
        "completed reclaim did not publish a fresh Cargo namespace"
    );
    let execution = Command::new(third_generation.join("debug/cache-fixture"))
        .output()
        .expect("execute rebuilt Cargo fixture");
    assert!(execution.status.success());
    assert_eq!(execution.stdout, b"dependency-value generated-value\n");
    assert_eq!(
        std::fs::read(find_fixture_generated_source(&third_generation))
            .expect("read fresh build-script output"),
        b"fn generated_message() -> &'static str { \"generated-value\" }\n",
        "fresh generation did not regenerate the retained zero-length OUT_DIR name"
    );
    assert!(
        find_fixture_rlib(&third_generation)
            .metadata()
            .expect("stat rebuilt helper rlib")
            .blocks()
            > 0,
        "Cargo reported compilation but did not restore the helper payload"
    );
}

/// Topology changes participate in the target lease. Once a shared-lease creator finishes, an
/// alias outside the candidate makes the inode deliberately unreclaimable: truncating it would
/// corrupt data outside the cleanup boundary.
#[test]
fn target_pruner_blocks_a_leased_link_creator_and_skips_external_aliases() {
    let root = tempfile::tempdir().expect("target root");
    let outside = tempfile::tempdir().expect("external alias root");
    let target = root.path().join("candidate");
    std::fs::create_dir(&target).expect("create target");
    let payload = target.join("old-payload");
    let alias = outside.path().join("outside-alias");
    std::fs::write(&payload, b"must remain through both links").expect("write target payload");
    age_file(&payload);

    let mut creator = ChildGuard(Some(
        Command::new("flock")
            .args([
                std::ffi::OsStr::new("-s"),
                target.as_os_str(),
                std::ffi::OsStr::new("/bin/bash"),
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new("ln -- \"$PAYLOAD\" \"$ALIAS\"; printf L; IFS= read -r _"),
            ])
            .env("PAYLOAD", &payload)
            .env("ALIAS", &alias)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn shared-lease hardlink creator"),
    ));
    let creator_stdout = creator
        .child_mut()
        .stdout
        .take()
        .expect("hardlink creator stdout");
    assert_eq!(
        read_marker_with_timeout(creator_stdout, "leased hardlink creation"),
        b'L'
    );

    let busy = run_direct_target_pruner(root.path(), false);
    let busy_text = format!(
        "{}{}",
        String::from_utf8_lossy(&busy.stdout),
        String::from_utf8_lossy(&busy.stderr)
    );
    assert!(
        busy.status.success() && busy_text.contains("concurrent cargo holds target lease"),
        "pruner did not defer to shared-lease topology mutation:\n{busy_text}"
    );

    creator
        .child_mut()
        .stdin
        .take()
        .expect("hardlink creator stdin")
        .write_all(b"release\n")
        .expect("release hardlink creator");
    let status = creator.wait().expect("reap hardlink creator");
    assert!(status.success(), "hardlink creator failed: {status:?}");
    assert_eq!(payload.metadata().expect("stat target link").nlink(), 2);

    let skipped = run_direct_target_pruner(root.path(), false);
    let skipped_text = format!(
        "{}{}",
        String::from_utf8_lossy(&skipped.stdout),
        String::from_utf8_lossy(&skipped.stderr)
    );
    assert!(
        skipped.status.success() && skipped_text.contains("external-hardlink payload"),
        "pruner did not report the out-of-boundary hardlink:\n{skipped_text}"
    );
    assert_eq!(
        std::fs::read(&payload).expect("read retained target link"),
        b"must remain through both links"
    );
    assert_eq!(
        std::fs::read(&alias).expect("read retained external alias"),
        b"must remain through both links"
    );
}

/// Cargo follows its fingerprint paths. A symlink or special file inside `.fingerprint` cannot
/// be invalidated through the no-follow target walker, so payload reclamation must fail before
/// touching any output rather than leave an external valid fingerprint beside a zero binary.
#[test]
fn target_pruner_refuses_a_nonregular_fingerprint_before_payload_reclaim() {
    let root = tempfile::tempdir().expect("target root");
    let external = tempfile::tempdir().expect("external fingerprint root");
    let target = root.path().join("candidate");
    let fingerprint = target.join("debug/.fingerprint/cache-fixture");
    std::fs::create_dir_all(&fingerprint).expect("create fingerprint directory");
    let external_fingerprint = external.path().join("fingerprint");
    std::fs::write(&external_fingerprint, b"valid external fingerprint")
        .expect("write external fingerprint");
    age_file(&external_fingerprint);
    std::os::unix::fs::symlink(&external_fingerprint, fingerprint.join("state"))
        .expect("symlink fingerprint outside target");
    let output = target.join("debug/cache-fixture");
    std::fs::write(&output, b"must survive invalidation failure").expect("write cached output");
    age_file(&output);

    let rejected = run_direct_target_pruner(root.path(), false);
    let rejected_text = format!(
        "{}{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(
        rejected.status.code(),
        Some(49),
        "nonregular fingerprint was accepted:\n{rejected_text}"
    );
    assert!(
        rejected_text.contains("nonregular entry inside Cargo .fingerprint"),
        "fingerprint rejection did not identify the unsafe entry:\n{rejected_text}"
    );
    assert_eq!(
        std::fs::read(&output).expect("read output after fingerprint rejection"),
        b"must survive invalidation failure"
    );
    assert_eq!(
        std::fs::read(&external_fingerprint).expect("read external fingerprint"),
        b"valid external fingerprint"
    );
}

fn read_marker_with_timeout<R: Read + Send + 'static>(mut reader: R, context: &str) -> u8 {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut marker = [0_u8; 1];
        let result = reader.read_exact(&mut marker).map(|()| marker[0]);
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
        .unwrap_or_else(|error| panic!("failed reading {context}: {error}"))
}

/// Force the stale-inode interleaving deterministically: the wrapper resolves an old target
/// symlink then waits for its shared lease while link setup wants to repoint it to this worktree's
/// managed generation. The checkout lock must let the wrapper acquire the target lease first;
/// link setup must then wait on that exact old target fd before atomically switching the symlink.
#[test]
fn cargo_target_runner_and_link_repoint_share_one_target_lease() {
    let work = tempfile::tempdir().expect("worktree");
    let btrfs = tempfile::tempdir().expect("btrfs root");
    let target = work.path().join("target");
    let old_target = work.path().join("old-target");
    std::fs::create_dir(&old_target).expect("create old target generation");
    std::fs::write(old_target.join("local-artifact"), b"local").expect("write old artifact");
    std::os::unix::fs::symlink(&old_target, &target).expect("link old target generation");

    let mut blocker = ChildGuard(Some(
        Command::new("flock")
            .args(["-x", "target", "/bin/sh", "-c", "printf L; IFS= read -r _"])
            .current_dir(work.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("hold initial exclusive target lease"),
    ));
    let blocker_stdout = blocker
        .child_mut()
        .stdout
        .take()
        .expect("target blocker stdout");
    assert_eq!(
        read_marker_with_timeout(blocker_stdout, "initial target lease"),
        b'L'
    );

    let signals = tempfile::tempdir().expect("link signal directory");
    let checkout_signal = signals.path().join("checkout-lock.fifo");
    let checkout_attempt_signal = signals.path().join("checkout-attempt.fifo");
    let checkout_attempt_release = signals.path().join("checkout-attempt-release.fifo");
    let target_attempt_signal = signals.path().join("target-attempt.fifo");
    let lock_signal = signals.path().join("target-lock.fifo");
    let status = Command::new("mkfifo")
        .args([
            checkout_signal.as_os_str(),
            checkout_attempt_signal.as_os_str(),
            checkout_attempt_release.as_os_str(),
            target_attempt_signal.as_os_str(),
            lock_signal.as_os_str(),
        ])
        .status()
        .expect("create lease protocol FIFOs");
    assert!(status.success(), "mkfifo failed: {status:?}");
    let checkout_reader = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&checkout_signal)
        .expect("open checkout-lock FIFO without blocking");
    let checkout_attempt_reader = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&checkout_attempt_signal)
        .expect("open checkout-attempt FIFO without blocking");
    let mut checkout_attempt_releaser = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&checkout_attempt_release)
        .expect("open checkout-attempt release FIFO without blocking");
    let target_attempt_reader = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target_attempt_signal)
        .expect("open target-attempt FIFO without blocking");
    let signal_reader = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_signal)
        .expect("open link-lock FIFO without blocking");

    let shim_dir = tempfile::tempdir().expect("flock shim directory");
    let shim = shim_dir.path().join("flock");
    std::fs::write(
        &shim,
        "#!/bin/bash\n\
         resolved=$(readlink \"/proc/$$/fd/${2:-0}\" 2>/dev/null || true)\n\
         if [[ ${1:-} == \"$CHECKOUT_PATH\" ]]; then\n\
           printf W >\"$CHECKOUT_ATTEMPT_SIGNAL\"\n\
           IFS= read -r _ <\"$CHECKOUT_ATTEMPT_RELEASE\"\n\
         fi\n\
         if [[ ${1:-} == -x && $resolved == \"$TARGET_PATH\" ]]; then\n\
           printf X >\"$LOCK_SIGNAL\"\n\
         fi\n\
         if [[ ${1:-} == -s && $resolved == \"$TARGET_PATH\" ]]; then\n\
           printf T >\"$TARGET_ATTEMPT_SIGNAL\"\n\
         fi\n\
         \"$REAL_FLOCK\" \"$@\"\n\
         rc=$?\n\
         if ((rc == 0)) && [[ ${1:-} == -s && $resolved == \"$CHECKOUT_PATH\" ]]; then\n\
           printf Q >\"$CHECKOUT_SIGNAL\"\n\
         fi\n\
         exit \"$rc\"\n",
    )
    .expect("write flock shim");
    let mut permissions = std::fs::metadata(&shim)
        .expect("stat flock shim")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&shim, permissions).expect("make flock shim executable");
    let original_path = std::env::var_os("PATH").expect("PATH");
    let real_flock = std::env::split_paths(&original_path)
        .map(|directory| directory.join("flock"))
        .find(|candidate| candidate.is_file())
        .expect("find real flock");
    let shim_path = std::env::join_paths(
        std::iter::once(shim_dir.path().to_path_buf()).chain(std::env::split_paths(&original_path)),
    )
    .expect("build shim PATH");

    let mut cargo = ChildGuard(Some(
        Command::new(repo_root().join("scripts/cargo-target-run.sh"))
            .args(["/bin/sh", "-c", "printf C; IFS= read -r _"])
            .env("CARGO_TARGET_DIR", "target")
            .env("PATH", &shim_path)
            .env("REAL_FLOCK", &real_flock)
            .env("CHECKOUT_PATH", work.path())
            .env("CHECKOUT_SIGNAL", &checkout_signal)
            .env("CHECKOUT_ATTEMPT_SIGNAL", &checkout_attempt_signal)
            .env("CHECKOUT_ATTEMPT_RELEASE", &checkout_attempt_release)
            .env("TARGET_PATH", &old_target)
            .env("TARGET_ATTEMPT_SIGNAL", &target_attempt_signal)
            .env("LOCK_SIGNAL", &lock_signal)
            .current_dir(work.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn leased Cargo stand-in"),
    ));
    assert_eq!(
        read_marker_with_timeout(checkout_reader, "wrapper checkout lease"),
        b'Q',
        "Cargo wrapper never acquired the checkout lease before waiting on target"
    );
    assert_eq!(
        read_marker_with_timeout(target_attempt_reader, "wrapper target-lease attempt"),
        b'T',
        "Cargo wrapper never reached the blocked target flock while its checkout fd was live"
    );

    let link = ChildGuard(Some(
        Command::new(repo_root().join("scripts/cargo-target-link.sh"))
            .env("BTRFS_ROOT", btrfs.path())
            .env("PATH", &shim_path)
            .env("REAL_FLOCK", &real_flock)
            .env("CHECKOUT_PATH", work.path())
            .env("CHECKOUT_SIGNAL", &checkout_signal)
            .env("CHECKOUT_ATTEMPT_SIGNAL", &checkout_attempt_signal)
            .env("CHECKOUT_ATTEMPT_RELEASE", &checkout_attempt_release)
            .env("TARGET_PATH", &old_target)
            .env("TARGET_ATTEMPT_SIGNAL", &target_attempt_signal)
            .env("LOCK_SIGNAL", &lock_signal)
            .env_remove("CARGO_TARGET_LINK_LOCKED")
            .current_dir(work.path())
            .spawn()
            .expect("spawn concurrent target repoint"),
    ));
    assert_eq!(
        read_marker_with_timeout(checkout_attempt_reader, "link checkout-lock attempt"),
        b'W',
        "link setup never reached the exclusive checkout-lock acquisition"
    );
    let checkout_probe = Command::new(&real_flock)
        .args(["-x", "-n", "-E", "42", ".", "/bin/true"])
        .current_dir(work.path())
        .status()
        .expect("probe checkout lease while link setup is paused before flock");
    assert_eq!(
        checkout_probe.code(),
        Some(42),
        "checkout probe did not fail specifically on lock contention; the Cargo wrapper may have \
         released its checkout lease before acquiring its target lease: {checkout_probe:?}"
    );
    checkout_attempt_releaser
        .write_all(b"attempt\n")
        .expect("release link checkout-lock attempt");

    blocker
        .child_mut()
        .stdin
        .take()
        .expect("target blocker stdin")
        .write_all(b"release\n")
        .expect("release initial target lease");
    let status = blocker.wait().expect("reap initial target blocker");
    assert!(status.success(), "target blocker failed: {status:?}");

    let cargo_stdout = cargo
        .child_mut()
        .stdout
        .take()
        .expect("Cargo stand-in stdout");
    assert_eq!(
        read_marker_with_timeout(cargo_stdout, "Cargo target lease"),
        b'C',
        "link setup acquired the checkout lock before the already-waiting Cargo wrapper"
    );
    assert_eq!(
        read_marker_with_timeout(signal_reader, "link target lease attempt"),
        b'X',
        "link setup never attempted to lock the local target before migration"
    );
    assert!(
        std::fs::symlink_metadata(&target)
            .expect("stat target while repoint is blocked")
            .file_type()
            .is_symlink()
            && std::fs::read_link(&target).expect("read blocked target link") == old_target,
        "link setup rewired the old target before the Cargo target lease was released"
    );

    cargo
        .child_mut()
        .stdin
        .take()
        .expect("Cargo stand-in stdin")
        .write_all(b"release\n")
        .expect("release Cargo stand-in");
    let status = cargo.wait().expect("reap Cargo stand-in");
    assert!(status.success(), "Cargo stand-in failed: {status:?}");
    let status = link.wait().expect("reap target repoint");
    assert!(
        status.success(),
        "target repoint failed after Cargo released its target lease: {status:?}"
    );
    assert!(
        std::fs::symlink_metadata(&target)
            .expect("stat repointed target")
            .file_type()
            .is_symlink(),
        "target was not repointed after the shared lease was released"
    );
    assert_ne!(
        std::fs::read_link(&target).expect("read repointed target link"),
        old_target,
        "target still resolves to the old generation after its lease was released"
    );
    assert_target_usable(work.path(), "after synchronized target repoint");
}

#[test]
fn disk_preflight_shell_scripts_parse() {
    let status = Command::new("bash")
        .arg("-n")
        .args([
            repo_root().join("scripts/runner-disk-preflight.sh"),
            repo_root().join("scripts/prune-cargo-target.sh"),
            repo_root().join("scripts/install-runner-disk-guard.sh"),
        ])
        .status()
        .expect("run bash syntax check for disk-preflight scripts");
    assert!(status.success(), "disk-preflight shell syntax is invalid");
}

/// Exercise the shared installer into a DESTDIR and verify both contents and modes. A string
/// search for an `install` command is satisfiable by comments or dead code and cannot prove the
/// timer entrypoint and privileged helper are deployed as one unit.
#[test]
fn disk_preflight_installs_its_privileged_helper_beside_the_timer_entrypoint() {
    let destination = tempfile::tempdir().expect("installer DESTDIR");
    let root = repo_root();
    let status = Command::new(root.join("scripts/install-runner-disk-guard.sh"))
        .args([root.as_os_str(), destination.path().as_os_str()])
        .status()
        .expect("run disk-guard installer");
    assert!(status.success(), "disk-guard installer failed: {status:?}");

    for (source, installed, mode) in [
        (
            "scripts/runner-disk-preflight.sh",
            "usr/local/bin/runner-disk-preflight.sh",
            0o755,
        ),
        (
            "scripts/prune-cargo-target.sh",
            "usr/local/bin/prune-cargo-target.sh",
            0o755,
        ),
        (
            "scripts/runner-disk-guard.service",
            "etc/systemd/system/runner-disk-guard.service",
            0o644,
        ),
        (
            "scripts/runner-disk-guard.timer",
            "etc/systemd/system/runner-disk-guard.timer",
            0o644,
        ),
    ] {
        let source = root.join(source);
        let installed = destination.path().join(installed);
        assert_eq!(
            std::fs::read(&installed).expect("read installed disk-guard file"),
            std::fs::read(&source).expect("read source disk-guard file"),
            "installer deployed different bytes for {source:?}"
        );
        assert_eq!(
            std::fs::metadata(&installed)
                .expect("stat installed disk-guard file")
                .permissions()
                .mode()
                & 0o777,
            mode,
            "installer used the wrong mode for {installed:?}"
        );
    }

    let build = std::fs::read_to_string(repo_root().join("scripts/build-ami.sh"))
        .expect("read build-ami.sh");
    let setup = std::fs::read_to_string(repo_root().join("scripts/setup-runner.sh"))
        .expect("read setup-runner.sh");
    assert!(
        build.contains("/tmp/fcvm/scripts/install-runner-disk-guard.sh /tmp/fcvm")
            && setup
                .contains("/tmp/fcvm-passt/scripts/install-runner-disk-guard.sh /tmp/fcvm-passt"),
        "an AMI provisioning path does not invoke the behaviorally verified shared installer"
    );
}

/// All recipe-level Cargo executions must expand through the lease wrapper.
/// Keeping the override in one variable makes this enforceable, while the raw
/// command check catches one-off recipes that bypass `$(CARGO)`.
fn shell_command_segments(line: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if escaped {
            segment.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            segment.push(ch);
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
            segment.push(ch);
            continue;
        }
        let chain_boundary = quote.is_none()
            && (ch == ';' || ((ch == '&' || ch == '|') && chars.peek().copied() == Some(ch)));
        if chain_boundary {
            if ch != ';' {
                chars.next();
            }
            segments.push(std::mem::take(&mut segment));
        } else {
            segment.push(ch);
        }
    }
    segments.push(segment);
    segments
}

#[test]
fn makefile_routes_every_cargo_command_through_the_target_lease() {
    let mk = std::fs::read_to_string(repo_root().join("Makefile")).expect("read Makefile");
    assert!(
        mk.lines().any(|line| {
            let Some(assignment) = line.trim_start().strip_prefix("override CARGO") else {
                return false;
            };
            let assignment = assignment.trim_start();
            (assignment.starts_with("::=")
                || assignment.starts_with(":=")
                || assignment.starts_with('='))
                && assignment.contains("scripts/cargo-target-run.sh")
        }),
        "Makefile's CARGO command is not forced through scripts/cargo-target-run.sh"
    );

    assert_eq!(
        shell_command_segments("echo building && cargo build"),
        ["echo building ", " cargo build"],
        "chained Cargo commands would escape the Makefile guard"
    );
    assert_eq!(
        shell_command_segments("printf 'cargo test && quoted; only'"),
        ["printf 'cargo test && quoted; only'"],
        "quoted output text was split into a false command"
    );

    let raw_commands = [
        "cargo build",
        "cargo test",
        "cargo nextest",
        "cargo install",
        "cargo bench",
        "cargo fmt",
        "cargo clippy",
        "cargo audit",
        "cargo deny",
        "cargo update",
    ];
    let bypasses: Vec<String> = mk
        .lines()
        .filter(|line| line.starts_with('\t'))
        .flat_map(shell_command_segments)
        .map(|segment| {
            segment
                .trim_start()
                .trim_start_matches(['@', '-', '+'])
                .trim_start()
                .to_string()
        })
        .filter(|command| {
            !command.starts_with('#')
                && !command.starts_with("echo ")
                && !command.starts_with("printf ")
        })
        .filter(|command| {
            raw_commands
                .iter()
                .any(|raw| command.starts_with(raw) || command.contains(&format!(" {raw}")))
        })
        .collect();
    assert!(
        bypasses.is_empty(),
        "Makefile recipe(s) bypass the cargo-target shared lease: {bypasses:#?}"
    );
    assert!(
        !mk.contains("rm -rf target")
            && mk.lines().any(|line| {
                line.starts_with('\t')
                    && line.contains("scripts/cargo-target-link.sh")
                    && line.contains("--rotate")
            }),
        "Makefile clean mutates target dentries outside the generation/lease protocol"
    );
}

/// The hourly guard runs outside Actions jobs, so self-hosted workflows and maintenance scripts
/// must not bypass the target inode lease with a raw Cargo command. Hosted-only CI lanes do not
/// share these persistent targets and are intentionally outside this assertion.
#[test]
fn persistent_runner_entrypoints_route_cargo_through_the_lease() {
    let ci = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml"))
        .expect("read primary CI workflow");
    let self_hosted = ci
        .split_once("# Runner 1a: Host")
        .map(|(_, section)| section)
        .expect("find self-hosted CI jobs");
    assert_eq!(
        self_hosted.matches("cache-targets: \"false\"").count(),
        2,
        "both persistent host matrices must prevent rust-cache from materializing an unleased \
         real target directory after disk preflight"
    );
    let weekly = std::fs::read_to_string(repo_root().join(".github/workflows/weekly.yml"))
        .expect("read weekly workflow");
    assert!(
        !weekly.lines().any(|line| {
            let command = line.trim_start();
            command.starts_with("sudo rm") && command.contains("/target")
        }),
        "weekly self-hosted cleanup recursively removes target outside the lease protocol"
    );

    let kernels = std::fs::read_to_string(repo_root().join(".github/workflows/kernels.yml"))
        .expect("read kernel workflow");
    assert_eq!(
        kernels.matches("make build-host-tools").count(),
        2,
        "both persistent kernel-builder jobs must use the leased Make target"
    );
    assert!(
        !kernels
            .lines()
            .any(|line| line.trim_start().starts_with("cargo build")),
        "kernel workflow contains a raw Cargo build that can race the systemd pruner"
    );

    let fuse_runner = std::fs::read_to_string(repo_root().join("scripts/run_fuse_pipe_tests.sh"))
        .expect("read fuse-pipe runner");
    assert!(
        fuse_runner.contains("make cargo-target-link")
            && fuse_runner
                .matches("\"${CARGO_RUNNER}\" cargo test")
                .count()
                == 4,
        "fuse-pipe runner does not set up and lease all four Cargo test invocations"
    );

    for path in ["scripts/build-ami.sh", "scripts/fcvm-init.sh"] {
        let source = std::fs::read_to_string(repo_root().join(path))
            .unwrap_or_else(|error| panic!("read {path}: {error}"));
        assert!(
            source.contains("make build-host-tools") && !source.contains("cargo build"),
            "{path} bypasses the leased host-tools Make target"
        );
    }
}
