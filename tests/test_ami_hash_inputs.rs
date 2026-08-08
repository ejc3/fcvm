//! The runner AMI's cache key must change when the host kernel's inputs change.
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
//!
//! These tests EXECUTE the real `compute_hash` against a fixture tree and assert
//! that mutating each input moves the hash. An earlier version scanned the
//! function's source text for the strings `patches-arm64` and `kernel_version` —
//! which the explanatory COMMENTS also contain, so deleting the code and keeping
//! the comments left it green. A check satisfiable by a comment about the check
//! is exactly the failure mode this file exists to prevent.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Extract `compute_hash()` verbatim from the real script and run it against a
/// fixture tree. `create_user_data` (its only helper call) is stubbed so this
/// exercises the kernel-input behaviour in isolation.
fn run_compute_hash(fixture: &Path) -> String {
    let script = std::fs::read_to_string(repo_root().join("scripts/build-ami.sh"))
        .expect("read scripts/build-ami.sh");

    let start = script
        .find("compute_hash()")
        .expect("build-ami.sh has no compute_hash() — cannot evaluate the cache key");
    let rest = &script[start..];
    let end = rest
        .find("\n}\n")
        .expect("compute_hash() has no closing brace")
        + 2;
    let func = &rest[..end];

    let program = format!(
        "set -u\n\
         KERNEL_DIR={fx}/kernel\n\
         SCRIPT_DIR={fx}/scripts\n\
         create_user_data() {{ printf 'stub-user-data'; }}\n\
         {func}\n\
         compute_hash\n",
        fx = fixture.display(),
        func = func
    );

    let out = Command::new("bash")
        .arg("-c")
        .arg(&program)
        .output()
        .expect("run bash");
    assert!(
        out.status.success(),
        "compute_hash failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        !hash.is_empty(),
        "compute_hash produced no output — every comparison below would be vacuously equal"
    );
    hash
}

/// Build a minimal tree with the layout `compute_hash` reads.
fn make_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let w = |rel: &str, body: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };

    w("kernel/nested.conf", "CONFIG_KVM=y\n");
    w("kernel/patches/0001-fuse.patch", "fuse patch v1\n");
    // The host kernel's real patch set, including the arm64-only files that were
    // invisible to the old hash.
    w("kernel/patches-arm64/0001-fuse.patch", "fuse patch v1\n");
    w(
        "kernel/patches-arm64/nv2-vsock-cache-sync.patch",
        "dsb sy\n",
    );
    w(
        "kernel/patches-arm64/nv2-vsock-rx-barrier.patch",
        "dsb sy rx\n",
    );
    w("kernel/patches-arm64/wfx-stopped-exit.patch", "wfx\n");
    // Guest-only; must NOT affect the host kernel's hash.
    w(
        "kernel/patches-arm64/mmfr4-override.vm.patch",
        "guest only\n",
    );
    w(
        "rootfs-config.toml",
        "boot_args = \"kvm-arm.mode=nested\"\nkernel_version = \"6.18.3\"\n",
    );
    w("scripts/build-passt.sh", "#!/bin/sh\n");
    w("scripts/passt-0001.patch", "passt\n");
    w("scripts/runner-disk-preflight.sh", "#!/bin/sh\n");
    w("scripts/runner-disk-guard.service", "[Unit]\n");
    w("scripts/runner-disk-guard.timer", "[Timer]\n");
    dir
}

/// Same inputs must give the same hash — otherwise "it changed" proves nothing.
#[test]
fn ami_hash_is_stable_for_identical_inputs() {
    let fx = make_fixture();
    let a = run_compute_hash(fx.path());
    let b = run_compute_hash(fx.path());
    assert_eq!(
        a, b,
        "compute_hash is not deterministic, so every mutation test below would pass for the \
         wrong reason (a hash that changes for everything gates nothing)"
    );
}

/// Mutating any host-kernel patch must move the hash.
#[test]
fn ami_hash_changes_when_a_host_kernel_patch_changes() {
    let fx = make_fixture();
    let base = run_compute_hash(fx.path());

    // The seven arm64-only patches were the ones the old hash could not see.
    for patch in [
        "nv2-vsock-cache-sync.patch",
        "nv2-vsock-rx-barrier.patch",
        "wfx-stopped-exit.patch",
    ] {
        let p = fx.path().join("kernel/patches-arm64").join(patch);
        let original = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, format!("{original}MUTATED\n")).unwrap();
        let mutated = run_compute_hash(fx.path());
        std::fs::write(&p, &original).unwrap();

        assert_ne!(
            base, mutated,
            "editing kernel/patches-arm64/{patch} left the AMI hash at {base}. That patch is \
             built into the host kernel, so build-ami.sh would find a 'matching' AMI and reuse \
             an image carrying the OLD kernel. For the two nv2-vsock-* patches that means \
             shipping a host kernel without the DSB cache-coherency fix."
        );
    }
}

/// Bumping the host kernel version must move the hash.
#[test]
fn ami_hash_changes_when_kernel_version_changes() {
    let fx = make_fixture();
    let base = run_compute_hash(fx.path());

    let cfg = fx.path().join("rootfs-config.toml");
    let original = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, original.replace("6.18.3", "7.0.14")).unwrap();
    let mutated = run_compute_hash(fx.path());
    std::fs::write(&cfg, &original).unwrap();

    assert_ne!(
        base, mutated,
        "bumping kernel_version left the AMI hash at {base}, so a version bump alone reuses an \
         AMI built from the previous kernel version"
    );
}

/// A guest-only `.vm.patch` must NOT move the host hash — otherwise the hash
/// churns on changes that cannot affect the baked host kernel, and "it changed"
/// stops meaning anything.
#[test]
fn ami_hash_ignores_guest_only_vm_patches() {
    let fx = make_fixture();
    let base = run_compute_hash(fx.path());

    let p = fx
        .path()
        .join("kernel/patches-arm64/mmfr4-override.vm.patch");
    let original = std::fs::read_to_string(&p).unwrap();
    std::fs::write(&p, format!("{original}MUTATED\n")).unwrap();
    let mutated = run_compute_hash(fx.path());
    std::fs::write(&p, &original).unwrap();

    assert_eq!(
        base, mutated,
        "a *.vm.patch is applied only to the GUEST kernel (see compute_host_kernel_sha in \
         src/setup/kernel.rs, which excludes them), so it must not invalidate the host AMI"
    );
}
