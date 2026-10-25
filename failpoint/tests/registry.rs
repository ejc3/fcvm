//! The registry in `points.rs` names exactly the points the sources hit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
    {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Names in `failpoint::hit("x")`, `hit_async("x")` and `hit_scoped("x")` calls. A call
/// quoted inside a string literal (`"failpoint::hit_async(\"x\")"`, in source-order tests)
/// has a backslash before its quote and is not a call.
fn hit_names(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (at, _) in source.match_indices("failpoint::hit") {
        let rest = &source[at + "failpoint::hit".len()..];
        let rest = rest
            .strip_prefix("_async")
            .or_else(|| rest.strip_prefix("_scoped"))
            .unwrap_or(rest);
        if let Some(rest) = rest.strip_prefix("(\"") {
            if let Some(end) = rest.find('"') {
                names.push(rest[..end].to_string());
            }
        }
    }
    names
}

fn points_in(dirs: &[&str]) -> BTreeSet<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for dir in dirs {
        rust_files(&root.join(dir), &mut files);
    }
    let names: BTreeSet<String> = files
        .iter()
        .flat_map(|f| hit_names(&std::fs::read_to_string(f).unwrap()))
        .collect();
    assert!(
        !names.is_empty(),
        "found no failpoint calls under {dirs:?}: the scan is broken"
    );
    names
}

fn assert_same(side: &str, hit: BTreeSet<String>, registered: &[&str]) {
    let registered: BTreeSet<String> = registered.iter().map(|s| s.to_string()).collect();
    let unregistered: Vec<_> = hit.difference(&registered).collect();
    let unused: Vec<_> = registered.difference(&hit).collect();
    assert!(
        unregistered.is_empty() && unused.is_empty(),
        "{side} failpoints out of step with failpoint/src/points.rs: hit but not registered \
         {unregistered:?}, registered but never hit {unused:?}"
    );
}

#[test]
fn host_points_match_the_sources() {
    assert_same("host", points_in(&["src"]), failpoint::HOST_POINTS);
}

#[test]
fn guest_points_match_the_sources() {
    assert_same(
        "guest",
        points_in(&["fc-agent/src"]),
        failpoint::GUEST_POINTS,
    );
}

#[test]
fn a_quoted_call_is_not_a_call() {
    assert_eq!(
        hit_names(
            r#"failpoint::hit("a"); "failpoint::hit_async(\"b\")"; failpoint::hit_scoped("c")"#
        ),
        vec!["a", "c"]
    );
}
