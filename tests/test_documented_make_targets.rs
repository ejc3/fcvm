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
            "{goal} reported success with no criterion output directory, so the suite \
             persisted nothing and its numbers cannot be compared against anything. \
             stderr: {}",
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

/// The hugepage pool lock must be owned by the invoking user, not root.
///
/// The hugepage pool lock is one FILE, opened read-only, never with O_CREAT.
///
/// The pool is host-global state shared by `setup-hugepages`,
/// `bench-chromium-fault`, and `bench/chromium/reqbench.sh` (which holds the
/// lock for a phase lifetime), so all of them serialize on one inode:
/// `/mnt/fcvm-btrfs/hugepage-pool.lock`. The recipes take it through
/// `scripts/hugepage-pool-lock.sh`. What went wrong before, in order:
///
/// - The recipes created it with `sudo touch` + `chmod 666`. `fs.protected_regular`
///   refuses an O_CREAT open of a file owned by someone else in a sticky
///   world-writable directory, root included, and util-linux `flock <path>` and
///   bash `<>` both use O_CREAT. So every unprivileged run after the first died:
///
///   ```text
///   ==> Allocating hugepage pool (512 pages = 1024MB)...
///   flock: cannot open lock file /mnt/fcvm-btrfs/hugepage-pool.lock: Permission denied
///   make: *** [Makefile:643: setup-hugepages] Error 66
///   ```
///
/// - `chown` to the invoker at every site raced between two users (codex, #868)
///   and made root `chmod`/`chown` a path the directory owner can replace with a
///   symlink (CodeRabbit, #868).
/// - A lock DIRECTORY had neither problem but changed the lock's identity, so an
///   old reqbench.sh phase still holding the file was no longer serialized
///   against new code holding the directory (codex, #868).
///
/// So: the same file, opened READ-ONLY (`flock(2)` does not care about the open
/// mode, and protected_regular only governs O_CREAT), created atomically when
/// absent (a hard link fails if the name exists, so concurrent creators agree on
/// one inode), and never chmod'ed or chown'ed by anyone.
#[test]
fn hugepage_lock_is_taken_through_the_helper_at_every_site() {
    let makefile = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Makefile"))
        .expect("read Makefile");
    let recipe_lines: Vec<&str> = makefile
        .lines()
        .filter(|l| l.starts_with('\t') && l.contains("hugepage-pool"))
        .collect();
    assert!(
        recipe_lines.len() >= 2,
        "expected both recipes to take the hugepage pool lock; found {recipe_lines:?}"
    );
    for line in &recipe_lines {
        assert!(
            line.contains("scripts/hugepage-pool-lock.sh"),
            "a recipe takes the hugepage pool lock without the helper, so it is one \
             more site that has to get O_CREAT and creation right on its own:\n{line}"
        );
        for verb in ["touch ", "chown ", "chmod ", "mkdir "] {
            assert!(
                !line.contains(verb),
                "a recipe runs `{verb}` around the hugepage lock; creation belongs to \
                 the helper, and a privileged metadata change under the invoker's \
                 directory follows whatever symlink sits there:\n{line}"
            );
        }
    }
    let helper = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/hugepage-pool-lock.sh"
    ))
    .expect("read scripts/hugepage-pool-lock.sh");
    // Code lines only: the helper's comments name the very forms it avoids.
    let helper_code: String = helper
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        helper_code.contains("exec 9<\"$lock\""),
        "the helper must open the lock READ-ONLY (`exec 9<\"$lock\"`); any O_CREAT open \
         (`flock <path>`, `<>`) is refused by protected_regular for a file someone else \
         owns"
    );
    assert!(
        !helper_code.contains("<>") && !helper_code.contains("flock -x -w \"$wait_s\" \"$lock\""),
        "the helper opens the lock with O_CREAT somewhere"
    );
    assert!(
        helper_code.contains("ln \"$tmp\" \"$1\""),
        "the helper must create the lock atomically (hard link onto the final name)"
    );
    assert!(
        helper_code.contains("[ -L \"$lock\" ]"),
        "the helper must refuse a symlink at the lock path before opening it"
    );
    assert!(
        helper_code.contains("stat -Lc %d:%i \"/proc/$$/fd/9\""),
        "the helper must verify, after the open, that fd 9 is the path's own inode"
    );
    for verb in ["chown ", "chmod "] {
        assert!(
            !helper_code.contains(verb),
            "the helper runs `{verb}`; the mode is set by `install -m` on the file it \
             creates, and nothing may follow a path under the user's directory"
        );
    }
    // Privilege is spent only on the fixed shared path, through fixed programs
    // (CodeRabbit on #868): never `sudo bash -c`/`sudo sh -c`, never a
    // caller-supplied path.
    // `$tmp` is the one derived name sudo may see, and only as a sibling of
    // the fixed path.
    assert!(
        helper_code.contains("tmp=\"$(mktemp -u -- \"$default_lock.XXXXXXXX\")\""),
        "the temp name sudo creates must be derived from the fixed shared path"
    );
    for line in helper_code.lines().filter(|l| l.contains("sudo ")) {
        assert!(
            !line.contains("sudo bash -c") && !line.contains("sudo sh -c"),
            "the helper hands sudo a shell string:\n{line}"
        );
        assert!(
            line.contains("$default_lock")
                || line.contains("$default_dir")
                || line.contains("\"$tmp\""),
            "the helper runs sudo on something other than the fixed shared lock:\n{line}"
        );
    }
}

