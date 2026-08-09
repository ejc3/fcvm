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

fn makefile_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Makefile");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn test_config_wrapper_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/with-test-config.sh");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn privileged_runner_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/root-test-runner.sh");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
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

#[test]
fn test_recipes_use_an_exact_worktree_local_config() {
    let makefile = makefile_source();
    let wrapper = test_config_wrapper_source();

    assert!(
        makefile.contains("TEST_CONFIG_WRAPPER := ./scripts/with-test-config.sh"),
        "all test recipes need one fail-closed config-isolation entrypoint"
    );
    assert!(
        makefile.matches("$(TEST_CONFIG_WRAPPER)").count() >= 4,
        "all four host nextest recipes must use config isolation"
    );
    assert!(
        wrapper.contains("mktemp -d \"$target_dir/test-config.XXXXXX\""),
        "each test run needs its own directory below the per-worktree cargo target"
    );
    assert!(
        wrapper.contains("target_dir=\"$(cd \"$target_dir\" && pwd -P)\""),
        "XDG_CONFIG_HOME must be absolute; the directories crate ignores relative XDG paths \
         and silently falls back to the shared ~/.config/fcvm directory"
    );
    assert!(
        wrapper.contains("export XDG_CONFIG_HOME=\"$config_home\""),
        "the isolated config directory must reach nextest, sudo, and every fcvm child"
    );
    assert!(
        wrapper.contains("./target/release/fcvm setup --generate-config --force"),
        "the exact branch binary must generate its embedded config before nextest starts"
    );
}

#[test]
fn test_unit_recipe_honors_its_filter() {
    let makefile = makefile_source();

    assert!(
        makefile.contains("$(TEST_CONFIG_WRAPPER) $(NEXTEST) --no-default-features $(FILTER)"),
        "make test-unit FILTER=<name> must pass the filter to nextest; otherwise \
         red/green verification silently runs unrelated tests instead of the named guard"
    );
}

#[test]
fn privileged_runner_keeps_the_isolated_xdg_config_authoritative() {
    let runner = privileged_runner_source();
    let guard = runner
        .find("[ -n \"${XDG_CONFIG_HOME:-}\" ]")
        .expect("the privileged runner must detect an isolated XDG config");
    let unset = runner
        .find("unset SUDO_USER")
        .expect("the privileged runner must remove sudo's shared-config override");
    let exec = runner
        .find("exec setpriv --pdeathsig KILL")
        .expect("the privileged runner must retain its pdeathsig exec");

    assert!(
        guard < unset && unset < exec,
        "SUDO_USER must be removed after confirming XDG isolation and before executing the \
         root test binary; otherwise fcvm reopens the shared ~/.config/fcvm path"
    );
}
