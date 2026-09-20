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
//! The child also runs in a mount namespace of its own, entered before it executes, with
//! nothing shared in either direction. Every mount that it, podman, conmon, conmon's exit
//! command or the product makes lives there and goes with the last process in it. None can
//! reach the host's mount table, whatever becomes of the child, so the parent has no mount
//! to find or detach.
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

/// What every line about this test's progress starts with. The parent prints how long
/// each of its steps took and repeats what the child said about its own, in a passing run
/// too, so that a slow run says where the time went.
const NOTE: &str = "storage-image-runroot:";

/// Set for a child that is to be killed once its container runs.
const CHILD_DIES: &str = "FCVM_STORAGE_IMAGE_RUNROOT_TEST_DIES";

/// Printed by that child before it dies, so that its parent knows how far it got.
const CONTAINER_RUNS: &str = "storage-image-runroot: the reference container runs";

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
    let fixture = Fixture::new()?;
    let ran = fixture.run_child(false)?;
    // The child has exited and its container still runs, in the child's namespace.
    let on_the_host = mounts_below(&fixture.root);
    let root = fixture.root.clone();
    // Before the scratch directory is dropped, and whatever the child did.
    let cleaned = fixture.clean_up(ran.namespace);

    let (stdout, stderr) = (
        String::from_utf8_lossy(&ran.output.stdout),
        String::from_utf8_lossy(&ran.output.stderr),
    );
    anyhow::ensure!(
        ran.output.status.success(),
        "{}\n{stdout}\n{stderr}",
        ran.output.status
    );
    anyhow::ensure!(
        // Not a whole line: libtest can print `test <name> ... ` ahead of it, unterminated.
        stdout.contains(BODY_RAN),
        "the re-executed test exited 0 without running its body. Is TEST_NAME still the name of this test?\n{stdout}\n{stderr}"
    );
    cleaned.context("cleaning up the private store")?;
    nothing_on_the_host(&root, on_the_host?)
}

/// A child that is killed runs no cleanup of its own, and its container runs on. The
/// parent still removes the container, from inside the child's namespace, and the host's
/// mount table never holds anything of the store.
#[test]
fn a_killed_child_leaves_nothing_on_the_host() -> Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let fixture = Fixture::new()?;
    let ran = fixture.run_child(true)?;
    let on_the_host = mounts_below(&fixture.root);
    let root = fixture.root.clone();
    let cleaned = fixture.clean_up(ran.namespace);

    let stdout = String::from_utf8_lossy(&ran.output.stdout);
    anyhow::ensure!(
        ran.output.status.signal() == Some(libc::SIGKILL) && stdout.contains(CONTAINER_RUNS),
        "the child was to be killed once its container ran: {}\n{stdout}\n{}",
        ran.output.status,
        String::from_utf8_lossy(&ran.output.stderr)
    );
    cleaned.context("cleaning up after the killed child")?;
    nothing_on_the_host(&root, on_the_host?)
}

/// What both tests require of the host once the cleanup has returned: its mount table
/// held nothing of the store while the container ran, holds nothing now, and the scratch
/// directory is gone.
fn nothing_on_the_host(root: &Path, while_the_container_ran: Vec<PathBuf>) -> Result<()> {
    anyhow::ensure!(
        while_the_container_ran.is_empty(),
        "the host's mount table had entries under {} while its container ran: {while_the_container_ran:?}",
        root.display()
    );
    let now = mounts_below(root)?;
    anyhow::ensure!(
        now.is_empty(),
        "the host's mount table has entries under {} after the cleanup: {now:?}",
        root.display()
    );
    anyhow::ensure!(!root.exists(), "{} outlived the cleanup", root.display());
    Ok(())
}

/// A private store under a scratch directory, for a child to run the test body against.
struct Fixture {
    scratch: tempfile::TempDir,
    root: PathBuf,
    conf: PathBuf,
    reference: String,
}

/// What a child left behind: its output, and its mount namespace.
struct Ran {
    output: std::process::Output,
    namespace: Result<std::fs::File>,
}

