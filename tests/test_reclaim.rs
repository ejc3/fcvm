//! A SIGKILLed fcvm runs no teardown. What it made on the host is recorded with its owner
//! before it is made (src/reclaim.rs), and the next launch reclaims it.
//!
//! Each test kills fcvm with SIGKILL while the resource exists, then runs another VM to
//! completion (every launch reclaims first) and checks that the resource is gone. Another
//! test's launch can reclaim it first, which changes nothing that is checked: each check
//! is that the resource is gone after a launch, and that it existed while fcvm lived.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn vm_disks(vm_id: &str) -> PathBuf {
    fcvm::paths::data_dir().join("vm-disks").join(vm_id)
}

/// What `fcvm ls --json` says about the VM whose fcvm process is `pid`.
async fn listed(pid: u32) -> Result<serde_json::Value> {
    let output = tokio::process::Command::new(common::find_fcvm_binary()?)
        .args(["ls", "--json", "--pid", &pid.to_string()])
        .output()
        .await
        .context("running fcvm ls")?;
    let listed: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing fcvm ls --json")?;
    listed
        .get(0)
        .cloned()
        .with_context(|| format!("fcvm ls lists no VM for pid {pid}: {listed}"))
}

/// The vm_id of the VM whose fcvm process is `pid`.
async fn vm_id_of(pid: u32) -> Result<String> {
    let vm = listed(pid).await?;
    vm["vm_id"]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("no vm_id in {vm}"))
}

async fn sigkill(child: &mut tokio::process::Child, pid: u32) -> Result<()> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .context("sending SIGKILL to fcvm")?;
    child.wait().await.context("reaping the killed fcvm")?;
    Ok(())
}

/// Stop a VM the way an operator does, and wait for its teardown to finish.
async fn stop(child: &mut tokio::process::Child, pid: u32) -> Result<()> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .context("sending SIGTERM to fcvm")?;
    tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .context("fcvm did not exit within 60 s of SIGTERM")?
        .context("waiting for fcvm")?;
    Ok(())
}

/// Run a VM to completion. Its launch reclaims what killed fcvm processes left.
async fn next_launch() -> Result<()> {
    next_launch_on("rootless").await
}

/// [`next_launch`] in a network mode, so that it runs as the user that the killed
/// process ran as: each user reclaims only its own records.
async fn next_launch_on(network: &str) -> Result<()> {
    next_launch_with(network, &[]).await
}

/// [`next_launch_on`] with `env` set for the launch, such as a data dir of its own.
async fn next_launch_with(network: &str, env: &[(&str, &str)]) -> Result<()> {
    let (name, _, _, _) = common::unique_names("reclaim-next");
    let (mut child, _) = common::spawn_fcvm_with_env(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--network",
            network,
            "--no-snapshot",
            common::TEST_IMAGE,
            "true",
        ],
        env,
    )
    .await?;
    let status = tokio::time::timeout(Duration::from_secs(180), child.wait())
        .await
        .context("the next launch did not finish within 180 s")?
        .context("waiting for the next launch")?;
    anyhow::ensure!(status.success(), "the next launch failed: {status}");
    Ok(())
}

/// Poll `found` every 10 ms until it returns Some, for at most `limit`.
async fn wait_for<T>(
    what: &str,
    limit: Duration,
    mut found: impl FnMut() -> Option<T>,
) -> Result<T> {
    let start = Instant::now();
    loop {
        if let Some(value) = found() {
            return Ok(value);
        }
        anyhow::ensure!(start.elapsed() < limit, "{what} within {limit:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// #1183 and #1185: a VM's runtime directory, its disk inside, and pasta's PID file stay
/// when fcvm is SIGKILLed, and the next launch removes them.
#[tokio::test]
async fn a_sigkilled_vm_leaves_its_runtime_dir_and_pasta_pid_file_to_the_next_launch() -> Result<()>
{
    let (name, _, _, _) = common::unique_names("reclaim-vm");
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::TEST_IMAGE,
    ])
    .await?;
    let observed = async {
        common::poll_health_by_pid(pid, 120).await?;
        let vm_id = vm_id_of(pid).await?;
        let runtime_dir = vm_disks(&vm_id);
        // Where pasta's PID file is, and where it was before it moved into the VM's
        // directory: neither may remain.
        let pid_files = [
            runtime_dir.join("pasta.pid"),
            fcvm::paths::data_dir().join(format!("pasta-{vm_id}.pid")),
        ];
        anyhow::ensure!(
            runtime_dir.join("disks/rootfs.raw").exists(),
            "the running VM has no disk in {}",
            runtime_dir.display()
        );
        anyhow::ensure!(
            pid_files.iter().any(|f| f.exists()),
            "the running rootless VM has no pasta PID file at {pid_files:?}"
        );
        Ok((runtime_dir, pid_files))
    }
    .await;
    sigkill(&mut child, pid).await?;
    let (runtime_dir, pid_files) = observed?;

    next_launch().await?;
    assert!(
        !runtime_dir.exists(),
        "the killed VM's runtime directory survived the next launch: {}",
        runtime_dir.display()
    );
    let left: Vec<_> = pid_files.iter().filter(|f| f.exists()).collect();
    assert!(
        left.is_empty(),
        "the killed VM's pasta PID file survived the next launch: {left:?}"
    );
    Ok(())
}

/// The first `.reached` file under a park directory: a process parked at the point.
fn parked(dir: &Path) -> Option<PathBuf> {
    for actor in std::fs::read_dir(dir).ok()?.flatten() {
        for event in std::fs::read_dir(actor.path()).ok()?.flatten() {
            if event.file_name().to_string_lossy().ends_with(".reached") {
                return Some(event.path());
            }
        }
    }
    None
}

/// #1183: after a SIGKILL of a restore that made its own runtime directory, the next
/// launch removes it. The Firecracker fcvm builds overrides every source path at load
/// (#1318), so the restore makes no mountpoint at the source VM's directory (#1181); the
/// reclaim of one an older binary made is covered by the unit tests in `src/reclaim.rs`.
#[cfg(feature = "integration-slow")]
#[tokio::test]
async fn a_sigkilled_restore_leaves_its_runtime_dir_to_the_next_launch() -> Result<()> {
    let (source_name, _, snapshot, _) = common::unique_names("reclaim-mount");

    let (mut source, source_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &source_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::TEST_IMAGE,
    ])
    .await?;
    let created = async {
        common::poll_health_by_pid(source_pid, 120).await?;
        let source_id = vm_id_of(source_pid).await?;
        common::create_snapshot_by_pid(source_pid, &snapshot).await?;
        Ok::<_, anyhow::Error>(source_id)
    }
    .await;
    stop(&mut source, source_pid).await?;
    let source_id = created?;
    let source_dir = vm_disks(&source_id);
    anyhow::ensure!(
        !source_dir.exists(),
        "the source VM's teardown left {}",
        source_dir.display()
    );

    let result = async {
        // A restore killed after it made its own directory, before its VMM loaded the
        // snapshot.
        let park = tempfile::tempdir()?;
        let spec = format!("restore.pre_load:park:{}", park.path().display());
        let (killed_name, _, _, _) = common::unique_names("reclaim-mount-killed");
        let (mut killed, killed_pid) = common::spawn_fcvm_with_env(
            &[
                "snapshot",
                "run",
                "--snapshot",
                &snapshot,
                "--name",
                &killed_name,
            ],
            &[("FCVM_FAILPOINT", spec.as_str())],
        )
        .await?;
        let observed = async {
            wait_for(
                "the restore did not park at restore.pre_load",
                Duration::from_secs(120),
                || parked(park.path()),
            )
            .await?;
            let killed_dir = vm_disks(&vm_id_of(killed_pid).await?);
            anyhow::ensure!(
                killed_dir.join("disks").is_dir(),
                "the parked restore has not made {}",
                killed_dir.display()
            );
            anyhow::ensure!(
                !source_dir.exists(),
                "the parked restore recreated the source VM's directory {}, which a load \
                 that overrides every source path does not need",
                source_dir.display()
            );
            Ok(killed_dir)
        }
        .await;
        sigkill(&mut killed, killed_pid).await?;
        let killed_dir = observed?;

        next_launch().await?;
        anyhow::ensure!(
            !killed_dir.exists(),
            "the killed restore's runtime directory survived the next launch: {}",
            killed_dir.display()
        );
        Ok(())
    }
    .await;
    common::delete_snapshot(&snapshot).await.ok();
    result
}

