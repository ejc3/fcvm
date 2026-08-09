//! `make cargo-target-link` must leave `target/` resolving to a real directory.
//!
//! Every build and test recipe runs cargo with `CARGO_TARGET_DIR=target`, where
//! `target` is a symlink onto btrfs (the root filesystem is small and a link step
//! dies with "No space left on device" mid-build). The symlink is created once and
//! then trusted forever — but `/mnt/fcvm-btrfs` on the CI runners is EPHEMERAL and
//! gets reset out from under a checkout that persists across jobs. The link is
//! then dangling, and cargo does not create the directory through it: it builds a
//! tempdir and `rename()`s it onto the path, which fails on an existing symlink
//! with ENOTDIR.
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

use std::fs::{FileTimes, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
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
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(946_684_800);
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path:?} to age it: {e}"));
    file.set_times(FileTimes::new().set_accessed(old).set_modified(old))
        .unwrap_or_else(|e| panic!("age {path:?}: {e}"));
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
/// directory lease, while its idle sibling is emptied in the same preflight.
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
        !idle_payload.exists(),
        "idle sibling was not reclaimed independently:\n{out}"
    );
    assert!(
        active_target.is_dir() && idle_target.is_dir(),
        "pruner removed a leased directory inode and dangled a worktree symlink"
    );
    assert_target_usable(idle_work.path(), "after idle target pruning");

    std::fs::write(&release, b"release").unwrap();
    let status = build.wait().expect("wait for fake Cargo process");
    assert!(status.success(), "fake Cargo process failed: {status}");
    assert_eq!(
        std::fs::read(active_target.join("concurrent-build-output")).unwrap(),
        b"built",
        "concurrent build did not complete after retaining its target"
    );
}

/// Pin the second half of the protocol: activity can begin after the cheap age
/// check but before the pruner has acquired its exclusive lease.  A PATH-local
/// flock shim refreshes an artifact immediately after the real exclusive flock
/// succeeds; the production under-lock recheck must then retain it.
#[test]
fn target_pruner_rechecks_activity_after_acquiring_the_exclusive_lease() {
    let btrfs = tempfile::tempdir().expect("btrfs root");
    let runner_work = tempfile::tempdir().expect("runner work root");
    let work = tempfile::tempdir().expect("worktree");
    let (ok, out) = run_link(work.path(), btrfs.path());
    assert!(ok, "cargo-target-link.sh failed:\n{out}");

    let target = std::fs::read_link(work.path().join("target")).unwrap();
    let payload = target.join("old-artifact");
    std::fs::write(&payload, b"keep after refresh").unwrap();
    age_file(&payload);

    let shim_dir = tempfile::tempdir().expect("flock shim dir");
    let shim = shim_dir.path().join("flock");
    std::fs::write(
        &shim,
        r#"#!/usr/bin/env bash
/usr/bin/flock "$@"
rc=$?
if ((rc == 0)) && [[ " $* " == *" -x "* ]]; then
    /usr/bin/touch -- "$REFRESH_AFTER_EXCLUSIVE_LOCK"
fi
exit "$rc"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&shim).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&shim, permissions).unwrap();
    let path = format!(
        "{}:{}",
        shim_dir.path().display(),
        std::env::var("PATH").expect("PATH")
    );

    let out = Command::new(repo_root().join("scripts/runner-disk-preflight.sh"))
        .arg("--target-dirs-only")
        .env("BTRFS_ROOT", btrfs.path())
        .env("RUNNER_WORK_ROOT", runner_work.path())
        .env("TARGET_AGE_DAYS", "1")
        .env("REFRESH_AFTER_EXCLUSIVE_LOCK", &payload)
        .env("PATH", path)
        .output()
        .expect("run target-dir preflight with flock shim");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "target-dir preflight failed:\n{text}");
    assert!(
        text.contains("became active before exclusive lease"),
        "preflight did not perform the activity recheck under its exclusive lease:\n{text}"
    );
    assert!(
        payload.exists(),
        "artifact refreshed after exclusive acquisition was still deleted"
    );
}

/// All recipe-level Cargo executions must expand through the lease wrapper.
/// Keeping the override in one variable makes this enforceable, while the raw
/// command check catches one-off recipes that bypass `$(CARGO)`.
#[test]
fn makefile_routes_every_cargo_command_through_the_target_lease() {
    let mk = std::fs::read_to_string(repo_root().join("Makefile")).expect("read Makefile");
    assert!(
        mk.lines().any(|line| {
            line.starts_with("override CARGO =") && line.contains("scripts/cargo-target-run.sh")
        }),
        "Makefile's CARGO command is not forced through scripts/cargo-target-run.sh"
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
    let bypasses: Vec<&str> = mk
        .lines()
        .filter(|line| line.starts_with('\t'))
        .map(str::trim)
        .filter(|line| {
            let command = line.trim_start_matches('@');
            !command.starts_with('#')
                && !command.starts_with("echo ")
                && !command.starts_with("printf ")
        })
        .filter(|line| {
            raw_commands
                .iter()
                .any(|command| line.starts_with(command) || line.contains(&format!(" {command}")))
        })
        .collect();
    assert!(
        bypasses.is_empty(),
        "Makefile recipe(s) bypass the cargo-target shared lease: {bypasses:#?}"
    );
}