/// Run `program` as an unprivileged stand-in when the tests are root.
///
/// The read-only open matters only for an unprivileged opener: for root,
/// "protected_regular would refuse O_CREAT" and "the owner is untrusted" are
/// the same condition (a file owned by neither root nor the directory's
/// owner). Under the privileged runner the helper is therefore driven as uid
/// 65534 through `setpriv`, from a copy placed where that uid can read it. No
/// sudo anywhere: `make test-fast` shims it to fail, and root needs none.
fn as_unprivileged(program: &str) -> std::process::Command {
    if unsafe { libc::geteuid() } == 0 {
        let mut c = std::process::Command::new("setpriv");
        c.args(["--reuid=65534", "--regid=65534", "--clear-groups"]);
        c.arg(program);
        c
    } else {
        std::process::Command::new(program)
    }
}

/// A copy of the helper an unprivileged stand-in can execute.
fn helper_copy(dir: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/hugepage-pool-lock.sh");
    let dst = dir.join("hugepage-pool-lock.sh");
    std::fs::copy(src, &dst).expect("copy helper");
    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dst.to_string_lossy().into_owned()
}

/// The helper opens the lock read-only, never with O_CREAT.
///
/// Two fixtures, one per lane, each proving the failure it guards against
/// before showing the helper succeed:
/// - as root (the privileged lanes): the production shape, a root-owned lock
///   in a user-owned 1777 directory, opened by uid 65534, where `flock <path>`
///   (O_CREAT) is refused by protected_regular;
/// - as a user (the unprivileged lanes, where no other uid's file can be
///   staged): a 0444 lock of one's own, which a `<>` open refuses.
///
/// A box with the sysctl off fails the root branch instead of passing it.
#[test]
fn hugepage_lock_helper_opens_the_lock_without_o_creat() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("hugepage-pool.lock");
    let root = unsafe { libc::geteuid() } == 0;
    let helper = helper_copy(dir.path());
    if root {
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o1777))
            .expect("chmod 1777");
        std::os::unix::fs::chown(dir.path(), Some(65534), Some(65534)).expect("chown dir");
        std::fs::write(&lock, b"").expect("root-owned lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).expect("0644");
        let refused = as_unprivileged("flock")
            .args(["-x", "-n"])
            .arg(&lock)
            .arg("true")
            .status()
            .expect("flock by path as uid 65534");
        assert!(
            !refused.success(),
            "fixture does not reproduce: `flock <path>` (O_CREAT) on a root-owned lock in \
             a user-owned sticky directory succeeded for uid 65534, so fs.protected_regular \
             is off on this box"
        );
    } else {
        std::fs::write(&lock, b"").expect("lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o444)).expect("0444");
        let refused = std::process::Command::new("bash")
            .args(["-c", "exec 9<>\"$1\"", "_"])
            .arg(&lock)
            .status()
            .expect("run <> open");
        assert!(
            !refused.success(),
            "fixture does not reproduce: a `<>` open of a 0444 lock succeeded"
        );
    }

    let out = as_unprivileged(&helper)
        .env("HUGEPAGE_POOL_LOCK", &lock)
        .env("HUGEPAGE_POOL_LOCK_WAIT", "2")
        .args(["sh", "-c", "echo took-it"])
        .output()
        .expect("run helper");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && text.contains("took-it"),
        "the helper failed to take a lock it only needs to open read-only:\n{text}"
    );
    if root {
        // Hand the directory back so the tempdir can be removed.
        let _ = std::os::unix::fs::chown(dir.path(), Some(0), Some(0));
    }
}

