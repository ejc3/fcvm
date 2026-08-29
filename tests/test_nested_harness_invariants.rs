//! Static guards for the simple nested-VM smoke-test harness.
//!
//! Environment variables from the host test process do not automatically cross
//! the exec boundary into L1. Keep the snapshot-mode matrix wired through to
//! the L2 `fcvm` process explicitly.

use std::path::PathBuf;

fn nested_test_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/test_kvm.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn nested_smoke_test_source() -> String {
    let source = nested_test_source();
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

fn shared_test_helpers_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/mod.rs");
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

/// #875: an L1 whose resolv.conf sources name only a loopback stub fails the
/// inner fcvm with an error about the host's DNS, naming nothing about L1. The
/// smoke test must reach its own verdict on L1 before the inner run.
#[test]
fn simple_nested_smoke_test_gates_on_l1_dns_before_the_inner_run() {
    let source = nested_smoke_test_source();

    let gate = source
        .find("assert_l1_dns_is_usable")
        .expect("the smoke test must check L1's nameservers itself");
    let inner_run = source
        .find("timeout 600 fcvm podman run")
        .expect("the inner fcvm run is still the bounded exec");

    assert!(
        gate < inner_run,
        "the DNS gate must run before the inner fcvm, or the failure it explains          has already happened"
    );
}

/// The gate is only worth having if it decides the way the inner fcvm decides,
/// and if it shows the L1 state behind its verdict.
#[test]
fn l1_dns_gate_reuses_the_predicate_the_inner_fcvm_uses() {
    let source = nested_test_source();

    assert!(
        source.contains("fcvm::network::nameservers_from_sources"),
        "the gate must use the production predicate; a re-implementation can accept          an L1 the inner fcvm rejects"
    );
    assert!(
        source.contains("fcvm::network::RESOLV_CONF_SOURCES"),
        "the gate must read the same files the inner fcvm reads"
    );
    for evidence in [
        "--- {} ---",
        "--- /proc/cmdline ---",
        "L1 reports healthy with no nameserver the inner fcvm can use",
    ] {
        assert!(
            source.contains(evidence),
            "a firing gate must print the L1 state that produced its verdict,              missing: {evidence}"
        );
    }
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
        "FCVM_CONFIG_DIR must be absolute; fcvm refuses relative values because they cross \
         process boundaries with different working directories"
    );
    assert!(
        wrapper.contains("export FCVM_CONFIG_DIR=\"$config_home\""),
        "the isolated config directory must reach nextest, sudo, and every fcvm child"
    );
    assert!(
        !wrapper.contains("export XDG_CONFIG_HOME"),
        "the wrapper must NOT touch XDG_CONFIG_HOME: podman and skopeo resolve \
         containers/storage.conf through it with version- and uid-dependent fallbacks, which \
         split the test's image store from fcvm's (rootless skopeo hex-ID failures locally; \
         root podman 'image not known' on a CI runner)"
    );
    assert!(
        wrapper.contains("./target/release/fcvm setup --generate-config --force"),
        "the exact branch binary must generate its embedded config before nextest starts"
    );
}

#[test]
fn isolated_config_wraps_the_fc_mock_recipe() {
    let makefile = makefile_source();
    let start = makefile
        .find("_test-fc-mock: cargo-target-link")
        .expect("fc-mock run-only recipe is missing");
    let end = makefile[start..]
        .find("test-fc-mock: show-notes")
        .map(|offset| start + offset)
        .expect("fc-mock public recipe must follow its run-only recipe");
    let recipe = &makefile[start..end];

    assert!(
        recipe.contains("$(TEST_CONFIG_WRAPPER) $(NEXTEST)"),
        "fc-mock must use the same per-worktree config isolation as every other host nextest recipe"
    );
}

#[test]
fn isolated_config_mounts_the_active_xdg_config_into_nested_guests() {
    let source = nested_test_source();

    assert!(
        source.contains("var_os(\"FCVM_CONFIG_DIR\")"),
        "nested launches must resolve the config mounted into L1 from the active FCVM_CONFIG_DIR"
    );
    assert!(
        source.contains("join(\"fcvm\")"),
        "the mounted host path must be XDG_CONFIG_HOME/fcvm"
    );
    assert_eq!(
        source.matches("active_fcvm_config_dir()").count(),
        3,
        "the helper definition and both nested launch paths must share the active config resolver"
    );
    assert!(
        source.contains("config_dir.display()"),
        "the resolved XDG fcvm directory must feed the /root/.config/fcvm mount"
    );
}

#[test]
fn nested_config_fallback_matches_config_generation_path() {
    let source = nested_test_source();

    assert!(
        source.contains("/tmp/fcvm-config"),
        "without XDG_CONFIG_HOME or HOME, nested launches must mount the same /tmp/fcvm-config directory where shared setup generates rootfs-config.toml"
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

#[test]
fn config_schema_check_reads_the_active_xdg_path() {
    let source = shared_test_helpers_source();
    let start = source
        .find("fn ensure_config_exists()")
        .expect("shared config setup helper is missing");
    let end = source[start..]
        .find("/// Make a spawned fcvm child")
        .map(|offset| start + offset)
        .expect("config setup helper must precede the child-lifecycle helper");
    let helper = &source[start..end];
    let xdg = helper
        .find("var_os(\"XDG_CONFIG_HOME\")")
        .expect("config validation must inspect XDG_CONFIG_HOME");
    let home = helper
        .find("var_os(\"HOME\")")
        .expect("config validation must retain the HOME fallback");

    assert!(
        xdg < home,
        "the isolated XDG config must be checked before falling back to ~/.config/fcvm"
    );
    assert!(
        helper.contains("join(\"fcvm/rootfs-config.toml\")"),
        "XDG_CONFIG_HOME must resolve to its fcvm/rootfs-config.toml child"
    );
}
