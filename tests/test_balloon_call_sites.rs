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
//! Dropping any one of them still compiles, and the unit tests of the functions on
//! either side still pass, because each of those is given the value it then finds.
//! Only VM tests see the difference, and they take minutes and need KVM:
//! `test_restored_clone_reboot_keeps_its_balloon` and
//! `test_disk_only_clone_keeps_the_balloon` (#1052),
//! `test_balloon_target_honored_on_snapshot_cache_hit` (#1053) and
//! `test_snapshot_and_restored_clone_record_the_balloon_the_vmm_has`.

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
