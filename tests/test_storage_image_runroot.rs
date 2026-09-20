//! Building a storage image must not disturb containers of the default store (#944).
//!
//! `build_storage_image` loads the archive into a temporary podman store. Podman keeps
//! its record of mounted layers in the runroot, and a store rewrites that record from the
//! layers it knows. A temporary store that shares the default runroot therefore erases
//! the record of every container running in the default store. The next podman process
//! to exit finds no mounted layer and lazily unmounts the default store's overlay home.
//! Each running container then loses its merged root from the host's view: inspect
//! reports no MergedDir and `podman exec -u <name>` fails with "unable to find user",
//! while the container keeps running. Only a new container recovers.
//!
//! The child process below gets its own "default store" from a private storage.conf, so a
//! failing run cannot detach containers that belong to anything else on the host. The
//! temporary store is the one `build_storage_image` creates, so removing its private
//! runroot turns this test red.
//!
//! Root only, like the CI jobs where the failure showed up.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_ROOT: &str = "FCVM_STORAGE_IMAGE_RUNROOT_TEST_ROOT";
const CHILD_REFERENCE: &str = "FCVM_STORAGE_IMAGE_RUNROOT_TEST_REFERENCE";
const TEST_NAME: &str = "test_storage_image_build_leaves_running_containers_attached";

/// Printed by the child once its body has run to the end. `--exact` with a name that
/// matches no test exits 0 having run nothing, so after a rename of the test the exit
/// status alone would keep this green with no body behind it.
const BODY_RAN: &str = "storage-image-runroot: the test body ran to its end";

const ARCHIVE: &str = "alpine.tar";
const GRAPHROOT: &str = "store";
const RUNROOT: &str = "run";

/// Where podman is told to write conmon's pid, below the scratch directory.
const CONMON_PIDFILE: &str = "conmon.pid";

/// conmon's pid and its start time, written by the test body while the reference
/// container runs. A pid names whatever process holds the number now. With the start
/// time it names one process.
const CONMON_IDENTITY: &str = "conmon.identity";

/// Bounded, and the container runs with `--rm`: a killed test cannot run its
/// cleanup, so the container has to remove itself.
const REFERENCE_LIFETIME_SECS: &str = "300";

#[test]
fn test_storage_image_build_leaves_running_containers_attached() -> Result<()> {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let reference = std::env::var(CHILD_REFERENCE).context("reference container name")?;
        build_next_to_a_running_container(Path::new(&root), &reference)?;
        println!("{BODY_RAN}");
        return Ok(());
    }
    anyhow::ensure!(
        nix::unistd::geteuid().is_root(),
        "this test needs root: run it with `make test-root`"
    );

    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    save_reference_image(&root.join(ARCHIVE))?;
    let conf = root.join("storage.conf");
    std::fs::write(
        &conf,
        format!(
            "[storage]\ndriver = \"overlay\"\ngraphroot = {:?}\nrunroot = {:?}\n",
            root.join(GRAPHROOT),
            root.join(RUNROOT)
        ),
    )?;
    // Named in the log so that a directory, mount or process left behind can be traced to its run.
    println!("private store under {}", root.display());
    let reference = format!("fcvm-runroot-{}", uuid::Uuid::new_v4().simple());

    let mut child = Command::new(std::env::current_exe()?);
    child
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_ROOT, &root)
        .env(CHILD_REFERENCE, &reference)
        .env("CONTAINERS_STORAGE_CONF", &conf);
    // nextest kills this process at its timeout. The child must not outlive it.
    common::set_test_pdeathsig_std(&mut child);
    let output = child
        .output()
        .context("running the test body against the private store")?;

    // Before `scratch` is dropped, and whatever the child did.
    let cleaned = clean_up_private_store(scratch, || remove_reference(&conf, &reference));

    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    anyhow::ensure!(
        output.status.success(),
        "{}\n{stdout}\n{stderr}",
        output.status
    );
    anyhow::ensure!(
        // Not a whole line: libtest can print `test <name> ... ` ahead of it, unterminated.
        stdout.contains(BODY_RAN),
        "the re-executed test exited 0 without running its body. Is TEST_NAME still the name of this test?\n{stdout}\n{stderr}"
    );
    cleaned.context("cleaning up the private store")
}

