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

    fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        self.0
            .take()
            .expect("child already reaped")
            .wait_with_output()
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

/// A recipe line under `scripts/cargo-target-run.sh` (`TARGET_LEASE_SHELL` in the Makefile)
/// holds the published generation's shared lease for its whole life, and every child
/// inherits the descriptor. `scripts/cargo-target-link.sh` takes that same generation
/// exclusively before it publishes, so a link run from inside such a line waits on its own
/// ancestor. Observed 2026-09-02 on parallel-box 2: `make bench-chromium-corpus` ->
/// corpus_campaign.sh -> `make -C $REPO bench-chromium-request-golden` ->
/// cargo-target-link.sh, `flock -x` parked in locks_lock_inode_wait with the box at load
/// 0.00, until killed.
///
/// The wrapper names the lease it hands down (`FCVM_TARGET_LEASE_HELD`) and the link script
/// refuses at once instead of waiting. RED BEFORE THE FIX: the link never exits and this
/// test kills the process group at the deadline.
#[test]
fn cargo_target_link_refuses_inside_a_leased_recipe_line() {
    use std::os::unix::process::CommandExt;

    let work = tempfile::tempdir().expect("tempdir");
    let btrfs = tempfile::tempdir().expect("tempdir");
    let (ok, out) = run_link(work.path(), btrfs.path());
    assert!(ok, "cargo-target-link.sh failed:\n{out}");
    assert_target_usable(work.path(), "before the leased line");
    let published = std::fs::read_link(work.path().join("target")).expect("target link");

    // The wrapper's `-c <line>` form is how make invokes a SHELL; the line runs
    // the link script the way a nested `make` prerequisite would.
    let line = format!(
        "exec bash {}",
        shell_quote(&repo_root().join("scripts/cargo-target-link.sh"))
    );
    let mut child = ChildGuard(Some(
        Command::new(repo_root().join("scripts/cargo-target-run.sh"))
            .args(["-c", &line])
            .env("BTRFS_ROOT", btrfs.path())
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_TARGET_LINK_LOCKED")
            .env_remove("FCVM_TARGET_LEASE_HELD")
            .current_dir(work.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("spawn scripts/cargo-target-run.sh -c"),
    ));
    let pgid = child.child_mut().id() as libc::pid_t;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if child
            .child_mut()
            .try_wait()
            .expect("poll leased line")
            .is_some()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // The whole group: the wrapper exec'd into the link script, which
            // re-exec'd under `flock <checkout>` and forked the blocked `flock -x`.
            // SAFETY: plain libc call on a pgid this test created with process_group(0).
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
            panic!(
                "cargo-target-link.sh ran inside a leased recipe line and never returned: it \
                 is waiting for the exclusive generation lease its own ancestor holds shared \
                 (published generation {published:?}). The link script must refuse instead."
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().expect("collect leased line");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "the link script published target/ from inside a leased recipe line; a recipe that \
         runs make under the lease would have hung here before the guard:\n{text}"
    );
    assert!(
        text.contains("FCVM_TARGET_LEASE_HELD"),
        "the refusal must name the inherited lease marker so the recipe author knows what to \
         change:\n{text}"
    );
    // Refusing must leave the published link alone.
    assert_eq!(
        std::fs::read_link(work.path().join("target")).expect("target link"),
        published,
        "the refused link run replaced target/"
    );
    assert_target_usable(work.path(), "after the refused leased line");
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
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

    // And it must be the script's own fallback link, not the stale link with
    // its target recreated. `$BTRFS_ROOT` absent means the volume is unmounted;
    // recreating the path underneath a mountpoint writes build artifacts to the
    // small root filesystem while still looking like btrfs, the exact failure
    // this whole indirection exists to avoid.
    assert_local_fallback_link(work.path(), &gone, "btrfs root absent");
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
/// anywhere in the Makefile is also satisfied by a comment mentioning the
/// script, which would pass with the recipe deleted.
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

/// The disk preflight selects per-worktree children, never the parent
/// `cargo-target/` directory. A glob that matches the parent makes an idle
/// sibling unreclaimable on its own, and removing the parent leaves every
/// checkout's target symlink dangling.
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

/// Once one candidate has been fully inspected and reclaimed, its exclusive lease no longer
/// protects any pending decision. Release that lease immediately while retaining later candidate
/// leases, so a slow sibling cannot unnecessarily block Cargo on an already completed target.
fn assert_target_pruner_releases_each_completed_candidate_lease(fail_reclaim: bool) {
    let root = tempfile::tempdir().expect("target root");
    let first = root.path().join("candidate-a");
    let second = root.path().join("candidate-b");
    for candidate in [&first, &second] {
        std::fs::create_dir(candidate).expect("create candidate");
        let payload = candidate.join("old-payload");
        std::fs::write(&payload, b"old payload").expect("write old payload");
        age_file(&payload);
    }

    let signals = tempfile::tempdir().expect("release sync socket directory");
    let socket_path = signals.path().join("released.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .expect("bind candidate-release sync socket");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    let expected_releases = if fail_reclaim { 1 } else { 2 };
    let sync = std::thread::spawn(move || {
        let _signals = signals;
        for index in 0..expected_releases {
            let (mut channel, _) = listener.accept().expect("accept candidate-release sync");
            let mut path = Vec::new();
            loop {
                let mut byte = [0_u8; 1];
                channel
                    .read_exact(&mut byte)
                    .expect("read released candidate path");
                if byte == [0] {
                    break;
                }
                path.push(byte[0]);
            }
            if index == 0 {
                let path = PathBuf::from(String::from_utf8(path).expect("candidate path is UTF-8"));
                ready_tx
                    .send(path)
                    .expect("signal first released candidate");
                continue_rx.recv().expect("wait for candidate lease probes");
            }
            channel.write_all(b"C").expect("release target helper");
        }
    });

    let mut helper_command = Command::new(repo_root().join("scripts/prune-cargo-target.sh"));
    helper_command
        .args([
            std::ffi::OsStr::new("1 day ago"),
            std::ffi::OsStr::new("0"),
            std::ffi::OsStr::new(""),
            root.path().as_os_str(),
        ])
        .env("FCVM_PRUNE_TEST_AFTER_RELEASE_SOCKET", &socket_path);
    if fail_reclaim {
        helper_command.env("FCVM_PRUNE_TEST_FAIL_AFTER_FINGERPRINTS", "1");
    }
    let helper = ChildGuard(Some(
        helper_command
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn release-synchronized target helper"),
    ));

    let released = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("helper did not announce a released candidate lease");
    let retained = if released == first { &second } else { &first };
    assert!(
        released == first || released == second,
        "helper announced an unknown candidate path: {released:?}"
    );
    let released_probe = Command::new("flock")
        .args(["-x", "-n", "-E", "42"])
        .arg(&released)
        .arg("/bin/true")
        .status()
        .expect("probe released candidate lease");
    assert!(
        released_probe.success(),
        "completed candidate still held its exclusive lease: {released:?} {released_probe:?}"
    );
    let retained_probe = Command::new("flock")
        .args(["-x", "-n", "-E", "42"])
        .arg(retained)
        .arg("/bin/true")
        .status()
        .expect("probe retained candidate lease");
    assert_eq!(
        retained_probe.code(),
        Some(42),
        "later candidate was unlocked before its safety checks and reclaim: {retained:?} \
         {retained_probe:?}"
    );

    continue_tx.send(()).expect("continue target helper");
    let output = helper.wait_with_output().expect("wait for target helper");
    let status = output.status;
    let stderr = String::from_utf8_lossy(&output.stderr);
    sync.join().expect("join candidate-release sync peer");
    if fail_reclaim {
        assert_eq!(
            status.code(),
            Some(49),
            "injected reclaim failure did not propagate after releasing its lease: {status:?}"
        );
    } else {
        assert!(status.success(), "target helper failed: {status:?}");
    }
    let mut duration_paths = HashSet::new();
    for line in stderr.lines() {
        let Some((_, metric)) = line.split_once("candidate lease released after ") else {
            continue;
        };
        let (seconds, path) = metric
            .split_once("s: ")
            .unwrap_or_else(|| panic!("malformed candidate lease duration metric: {line}"));
        let seconds: f64 = seconds.parse().unwrap_or_else(|error| {
            panic!("invalid candidate lease duration {seconds:?}: {error}")
        });
        assert!(
            seconds.is_finite() && seconds >= 0.0,
            "candidate lease duration is not finite and nonnegative: {line}"
        );
        let path = PathBuf::from(path);
        assert!(
            path == first || path == second,
            "duration metric named an unknown candidate: {line}"
        );
        assert!(
            duration_paths.insert(path),
            "candidate emitted more than one lease duration metric: {line}"
        );
    }
    assert_eq!(
        duration_paths.len(),
        expected_releases,
        "target helper did not report exactly one lease duration for each completed candidate:\n{stderr}"
    );
    for candidate in [&first, &second] {
        let length = std::fs::metadata(candidate.join("old-payload"))
            .expect("stat candidate payload")
            .len();
        if fail_reclaim {
            assert_eq!(
                length, 11,
                "failed reclaim changed a candidate payload: {candidate:?}"
            );
        } else {
            assert_eq!(
                length, 0,
                "candidate payload was not reclaimed: {candidate:?}"
            );
        }
    }
}

#[test]
fn target_pruner_releases_each_completed_candidate_lease() {
    assert_target_pruner_releases_each_completed_candidate_lease(false);
}

#[test]
fn target_pruner_releases_the_failed_candidate_lease() {
    assert_target_pruner_releases_each_completed_candidate_lease(true);
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
        .env("RUSTC_WRAPPER", project.join(".fcvm-rustc-wrapper.sh"))
        .env("FCVM_RUSTC_LOG", project.join(".fcvm-rustc-invocations"))
        .current_dir(project)
        .output()
        .expect("build Cargo cache fixture through target lease")
}

fn assert_fixture_crates_compiled(project: &Path, diagnostics: &str) {
    let invocations = std::fs::read_to_string(project.join(".fcvm-rustc-invocations"))
        .unwrap_or_else(|error| panic!("read fixture rustc invocations: {error}; {diagnostics}"));
    let crates: Vec<_> = invocations.lines().collect();
    for expected in ["cache_helper", "cache_fixture"] {
        assert!(
            crates.contains(&expected),
            "rustc was not invoked for {expected} after cache invalidation; crates={crates:?}\n{diagnostics}"
        );
    }
}

fn clear_fixture_compile_log(project: &Path) {
    match std::fs::remove_file(project.join(".fcvm-rustc-invocations")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("clear fixture rustc invocations: {error}"),
    }
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
    let rustc_wrapper = project.path().join(".fcvm-rustc-wrapper.sh");
    std::fs::write(
        &rustc_wrapper,
        "#!/bin/sh\n\
         rustc=$1\n\
         shift\n\
         previous=\n\
         for argument do\n\
             if [ \"$previous\" = --crate-name ]; then\n\
                 printf '%s\\n' \"$argument\" >>\"$FCVM_RUSTC_LOG\"\n\
             fi\n\
             previous=$argument\n\
         done\n\
         exec \"$rustc\" \"$@\"\n",
    )
    .expect("write fixture rustc wrapper");
    std::fs::set_permissions(&rustc_wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("make fixture rustc wrapper executable");

    let first = run_cargo_fixture_build(project.path(), target_root.path());
    assert!(
        first.status.success(),
        "initial Cargo fixture build failed:\n{}{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first_text = format!(
        "{}{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert_fixture_crates_compiled(project.path(), &first_text);
    clear_fixture_compile_log(project.path());
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
        rebuilt_after_interruption.status.success(),
        "Cargo accepted a cache after its durable fingerprints were invalidated:\n{rebuilt_text}"
    );
    assert_fixture_crates_compiled(project.path(), &rebuilt_text);
    clear_fixture_compile_log(project.path());

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
        rebuilt.status.success(),
        "Cargo did not rebuild reclaimed hardlinked outputs:\n{rebuilt_text}"
    );
    assert_fixture_crates_compiled(project.path(), &rebuilt_text);
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
            repo_root().join("scripts/cargo-target-lib.sh"),
            repo_root().join("scripts/cargo-target-link.sh"),
            repo_root().join("scripts/cargo-target-run.sh"),
            repo_root().join("scripts/run_fuse_pipe_tests.sh"),
        ])
        .status()
        .expect("run bash syntax check for disk-preflight scripts");
    assert!(status.success(), "disk-preflight shell syntax is invalid");
}

#[test]
fn disk_preflight_initializes_candidate_roots_without_sc1007() {
    let source = std::fs::read_to_string(repo_root().join("scripts/runner-disk-preflight.sh"))
        .expect("read runner disk preflight");
    let stage = source
        .split_once("stage_target_dirs() {")
        .expect("stage_target_dirs function is missing")
        .1
        .split_once("\n}")
        .expect("stage_target_dirs function is unterminated")
        .0;
    let declaration = stage
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("local runner_root"))
        .expect("stage_target_dirs does not declare its candidate roots");
    assert_eq!(
        declaration, "local runner_root='' btrfs_target_root=''",
        "candidate-root locals must use explicit empty assignments accepted by ShellCheck SC1007"
    );
}

