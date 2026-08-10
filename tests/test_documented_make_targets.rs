//! Every `make` target PERFORMANCE.md tells a reader to run must exist.
//!
//! PERFORMANCE.md's "Quick Reference" listed `make bench-quick`,
//! `bench-throughput`, `bench-operations` and `bench-protocol`. None of the
//! four existed: each died with `No rule to make target` at exit 2. That is
//! the same shape as the `_test-unit` FILTER defect (an advertised interface
//! that silently was not there), and it is worse in a performance guide,
//! because the reader's first move after reading a benchmark table is to run
//! the command that reproduces it.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
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
#[test]
fn metadata_benchmarks_name_the_cache_they_measure() {
    let src = repo_file("fuse-pipe/benches/operations.rs");

    for op in ["bench_getattr", "bench_lookup"] {
        let start = src
            .find(&format!("fn {op}("))
            .unwrap_or_else(|| panic!("operations.rs has no {op}"));
        let body = &src[start..];
        let end = body.find("\nfn ").unwrap_or(body.len());
        let body = &body[..end];

        assert!(
            body.contains("_uncached"),
            "{op} has no `_uncached` case, so nothing in it measures a FUSE round trip: a \
             repeated path is answered by the kernel's attribute/dentry cache"
        );
        assert!(
            body.contains("pool_file("),
            "{op}'s uncached case must walk the distinct-file pool; restating one path \
             measures the cache no matter what the case is called"
        );
        assert!(
            !body.contains("\"fuse_256_readers\""),
            "{op} still has a case named plainly `fuse_256_readers`. Name it for the cache \
             it measures (`_attr_cache_hit` / `_dentry_cache_hit`) or make it uncached; an \
             unqualified name reads as the round-trip cost and is off by ~100x."
        );
    }
}