/// The programs that can open a store again: conmon, and the podman it runs as the
/// container's exit command.
const STORE_PROGRAMS: [&str; 2] = ["conmon", "podman"];

/// How long the cleanup waits for those programs to be gone.
const STORE_PROCESS_TIMEOUT: Duration = Duration::from_secs(60);

/// The child removes its container on every path it controls, and a killed child
/// cannot. `--ignore` makes a container that is already gone a success. Anything else
/// is a failure, with what podman said.
fn remove_reference(conf: &Path, reference: &str) -> Result<()> {
    let mut remove = Command::new("podman");
    remove
        .args(["rm", "-f", "-t", "0", "--ignore", reference])
        .env("CONTAINERS_STORAGE_CONF", conf);
    common::set_test_pdeathsig_std(&mut remove);
    let removed = remove.output().context("running podman rm")?;
    anyhow::ensure!(
        removed.status.success(),
        "podman rm {reference}: {}: {}",
        removed.status,
        String::from_utf8_lossy(&removed.stderr).trim()
    );
    Ok(())
}

/// Leave nothing of the private store behind: no container, no process that can open
/// the store again, no mount, and no directory. In that order, because a process that
/// opens the store mounts its overlay home, and a mount made after the detach keeps the
/// directory on the host when it is removed. `remove_container` is the step that needs
/// podman, so that the rest can be tested without one.
///
/// The wait runs whatever the removal returned, and every failure is reported, the
/// removal's first. A store that is still in use when the wait expires is left as it is,
/// mounts and directory: taking either away under a process that can open the store
/// again is the race this cleanup exists to avoid. The failure names the process and
/// the directory.
fn clean_up_private_store(
    scratch: tempfile::TempDir,
    remove_container: impl FnOnce() -> Result<()>,
) -> Result<()> {
    clean_up_within(scratch, remove_container, STORE_PROCESS_TIMEOUT)
}

