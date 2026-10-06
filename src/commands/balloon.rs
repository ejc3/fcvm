//! `fcvm balloon`: read or set the balloon target of a running VM.
//!
//! The command talks to the VM's Firecracker on its API socket and writes no
//! state. A snapshot taken afterwards records the new target, because snapshots
//! read the device from the VMM. The VM's own state file, and its own relaunch
//! after a guest reboot, keep the target the VM booted or was restored with.
//!
//! A set holds the VM's snapshot lock from its device check to its report, so it
//! cannot land between a snapshot's read of the balloon and its save, and two sets
//! cannot interleave. A report takes no lock.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::cli::BalloonArgs;
use crate::firecracker::api::{BalloonStats, BalloonUpdate};
use crate::firecracker::FirecrackerClient;
use crate::hypervisor::Backend;
use crate::paths;
use crate::state::{StateManager, VmState};

use super::common::{
    acquire_vm_snapshot_lock_within, firecracker_refused, load_vm_state, BALLOON_TARGET_REFUSALS,
};

/// How long a set waits for the VM's snapshot lock. A snapshot of a large VM holds
/// it for minutes; the command gives up and says so instead of waiting that out.
const SNAPSHOT_LOCK_WAIT: Duration = Duration::from_secs(60);

pub async fn cmd_balloon(args: BalloonArgs) -> Result<()> {
    let state_manager = StateManager::new(paths::state_dir());
    state_manager.init().await?;
    let vm_state = load_vm_state(&state_manager, args.pid, args.name.as_deref()).await?;
    let vm = vm_label(&vm_state);

    // What can be refused from the VM's state is refused before any request.
    check_request(&vm_state, args.mib)?;

    // A set holds the per-VM snapshot lock from the device check to the report.
    // Every snapshot holds that lock from its read of the balloon to its save, and
    // Firecracker applies a PATCH to a paused VM, so without the lock a target
    // could land in between and leave a snapshot whose record and saved device
    // disagree. Two sets cannot interleave either, so each reports the target it
    // set. A report alone changes nothing and takes no lock: it does not wait for
    // a snapshot.
    let _snapshot_lock = match args.mib {
        Some(mib) => {
            let disk = paths::vm_runtime_dir(&vm_state.vm_id).join("disks/rootfs.raw");
            Some(lock_for_set(&disk, mib, &vm, SNAPSHOT_LOCK_WAIT).await?)
        }
        None => None,
    };

    let socket = paths::vm_runtime_dir(&vm_state.vm_id).join("firecracker.sock");
    anyhow::ensure!(
        socket.exists(),
        "VM socket not found - VM may not be running: {}",
        socket.display()
    );
    let client = FirecrackerClient::new(socket)?;

    // `GET /vm/config` says whether the VM has the device with a 200 either way,
    // so "no device" is an answer here and not one of several causes of a 400.
    let device = client
        .balloon_target_mib()
        .await
        .with_context(|| format!("reading the balloon device of VM {vm}"))?;
    require_device(device, &vm)?;

    if let Some(mib) = args.mib {
        client
            .patch_balloon(BalloonUpdate { amount_mib: mib })
            .await
            .map_err(|error| set_failure(error, mib, &vm))?;
        // failpoint: hold between the set and the report, where another set must
        // not land.
        failpoint::hit_async("balloon.post_set_pre_report").await;
    }

    // Printed right after a set, the size is still on its way to the target.
    let stats = client
        .balloon_stats()
        .await
        .with_context(|| format!("reading the balloon's target and size from VM {vm}"))?;
    println!("{}", report_line(&stats)?);
    Ok(())
}

/// The VM's snapshot lock for a set, or why the target was not set. `disk_path` is
/// the VM's root disk, which is how the snapshot paths name the lock.
async fn lock_for_set(
    disk_path: &Path,
    mib: u32,
    vm: &str,
    wait: Duration,
) -> Result<std::fs::File> {
    acquire_vm_snapshot_lock_within(disk_path, wait)
        .await
        .with_context(|| format!("taking the snapshot lock of VM {vm}"))?
        .with_context(|| {
            format!(
                "the balloon target of VM {vm} was not set to {mib} MiB: a snapshot of the \
                 VM, or another `fcvm balloon` that sets its target, still held the VM's \
                 snapshot lock after {}s. Run the command again when it is done",
                wait.as_secs()
            )
        })
}