/// Every writer of the hugepage pool takes the shared lock (codex on #868).
///
/// The pool is host-global, and reqbench.sh holds the lock SHARED for a whole
/// phase precisely so that nobody shrinks the pool under its clones. A writer
/// that skips the lock (`bench-hugepages*` restoring the pool to zero, bench.sh
/// growing or restoring it) defeats that lease. So every line that writes
/// `nr_hugepages`, in the Makefile and in every bench/scripts shell script, must
/// be an argument of `scripts/hugepage-pool-lock.sh`, on the same line or on an
/// earlier line of the same backslash-continued command. reqbench.sh writes
/// through `$HUGEPAGE_POOL_FILE` under its own lease, which
/// bench/chromium/test_reqbench.py pins (`test_pool_grow_respects_exclusive_holder`),
/// so this enumeration deliberately keys on the literal path.
#[test]
fn every_hugepage_pool_writer_takes_the_lock() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("Makefile")];
    for dir in ["bench/chromium", "scripts"] {
        for entry in std::fs::read_dir(root.join(dir)).expect(dir) {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|e| e == "sh") {
                files.push(path);
            }
        }
    }
    let writes_pool = |line: &str| {
        let code = line.trim_start();
        !code.starts_with('#')
            && line.contains("nr_hugepages")
            && (line.contains("> /proc/sys/vm/nr_hugepages")
                || line.contains("tee")
                || line.contains("sysctl"))
    };
    let mut writers = 0;
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !writes_pool(line) {
                continue;
            }
            writers += 1;
            let mut covered = line.contains("hugepage-pool-lock.sh");
            let mut j = i;
            while !covered && j > 0 && lines[j - 1].trim_end().ends_with('\\') {
                j -= 1;
                covered = lines[j].contains("hugepage-pool-lock.sh");
            }
            assert!(
                covered,
                "{}:{}: writes nr_hugepages outside scripts/hugepage-pool-lock.sh; a bench \
                 phase holding the pool lease can have the pool shrunk under its clones:\n{line}",
                file.strip_prefix(root).unwrap().display(),
                i + 1
            );
        }
    }
    assert!(
        writers >= 8,
        "expected the six Makefile writers and bench.sh's two; found {writers}. If a \
         writer moved, move this pin with it."
    );
}

