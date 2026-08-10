//! The benchmark targets have to exist, run, and run one at a time.
//!
//! PERFORMANCE.md's "Quick Reference" listed `make bench-quick`,
//! `bench-throughput`, `bench-operations` and `bench-protocol`. None of the
//! four existed: each died with `No rule to make target` at exit 2. That is
//! the same shape as the `_test-unit` FILTER defect (an advertised interface
//! that silently was not there), and it is worse in a performance guide,
//! because the reader's first move after reading a benchmark table is to run
//! the command that reproduces it.
//!
//! The targets that replaced them were verified with `make -n`, which prints a
//! command line without asking cargo whether it would accept one. Four of the
//! tests below therefore drive real `make` against the real Makefile with a
//! stub standing in for cargo, so a recipe that make expands happily but cargo
//! rejects, or that runs two benchmark suites at once, or that writes its
//! results somewhere its own ownership repair never looks, fails here instead
//! of on a reader's box.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The identity `make` must run as, when the harness itself is root.
///
/// The Host-Root CI legs run this whole test binary as root (their nextest
/// runner is `sudo -E`), and the Makefile refuses root on the host: its guard
/// exists so build plumbing can never leave root-owned files in `target/`.
/// A bypass variable would disarm that guard for real users, so the harness
/// instead runs `make` as the user sudo recorded — the identity the guard
/// tells a person to use. Inside containers the guard already permits root
/// and no drop happens.
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

/// Hand the scratch tree to the user `make` will run as, so the cargo stub
/// can record into it.
fn chown_tree(root: &Path, uid: u32, gid: u32) {
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

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Collect `make <target>` mentions from a document.
fn documented_make_targets(doc: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in doc.lines() {
        let mut rest = line;
        while let Some(idx) = rest.find("make ") {
            rest = &rest[idx + "make ".len()..];
            let target: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !target.is_empty() {
                found.insert(target);
            }
        }
    }
    found
}

/// Targets the Makefile actually defines, including `.PHONY`-only ones.
fn makefile_targets(makefile: &str) -> BTreeSet<String> {
    makefile
        .lines()
        .filter(|line| !line.starts_with(['\t', ' ', '#']))
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, sep)| !name.is_empty() && !sep.starts_with('='))
        .flat_map(|(names, _)| names.split_whitespace().map(str::to_owned))
        .filter(|name| !name.starts_with('.') && !name.contains('$') && !name.contains('%'))
        .collect()
}

#[test]
fn performance_guide_only_names_targets_that_exist() {
    let makefile = makefile_targets(&repo_file("Makefile"));
    let documented = documented_make_targets(&repo_file("PERFORMANCE.md"));

    assert!(
        documented.contains("bench"),
        "the scan found no `make bench` in PERFORMANCE.md, so it is not reading the document \
         it thinks it is and cannot fail for the right reason"
    );

    let missing: Vec<&String> = documented.difference(&makefile).collect();
    assert!(
        missing.is_empty(),
        "PERFORMANCE.md tells the reader to run make targets that do not exist: {missing:?}. \
         Each one dies with `No rule to make target` at exit 2."
    );
}

#[test]
fn readme_only_names_targets_that_exist() {
    let makefile = makefile_targets(&repo_file("Makefile"));
    let documented = documented_make_targets(&repo_file("README.md"));

    assert!(
        !documented.is_empty(),
        "the scan found no `make` targets in README.md, so it cannot fail for the right reason"
    );

    let missing: Vec<&String> = documented.difference(&makefile).collect();
    assert!(
        missing.is_empty(),
        "README.md tells the reader to run make targets that do not exist: {missing:?}"
    );
}

