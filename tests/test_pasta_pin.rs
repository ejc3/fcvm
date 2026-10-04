//! Both pasta builds pin the same upstream commit, the same verified archive,
//! and the same carried patch. `fcvm setup` (src/setup/pasta.rs) and
//! scripts/build-passt.sh must agree on all three, or a host builds a pasta that
//! differs from CI's. The patch is dropped once the pin moves past its upstream
//! merge.
//!
//! The last three tests run `fill_build_dir()` from scripts/build-passt.sh, the
//! step that decides what that script builds from.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn in_manifest(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
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

/// `fill_build_dir()` from scripts/build-passt.sh, verbatim. It is extracted and
/// not sourced, so no test here can reach the script's download from upstream,
/// its build or its install.
fn fill_build_dir_source() -> String {
    let script = read("scripts/build-passt.sh");
    let start = script
        .find("fill_build_dir() {")
        .expect("scripts/build-passt.sh has no fill_build_dir()");
    let rest = &script[start..];
    let end = rest
        .find("\n}\n")
        .expect("fill_build_dir() has no closing brace")
        + 2;
    rest[..end].to_string()
}

/// Call `fill_build_dir <kept> <build> <url> <sha256> <the carried patch>` under
/// the script's own shell options. `build` is created empty first, as the script
/// creates it with `mktemp -d`.
fn fill_build_dir(kept: &Path, build: &Path, url: &str, sha256: &str) -> Output {
    std::fs::create_dir_all(build).unwrap();
    // The arguments go through the environment and are never pasted into the
    // program text.
    let program = format!(
        "set -euo pipefail\n{}\nfill_build_dir \"$KEPT\" \"$BUILD\" \"$URL\" \"$SHA256\" \"$PATCH\"\n",
        fill_build_dir_source()
    );
    Command::new("bash")
        .arg("-c")
        .arg(program)
        .env("KEPT", kept)
        .env("BUILD", build)
        .env("URL", url)
        .env("SHA256", sha256)
        .env("PATCH", in_manifest(PATCH_FILE))
        .output()
        .expect("running bash")
}

fn said(run: &Output) -> String {
    format!(
        "{}\nstdout: {}\nstderr: {}",
        run.status,
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    )
}

/// How many of the patch's three guards `udp.c` in `tree` carries.
fn guards(tree: &Path) -> usize {
    std::fs::read_to_string(tree.join("udp.c"))
        .unwrap_or_else(|e| panic!("reading udp.c in {}: {e}", tree.display()))
        .matches("if (uflow)")
        .count()
}

/// A stand-in for the upstream archive: one top-level directory holding a
/// `Makefile` and the part of `udp.c` the carried patch changes. That part is
/// read out of the patch (its context and removed lines), so the real patch
/// applies to it and no copy of upstream's source is kept here.
struct Fixture {
    dir: tempfile::TempDir,
    archive: PathBuf,
    url: String,
    sha256: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree/passt-fixture");
        std::fs::create_dir_all(&tree).unwrap();
        let mut unpatched = String::new();
        let mut in_hunk = false;
        for line in read(PATCH_FILE).lines() {
            if line.starts_with("@@") {
                in_hunk = true;
            } else if in_hunk && !line.starts_with('+') {
                unpatched.push_str(line.get(1..).unwrap_or(""));
                unpatched.push('\n');
            }
        }
        std::fs::write(tree.join("udp.c"), unpatched).unwrap();
        std::fs::write(tree.join("Makefile"), "all:\n").unwrap();
        let archive = dir.path().join("passt-fixture.tar.xz");
        let packed = Command::new("tar")
            .arg("-cJf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path().join("tree"))
            .arg("passt-fixture")
            .output()
            .expect("running tar");
        assert!(
            packed.status.success(),
            "packing the fixture: {}",
            said(&packed)
        );
        let sha256 = sha256_hex(&std::fs::read(&archive).unwrap());
        let url = format!("file://{}", archive.display());
        Self {
            dir,
            archive,
            url,
            sha256,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// A run that is killed after `tar` has written `Makefile` and before the patch
/// loop has finished leaves an unpatched tree. Taken for a finished one, it makes
/// the next run build and install the unpatched pin. The directory left here
/// sits under the very name the next run keeps its source under.
#[test]
fn a_half_made_source_directory_is_never_built_from() {
    let fx = Fixture::new();
    let kept = fx.path("passt-build-0123456789ab");
    std::fs::create_dir_all(&kept).unwrap();
    let extracted = Command::new("tar")
        .arg("-xJf")
        .arg(&fx.archive)
        .arg("-C")
        .arg(&kept)
        .arg("--strip-components=1")
        .output()
        .expect("running tar");
    assert!(extracted.status.success(), "{}", said(&extracted));
    assert!(kept.join("Makefile").is_file());
    assert_eq!(guards(&kept), 0, "the fixture must start unpatched");

    let build = fx.path("passt-build-0123456789ab.build.next-run");
    let run = fill_build_dir(&kept, &build, &fx.url, &fx.sha256);
    assert!(
        run.status.success(),
        "fill_build_dir failed: {}",
        said(&run)
    );
    assert_eq!(
        guards(&build),
        3,
        "the next run would build an unpatched tree: it took the half-made directory \
         for a finished one. {}",
        said(&run)
    );

    // Whatever holds the kept name without being a finished tree is not this
    // run's to change, and a copy this run could not keep is removed.
    assert_eq!(guards(&kept), 0, "the half-made directory was modified");
    let left: Vec<String> = std::fs::read_dir(fx.dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".stage."))
        .collect();
    assert!(left.is_empty(), "staging directories left behind: {left:?}");
}

/// The source a run keeps for later runs is the verified and patched tree, and a
/// later run builds from it with no download.
#[test]
fn a_kept_source_tree_is_patched_and_serves_a_later_run_without_a_download() {
    let fx = Fixture::new();
    let kept = fx.path("passt-build-0123456789ab");

    let first_build = fx.path("passt-build-0123456789ab.build.first");
    let first = fill_build_dir(&kept, &first_build, &fx.url, &fx.sha256);
    assert!(first.status.success(), "first run: {}", said(&first));
    assert_eq!(guards(&first_build), 3);
    assert_eq!(guards(&kept), 3, "the kept tree is not the patched one");

    let nowhere = format!("file://{}", fx.path("absent.tar.xz").display());
    let second_build = fx.path("passt-build-0123456789ab.build.second");
    let second = fill_build_dir(&kept, &second_build, &nowhere, &fx.sha256);
    assert!(
        second.status.success(),
        "a run with a kept tree still needed the archive: {}",
        said(&second)
    );
    assert_eq!(guards(&second_build), 3);
}

/// The checksum stops a run before anything is extracted, and nothing appears
/// under the kept name.
#[test]
fn an_archive_with_the_wrong_checksum_is_neither_extracted_nor_kept() {
    let fx = Fixture::new();
    let kept = fx.path("passt-build-0123456789ab");
    let build = fx.path("passt-build-0123456789ab.build.only");
    let run = fill_build_dir(&kept, &build, &fx.url, &"0".repeat(64));
    assert!(
        !run.status.success(),
        "a wrong checksum was accepted: {}",
        said(&run)
    );
    assert!(!build.join("udp.c").exists(), "the archive was extracted");
    assert!(!kept.exists(), "a tree was kept from an unverified archive");
}
