//! A bench that publishes a port has to wait for the server behind it.
//!
//! Without `--health-check`, fcvm reports a VM healthy once its container is
//! running. nginx is still inside its entrypoint at that point, so a snapshot
//! taken right after "healthy" can hold a server that has not bound its port,
//! and every clone of that snapshot resets connections until the entrypoint
//! finishes. `make bench-vm` runs the benches with `--test`, which makes one
//! request per clone, so the job failed whenever the snapshot landed in that
//! window (#943): `last_response: 0 bytes`, `connect_err: connect succeeded`.
//!
//! With `--health-check` the baseline is healthy only once nginx answers, so
//! the snapshot holds a listening server, and clones inherit the URL.
//!
//! `SnapshotFixture` in `tests/common/mod.rs` had the same gap: a test that
//! fetched from nginx in its clone got `Connection refused`. It waits for nginx
//! over exec before it snapshots, for the reason given on its test below.

use std::fs;
use std::path::Path;

/// The argument list that holds a `"--publish"`: its innermost `[...]`, or its line.
#[derive(Debug)]
struct PublishingArgs {
    line: usize,
    text: String,
}

impl PublishingArgs {
    fn waits_for_the_server(&self) -> bool {
        self.text.contains("\"--health-check\"")
    }
}

/// Every innermost `[...]` in `source` that contains the literal `"--publish"`,
/// or the literal's own line when no bracket encloses it.
///
/// Line comments, block comments (nested ones too), string literals and char
/// literals are skipped, so a commented-out fixture is not a finding, and a
/// `]` inside a comment or a literal such as `"[::1]"` or `'['` cannot
/// unbalance the brackets.
fn publishing_args(source: &str) -> Vec<PublishingArgs> {
    let bytes = source.as_bytes();
    let mut open = Vec::new();
    let mut spans = Vec::new();
    let mut literals = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'\'' if bytes.get(i + 1) == Some(&b'\\') && bytes.get(i + 3) == Some(&b'\'') => i += 4,
            b'\'' if bytes.get(i + 2) == Some(&b'\'') => i += 3,
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                if &source[start..i] == "\"--publish\"" {
                    literals.push(start);
                }
            }
            b'[' => {
                open.push(i);
                i += 1;
            }
            b']' => {
                if let Some(start) = open.pop() {
                    spans.push((start, i + 1));
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    literals
        .into_iter()
        .map(|literal| {
            // Outside every `[...]`, as in `.arg("--publish")`, the finding is the
            // line itself: dropping it would let that spelling escape the check.
            let (start, end) = spans
                .iter()
                .filter(|(start, end)| *start < literal && literal < *end)
                .min_by_key(|(start, end)| end - start)
                .copied()
                .unwrap_or_else(|| {
                    let start = source[..literal].rfind('\n').map_or(0, |n| n + 1);
                    let end = source[literal..]
                        .find('\n')
                        .map_or(source.len(), |n| literal + n);
                    (start, end)
                });
            PublishingArgs {
                line: 1 + source[..start].matches('\n').count(),
                text: source[start..end].to_string(),
            }
        })
        .collect()
}

#[test]
fn a_publish_without_a_health_check_is_found() {
    let found = publishing_args(
        r#"let f = CloneFixture::setup("x", "rootless", &["--publish", "8080:80"]);"#,
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].line, 1);
    assert!(!found[0].waits_for_the_server(), "{found:?}");
}

#[test]
fn a_publish_with_a_health_check_passes() {
    let found = publishing_args(
        "let args = vec![\n    \"--publish\",\n    \"8080:80\",\n    \"--health-check\",\n    \"http://localhost:80\",\n];",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].waits_for_the_server(), "{found:?}");
}

#[test]
fn comments_and_bracketed_literals_do_not_count() {
    let source = "// setup(&[\"--publish\", \"8080:80\"])\n\
                  let c = '[';\n\
                  let a = [\"[::1]\", \"--publish\", \"80:80\"];";
    let found = publishing_args(source);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].line, 3);
    assert_eq!(found[0].text, "[\"[::1]\", \"--publish\", \"80:80\"]");
}

#[test]
fn every_bench_that_publishes_a_port_waits_for_its_server() {
    let benches = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches");
    let mut paths: Vec<_> = fs::read_dir(&benches)
        .unwrap_or_else(|e| panic!("reading {}: {e}", benches.display()))
        .map(|entry| entry.expect("reading a benches/ entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    paths.sort();

    let mut publishing = Vec::new();
    let mut missing = Vec::new();
    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let source =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for args in publishing_args(&source) {
            publishing.push(name.clone());
            if !args.waits_for_the_server() {
                missing.push(format!("benches/{name}:{}: {}", args.line, args.text));
            }
        }
    }

    // Both of these publish nginx's port. Finding neither means the scan is
    // broken or the fixtures moved, and a scan that finds nothing proves nothing.
    for expected in ["clone.rs", "exec.rs"] {
        assert!(
            publishing.iter().any(|name| name == expected),
            "found no `\"--publish\"` in benches/{expected}; scanned {paths:?}"
        );
    }
    assert!(
        missing.is_empty(),
        "these bench fixtures publish a port without `--health-check`, so their snapshot \
         can hold a server that is not listening yet and a clone resets the first request:\n{}",
        missing.join("\n")
    );
}

#[test]
fn a_bracket_inside_a_block_comment_does_not_end_the_args() {
    let found = publishing_args("let a = [/* ] /* nested ] */ */ \"--publish\", \"80:80\"];");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(!found[0].waits_for_the_server(), "{found:?}");
}

#[test]
fn a_publish_outside_any_brackets_is_still_found() {
    let found = publishing_args("let x = 1;\ncmd.arg(\"--publish\").arg(\"8080:80\");\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].line, 2);
    assert!(!found[0].waits_for_the_server(), "{found:?}");
}

/// `SnapshotFixture` snapshots nginx for the tests in `test_clone_restore_fixes.rs`, and
/// `test_restore_network_cleanup_reestablishes_gateway` fetches from nginx as soon as its
/// clone reads healthy. The fixture passes no health check, so its baseline is "healthy"
/// while nginx is still starting, and the clone answered `Connection refused`: on the
/// test's first try on main's x64 root job at 6f06bd1d, and on all four tries on #946's.
///
/// The fixture waits over exec and not with `--health-check`. A bridged health probe
/// reaches the guest through a host route keyed by guest address, baselines restored from
/// one cached snapshot share that address, and the newest takes the route (#948), so with
/// a health check the older baseline never read healthy.
#[test]
fn the_shared_snapshot_fixture_snapshots_a_serving_nginx() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/mod.rs");
    let source =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let start = source
        .find("impl SnapshotFixture")
        .expect("tests/common/mod.rs has no `impl SnapshotFixture`");
    let fixture = &source[start..];
    let healthy = fixture
        .find("poll_health_by_pid(baseline_pid")
        .expect("SnapshotFixture::new no longer polls its baseline's health");
    let snapshot = fixture
        .find("create_snapshot_by_pid(baseline_pid")
        .expect("SnapshotFixture::new no longer snapshots its baseline");
    assert!(
        healthy < snapshot,
        "the fixture snapshots before it polls health"
    );
    let between = &fixture[healthy..snapshot];
    assert!(
        between.contains("wait_for_nginx(baseline_pid"),
        "SnapshotFixture snapshots its baseline as soon as the container is running, so the \
         snapshot can hold nginx before it listens:\n{between}"
    );
}