/// The metadata benchmarks must state which cache they are measuring.
///
/// `single_op/getattr` and `single_op/lookup` used to stat one fixed path in a
/// loop. A FUSE mount defaults to a 1s `attr_timeout`/`entry_timeout`, so after
/// the first call the kernel answered from its attribute and dentry caches and
/// the server was never contacted. Measured on Graviton3 the two reported
/// 1.06µs against a 1.02µs host baseline, and PERFORMANCE.md published that as
/// "metadata ops (getattr, lookup) have ~5% overhead" — a claim about fuse-pipe
/// drawn from a measurement fuse-pipe never took part in. Every operation that
/// does reach the server on the same box costs 40-56x the host, not 1.05x.
///
/// Both shapes are worth publishing; they just have to be named. This keeps a
/// bare `fuse_256_readers` case from creeping back in, since its name would
/// claim to be the round trip while measuring the cache.
///
/// Naming is not enough on its own, so each round-trip case also has to carry
/// the runtime check that it reaches the server. The first attempt at one did
/// not: it walked a pool of 20,000 distinct files, which produces cold lookups
/// only until the walk wraps, and nothing in it could tell whether it had.
#[test]
fn metadata_benchmarks_name_the_cache_they_measure() {
    let src = repo_file("fuse-pipe/benches/operations.rs");

    for (op, check) in [
        ("bench_getattr", "assert_forced_getattr_reaches_server("),
        ("bench_lookup", "assert_missing_lookup_reaches_server("),
    ] {
        let start = src
            .find(&format!("fn {op}("))
            .unwrap_or_else(|| panic!("operations.rs has no {op}"));
        let body = &src[start..];
        let end = body.find("\nfn ").unwrap_or(body.len());
        let body = &body[..end];

        assert!(
            body.contains("_cache_hit"),
            "{op} no longer names its cached case for the cache it measures"
        );
        assert!(
            body.contains("_round_trip"),
            "{op} has no `_round_trip` case, so nothing in it measures a FUSE round trip: a \
             repeated path is answered by the kernel's attribute/dentry cache"
        );
        assert!(
            body.contains(check),
            "{op}'s round-trip case must call {check}..) first. A case that is merely named \
             for the round trip and never checked is how the cached measurement got published \
             the first time."
        );
        assert!(
            !body.contains("\"fuse_256_readers\""),
            "{op} still has a case named plainly `fuse_256_readers`. Name it for the cache \
             it measures (`_attr_cache_hit` / `_dentry_cache_hit`) or make it a checked round \
             trip; an unqualified name reads as the round-trip cost and is off by ~300x."
        );
    }
}

/// No metadata case may depend on a finite pool of distinct paths.
///
/// `sample_size` bounds criterion's samples, not its `iter` calls, so a pool
/// sized against the sample count does not bound anything. Measured on the
/// merged version at criterion's defaults, `single_op/getattr`:
///
///     fuse_256_readers_attr_cache_hit: 17M iterations   [297.38 ns .. 310.12 ns]
///     fuse_256_readers_uncached:       20k iterations   [181.50 µs .. 465.15 µs]
///
/// 20,000 measurement iterations against a 20,000-file pool, on top of a
/// warm-up phase that is untimed, uncounted and had already walked thousands of
/// entries. The walk wraps. Whether a revisited inode is then still cached
/// depends on how long one pass takes against the mount's 1s entry_timeout,
/// which is a property of the host and the flags rather than of the benchmark.
/// The case reports whatever blend it got without being able to say so, and
/// 17M against 20k is the scale it degrades toward as more revisits land inside
/// the timeout.
///
/// The replacements have no pool and no such dependence: a forced statx cannot
/// be served from the attribute cache, and an absent name is never cached.
#[test]
fn metadata_benchmarks_cannot_wrap_a_fixed_pool() {
    let src = repo_file("fuse-pipe/benches/operations.rs");
    for banned in ["METADATA_POOL", "pool_file("] {
        assert!(
            !src.contains(banned),
            "operations.rs is back on a fixed path pool ({banned}). A pool only produces cold \
             metadata operations until it wraps, and criterion runs orders of magnitude more \
             iterations than a pool can hold."
        );
    }
}

// ---------------------------------------------------------------------------
// Harness: drive the real Makefile with a stub in place of cargo.
// ---------------------------------------------------------------------------

