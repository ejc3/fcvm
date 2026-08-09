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

const TEST_SOURCE_COMMIT: &str = "0000000000000000000000000000000000000000";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Extract `compute_hash()` verbatim from the real script and run it against a
/// fixture tree. `create_user_data` (its only helper call) is stubbed so this
/// exercises the kernel-input behaviour in isolation.
fn run_compute_hash(fixture: &Path) -> String {
    run_compute_hash_for_commit(fixture, TEST_SOURCE_COMMIT)
}

fn run_compute_hash_for_commit(fixture: &Path, source_commit: &str) -> String {
    let (ok, out, err) = try_compute_hash_for_commit(fixture, source_commit);
    assert!(ok, "compute_hash failed: {err}");
    assert!(
        !out.is_empty(),
        "compute_hash produced no output — every comparison below would be vacuously equal"
    );
    out
}

/// Same, but hands back the exit status so the fail-closed cases can assert on it.
fn try_compute_hash(fixture: &Path) -> (bool, String, String) {
    try_compute_hash_for_commit(fixture, TEST_SOURCE_COMMIT)
}

fn try_compute_hash_for_commit(fixture: &Path, source_commit: &str) -> (bool, String, String) {
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

    // Paths go through the ENVIRONMENT, never interpolated into shell source: a
    // TMPDIR containing whitespace or a metacharacter would otherwise split these
    // assignments, the helper would hash no fixture inputs at all, and every
    // mutation test below would compare two copies of the empty-stream digest.
    let program = format!(
        "set -u\n\
         create_user_data() {{ printf 'stub-user-data:%s' \"$1\"; }}\n\
         {func}\n\
         compute_hash \"$SOURCE_COMMIT\"\n",
        func = func
    );

    let out = Command::new("bash")
        .arg("-c")
        .arg(&program)
        .env("KERNEL_DIR", fixture.join("kernel"))
        .env("SCRIPT_DIR", fixture.join("scripts"))
        .env("SOURCE_COMMIT", source_commit)
        .output()
        .expect("run bash");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

fn render_user_data(source_commit: &str) -> String {
    let script = std::fs::read_to_string(repo_root().join("scripts/build-ami.sh"))
        .expect("read scripts/build-ami.sh");
    let start = script
        .find("\ncreate_user_data() {")
        .map(|offset| offset + 1)
        .expect("build-ami.sh has no create_user_data()");
    let rest = &script[start..];
    let end = rest
        .find("\nUSERDATA\n}")
        .expect("create_user_data() has no USERDATA terminator")
        + "\nUSERDATA\n}".len();
    let function = &rest[..end];
    let program = format!("{function}\ncreate_user_data \"$SOURCE_COMMIT\"\n");
    let output = Command::new("bash")
        .arg("-c")
        .arg(program)
        .env("SOURCE_COMMIT", source_commit)
        .output()
        .expect("render AMI user data");
    assert!(
        output.status.success(),
        "create_user_data failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("user data is UTF-8")
}

fn materialize_pinned_tree(repo: &Path, commit: &str, destination: &Path) -> std::process::Output {
    let script = std::fs::read_to_string(repo_root().join("scripts/build-ami.sh"))
        .expect("read scripts/build-ami.sh");
    let start = script
        .find("materialize_pinned_source_tree()")
        .expect("build-ami.sh has no pinned-tree materializer");
    let rest = &script[start..];
    let end = rest
        .find("\n}\n")
        .expect("materialize_pinned_source_tree() has no closing brace")
        + 2;
    let function = &rest[..end];
    let program = format!(
        "set -uo pipefail\n{function}\nmaterialize_pinned_source_tree \"$REPO\" \"$COMMIT\" \"$DEST\"\n"
    );
    Command::new("bash")
        .arg("-c")
        .arg(program)
        .env("REPO", repo)
        .env("COMMIT", commit)
        .env("DEST", destination)
        .output()
        .expect("run pinned-tree materializer")
}

fn compute_pinned_hash(repo: &Path, commit: &str) -> std::process::Output {
    let script = std::fs::read_to_string(repo_root().join("scripts/build-ami.sh"))
        .expect("read scripts/build-ami.sh");
    let extract = |name: &str| {
        let marker = format!("{name}()");
        let start = script
            .find(&marker)
            .unwrap_or_else(|| panic!("build-ami.sh has no {marker}"));
        let rest = &script[start..];
        let end = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{marker} has no closing brace"))
            + 2;
        rest[..end].to_string()
    };
    let materialize = extract("materialize_pinned_source_tree");
    let compute = extract("compute_hash");
    let pinned = extract("compute_pinned_hash");
    let program = format!(
        "set -uo pipefail\n\
         create_user_data() {{ printf 'stub-user-data:%s' \"$1\"; }}\n\
         {materialize}\n{compute}\n{pinned}\n\
         compute_pinned_hash \"$REPO\" \"$COMMIT\"\n"
    );
    Command::new("bash")
        .arg("-c")
        .arg(program)
        .env("REPO", repo)
        .env("COMMIT", commit)
        .output()
        .expect("run pinned hash computation")
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
    // Two kernel_version assignments on purpose: only the host_kernel one may
    // move the hash. A bare grep would pick up both and churn the key whenever an
    // unrelated profile changed.
    w(
        "rootfs-config.toml",
        "boot_args = \"kvm-arm.mode=nested\"\n\
         [kernel_profiles.btrfs.arm64]\n\
         kernel_version = \"6.16.1\"\n\
         [kernel_profiles.nested.arm64.host_kernel]\n\
         kernel_version = \"6.18.3\"\n",
    );
    w("scripts/build-passt.sh", "#!/bin/sh\n");
    w("scripts/install-runner-disk-guard.sh", "#!/bin/sh\n");
    w(
        "scripts/build-ami.sh",
        "#!/bin/sh\n# pinned provisioning script\n",
    );
    w("scripts/passt-0001.patch", "passt\n");
    w("scripts/runner-disk-preflight.sh", "#!/bin/sh\n");
    w("scripts/prune-cargo-target.sh", "#!/bin/sh\n");
    w("scripts/runner-disk-guard.service", "[Unit]\n");
    w("scripts/runner-disk-guard.timer", "[Timer]\n");
    w(
        "src/setup/kernel.rs",
        "// host-tool setup implementation revision one\n",
    );
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

/// A cache key computed over FEWER inputs than intended is the stale-AMI bug in
/// disguise: it looks valid, `check_existing_ami` matches it, and the builder
/// hands back an image carrying a different kernel.
///
/// `hash=$(compute_hash)` does not inherit `errexit` inside the command
/// substitution, so every read has to police itself. These pin that.
#[test]
fn ami_hash_refuses_to_compute_when_an_input_is_unreadable() {
    let fx = make_fixture();
    let patch = fx
        .path()
        .join("kernel/patches-arm64/nv2-vsock-cache-sync.patch");

    // A dangling symlink is the realistic shape: the patch set carries symlinks
    // into kernel/patches, and a rename on the other side leaves exactly this.
    std::fs::remove_file(&patch).unwrap();
    std::os::unix::fs::symlink("/nonexistent/gone.patch", &patch).unwrap();

    let (ok, out, err) = try_compute_hash(fx.path());
    assert!(
        !ok,
        "compute_hash SUCCEEDED with an unreadable host-kernel patch, returning {out:?}. That \
         key omits a patch the host kernel is built from — for the two nv2-vsock-* patches that \
         means reusing an AMI whose kernel lacks the DSB cache-coherency fix. stderr: {err}"
    );
}

#[test]
fn ami_hash_refuses_to_compute_when_no_host_patches_match() {
    let fx = make_fixture();
    for e in std::fs::read_dir(fx.path().join("kernel/patches-arm64")).unwrap() {
        std::fs::remove_file(e.unwrap().path()).unwrap();
    }
    let (ok, out, err) = try_compute_hash(fx.path());
    assert!(
        !ok,
        "compute_hash SUCCEEDED with no host-kernel patches at all, returning {out:?} — a key \
         computed over an empty patch set, which matches nothing that was ever built. \
         stderr: {err}"
    );
}

/// The key must identify the HOST kernel, not every kernel in the file.
/// rootfs-config.toml carries eight `kernel_version` assignments; a bare grep
/// churned the key — and forced a full EC2 rebuild — whenever an unrelated
/// profile moved.
#[test]
fn ami_hash_ignores_unrelated_kernel_profiles() {
    let fx = make_fixture();
    let base = run_compute_hash(fx.path());

    let cfg = fx.path().join("rootfs-config.toml");
    let original = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, original.replace("6.16.1", "6.17.9")).unwrap();
    let mutated = run_compute_hash(fx.path());

    assert_eq!(
        base, mutated,
        "bumping [kernel_profiles.btrfs.arm64] moved the AMI hash. That profile cannot affect \
         the host kernel baked into this AMI, so the change only costs a full EC2 rebuild for \
         nothing"
    );
}

/// The privileged target helper is installed into the AMI beside the timer entrypoint.  A
/// helper change must therefore invalidate the same image cache key as a parent-script change;
/// otherwise the builder silently reuses an AMI with an incompatible old protocol.
#[test]
fn ami_hash_changes_when_the_privileged_target_pruner_changes() {
    let fx = make_fixture();
    let base = run_compute_hash(fx.path());

    let helper = fx.path().join("scripts/prune-cargo-target.sh");
    std::fs::write(&helper, "#!/bin/sh\n# changed lease protocol\n").unwrap();
    let mutated = run_compute_hash(fx.path());

    assert_ne!(
        base, mutated,
        "editing the target-pruning helper left the AMI hash at {base}; build-ami.sh would \
         reuse an image carrying the old helper beside the new preflight protocol"
    );
}

#[test]
fn ami_hash_changes_when_the_disk_guard_installer_changes() {
    let fx = make_fixture();
    let base = run_compute_hash(fx.path());
    std::fs::write(
        fx.path().join("scripts/install-runner-disk-guard.sh"),
        "#!/bin/sh\n# changed deployment behavior\n",
    )
    .unwrap();
    let mutated = run_compute_hash(fx.path());
    assert_ne!(
        base, mutated,
        "editing the shared disk-guard installer did not invalidate the AMI cache key"
    );
}

#[test]
fn ami_hash_refuses_a_missing_privileged_target_pruner() {
    let fx = make_fixture();
    std::fs::remove_file(fx.path().join("scripts/prune-cargo-target.sh")).unwrap();
    let (ok, out, error) = try_compute_hash(fx.path());
    assert!(
        !ok && out.is_empty() && error.contains("missing or unreadable"),
        "compute_hash emitted a cache key without the installed target helper: \
         ok={ok} out={out:?} error={error:?}"
    );
}

/// Concatenating files without framing lets bytes move across a file boundary while the hash
/// stays unchanged (`ab` + `cd` equals `abc` + `d`). Per-file digests include both identity and
/// contents, so the cache key continues to identify the installed parent/helper pair.
#[test]
fn ami_hash_frames_the_disk_guard_files_independently() {
    let fx = make_fixture();
    std::fs::write(fx.path().join("scripts/runner-disk-preflight.sh"), "ab").unwrap();
    std::fs::write(fx.path().join("scripts/prune-cargo-target.sh"), "cd").unwrap();
    let base = run_compute_hash(fx.path());

    std::fs::write(fx.path().join("scripts/runner-disk-preflight.sh"), "abc").unwrap();
    std::fs::write(fx.path().join("scripts/prune-cargo-target.sh"), "d").unwrap();
    let boundary_shifted = run_compute_hash(fx.path());

    assert_ne!(
        base, boundary_shifted,
        "moving bytes across the parent/helper boundary left the AMI hash unchanged; the cache \
         key does not frame the two installed files independently"
    );
}

/// The builder must provision from the exact checkout whose files compute_hash read. Cloning
/// moving `main` after hashing admits a different parent/helper pair if a merge lands while the
/// cloud instance boots.
#[test]
fn ami_user_data_fetches_the_exact_hashed_source_commit() {
    let source_commit = "1234567890abcdef1234567890abcdef12345678";
    let user_data = render_user_data(source_commit);
    assert!(
        user_data.contains(&format!("FCVM_SOURCE_COMMIT=\"{source_commit}\""))
            && user_data
                .contains("git -C /tmp/fcvm fetch --depth 1 origin \"$FCVM_SOURCE_COMMIT\"")
            && user_data.contains("/tmp/fcvm/scripts/install-runner-disk-guard.sh /tmp/fcvm")
            && !user_data.contains("__FCVM_SOURCE_COMMIT__"),
        "rendered AMI user data is not pinned to {source_commit}:\n{user_data}"
    );
    assert!(
        user_data.contains("sudo -u ubuntu env HOME=/home/ubuntu bash -c")
            && user_data
                .contains("source \"$HOME/.cargo/env\"; cd /tmp/fcvm; make build-host-tools",)
            && !user_data.contains("export HOME=/root")
            && !user_data
                .lines()
                .any(|line| line == "make build-host-tools"),
        "AMI user data invokes the host Make target as root even though the Makefile rejects and \
         forbids root-owned build artifacts:\n{user_data}"
    );

    let script = std::fs::read_to_string(repo_root().join("scripts/build-ami.sh"))
        .expect("read scripts/build-ami.sh");
    assert!(
        script.contains("create_user_data \"$source_commit\"")
            && !script.contains("git clone --depth 1 https://github.com/ejc3/fcvm.git /tmp/fcvm"),
        "AMI user data does not fetch the exact source commit selected before hashing"
    );
}

/// Hash inputs must come from the same immutable tree the builder fetches, not from dirty
/// working-tree bytes merely labelled with HEAD. Two commits differing only in otherwise-unlisted
/// host-tool source prove the key binds the exact commit fetched and compiled by user data; an
/// uncommitted helper mutation proves the archive wins. Mutating build-ami.sh itself must fail
/// because that running function generates part of the hash and cannot safely disagree with the
/// pinned script.
#[test]
fn ami_hash_materializes_the_pinned_tree_and_rejects_a_dirty_provisioner() {
    let fx = make_fixture();
    for args in [
        ["init", "-q"].as_slice(),
        ["config", "user.email", "test@example.invalid"].as_slice(),
        ["config", "user.name", "fcvm test"].as_slice(),
        ["add", "."].as_slice(),
        ["commit", "-qm", "fixture"].as_slice(),
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(fx.path())
            .status()
            .expect("run fixture git command");
        assert!(status.success(), "git {args:?} failed: {status:?}");
    }
    let commit_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(fx.path())
        .output()
        .expect("resolve fixture commit");
    assert!(commit_output.status.success());
    let commit = String::from_utf8(commit_output.stdout)
        .expect("commit is UTF-8")
        .trim()
        .to_string();

    let first_hash = compute_pinned_hash(fx.path(), &commit);
    assert!(
        first_hash.status.success(),
        "clean pinned hash failed: {}",
        String::from_utf8_lossy(&first_hash.stderr)
    );
    let first_hash_text = String::from_utf8(first_hash.stdout.clone())
        .expect("first pinned hash is UTF-8")
        .trim()
        .to_string();
    assert!(
        first_hash_text.len() == 12 && first_hash_text.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "pinned hash is not a 12-digit hexadecimal cache key: {first_hash_text:?}"
    );

    let host_tool_source = fx.path().join("src/setup/kernel.rs");
    std::fs::write(
        &host_tool_source,
        b"// host-tool setup implementation revision two\n",
    )
    .expect("write second committed host-tool revision");
    for args in [
        ["add", "src/setup/kernel.rs"].as_slice(),
        ["commit", "-qm", "second provisioned host-tool source"].as_slice(),
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(fx.path())
            .status()
            .expect("commit second hashed fixture revision");
        assert!(status.success(), "git {args:?} failed: {status:?}");
    }
    let commit_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(fx.path())
        .output()
        .expect("resolve second fixture commit");
    assert!(commit_output.status.success());
    let second_commit = String::from_utf8(commit_output.stdout)
        .expect("second commit is UTF-8")
        .trim()
        .to_string();
    let second_hash = compute_pinned_hash(fx.path(), &second_commit);
    assert!(
        second_hash.status.success(),
        "second pinned hash failed: {}",
        String::from_utf8_lossy(&second_hash.stderr)
    );
    let second_hash_text = String::from_utf8(second_hash.stdout.clone())
        .expect("second pinned hash is UTF-8")
        .trim()
        .to_string();
    assert!(
        second_hash_text.len() == 12
            && second_hash_text
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            && first_hash_text != second_hash_text,
        "compute_pinned_hash ignored an otherwise-unlisted host-tool source change even though AMI user data fetches and compiles the new commit: first={first_hash_text:?} second={second_hash_text:?}"
    );

    let helper = fx.path().join("scripts/prune-cargo-target.sh");
    let pinned_helper = std::fs::read(&helper).expect("read pinned helper");
    std::fs::write(&helper, b"dirty helper bytes\n").expect("dirty helper");
    let archived = tempfile::tempdir().expect("archived source tree");
    let output = materialize_pinned_tree(fx.path(), &second_commit, archived.path());
    assert!(
        output.status.success(),
        "materializing committed source failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(archived.path().join("scripts/prune-cargo-target.sh")).unwrap(),
        pinned_helper,
        "pinned source tree copied dirty working-tree helper bytes"
    );
    let direct_archived_hash = run_compute_hash_for_commit(archived.path(), &second_commit);
    assert_eq!(
        second_hash_text, direct_archived_hash,
        "compute_pinned_hash did not return compute_hash of the exact archived source tree"
    );
    let dirty_worktree_hash = compute_pinned_hash(fx.path(), &second_commit);
    assert!(
        dirty_worktree_hash.status.success(),
        "pinned hash read dirty worktree state or failed: {}",
        String::from_utf8_lossy(&dirty_worktree_hash.stderr)
    );
    assert_eq!(
        second_hash.stdout, dirty_worktree_hash.stdout,
        "dirty helper bytes changed the supposedly commit-pinned AMI hash"
    );

    std::fs::write(
        fx.path().join("scripts/build-ami.sh"),
        b"dirty provisioning script\n",
    )
    .expect("dirty provisioning script");
    let rejected = tempfile::tempdir().expect("rejected source tree");
    let output = materialize_pinned_tree(fx.path(), &second_commit, rejected.path());
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("differs from pinned commit"),
        "dirty provisioning script was allowed to hash/provision different bytes: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let rejected_hash = compute_pinned_hash(fx.path(), &second_commit);
    assert!(
        !rejected_hash.status.success()
            && String::from_utf8_lossy(&rejected_hash.stderr)
                .contains("differs from pinned commit"),
        "dirty running provisioner was allowed to compute a pinned hash: status={:?} stdout={} stderr={}",
        rejected_hash.status,
        String::from_utf8_lossy(&rejected_hash.stdout),
        String::from_utf8_lossy(&rejected_hash.stderr)
    );
}
