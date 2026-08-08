//! The runner AMI's cache key must cover everything baked into the AMI.
//!
//! `scripts/build-ami.sh` skips the (expensive) rebuild when an AMI already
//! exists tagged with the same `compute_hash`. Anything the host kernel is built
//! from but the hash does not read is therefore a *silent staleness* bug: the
//! source changes, the hash does not, and the builder hands back an image
//! carrying the OLD kernel.
//!
//! That was real. `compute_hash` read `kernel/patches/*.patch`, while
//! `[kernel_profiles.nested.arm64.host_kernel].build_inputs` is
//! `kernel/patches-arm64/*.patch` — a different directory. Only two of the nine
//! files there are symlinks back into `kernel/patches`; the other seven were
//! invisible, including `nv2-vsock-cache-sync.patch` and
//! `nv2-vsock-rx-barrier.patch`, the DSB cache-coherency patches AGENTS.md
//! documents as required for NV2 correctness. `kernel_version` was not read
//! either, so a pure version bump changed nothing the cache key could see.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn build_ami_sh() -> String {
    let p = repo_root().join("scripts/build-ami.sh");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Return the `compute_hash()` function body, so assertions cannot be satisfied
/// by an unrelated mention of a path elsewhere in the script.
fn compute_hash_body(script: &str) -> String {
    let start = script
        .find("compute_hash()")
        .expect("build-ami.sh has no compute_hash() — cannot evaluate the cache key");
    let rest = &script[start..];
    let end = rest
        .find("\n}\n")
        .expect("compute_hash() has no closing brace");
    rest[..end].to_string()
}

fn host_kernel_table() -> toml::Value {
    let p = repo_root().join("rootfs-config.toml");
    let text =
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
    let cfg: toml::Value = text.parse().expect("rootfs-config.toml is not valid TOML");
    cfg.get("kernel_profiles")
        .and_then(|v| v.get("nested"))
        .and_then(|v| v.get("arm64"))
        .and_then(|v| v.get("host_kernel"))
        .cloned()
        .expect("rootfs-config.toml has no [kernel_profiles.nested.arm64.host_kernel]")
}

/// Every `build_inputs` glob of the host kernel must be read by `compute_hash`.
#[test]
fn ami_hash_covers_every_host_kernel_build_input() {
    let body = compute_hash_body(&build_ami_sh());
    let host = host_kernel_table();

    let inputs = host
        .get("build_inputs")
        .and_then(|v| v.as_array())
        .expect("host_kernel has no build_inputs array");
    assert!(
        !inputs.is_empty(),
        "host_kernel.build_inputs is empty — nothing to check, which means this test \
         would pass no matter what compute_hash reads"
    );

    for input in inputs {
        let pattern = input
            .as_str()
            .expect("a build_inputs entry is not a string");
        // Compare on the directory: the script iterates a glob, so the literal
        // pattern string need not appear, but the directory must.
        let dir = pattern
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or(pattern)
            .trim_start_matches("kernel/");
        assert!(
            body.contains(dir),
            "the host kernel is built from `{pattern}`, but compute_hash() in \
             scripts/build-ami.sh never reads `{dir}`. A change there would leave the AMI \
             hash identical, so the builder reuses an image carrying the OLD host kernel. \
             compute_hash body:\n{body}"
        );
    }
}

/// The kernel version is part of the baked kernel's identity.
#[test]
fn ami_hash_covers_host_kernel_version() {
    let body = compute_hash_body(&build_ami_sh());
    let host = host_kernel_table();
    host.get("kernel_version")
        .and_then(|v| v.as_str())
        .expect("host_kernel has no kernel_version");

    assert!(
        body.contains("kernel_version"),
        "compute_hash() never reads `kernel_version`, so bumping the host kernel version \
         alone leaves the AMI cache key unchanged and the builder reuses an AMI built from \
         the previous version. compute_hash body:\n{body}"
    );
}