/// A `snapshot create` SIGKILLed after it staged leaves `<tag>.creating`, with the guest's
/// memory image in it, and nothing but a later create of the same tag removed it. The next
/// launch removes it. The unit tests in `src/reclaim.rs` cover a pass that finds a later
/// create of the tag using the directory.
#[cfg(feature = "integration-slow")]
#[tokio::test]
async fn a_sigkilled_snapshot_create_leaves_its_staging_directory_to_the_next_launch() -> Result<()>
{
    let (source_name, _, snapshot, _) = common::unique_names("reclaim-staging");
    let staging = fcvm::paths::snapshot_dir().join(format!("{snapshot}.creating"));
    let (mut source, source_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &source_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::TEST_IMAGE,
    ])
    .await?;
    let result = async {
        common::poll_health_by_pid(source_pid, 120).await?;
        // Killed with the snapshot complete in its staging directory, before the rename
        // that publishes it. The source VM has resumed by then.
        let park = tempfile::tempdir()?;
        let spec = format!("snapshot.install.pre_rename:park:{}", park.path().display());
        let (mut create, create_pid) = common::spawn_fcvm_with_env(
            &[
                "snapshot",
                "create",
                "--pid",
                &source_pid.to_string(),
                "--tag",
                &snapshot,
            ],
            &[("FCVM_FAILPOINT", spec.as_str())],
        )
        .await?;
        let observed = async {
            wait_for(
                "the create did not park at snapshot.install.pre_rename",
                Duration::from_secs(180),
                || parked(park.path()),
            )
            .await?;
            anyhow::ensure!(
                staging.join("memory.bin").is_file(),
                "the parked create has no memory image in {}",
                staging.display()
            );
            Ok(())
        }
        .await;
        sigkill(&mut create, create_pid).await?;
        observed?;

        next_launch().await?;
        anyhow::ensure!(
            !staging.exists(),
            "the killed create's staging directory survived the next launch: {}",
            staging.display()
        );
        Ok(())
    }
    .await;
    let stopped = stop(&mut source, source_pid).await;
    // What a failed run leaves: the staging directory, or a snapshot the create published.
    let _ = fcvm::storage::disk::remove_snapshot_dir(&staging).await;
    common::delete_snapshot(&snapshot).await.ok();
    result.and(stopped)
}

