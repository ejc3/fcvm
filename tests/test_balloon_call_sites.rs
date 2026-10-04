//! Source pins for the statements that carry a VM's balloon target to the cold boots
//! made from its snapshots.
//!
//! A cold boot attaches a balloon device only when its RunArgs name a target. A
//! disk-only clone and a restored clone's relaunch after a guest reboot get theirs
//! through the statements pinned here: the VM's state takes `--balloon`, a clone's
//! state takes the snapshot's record, and each cold boot is handed the record of the
//! VM it boots. Dropping any one of them still compiles, and the unit tests of the
//! functions on either side still pass, because each of those is given the value it
//! then finds. Only the VM tests `test_restored_clone_reboot_keeps_its_balloon` and
//! `test_disk_only_clone_keeps_the_balloon` see the missing device (#1052), and they
//! take minutes and need KVM.

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
            "a restored VM's state takes the snapshot's balloon record",
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

    let missing: Vec<String> = pins
        .iter()
        .filter(|(path, _, statement)| !code(path).contains(statement))
        .map(|(path, what, statement)| format!("{path}: {what}\n    {statement}"))
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {} statements that carry a VM's balloon target to its cold boots are gone. \
         Without one, a rebooted clone or a disk-only clone of a --balloon VM boots with no \
         balloon device. If a statement was only rewritten, update its pin here.\n{}",
        missing.len(),
        pins.len(),
        missing.join("\n")
    );
}
