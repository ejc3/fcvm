//! The build recipe must record which sibling FUSE trees it compiled against.
//!
//! fuse-pipe builds fuse-backend-rs from a path dependency that resolves to a
//! checkout NEXT TO this repository (`fuse-pipe/Cargo.toml`:
//! `path = "../../fuse-backend-rs"`), and the container legs mount sibling
//! `fuse-backend-rs` and `fuser` checkouts into the build (Makefile
//! `-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs`). Nothing in the build
//! log said WHICH tree that was. Issue #807 is the bill for that: two local
//! fuse-backend-rs checkouts drifted 19 commits apart, CI's pinned master had
//! neither, and `test_rootless_map_nonroot_reader` failed deterministically on
//! one box against a main that was green in CI. A green suite was a claim
//! about an unrecorded FUSE tree.
//!
//! `make build` therefore runs `dep-provenance`, which prints one line per
//! sibling dependency: `git describe --always --dirty` of the checkout, or
//! `MISSING` when there is no checkout to describe. These tests pin the hook
//! structurally (same convention as tests/test_ci_workflow_coverage.rs and
//! MakefileBenchGraph in bench/chromium/test_reqbench.py) and prove the
//! recipe's two branches against a scratch tree, so neither the target nor
//! the `build` prerequisite can be dropped silently.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_makefile() -> String {
    let path = repo_root().join("Makefile");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Prerequisites and recipe lines of every rule for `target`.
///
/// Make accumulates prerequisites across rule lines, so this does too. A line
/// whose text after the colon starts with `=` is a variable assignment
/// (`FOO := bar`), not a rule.
fn rule(makefile: &str, target: &str) -> Option<(Vec<String>, Vec<String>)> {
    let mut prereqs = Vec::new();
    let mut recipe = Vec::new();
    let mut found = false;
    let mut in_rule = false;
    for line in makefile.lines() {
        if line.starts_with('\t') {
            if in_rule {
                recipe.push(line.trim().to_string());
            }
            continue;
        }
        in_rule = false;
        let Some((names, rest)) = line.split_once(':') else {
            continue;
        };
        if rest.starts_with('=') || names.starts_with(['#', ' ', '.']) {
            continue;
        }
        if names.split_whitespace().any(|n| n == target) {
            found = true;
            in_rule = true;
            let deps = rest.split('#').next().unwrap_or("");
            prereqs.extend(deps.split_whitespace().map(str::to_owned));
        }
    }
    found.then_some((prereqs, recipe))
}

/// The pin: `build` must depend on `dep-provenance`, and the target must
/// describe both sibling trees with a MISSING fallback.
#[test]
fn build_recipe_carries_the_dep_provenance_hook() {
    let makefile = repo_makefile();

    let (build_prereqs, _) = rule(&makefile, "build").expect("Makefile has no `build` rule");
    // Positive control: cargo-target-link predates this test and every cargo
    // entry point depends on it. If the parser cannot see it, the parser is
    // not reading the rule it thinks it is and cannot fail for the right
    // reason.
    assert!(
        build_prereqs.iter().any(|p| p == "cargo-target-link"),
        "the parser found a `build` rule without the cargo-target-link prerequisite that is \
         known to be there; it is misreading the Makefile. prereqs: {build_prereqs:?}"
    );
    assert!(
        build_prereqs.iter().any(|p| p == "dep-provenance"),
        "`build` no longer runs dep-provenance, so nothing records which sibling FUSE trees \
         the build compiled against. That record is what issue #807 was missing when two \
         fuse-backend-rs checkouts silently diverged by 19 commits. prereqs: {build_prereqs:?}"
    );

    let (_, recipe) =
        rule(&makefile, "dep-provenance").expect("Makefile has no `dep-provenance` rule");
    let recipe = recipe.join("\n");
    for needed in [
        "fuse-backend-rs",
        "fuser",
        "describe --always --dirty",
        "MISSING",
    ] {
        assert!(
            recipe.contains(needed),
            "the dep-provenance recipe lost `{needed}`; it must print one \
             `git describe --always --dirty` line per sibling dependency, or MISSING when \
             the checkout is absent. recipe:\n{recipe}"
        );
    }

    assert!(
        makefile
            .lines()
            .filter(|l| l.starts_with(".PHONY"))
            .any(|l| l.split_whitespace().any(|w| w == "dep-provenance")),
        "dep-provenance is not declared .PHONY, so a file named `dep-provenance` in the \
         repository root would silence the provenance log"
    );
}

// ---------------------------------------------------------------------------
// Functional proof against a scratch tree: both recipe branches, every box.
// ---------------------------------------------------------------------------