/// A snapshot directory `tag` under `<data_dir>/snapshots` that `snapshot export` accepts,
/// made without a VM: the config a create writes, a Firecracker that only prints a version,
/// and a memory image, VM state and disk of a few pages.
fn snapshot_without_a_vm(data_dir: &Path, tag: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = data_dir.join("snapshots").join(tag);
    std::fs::create_dir_all(dir.join("disk"))?;
    let firecracker = data_dir.join("firecracker");
    std::fs::write(
        &firecracker,
        "#!/bin/sh\necho 'Firecracker v0.0.0-example'\n",
    )?;
    std::fs::set_permissions(&firecracker, std::fs::Permissions::from_mode(0o755))?;

    let mut vm =
        fcvm::state::VmState::new("vm-export".to_string(), "example-image".to_string(), 1, 1);
    vm.config.source_vsock_socket_path = Some(PathBuf::from("/run/example-vsock/vsock.sock"));
    let mut config = fcvm::commands::common::build_snapshot_config(
        &vm,
        tag,
        fcvm::storage::SnapshotType::User,
        &dir,
        Vec::new(),
        Vec::new(),
        None,
    )?;
    config.metadata.firecracker_bin = Some(firecracker);
    std::fs::write(dir.join("config.json"), serde_json::to_vec_pretty(&config)?)?;
    std::fs::write(dir.join("memory.bin"), vec![1u8; 1 << 20])?;
    std::fs::write(dir.join("vmstate.bin"), vec![2u8; 4096])?;
    std::fs::write(dir.join("disk/disk.raw"), vec![3u8; 8192])?;
    Ok(())
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

/// `snapshot export --output` writes a temporary beside its output. An export killed before
/// it renamed the temporary leaves it, with the output's name still unused, and the next
/// export to a file removes it before it writes its own.
#[tokio::test]
async fn a_sigkilled_export_leaves_its_temporary_to_the_next_export() -> Result<()> {
    let data = tempfile::tempdir()?;
    let tag = "reclaim-export";
    snapshot_without_a_vm(data.path(), tag)?;
    let out = tempfile::tempdir()?;
    let output = out.path().join("snapshot.archive");
    let park = tempfile::tempdir()?;
    let spec = format!("snapshot.export.pre_rename:park:{}", park.path().display());
    let export = || {
        let mut command = tokio::process::Command::new(common::find_fcvm_binary().unwrap());
        command
            .args(["snapshot", "export", tag, "--output"])
            .arg(&output)
            .env("FCVM_DATA_DIR", data.path());
        common::set_test_pdeathsig(&mut command);
        command
    };

    let mut killed = export()
        .env("FCVM_FAILPOINT", &spec)
        .spawn()
        .context("spawning the export to kill")?;
    let killed_pid = killed.id().context("the export has no pid")?;
    let observed = async {
        wait_for(
            "the export did not park at snapshot.export.pre_rename",
            Duration::from_secs(120),
            || parked(park.path()),
        )
        .await?;
        let names = names_in(out.path())?;
        anyhow::ensure!(
            names.len() == 1 && names[0].ends_with(".tmp"),
            "the parked export has not written exactly one temporary beside its output: \
             {names:?}"
        );
        Ok(out.path().join(&names[0]))
    }
    .await;
    sigkill(&mut killed, killed_pid).await?;
    let temporary = observed?;
    anyhow::ensure!(
        temporary.exists(),
        "the killed export's temporary is already gone, so the kill ran its cleanup"
    );

    let status = export().status().await.context("running the next export")?;
    anyhow::ensure!(status.success(), "the next export failed: {status}");
    assert_eq!(
        names_in(out.path())?,
        ["snapshot.archive"],
        "the killed export's temporary survived the next export"
    );
    Ok(())
}

/// Every `fcvm snapshot export` and `fcvm snapshot import` the tests start by hand carries
/// the parent-death signal (CLAUDE.md, "PROCESS TEARDOWN IS PER-HOP"): one that outlived a
/// test nextest killed would keep its snapshot's lock. A command is armed between its
/// arguments and the call that runs it, or it is built by this file's `fcvm` closure, each
/// of which arms what it returns.
#[test]
fn every_hand_built_export_and_import_command_dies_with_its_test() {
    const ARMED: &str = "common::set_test_pdeathsig(&mut ";
    const RUNS: [&str; 3] = ["spawn()", ".status()", ".output()"];
    // Per file: how many places build an export command, and an import command.
    for (file, source, exports, imports) in [
        ("test_reclaim.rs", include_str!("test_reclaim.rs"), 4, 3),
        (
            "test_lifecycle_interleave.rs",
            include_str!("test_lifecycle_interleave.rs"),
            1,
            0,
        ),
        ("test_pmem.rs", include_str!("test_pmem.rs"), 1, 1),
        (
            "test_snapshot_clone.rs",
            include_str!("test_snapshot_clone.rs"),
            1,
            1,
        ),
        (
            "test_localhost_image.rs",
            include_str!("test_localhost_image.rs"),
            1,
            1,
        ),
    ] {
        for (subcommand, expected) in [("export", exports), ("import", imports)] {
            let args = format!(r#"["snapshot", "{subcommand}","#);
            let built: Vec<usize> = source.match_indices(&args).map(|(at, _)| at).collect();
            assert_eq!(
                built.len(),
                expected,
                "{file}: the places that build a snapshot {subcommand} command changed"
            );
            for at in built {
                let rest = &source[at..];
                let run = RUNS
                    .iter()
                    .filter_map(|run| rest.find(run))
                    .min()
                    .unwrap_or_else(|| panic!("{file}: a snapshot {subcommand} is never run"));
                let by_closure = source[..at].ends_with("fcvm(&");
                assert!(
                    rest[..run].contains(ARMED) || by_closure,
                    "{file}: a snapshot {subcommand} command is built without \
                     set_test_pdeathsig:\n{}",
                    &rest[..run]
                );
            }
        }
        // The closure that builds the other commands arms each one.
        // In two pieces, so this line is not one of the closures it finds.
        for closure in source
            .split(concat!("let fcvm = ", "|args: &[&str]| {"))
            .skip(1)
        {
            let body = closure.split("\n    };").next().unwrap();
            assert!(
                body.contains(ARMED),
                "{file}: an fcvm closure builds commands without set_test_pdeathsig:\n{body}"
            );
        }
    }
}

/// `snapshot import` stages into `<tag>.creating`. An import killed before the rename that
/// publishes it leaves that directory, with the memory image in it, and no tag. The next
/// import removes it before it stages its own. The next import is of another tag, so what
/// removes the directory is the reclaim and not the staging of the same name.
#[tokio::test]
async fn a_sigkilled_import_leaves_its_staging_directory_to_the_next_import() -> Result<()> {
    let data = tempfile::tempdir()?;
    let archives = tempfile::tempdir()?;
    let snapshots = data.path().join("snapshots");
    let (killed_tag, next_tag) = ("reclaim-import-killed", "reclaim-import-next");
    let archive_of = |tag: &str| archives.path().join(format!("{tag}.archive"));
    let fcvm = |args: &[&str]| {
        let mut command = tokio::process::Command::new(common::find_fcvm_binary().unwrap());
        command.args(args).env("FCVM_DATA_DIR", data.path());
        // An import parked at its failpoint must not outlive a test that nextest killed.
        common::set_test_pdeathsig(&mut command);
        command
    };
    for tag in [killed_tag, next_tag] {
        snapshot_without_a_vm(data.path(), tag)?;
        let status = fcvm(&["snapshot", "export", tag, "--output"])
            .arg(archive_of(tag))
            .status()
            .await
            .context("exporting a snapshot to import")?;
        anyhow::ensure!(status.success(), "the export of {tag} failed: {status}");
        std::fs::remove_dir_all(snapshots.join(tag))?;
    }
    let import = |tag: &str| {
        let mut command = fcvm(&["snapshot", "import", "--input"]);
        command.arg(archive_of(tag));
        command
    };
    let staging = snapshots.join(format!("{killed_tag}.creating"));

    // Killed with the snapshot whole in its staging directory, verified and synced, before
    // the rename that publishes it.
    let park = tempfile::tempdir()?;
    let spec = format!("snapshot.import.pre_publish:park:{}", park.path().display());
    let mut killed = import(killed_tag)
        .env("FCVM_FAILPOINT", &spec)
        .spawn()
        .context("spawning the import to kill")?;
    let killed_pid = killed.id().context("the import has no pid")?;
    let observed = async {
        wait_for(
            "the import did not park at snapshot.import.pre_publish",
            Duration::from_secs(120),
            || parked(park.path()),
        )
        .await?;
        anyhow::ensure!(
            staging.join("memory.bin").is_file() && staging.join("config.json").is_file(),
            "the parked import has not staged its snapshot in {}",
            staging.display()
        );
        anyhow::ensure!(
            !snapshots.join(killed_tag).exists(),
            "the parked import has published its tag before the rename"
        );
        Ok(())
    }
    .await;
    sigkill(&mut killed, killed_pid).await?;
    observed?;
    anyhow::ensure!(
        staging.exists(),
        "the killed import's staging directory is already gone, so the kill ran its cleanup"
    );

    let status = import(next_tag)
        .status()
        .await
        .context("running the next import")?;
    anyhow::ensure!(status.success(), "the next import failed: {status}");
    anyhow::ensure!(
        !staging.exists(),
        "the killed import's staging directory survived the next import: {}",
        staging.display()
    );
    anyhow::ensure!(
        !snapshots.join(killed_tag).exists(),
        "the killed import's tag exists"
    );
    anyhow::ensure!(
        snapshots.join(next_tag).join("config.json").is_file(),
        "the next import did not install its snapshot"
    );
    Ok(())
}

/// `snapshot import` stages a pmem store entry its host lacks in the store's `tmp/`, and
/// publishes it only once the archive has verified. An import killed with the entry staged
/// leaves that temp and its staging directory, and no entry. The next import, of a snapshot
/// without pmem images, removes both before it stages its own.
#[tokio::test]
async fn a_sigkilled_import_leaves_its_staged_pmem_entry_to_the_next_import() -> Result<()> {
    use std::os::unix::fs::FileExt;
    let data = tempfile::tempdir()?;
    let archives = tempfile::tempdir()?;
    let snapshots = data.path().join("snapshots");
    let (killed_tag, next_tag) = ("reclaim-import-pmem-killed", "reclaim-import-pmem-next");
    let archive_of = |tag: &str| archives.path().join(format!("{tag}.archive"));
    let fcvm = |args: &[&str]| {
        let mut command = tokio::process::Command::new(common::find_fcvm_binary().unwrap());
        command.args(args).env("FCVM_DATA_DIR", data.path());
        // An import parked at its failpoint must not outlive a test that nextest killed.
        common::set_test_pdeathsig(&mut command);
        command
    };

    // The snapshot to kill the import of maps one pmem image: an entry of the store under
    // this test's data directory, published as a `--pmem` run publishes it. The store
    // refuses an image on tmpfs, which the system temp directory can be, so the image is
    // under fcvm's data directory.
    let images = tempfile::tempdir_in(fcvm::paths::data_dir())?;
    let image = images.path().join("model.img");
    {
        let file = std::fs::File::create(&image)?;
        file.set_len(fcvm::storage::pmem::PMEM_ALIGNMENT)?;
        file.write_all_at(&[7u8; 8192], 4096)?;
    }
    for tag in [killed_tag, next_tag] {
        snapshot_without_a_vm(data.path(), tag)?;
    }
    let store = fcvm::storage::pmem_store::PmemStore::open(data.path())?;
    let device = fcvm::storage::pmem::resolve_pmem_devices(
        &store,
        &[format!("{}:/models:ro", image.display())],
        &[],
    )?
    .remove(0);
    let entry = PathBuf::from(&device.path);
    let store_tmp = store.dir().join("tmp");
    drop(store);
    {
        let config_path = snapshots.join(killed_tag).join("config.json");
        let mut config: fcvm::storage::SnapshotConfig =
            serde_json::from_slice(&std::fs::read(&config_path)?)?;
        config.metadata.pmem_devices = vec![device];
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    }
    for tag in [killed_tag, next_tag] {
        let status = fcvm(&["snapshot", "export", tag, "--output"])
            .arg(archive_of(tag))
            .status()
            .await
            .context("exporting a snapshot to import")?;
        anyhow::ensure!(status.success(), "the export of {tag} failed: {status}");
        std::fs::remove_dir_all(snapshots.join(tag))?;
    }
    // The importing host never had the entry.
    std::fs::remove_file(&entry)?;
    anyhow::ensure!(
        names_in(&store_tmp)?.is_empty(),
        "control: the store has a temp before the import: {:?}",
        names_in(&store_tmp)?
    );

    let import = |tag: &str| {
        let mut command = fcvm(&["snapshot", "import", "--input"]);
        command.arg(archive_of(tag));
        command
    };
    let staging = snapshots.join(format!("{killed_tag}.creating"));

    // Killed with the archive verified and the entry whole in the store's temp directory,
    // before anything of the import has a name outside its staging directory.
    let park = tempfile::tempdir()?;
    let spec = format!(
        "snapshot.import.externals_staged:park:{}",
        park.path().display()
    );
    let mut killed = import(killed_tag)
        .env("FCVM_FAILPOINT", &spec)
        .spawn()
        .context("spawning the import to kill")?;
    let killed_pid = killed.id().context("the import has no pid")?;
    let observed = async {
        // An import that ends instead of parking is reported at once, with its status.
        wait_for(
            "the import did not park at snapshot.import.externals_staged",
            Duration::from_secs(60),
            || {
                parked(park.path())
                    .map(|_| Ok(()))
                    .or_else(|| killed.try_wait().ok().flatten().map(Err))
            },
        )
        .await?
        .map_err(|status| anyhow::anyhow!("the import ended before it parked: {status}"))?;
        let temps = names_in(&store_tmp)?;
        anyhow::ensure!(
            temps.len() == 1,
            "the parked import has not staged exactly one pmem entry in {}: {temps:?}",
            store_tmp.display()
        );
        anyhow::ensure!(
            !entry.exists(),
            "the parked import has published its pmem entry before the failpoint"
        );
        anyhow::ensure!(
            staging.join("memory.bin").is_file() && !snapshots.join(killed_tag).exists(),
            "the parked import has not staged its snapshot in {}, or has published it",
            staging.display()
        );
        Ok(())
    }
    .await;
    let reaped = sigkill(&mut killed, killed_pid).await;
    observed?;
    reaped?;
    anyhow::ensure!(
        names_in(&store_tmp)?.len() == 1 && staging.exists(),
        "the killed import's temp or staging directory is already gone, so the kill ran \
         its cleanup"
    );

    let status = import(next_tag)
        .status()
        .await
        .context("running the next import")?;
    anyhow::ensure!(status.success(), "the next import failed: {status}");
    assert_eq!(
        names_in(&store_tmp)?,
        Vec::<String>::new(),
        "the killed import's staged pmem entry survived the next import"
    );
    anyhow::ensure!(
        !staging.exists(),
        "the killed import's staging directory survived the next import: {}",
        staging.display()
    );
    anyhow::ensure!(
        !entry.exists() && !snapshots.join(killed_tag).exists(),
        "the killed import published its pmem entry or its tag"
    );
    anyhow::ensure!(
        snapshots.join(next_tag).join("config.json").is_file(),
        "the next import did not install its snapshot"
    );
    Ok(())
}

/// `snapshot import` stages an image disk its host lacks in a temporary beside the disk's
/// path in the image cache, recorded for reclaim before the file exists, and publishes it
/// only once the archive has verified. An import killed with the disk staged leaves that
/// temporary and its record. The next import, of a snapshot without an image disk, removes
/// both before it stages its own.
///
/// The image cache is the configured one. Its place comes from the config's assets
/// directory, with the kernels and the rootfs, and not from `FCVM_DATA_DIR`, so this test
/// cannot give the fcvm it runs a cache of its own without a config of its own. It uses a
/// disk name no image has, and removes what it made there. The reclaim record is under the
/// test's data directory, where no other test's launch finds it.
#[tokio::test]
async fn a_sigkilled_import_leaves_its_staged_image_disk_to_the_next_import() -> Result<()> {
    let data = tempfile::tempdir()?;
    let archives = tempfile::tempdir()?;
    let snapshots = data.path().join("snapshots");
    let (killed_tag, next_tag) = ("reclaim-import-disk-killed", "reclaim-import-disk-next");
    let archive_of = |tag: &str| archives.path().join(format!("{tag}.archive"));
    let fcvm = |args: &[&str]| {
        let mut command = tokio::process::Command::new(common::find_fcvm_binary().unwrap());
        command.args(args).env("FCVM_DATA_DIR", data.path());
        // An import parked at its failpoint must not outlive a test that nextest killed.
        common::set_test_pdeathsig(&mut command);
        command
    };

    // A Docker archive disk, the kind a snapshot records no identity for, under the digest
    // of no image.
    let image_cache = fcvm::paths::image_cache_dir();
    std::fs::create_dir_all(&image_cache)?;
    let digest = uuid::Uuid::new_v4().simple().to_string();
    let disk = image_cache.join(format!("{digest}.docker.tar"));
    let of_this_test = || -> Result<Vec<String>> {
        Ok(names_in(&image_cache)?
            .into_iter()
            .filter(|name| name.starts_with(&digest))
            .collect())
    };
    let result = async {
        std::fs::write(&disk, vec![9u8; 64 * 1024])?;
        for tag in [killed_tag, next_tag] {
            snapshot_without_a_vm(data.path(), tag)?;
        }
        {
            let config_path = snapshots.join(killed_tag).join("config.json");
            let mut config: fcvm::storage::SnapshotConfig =
                serde_json::from_slice(&std::fs::read(&config_path)?)?;
            config.metadata.image_disk_path = Some(disk.clone());
            std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
        }
        for tag in [killed_tag, next_tag] {
            let status = fcvm(&["snapshot", "export", tag, "--output"])
                .arg(archive_of(tag))
                .status()
                .await
                .context("exporting a snapshot to import")?;
            anyhow::ensure!(status.success(), "the export of {tag} failed: {status}");
            std::fs::remove_dir_all(snapshots.join(tag))?;
        }
        // The importing host never had the disk.
        std::fs::remove_file(&disk)?;
        anyhow::ensure!(
            of_this_test()?.is_empty(),
            "control: the image cache holds something of this test before the import: {:?}",
            of_this_test()?
        );

        let import = |tag: &str| {
            let mut command = fcvm(&["snapshot", "import", "--input"]);
            command.arg(archive_of(tag));
            command
        };
        let owners = fcvm::reclaim::owner_dir_in(data.path())?;
        let temp_records = || -> Result<Vec<String>> {
            Ok(names_in(&owners)?
                .into_iter()
                .filter(|name| name.starts_with("store-temp-") && name.ends_with(".json"))
                .collect())
        };

        // Killed with the archive verified and the disk whole in its temporary, before
        // anything of the import has a name outside its staging directory.
        let park = tempfile::tempdir()?;
        let spec = format!(
            "snapshot.import.externals_staged:park:{}",
            park.path().display()
        );
        let mut killed = import(killed_tag)
            .env("FCVM_FAILPOINT", &spec)
            .spawn()
            .context("spawning the import to kill")?;
        let killed_pid = killed.id().context("the import has no pid")?;
        let observed = async {
            // An import that ends instead of parking is reported at once, with its status.
            wait_for(
                "the import did not park at snapshot.import.externals_staged",
                Duration::from_secs(60),
                || {
                    parked(park.path())
                        .map(|_| Ok(()))
                        .or_else(|| killed.try_wait().ok().flatten().map(Err))
                },
            )
            .await?
            .map_err(|status| anyhow::anyhow!("the import ended before it parked: {status}"))?;
            let staged = of_this_test()?;
            anyhow::ensure!(
                staged.len() == 1 && staged[0].ends_with(".tmp") && !disk.exists(),
                "the parked import has not staged exactly one temporary of its image disk \
                 in {}, or has published the disk: {staged:?}",
                image_cache.display()
            );
            anyhow::ensure!(
                temp_records()?.len() == 1,
                "the staged image disk has no reclaim record: {:?}",
                names_in(&owners)?
            );
            Ok(())
        }
        .await;
        let reaped = sigkill(&mut killed, killed_pid).await;
        observed?;
        reaped?;
        anyhow::ensure!(
            of_this_test()?.len() == 1,
            "the killed import's temporary is already gone, so the kill ran its cleanup"
        );

        let status = import(next_tag)
            .status()
            .await
            .context("running the next import")?;
        anyhow::ensure!(status.success(), "the next import failed: {status}");
        // Errors, not panics: what is left in the image cache is removed below.
        anyhow::ensure!(
            of_this_test()?.is_empty(),
            "the killed import's staged image disk survived the next import: {:?}",
            of_this_test()?
        );
        anyhow::ensure!(
            temp_records()?.is_empty(),
            "the record of the killed import's staged image disk survived the next import: {:?}",
            temp_records()?
        );
        anyhow::ensure!(
            !snapshots.join(killed_tag).exists(),
            "the killed import published its tag"
        );
        anyhow::ensure!(
            snapshots.join(next_tag).join("config.json").is_file(),
            "the next import did not install its snapshot"
        );
        Ok(())
    }
    .await;
    for name in of_this_test().unwrap_or_default() {
        let _ = std::fs::remove_file(image_cache.join(name));
    }
    result
}

/// Build a localhost image whose content, and so whose digest, is this test's own, and
/// return its image-cache key (the manifest digest, or the image id when there is none).
async fn unique_localhost_image(name: &str) -> Result<String> {
    let context = tempfile::tempdir()?;
    std::fs::write(
        context.path().join("Containerfile"),
        format!(
            "FROM {}\nLABEL fcvm.reclaim.test=\"{}\"\n",
            common::TEST_IMAGE,
            uuid::Uuid::new_v4()
        ),
    )?;
    let built = tokio::process::Command::new("podman")
        .args(["build", "-q", "-t", name])
        .arg(context.path())
        .output()
        .await
        .context("running podman build")?;
    anyhow::ensure!(
        built.status.success(),
        "podman build failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let inspected = tokio::process::Command::new("podman")
        .args(["image", "inspect", name, "--format", "{{.Digest}}\t{{.Id}}"])
        .output()
        .await
        .context("running podman image inspect")?;
    anyhow::ensure!(inspected.status.success(), "podman image inspect failed");
    // Only the line end: a locally built image has an empty digest, so the line starts
    // with the tab.
    let line = String::from_utf8_lossy(&inspected.stdout)
        .trim_end()
        .to_string();
    let (digest, id) = line
        .split_once('\t')
        .with_context(|| format!("unexpected inspect output {line:?}"))?;
    let key = if digest.is_empty() { id } else { digest };
    Ok(key.trim_start_matches("sha256:").to_string())
}

/// Image-cache temporaries of `key`: names beginning with it and ending in `.tmp`.
fn cache_temps(key: &str) -> Vec<PathBuf> {
    std::fs::read_dir(fcvm::paths::image_cache_dir())
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().unwrap_or_default().to_string_lossy();
                    name.starts_with(key) && name.ends_with(".tmp")
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every temporary a record of the fcvm process `pid` lists, in the owner directory `dir`.
fn recorded_temps(dir: &Path, pid: u32) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut temps = Vec::new();
    for entry in entries.flatten() {
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if record.get("pid").and_then(|p| p.as_u64()) != Some(u64::from(pid)) {
            continue;
        }
        for resource in record["resources"].as_array().into_iter().flatten() {
            if resource["kind"] == "temp" {
                if let Some(path) = resource["path"].as_str() {
                    temps.push(PathBuf::from(path));
                }
            }
        }
    }
    temps
}

/// The lock files of the records the fcvm process `pid` wrote in the owner directory `dir`.
fn record_locks(dir: &Path, pid: u32) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            (record.get("pid").and_then(|p| p.as_u64()) == Some(u64::from(pid)))
                .then(|| entry.path().with_extension("lock"))
        })
        .collect()
}