#[test]
fn cargo_target_wrappers_share_one_retirement_protocol() {
    let library = std::fs::read_to_string(repo_root().join("scripts/cargo-target-lib.sh"))
        .expect("read shared cargo-target library");
    assert!(
        library.contains("target_is_retired()")
            && library.contains("user.fcvm.retired")
            && library.contains("unsupported retired-generation marker"),
        "shared cargo-target library does not own the retirement protocol"
    );
    for wrapper in [
        "scripts/cargo-target-link.sh",
        "scripts/cargo-target-run.sh",
    ] {
        let source = std::fs::read_to_string(repo_root().join(wrapper))
            .unwrap_or_else(|error| panic!("read {wrapper}: {error}"));
        assert!(
            source.contains("cargo-target-lib.sh") && !source.contains("target_is_retired()"),
            "{wrapper} does not source the shared retirement protocol exactly once"
        );
    }
}

/// `sudo` resets the environment on the privileged half of the fuse-pipe
/// sweep. Run the real orchestration script with an environment-clearing sudo
/// shim and observe the Cargo process, proving both managed-target variables
/// cross that boundary as values rather than merely appearing in source text.
#[test]
fn fuse_pipe_privileged_cargo_preserves_target_protocol_environment() {
    let fixture = tempfile::tempdir().expect("fuse-pipe environment fixture");
    let shims = fixture.path().join("bin");
    let log_dir = fixture.path().join("logs");
    let cargo_target = fixture.path().join("cargo-target");
    let btrfs_root = fixture.path().join("btrfs-root");
    let observed = fixture.path().join("cargo-environment.log");
    for directory in [&shims, &log_dir, &cargo_target, &btrfs_root] {
        std::fs::create_dir_all(directory).expect("create fuse-pipe fixture directory");
    }

    for (name, source) in [
        ("make", "#!/bin/sh\nexit 0\n"),
        (
            "cargo",
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$*\" \"${CARGO_TARGET_DIR-unset}\" \"${BTRFS_ROOT-unset}\" >>\"$FCVM_TEST_LOG\"\n",
        ),
        (
            "sudo",
            "#!/bin/sh\nexec /usr/bin/env -i PATH=\"$PATH\" FCVM_TEST_LOG=\"$FCVM_TEST_LOG\" \"$@\"\n",
        ),
    ] {
        let path = shims.join(name);
        std::fs::write(&path, source).expect("write fuse-pipe command shim");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make fuse-pipe command shim executable");
    }

    let inherited_path = std::env::var_os("PATH").expect("PATH is set");
    let mut path = shims.into_os_string();
    path.push(":");
    path.push(inherited_path);
    let output = Command::new(repo_root().join("scripts/run_fuse_pipe_tests.sh"))
        .env("PATH", path)
        .env("FCVM_TEST_LOG", &observed)
        .env("LOG_DIR", &log_dir)
        .env("STEP_TIMEOUT", "5")
        .env("CARGO_TARGET_DIR", &cargo_target)
        .env("BTRFS_ROOT", &btrfs_root)
        .output()
        .expect("run real fuse-pipe orchestration with command shims");
    assert!(
        output.status.success(),
        "fuse-pipe orchestration failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let observed = std::fs::read_to_string(&observed).expect("read observed Cargo environment");
    for test_name in ["stress", "pjdfstest_matrix"] {
        let expected_prefix = format!("test -p fuse-pipe --test {test_name} -- --nocapture|");
        let line = observed
            .lines()
            .find(|line| line.starts_with(&expected_prefix))
            .unwrap_or_else(|| {
                panic!("privileged {test_name} Cargo command was not observed:\n{observed}")
            });
        assert_eq!(
            line,
            format!(
                "test -p fuse-pipe --test {test_name} -- --nocapture|{}|{}",
                cargo_target.display(),
                btrfs_root.display()
            ),
            "privileged {test_name} lost the managed-target environment through sudo"
        );
    }
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
    assert_eq!(
        fuse_runner
            .matches("CARGO_TARGET_DIR=\"${CARGO_TARGET_DIR:-target}\"")
            .count(),
        2,
        "both privileged fuse-pipe Cargo runs must preserve CARGO_TARGET_DIR through sudo"
    );
    assert_eq!(
        fuse_runner
            .matches("BTRFS_ROOT=\"${BTRFS_ROOT:-/mnt/fcvm-btrfs}\"")
            .count(),
        2,
        "both privileged fuse-pipe Cargo runs must preserve BTRFS_ROOT through sudo"
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

/// The volume EXISTS but cannot be written: keep the local `target/`.
///
/// GitHub-hosted runners have no `/mnt/fcvm-btrfs`, and podman CREATES it as an
/// empty root-owned directory to satisfy `CONTAINER_RUN_BASE`'s bind mount. Inside
/// the container the volume is therefore a directory (`[ -d ]` true) that the
/// build user cannot write, while the checkout's own `target/` is a bind mount --
/// a real directory the recipe should simply keep. The script tested `-d`, took
/// the btrfs branch, and died on `mkdir` before reaching its own
/// "retaining unmanaged local target/" exit:
///
/// ```text
/// mkdir: cannot create directory '/mnt/fcvm-btrfs/cargo-target': Permission denied
/// ERROR: cannot create /mnt/fcvm-btrfs/cargo-target/fcvm-1602bce1
/// make: *** [Makefile:337: cargo-target-link] Error 1
/// ```
///
/// Every Weekly `container-bench` since 2026-08-10 (the script landed 08-09; the
/// last green Weekly was 08-03). Unwritable is produced two ways because this
/// file also runs under sudo, where mode bits cannot stop root: as a normal user
/// a 0o555 tempdir, as root a procfs path that no uid can `mkdir` under.
#[test]
fn cargo_target_link_keeps_a_bind_mounted_target_when_btrfs_is_unwritable() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    // The bind-mounted target/: a real directory that already exists.
    let target = checkout.path().join("target");
    std::fs::create_dir(&target).expect("pre-existing target/");
    // Retention is about THIS dentry. A run that deletes it and creates
    // another writable directory in its place satisfies "target/ is a real
    // directory" while the bind mount it carried is gone, so the identity and
    // the contents are what the assertions read. Both are needed: a
    // delete-and-recreate here reuses the same inode number, so it is the
    // sentinel that catches it.
    let sentinel = target.join("carried-by-the-mount");
    std::fs::write(&sentinel, b"data only reachable through the mounted dentry")
        .expect("write sentinel into the pre-existing target/");
    let before = std::fs::symlink_metadata(&target).expect("stat the pre-existing target/");

    let unwritable_tmp = tempfile::tempdir().expect("btrfs stand-in");
    let btrfs_root: PathBuf = if nix_geteuid_is_root() {
        PathBuf::from("/proc")
    } else {
        std::fs::set_permissions(
            unwritable_tmp.path(),
            std::fs::Permissions::from_mode(0o555),
        )
        .expect("chmod 0555");
        unwritable_tmp.path().to_path_buf()
    };

    let (ok, out) = run_link(checkout.path(), &btrfs_root);

    // Restore so the tempdir can be removed whatever the outcome.
    let _ = std::fs::set_permissions(
        unwritable_tmp.path(),
        std::fs::Permissions::from_mode(0o755),
    );

    assert!(
        ok,
        "the recipe failed instead of keeping the existing target/ when the btrfs \
         volume is present but unwritable:\n{out}"
    );
    assert!(
        target.is_dir() && !target.is_symlink(),
        "target/ should have been kept as the real (bind-mounted) directory it was; \
         it is now {:?}\n{out}",
        std::fs::symlink_metadata(&target).map(|m| m.file_type())
    );
    let after = std::fs::symlink_metadata(&target).expect("stat the retained target/");
    assert_eq!(
        (before.dev(), before.ino()),
        (after.dev(), after.ino()),
        "target/ is a directory again but not the SAME one: dev/ino changed, so the dentry the \
         bind mount was attached to was unlinked and replaced:\n{out}"
    );
    assert_eq!(
        std::fs::read(&sentinel).ok().as_deref(),
        Some(b"data only reachable through the mounted dentry".as_slice()),
        "the retained target/ lost the contents it came in with:\n{out}"
    );
    assert_target_usable(checkout.path(), "unwritable btrfs volume");
}

fn nix_geteuid_is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The worktree directory EXISTS on the volume but cannot be written into.
///
/// `mkdir -p` is idempotent: on an existing directory it succeeds without
/// creating anything, so a writability probe built on it alone selects the
/// managed branch and publishes a symlink to a directory Cargo cannot write --
/// exit 0, then an opaque failure several steps later. Ownership changes and
/// read-only remounts both produce this state. The probe has to CREATE an entry
/// inside the directory, not merely confirm the directory.
///
/// Root cannot be stopped by mode bits, so the root branch remounts the volume
/// read-only through a bind mount and gets EROFS instead.
#[test]
fn cargo_target_link_falls_back_when_the_existing_worktree_dir_is_unwritable() {
    // A FRESH checkout: no target/ yet. With one pre-existing, the script's
    // "retaining unmanaged local target/" exit fires before any symlink is
    // published and hides exactly the defect under test.
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let _wt = unwritable_managed_worktree_dir(checkout.path(), btrfs.path());

    let (ok, out) = run_link(checkout.path(), btrfs.path());

    assert!(ok, "the recipe failed outright:\n{out}");
    assert_local_fallback_link(
        checkout.path(),
        btrfs.path(),
        "existing but unwritable worktree dir",
    );
}

/// "Is a directory" is not "is writable". A `-d` test alone reports a target/
/// nothing can write as a successful fallback, and cargo fails several steps
/// later with its own error, so every path that leaves target/ as a plain local
/// directory runs `require_writable_local_target`. Two of the three exits are
/// driven here through the fixture every uid can use: target/ pointing at
/// /proc/self, a directory root itself cannot create in. The third (retaining an
/// unmanaged REAL directory) cannot be staged for uid 0 without a mount or
/// `chattr +i`, neither of which the container lane permits (overlayfs refuses
/// the flag), so it is pinned structurally in
/// `every_local_target_exit_probes_writability_first`.
#[test]
fn cargo_target_link_refuses_an_unwritable_local_target_when_btrfs_is_unusable() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    std::os::unix::fs::symlink("/proc/self", checkout.path().join("target"))
        .expect("procfs-backed local target/");
    let unwritable_tmp = tempfile::tempdir().expect("btrfs stand-in");
    let btrfs_root: PathBuf = if nix_geteuid_is_root() {
        PathBuf::from("/proc")
    } else {
        std::fs::set_permissions(
            unwritable_tmp.path(),
            std::fs::Permissions::from_mode(0o555),
        )
        .expect("chmod 0555");
        unwritable_tmp.path().to_path_buf()
    };

    let (ok, out) = run_link(checkout.path(), &btrfs_root);
    let _ = std::fs::set_permissions(
        unwritable_tmp.path(),
        std::fs::Permissions::from_mode(0o755),
    );

    assert!(
        !ok,
        "the recipe reported success with a target/ nothing can write (btrfs unusable, \
         local target/ kept on a `-d` test alone):\n{out}"
    );
    assert!(
        out.contains("nothing can write"),
        "the failure must say the local target/ is unwritable, not something else:\n{out}"
    );
}

#[test]
fn cargo_target_link_refuses_an_unwritable_local_target_on_managed_fallback() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    // The managed worktree dir cannot be written, so the script falls back to
    // the local target/ ... which is just as unwritable, and is not a managed
    // link, so the fallback keeps it.
    let _wt = unwritable_managed_worktree_dir(checkout.path(), btrfs.path());
    std::os::unix::fs::symlink("/proc/self", checkout.path().join("target"))
        .expect("procfs-backed local target/");

    let (ok, out) = run_link(checkout.path(), btrfs.path());

    assert!(
        !ok,
        "the managed-dir fallback reported success with a local target/ nothing can \
         write:\n{out}"
    );
    assert!(
        out.contains("nothing can write"),
        "the failure must say the local target/ is unwritable, not something else:\n{out}"
    );
}