impl Fixture {
    fn new() -> Result<Self> {
        anyhow::ensure!(
            nix::unistd::geteuid().is_root(),
            "this test needs root: run it with `make test-root`"
        );
        let scratch = tempfile::TempDir::new()?;
        let root = scratch.path().canonicalize()?;
        let began = Instant::now();
        save_reference_image(&root.join(ARCHIVE))?;
        println!(
            "{NOTE} the reference image was saved after {:.1}s",
            began.elapsed().as_secs_f32()
        );
        let conf = root.join("storage.conf");
        std::fs::write(
            &conf,
            format!(
                "[storage]\ndriver = \"overlay\"\ngraphroot = {:?}\nrunroot = {:?}\n",
                root.join(GRAPHROOT),
                root.join(RUNROOT)
            ),
        )?;
        // Named in the log so that a directory or process left behind can be traced to its run.
        println!("private store under {}", root.display());
        let reference = format!("fcvm-runroot-{}", uuid::Uuid::new_v4().simple());
        Ok(Self {
            scratch,
            root,
            conf,
            reference,
        })
    }

    /// Run the test body in a child: this binary again, with the private store as its
    /// default store, in a mount namespace of its own. `dies` has the child killed once
    /// its container runs.
    fn run_child(&self, dies: bool) -> Result<Ran> {
        let began = Instant::now();
        let mut child = Command::new(std::env::current_exe()?);
        child
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_ROOT, &self.root)
            .env(CHILD_REFERENCE, &self.reference)
            .env("CONTAINERS_STORAGE_CONF", &self.conf)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if dies {
            child.env(CHILD_DIES, "1");
        }
        // nextest kills this process at its timeout. The child must not outlive it.
        common::set_test_pdeathsig_std(&mut child);
        in_its_own_mount_namespace(&mut child);
        let child = child
            .spawn()
            .context("starting the test body against the private store")?;
        // `spawn` returns once the child has run its hooks and called exec, so it is in its
        // namespace by now. Its pid is this process's to wait for, so no other process
        // can have it yet.
        let namespace = mount_namespace_of(child.id());
        let output = child
            .wait_with_output()
            .context("waiting for the test body")?;
        println!(
            "{NOTE} the child ran for {:.1}s",
            began.elapsed().as_secs_f32()
        );
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(at) = line.find(NOTE) {
                println!("  child: {}", &line[at..]);
            }
        }
        Ok(Ran { output, namespace })
    }

    /// Leave nothing of the private store behind. `namespace` is the child's.
    fn clean_up(self, namespace: Result<std::fs::File>) -> Result<()> {
        let Self {
            scratch,
            conf,
            reference,
            ..
        } = self;
        clean_up_private_store(scratch, move || {
            let namespace = namespace.context("the child's mount namespace")?;
            remove_reference(&conf, &reference, &namespace)
        })
    }
}

/// Have `command` start in a mount namespace of its own, with every mount in it private.
/// Nothing it mounts reaches the host's mount table, and nothing mounted on the host
/// afterwards reaches it. Only system calls are made between fork and exec.
fn in_its_own_mount_namespace(command: &mut Command) {
    use nix::mount::MsFlags;
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook runs in the forked child before exec and makes two system calls.
    unsafe {
        command.pre_exec(|| {
            nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNS)?;
            nix::mount::mount(
                None::<&str>,
                "/",
                None::<&str>,
                MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                None::<&str>,
            )?;
            Ok(())
        });
    }
}

/// The mount namespace of process `pid`, which must not be this process's own.
fn mount_namespace_of(pid: u32) -> Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;
    let path = format!("/proc/{pid}/ns/mnt");
    let namespace = std::fs::File::open(&path).with_context(|| format!("opening {path}"))?;
    let own = std::fs::metadata("/proc/self/ns/mnt").context("reading /proc/self/ns/mnt")?;
    anyhow::ensure!(
        namespace.metadata()?.ino() != own.ino(),
        "process {pid} shares this process's mount namespace"
    );
    Ok(namespace)
}

/// The programs that can open a store again: conmon, and the podman it runs as the
/// container's exit command.
const STORE_PROGRAMS: [&str; 2] = ["conmon", "podman"];