/// How a VM is named in this command's errors.
fn vm_label(vm_state: &VmState) -> String {
    format!("'{}'", vm_state.name.as_deref().unwrap_or(&vm_state.vm_id))
}

/// Refuse what needs no request to refuse: a backend fcvm cannot ask, and a target
/// above the VM's memory, which Firecracker would refuse with a 400.
fn check_request(vm_state: &VmState, mib: Option<u32>) -> Result<()> {
    let vm = vm_label(vm_state);
    match vm_state.config.hypervisor {
        Backend::Firecracker => {}
        Backend::CloudHypervisor => anyhow::bail!(
            "VM {vm} runs on Cloud Hypervisor, and fcvm balloon needs Firecracker: fcvm's \
             Cloud Hypervisor client has no call that reads or resizes a balloon"
        ),
    }
    if let Some(mib) = mib {
        anyhow::ensure!(
            mib <= vm_state.config.memory_mib,
            "a balloon target of {mib} MiB is above the {} MiB of memory VM {vm} has",
            vm_state.config.memory_mib
        );
    }
    Ok(())
}

/// A VM gets a balloon device at boot or not at all.
fn require_device(device: Option<u32>, vm: &str) -> Result<()> {
    anyhow::ensure!(
        device.is_some(),
        "VM {vm} has no balloon device: one is attached only at boot, with \
         `fcvm podman run --balloon`"
    );
    Ok(())
}

/// What a failed `PATCH /balloon` is reported as. Every failure says what was being
/// done. A 400 also says what Firecracker refuses; a timeout or a dead VMM is
/// neither of those causes.
fn set_failure(error: anyhow::Error, mib: u32, vm: &str) -> anyhow::Error {
    let doing = format!("setting the balloon target of VM {vm} to {mib} MiB");
    if firecracker_refused(&error) {
        error.context(format!("{doing} ({BALLOON_TARGET_REFUSALS})"))
    } else {
        error.context(doing)
    }
}