/// Wait until no process holds any of `locks`. A helper of the killed fcvm (skopeo,
/// mkfs.ext4) inherits its owner's lock and keeps writing until it exits; until then the
/// reclaim leaves its temporaries alone, as it must.
async fn wait_for_helpers(locks: &[PathBuf]) -> Result<()> {
    wait_for(
        "a helper of the killed fcvm still holds its owner lock",
        Duration::from_secs(180),
        || {
            locks
                .iter()
                .all(|lock| match std::fs::File::open(lock) {
                    Ok(file) => fs2::FileExt::try_lock_exclusive(&file).is_ok(),
                    Err(_) => true,
                })
                .then_some(())
        },
    )
    .await
}

/// #1186: a SIGKILLed export of a localhost image leaves its temporary archive, and a
/// SIGKILLed storage image build leaves its temporary store and image. The next launch
/// removes them.
///
/// The killed fcvm and the next launch use a data dir of their own, so their owner records
/// are where no other test's launch reclaims them. With the shared data dir any VM launch of
/// the parallel suite may take the records once the killed build's helpers exit: in
/// CI 40897395580 another test's launch reclaimed the killed build's three temporaries
/// ("owners=1 resources=3") 0.1 s before this test checked that the store was still there
/// (#1436). That pass is made here on purpose, at that moment, as every launch of the suite
/// does it, and it must leave this test's records alone.
#[cfg(feature = "integration-slow")]
#[tokio::test]
async fn a_sigkilled_image_export_leaves_its_temporaries_to_the_next_launch() -> Result<()> {
    let image = format!("localhost/fcvm-reclaim-{}", uuid::Uuid::new_v4().simple());
    let key = unique_localhost_image(&image).await?;
    // Short: a VM's sockets sit under <data>/vm-disks/vm-<32 hex>/, and AF_UNIX paths stop at
    // 108 bytes.
    let data_dir = fcvm::paths::data_dir().join(format!(
        "rt-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let data_dir_str = data_dir.to_str().context("data dir path")?.to_string();
    let env = [("FCVM_DATA_DIR", data_dir_str.as_str())];
    let owners = fcvm::reclaim::owner_dir_in(&data_dir)?;
    let result = async {
        // Killed while podman writes the archive.
        let (name, _, _, _) = common::unique_names("reclaim-export");
        let (mut child, pid) = common::spawn_fcvm_with_env(
            &["podman", "run", "--name", &name, "--no-snapshot", &image],
            &env,
        )
        .await?;
        let observed = wait_for(
            "no temporary archive appeared",
            Duration::from_secs(120),
            || {
                cache_temps(&key)
                    .into_iter()
                    .find(|p| p.to_string_lossy().contains(".docker.tar."))
            },
        )
        .await;
        let locks = record_locks(&owners, pid);
        sigkill(&mut child, pid).await?;
        let archive_temp = observed?;
        wait_for_helpers(&locks).await?;
        next_launch_with("rootless", &env).await?;
        anyhow::ensure!(
            !archive_temp.exists(),
            "the killed export's archive survived the next launch: {}",
            archive_temp.display()
        );

        // Killed while the storage image is built from the published archive.
        let (name, _, _, _) = common::unique_names("reclaim-storage");
        let (mut child, pid) = common::spawn_fcvm_with_env(
            &["podman", "run", "--name", &name, "--no-snapshot", &image],
            &env,
        )
        .await?;
        let observed = wait_for(
            "no temporary storage image appeared",
            Duration::from_secs(180),
            || {
                cache_temps(&key)
                    .into_iter()
                    .find(|p| p.to_string_lossy().contains(".storage-"))
            },
        )
        .await;
        let recorded = recorded_temps(&owners, pid);
        let locks = record_locks(&owners, pid);
        sigkill(&mut child, pid).await?;
        wait_for_helpers(&locks).await?;
        // What any other launch of the suite may do now that the helpers are gone.
        fcvm::reclaim::reclaim_abandoned().await;
        let storage_temp = observed?;
        let store = recorded.iter().find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("tmp-storage-"))
        });
        anyhow::ensure!(
            store.is_some_and(|s| s.is_dir()),
            "no record of the killed build lists an existing temporary store: {recorded:?}"
        );
        next_launch_with("rootless", &env).await?;
        let left: Vec<_> = std::iter::once(&storage_temp)
            .chain(recorded.iter())
            .filter(|p| p.exists())
            .collect();
        anyhow::ensure!(
            left.is_empty(),
            "the killed build's temporaries survived the next launch: {left:?}"
        );
        Ok(())
    }
    .await;
    let _ = tokio::process::Command::new("podman")
        .args(["rmi", "-f", &image])
        .output()
        .await;
    let _ = std::fs::remove_dir_all(&data_dir);
    // The image's cache entries: no other test uses this digest.
    for entry in std::fs::read_dir(fcvm::paths::image_cache_dir())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&key) && !name.ends_with(".lock") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    result
}

