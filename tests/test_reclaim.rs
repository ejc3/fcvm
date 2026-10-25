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
    let (name, _, _, _) = common::unique_names("reclaim-next");
    let (mut child, _) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &name,
        "--network",
        network,
        "--no-snapshot",
        common::TEST_IMAGE,
        "true",
    ])
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

/// #1181: a restore of a snapshot whose source VM is gone recreates the source's
/// directory as a mountpoint, and the last lease on it removes it (#1304). After a
/// SIGKILL of a restore that made it, with the clone's own runtime directory (#1183), the
/// next launch removes both.
#[cfg(feature = "integration-slow")]
#[tokio::test]
async fn a_sigkilled_restore_leaves_its_mountpoint_and_runtime_dir_to_the_next_launch() -> Result<()>
{
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
        // A restore killed after it made the mountpoint and its own directory, before
        // its VMM loaded the snapshot.
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
                source_dir.is_dir() && killed_dir.join("disks").is_dir(),
                "the parked restore has not made {} and {}",
                source_dir.display(),
                killed_dir.display()
            );
            Ok(killed_dir)
        }
        .await;
        sigkill(&mut killed, killed_pid).await?;
        let killed_dir = observed?;

        next_launch().await?;
        anyhow::ensure!(
            !source_dir.exists(),
            "the killed restore's mountpoint survived the next launch: {}",
            source_dir.display()
        );
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

/// Every temporary a record of the fcvm process `pid` lists, in this user's owner
/// directory.
fn recorded_temps(pid: u32) -> Vec<PathBuf> {
    let dir = fcvm::paths::data_dir()
        .join("reclaim")
        .join(nix::unistd::geteuid().as_raw().to_string());
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

/// #1186: a SIGKILLed export of a localhost image leaves its temporary archive, and a
/// SIGKILLed storage image build leaves its temporary store and image. The next launch
/// removes them.
#[cfg(feature = "integration-slow")]
#[tokio::test]
async fn a_sigkilled_image_export_leaves_its_temporaries_to_the_next_launch() -> Result<()> {
    let image = format!("localhost/fcvm-reclaim-{}", uuid::Uuid::new_v4().simple());
    let key = unique_localhost_image(&image).await?;
    let result = async {
        // Killed while podman writes the archive.
        let (name, _, _, _) = common::unique_names("reclaim-export");
        let (mut child, pid) =
            common::spawn_fcvm(&["podman", "run", "--name", &name, "--no-snapshot", &image])
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
        sigkill(&mut child, pid).await?;
        let archive_temp = observed?;
        next_launch().await?;
        anyhow::ensure!(
            !archive_temp.exists(),
            "the killed export's archive survived the next launch: {}",
            archive_temp.display()
        );

        // Killed while the storage image is built from the published archive.
        let (name, _, _, _) = common::unique_names("reclaim-storage");
        let (mut child, pid) =
            common::spawn_fcvm(&["podman", "run", "--name", &name, "--no-snapshot", &image])
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
        let recorded = recorded_temps(pid);
        sigkill(&mut child, pid).await?;
        let storage_temp = observed?;
        let store = recorded.iter().find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("tmp-storage-"))
        });
        anyhow::ensure!(
            store.is_some_and(|s| s.is_dir()),
            "no record of the killed build lists an existing temporary store: {recorded:?}"
        );
        next_launch().await?;
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
            let (prefix, last) = host_ip.rsplit_once('.').context("host_ip")?;
            let via = format!("{prefix}.{}", last.parse::<u8>()? + 1);
            Some((field("guest_ip")?, via))
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
            let named = rule.split_whitespace().any(|token| {
                token == self.host_veth
                    || token == self.host_ip
                    || token == format!("{}/32", self.host_ip)
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
    let port = common::find_available_high_port()?;
    let publish = format!("{port}:80");
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