/// A managed link is DROPPED, never probed through, when its generation cannot
/// be leased.
///
/// The unusable-volume path (the volume exists but `$WT_TARGET` cannot be
/// created) never opens a generation lease, yet `target/` can still resolve
/// into the managed tree: a rotated `.generation-*` directory outlives the
/// canonical path the pruner removed. Probing writability THROUGH that link
/// creates and removes a file inside a generation this run holds no lease on,
/// which is the census/rewalk race the lease protocol exists to prevent; and if
/// that generation is itself unwritable the script fails instead of falling
/// back. So the link goes and a local `target/` takes its place; the generation
/// is left untouched for the pruner. Staged for every uid: a dangling symlink
/// at `$WT_TARGET` defeats `mkdir -p` for root too.
#[test]
fn cargo_target_link_drops_a_managed_link_it_cannot_lease() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let wt = managed_worktree_dir(checkout.path(), btrfs.path());
    std::fs::create_dir_all(wt.parent().unwrap()).expect("cargo-target parent");
    let generation = wt.with_file_name(format!(
        "{}.generation-stale000",
        wt.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&generation).expect("retained generation");
    // `set_file_time` opens a FILE; a directory's mtime is pinned with touch.
    let pinned = std::process::Command::new("touch")
        .args(["-d", "@1600000000"])
        .arg(&generation)
        .status()
        .expect("run touch");
    assert!(pinned.success(), "pin the generation's mtime");
    std::os::unix::fs::symlink(btrfs.path().join("cargo-target/absent"), &wt)
        .expect("dangling $WT_TARGET, so mkdir -p fails for every uid");
    std::os::unix::fs::symlink(&generation, checkout.path().join("target"))
        .expect("target/ -> retained generation");

    let (ok, out) = run_link(checkout.path(), btrfs.path());

    assert!(
        ok,
        "the recipe failed instead of falling back to a local target/:\n{out}"
    );
    assert_local_fallback_link(
        checkout.path(),
        btrfs.path(),
        "managed link dropped on an unusable volume",
    );
    let mtime = std::fs::metadata(&generation)
        .expect("generation still exists")
        .modified()
        .expect("mtime")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    assert_eq!(
        mtime, 1_600_000_000,
        "the retained generation was written through (a probe file was created and \
         removed in it) without a lease\n{out}"
    );
}

/// An existing `$WT_TARGET` that cannot be OPENED falls back too. Losing read
/// permission as well as write (an ownership change that leaves a fresh
/// checkout's directory 0700, say) passes `mkdir -p`, but the generation-lease
/// open `exec {fd}<"$candidate"` fails; under `set -e` that ends the script
/// instead of creating the promised local target/.
/// Root can open any directory, so when the tests are root the script runs as
/// uid 65534 through `setpriv`, from a copy of scripts/ that uid can read, over
/// tempdirs handed to that uid.
#[test]
fn cargo_target_link_falls_back_when_the_managed_dir_cannot_be_opened() {
    use std::os::unix::fs::PermissionsExt as _;
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let wt = managed_worktree_dir(checkout.path(), btrfs.path());
    std::fs::create_dir_all(&wt).expect("existing managed dir");
    std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");

    let (ok, out) = run_link_unprivileged(checkout.path(), btrfs.path());

    // Hand the directory back before asserting, so the tempdir can be removed.
    let _ = std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o755));
    assert!(
        ok,
        "the recipe died instead of falling back when the managed dir cannot be \
         opened:\n{out}"
    );
    assert_local_fallback_link(
        checkout.path(),
        btrfs.path(),
        "managed dir that cannot be opened",
    );
}

/// `run_link`, but as uid 65534 when the tests are root: root can open and
/// write any directory, so a fixture about permissions must run as someone who
/// cannot. Both tempdirs are handed to that uid, and scripts/ is copied where it
/// can read them (the script sources cargo-target-lib.sh next to itself).
fn run_link_unprivileged(dir: &Path, btrfs_root: &Path) -> (bool, String) {
    run_link_unprivileged_with(dir, btrfs_root, &[])
}

/// `run_link` with extra script arguments (`--rotate`).
fn run_link_with(dir: &Path, btrfs_root: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(repo_root().join("scripts/cargo-target-link.sh"))
        .args(args)
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

/// The PUBLISHED generation that cannot be opened FAILS CLOSED; it does not
/// fall back. Without that generation's lease the script cannot know whether a
/// Cargo wrapper still holds it shared, and a generation
/// that lost only read permission (0333) is one Cargo can still create entries
/// in; replacing `target/` under such a build would split it across two trees.
/// Same unprivileged stand-in as the candidate case.
#[test]
fn cargo_target_link_refuses_to_replace_a_published_generation_it_cannot_lease() {
    use std::os::unix::fs::PermissionsExt as _;
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let wt = managed_worktree_dir(checkout.path(), btrfs.path());
    std::fs::create_dir_all(&wt).expect("published generation");
    std::os::unix::fs::symlink(&wt, checkout.path().join("target")).expect("published link");
    // Write and search, no read: Cargo can still create entries here, the
    // lease open (O_RDONLY) cannot succeed.
    std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o333)).expect("chmod 333");

    let (ok, out) = run_link_unprivileged(checkout.path(), btrfs.path());

    let _ = std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o755));
    assert!(
        !ok,
        "the recipe replaced target/ under a published generation it could not lease:\n{out}"
    );
    assert!(
        out.contains("refusing to replace target/"),
        "the refusal must say why:\n{out}"
    );
    let target = checkout.path().join("target");
    assert!(
        target.is_symlink() && std::fs::read_link(&target).expect("readlink") == wt,
        "target/ must be left pointing at the published generation\n{out}"
    );
}

/// `--rotate` must not report a clean it did not perform.
///
/// With an UNMANAGED `target/` link and a managed candidate that cannot be
/// written, retaining the link and exiting 0 reports `make clean` as done while
/// the linked directory's payload survives. Both the unusable-volume branch and
/// the fallback refuse instead.
#[test]
fn cargo_target_link_rotate_refuses_to_retain_an_unmanaged_link() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let _wt = unwritable_managed_worktree_dir(checkout.path(), btrfs.path());
    let elsewhere = tempfile::tempdir().expect("unmanaged payload dir");
    let payload = elsewhere.path().join("payload");
    std::fs::write(&payload, b"built artifacts").expect("payload");
    std::os::unix::fs::symlink(elsewhere.path(), checkout.path().join("target"))
        .expect("unmanaged link");

    let (ok, out) = run_link_with(checkout.path(), btrfs.path(), &["--rotate"]);

    assert!(
        !ok,
        "--rotate reported success while retaining an unmanaged target/ link whose \
         payload it cannot rotate away:\n{out}"
    );
    assert!(
        out.contains("refusing unsafe clean"),
        "the refusal must say why:\n{out}"
    );
    assert!(
        payload.exists(),
        "a refused rotation must not delete the payload either"
    );
}

/// A dangling UNMANAGED link is replaced on the fallback path, not tripped
/// over. It is absent to `[ -e target ]` and EEXIST to `mkdir -p target`, which
/// under `set -e` ends the script before the diagnostic that follows.
#[test]
fn cargo_target_link_replaces_a_dangling_unmanaged_link_on_fallback() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let _wt = unwritable_managed_worktree_dir(checkout.path(), btrfs.path());
    std::os::unix::fs::symlink("/nonexistent/elsewhere", checkout.path().join("target"))
        .expect("dangling unmanaged link");

    let (ok, out) = run_link(checkout.path(), btrfs.path());

    assert!(
        ok,
        "the recipe died on a dangling unmanaged target/ link instead of replacing it:\n{out}"
    );
    assert_target_usable(checkout.path(), "dangling unmanaged link replaced");
}

/// A RETIRED generation that is readable but no longer writable rotates to a
/// fresh one. The candidate write probe runs only when the candidate is REUSED:
/// probing a retired one first sends the script to `fallback_to_local`, which
/// drops the managed link and leaves every later run on the root filesystem,
/// while `new_generation` needs only the parent writable. Unprivileged
/// stand-in as elsewhere: root writes anything.
#[test]
fn cargo_target_link_rotates_a_retired_generation_it_cannot_write() {
    use std::os::unix::fs::PermissionsExt as _;
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let wt = managed_worktree_dir(checkout.path(), btrfs.path());
    std::fs::create_dir_all(&wt).expect("retired generation");
    // Mark it retired exactly as retire_target does.
    let marked = Command::new("python3")
        .args([
            "-c",
            "import os, sys; os.setxattr(sys.argv[1], b'user.fcvm.retired', b'v1')",
        ])
        .arg(&wt)
        .status()
        .expect("python3");
    assert!(
        marked.success(),
        "set the retirement xattr on {} (user xattrs must be supported there)",
        wt.display()
    );
    std::os::unix::fs::symlink(&wt, checkout.path().join("target")).expect("published link");
    std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o555)).expect("chmod 555");

    let (ok, out) = run_link_unprivileged(checkout.path(), btrfs.path());

    let _ = std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o755));
    assert!(
        ok,
        "the recipe failed on a retired read-only generation:\n{out}"
    );
    let target = checkout.path().join("target");
    assert!(
        target.is_symlink(),
        "target/ was replaced by a local directory instead of rotating to a fresh \
         managed generation:\n{out}"
    );
    let link = std::fs::read_link(&target).expect("readlink");
    assert!(
        link.to_string_lossy().contains("/cargo-target/") && link != wt,
        "target/ should point at a FRESH managed generation, not {}\n{out}",
        link.display()
    );
    assert_target_usable(checkout.path(), "rotated to a fresh generation");
}

/// Dropping a published managed link waits for the build that holds it.
/// Unlinking `target/` on the unusable-volume path with no lease on the
/// generation it published lets a Cargo wrapper holding that generation SHARED
/// resolve later `target/...` paths into a different tree. The script takes the
/// generation's exclusive lease first: with a shared holder present it blocks,
/// and the link stays.
#[test]
fn cargo_target_link_waits_for_a_shared_holder_before_dropping_a_managed_link() {
    use std::os::unix::io::AsRawFd as _;
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let wt = managed_worktree_dir(checkout.path(), btrfs.path());
    std::fs::create_dir_all(wt.parent().unwrap()).expect("cargo-target parent");
    let generation = wt.with_file_name(format!(
        "{}.generation-held0000",
        wt.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&generation).expect("published generation");
    // $WT_TARGET cannot be created (dangling symlink), so the volume is unusable.
    std::os::unix::fs::symlink(btrfs.path().join("cargo-target/absent"), &wt)
        .expect("dangling $WT_TARGET");
    std::os::unix::fs::symlink(&generation, checkout.path().join("target"))
        .expect("target/ -> published generation");
    // A Cargo wrapper mid-build: the generation is held SHARED for the run.
    let holder = std::fs::File::open(&generation).expect("open generation");
    let held = unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_SH) };
    assert_eq!(held, 0, "take the shared lease");

    let out = Command::new("timeout")
        .arg("4")
        .arg(repo_root().join("scripts/cargo-target-link.sh"))
        .env("BTRFS_ROOT", btrfs.path())
        .env_remove("CARGO_TARGET_LINK_LOCKED")
        .current_dir(checkout.path())
        .output()
        .expect("run under timeout");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    drop(holder);

    assert_eq!(
        out.status.code(),
        Some(124),
        "the script did not wait for the shared holder (expected timeout exit 124):\n{text}"
    );
    let target = checkout.path().join("target");
    assert!(
        target.is_symlink() && std::fs::read_link(&target).expect("readlink") == generation,
        "target/ was dropped while a build still held its generation\n{text}"
    );
}