/// An overridden lock path is never created with privileges (CodeRabbit on
/// #868): `HUGEPAGE_POOL_LOCK` is a test knob, and a caller-supplied path must
/// not reach `sudo`. Only the fixed default path is created as root, through
/// fixed programs. A `sudo` stub first on PATH records any escalation.
#[test]
fn hugepage_lock_helper_never_escalates_for_an_overridden_path() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("bin");
    let marker = dir.path().join("sudo-was-called");
    std::fs::write(
        bin.join("sudo"),
        format!("#!/bin/sh\ntouch {}\nexit 1\n", marker.display()),
    )
    .expect("stub");
    std::fs::set_permissions(bin.join("sudo"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/hugepage-pool-lock.sh");
    let run = |lock: &std::path::Path| {
        let out = std::process::Command::new(helper)
            .env("PATH", &path)
            .env("HUGEPAGE_POOL_LOCK", lock)
            .env("HUGEPAGE_POOL_LOCK_WAIT", "2")
            .args(["sh", "-c", "echo took-it"])
            .output()
            .expect("run helper");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success() && text.contains("took-it"), text)
    };

    // Absent lock in a writable directory: created unprivileged.
    let (ok, text) = run(&dir.path().join("hugepage-pool.lock"));
    assert!(
        ok,
        "the helper failed to create an overridden lock unprivileged:\n{text}"
    );
    assert!(
        !marker.exists(),
        "the helper reached for sudo to create a caller-supplied path:\n{text}"
    );

    // Absent lock where nothing can be created: refused, still without sudo.
    let (ok, text) = run(std::path::Path::new("/proc/self/hugepage-pool.lock"));
    assert!(
        !ok,
        "the helper claimed to lock a path it cannot create:\n{text}"
    );
    assert!(
        !marker.exists(),
        "the helper escalated to create a caller-supplied path under /proc:\n{text}"
    );
}

/// A symlink at the lock path is refused, not followed (codex, CodeRabbit on
/// #868): whoever planted it can repoint it under a holder, and the next
/// caller would lock a different inode.
#[test]
fn hugepage_lock_helper_refuses_a_planted_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::write(&elsewhere, b"").expect("target file");
    let lock = dir.path().join("hugepage-pool.lock");
    std::os::unix::fs::symlink(&elsewhere, &lock).expect("planted symlink");
    let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/hugepage-pool-lock.sh");
    let out = std::process::Command::new(helper)
        .env("HUGEPAGE_POOL_LOCK", &lock)
        .env("HUGEPAGE_POOL_LOCK_WAIT", "2")
        .args(["sh", "-c", "echo took-it"])
        .output()
        .expect("run helper");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success() && !text.contains("took-it"),
        "the helper locked THROUGH a planted symlink instead of refusing it:\n{text}"
    );
    assert!(
        text.contains("symlink"),
        "the refusal must say why:\n{text}"
    );
    assert!(
        lock.is_symlink(),
        "the helper must not replace an entry it did not create"
    );
}

/// A lock owned by anyone but root, the invoking user, or the directory's
/// owner is refused: that owner can unlink and recreate it under a holder.
///
/// Privileged lanes only: staging a file owned by another uid takes root, and
/// `make test-fast` shims sudo to fail.
#[cfg(feature = "privileged-tests")]
#[test]
fn hugepage_lock_helper_refuses_a_lock_another_user_can_recreate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("hugepage-pool.lock");
    std::fs::write(&lock, b"").expect("lock");
    std::os::unix::fs::chown(&lock, Some(65534), Some(65534))
        .expect("stage a lock owned by another uid (this test runs as root)");
    let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/hugepage-pool-lock.sh");
    let out = std::process::Command::new(helper)
        .env("HUGEPAGE_POOL_LOCK", &lock)
        .env("HUGEPAGE_POOL_LOCK_WAIT", "2")
        .args(["sh", "-c", "echo took-it"])
        .output()
        .expect("run helper");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success() && !text.contains("took-it"),
        "the helper trusted a lock owned by uid 65534, who can recreate it under a \
         holder:\n{text}"
    );
    assert!(
        text.contains("owned by uid 65534"),
        "the refusal must say why:\n{text}"
    );
}

