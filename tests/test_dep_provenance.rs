//! The build recipes must record which FUSE dependency code they compiled
//! against, and that record must stay honest.
//!
//! Two dependencies, two mechanisms:
//!
//! * `fuse-backend-rs` is a sibling path dependency (`fuse-pipe/Cargo.toml`:
//!   `path = "../../fuse-backend-rs"`), so the compiled code is whatever that
//!   directory holds, not what any lockfile pins. Its provenance line is
//!   `git describe --always --dirty` of the checkout, `MISSING` when there is
//!   no checkout, and when the tree is dirty a `+<12 hex>` digest of
//!   `git diff HEAD`, because every possible local edit against one commit
//!   otherwise prints the same `-dirty` value.
//! * `fuser` is a git dependency (`fuse-pipe/Cargo.toml` declares the URL,
//!   `Cargo.lock` pins the revision). Cargo compiles the locked revision from
//!   its git cache, never the sibling `/workspace/fuser` mount, so its
//!   provenance line is the lock's resolved `source` string, which carries
//!   the exact commit after `#`.
//!
//! Issue #807 is the bill for not recording this: two local fuse-backend-rs
//! checkouts drifted 19 commits apart, CI's pinned master had neither, and
//! `test_rootless_map_nonroot_reader` failed deterministically on one box
//! against a main that was green in CI. A green suite was a claim about an
//! unrecorded FUSE tree.
//!
//! `make build` and `make build-host-tools` (the target the kernel workflows
//! call) both run `dep-provenance`, and both re-derive the provenance after
//! their cargo recipes finish, failing the build if it changed mid-compile.
//! These tests pin the hooks structurally (same convention as
//! tests/test_ci_workflow_coverage.rs and MakefileBenchGraph in
//! bench/chromium/test_reqbench.py) and prove every recipe branch against a
//! scratch tree, so none of them can be dropped silently.

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