#[cfg(feature = "privileged-tests")]
fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program} {args:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The /30 peer of a bridged veth's host-end address (`.1` to `.2`).
#[cfg(feature = "privileged-tests")]
fn peer_of(host_ip: &str) -> String {
    let (prefix, last) = host_ip.rsplit_once('.').expect("an IPv4 address");
    format!(
        "{prefix}.{}",
        last.parse::<u8>().expect("an IPv4 address") + 1
    )
}

#[cfg(feature = "privileged-tests")]
/// The host objects of one bridged VM, read from its state.
#[derive(Debug)]
struct BridgedObjects {
    namespace: String,
    host_veth: String,
    host_ip: String,
    /// A clone's host route to its guest address: `(guest_ip, via)`.
    route: Option<(String, String)>,
}

#[cfg(feature = "privileged-tests")]
impl BridgedObjects {
    async fn of(pid: u32, clone: bool) -> Result<Self> {
        let vm = listed(pid).await?;
        let network = &vm["config"]["network"];
        let field = |name: &str| {
            network[name]
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("no {name} in {network}"))
        };
        let host_veth = field("host_veth")?;
        let host_ip = field("host_ip")?;
        let base = host_veth
            .strip_prefix("veth0-")
            .with_context(|| format!("unexpected host veth {host_veth}"))?;
        let route = if clone {
            // The namespace end of a clone's veth is the host end's /30 peer.
            Some((field("guest_ip")?, peer_of(&host_ip)))
        } else {
            None
        };
        Ok(Self {
            namespace: format!("fcvm-{base}"),
            host_veth,
            host_ip,
            route,
        })
    }

    /// Every object of this VM that is still on the host.
    fn present(&self) -> Result<Vec<String>> {
        let mut present = Vec::new();
        if Path::new("/var/run/netns").join(&self.namespace).exists() {
            present.push(format!("namespace {}", self.namespace));
        }
        if Path::new("/sys/class/net").join(&self.host_veth).exists() {
            present.push(format!("interface {}", self.host_veth));
        }
        let rules = command_output("iptables-save", &[])?;
        for rule in rules.lines() {
            // Token-exact, so that 172.30.0.1 does not match 172.30.0.13.
            // The host end's address and its /30 peer: the guest of a baseline and the
            // namespace end of a clone's veth, which the published ports target.
            let peer = peer_of(&self.host_ip);
            let named = rule.split_whitespace().any(|token| {
                token == self.host_veth
                    || [&self.host_ip, &peer].into_iter().any(|ip| {
                        token == ip.as_str()
                            || token == format!("{ip}/32")
                            || token.starts_with(&format!("{ip}:"))
                    })
            });
            if named {
                present.push(format!("rule {rule}"));
            }
        }
        if let Some((guest_ip, via)) = &self.route {
            let routes = command_output("ip", &["route", "show", &format!("{guest_ip}/32")])?;
            if routes.split_whitespace().any(|token| token == via) {
                present.push(format!("route {guest_ip}/32 via {via}"));
            }
        }
        Ok(present)
    }
}