/// Every exit that leaves target/ as a plain local directory probes it first.
///
/// Behavioural coverage above reaches two of the three exits; this pins all of
/// them by shape, so a fourth exit (or a probe dropped from one) fails here.
#[test]
fn every_local_target_exit_probes_writability_first() {
    let script =
        std::fs::read_to_string(repo_root().join("scripts/cargo-target-link.sh")).expect("script");
    let lines: Vec<&str> = script.lines().collect();
    fn is_code(l: &str) -> bool {
        !l.trim().is_empty() && !l.trim().starts_with('#')
    }
    let mut exits = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "exit 0" {
            continue;
        }
        exits += 1;
        let preceding: Vec<&str> = lines[..i]
            .iter()
            .rev()
            .filter(|l| is_code(l))
            .take(3)
            .copied()
            .collect();
        assert!(
            preceding
                .iter()
                .any(|l| l.contains("require_writable_local_target")),
            "line {}: `exit 0` without a writability probe among the three preceding \
             statements {:?}; a target/ that passes `-d` but cannot be written is a cargo \
             error several steps later",
            i + 1,
            preceding
        );
    }
    assert!(
        exits >= 2,
        "expected the managed-dir fallback and the unmanaged-target retention to each \
         `exit 0`; found {exits}. If the script's shape changed, move this pin with it."
    );
    let last = lines
        .iter()
        .rev()
        .find(|l| is_code(l))
        .map(|l| l.trim())
        .unwrap_or("");
    assert_eq!(
        last, "require_writable_local_target",
        "the script's fall-through end (the else-branch and self-heal paths) must be \
         the probe itself"
    );
}

/// Helper: this checkout's managed worktree dir, exactly as the script names it.
fn managed_worktree_dir(checkout: &Path, btrfs_root: &Path) -> PathBuf {
    let p = std::fs::canonicalize(checkout).expect("canonical checkout path");
    let base: String = p
        .file_name()
        .unwrap()
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let digest = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(p.to_string_lossy().as_bytes()))
    };
    btrfs_root
        .join("cargo-target")
        .join(format!("{base}-{}", &digest[..8]))
}

/// Helper: every fallback payload this checkout holds, published or not,
/// sorted. Each activation names its own, so more than one means an earlier
/// payload was neither published nor reclaimed.
fn local_fallback_payloads(checkout: &Path) -> Vec<PathBuf> {
    let checkout = std::fs::canonicalize(checkout).expect("canonical checkout path");
    let mut payloads: Vec<PathBuf> = std::fs::read_dir(&checkout)
        .unwrap_or_else(|error| panic!("read {checkout:?}: {error}"))
        .map(|entry| entry.expect("checkout entry").path())
        .filter(|entry| {
            entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".cargo-target-local.generation-"))
        })
        .collect();
    payloads.sort();
    payloads
}

/// The script's own fallback: `target` is a link to the checkout-local payload,
/// never a real `target/` dentry, so a later run can replace it once the volume
/// is back. Returns the payload path.
fn assert_local_fallback_link(checkout: &Path, btrfs_root: &Path, ctx: &str) -> PathBuf {
    let target = checkout.join("target");
    let link = std::fs::read_link(&target).unwrap_or_else(|error| {
        panic!(
            "{ctx}: target/ is not a symlink ({error}); a real target/ dentry is retained as \
             unmanaged by every later run, so the checkout would never return to the btrfs \
             cache. symlink_metadata={:?}",
            std::fs::symlink_metadata(&target).map(|m| m.file_type())
        )
    });
    assert!(
        !link.starts_with(btrfs_root),
        "{ctx}: target/ -> {link:?} points into the btrfs root {btrfs_root:?}"
    );
    assert_eq!(
        local_fallback_payloads(checkout),
        vec![link.clone()],
        "{ctx}: target/ must publish the checkout's one fallback payload"
    );
    assert_target_usable(checkout, ctx);
    link
}

/// Make this checkout's managed worktree dir "exist but refuse writes" without
/// any privilege: point it at procfs, where creating an entry is refused for
/// root and non-root alike. `mkdir -p` on it succeeds (it exists), a lease fd
/// opens, and the write probe inside the lease fails -- exactly the
/// ownership-change / read-only-remount state, staged with a symlink.
///
/// A chmod as the invoking user does not stop root, and a read-only bind
/// remount is refused for UID 0 WITHOUT CAP_SYS_ADMIN (an unprivileged
/// container), which fails the test before it reaches the script.
fn unwritable_managed_worktree_dir(checkout: &Path, btrfs_root: &Path) -> PathBuf {
    let wt = managed_worktree_dir(checkout, btrfs_root);
    std::fs::create_dir_all(wt.parent().unwrap()).expect("cargo-target parent");
    std::os::unix::fs::symlink("/proc/self", &wt).expect("procfs-backed worktree dir");
    assert!(
        wt.is_dir(),
        "the procfs stand-in does not resolve to a directory"
    );
    assert!(
        std::fs::File::create(wt.join(".fcvm-fixture-probe")).is_err(),
        "the procfs stand-in accepted a write; the fixture proves nothing"
    );
    wt
}

/// A managed symlink whose directory has become unwritable must be REPLACED.
///
/// With `target` already pointing at the managed directory, noticing that the
/// directory is unwritable and warning is not enough: the link still resolves,
/// so the dangling-link repair is skipped and the final `-d target` check
/// passes, which is exit 0 with a target Cargo cannot create files in. The
/// fallback repoints the existing managed link at the local payload, under the
/// generation lease it already holds for that replacement.
#[test]
fn cargo_target_link_replaces_a_managed_link_to_an_unwritable_dir() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let wt = unwritable_managed_worktree_dir(checkout.path(), btrfs.path());
    std::os::unix::fs::symlink(&wt, checkout.path().join("target")).expect("managed link");

    let (ok, out) = run_link(checkout.path(), btrfs.path());

    assert!(ok, "the recipe failed outright:\n{out}");
    assert_local_fallback_link(
        checkout.path(),
        btrfs.path(),
        "managed link to an unwritable dir",
    );
}

/// The write probe must happen INSIDE the generation lease, never before it.
///
/// `prune-cargo-target.sh` takes LOCK_EX on a generation, takes a census of its
/// entries, then rewalks the locked tree. A probe entry created before the
/// script has leased the generation appears after the census with no record,
/// and one removed during the rewalk raises "target entry disappeared during
/// reclaim", so an ordinary concurrent `make` aborts the hourly preflight and
/// its job.
///
/// The test IS the pruner: it holds LOCK_EX on the worktree directory and runs
/// the script under a timeout. Correct behaviour is to block at the lease
/// without having touched the directory -- so its mtime (which any create or
/// unlink inside it bumps) must be unchanged when the timeout fires.
#[test]
fn cargo_target_link_does_not_touch_a_generation_it_has_not_leased() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let wt = managed_worktree_dir(checkout.path(), btrfs.path());
    std::fs::create_dir_all(&wt).expect("managed dir");

    // Be the pruner: LOCK_EX on the generation directory.
    let dir = std::fs::File::open(&wt).expect("open the generation dir");
    use std::os::unix::io::AsRawFd;
    let locked = unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(
        locked,
        0,
        "could not take the pruner's LOCK_EX on {}",
        wt.display()
    );
    // Settle so that a later create/unlink moves the mtime by a visible amount.
    // (`touch`, not set_file_time: that helper opens for write, which a
    // directory refuses with EISDIR.)
    assert!(
        Command::new("touch")
            .args(["-d", "@1600000000"])
            .arg(&wt)
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "could not settle the directory mtime"
    );
    let before = std::fs::metadata(&wt).expect("stat").mtime();

    let out = Command::new("timeout")
        .arg("4")
        .arg(repo_root().join("scripts/cargo-target-link.sh"))
        .env("BTRFS_ROOT", btrfs.path())
        .env_remove("CARGO_TARGET_LINK_LOCKED")
        .current_dir(checkout.path())
        .output()
        .expect("run the script under timeout");
    let after = std::fs::metadata(&wt).expect("stat").mtime();
    drop(dir); // releases the lock

    assert_eq!(
        out.status.code(),
        Some(124),
        "the script did not block on the pruner's lease; it completed with {:?}\n{}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        before, after,
        "the generation directory was modified (mtime {before} -> {after}) while the \
         pruner held its lease: the write probe ran outside the lock"
    );
}

/// A fallback the script created must not outlive the outage that caused it.
///
/// A REAL target/ directory created while the volume is unavailable sends the
/// next run with the volume back down the pre-protocol branch, which retains
/// any real target/ as unmanaged: the checkout never returns to the btrfs cache
/// and every later build lands on the root filesystem until the disk guard
/// quarantines the runner. The fallback is a link to a checkout-local payload,
/// which the recovered run replaces under the same lease as any other published
/// link, and reclaims once it publishes nothing.
#[test]
fn cargo_target_link_returns_to_btrfs_after_a_volume_outage() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let volume_parent = tempfile::tempdir().expect("volume parent");
    let btrfs = volume_parent.path().join("fcvm-btrfs");
    assert!(!btrfs.exists(), "precondition: the volume is not mounted");

    let (ok, out) = run_link(checkout.path(), &btrfs);
    assert!(ok, "run 1 (volume absent) failed:\n{out}");
    assert_target_usable(checkout.path(), "run 1, volume absent");
    let target = checkout.path().join("target");
    let payload = std::fs::canonicalize(&target).expect("resolve the fallback payload");
    std::fs::write(
        target.join("built-during-outage"),
        b"built while the volume was down",
    )
    .expect("write through target/ during the outage");

    std::fs::create_dir_all(&btrfs).expect("the volume is mounted again");

    let (ok, out) = run_link(checkout.path(), &btrfs);
    assert!(ok, "run 2 (volume back) failed:\n{out}");
    let link = std::fs::read_link(&target).unwrap_or_else(|error| {
        panic!(
            "run 2: target/ is not a symlink ({error}); the fallback the script created \
             during the outage was retained as an unmanaged real target/, so this checkout \
             never returns to the btrfs cache and every later build stays on the root \
             filesystem:\n{out}"
        )
    });
    assert!(
        link.starts_with(btrfs.join("cargo-target")),
        "run 2: target/ -> {link:?} is not under the recovered volume:\n{out}"
    );
    assert_target_usable(checkout.path(), "run 2, volume back");
    assert!(
        !target.join("built-during-outage").exists(),
        "the outage payload is still published through target/"
    );
    assert!(
        local_fallback_payloads(checkout.path()).is_empty(),
        "the outage payload is unreachable and sits on the root filesystem this indirection \
         keeps free; nothing else enumerates it:\n{out}"
    );
    assert!(
        out.contains(payload.to_str().expect("utf-8 payload path")),
        "run 2 must name the payload {payload:?} it reclaimed:\n{out}"
    );
}

/// A real target/ the script did not create is retained, volume or no volume.
/// Its dentry may carry a mount that exists only in another mount namespace,
/// and renaming or removing it could move or detach that mount; only the
/// script's own fallback lives behind a replaceable link.
#[test]
fn cargo_target_link_retains_a_real_target_it_did_not_create() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let volume_parent = tempfile::tempdir().expect("volume parent");
    let btrfs = volume_parent.path().join("fcvm-btrfs");
    let target = checkout.path().join("target");
    std::fs::create_dir(&target).expect("pre-existing real target/");
    let artifact = target.join("pre-protocol-artifact");
    std::fs::write(&artifact, b"not the script's to move").expect("write artifact");

    for (volume_present, ctx) in [(false, "volume absent"), (true, "volume back")] {
        if volume_present {
            std::fs::create_dir_all(&btrfs).expect("mount the volume");
        }
        let (ok, out) = run_link(checkout.path(), &btrfs);
        assert!(ok, "{ctx}: the recipe failed:\n{out}");
        assert!(
            std::fs::symlink_metadata(&target)
                .expect("target/ must exist")
                .file_type()
                .is_dir(),
            "{ctx}: a real target/ the script did not create was replaced by {:?}:\n{out}",
            std::fs::read_link(&target)
        );
        if volume_present {
            assert!(
                out.contains("retaining unmanaged local target/"),
                "{ctx}: retention must be stated:\n{out}"
            );
        }
        assert_eq!(
            std::fs::read(&artifact).expect("read the retained artifact"),
            b"not the script's to move",
            "{ctx}: the retained payload changed"
        );
        assert!(
            local_fallback_payloads(checkout.path()).is_empty(),
            "{ctx}: a fallback payload was created beside a retained real target/:\n{out}"
        );
        assert_target_usable(checkout.path(), ctx);
    }
}

