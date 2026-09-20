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
    let cleaned = clean_up_private_store(&root, || remove_reference(&conf, &reference));

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

/// The child removes its container on every path it controls. A killed child cannot.
fn remove_reference(conf: &Path, reference: &str) -> Result<()> {
    let _ = Command::new("podman")
        .args(["rm", "-f", "-t", "0", reference])
        .env("CONTAINERS_STORAGE_CONF", conf)
        .output();
    Ok(())
}

/// Leave nothing of the private store behind. `remove_container` is the step that needs
/// podman, so that the rest can be tested without one.
fn clean_up_private_store(
    root: &Path,
    remove_container: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let _ = remove_container();
    // A mount left below the directory would outlive its removal.
    detach_mounts_below(root);
    Ok(())
}

/// How long the stand-in below lives.
const STAND_IN_SECS: &str = "2";

/// Kills and reaps the stand-in on every way out of the test.
struct StandIn(std::process::Child);

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// conmon runs the container's exit command, `podman container cleanup --rm`, after it
/// has written the exit file that `podman rm -f` waits for. So that podman process can
/// still be running, and can open the private store again, when `podman rm` has
/// returned. Opening the store mounts its overlay home. A cleanup that detaches the
/// mounts and deletes the directory before that process is gone leaves a mount and a
/// directory behind on the host.
///
/// The stand-in is that process without the podman in it: it is named `podman`, its
/// command line names the store, and the removal returns while it is still running.
#[test]
fn the_cleanup_waits_for_a_process_that_still_names_the_store() -> Result<()> {
    let scratch = tempfile::TempDir::new()?;
    let root = scratch.path().canonicalize()?;
    let bin = root.join("bin");
    std::fs::create_dir(&bin)?;
    let sleep = ["/usr/bin/sleep", "/bin/sleep"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.exists())
        .context("no sleep on this host")?;
    std::os::unix::fs::symlink(sleep, bin.join("podman"))?;
    let mut command = Command::new(bin.join("podman"));
    command.arg(STAND_IN_SECS).stdin(Stdio::null());
    common::set_test_pdeathsig_std(&mut command);
    let mut stand_in = StandIn(command.spawn().context("starting the stand-in")?);

    let mut running_at_removal = false;
    clean_up_private_store(&root, || {
        running_at_removal = stand_in.0.try_wait()?.is_none();
        Ok(())
    })?;

    anyhow::ensure!(
        running_at_removal,
        "the stand-in was gone before the removal returned, so this run shows nothing"
    );
    anyhow::ensure!(
        stand_in.0.try_wait()?.is_some(),
        "the cleanup returned while a process that names the store was still running"
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
    let _container = ReferenceContainer::start(reference)?;
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
    /// Started the way tests/test_exec_podman_parity.rs starts its reference.
    fn start(name: &str) -> Result<Self> {
        // The guard exists before `podman run`: a start that fails half way can
        // leave a created container behind.
        let container = Self(name.to_owned());
        podman(&[
            "run",
            "-d",
            "--rm",
            "--name",
            name,
            "--network",
            "none",
            common::ALPINE_IMAGE,
            "sleep",
            REFERENCE_LIFETIME_SECS,
        ])?;
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