/// Scratch directory for one harness run.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fcvm-make-harness-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).expect("create scratch bin");
        fs::create_dir_all(dir.join("rec")).expect("create scratch rec");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write_exec(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.0.join("bin").join(name);
        fs::write(&path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// What one harness run observed.
struct MakeRun {
    ok: bool,
    stderr: String,
    /// Arguments of every stub-cargo invocation, in start order.
    invocations: Vec<String>,
    /// One entry per stub-cargo invocation that started while another was
    /// still running.
    overlaps: Vec<String>,
    /// `CRITERION_HOME` as each stub-cargo invocation received it.
    criterion_homes: Vec<String>,
}

/// Drive `make` against the real Makefile with cargo, find and sudo stubbed.
///
/// `CARGO_BIN` is the toolchain-selection hook the Makefile leaves overridable;
/// `CARGO` itself is `override`n, so the stub still runs through the real
/// `cargo-target-run.sh` wrapper. Only `build` is neutralised, so nothing here
/// compiles anything.
///
/// `find` and `sudo` are stubbed on PATH in every run, for two reasons. They
/// set up the ownership-repair precondition without needing an actually
/// root-owned file, and they make it impossible for a unit test to `chown -R`
/// a developer's real criterion output.
struct Harness<'a> {
    name: &'a str,
    goals: &'a [&'a str],
    make_flags: &'a [&'a str],
    /// Point `CRITERION_HOME` at a scratch directory. Cleared by
    /// [`Harness::keep_default_criterion_home`] for the test that checks what
    /// the Makefile picks on its own.
    scratch_criterion_home: bool,
    find_reports_root_owned: bool,
    sudo_succeeds: bool,
    /// Leave `CRITERION_HOME` uncreated, standing in for a criterion that
    /// could not write it (a read-only or root-squashed mount).
    criterion_home_missing: bool,
}

impl<'a> Harness<'a> {
    fn new(name: &'a str, goals: &'a [&'a str]) -> Self {
        Harness {
            name,
            goals,
            make_flags: &[],
            scratch_criterion_home: true,
            find_reports_root_owned: false,
            sudo_succeeds: true,
            criterion_home_missing: false,
        }
    }

    fn without_criterion_output(mut self) -> Self {
        self.criterion_home_missing = true;
        self
    }

    fn make_flags(mut self, flags: &'a [&'a str]) -> Self {
        self.make_flags = flags;
        self
    }

    fn keep_default_criterion_home(mut self) -> Self {
        self.scratch_criterion_home = false;
        self
    }

    fn with_root_owned_output(mut self) -> Self {
        self.find_reports_root_owned = true;
        self
    }

    fn with_failing_sudo(mut self) -> Self {
        self.sudo_succeeds = false;
        self
    }