/// `--rotate` keeps refusing while the volume is down: the fallback link, and a
/// fallback payload with no link over it, would both report a clean while the
/// payload survives.
#[test]
fn cargo_target_link_rotate_refuses_the_local_fallback_while_the_volume_is_down() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let volume_parent = tempfile::tempdir().expect("volume parent");
    let btrfs = volume_parent.path().join("fcvm-btrfs");
    let (ok, out) = run_link(checkout.path(), &btrfs);
    assert!(ok, "fallback failed:\n{out}");
    let payload = assert_local_fallback_link(checkout.path(), &btrfs, "volume absent");
    let target = checkout.path().join("target");
    std::fs::write(target.join("artifact"), b"survives").expect("write through target/");

    let (ok, out) = run_link_with(checkout.path(), &btrfs, &["--rotate"]);
    assert!(
        !ok && out.contains("refusing unsafe clean"),
        "--rotate reported a clean it cannot perform on the fallback link:\n{out}"
    );
    assert_eq!(
        std::fs::read_link(&target).expect("readlink"),
        payload,
        "a refused rotation must leave the fallback link in place"
    );

    std::fs::remove_file(&target).expect("unlink the fallback link");
    let (ok, out) = run_link_with(checkout.path(), &btrfs, &["--rotate"]);
    assert!(
        !ok && out.contains("refusing unsafe clean"),
        "--rotate republished a surviving fallback payload as a clean target/:\n{out}"
    );
    assert!(
        std::fs::symlink_metadata(&target).is_err(),
        "a refused rotation must not publish anything"
    );
    assert_eq!(
        std::fs::read(payload.join("artifact")).expect("read the surviving payload"),
        b"survives"
    );
}

/// A clean the volume outlived must not leave a payload a later outage can
/// republish.
///
/// The volume is absent, so a fallback payload is published and built into; the
/// volume returns and the next run publishes a btrfs generation; `--rotate`
/// (the `clean` recipe) reports a clean; the volume goes away again. The
/// fallback published by the last run must be empty. Anything the first outage
/// left in it is cargo cache and build-script output that the clean reported
/// gone.
#[test]
fn cargo_target_link_publishes_a_fresh_fallback_after_a_clean() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let volume_parent = tempfile::tempdir().expect("volume parent");
    let btrfs = volume_parent.path().join("fcvm-btrfs");
    let target = checkout.path().join("target");

    let (ok, out) = run_link(checkout.path(), &btrfs);
    assert!(ok, "run 1 (volume absent) failed:\n{out}");
    let first_payload = std::fs::read_link(&target).expect("run 1 must publish a fallback link");
    std::fs::write(target.join("stale-artifact"), b"built before the clean")
        .expect("write through the fallback link");

    std::fs::create_dir_all(&btrfs).expect("the volume is mounted again");
    let (ok, out) = run_link(checkout.path(), &btrfs);
    assert!(ok, "run 2 (volume back) failed:\n{out}");

    let (ok, out) = run_link_with(checkout.path(), &btrfs, &["--rotate"]);
    assert!(ok, "run 3 (--rotate, the clean recipe) failed:\n{out}");

    std::fs::remove_dir_all(&btrfs).expect("the volume goes away again");
    let (ok, out) = run_link(checkout.path(), &btrfs);
    assert!(ok, "run 4 (volume absent again) failed:\n{out}");
    assert_target_usable(checkout.path(), "run 4, volume absent again");

    let republished = std::fs::read_link(&target).expect("run 4 must publish a fallback link");
    assert!(
        !target.join("stale-artifact").exists(),
        "target/ -> {republished:?} republishes {first_payload:?}, the payload the clean \
         reported gone; every cargo fingerprint and build-script OUT_DIR written before the \
         clean is back:\n{out}"
    );
    assert_ne!(
        republished, first_payload,
        "run 4 reused an earlier outage's payload instead of publishing a fresh one:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Reclaiming a fallback payload, and dropping a link, are destructive acts.
// ---------------------------------------------------------------------------

/// `run_link_with`, but as uid 65534 when the tests are root: root can open and
/// write any directory, so a fixture about permissions must run as someone who
/// cannot. Both tempdirs are handed to that uid, and scripts/ is copied where
/// it can read them (the script sources cargo-target-lib.sh next to itself). A
/// btrfs root that does not exist is the volume-is-gone fixture and is left
/// alone.
fn run_link_unprivileged_with(dir: &Path, btrfs_root: &Path, args: &[&str]) -> (bool, String) {
    if unsafe { libc::geteuid() } != 0 {
        return run_link_with(dir, btrfs_root, args);
    }
    let tools = tempfile::tempdir().expect("tools tempdir");
    std::fs::set_permissions(tools.path(), std::fs::Permissions::from_mode(0o755)).expect("0755");
    let scripts = tools.path().join("scripts");
    std::fs::create_dir(&scripts).expect("scripts dir");
    for name in ["cargo-target-link.sh", "cargo-target-lib.sh"] {
        std::fs::copy(repo_root().join("scripts").join(name), scripts.join(name)).expect(name);
    }
    std::fs::set_permissions(
        scripts.join("cargo-target-link.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("0755");
    for owned in [dir, btrfs_root] {
        if !owned.exists() {
            continue;
        }
        let chowned = Command::new("chown")
            .args(["-R", "65534:65534"])
            .arg(owned)
            .status()
            .expect("chown -R");
        assert!(chowned.success(), "hand {} to uid 65534", owned.display());
    }
    let out = Command::new("setpriv")
        .args(["--reuid=65534", "--regid=65534", "--clear-groups"])
        .arg(scripts.join("cargo-target-link.sh"))
        .args(args)
        .env("BTRFS_ROOT", btrfs_root)
        .env_remove("CARGO_TARGET_LINK_LOCKED")
        .current_dir(dir)
        .output()
        .expect("run scripts/cargo-target-link.sh as uid 65534");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// Stage a checkout that has been through a volume outage: `target/` publishes
/// a checkout-local fallback payload. Returns that payload.
#[cfg(feature = "privileged-tests")]
fn stage_outage_fallback(checkout: &Path, absent_volume: &Path) -> PathBuf {
    let (ok, out) = run_link(checkout, absent_volume);
    assert!(ok, "the outage run failed:\n{out}");
    assert_local_fallback_link(checkout, absent_volume, "volume down")
}

/// Reclaiming an unpublished payload must not cross a mount boundary.
///
/// `rm -rf` descends into whatever is mounted underneath and unlinks it. A
/// payload sits in the checkout for the life of an outage, which is exactly
/// when a bind mount can be placed under it; the data behind that mount belongs
/// to someone else and its dentry may be the only reference another mount
/// namespace has. The reclaim must refuse to descend and say so.
#[cfg(feature = "privileged-tests")]
#[test]
fn cargo_target_link_refuses_to_reclaim_a_payload_across_a_mount_boundary() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let volume_parent = tempfile::tempdir().expect("volume parent");
    let btrfs = volume_parent.path().join("fcvm-btrfs");
    let payload = stage_outage_fallback(checkout.path(), &btrfs);

    let source = tempfile::tempdir().expect("bind source");
    let sentinel = source.path().join("must-survive");
    std::fs::write(&sentinel, b"mounted data").expect("write mounted sentinel");
    let mountpoint = payload.join("mounted-source");
    std::fs::create_dir_all(&mountpoint).expect("mountpoint inside the payload");
    let status = Command::new("mount")
        .args([
            std::ffi::OsStr::new("--bind"),
            source.path().as_os_str(),
            mountpoint.as_os_str(),
        ])
        .status()
        .expect("run bind mount");
    assert!(status.success(), "bind mount failed: {status:?}");
    let mount = BindMountGuard(mountpoint.clone());

    std::fs::create_dir_all(&btrfs).expect("the volume comes back");
    let (ok, out) = run_link(checkout.path(), &btrfs);

    assert!(
        sentinel.exists(),
        "reclaiming {payload:?} crossed the bind mount at {mountpoint:?} and deleted unrelated \
         mounted data:\n{out}"
    );
    assert!(
        !ok,
        "a payload the run could not reclaim was reported as a successful setup; it stays on \
         the root filesystem this indirection exists to keep free:\n{out}"
    );
    assert!(
        out.contains(&payload.display().to_string()),
        "the refusal does not name the payload that was left behind:\n{out}"
    );
    mount.unmount();
}

/// A payload owned by another uid is never reclaimed, and never silently.
///
/// The container lane runs as root against a bind-mounted checkout, so a
/// payload written there is root-owned on the host. Removing another identity's
/// tree is not this script's business, and root can do it without noticing.
/// Refusing is right; refusing without a word is what lets payloads accumulate.
#[cfg(feature = "privileged-tests")]
#[test]
fn cargo_target_link_refuses_to_reclaim_a_payload_owned_by_another_user() {
    assert!(
        nix_geteuid_is_root(),
        "BLOCKED: this fixture needs root to create a payload owned by another uid"
    );
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let volume_parent = tempfile::tempdir().expect("volume parent");
    let btrfs = volume_parent.path().join("fcvm-btrfs");
    let ours = stage_outage_fallback(checkout.path(), &btrfs);

    let foreign = checkout
        .path()
        .join(".cargo-target-local.generation-foreign");
    std::fs::create_dir(&foreign).expect("foreign payload");
    let sentinel = foreign.join("must-survive");
    std::fs::write(&sentinel, b"another identity's build").expect("write foreign sentinel");
    let chowned = Command::new("chown")
        .args(["-R", "65534:65534"])
        .arg(&foreign)
        .status()
        .expect("chown -R");
    assert!(chowned.success(), "hand the foreign payload to uid 65534");

    std::fs::create_dir_all(&btrfs).expect("the volume comes back");
    let (ok, out) = run_link(checkout.path(), &btrfs);

    assert!(
        sentinel.exists(),
        "the reclaim removed a payload owned by uid 65534:\n{out}"
    );
    assert!(
        out.contains(&foreign.display().to_string()),
        "a payload that cannot be reclaimed was passed over without a word, so nothing tells an \
         operator why the root filesystem keeps filling:\n{out}"
    );
    assert!(
        ok,
        "a foreign-owned payload failed the run; a checkout shared by the host user and the \
         container root would then never build again:\n{out}"
    );
    assert!(
        !ours.exists(),
        "our own unpublished payload {ours:?} was left behind:\n{out}"
    );
}

/// Run the script with the reclaim's ownership check and its open of `victim`
/// separated by a rename, so the descriptor the reclaim goes on to prune is
/// never the directory that check approved.
///
/// The interleave is by construction, not by timing: the swap runs inside the
/// `os.stat` call whose result the ownership check reads, so the open that
/// follows can only ever see the replacement. The hook is a `sitecustomize`
/// module on `PYTHONPATH`, which CPython imports before it runs the reclaim, so
/// the script, its walk and its removal are the real ones.
fn run_link_with_payload_swapped_after_its_check(
    dir: &Path,
    btrfs_root: &Path,
    victim: &str,
    replacement: &Path,
) -> (bool, String) {
    let hook_dir = tempfile::tempdir().expect("hook tempdir");
    std::fs::set_permissions(hook_dir.path(), std::fs::Permissions::from_mode(0o755))
        .expect("0755");
    let checkout = dir.to_string_lossy().into_owned();
    let replacement_path = replacement.to_string_lossy().into_owned();
    let program = format!(
        r#"import os

_real_stat = os.stat
_checkout = {checkout:?}
_victim = {victim:?}
_replacement = {replacement_path:?}
_moved_aside = os.path.join(_checkout, "payload-moved-aside")
_marker = os.path.join(_checkout, "swap-performed")


def _stat(path, *args, dir_fd=None, follow_symlinks=True, **kwargs):
    info = _real_stat(path, *args, dir_fd=dir_fd, follow_symlinks=follow_symlinks, **kwargs)
    if dir_fd is not None and path == _victim and not os.path.lexists(_marker):
        os.rename(os.path.join(_checkout, _victim), _moved_aside)
        os.rename(_replacement, os.path.join(_checkout, _victim))
        with open(_marker, "w") as handle:
            handle.write("swapped\n")
    return info


os.stat = _stat
"#
    );
    std::fs::write(hook_dir.path().join("sitecustomize.py"), program).expect("write the hook");
    let out = Command::new(repo_root().join("scripts/cargo-target-link.sh"))
        .env("BTRFS_ROOT", btrfs_root)
        .env("PYTHONPATH", hook_dir.path())
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

/// Assert the swapped-in tree came through the run whole.
fn assert_bystander_survived(swapped: &Path, before: &std::fs::Metadata, out: &str) {
    assert!(
        swapped.is_dir(),
        "the reclaim removed {swapped:?}, the tree renamed onto the payload name after the \
         ownership check approved a different directory:\n{out}"
    );
    let sentinel = swapped.join("must-survive");
    let contents = std::fs::read(&sentinel).unwrap_or_else(|error| {
        panic!(
            "the reclaim pruned {sentinel:?}, so a tree its ownership check never saw was \
             erased: {error}\n{out}"
        )
    });
    assert_eq!(
        contents, b"a tree the reclaim never checked",
        "the reclaim rewrote the contents of a tree its ownership check never saw:\n{out}"
    );
    let after = std::fs::symlink_metadata(swapped).expect("stat the swapped-in tree");
    assert_eq!(
        (after.dev(), after.ino()),
        (before.dev(), before.ino()),
        "the swapped-in tree was removed and something else took its name; the reclaim acted \
         on it under a check made about a different inode:\n{out}"
    );
}

/// A payload's ownership check and the open that follows are two separate
/// resolutions of one pathname. A rename in between hands the reclaim a
/// descriptor for a directory nothing checked, and the comparison further down
/// only proves the pathname still names that replacement, so its tree is pruned
/// and removed on the strength of a check made about a different inode.
#[test]
fn cargo_target_link_refuses_a_payload_whose_identity_changed_before_it_was_opened() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let victim_name = ".cargo-target-local.generation-victim";
    std::fs::create_dir(checkout.path().join(victim_name)).expect("unpublished payload");
    let bystander = checkout.path().join("bystander");
    std::fs::create_dir(&bystander).expect("bystander tree");
    std::fs::write(
        bystander.join("must-survive"),
        b"a tree the reclaim never checked",
    )
    .expect("write the bystander sentinel");
    let before = std::fs::symlink_metadata(&bystander).expect("stat the bystander");

    let (ok, out) = run_link_with_payload_swapped_after_its_check(
        checkout.path(),
        btrfs.path(),
        victim_name,
        &bystander,
    );

    assert!(
        checkout.path().join("swap-performed").is_file(),
        "the swap never happened, so this run says nothing about the window it is about:\n{out}"
    );
    let swapped = checkout.path().join(victim_name);
    assert_bystander_survived(&swapped, &before, &out);
    assert!(
        out.contains(&swapped.display().to_string()),
        "a payload the reclaim refused was passed over without naming it:\n{out}"
    );
    assert!(
        out.contains("changed identity"),
        "the refusal does not say why the payload was left alone:\n{out}"
    );
    assert!(
        !ok,
        "the run reported success while a payload it owns went unreclaimed; nothing else \
         enumerates it and its name is never published again:\n{out}"
    );
}

/// The same window, defeating the guard it exists for. A payload owned by
/// another uid is never removed, and the container lane runs as root against
/// the same bind-mounted checkout, so root is not stopped by permissions once
/// the check has been made about a different inode.
#[cfg(feature = "privileged-tests")]
#[test]
fn cargo_target_link_refuses_a_payload_swapped_for_another_users_tree() {
    assert!(
        nix_geteuid_is_root(),
        "BLOCKED: this fixture needs root to stage a tree owned by another uid"
    );
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let victim_name = ".cargo-target-local.generation-victim";
    std::fs::create_dir(checkout.path().join(victim_name)).expect("unpublished payload");
    let foreign = checkout.path().join("another-identitys-tree");
    std::fs::create_dir(&foreign).expect("foreign tree");
    std::fs::write(
        foreign.join("must-survive"),
        b"a tree the reclaim never checked",
    )
    .expect("write the foreign sentinel");
    let chowned = Command::new("chown")
        .args(["-R", "65534:65534"])
        .arg(&foreign)
        .status()
        .expect("chown -R");
    assert!(chowned.success(), "hand the foreign tree to uid 65534");
    let before = std::fs::symlink_metadata(&foreign).expect("stat the foreign tree");

    let (ok, out) = run_link_with_payload_swapped_after_its_check(
        checkout.path(),
        btrfs.path(),
        victim_name,
        &foreign,
    );

    assert!(
        checkout.path().join("swap-performed").is_file(),
        "the swap never happened, so this run says nothing about the window it is about:\n{out}"
    );
    let swapped = checkout.path().join(victim_name);
    assert_bystander_survived(&swapped, &before, &out);
    assert!(
        out.contains(&swapped.display().to_string()) && out.contains("65534"),
        "the run erased or passed over another identity's tree without naming it and the uid \
         that owns it:\n{out}"
    );
    assert!(
        !ok,
        "the run reported success while a payload it owns went unreclaimed; nothing else \
         enumerates it and its name is never published again:\n{out}"
    );
}

/// A payload this identity owns and cannot reclaim fails the run.
///
/// It is on the root filesystem, nothing else enumerates it, and its name is
/// never published again, so no later run reclaims it either. Passing over it
/// turns one directory into one tree per outage cycle.
#[test]
fn cargo_target_link_fails_closed_on_a_payload_it_cannot_reclaim() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let volume_parent = tempfile::tempdir().expect("volume parent");
    let btrfs = volume_parent.path().join("fcvm-btrfs");
    let (ok, out) = run_link_unprivileged(checkout.path(), &btrfs);
    assert!(ok, "the outage run failed:\n{out}");

    let stuck = checkout.path().join(".cargo-target-local.generation-stuck");
    std::fs::create_dir(&stuck).expect("second payload");
    std::fs::write(stuck.join("artifact"), b"unreachable").expect("write into it");
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");

    std::fs::create_dir_all(&btrfs).expect("the volume comes back");
    let (ok, out) = run_link_unprivileged(checkout.path(), &btrfs);

    let restored = std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755));
    assert!(
        out.contains(&stuck.display().to_string()),
        "the run passed over a payload it could not open without naming it:\n{out}"
    );
    assert!(
        !ok,
        "the run reported success while leaving a payload it owns and cannot reclaim on the \
         root filesystem:\n{out}"
    );
    restored.expect("restore mode for cleanup");
}