/// `clean_up_private_store` with the wait's limit as a parameter, for the tests.
fn clean_up_within(
    scratch: tempfile::TempDir,
    remove_container: impl FnOnce() -> Result<()>,
    limit: Duration,
) -> Result<()> {
    let root = scratch.path().canonicalize()?;
    let mut failures = Vec::new();
    if let Err(error) = remove_container() {
        failures.push(format!("removing the container: {error:#}"));
    }
    match wait_until_store_is_unused(&root, limit) {
        Ok(()) => {
            detach_mounts_below(&root);
            match mounts_below(&root) {
                Ok(mounts) if mounts.is_empty() => {}
                Ok(mounts) => failures.push(format!(
                    "still mounted below {} after the cleanup: {mounts:?}",
                    root.display()
                )),
                Err(error) => failures.push(format!("reading the mount table: {error:#}")),
            }
            // Dropping it would ignore a removal that fails.
            if let Err(error) = scratch.close() {
                failures.push(format!("removing {}: {error}", root.display()));
            }
        }
        Err(error) => {
            let kept = scratch.keep();
            failures.push(format!("{error:#}\nleft {} in place", kept.display()));
        }
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

/// `podman rm -f` returns once conmon has written the container's exit file. conmon
/// writes it before it runs the exit command, so both can still be alive then.
fn wait_until_store_is_unused(root: &Path, limit: Duration) -> Result<()> {
    let deadline = Instant::now() + limit;
    loop {
        let users = store_users(root);
        if users.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "still using the store under {} after {limit:?}:\n{}",
            root.display(),
            users.join("\n")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Processes other than this one that can still open the store under `root`, one line
/// each. Nothing here reads a process's memory: a program's name, its state and start
/// time, its working directory and what its descriptors point at are kept with the
/// task, and reading them does not block on a process that is stuck in the kernel.
///
/// Two things tie a process to the store. conmon was recorded, by pid and start time,
/// while the container ran ([`record_conmon`]). The start time tells it from a process
/// that was given its pid later. conmon closes what it held under the store before it
/// runs the exit command, and its working directory is its caller's, so by then the
/// record is all there is. It forks the exit command and waits for it (conmon
/// `do_exit_command`), so it is there for as long as that command is. And any conmon or
/// podman whose working directory or open files are under `root` is using the store,
/// whatever its arguments say.
fn store_users(root: &Path) -> Vec<String> {
    let own = std::process::id();
    let mut users = Vec::new();
    let mut counted = None;
    if let Some((pid, started)) = recorded_conmon(root) {
        // A process that has exited keeps its entry under /proc, with its start time,
        // until its parent has reaped it.
        let running = state_and_start(pid).filter(|(state, _)| !matches!(state, 'Z' | 'X'));
        let user = match (running, started) {
            (Some((_, start)), Some(started)) => (start == started)
                .then(|| format!("{pid} conmon: the process recorded in {CONMON_IDENTITY}")),
            // No start time was recorded, so the name decides. That errs towards waiting.
            (Some(_), None) => (program_of(pid).as_deref() == Some("conmon"))
                .then(|| format!("{pid} conmon: the pid in {CONMON_PIDFILE}")),
            (None, _) => None,
        };
        if let Some(user) = user {
            users.push(user);
            counted = Some(pid);
        }
    }
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own || Some(pid) == counted {
            continue;
        }
        let Some(program) = program_of(pid) else {
            continue;
        };
        if !STORE_PROGRAMS.contains(&program.as_str()) {
            continue;
        }
        if let Some(held) = held_below(pid, root) {
            users.push(format!("{pid} {program}: {held}"));
        }
    }
    users
}

/// Write conmon's identity next to the pid file podman wrote. The caller has just
/// started the container and goes on to require an answer from it, so conmon is running
/// across this read, and the pid in podman's file is its own. The file appears under its
/// name complete or not at all.
fn record_conmon(root: &Path) -> Result<()> {
    let pidfile = root.join(CONMON_PIDFILE);
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .with_context(|| format!("reading {}", pidfile.display()))?
        .trim()
        .parse()
        .with_context(|| format!("{} does not hold a pid", pidfile.display()))?;
    let (_, started) =
        state_and_start(pid).with_context(|| format!("conmon ({pid}) is not running"))?;
    let unfinished = root.join(format!("{CONMON_IDENTITY}.tmp"));
    std::fs::write(&unfinished, format!("{pid} {started}"))?;
    std::fs::rename(&unfinished, root.join(CONMON_IDENTITY))?;
    Ok(())
}

/// conmon's pid, and its start time when the test body got as far as recording it. With
/// only podman's file there is no start time to compare.
fn recorded_conmon(root: &Path) -> Option<(u32, Option<u64>)> {
    let identity = std::fs::read_to_string(root.join(CONMON_IDENTITY)).ok();
    if let Some((pid, started)) = identity
        .as_deref()
        .and_then(|text| text.trim().split_once(' '))
    {
        if let (Ok(pid), Ok(started)) = (pid.parse(), started.parse()) {
            return Some((pid, Some(started)));
        }
    }
    let pid = std::fs::read_to_string(root.join(CONMON_PIDFILE)).ok()?;
    Some((pid.trim().parse().ok()?, None))
}

/// The state and the start time of a process, from one read of `/proc/<pid>/stat`, so
/// that both describe the same process. The start time counts clock ticks since boot,
/// and a pid that has been given to another process has a later one. The kernel fills
/// this file from the task and does not read the process's memory.
fn state_and_start(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The program's name is in parentheses and can hold spaces and parentheses itself.
    let mut fields = stat.get(stat.rfind(')')? + 1..)?.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    // The state is field 3 and the start time is field 22.
    let started = fields.nth(18)?.parse().ok()?;
    Some((state, started))
}

/// The kernel's name for the process, from `/proc/<pid>/comm`.
fn program_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim_end().to_owned())
}

/// What the process holds at or below `root`: its working directory, or the first open
/// file. Both are symbolic links under `/proc/<pid>`.
fn held_below(pid: u32, root: &Path) -> Option<String> {
    let process = PathBuf::from(format!("/proc/{pid}"));
    if let Ok(cwd) = std::fs::read_link(process.join("cwd")) {
        if cwd.starts_with(root) {
            return Some(format!("working directory {}", cwd.display()));
        }
    }
    for descriptor in std::fs::read_dir(process.join("fd"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Ok(target) = std::fs::read_link(descriptor.path()) {
            if target.starts_with(root) {
                return Some(format!(
                    "descriptor {} is {}",
                    descriptor.file_name().to_string_lossy(),
                    target.display()
                ));
            }
        }
    }
    None
}

/// How long a stand-in lives.
const STAND_IN_SECS: &str = "2";

/// Kills and reaps a stand-in on every way out of its test.
struct StandIn(std::process::Child);

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `sleep <secs>` under another program's name: a symlink `<dir>/<program>`, run by that
/// path, so the kernel's name for the process is `program`.
fn stand_in(dir: &Path, program: &str, secs: &str, stdin: Stdio) -> Result<StandIn> {
    std::fs::create_dir_all(dir)?;
    let sleep = ["/usr/bin/sleep", "/bin/sleep"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.exists())
        .context("no sleep on this host")?;
    std::os::unix::fs::symlink(sleep, dir.join(program))?;
    let mut command = Command::new(dir.join(program));
    command.arg(secs).stdin(stdin);
    common::set_test_pdeathsig_std(&mut command);
    Ok(StandIn(command.spawn().context("starting the stand-in")?))
}

/// Run the cleanup with a removal that returns at once, and say whether the stand-in was
/// running at the removal and whether it was gone when the cleanup returned.
fn clean_up_next_to(stand_in: &mut StandIn, scratch: tempfile::TempDir) -> Result<()> {
    let mut running_at_removal = false;
    clean_up_private_store(scratch, || {
        running_at_removal = stand_in.0.try_wait()?.is_none();
        Ok(())
    })?;
    anyhow::ensure!(
        running_at_removal,
        "the stand-in was gone before the removal returned, so this run shows nothing"
    );
    anyhow::ensure!(
        stand_in.0.try_wait()?.is_some(),
        "the cleanup returned while a process that uses the store was still running"
    );
    Ok(())
}

/// conmon runs the container's exit command, `podman container cleanup --rm`, after it
/// has written the exit file that `podman rm -f` waits for. So that podman process can
/// still be running, and can open the private store again, when `podman rm` has
/// returned. Opening the store mounts its overlay home. A cleanup that detaches the
/// mounts and deletes the directory before that process is gone leaves a mount and a
/// directory behind on the host.
///
/// The stand-in is conmon in the state it is in while that command runs: it names the
/// store in its arguments, it has closed everything it held under it, and it is the
/// process that was recorded.
#[test]
fn the_cleanup_waits_for_a_process_that_still_names_the_store() -> Result<()> {
    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    let mut conmon = stand_in(&root.join("bin"), "conmon", STAND_IN_SECS, Stdio::null())?;
    std::fs::write(root.join(CONMON_PIDFILE), conmon.0.id().to_string())?;
    record_conmon(&root)?;
    clean_up_next_to(&mut conmon, scratch)
}

/// A file under `root` that is held open, as the standard input of a stand-in.
fn held_under(root: &Path) -> Result<Stdio> {
    std::fs::write(root.join("held"), "")?;
    Ok(Stdio::from(std::fs::File::open(root.join("held"))?))
}

/// The exit command holds the store's database and lock files while it runs, and
/// nothing in its arguments has to name the store. The stand-in is started from outside
/// the scratch directory, and an open file is all that ties it to the store.
#[test]
fn the_cleanup_waits_for_a_process_that_holds_a_file_under_the_store() -> Result<()> {
    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    let outside = tempfile::TempDir::new()?;
    let mut podman = stand_in(outside.path(), "podman", STAND_IN_SECS, held_under(&root)?)?;
    clean_up_next_to(&mut podman, scratch)
}

/// Detaches what a test mounted below its scratch directory on every way out of it.
struct MountsBelow(PathBuf);

impl Drop for MountsBelow {
    fn drop(&mut self) {
        detach_mounts_below(&self.0);
    }
}

/// When `podman rm` fails, the store still has to be released.
#[test]
fn a_failed_removal_does_not_skip_the_wait_and_the_detach() -> Result<()> {
    anyhow::ensure!(
        nix::unistd::geteuid().is_root(),
        "this test mounts something: run it with `make test-root`"
    );
    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    let _mounts = MountsBelow(root.clone());
    let outside = tempfile::TempDir::new()?;
    let mounted = root.join("mounted");
    std::fs::create_dir(&mounted)?;
    nix::mount::mount(
        Some(&mounted),
        &mounted,
        None::<&str>,
        nix::mount::MsFlags::MS_BIND,
        None::<&str>,
    )?;
    let mut podman = stand_in(outside.path(), "podman", STAND_IN_SECS, held_under(&root)?)?;

    let cleaned = clean_up_private_store(scratch, || anyhow::bail!("podman rm said no"));

    let mut skipped = Vec::new();
    if podman.0.try_wait()?.is_none() {
        skipped.push("the wait");
    }
    if !mounts_below(&root)?.is_empty() {
        skipped.push("the detach");
    }
    anyhow::ensure!(skipped.is_empty(), "a failed removal skipped {skipped:?}");
    let error = format!(
        "{:#}",
        cleaned.expect_err("the removal's error is the result")
    );
    anyhow::ensure!(error.contains("podman rm said no"), "{error}");
    Ok(())
}

/// Removes a directory a test had the cleanup leave in place.
struct RemoveTree(PathBuf);

impl Drop for RemoveTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A process that still uses the store when the wait expires keeps it. Detaching the
/// mounts and removing the directory under it is the race this cleanup exists to avoid:
/// it can mount the store's overlay home again afterwards.
#[test]
fn a_user_that_outlives_the_wait_keeps_its_store() -> Result<()> {
    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    // Declared first, so it runs last: after the stand-in below has been killed.
    let _remove = RemoveTree(root.clone());
    let outside = tempfile::TempDir::new()?;
    let podman = stand_in(outside.path(), "podman", "600", held_under(&root)?)?;

    let cleaned = clean_up_within(scratch, || Ok(()), Duration::from_millis(300));

    let error = format!(
        "{:#}",
        cleaned.expect_err("a store that is still in use is a failure")
    );
    anyhow::ensure!(
        error.contains(&format!("{} podman", podman.0.id())),
        "{error}"
    );
    anyhow::ensure!(
        root.join("held").exists(),
        "the store was removed under a process that still uses it:\n{error}"
    );
    Ok(())
}

/// A pid is given out again. When the recorded conmon has exited and another conmon on
/// the host has its pid, the pid and the name both match a process that has nothing to
/// do with the store. The stand-in is that process: a conmon outside the store, under a
/// pid that was recorded with an earlier start time.
#[test]
fn a_reused_pid_is_not_the_recorded_conmon() -> Result<()> {
    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    let _remove = RemoveTree(root.clone());
    let outside = tempfile::TempDir::new()?;
    let other = stand_in(outside.path(), "conmon", "600", Stdio::null())?;
    let pid = other.0.id();
    let (_, started) = state_and_start(pid).context("the stand-in is not running")?;
    std::fs::write(root.join(CONMON_IDENTITY), format!("{pid} {}", started - 1))?;
    clean_up_within(scratch, || Ok(()), Duration::from_millis(300))
        .context("the cleanup waited for a process that is not the recorded conmon")?;
    anyhow::ensure!(!root.exists(), "the store is still there");
    Ok(())
}

/// Two files under /proc/<pid> are read out of the process's own memory, and that read
/// can block for good on a process that is stuck in the kernel. A bounded wait that
/// reads them is not bounded. The names are put together here so that this test does
/// not find itself.
#[test]
fn the_cleanup_never_reads_another_process_memory() -> Result<()> {
    let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(file!()))?;
    let forbidden = [["cmd", "line"].concat(), ["envi", "ron"].concat()];
    let found: Vec<String> = source
        .lines()
        .enumerate()
        .filter_map(|(number, line)| {
            let code = line.split("//").next().unwrap_or_default();
            forbidden
                .iter()
                .any(|name| code.contains(name.as_str()))
                .then(|| format!("{}: {}", number + 1, line.trim()))
        })
        .collect();
    anyhow::ensure!(
        found.is_empty(),
        "this file reads a process's memory through /proc:\n{}",
        found.join("\n")
    );
    Ok(())
}

/// The test body. Every podman call here, and every podman call the product makes,
/// inherits the private storage.conf from the environment.
fn build_next_to_a_running_container(root: &Path, reference: &str) -> Result<()> {
    let runroot = root.join(RUNROOT);
    // A product that shares the runroot detaches every running container of the
    // store it shares it with. Go no further unless that store is the private one.
    let store = podman(&[
        "info",
        "--format",
        "{{.Store.GraphRoot}}\n{{.Store.RunRoot}}",
    ])?;
    anyhow::ensure!(
        store == format!("{}\n{}", root.join(GRAPHROOT).display(), runroot.display()),
        "podman did not resolve the private store: {store}"
    );

    let archive = root.join(ARCHIVE);
    podman(&["load", "-i", utf8(&archive)?])?;
    let _container = ReferenceContainer::start(reference, root)?;
    let before = Observed::of(reference, &runroot)?;
    anyhow::ensure!(
        before.attached(),
        "the fixture is broken before any build:\n{before}"
    );

    let cache = root.join("cache");
    std::fs::create_dir(&cache)?;
    let image = cache.join("alpine.storage.img");
    tokio::runtime::Runtime::new()?
        .block_on(fcvm::commands::podman::build_storage_image(
            &archive, &image,
        ))
        .context("building the storage image")?;

    // The overlay home is unmounted by the next podman process of the default
    // store that exits, not by the build. Make sure one has exited before looking.
    podman(&["ps"])?;
    let after = Observed::of(reference, &runroot)?;
    anyhow::ensure!(
        after.attached(),
        "build_storage_image detached a running container of the default store\nbefore:\n{before}\nafter:\n{after}"
    );

    // The temporary store, runroot included, is gone once the image exists.
    let beside_the_image: Vec<PathBuf> = std::fs::read_dir(&cache)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path != &image)
        .collect();
    anyhow::ensure!(
        beside_the_image.is_empty(),
        "build_storage_image left files next to the image: {beside_the_image:?}"
    );
    let mounts = mounts_below(&cache)?;
    anyhow::ensure!(
        mounts.is_empty(),
        "build_storage_image left mounts behind: {mounts:?}"
    );

    // The guest mounts the image as an additional image store. Only the store's
    // data belongs in it, never the temporary store's runtime state.
    anyhow::ensure!(
        image_top_level(&image)? == ["lost+found", "overlay", "overlay-images", "overlay-layers"],
        "unexpected top level in the storage image: {:?}",
        image_top_level(&image)?
    );
    Ok(())
}

