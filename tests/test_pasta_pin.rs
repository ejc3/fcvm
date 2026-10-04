//! Both pasta builds pin the same upstream commit, the same verified archive,
//! and the same carried patch. `fcvm setup` (src/setup/pasta.rs) and
//! scripts/build-passt.sh must agree on all three, or a host builds a pasta that
//! differs from CI's. The patch is dropped once the pin moves past its upstream
//! merge.

use sha2::{Digest, Sha256};

/// The pin is the upstream fix that keeps `-a`, `-g` and `-n` on a host without
/// IPv4. It also contains 3f57f0382f6a, the upstream merge of the addr_seen fix
/// for issue #661 that this repo used to carry as a patch (stable patch-id
/// 340576aef02fa19411e69e3587566f3c4950057a): `git merge-base --is-ancestor
/// 3f57f0382f6a 4e8aa70379a3` holds upstream. Nothing here checks that ancestry,
/// so a later pin has to be checked the same way.
const PINNED_COMMIT: &str = "4e8aa70379a35ec9deb11d76513e7f6c4123b667";
const ARCHIVE_SHA256: &str = "ef88ad2c6137b52286e6fcd10311d61f2ab329e5558abe097170e9e5851da8b9";

/// `scripts/passt-udp-sock-errs-null-flow.patch`, carried on top of the pin and
/// applied by both build paths, so it is hashed into the pasta binary's
/// identity. Pinning its content keeps the patch and the archive from drifting
/// apart; `the_carried_patch_guards_the_null_flow_udp_error_sites` checks what
/// the patch does.
const PATCH_FILE: &str = "scripts/passt-udp-sock-errs-null-flow.patch";
const PATCH_SHA256: &str = "6ba39e5c7945fd84545c3e5c1c60ab4ab2f48f7f6b282a608064b48463335019";

fn in_manifest(path: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn read(path: &str) -> String {
    std::fs::read_to_string(in_manifest(path)).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn both_pasta_builds_pin_the_same_upstream_commit_and_patch() {
    let config: toml::Value = toml::from_str(&read("rootfs-config.toml")).unwrap();
    assert_eq!(config["pasta"]["commit"].as_str(), Some(PINNED_COMMIT));
    assert_eq!(
        config["pasta"]["repo"].as_str(),
        Some("https://passt.top/passt")
    );

    let script = read("scripts/build-passt.sh");
    for (name, value) in [
        ("PASST_COMMIT", PINNED_COMMIT),
        (
            "PASST_TARBALL_URL",
            "https://passt.top/passt/snapshot/passt-${PASST_COMMIT}.tar.xz",
        ),
        ("PASST_TARBALL_SHA256", ARCHIVE_SHA256),
    ] {
        assert!(
            script
                .lines()
                .any(|line| line == format!("{name}=\"{value}\"")),
            "runner build must use the same upstream commit and verified archive: {name}"
        );
    }

    // The carried patch: pinned by content, and applied by both build paths.
    let patch_basename = PATCH_FILE.rsplit('/').next().unwrap();
    let patch = read(PATCH_FILE);
    assert_eq!(
        sha256_hex(patch.as_bytes()),
        PATCH_SHA256,
        "{PATCH_FILE} changed; re-review it and update PATCH_SHA256"
    );
    assert!(
        script.contains(patch_basename),
        "scripts/build-passt.sh must apply {patch_basename}"
    );
    let setup = read("src/setup/pasta.rs");
    assert!(
        setup.contains(&format!("scripts/{patch_basename}")),
        "src/setup/pasta.rs must embed and apply {patch_basename}"
    );

    // The addr_seen fix that this repo once carried is in the pin; its patch
    // files must stay deleted so neither build re-applies an upstream change.
    for patch in [
        "pasta/patches/0001-tap-dont-let-overheard-traffic-move-addr_seen.patch",
        "scripts/passt-addr-seen.patch",
    ] {
        assert!(
            !in_manifest(patch).exists(),
            "upstream already contains {patch}"
        );
    }
}

/// The carried patch guards every flow-specific log call in `udp_sock_errs()`
/// against a NULL flow. The listening-socket caller `udp_sock_fwd()` passes
/// `FLOW_SIDX_NONE`, so `uflow` is NULL there, and the flow logging macros read
/// the flow's `state` field before any level check. Without the guard pasta
/// dereferences NULL and dies, taking the network from a VM that publishes a UDP
/// port. This fails if the patch, which both build paths apply to the tree they
/// compile, stops guarding one of the three sites.
///
/// This is a source-level pin, not a run-time trigger. The crash path needs the
/// published listening socket to report a socket error, but that socket is
/// receive-only on the host side: ICMP errors attach only to a socket that sent
/// a matching datagram, and the datagram-sending flow sockets always carry a
/// valid flow. So EPOLLERR / an unqueued error on the listening socket cannot be
/// produced from user space here; triggering it would need kernel fault
/// injection. The run-time path is therefore not exercised by a test.
#[test]
fn the_carried_patch_guards_the_null_flow_udp_error_sites() {
    let patch = read(PATCH_FILE);

    // The three flow-specific log calls the patch must guard, each with the
    // non-flow fallback it must add (matching udp_sock_recverr's no-flow logging).
    let sites = [
        (
            "flow_perror_ratelimit(uflow, now,",
            "err_perror(\"Error reading SO_ERROR\");",
        ),
        (
            "flow_dbg(uflow, \"Unqueued error on UDP socket: %s\",",
            "debug(\"Unqueued error on UDP socket: %s\",",
        ),
        (
            "flow_err_ratelimit(uflow, now,",
            "err(\"EPOLLERR event without reported errors\");",
        ),
    ];

    let removed: Vec<&str> = patch
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .collect();
    let added: Vec<&str> = patch
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .collect();

    for (flow_call, fallback) in sites {
        assert!(
            removed.iter().any(|l| l.contains(flow_call)),
            "patch does not touch the unguarded flow call `{flow_call}`"
        );
        assert!(
            added.iter().any(|l| l.contains(fallback)),
            "patch does not add the NULL-flow fallback `{fallback}`"
        );
    }

    let guards = added.iter().filter(|l| l.contains("if (uflow)")).count();
    assert_eq!(
        guards, 3,
        "all three sites must be guarded with `if (uflow)`; found {guards}"
    );
    assert!(
        added
            .iter()
            .any(|l| l.trim_start_matches('+').trim() == "else"),
        "the guard must fall back with `else`"
    );
}