/// `[ -d target ]` is false both when the link dangles and when resolving it is
/// refused. Only the first proves nothing is published there. A build past the
/// checkout lock holds nothing but the generation's shared lease, so replacing
/// `target/` without taking that lease splits it across two trees; a resolution
/// error must fail closed instead.
#[test]
fn cargo_target_link_refuses_to_drop_a_link_it_cannot_resolve() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let cargo_target = btrfs.path().join("cargo-target");
    let blocked = cargo_target.join("blocked");
    let generation = blocked.join("held-generation");
    std::fs::create_dir_all(&generation).expect("published generation");
    std::fs::write(generation.join("artifact"), b"a running build's output").expect("artifact");
    let target = checkout.path().join("target");
    std::os::unix::fs::symlink(&generation, &target).expect("managed link");

    if unsafe { libc::geteuid() } == 0 {
        let chowned = Command::new("chown")
            .args(["-R", "65534:65534"])
            .arg(checkout.path())
            .arg(btrfs.path())
            .status()
            .expect("chown -R");
        assert!(chowned.success(), "hand the fixture to uid 65534");
    }
    // Search denied on an ancestor: the destination exists, resolving it does
    // not. 0555 on cargo-target also makes `mkdir -p $WT_TARGET` fail, which is
    // what sends the run down the unusable-volume path.
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    std::fs::set_permissions(&cargo_target, std::fs::Permissions::from_mode(0o555))
        .expect("chmod 555");

    let (ok, out) = run_link_unprivileged(checkout.path(), btrfs.path());

    let restored = (
        std::fs::set_permissions(&cargo_target, std::fs::Permissions::from_mode(0o755)),
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)),
    );
    let still_linked = std::fs::read_link(&target);
    assert_eq!(
        still_linked.as_deref().ok(),
        Some(generation.as_path()),
        "target/ was repointed away from a generation whose lease could not be taken, so a \
         build still resolving through it now writes into a different tree:\n{out}"
    );
    assert!(
        !ok,
        "a resolution failure was treated as a dangling link and the run reported success:\n{out}"
    );
    restored.0.expect("restore cargo-target mode");
    restored.1.expect("restore blocked mode");
}

/// `--rotate` promises a clean namespace. A candidate that cannot be written
/// cannot be retired, so the fallback must refuse rather than report a clean
/// while the generation keeps every byte the clean said was gone: the next run
/// finds it unretired and republishes it.
#[test]
fn cargo_target_link_rotate_refuses_an_unretired_generation_it_cannot_write() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let worktree_dir = managed_worktree_dir(checkout.path(), btrfs.path());
    std::fs::create_dir_all(&worktree_dir).expect("managed worktree dir");
    let artifact = worktree_dir.join("cargo-cache-entry");
    std::fs::write(&artifact, b"payload the clean claims to remove").expect("artifact");
    if unsafe { libc::geteuid() } == 0 {
        let chowned = Command::new("chown")
            .args(["-R", "65534:65534"])
            .arg(checkout.path())
            .arg(btrfs.path())
            .status()
            .expect("chown -R");
        assert!(chowned.success(), "hand the fixture to uid 65534");
    }
    // Readable and searchable, not writable: the lease opens, the retirement
    // marker cannot be written, so the candidate cannot be retired.
    std::fs::set_permissions(&worktree_dir, std::fs::Permissions::from_mode(0o555))
        .expect("chmod 555");

    let (ok, out) = run_link_unprivileged_with(checkout.path(), btrfs.path(), &["--rotate"]);

    let restored = std::fs::set_permissions(&worktree_dir, std::fs::Permissions::from_mode(0o755));
    assert!(
        artifact.exists(),
        "control failed: the fixture's payload disappeared, so the assertion below proves \
         nothing:\n{out}"
    );
    assert!(
        !ok,
        "--rotate reported a clean target while {worktree_dir:?} kept its payload unretired; \
         the next run reuses it and republishes everything the clean said was gone:\n{out}"
    );
    restored.expect("restore worktree dir mode");
}

// ---------------------------------------------------------------------------
// One lease per RECIPE, not per Cargo process.
// ---------------------------------------------------------------------------

/// Mark a generation retired exactly as `retire_target` does.
fn retire_generation(generation: &Path) {
    let marked = Command::new("/usr/bin/python3")
        .args([
            "-c",
            "import os, sys; os.setxattr(sys.argv[1], b'user.fcvm.retired', b'v1')",
        ])
        .arg(generation)
        .status()
        .expect("python3");
    assert!(
        marked.success(),
        "set the retirement xattr on {} (user xattrs must be supported there)",
        generation.display()
    );
}

/// Is `path`'s flock free for the given mode? `flock -n` exits 42 on contention.
fn lease_is_available(path: &Path, mode: &str) -> bool {
    let status = Command::new("flock")
        .args([mode, "-n", "-E", "42"])
        .arg(path)
        .arg("/bin/true")
        .status()
        .expect("probe the generation lease");
    match status.code() {
        Some(0) => true,
        Some(42) => false,
        other => panic!("lease probe on {path:?} failed for another reason: {other:?}"),
    }
}