/// What the host can tell about a running container.
struct Observed {
    /// `podman inspect` MergedDir, verbatim. `<no value>` once the mount record is gone.
    merged_dir: String,
    /// Whether the host reads the container's /etc/passwd through MergedDir, which
    /// is where podman looks a user name up.
    passwd_visible_from_host: bool,
    /// `podman exec -u nobody <container> id -u`: stdout, or the failure.
    nobody: String,
    /// The default store's record of mounted layers.
    mount_record: String,
}

impl Observed {
    fn of(container: &str, runroot: &Path) -> Result<Self> {
        let merged_dir = podman(&[
            "inspect",
            "--format",
            "{{.GraphDriver.Data.MergedDir}}",
            container,
        ])?;
        let passwd_visible_from_host =
            merged_dir.starts_with('/') && Path::new(&merged_dir).join("etc/passwd").exists();
        let exec = Command::new("podman")
            .args(["exec", "-u", "nobody", container, "id", "-u"])
            .output()
            .context("running podman exec")?;
        let nobody = if exec.status.success() {
            String::from_utf8_lossy(&exec.stdout).trim().to_owned()
        } else {
            format!(
                "{}: {}",
                exec.status,
                String::from_utf8_lossy(&exec.stderr).trim()
            )
        };
        let mount_record = std::fs::read_to_string(runroot.join("overlay-layers/mountpoints.json"))
            .unwrap_or_else(|error| format!("unreadable: {error}"));
        Ok(Self {
            merged_dir,
            passwd_visible_from_host,
            nobody,
            mount_record,
        })
    }