/// #1184: a bridged VM's namespace, veth, FORWARD and DNAT rules, and a bridged clone's
/// host route stay when fcvm is SIGKILLed, and the next launch removes them.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn a_sigkilled_bridged_vm_leaves_its_network_to_the_next_launch() -> Result<()> {
    let (name, clone_name, snapshot, _) = common::unique_names("reclaim-bridged");
    // Two host ports on one guest port: their MASQUERADE and FORWARD rules are the
    // same rule twice.
    let port = common::find_available_high_port()?;
    let publish = format!("{port}:80");
    let second_port = common::find_available_port(port + 1, 1000)?;
    let second_publish = format!("{second_port}:80");
    let (mut baseline, baseline_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &name,
        "--network",
        "bridged",
        "--no-snapshot",
        "--publish",
        &publish,
        "--publish",
        &second_publish,
        common::TEST_IMAGE,
    ])
    .await?;
    let mut clone = None;
    let observed = async {
        common::poll_health_by_pid(baseline_pid, 120).await?;
        let baseline_objects = BridgedObjects::of(baseline_pid, false).await?;
        common::create_snapshot_by_pid(baseline_pid, &snapshot).await?;
        let (child, clone_pid) = common::spawn_fcvm(&[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot,
            "--name",
            &clone_name,
        ])
        .await?;
        clone = Some((child, clone_pid));
        common::poll_health_by_pid(clone_pid, 120).await?;
        let clone_objects = BridgedObjects::of(clone_pid, true).await?;
        for objects in [&baseline_objects, &clone_objects] {
            let present = objects.present()?;
            // Namespace, veth, its FORWARD rule, and for the baseline four rules per
            // published port and for the clone its route.
            anyhow::ensure!(
                present.iter().any(|p| p.starts_with("namespace"))
                    && present.iter().any(|p| p.starts_with("interface"))
                    && present.iter().any(|p| p.starts_with("rule")),
                "the running VM's network is not all there: {present:?} ({objects:?})"
            );
        }
        anyhow::ensure!(
            baseline_objects
                .present()?
                .iter()
                .any(|p| p.contains("DNAT")),
            "the baseline has no DNAT rule for --publish {publish}"
        );
        anyhow::ensure!(
            clone_objects
                .present()?
                .iter()
                .any(|p| p.starts_with("route")),
            "the clone has no host route to its guest"
        );
        Ok((baseline_objects, clone_objects))
    }
    .await;
    sigkill(&mut baseline, baseline_pid).await?;
    if let Some((mut child, pid)) = clone.take() {
        sigkill(&mut child, pid).await?;
    }
    let result = async {
        let (baseline_objects, clone_objects) = observed?;
        next_launch_on("bridged").await?;
        for objects in [&baseline_objects, &clone_objects] {
            let present = objects.present()?;
            anyhow::ensure!(
                present.is_empty(),
                "the killed VM's network survived the next launch: {present:?}"
            );
        }
        Ok(())
    }
    .await;
    common::delete_snapshot(&snapshot).await.ok();
    result
}