/// Run one recipe line through the wrapper exactly as make would (`SHELL -c
/// '<line>'`) and hold it there. The line reports through a FIFO rather than
/// stdout, which also carries the rotation notice from the link script.
fn spawn_leased_recipe_line(checkout: &Path, btrfs_root: &Path, ready: &Path) -> ChildGuard {
    ChildGuard(Some(
        Command::new(repo_root().join("scripts/cargo-target-run.sh"))
            .args(["-c", "printf R >\"$READY\"; IFS= read -r _"])
            .env("BTRFS_ROOT", btrfs_root)
            .env("CARGO_TARGET_DIR", "target")
            .env("READY", ready)
            .current_dir(checkout)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("run a recipe line through the target lease wrapper"),
    ))
}

/// The wrapper has to be usable as a recipe's `SHELL`, because that is the only
/// way one lease covers a whole recipe line: make runs `$(SHELL) -c '<line>'`.
/// The lease it holds must be SHARED, or every concurrent reader of the same
/// generation stalls for the length of the recipe.
#[test]
fn cargo_target_run_wrapper_leases_a_recipe_line_shared() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let (ok, out) = run_link(checkout.path(), btrfs.path());
    assert!(ok, "publishing the initial generation failed:\n{out}");
    let generation =
        std::fs::read_link(checkout.path().join("target")).expect("published generation");

    let signals = tempfile::tempdir().expect("signal directory");
    let ready = open_fifo(&signals.path().join("recipe-line-ready"));
    let mut child = spawn_leased_recipe_line(
        checkout.path(),
        btrfs.path(),
        &signals.path().join("recipe-line-ready"),
    );
    assert_eq!(
        read_marker_with_timeout(ready, "the recipe line the wrapper was given"),
        b'R',
        "the wrapper did not run the recipe line make hands to a SHELL"
    );
    assert!(
        !lease_is_available(&generation, "-x"),
        "no lease is held while the recipe line runs, so a concurrent make can republish \
         target/ between one command in the recipe and the next"
    );
    assert!(
        lease_is_available(&generation, "-s"),
        "the recipe line holds the generation EXCLUSIVELY; every other reader of the same \
         generation stalls until the line ends"
    );

    child
        .child_mut()
        .stdin
        .take()
        .expect("wrapper stdin")
        .write_all(b"release\n")
        .expect("release the recipe line");
    let status = child.wait().expect("reap the wrapper");
    assert!(status.success(), "the wrapper failed: {status:?}");
}

/// A recipe line that arrives on a retired generation rotates first and then
/// holds the FRESH one, still shared. Handing the line an exclusive lease, or
/// none at all, are the two ways the rotation path breaks the contract.
#[test]
fn cargo_target_run_wrapper_leases_a_rotated_generation_shared() {
    let checkout = tempfile::tempdir().expect("checkout tempdir");
    let btrfs = tempfile::tempdir().expect("btrfs stand-in");
    let (ok, out) = run_link(checkout.path(), btrfs.path());
    assert!(ok, "publishing the initial generation failed:\n{out}");
    let retired = std::fs::read_link(checkout.path().join("target")).expect("published generation");
    retire_generation(&retired);

    let signals = tempfile::tempdir().expect("signal directory");
    let ready = open_fifo(&signals.path().join("recipe-line-ready"));
    let mut child = spawn_leased_recipe_line(
        checkout.path(),
        btrfs.path(),
        &signals.path().join("recipe-line-ready"),
    );
    assert_eq!(
        read_marker_with_timeout(ready, "the recipe line after a rotation"),
        b'R',
        "the wrapper did not run the recipe line after rotating a retired generation"
    );
    let fresh =
        std::fs::read_link(checkout.path().join("target")).expect("read the rotated target link");
    assert_ne!(
        fresh, retired,
        "the recipe line ran against the retired generation the pruner is about to empty"
    );
    assert!(
        !lease_is_available(&fresh, "-x"),
        "the rotated generation is not leased, so the rest of the recipe can be republished \
         out from under it"
    );
    assert!(
        lease_is_available(&fresh, "-s"),
        "the rotation left the recipe line holding the fresh generation exclusively"
    );

    child
        .child_mut()
        .stdin
        .take()
        .expect("wrapper stdin")
        .write_all(b"release\n")
        .expect("release the recipe line");
    let status = child.wait().expect("reap the wrapper");
    assert!(status.success(), "the wrapper failed: {status:?}");
}

/// Logical recipe lines per target (backslash continuations joined), plus the
/// targets whose recipe runs under the lease wrapper.
fn makefile_recipes(makefile: &str) -> (Vec<(String, String)>, HashSet<String>) {
    let mut recipes: Vec<(String, String)> = Vec::new();
    let mut leased: HashSet<String> = HashSet::new();
    let mut current: Vec<String> = Vec::new();
    let mut pending = String::new();
    for line in makefile.lines() {
        if let Some(rest) = line.strip_prefix('\t') {
            pending.push_str(rest);
            if rest.trim_end().ends_with('\\') {
                pending.push('\n');
                continue;
            }
            for target in &current {
                recipes.push((target.clone(), pending.clone()));
            }
            pending.clear();
            continue;
        }
        if !pending.is_empty() {
            for target in &current {
                recipes.push((target.clone(), pending.clone()));
            }
            pending.clear();
        }
        let Some((names, rest)) = line.split_once(':') else {
            current.clear();
            continue;
        };
        if rest.starts_with('=') || names.starts_with(['#', ' ', '\t', '.']) {
            current.clear();
            continue;
        }
        let targets: Vec<String> = names.split_whitespace().map(str::to_owned).collect();
        if rest.trim_start().starts_with("private SHELL") {
            leased.extend(targets);
            current.clear();
            continue;
        }
        current = targets;
    }
    (recipes, leased)
}

/// Does this recipe command read or write through the `target/` symlink itself?
///
/// `CARGO_TARGET_DIR=target` names no path component, and `cargo-target-run.sh`
/// is a script name, so only a `target/` preceded by a separator counts.
/// A recipe line that runs one command through the lease wrapper itself
/// (`$(TARGET_LEASE_SHELL) <cmd>`): the wrapper holds the generation lease for
/// that command, so the line is leased without the target being. This is the
/// form for a target whose recipe cannot run under the wrapper as a whole
/// because it runs `make` (through a script), whose own cargo-target-link
/// prerequisite would then wait forever on the lease the recipe holds.
fn leased_per_command(command: &str) -> bool {
    let body = command
        .trim_start()
        .trim_start_matches(['@', '-', '+'])
        .trim_start();
    body.starts_with("$(TARGET_LEASE_SHELL) ")
}

fn touches_raw_target(command: &str) -> bool {
    let body = command
        .trim_start()
        .trim_start_matches(['@', '-', '+'])
        .trim_start();
    if body.starts_with('#') {
        return false;
    }
    let bytes = body.as_bytes();
    body.match_indices("target/").any(|(at, _)| match at {
        0 => true,
        _ => matches!(bytes[at - 1], b' ' | b'\t' | b'/' | b'"' | b'\'' | b'='),
    })
}

/// Every recipe line that reaches through `target/` outside Cargo must hold the
/// generation lease while it runs.
///
/// `cargo-target-run.sh` leases only for the length of one Cargo process, so a
/// concurrent `make` republishes `target/` in the gaps. `build` writes
/// `fc-agent` through the link, copies it back through the link, and then runs
/// the binary it just produced; a repoint in between sends the read into a
/// generation the write never reached, and the last step is masked by
/// `|| true`, so the recipe can report success with no `target/release/fcvm`.
#[test]
fn makefile_leases_every_raw_target_access() {
    let makefile = std::fs::read_to_string(repo_root().join("Makefile")).expect("read Makefile");
    let (recipes, leased) = makefile_recipes(&makefile);
    let corpus_extra = std::fs::read_to_string(repo_root().join("bench/chromium/corpus_extra.sh"))
        .expect("read corpus_extra.sh");

    // Positive control: the parser must find recipe lines that are known to be
    // there, or its emptiness proves nothing.
    assert!(
        recipes
            .iter()
            .any(|(target, command)| target == "build" && command.contains("-p fcvm")),
        "the Makefile parser found no `build` recipe; it is not reading the rules it thinks it is"
    );
    assert!(
        touches_raw_target("\tcp target/$(MUSL_TARGET)/release/fc-agent target/release/fc-agent")
            && touches_raw_target("\t@./target/release/fcvm setup")
            && !touches_raw_target("\t@# Symlink ~/.cargo and target/ to btrfs")
            && !touches_raw_target("\tCARGO_TARGET_DIR=target $(CARGO) build")
            && !touches_raw_target("\t@\"$(MAKEFILE_DIR)scripts/cargo-target-run.sh\" cargo build"),
        "the raw-target detector does not classify the known cases correctly"
    );
    assert!(
        leased_per_command(
            "\t@$(TARGET_LEASE_SHELL) sha256sum \"$(CURDIR)/target/release/fcvm\" >> m"
        ) && !leased_per_command("\t@sha256sum \"$(CURDIR)/target/release/fcvm\" >> m")
            && !leased_per_command("\t@echo $(TARGET_LEASE_SHELL) sha256sum target/release/fcvm"),
        "the per-command lease detector does not classify the known cases correctly"
    );

    let unleased: Vec<String> = recipes
        .iter()
        .filter(|(target, command)| {
            touches_raw_target(command) && !leased.contains(target) && !leased_per_command(command)
        })
        .map(|(target, command)| format!("{target}: {}", command.lines().next().unwrap_or("")))
        .collect();
    assert!(
        unleased.is_empty(),
        "these recipe lines reach through target/ without holding the generation lease, so a \
         concurrent make can repoint the link underneath them: {unleased:#?}"
    );
    assert!(
        corpus_extra.contains("$REPO/target/release/fcvm"),
        "corpus_extra.sh no longer reads the built fcvm through target/; re-audit its lease"
    );
    assert!(
        leased.contains("bench-chromium-corpus-extra"),
        "bench-chromium-corpus-extra invokes a script that reads target/release/fcvm, so the \
         complete recipe must hold the target-generation lease"
    );
    // The corpus campaign is the other way round: its orchestrator runs `make -C`
    // once per phase, and a sub-make's cargo-target-link prerequisite takes the
    // generation exclusively, so a recipe holding it shared deadlocks against its
    // own child (observed 2026-09-02, `flock -x` in locks_lock_inode_wait on an
    // idle box). Its one direct target/ read is leased per command instead, and
    // the binary the orchestrator reads is sealed by reqbench.sh into a runtime
    // bundle that every later phase checks by hash.
    assert!(
        !leased.contains("bench-chromium-corpus"),
        "bench-chromium-corpus runs under the lease wrapper as a whole; its orchestrator runs \
         make, whose cargo-target-link prerequisite then waits forever on this recipe's lease"
    );
    assert!(
        recipes.iter().any(|(target, command)| {
            target == "bench-chromium-corpus"
                && touches_raw_target(command)
                && leased_per_command(command)
        }),
        "the bench-chromium-corpus recipe no longer leases its target/release/fcvm read per \
         command; re-audit how MANIFEST.sha256 gets the fcvm hash"
    );

    assert!(
        makefile.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("TARGET_LEASE_SHELL")
                && trimmed.contains("scripts/cargo-target-run.sh")
        }),
        "TARGET_LEASE_SHELL must be scripts/cargo-target-run.sh: it is the script that already \
         implements the lease, and the one a scratch checkout that runs any cargo-bearing target \
         already stages, so no recipe can die with `Error 127` for a shell it cannot resolve"
    );
    for line in makefile.lines() {
        if let Some((_, rest)) = line.split_once(':') {
            if rest.trim_start().starts_with("private SHELL") {
                assert!(
                    rest.contains("$(TARGET_LEASE_SHELL)"),
                    "a target-specific SHELL names something other than TARGET_LEASE_SHELL: {line}"
                );
            }
        }
    }
    let global_shell: Vec<&str> = makefile
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("SHELL=")
                || trimmed.starts_with("SHELL =")
                || trimmed.starts_with("SHELL :=")
                || trimmed.starts_with("SHELL::=")
        })
        .filter(|line| {
            let value = line.split_once('=').map(|(_, v)| v.trim()).unwrap_or("");
            !matches!(value, "/bin/bash" | "/bin/sh")
        })
        .collect();
    assert!(
        global_shell.is_empty(),
        "the Makefile-wide SHELL is something other than a plain shell. A wrapper there runs \
         for every recipe line and every parse-time $(shell ...), including cargo-target-link \
         itself, whose script then blocks forever on the generation lease the wrapper is \
         already holding; and every scratch checkout that stages a partial scripts/ dies with \
         `Error 127`: {global_shell:#?}"
    );

    let build_body = recipes
        .iter()
        .find(|(target, command)| target == "build" && command.contains("-p fc-agent"))
        .map(|(_, command)| command.clone())
        .expect("`build` has no recipe line that builds fc-agent");
    for needed in [
        "cp target/$(MUSL_TARGET)/release/fc-agent",
        "./target/release/fcvm setup",
    ] {
        assert!(
            build_body.contains(needed),
            "`{needed}` is not in the same recipe line as the cargo commands that produce what \
             it reads. One recipe line is one shell and one lease; a separate line takes a new \
             lease, and the link can be repointed in between:\n{build_body}"
        );
    }
}