    fn attached(&self) -> bool {
        self.merged_dir.starts_with('/') && self.passwd_visible_from_host && self.nobody == "65534"
    }
}

impl std::fmt::Display for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "    MergedDir: {}", self.merged_dir)?;
        writeln!(
            f,
            "    /etc/passwd visible from the host: {}",
            self.passwd_visible_from_host
        )?;
        writeln!(f, "    exec -u nobody id -u: {}", self.nobody)?;
        write!(f, "    mount record: {}", self.mount_record)
    }
}

/// Removes the reference container as soon as the test body ends.
struct ReferenceContainer(String);

impl ReferenceContainer {
    /// Started the way tests/test_exec_podman_parity.rs starts its reference. Its conmon
    /// is recorded as soon as it runs.
    fn start(name: &str, root: &Path) -> Result<Self> {
        let pidfile = root.join(CONMON_PIDFILE);
        // The guard exists before `podman run`: a start that fails half way can
        // leave a created container behind.
        let container = Self(name.to_owned());
        podman(&[
            "run",
            "-d",
            "--rm",
            "--conmon-pidfile",
            utf8(&pidfile)?,
            "--name",
            name,
            "--network",
            "none",
            common::ALPINE_IMAGE,
            "sleep",
            REFERENCE_LIFETIME_SECS,
        ])?;
        record_conmon(root)?;
        Ok(container)
    }
}

