//! Static guards for the simple nested-VM smoke-test harness.
//!
//! Environment variables from the host test process do not automatically cross
//! the exec boundary into L1. Keep the snapshot-mode matrix wired through to
//! the L2 `fcvm` process explicitly.

use std::path::PathBuf;

fn nested_smoke_test_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/test_kvm.rs");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let start = source
        .find("async fn test_nested_run_fcvm_inside_vm()")
        .expect("simple nested smoke test is missing");
    let end = source[start..]
        .find("/// Run an nested chain test")
        .map(|offset| start + offset)
        .expect("nested-chain helper must follow the simple smoke test");

    source[start..end].to_owned()
}

#[test]
fn simple_nested_smoke_test_propagates_the_snapshot_mode_into_l2() {
    let source = nested_smoke_test_source();

    assert!(
        source.contains("export FCVM_NO_SNAPSHOT=1"),
        "the nested test matrix's FCVM_NO_SNAPSHOT mode must cross the VM-exec \
         boundary; otherwise SnapshotDisabled still pauses and snapshots the L2 VM"
    );
}