/// The command's output: one JSON line.
fn report_line(stats: &BalloonStats) -> Result<String> {
    serde_json::to_string(stats).context("writing the balloon report")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firecracker::api::ApiRefusal;

    fn vm(hypervisor: Backend, memory_mib: u32) -> VmState {
        let mut state = VmState::new(
            "vm-0123456789abcdef".to_string(),
            "nginx:alpine".to_string(),
            2,
            memory_mib,
        );
        state.name = Some("web".to_string());
        state.config.hypervisor = hypervisor;
        state
    }

    /// fcvm's Cloud Hypervisor client has no balloon call, so a Cloud Hypervisor VM
    /// is refused by name, from its state, before a client is made for its socket.
    #[test]
    fn a_cloud_hypervisor_vm_is_refused_by_name_before_any_request() {
        for mib in [None, Some(64)] {
            let error = check_request(&vm(Backend::CloudHypervisor, 1024), mib)
                .expect_err("a Cloud Hypervisor VM was accepted")
                .to_string();
            assert!(
                error.contains("Cloud Hypervisor") && error.contains("'web'"),
                "{error}"
            );
        }
        assert!(check_request(&vm(Backend::Firecracker, 1024), None).is_ok());

        let source = include_str!("balloon.rs");
        let start = source
            .find("pub async fn cmd_balloon(")
            .expect("no cmd_balloon");
        // The function ends at the first closing brace in column one.
        let end = source[start..]
            .find("\n}\n")
            .expect("cmd_balloon has no end");
        let body = &source[start..start + end];
        let checked = body
            .find("check_request(&vm_state, args.mib)?;")
            .expect("cmd_balloon does not check the request");
        let client = body
            .find("FirecrackerClient::new(")
            .expect("cmd_balloon makes no client");
        assert!(
            checked < client,
            "cmd_balloon makes its client before it checks the request"
        );
    }

    /// A target above the VM's memory is refused from the VM's state, with both
    /// numbers. Firecracker would refuse it with a 400 that names neither.
    #[test]
    fn a_target_above_the_vms_memory_is_refused_before_any_request() {
        let state = vm(Backend::Firecracker, 1024);
        assert!(check_request(&state, Some(0)).is_ok());
        assert!(check_request(&state, Some(1024)).is_ok());
        let error = check_request(&state, Some(1025))
            .expect_err("a 1025 MiB target for a 1024 MiB VM was accepted")
            .to_string();
        assert!(
            error.contains("1025 MiB") && error.contains("1024 MiB"),
            "{error}"
        );
    }

    /// A VM booted without --balloon has no device and cannot get one.
    #[test]
    fn a_vm_with_no_balloon_device_is_told_where_one_comes_from() {
        assert!(require_device(Some(0), "'web'").is_ok());
        let error = require_device(None, "'web'")
            .expect_err("a VM with no balloon device was accepted")
            .to_string();
        assert!(
            error.contains("'web' has no balloon device") && error.contains("--balloon"),
            "{error}"
        );
    }

    /// A failed set always says what was being done. It says what Firecracker
    /// refuses only when Firecracker answered 400.
    #[test]
    fn a_failed_set_explains_a_refusal_and_nothing_else() {
        let refusal = |status| {
            anyhow::Error::new(ApiRefusal {
                status,
                reply: "the reason".to_string(),
            })
        };
        let refused = format!(
            "{:#}",
            set_failure(refusal(hyper::StatusCode::BAD_REQUEST), 96, "'web'")
        );
        assert!(
            refused.contains("setting the balloon target of VM 'web' to 96 MiB")
                && refused.contains("never activated")
                && refused.contains("--free-page-reporting")
                && refused.contains("400 Bad Request - the reason"),
            "{refused}"
        );
        for other in [
            refusal(hyper::StatusCode::INTERNAL_SERVER_ERROR),
            anyhow::anyhow!("Firecracker API PATCH /balloon timed out after 30s"),
        ] {
            let message = format!("{:#}", set_failure(other, 96, "'web'"));
            assert!(
                message.contains("setting the balloon target of VM 'web' to 96 MiB"),
                "{message}"
            );
            assert!(!message.contains("never activated"), "{message}");
        }
    }

    /// The output is one JSON line with the target and the size, in MiB.
    #[test]
    fn the_report_is_one_json_line() {
        let line = report_line(&BalloonStats {
            target_mib: 96,
            actual_mib: 64,
            total_memory: Some(1 << 30),
        })
        .unwrap();
        assert_eq!(line, r#"{"target_mib":96,"actual_mib":64}"#);
    }

    /// A set waits for the VM's snapshot lock only so long. While a snapshot, or
    /// another set, holds it past the wait, the set gives up and says the target
    /// was not set. It gets the lock once the holder is done.
    #[tokio::test]
    async fn a_set_gives_up_while_the_snapshot_lock_stays_held_and_says_nothing_was_set() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disks/rootfs.raw");
        let holder = crate::commands::common::acquire_vm_snapshot_lock(&disk)
            .await
            .unwrap();

        let wait = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let refused = tokio::time::timeout(
            Duration::from_secs(10),
            lock_for_set(&disk, 96, "'web'", wait),
        )
        .await
        .expect("a set with a 300 ms wait was still waiting for a held lock after 10s");
        let error = format!(
            "{:#}",
            refused.expect_err("a set got a lock that another holder has")
        );
        assert!(
            started.elapsed() >= wait,
            "gave up after {:?}",
            started.elapsed()
        );
        assert!(
            error.contains("the balloon target of VM 'web' was not set to 96 MiB"),
            "{error}"
        );

        drop(holder);
        lock_for_set(&disk, 96, "'web'", wait)
            .await
            .expect("the lock is free once its holder is done");
    }
}