    fn run(self) -> MakeRun {
        let make = which("make").expect(
            "BLOCKED: `make` is not on PATH, so this test cannot evaluate the Makefile it guards",
        );

        let scratch = Scratch::new(self.name);
        let rec = scratch.path().join("rec");
        let criterion_home = scratch.path().join("criterion");
        if !self.criterion_home_missing {
            fs::create_dir_all(&criterion_home).unwrap();
        }

        scratch.write_exec(
            "cargo-stub.sh",
            "#!/bin/sh\n\
             # Stands in for cargo inside the bench recipes: records its arguments\n\
             # and its criterion home, and stays alive long enough for an\n\
             # overlapping suite to be visible.\n\
             set -u\n\
             printf '%s\\n' \"$*\" >> \"$BENCH_RECORD_DIR/argv.log\"\n\
             printf '%s\\n' \"${CRITERION_HOME:-<unset>}\" >> \"$BENCH_RECORD_DIR/home.log\"\n\
             # A real `cargo bench` writes criterion output; the persistence guard in\n\
             # the bench recipes checks for it. BENCH_STUB_NO_OUTPUT stands in for the\n\
             # criterion that logged a persistence failure and still exited 0.\n\
             [ -n \"${BENCH_STUB_NO_OUTPUT:-}\" ] || mkdir -p \"${CRITERION_HOME:-.}\"\n\
             live=\"$BENCH_RECORD_DIR/live.$$\"\n\
             : > \"$live\"\n\
             if [ \"$(ls \"$BENCH_RECORD_DIR\"/live.* | wc -l)\" -gt 1 ]; then\n\
             \tprintf '%s\\n' \"$*\" >> \"$BENCH_RECORD_DIR/overlap.log\"\n\
             fi\n\
             sleep 0.5\n\
             rm -f \"$live\"\n\
             exit 0\n",
        );
        scratch.write_exec(
            "find",
            if self.find_reports_root_owned {
                "#!/bin/sh\necho criterion/single_op_getattr\nexit 0\n"
            } else {
                "#!/bin/sh\nexit 0\n"
            },
        );
        scratch.write_exec(
            "sudo",
            if self.sudo_succeeds {
                "#!/bin/sh\nexit 0\n"
            } else {
                "#!/bin/sh\necho 'sudo: a password is required' >&2\nexit 1\n"
            },
        );
        // Neutralise everything below the bench recipes. `build` keeps this
        // from compiling anything, and `cargo-target-link` must go too: it
        // takes the target generation's lease EXCLUSIVELY, while the `cargo
        // nextest` running this test holds that same lease shared for its
        // whole lifetime, so a harness that calls it deadlocks against its own
        // test runner. Measured: four harness runs plus
        // tests/test_cargo_target_link.rs left a `flock -x` on
        // <checkout>/target parked behind the runner's `flock -s`, and all
        // five tests hit nextest's 120s timeout.
        //
        // Skipping it is sound because `target/` already exists by then:
        // nextest itself reaches these tests through cargo-target-run.sh,
        // which refuses to start without it.
        assert!(
            repo_root().join("target").is_dir(),
            "BLOCKED: {} does not exist, so this harness cannot run make without publishing a \
             target generation, which would deadlock against the test runner's own lease",
            repo_root().join("target").display()
        );
        fs::write(
            scratch.path().join("stub.mk"),
            "build:\n\t@:\ncargo-target-link:\n\t@:\n",
        )
        .unwrap();

        let path = format!(
            "{}:{}",
            scratch.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );

        let mut cmd = Command::new(make);
        cmd.current_dir(repo_root())
            .arg("-f")
            .arg("Makefile")
            .arg("-f")
            .arg(scratch.path().join("stub.mk"))
            .args(self.make_flags)
            .args(self.goals)
            .arg(format!(
                "CARGO_BIN={}",
                scratch.path().join("bin/cargo-stub.sh").display()
            ));
        if self.scratch_criterion_home {
            cmd.arg(format!("CRITERION_HOME={}", criterion_home.display()));
        }
        // A -j inherited from the make that started the test suite would
        // otherwise decide what these tests measure.
        cmd.env_remove("MAKEFLAGS")
            .env_remove("MFLAGS")
            .env_remove("CRITERION_HOME")
            .env("PATH", path)
            .env("BENCH_RECORD_DIR", &rec);
        if self.criterion_home_missing {
            cmd.env("BENCH_STUB_NO_OUTPUT", "1");
        }
        if let Some((uid, gid)) = harness_user() {
            use std::os::unix::process::CommandExt;
            chown_tree(scratch.path(), uid, gid);
            cmd.uid(uid).gid(gid);
        }

        let out = cmd.output().expect("run make");
        let read = |f: &str| fs::read_to_string(rec.join(f)).unwrap_or_default();

        MakeRun {
            ok: out.status.success(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            invocations: read("argv.log").lines().map(str::to_owned).collect(),
            overlaps: read("overlap.log").lines().map(str::to_owned).collect(),
            criterion_homes: read("home.log").lines().map(str::to_owned).collect(),
        }
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// Criterion's flags have to reach the bench binary, not cargo.
///
/// `bench-quick` expanded them straight into the cargo command line, so it
/// could not run at all: cargo rejects the first one before any benchmark
/// starts. It shipped because it was checked with `make -n`, which prints a
/// command line without asking cargo whether it would accept one.
#[test]
fn bench_quick_passes_criterion_flags_after_the_cargo_separator() {
    // Positive control: this is why the separator matters. Run in an empty
    // directory, where the argument parser still rejects the flag before cargo
    // has looked for a manifest.
    let cargo = which("cargo").expect("BLOCKED: `cargo` is not on PATH");
    let empty = Scratch::new("cargo-control");
    let control = Command::new(&cargo)
        .current_dir(empty.path())
        .args(["bench", "--bench", "throughput", "--sample-size", "10"])
        // CI exports CARGO_TERM_COLOR=always, and cargo then wraps the argument
        // name in ANSI codes INSIDE the quotes — `'\x1b[1m\x1b[33m--sample-size...'`
        // — which broke the substring match below on every CI leg while passing
        // locally, where cargo sees a pipe and turns color off on its own.
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("run cargo");
    let control_err = String::from_utf8_lossy(&control.stderr);
    // The rejection MESSAGE is the property, not the exit status: a `cargo` shim on
    // PATH can forward the diagnostic and still exit 0, which is what CI has. Keying
    // the control on the status made this test pass locally and fail there.
    assert!(
        control_err.contains("unexpected argument '--sample-size'"),
        "cargo no longer rejects criterion flags given to it directly, so this test is guarding \
         a rule that no longer holds. cargo at {cargo} exited {status:?} and said: {control_err}",
        cargo = cargo.display(),
        status = control.status.code(),
    );

    let quick = Harness::new("bench-quick", &["bench-quick"]).run();
    assert!(
        quick.ok,
        "make bench-quick failed: {stderr}",
        stderr = quick.stderr
    );
    assert_eq!(
        quick.invocations.len(),
        3,
        "expected throughput, operations and protocol; got {:?}",
        quick.invocations
    );
    for argv in &quick.invocations {
        let sep = argv.find(" -- ").unwrap_or(usize::MAX);
        let flag = argv.find("--sample-size").unwrap_or_else(|| {
            panic!("bench-quick did not forward its criterion flags at all: {argv}")
        });
        assert!(
            sep < flag,
            "bench-quick puts criterion flags on cargo's own command line, where cargo rejects \
             them with `unexpected argument`: {argv}"
        );
    }

    // And the default path carries no separator with nothing behind it.
    let plain = Harness::new("bench-plain", &["bench"]).run();
    assert!(
        plain.ok,
        "make bench failed: {stderr}",
        stderr = plain.stderr
    );
    for argv in &plain.invocations {
        let argv = argv.trim_end();
        assert!(
            !argv.contains(" -- ") && !argv.ends_with(" --"),
            "plain `make bench` should pass nothing to the bench binary, separator included: \
             {argv}"
        );
    }
}

/// `make -j bench` must not start two benchmark suites at once.
///
/// As prerequisites of an empty `bench` target they did: measured with this
/// harness at `-j8`, all three ran together. Concurrent suites time each other,
/// and the unprivileged protocol suite races the privileged suites for
/// ownership of target/criterion. The recipe comment saying "do not run this
/// under -j" was not a control, and a -j can also arrive unasked through
/// MAKEFLAGS.
#[test]
fn parallel_make_never_runs_two_benchmark_suites_at_once() {
    let run = Harness::new("bench-parallel", &["bench"])
        .make_flags(&["-j8"])
        .run();

    assert!(
        run.ok,
        "make -j8 bench failed: {stderr}",
        stderr = run.stderr
    );
    assert_eq!(
        run.invocations.len(),
        3,
        "the harness saw {} suites, not 3, so it is not exercising `bench`: {:?}",
        run.invocations.len(),
        run.invocations
    );
    assert!(
        run.overlaps.is_empty(),
        "`make -j8 bench` ran benchmark suites concurrently: {:?}",
        run.overlaps
    );
}

/// Every suite has to agree on where criterion writes, and it has to be a path
/// the recipes name rather than one criterion picks.
///
/// Criterion's default is `$CARGO_TARGET_DIR/criterion`, and this Makefile sets
/// `CARGO_TARGET_DIR` to the relative string `target`, which criterion resolves
/// inside the bench binary, whose working directory cargo sets to the package
/// root. Every suite was writing to `fuse-pipe/target/criterion` while the
/// ownership repair scanned `target/` at the repo root and found nothing to do.
/// Measured on that arrangement: `make bench-quick` finished at exit 0 leaving
/// `single_op_getattr`, `parallel_reads` and `parallel_writes` root-owned, and
/// the next unprivileged `make bench-protocol` printed `Failed to access file
/// "target/criterion/serialize_lookup_request/new/sample.json": Permission
/// denied (os error 13)` for every result file, also at exit 0, because
/// criterion reports the failure and carries on.
#[test]
fn bench_recipes_pin_criterion_output_to_an_absolute_path() {
    let run = Harness::new("criterion-home", &["bench"])
        .keep_default_criterion_home()
        .run();
    assert!(run.ok, "make bench failed: {}", run.stderr);
    assert_eq!(
        run.criterion_homes.len(),
        3,
        "expected one criterion home per suite; got {:?}",
        run.criterion_homes
    );

    let target = repo_root().join("target/criterion");
    for home in &run.criterion_homes {
        assert_eq!(
            Path::new(home),
            target,
            "a bench recipe leaves criterion to choose its own output directory. Its second \
             choice is $CARGO_TARGET_DIR/criterion, relative to the bench binary's working \
             directory, which is the package root and not the repo root."
        );
    }
}

/// A failed ownership repair must fail the target.
///
/// The privileged suites kept only the benchmark's exit status, so a `chown`
/// that could not run reported the suite green and left the criterion output
/// root-owned, which is the state the repair exists to prevent and the one that
/// makes the next `bench-protocol` persist nothing and `_test-root` refuse to
/// start.
#[test]
fn a_failed_ownership_repair_fails_the_privileged_bench_target() {
    let repair_failed = Harness::new("repair-fails", &["bench-throughput"])
        .with_root_owned_output()
        .with_failing_sudo()
        .run();
    assert!(
        !repair_failed.ok,
        "bench-throughput reported success while the ownership repair failed, leaving the \
         criterion output root-owned. stderr: {}",
        repair_failed.stderr
    );
    assert!(
        repair_failed.stderr.contains("still root-owned"),
        "the failure should say what is now wrong with the criterion output; stderr was: {}",
        repair_failed.stderr
    );

    // Controls, so the failure above is attributable to the repair and not to
    // the harness: the same run passes when the repair succeeds, and when
    // there is nothing to repair.
    let repair_worked = Harness::new("repair-works", &["bench-throughput"])
        .with_root_owned_output()
        .run();
    assert!(
        repair_worked.ok,
        "bench-throughput failed even though the repair succeeded: {}",
        repair_worked.stderr
    );

    let nothing_to_repair = Harness::new("repair-skipped", &["bench-throughput"])
        .with_failing_sudo()
        .run();
    assert!(
        nothing_to_repair.ok,
        "bench-throughput failed with no root-owned files to repair: {}",
        nothing_to_repair.stderr
    );
}

/// A criterion run that persisted nothing must not report success.
///
/// criterion logs its persistence failures and still exits 0, so if
/// `CRITERION_HOME` points somewhere it cannot create — a read-only or
/// root-squashed mount — the suite prints timings, writes no `sample.json`,
/// no `estimates.json` and no baseline, and the recipe passes that 0 straight
/// through. Nothing can then be compared against the run and no regression can
/// ever be reported against it, which is the same shape as the defect this
/// file already guards on the ownership side: a benchmark that looks like it
/// ran and left nothing behind.
#[test]
fn a_bench_that_persisted_nothing_fails_its_target() {
    for goal in ["bench-throughput", "bench-protocol"] {
        let missing = Harness::new("criterion-home-missing", &[goal])
            .without_criterion_output()
            .run();
        assert!(
            !missing.ok,
            "{goal} reported success with no criterion output directory, so the suite              persisted nothing and its numbers cannot be compared against anything.              stderr: {}",
            missing.stderr
        );
        assert!(
            missing.stderr.contains("does not exist"),
            "{goal}'s failure must name the missing criterion output; stderr was: {}",
            missing.stderr
        );

        // Control: the same run passes when criterion did write its directory,
        // so the failure above is attributable to the missing output and not
        // to the harness.
        let present = Harness::new("criterion-home-present", &[goal]).run();
        assert!(
            present.ok,
            "{goal} failed even though the criterion output directory exists: {}",
            present.stderr
        );
    }
}