impl Drop for ReferenceContainer {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "-f", "-t", "0", &self.0])
            .output();
    }
}

/// Write the reference image to `archive`, from the store the environment resolves.
fn save_reference_image(archive: &Path) -> Result<()> {
    let present = Command::new("podman")
        .args(["image", "exists", common::ALPINE_IMAGE])
        .status()
        .context("running podman image exists")?
        .success();
    if !present {
        podman(&["pull", common::ALPINE_IMAGE])?;
    }
    podman(&[
        "save",
        "--format",
        "docker-archive",
        "-o",
        utf8(archive)?,
        common::ALPINE_IMAGE,
    ])?;
    Ok(())
}

fn podman(args: &[&str]) -> Result<String> {
    let output = Command::new("podman")
        .args(args)
        .output()
        .with_context(|| format!("running podman {args:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "podman {args:?}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

/// Mount points at or below `dir`, deepest first.
fn mounts_below(dir: &Path) -> Result<Vec<PathBuf>> {
    let table = std::fs::read_to_string("/proc/self/mountinfo")?;
    let mut mounts: Vec<PathBuf> = table
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(PathBuf::from)
        .filter(|mount| mount.starts_with(dir))
        .collect();
    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.components().count()));
    Ok(mounts)
}

fn detach_mounts_below(dir: &Path) {
    for mount in mounts_below(dir).unwrap_or_default() {
        let _ = nix::mount::umount2(&mount, nix::mount::MntFlags::MNT_DETACH);
    }
}

/// Names in the root directory of an ext4 image, sorted. `debugfs -R "ls -p"` prints
/// one `/inode/mode/uid/gid/name/size/` line per entry.
fn image_top_level(image: &Path) -> Result<Vec<String>> {
    let output = Command::new("debugfs")
        .args(["-R", "ls -p /"])
        .arg(image)
        .output()
        .context("running debugfs")?;
    anyhow::ensure!(
        output.status.success(),
        "debugfs: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split('/').nth(5))
        .filter(|name| !matches!(*name, "" | "." | ".."))
        .map(str::to_owned)
        .collect();
    names.sort();
    Ok(names)
}