/// The pin: both cargo-compiling build targets must depend on
/// `dep-provenance`, the target must report both dependencies with a MISSING
/// fallback, and both build recipes must re-check the provenance after their
/// cargo commands.
#[test]
fn build_recipes_carry_the_dep_provenance_hook() {
    let makefile = repo_makefile();

    // `build-host-tools` compiles the same FUSE-dependent packages and is
    // what .github/workflows/kernels.yml invokes, so it needs the identical
    // record; a hook on `build` alone leaves the kernel-builder logs blind.
    let mut recipes = Vec::new();
    for target in ["build", "build-host-tools"] {
        let (prereqs, recipe) =
            rule(&makefile, target).unwrap_or_else(|| panic!("Makefile has no `{target}` rule"));
        // Positive control: cargo-target-link predates this test and every
        // cargo entry point depends on it. If the parser cannot see it, the
        // parser is not reading the rule it thinks it is and cannot fail for
        // the right reason.
        assert!(
            prereqs.iter().any(|p| p == "cargo-target-link"),
            "the parser found a `{target}` rule without the cargo-target-link prerequisite \
             that is known to be there; it is misreading the Makefile. prereqs: {prereqs:?}"
        );
        assert!(
            prereqs.iter().any(|p| p == "dep-provenance"),
            "`{target}` no longer runs dep-provenance, so nothing records which FUSE code \
             the build compiled against. That record is what issue #807 was missing when two \
             fuse-backend-rs checkouts silently diverged by 19 commits. prereqs: {prereqs:?}"
        );
        recipes.push((target, recipe.join("\n")));
    }

    // The provenance printed before cargo runs only describes the sources
    // cargo read if nothing changed the sibling tree mid-build. Each build
    // recipe must capture the provenance before its cargo commands, re-derive
    // it after, and fail on a difference.
    for (target, recipe) in &recipes {
        for marker in [
            r#"before="$$($(MAKE) --no-print-directory dep-provenance)""#,
            r#"after="$$($(MAKE) --no-print-directory dep-provenance)""#,
            r#"if [ "$$before" != "$$after" ]"#,
        ] {
            assert!(
                recipe.contains(marker),
                "`{target}` lost the mid-build provenance guard (`{marker}`); a sibling \
                 checkout updated during compilation would then be logged as whatever it was \
                 before cargo ran. recipe:\n{recipe}"
            );
        }
    }

    let (_, recipe) =
        rule(&makefile, "dep-provenance").expect("Makefile has no `dep-provenance` rule");
    let recipe = recipe.join("\n");
    for needed in [
        "fuse-backend-rs",
        "fuser",
        "describe --always --dirty",
        "MISSING",
        // The dirty digest: two different local edits against one commit must
        // not print the same line.
        "sha256sum",
        // fuser provenance comes from the lock's resolved source, not from
        // describing the unused sibling checkout.
        "Cargo.lock",
    ] {
        assert!(
            recipe.contains(needed),
            "the dep-provenance recipe lost `{needed}`; it must print the sibling \
             fuse-backend-rs describe (with a dirty-content digest) and the Cargo.lock \
             resolved fuser source, or MISSING for either. recipe:\n{recipe}"
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
// Functional proof against a scratch tree: every recipe branch, every box.
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

fn run_make(fcvm_dir: &Path, target: &str) -> std::process::Output {
    let mut cmd = Command::new("make");
    cmd.arg("-C")
        .arg(fcvm_dir)
        .arg(target)
        // A -j inherited from the make that started the test suite would
        // interleave other goals' output into the lines under assertion.
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS");
    drop_priv(&mut cmd);
    // The test process may have written into the scratch since the last
    // chown; hand the whole tree back to the identity the children run as.
    chown_tree(fcvm_dir.parent().unwrap());
    cmd.output()
        .unwrap_or_else(|e| panic!("run make {target}: {e}"))
}

fn run_dep_provenance(fcvm_dir: &Path) -> String {
    let out = run_make(fcvm_dir, "dep-provenance");
    assert!(
        out.status.success(),
        "make dep-provenance failed (exit {:?}):\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The `<dep>: ` line from a dep-provenance run, exactly, so a spurious
/// prefix or suffix cannot hide behind a substring match.
fn line_for(stdout: &str, dep: &str) -> String {
    let prefix = format!("{dep}: ");
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in dep-provenance output:\n{stdout}"))
        .to_string()
}

/// Run the real recipe against a scratch layout and check every branch:
/// MISSING for an absent checkout and an absent lockfile, the clean describe,
/// the Cargo.lock-resolved fuser source (including the path-dep shape, where
/// the fuser entry has no source and the parser must not borrow the next
/// package's), and the dirty-content digest, where two different local edits
/// against the same commit must produce two different lines. Without that
/// digest a provenance line cannot distinguish WHICH dirty FUSE code was
/// compiled, which re-opens exactly the two-checkouts-disagree hole.
#[test]
fn dep_provenance_reports_describe_lock_source_and_missing() {
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

    // No git repo in the sibling dir, no Cargo.lock: both lines fall back.
    let stdout = run_dep_provenance(&fcvm_dir);
    assert_eq!(
        line_for(&stdout, "fuse-backend-rs"),
        "MISSING",
        "a sibling directory that is not a git checkout must report MISSING:\n{stdout}"
    );
    assert_eq!(
        line_for(&stdout, "fuser"),
        "MISSING",
        "with no Cargo.lock there is no resolved fuser revision to report:\n{stdout}"
    );

    git(&dep_dir, &["init", "-q"]);
    git(&dep_dir, &["add", "lib.rs"]);
    git(&dep_dir, &["commit", "-q", "-m", "scratch commit"]);

    let fuser_source =
        "git+https://github.com/example/fuser.git?branch=scratch#cafef00dcafef00dcafef00dcafef00dcafef00d";
    fs::write(
        fcvm_dir.join("Cargo.lock"),
        format!(
            "version = 4\n\n\
             [[package]]\nname = \"fuse-pipe\"\nversion = \"0.1.0\"\ndependencies = [\n \"fuser\",\n]\n\n\
             [[package]]\nname = \"fuser\"\nversion = \"0.16.0\"\nsource = \"{fuser_source}\"\n\n\
             [[package]]\nname = \"libc\"\nversion = \"0.2.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n"
        ),
    )
    .unwrap();
    chown_tree(&scratch);

    let expected = git(&dep_dir, &["describe", "--always", "--dirty"]);
    let stdout = run_dep_provenance(&fcvm_dir);
    assert_eq!(
        line_for(&stdout, "fuse-backend-rs"),
        expected,
        "a clean checkout must report exactly its describe output:\n{stdout}"
    );
    // Cargo compiles the revision Cargo.lock resolved, never the sibling
    // fuser checkout (fuse-pipe/Cargo.toml declares fuser as a git
    // dependency), so the provenance must come from the lock.
    assert_eq!(
        line_for(&stdout, "fuser"),
        fuser_source,
        "fuser provenance must be Cargo.lock's resolved source, which pins the exact \
         revision cargo compiles:\n{stdout}"
    );

    // A fuser entry with no source (the path-dependency shape): the parser
    // must report MISSING, not the next package's source.
    fs::write(
        fcvm_dir.join("Cargo.lock"),
        "version = 4\n\n\
         [[package]]\nname = \"fuser\"\nversion = \"0.16.0\"\n\n\
         [[package]]\nname = \"libc\"\nversion = \"0.2.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    )
    .unwrap();
    chown_tree(&scratch);
    let stdout = run_dep_provenance(&fcvm_dir);
    assert_eq!(
        line_for(&stdout, "fuser"),
        "MISSING",
        "a sourceless fuser lock entry must not inherit the next package's source:\n{stdout}"
    );

    // Dirty content A. The digest is computed over `git diff HEAD` (tracked
    // modifications), matching what describe's -dirty marker flags: an
    // untracked file cannot reach the build unless a tracked file references
    // it, which dirties the tree.
    fs::write(dep_dir.join("lib.rs"), "// scratch, dirty A\n").unwrap();
    chown_tree(&scratch);
    let dirty = git(&dep_dir, &["describe", "--always", "--dirty"]);
    assert!(
        dirty.ends_with("-dirty"),
        "control failed: the mutation did not dirty the scratch checkout ({dirty})"
    );
    let prefix = format!("{dirty}+");
    let line_a = line_for(&run_dep_provenance(&fcvm_dir), "fuse-backend-rs");
    let digest_a = line_a.strip_prefix(&prefix).unwrap_or_else(|| {
        panic!(
            "a dirty checkout must report `{prefix}<digest>`; without the digest every \
             possible local edit against this commit prints the same line: {line_a}"
        )
    });
    assert!(
        digest_a.len() == 12 && digest_a.chars().all(|c| c.is_ascii_hexdigit()),
        "the dirty digest must be 12 hex characters, got `{digest_a}` in `{line_a}`"
    );

    // Dirty content B, same commit: the line must change.
    fs::write(dep_dir.join("lib.rs"), "// scratch, dirty B\n").unwrap();
    chown_tree(&scratch);
    let line_b = line_for(&run_dep_provenance(&fcvm_dir), "fuse-backend-rs");
    let digest_b = line_b
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("the second dirty state lost the digest suffix: {line_b}"));
    assert_ne!(
        digest_a, digest_b,
        "two different dirty contents produced the same provenance line ({line_a}); the \
         digest is not reading the diff"
    );

    let _ = fs::remove_dir_all(&scratch);
}

fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Execute the real `build-host-tools` recipe with a stub cargo wrapper and
/// prove the mid-build guard behaviorally: a sibling checkout mutated while
/// "cargo" runs must fail the build with the provenance error, and an
/// unchanged one must build cleanly, so the guard cannot false-positive.
///
/// `build-host-tools` is the cheaper of the two guarded targets (identical
/// guard text, no musl leg); the structural test above pins the same guard on
/// `build`. The stub replaces scripts/cargo-target-run.sh, which is the one
/// seam every cargo command in the Makefile already routes through, so the
/// recipe under test is the real one, expanded by make itself.
#[test]
fn mid_build_dependency_change_fails_the_build() {
    assert!(
        Command::new("make").arg("--version").output().is_ok(),
        "BLOCKED: `make` is not runnable, so this test cannot evaluate the recipe it guards"
    );

    let scratch = std::env::temp_dir().join(format!(
        "fcvm-prov-guard-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&scratch);
    let fcvm_dir = scratch.join("fcvm");
    let dep_dir = scratch.join("fuse-backend-rs");
    fs::create_dir_all(fcvm_dir.join("scripts")).unwrap();
    fs::create_dir_all(&dep_dir).unwrap();
    fs::write(fcvm_dir.join("Makefile"), repo_makefile()).unwrap();
    // The cargo-target-link prerequisite manages btrfs target routing, which
    // a scratch tree has no business touching.
    write_executable(
        &fcvm_dir.join("scripts/cargo-target-link.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    );
    fs::write(dep_dir.join("lib.rs"), "// scratch\n").unwrap();
    chown_tree(&scratch);
    git(&dep_dir, &["init", "-q"]);
    git(&dep_dir, &["add", "lib.rs"]);
    git(&dep_dir, &["commit", "-q", "-m", "scratch commit"]);

    // "cargo" that edits the sibling FUSE tree mid-build: exactly the race
    // the guard exists for, compressed into the build step itself.
    write_executable(
        &fcvm_dir.join("scripts/cargo-target-run.sh"),
        "#!/usr/bin/env bash\n\
         echo \"stub cargo: $*\"\n\
         echo '// mutated mid-build' >> \"$(dirname \"$0\")/../../fuse-backend-rs/lib.rs\"\n",
    );
    let out = run_make(&fcvm_dir, "build-host-tools");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "build-host-tools succeeded although the sibling checkout changed mid-build; the \
         logged provenance describes a tree cargo never saw:\n{all}"
    );
    assert!(
        all.contains("dependency provenance changed during the build"),
        "the build failed for some reason other than the provenance guard:\n{all}"
    );

    // Control: same recipe, nothing mutates, the build must pass and the
    // guard must stay silent.
    git(&dep_dir, &["checkout", "--", "lib.rs"]);
    write_executable(
        &fcvm_dir.join("scripts/cargo-target-run.sh"),
        "#!/usr/bin/env bash\necho \"stub cargo: $*\"\n",
    );
    let out = run_make(&fcvm_dir, "build-host-tools");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && !all.contains("dependency provenance changed"),
        "the guard fired on an unchanged tree, which would fail every honest build:\n{all}"
    );

    let _ = fs::remove_dir_all(&scratch);
}