/// The identity `make` must run as when this binary is root: the Makefile
/// refuses root on the host, so the privileged lane hands its children back to
/// the user sudo recorded. Same constraint as tests/test_dep_provenance.rs.
fn make_child_identity() -> Option<(u32, u32)> {
    if unsafe { libc::geteuid() } != 0
        || Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
    {
        return None;
    }
    let parse = |key: &str| -> Option<u32> { std::env::var(key).ok()?.parse().ok() };
    match (parse("SUDO_UID"), parse("SUDO_GID")) {
        (Some(uid), Some(gid)) => Some((uid, gid)),
        _ => panic!(
            "BLOCKED: running as root on a host with no SUDO_UID/SUDO_GID; the Makefile refuses \
             root and this test has no user to hand make to"
        ),
    }
}

fn hand_tree_to_make_child(root: &Path) {
    let Some((uid, gid)) = make_child_identity() else {
        return;
    };
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        std::os::unix::fs::chown(&path, Some(uid), Some(gid))
            .unwrap_or_else(|error| panic!("chown {}: {error}", path.display()));
        if path.is_dir() && !path.is_symlink() {
            for entry in std::fs::read_dir(&path).expect("read scratch dir") {
                stack.push(entry.expect("scratch entry").path());
            }
        }
    }
}

fn scratch_make(fcvm_dir: &Path, btrfs_root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("make");
    command
        .arg("-C")
        .arg(fcvm_dir)
        .args(args)
        .env("BTRFS_ROOT", btrfs_root)
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS")
        .env_remove("CARGO_TARGET_LINK_LOCKED");
    if let Some((uid, gid)) = make_child_identity() {
        use std::os::unix::process::CommandExt;
        command.uid(uid).gid(gid);
    }
    command
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod 755");
}

fn open_fifo(path: &Path) -> std::fs::File {
    let status = Command::new("mkfifo").arg(path).status().expect("mkfifo");
    assert!(status.success(), "mkfifo {path:?} failed: {status:?}");
    // Read+write so neither end blocks on open and a write outlives its reader.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|error| panic!("open {path:?}: {error}"))
}

/// `build` produces `fc-agent` through the link, copies it back through the
/// link, and runs the binary it just built. One shared lease has to cover all
/// of that: `cargo-target-run.sh` releases when each Cargo process exits, and a
/// concurrent `make` waiting to republish `target/` is granted the exclusive
/// lease in that gap. The copy then reads a generation the build never wrote,
/// and `2>/dev/null || true` on the last step turns the missing binary into a
/// successful `make build`.
///
/// The lease is observed, not inferred: a helper blocks on the exclusive lease
/// from the moment the first Cargo process is about to exit, and must still be
/// blocked when the recipe reaches its last raw `target/` access. The final
/// assertion that it acquires once `make` returns is the control -- without it
/// a helper that never ran would pass.
#[test]
fn make_build_holds_one_generation_lease_across_the_whole_recipe() {
    assert!(
        Command::new("make").arg("--version").output().is_ok(),
        "BLOCKED: `make` is not runnable, so this test cannot evaluate the recipe it guards"
    );
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o755))
        .expect("chmod 755");
    let fcvm_dir = scratch.path().join("fcvm");
    let btrfs = scratch.path().join("btrfs");
    let signals = scratch.path().join("signals");
    let tools = scratch.path().join("tools");
    for directory in [&fcvm_dir.join("scripts"), &btrfs, &signals, &tools] {
        std::fs::create_dir_all(directory).expect("scratch layout");
    }
    std::fs::copy(repo_root().join("Makefile"), fcvm_dir.join("Makefile")).expect("Makefile");
    for name in [
        "cargo-target-link.sh",
        "cargo-target-run.sh",
        "cargo-target-lib.sh",
    ] {
        let destination = fcvm_dir.join("scripts").join(name);
        std::fs::copy(repo_root().join("scripts").join(name), &destination).expect(name);
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 755");
    }

    let cargo_ready_path = signals.join("cargo-ready");
    let cargo_release_path = signals.join("cargo-release");
    let fcvm_ready_path = signals.join("fcvm-ready");
    let fcvm_release_path = signals.join("fcvm-release");
    let cargo_ready = open_fifo(&cargo_ready_path);
    let mut cargo_release = open_fifo(&cargo_release_path);
    let fcvm_ready = open_fifo(&fcvm_ready_path);
    let mut fcvm_release = open_fifo(&fcvm_release_path);
    let marker = signals.join("exclusive-lease-was-granted");

    let stub_fcvm = tools.join("fcvm-stub");
    write_script(
        &stub_fcvm,
        "#!/usr/bin/env bash\n\
         printf F >\"$FCVM_READY\"\n\
         IFS= read -r _ <\"$FCVM_RELEASE\"\n",
    );
    let stub_cargo = tools.join("cargo-stub");
    write_script(
        &stub_cargo,
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         dir=\"${CARGO_TARGET_DIR:-target}\"\n\
         package=\"\"; triple=\"\"; previous=\"\"\n\
         for argument in \"$@\"; do\n\
           case \"$previous\" in\n\
             -p) package=\"$argument\" ;;\n\
             --target) triple=\"$argument\" ;;\n\
           esac\n\
           previous=\"$argument\"\n\
         done\n\
         out=\"$dir/release\"\n\
         [ -z \"$triple\" ] || out=\"$dir/$triple/release\"\n\
         mkdir -p \"$out\"\n\
         if [ \"$package\" = fcvm ]; then\n\
           cp \"$STUB_FCVM\" \"$out/fcvm\"\n\
           chmod 755 \"$out/fcvm\"\n\
           printf C >\"$CARGO_READY\"\n\
           IFS= read -r _ <\"$CARGO_RELEASE\"\n\
         else\n\
           printf agent >\"$out/$package\"\n\
         fi\n",
    );

    hand_tree_to_make_child(scratch.path());
    let published = scratch_make(&fcvm_dir, &btrfs, &["cargo-target-link"])
        .output()
        .expect("publish the generation");
    assert!(
        published.status.success(),
        "make cargo-target-link failed in the scratch checkout:\n{}{}",
        String::from_utf8_lossy(&published.stdout),
        String::from_utf8_lossy(&published.stderr)
    );
    let generation =
        std::fs::read_link(fcvm_dir.join("target")).expect("the scratch checkout has no link");

    hand_tree_to_make_child(scratch.path());
    let build_log = scratch.path().join("make-build.log");
    let build = ChildGuard(Some(
        scratch_make(&fcvm_dir, &btrfs, &["build"])
            .env("CARGO_BIN", &stub_cargo)
            .env("STUB_FCVM", &stub_fcvm)
            .env("CARGO_READY", &cargo_ready_path)
            .env("CARGO_RELEASE", &cargo_release_path)
            .env("FCVM_READY", &fcvm_ready_path)
            .env("FCVM_RELEASE", &fcvm_release_path)
            .stdout(std::fs::File::create(&build_log).expect("build log"))
            .stderr(
                std::fs::File::options()
                    .append(true)
                    .open(&build_log)
                    .expect("build log"),
            )
            .spawn()
            .expect("spawn make build"),
    ));
    let log = || std::fs::read_to_string(&build_log).unwrap_or_default();

    assert_eq!(
        read_marker_with_timeout(cargo_ready, "the first Cargo process"),
        b'C',
        "make build never reached its first cargo command:\n{}",
        log()
    );
    let mut waiter = ChildGuard(Some(
        Command::new("/bin/bash")
            .args([
                "-c",
                "exec {fd}<\"$1\"; printf W; flock -x \"$fd\"; : >\"$2\"",
                "lease-waiter",
            ])
            .arg(&generation)
            .arg(&marker)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the exclusive-lease waiter"),
    ));
    let waiter_stdout = waiter.child_mut().stdout.take().expect("waiter stdout");
    assert_eq!(
        read_marker_with_timeout(waiter_stdout, "the exclusive-lease waiter"),
        b'W',
        "the waiter never reached its blocking flock, so it proves nothing"
    );

    cargo_release
        .write_all(b"go\n")
        .expect("release the first cargo command");
    assert_eq!(
        read_marker_with_timeout(fcvm_ready, "the binary the recipe just built"),
        b'F',
        "make build never ran the binary its own recipe produced:\n{}",
        log()
    );
    assert!(
        !marker.exists(),
        "the generation lease was released between the recipe's cargo commands and its last \
         raw target/ access, so a concurrent make can repoint target/ in that window and the \
         recipe reads a tree its own build never wrote:\n{}",
        log()
    );
    fcvm_release
        .write_all(b"go\n")
        .expect("release the recipe's last step");

    let status = build.wait().expect("reap make build");
    assert!(status.success(), "make build failed:\n{}", log());
    assert_eq!(
        std::fs::read(generation.join("release/fc-agent"))
            .ok()
            .as_deref(),
        Some(b"agent".as_slice()),
        "the recipe did not leave fc-agent in the generation it held:\n{}",
        log()
    );
    // Control: the waiter was genuinely blocked on the lease, not broken.
    wait_for_path(&marker);
    let status = waiter.wait().expect("reap the lease waiter");
    assert!(status.success(), "the lease waiter failed: {status:?}");
}

/// The lease wrapper must not become a requirement of recipes that scratch
/// checkouts run.
///
/// tests/test_dep_provenance.rs stages a checkout holding the Makefile and at
/// most two scripts. A Makefile-wide `SHELL` pointed at a wrapper script kills
/// every recipe line in such a tree with `Error 127`, including targets that
/// never touch `target/`. This is the same staging, run from this binary, so a
/// filtered run of these tests cannot miss it.
#[test]
fn scratch_checkouts_still_run_the_targets_that_stage_no_scripts() {
    assert!(
        Command::new("make").arg("--version").output().is_ok(),
        "BLOCKED: `make` is not runnable, so this test cannot evaluate the recipes it guards"
    );
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o755))
        .expect("chmod 755");
    let fcvm_dir = scratch.path().join("fcvm");
    let btrfs = scratch.path().join("btrfs");
    std::fs::create_dir_all(fcvm_dir.join("scripts")).expect("scratch layout");
    std::fs::create_dir_all(&btrfs).expect("btrfs stand-in");
    std::fs::copy(repo_root().join("Makefile"), fcvm_dir.join("Makefile")).expect("Makefile");
    hand_tree_to_make_child(scratch.path());

    // Exactly what dep_provenance_reports_describe_lock_source_and_missing
    // stages: the Makefile and nothing else.
    let out = scratch_make(&fcvm_dir, &btrfs, &["dep-provenance"])
        .output()
        .expect("run make dep-provenance");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && text.contains("fuse-backend-rs: "),
        "make dep-provenance needs a script this checkout does not stage:\n{text}"
    );

    // And what mid_build_dependency_change_fails_the_build stages: two stubs,
    // one of which stands in for every cargo command.
    write_script(
        &fcvm_dir.join("scripts/cargo-target-link.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    );
    write_script(
        &fcvm_dir.join("scripts/cargo-target-run.sh"),
        "#!/usr/bin/env bash\necho \"stub cargo: $*\"\n",
    );
    hand_tree_to_make_child(scratch.path());
    let out = scratch_make(&fcvm_dir, &btrfs, &["build-host-tools"])
        .output()
        .expect("run make build-host-tools");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && text.contains("stub cargo: "),
        "make build-host-tools needs a script this checkout does not stage:\n{text}"
    );
}