/// #1184: a routed VM's namespace, veth, and the proxy NDP entry and MASQUERADE rule
/// its setup managed to add (a host whose interface refuses proxy NDP, or a run with
/// `--ipv6-prefix`, has neither) stay when fcvm is SIGKILLed, and the next launch removes
/// them.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn a_sigkilled_routed_vm_leaves_its_network_to_the_next_launch() -> Result<()> {
    let (name, _, _, _) = common::unique_names("reclaim-routed");
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &name,
        "--network",
        "routed",
        "--no-snapshot",
        common::TEST_IMAGE,
    ])
    .await?;
    let observed = async {
        common::poll_health_by_pid(pid, 120).await?;
        let vm = listed(pid).await?;
        let network = &vm["config"]["network"];
        let field = |name: &str| {
            network[name]
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("no {name} in {network}"))
        };
        Ok::<_, anyhow::Error>((
            field("namespace_name")?,
            field("host_veth")?,
            // Recorded with its prefix length.
            field("guest_ipv6")?.trim_end_matches("/128").to_string(),
        ))
    }
    .await;
    // Every routed object this VM's setup created and its teardown removes.
    let present =
        |(namespace, host_veth, address): &(String, String, String)| -> Result<Vec<String>> {
            let mut present = Vec::new();
            if Path::new("/var/run/netns").join(namespace).exists() {
                present.push(format!("namespace {namespace}"));
            }
            if Path::new("/sys/class/net").join(host_veth).exists() {
                present.push(format!("interface {host_veth}"));
            }
            let proxies = command_output("ip", &["-6", "neigh", "show", "proxy"])?;
            if proxies.split_whitespace().any(|token| token == address) {
                present.push(format!("proxy NDP entry {address}"));
            }
            let source = format!("{address}/128");
            let rules = command_output("ip6tables-save", &["-t", "nat"])?;
            for rule in rules.lines() {
                if rule.split_whitespace().any(|token| token == source) {
                    present.push(format!("rule {rule}"));
                }
            }
            Ok(present)
        };
    let before = observed.as_ref().ok().map(present);
    sigkill(&mut child, pid).await?;
    let objects = observed?;
    let before = before.context("the routed VM's objects were not read")??;
    println!("routed objects before the kill: {before:?}");
    anyhow::ensure!(
        before.iter().any(|p| p.starts_with("namespace"))
            && before.iter().any(|p| p.starts_with("interface")),
        "the running routed VM's network is not all there: {before:?}"
    );
    next_launch_on("routed").await?;
    let after = present(&objects)?;
    anyhow::ensure!(
        after.is_empty(),
        "the killed routed VM's network survived the next launch: {after:?}"
    );
    Ok(())
}