/// The identity `make` and `git` must run as, when the harness itself is root.
///
/// Same constraint as tests/test_documented_make_targets.rs: the Host-Root CI
/// legs run this binary as root and the Makefile refuses root on the host, so
/// hand the child to the user sudo recorded. Every child of this test runs as
/// that one identity, which also keeps the scratch git repo single-owner:
/// mixing owners trips git's dubious-ownership refusal, and safe.directory
/// cannot be granted from the command line. Inside containers the guard
/// already permits root.
fn harness_user() -> Option<(u32, u32)> {
    if unsafe { libc::geteuid() } != 0
        || Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
    {
        return None;
    }
    let parse = |key: &str| -> Option<u32> { std::env::var(key).ok()?.parse().ok() };
    match (parse("SUDO_UID"), parse("SUDO_GID")) {
        (Some(uid), Some(gid)) => Some((uid, gid)),
        _ => panic!(
            "BLOCKED: running as root on a host with no SUDO_UID/SUDO_GID; the Makefile \
             refuses root and the harness has no user to hand make to"
        ),
    }
}

fn drop_priv(cmd: &mut Command) {
    if let Some((uid, gid)) = harness_user() {
        use std::os::unix::process::CommandExt;
        cmd.uid(uid).gid(gid);
    }
}

fn chown_tree(root: &Path) {
    if let Some((uid, gid)) = harness_user() {
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            std::os::unix::fs::chown(&path, Some(uid), Some(gid))
                .unwrap_or_else(|e| panic!("chown {}: {e}", path.display()));
            if path.is_dir() {
                for entry in fs::read_dir(&path).unwrap() {
                    stack.push(entry.unwrap().path());
                }
            }
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        // The scratch repo must not inherit an identity requirement from the
        // box: commit fails without one on a fresh CI runner.
        .args(["-c", "user.name=fcvm-test", "-c", "user.email=fcvm@test"])
        .args(args);
    drop_priv(&mut cmd);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} in {}: {e}", dir.display()));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn run_dep_provenance(fcvm_dir: &Path) -> String {
    let mut cmd = Command::new("make");
    cmd.arg("-C")
        .arg(fcvm_dir)
        .arg("dep-provenance")
        // A -j inherited from the make that started the test suite would
        // interleave other goals' output into the lines under assertion.
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS");
    drop_priv(&mut cmd);
    // The test process may have written into the scratch since the last
    // chown; hand the whole tree back to the identity the children run as.
    chown_tree(fcvm_dir.parent().unwrap());
    let out = cmd.output().expect("run make dep-provenance");
    assert!(
        out.status.success(),
        "make dep-provenance failed (exit {:?}):\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run the real recipe against a scratch layout where fuse-backend-rs is a
/// known git repo and fuser is absent, and check both output lines against a
/// `git describe` the test computes independently. Then dirty the checkout
/// and check the `-dirty` suffix survives the recipe, because a provenance
/// line that cannot distinguish a dirty tree from its commit re-opens exactly
/// the two-checkouts-disagree hole.
#[test]
fn dep_provenance_reports_describe_and_missing() {
    assert!(
        Command::new("make").arg("--version").output().is_ok(),
        "BLOCKED: `make` is not runnable, so this test cannot evaluate the recipe it guards"
    );

    let scratch = std::env::temp_dir().join(format!(
        "fcvm-dep-provenance-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&scratch);
    let fcvm_dir = scratch.join("fcvm");
    let dep_dir = scratch.join("fuse-backend-rs");
    fs::create_dir_all(&fcvm_dir).unwrap();
    fs::create_dir_all(&dep_dir).unwrap();
    fs::write(fcvm_dir.join("Makefile"), repo_makefile()).unwrap();
    fs::write(dep_dir.join("lib.rs"), "// scratch\n").unwrap();
    chown_tree(&scratch);

    git(&dep_dir, &["init", "-q"]);
    git(&dep_dir, &["add", "lib.rs"]);
    git(&dep_dir, &["commit", "-q", "-m", "scratch commit"]);

    let expected = git(&dep_dir, &["describe", "--always", "--dirty"]);
    let stdout = run_dep_provenance(&fcvm_dir);
    assert!(
        stdout.contains(&format!("fuse-backend-rs: {expected}")),
        "dep-provenance did not report the sibling checkout's describe output \
         ({expected}):\n{stdout}"
    );
    assert!(
        stdout.contains("fuser: MISSING"),
        "dep-provenance did not report MISSING for the absent fuser checkout:\n{stdout}"
    );

    fs::write(dep_dir.join("lib.rs"), "// scratch, modified\n").unwrap();
    chown_tree(&scratch);
    let dirty = git(&dep_dir, &["describe", "--always", "--dirty"]);
    assert!(
        dirty.ends_with("-dirty"),
        "control failed: the mutation did not dirty the scratch checkout ({dirty})"
    );
    let stdout = run_dep_provenance(&fcvm_dir);
    assert!(
        stdout.contains(&format!("fuse-backend-rs: {dirty}")),
        "dep-provenance lost the --dirty marker; a clean-looking line for a dirty tree \
         misattributes every local-only FUSE edit:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&scratch);
}