/// How long the cleanup waits for those programs to be gone.
const STORE_PROCESS_TIMEOUT: Duration = Duration::from_secs(60);

/// `podman rm` on the reference container, from inside the namespace its mounts are in.
/// A podman that opened the store from the host's namespace would mount the store's
/// overlay home there. The child leaves its container running, and a killed child could
/// not remove it anyway. `--ignore` makes a container that is already gone a success.
/// Anything else is a failure, with what podman said.
fn remove_reference(conf: &Path, reference: &str, namespace: &std::fs::File) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut remove = Command::new("podman");
    remove
        .args(["rm", "-f", "-t", "0", "--ignore", reference])
        .env("CONTAINERS_STORAGE_CONF", conf);
    common::set_test_pdeathsig_std(&mut remove);
    let namespace = namespace
        .try_clone()
        .context("duplicating the namespace descriptor")?;
    // SAFETY: the hook runs in the forked child before exec and makes one system call.
    unsafe {
        remove.pre_exec(move || {
            nix::sched::setns(&namespace, nix::sched::CloneFlags::CLONE_NEWNS)?;
            Ok(())
        });
    }
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
/// the store again, and no directory. The store's mounts are in the child's namespace
/// and go with the last process in it, so there is none to detach here.
/// `remove_container` is the step that needs podman, so that the rest can be tested
/// without one.
///
/// The wait runs whatever the removal returned, and every failure is reported, the
/// removal's first. A store that is still in use when the wait expires keeps its
/// directory: a podman that runs on would make it again under the same name. The
/// failure names the process and the directory. The directory also stays if this
/// process's mount table, which is the host's, has an entry below it. The namespace
/// rules that out, and removing the directory would walk into the mount.
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
    let began = Instant::now();
    if let Err(error) = remove_container() {
        failures.push(format!("removing the container: {error:#}"));
    }
    let removed = began.elapsed();
    let mut keep = false;
    if let Err(error) = wait_until_store_is_unused(&root, limit) {
        failures.push(format!("{error:#}"));
        keep = true;
    }
    println!(
        "{NOTE} the removal took {:.1}s, the wait for the store's processes {:.1}s",
        removed.as_secs_f32(),
        (began.elapsed() - removed).as_secs_f32()
    );
    match mounts_below(&root) {
        Ok(mounts) if mounts.is_empty() => {}
        Ok(mounts) => {
            failures.push(format!(
                "the host's mount table has entries under {}: {mounts:?}",
                root.display()
            ));
            keep = true;
        }
        Err(error) => {
            failures.push(format!("reading the mount table: {error:#}"));
            keep = true;
        }
    }
    if keep {
        failures.push(format!("left {} in place", scratch.keep().display()));
    } else if let Err(error) = scratch.close() {
        // Dropping it would ignore a removal that fails.
        failures.push(format!("removing {}: {error}", root.display()));
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
/// returned. A cleanup that deletes the directory before that process is gone deletes
/// the store under a podman that still uses it.
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

/// When `podman rm` fails, the wait still has to run.
#[test]
fn a_failed_removal_does_not_skip_the_wait() -> Result<()> {
    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    let outside = tempfile::TempDir::new()?;
    let mut podman = stand_in(outside.path(), "podman", STAND_IN_SECS, held_under(&root)?)?;

    let cleaned = clean_up_private_store(scratch, || anyhow::bail!("podman rm said no"));

    anyhow::ensure!(
        podman.0.try_wait()?.is_some(),
        "a failed removal skipped the wait"
    );
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

/// A process that still uses the store when the wait expires keeps it. Removing the
/// directory under a podman that runs on is what the wait exists to avoid.
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

/// Unmounts what a test mounted, on every way out of it.
struct Unmount(PathBuf);

impl Drop for Unmount {
    fn drop(&mut self) {
        let _ = nix::mount::umount2(&self.0, nix::mount::MntFlags::MNT_DETACH);
    }
}

/// The child's namespace keeps every mount of the store out of this process's mount
/// table. If one is there all the same, the cleanup fails and leaves the directory:
/// removing it would walk into the mount and delete what it holds.
#[test]
fn a_mount_in_the_hosts_table_fails_the_cleanup_and_keeps_the_store() -> Result<()> {
    anyhow::ensure!(
        nix::unistd::geteuid().is_root(),
        "this test mounts something: run it with `make test-root`"
    );
    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    let _remove = RemoveTree(root.clone());
    let mounted = root.join("mounted");
    std::fs::create_dir(&mounted)?;
    nix::mount::mount(
        Some("tmpfs"),
        &mounted,
        Some("tmpfs"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )?;
    let _unmount = Unmount(mounted.clone());
    std::fs::write(mounted.join("kept"), "")?;

    let cleaned = clean_up_within(scratch, || Ok(()), Duration::from_millis(300));

    let error = format!(
        "{:#}",
        cleaned.expect_err("a mount in the host's table is a failure")
    );
    anyhow::ensure!(
        error.contains("the host's mount table has entries under"),
        "{error}"
    );
    anyhow::ensure!(
        mounted.join("kept").exists(),
        "the cleanup removed what the mount holds:\n{error}"
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
    // Every podman call below mounts something. Go no further in the mount namespace
    // of the process that started this one.
    let own = std::fs::read_link("/proc/self/ns/mnt")?;
    let host = std::fs::read_link(format!("/proc/{}/ns/mnt", nix::unistd::getppid()))?;
    anyhow::ensure!(
        own != host,
        "this process shares the mount namespace of the one that started it: {own:?}"
    );
    let began = Instant::now();
    let note = |what: &str| println!("{NOTE} {what} after {:.1}s", began.elapsed().as_secs_f32());
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
    note("podman resolved the private store");

    let archive = root.join(ARCHIVE);
    podman(&["load", "-i", utf8(&archive)?])?;
    note("the reference image was loaded");
    start_reference(reference, root)?;
    let before = Observed::of(reference, &runroot)?;
    anyhow::ensure!(
        before.attached(),
        "the fixture is broken before any build:\n{before}"
    );
    // The container runs, so the store's overlay home, the container's root and its shm
    // are mounted now. None of it belongs in the host's mount table.
    let on_the_host = mounts_in(&host_mount_table(), root)?;
    anyhow::ensure!(
        on_the_host.is_empty(),
        "the host's mount table has entries under the private store: {on_the_host:?}"
    );
    println!(
        "{CONTAINER_RUNS} after {:.1}s",
        began.elapsed().as_secs_f32()
    );
    if std::env::var_os(CHILD_DIES).is_some() {
        use std::io::Write;
        std::io::stdout().flush()?;
        nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGKILL)?;
        anyhow::bail!("SIGKILL did not end this process");
    }

    let cache = root.join("cache");
    std::fs::create_dir(&cache)?;
    let image = cache.join("alpine.storage.img");
    tokio::runtime::Runtime::new()?
        .block_on(fcvm::commands::podman::build_storage_image(
            &archive, &image,
        ))
        .context("building the storage image")?;
    note("the storage image was built");

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

/// Start the reference container the way tests/test_exec_podman_parity.rs starts its
/// reference, and record its conmon as soon as it runs. The parent removes it, from
/// inside this namespace, whatever becomes of this process.
fn start_reference(name: &str, root: &Path) -> Result<()> {
    let pidfile = root.join(CONMON_PIDFILE);
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
    record_conmon(root)
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

/// The mount table of the process that started this one: the host's.
fn host_mount_table() -> PathBuf {
    PathBuf::from(format!("/proc/{}/mountinfo", nix::unistd::getppid()))
}

/// Mount points at or below `dir` in this process's mount table, deepest first.
fn mounts_below(dir: &Path) -> Result<Vec<PathBuf>> {
    mounts_in(Path::new("/proc/self/mountinfo"), dir)
}

/// Mount points at or below `dir` in the mount table `table`, deepest first.
fn mounts_in(table: &Path, dir: &Path) -> Result<Vec<PathBuf>> {
    let table =
        std::fs::read_to_string(table).with_context(|| format!("reading {}", table.display()))?;
    let mut mounts: Vec<PathBuf> = table
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(PathBuf::from)
        .filter(|mount| mount.starts_with(dir))
        .collect();
    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.components().count()));
    Ok(mounts)
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