/// Concurrent first-time creators end up on ONE inode, and the helper keeps
/// the lock for the whole command (it must not `exec` into it: sudo closes
/// inherited descriptors, which would release the lock right before the
/// privileged pool write it exists to serialize).
#[test]
fn hugepage_lock_helper_holds_the_lock_while_the_command_runs() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("hugepage-pool.lock");
    let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/hugepage-pool-lock.sh");

    // Eight racers create the lock at once and each reports the inode behind
    // its fd 9.
    let racers: Vec<_> = (0..8)
        .map(|_| {
            std::process::Command::new(helper)
                .env("HUGEPAGE_POOL_LOCK", &lock)
                .env("HUGEPAGE_POOL_LOCK_WAIT", "10")
                .args(["sh", "-c", "stat -L -c %i /proc/self/fd/9"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn racer")
        })
        .collect();
    let mut inodes = std::collections::BTreeSet::new();
    for racer in racers {
        let out = racer.wait_with_output().expect("racer exit");
        assert!(out.status.success(), "a racer failed: {out:?}");
        inodes.insert(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    assert_eq!(
        inodes.len(),
        1,
        "concurrent creators locked different inodes {inodes:?}; the lock file was \
         replaced under a holder"
    );
    let meta = std::fs::metadata(&lock).expect("lock exists");
    assert_eq!(meta.permissions().mode() & 0o777, 0o644, "lock mode");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != "hugepage-pool.lock")
        .collect();
    assert!(
        leftovers.is_empty(),
        "creation left temp files behind: {leftovers:?}"
    );

    // Hold the lock via the helper; a by-path contender must be refused until
    // the held command exits.
    let mut holder = std::process::Command::new(helper)
        .env("HUGEPAGE_POOL_LOCK", &lock)
        .args(["sh", "-c", "echo held; read -r _"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn holder");
    {
        use std::io::BufRead as _;
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .expect("holder line");
        assert_eq!(line.trim(), "held");
    }
    let contended = std::process::Command::new("bash")
        .args(["-c", "exec 9<\"$1\" && flock -x -n 9", "_"])
        .arg(&lock)
        .status()
        .expect("contender");
    assert!(
        !contended.success(),
        "a contender took the lock while the helper's command was still running"
    );
    drop(holder.stdin.take());
    let _ = holder.wait();
    let free = std::process::Command::new("bash")
        .args(["-c", "exec 9<\"$1\" && flock -x -n 9", "_"])
        .arg(&lock)
        .status()
        .expect("taker");
    assert!(
        free.success(),
        "the lock stayed held after the helper's command exited"
    );
}

/// Every recipe that creates the hugepage lock must be valid shell AS MAKE RUNS IT.
///
/// #868's first revision put explanatory `#` lines inside a backslash-continued
/// recipe. In a Makefile a tab-indented `#` line is shell text, and a shell
/// comment swallows the trailing backslash, cutting the command in half:
///
/// ```text
/// /bin/bash: -c: line 8: syntax error: unexpected end of file
/// make: *** [Makefile:643: setup-hugepages] Error 2
/// ```
///
/// `make -n` printed it happily, and a first version of THIS test -- `make -n`
/// piped into `bash -n` -- passed against the broken Makefile, because what make
/// prints is not what make hands the shell. So the recipe is run through make
/// itself with `SHELL='bash -n'`: make invokes the shell exactly as it would in
/// CI, and the shell only parses. The threshold is forced high so the allocating
/// branch (the one with the lock) is the code path taken. Nothing is executed,
/// so no sudo, no /proc write.
#[test]
fn hugepage_lock_recipes_are_valid_shell_as_make_runs_them() {
    let repo = concat!(env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new("make")
        .args([
            "setup-hugepages",
            "HUGEPAGE_POOL_TESTS=99999",
            "SHELL=bash -n",
        ])
        .current_dir(repo)
        .output()
        .expect("run make with a syntax-only shell");
    assert!(
        out.status.success(),
        "the setup-hugepages recipe is not valid shell as make runs it:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
