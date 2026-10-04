//! Source pins for the statements that carry a VM's balloon target.
//!
//! A cold boot attaches a balloon device only when its RunArgs name a target. A
//! disk-only clone and a restored clone's relaunch after a guest reboot get theirs
//! through the statements in the first test: the VM's state takes `--balloon`, a
//! clone's state takes the snapshot's record, and each cold boot is handed the record
//! of the VM it boots. A memory restore brings the device back with the VMM state, at
//! the target of the run that made the snapshot, and the statements in the second
//! test hand the restore its caller's target and make the records follow the VMM,
//! whose target can be changed on its API socket without any record being written.
//!
//! A startup snapshot is named for the balloon target its workload initialized under,
//! and `fcvm balloon` can change the target while the workload initializes. The
//! statements in the third test ask for each startup snapshot at the target its run
//! started with, so the creator declines it once the device is at another, and make
//! `podman prepare` fail when it is declined.
//!
//! Dropping any one of them still compiles, and the unit tests of the functions on
//! either side still pass, because each of those is given the value it then finds.
//! Only VM tests see the difference, and they take minutes and need KVM:
//! `test_restored_clone_reboot_keeps_its_balloon` and
//! `test_disk_only_clone_keeps_the_balloon` (#1052),
//! `test_balloon_target_honored_on_snapshot_cache_hit` (#1053),
//! `test_snapshot_and_restored_clone_record_the_balloon_the_vmm_has` and
//! `test_startup_snapshot_is_not_taken_after_the_balloon_target_was_changed`, which
//! covers the two run loops and not `podman prepare`.

use std::path::PathBuf;

/// The code of a source file on one line: comment lines dropped, and every run of
/// whitespace collapsed to one space, so a pin does not depend on how rustfmt wraps
/// a call and a commented-out statement does not satisfy one.
fn code(path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The pins whose statement is not in its file's code, each as a line to print.
fn missing(pins: &[(&str, &str, &str)]) -> Vec<String> {
    pins.iter()
        .filter(|(path, _, statement)| !code(path).contains(statement))
        .map(|(path, what, statement)| format!("{path}: {what}\n    {statement}"))
        .collect()
}

#[test]
fn every_call_site_that_carries_the_balloon_still_does() {
    let pins = [
        (
            "src/commands/podman/mod.rs",
            "a VM's state records its --balloon",
            "vm_state.config.balloon_mib = args.balloon;",
        ),
        (
            "src/commands/snapshot.rs",
            "a clone's state starts with the snapshot's balloon record (a Cloud Hypervisor clone \
             keeps it and its snapshots copy it; a Firecracker restore replaces it)",
            "vm_state.config.balloon_mib = snapshot_config.metadata.balloon_mib;",
        ),
        (
            "src/commands/snapshot.rs",
            "the relaunch after a guest reboot is planned with the balloon in the VM's state",
            "match build_clone_reboot_plan( &snapshot_config.metadata, &port_mappings, &vm_name, \
             args.cpu.unwrap_or(snapshot_config.metadata.vcpu), \
             args.mem.unwrap_or(snapshot_config.metadata.memory_mib), \
             vm_state.config.balloon_mib,",
        ),
        (
            "src/commands/snapshot.rs",
            "the reboot plan hands its balloon to the synthesized RunArgs",
            "let synth_args = run_args_from_snapshot_metadata( meta, port_mappings, \
             vm_name.to_string(), cpu, mem, balloon,",
        ),
        (
            "src/commands/snapshot.rs",
            "a disk-only boot is given the snapshot's balloon record",
            "let run_args = run_args_from_snapshot_metadata( meta, &port_mappings, vm_name, \
             args.cpu.unwrap_or(meta.vcpu), args.mem.unwrap_or(meta.memory_mib), \
             meta.balloon_mib,",
        ),
    ];

    let missing = missing(&pins);
    assert!(
        missing.is_empty(),
        "{} of {} statements that carry a VM's balloon record are gone. Without one, a \
         rebooted clone or a disk-only clone of a --balloon VM boots with no balloon \
         device, or a snapshot of a Cloud Hypervisor clone records none. If a statement \
         was only rewritten, update its pin here.\n{}",
        missing.len(),
        pins.len(),
        missing.join("\n")
    );
}

#[test]
fn every_statement_that_sets_or_reads_a_restored_vms_balloon_still_does() {
    let pins = [
        (
            "src/commands/snapshot.rs",
            "a restore is handed the balloon target its caller named",
            "balloon_target_mib: args.balloon,",
        ),
        (
            "src/commands/common.rs",
            "a restore records the device the VMM has",
            "vm_state.config.balloon_mib = client .balloon_target_mib() .await \
             .context(\"reading the restored VM's balloon device\")?;",
        ),
        (
            "src/commands/common.rs",
            "a memory snapshot records the device the VMM has",
            "Ok(balloon_mib) => { snapshot_config.metadata.balloon_mib = balloon_mib;",
        ),
        (
            "src/commands/snapshot.rs",
            "a disk-only snapshot records the device the VMM has",
            "snapshot_config.metadata.balloon_mib = \
             crate::firecracker::FirecrackerClient::new(socket_path.clone())? \
             .balloon_target_mib() .await .context(\"reading the VM's balloon device\")?;",
        ),
    ];

    let missing = missing(&pins);
    assert!(
        missing.is_empty(),
        "{} of {} statements that set or read a restored VM's balloon are gone. Without \
         one, a snapshot cache hit runs at the target of the run that made the snapshot, \
         or a VM's record differs from its device and its next cold boot attaches the \
         record. \
         If a statement was only rewritten, update its pin here.\n{}",
        missing.len(),
        pins.len(),
        missing.join("\n")
    );
}

#[test]
fn every_startup_snapshot_is_asked_for_at_the_target_its_run_started_with() {
    let pins = [
        (
            "src/commands/podman/mod.rs",
            "the `podman run` loop asks for its startup snapshot at the run's --balloon",
            "let snap = CreateSnapshotParams::cache_entry( fc_backend, &startup_key, \
             BalloonRequirement::StartedAt(ctx.args.balloon),",
        ),
        (
            "src/commands/snapshot.rs",
            "the restore loop asks for its startup snapshot at the target the restore set",
            "let snap = CreateSnapshotParams::cache_entry( fc_backend, &startup_key, \
             BalloonRequirement::StartedAt(args.balloon),",
        ),
        (
            "src/commands/podman/mod.rs",
            "`podman prepare` asks for the snapshot it installs at the run's --balloon",
            "existing: target.existing, balloon: BalloonRequirement::StartedAt(ctx.args.balloon),",
        ),
        (
            "src/commands/podman/mod.rs",
            "`podman prepare` fails when its snapshot was declined",
            "SnapshotInstall::BalloonTargetChanged { started_at, now } => bail!(",
        ),
    ];

    let missing = missing(&pins);
    assert!(
        missing.is_empty(),
        "{} of {} statements that keep a startup snapshot to the balloon target its run \
         started with are gone. Without one, a run whose target `fcvm balloon` changed \
         while its workload initialized saves that workload under the name of the target \
         it started with, and later runs at that target restore it. If a statement was \
         only rewritten, update its pin here.\n{}",
        missing.len(),
        pins.len(),
        missing.join("\n")
    );
}
