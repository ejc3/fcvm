//! No harness under `bench/` or `fleet/` builds fcvm's state directory from a data root.
//!
//! The benchmark harnesses read VM state files. Three times a change to where or how fcvm
//! keeps that state broke them, because each joined `state` to a data root of its own
//! (`os.path.join(data_root, "state")`, `$DATA_ROOT/state`, `/mnt/fcvm-btrfs/state`). They
//! now ask the binary they run (`fcvm paths --json`, src/commands/paths.rs) and read inside
//! the directory it names. This test keeps the join from coming back: it scans every text
//! file under `bench/` and `fleet/` and fails on a line that joins `state` to a directory.
//!
//! Not scanned, each for its reason:
//! - `bench/chromium/results/`, `bench/chromium/corpus-live/`, `bench/chromium/report/evidence/`:
//!   recorded artifacts (captured pages, copies of harnesses as they ran), never executed.
//! - `fleet/web/public/`, `node_modules/`: vendored third-party code.
//! - `test_*` files and `state_fixture.py`: test fixtures that stand in for fcvm, so they
//!   lay out its store themselves; they never read a real one.
//!
//! Allowed lines are matched by file and exact text (`ALLOWED`), so a new join in the same
//! file still fails, and an entry whose line is gone fails as stale.

use regex::Regex;
use std::path::{Path, PathBuf};

/// A line that joins `state` to a directory: a Python or Rust path join, a pathlib `/`, or
/// a path string that puts `/state` after a variable, a substitution or the default data
/// root.
const JOIN_PATTERNS: &[&str] = &[
    // os.path.join(data_root, "state")
    r#"join\([^()]*,\s*["']state["']\s*\)"#,
    // data_dir.join("state")
    r#"\.join\(\s*"state"\s*\)"#,
    // Path(data_root) / "state"
    r#"/\s*["']state["']"#,
    // $DATA_ROOT/state, ${DATA_ROOT}/state, "$data/state", {data_root}/state,
    // $(dirname "$x")/state, /mnt/fcvm-btrfs/state
    r#"(?:\$\{?[A-Za-z_][A-Za-z0-9_]*\}?|\{[A-Za-z_][A-Za-z0-9_.]*\}|fcvm-btrfs|\))["']?/state(?:[/"'\s):}\]]|$)"#,
];

/// Lines that may keep a join, by path relative to the repository and exact trimmed text.
const ALLOWED: &[(&str, &str, &str)] = &[(
    "bench/start-rate/pieces/src/pieces.rs",
    r#"let dir = args.work_dir.join("state");"#,
    "a private store in the piece's own work directory, opened through fcvm's StateManager; \
     it is not fcvm's data root and the layout inside is the library's",
)];

/// Directories under the scanned roots that hold no harness code.
const SKIPPED_DIRS: &[&str] = &[
    "bench/chromium/results",
    "bench/chromium/corpus-live",
    "bench/chromium/report/evidence",
    "fleet/web/public",
];

/// Harness files the scan must reach, so a walk that finds nothing cannot pass.
const MUST_SCAN: &[&str] = &[
    "bench/chromium/reqbench.py",
    "bench/chromium/reqbench.sh",
    "bench/chromium/reqscale.py",
    "bench/chromium/corpus_mem.py",
    "bench/chromium/bench.sh",
    "bench/chromium/faultbench.py",
    "bench/restore-matrix.py",
];

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn join_regex() -> Regex {
    let alternatives: Vec<String> = JOIN_PATTERNS.iter().map(|p| format!("(?:{p})")).collect();
    Regex::new(&alternatives.join("|")).expect("the join patterns compile")
}

/// Whether a file is a fixture that stands in for fcvm (see the module doc).
fn is_fixture(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("test_") || name == "state_fixture.py"
}

/// Every file under `dir` (relative to the repository) the scan reads, recursively.
fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(root.join(dir))
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", root.join(dir).display()));
    for entry in entries {
        let entry = entry.expect("a directory entry");
        let relative = dir.join(entry.file_name());
        let file_type = entry.file_type().expect("a file type");
        if file_type.is_dir() {
            let name = entry.file_name();
            let skipped = SKIPPED_DIRS.iter().any(|s| Path::new(s) == relative)
                || [".git", "node_modules", "__pycache__", "target", ".scratch"]
                    .iter()
                    .any(|s| name == *s);
            if !skipped {
                walk(root, &relative, out);
            }
        } else if file_type.is_file() && !is_fixture(&relative) {
            out.push(relative);
        }
    }
}

/// `(file, line number, trimmed text)` of every join outside `ALLOWED`, and the number of
/// files read.
fn scan(root: &Path) -> (Vec<(String, usize, String)>, Vec<String>) {
    let mut files = Vec::new();
    for top in ["bench", "fleet"] {
        walk(root, Path::new(top), &mut files);
    }
    let regex = join_regex();
    let mut found = Vec::new();
    let mut read = Vec::new();
    for file in files {
        // Binary files (images, archives) are not code.
        let Ok(text) = std::fs::read_to_string(root.join(&file)) else {
            continue;
        };
        let name = file.to_string_lossy().into_owned();
        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                found.push((name.clone(), index + 1, line.trim().to_string()));
            }
        }
        read.push(name);
    }
    (found, read)
}

#[test]
fn no_harness_joins_state_to_a_data_root() {
    let root = repo();
    let (found, read) = scan(&root);
    for must in MUST_SCAN {
        assert!(
            read.iter().any(|r| r == must),
            "the scan did not read {must}; it read {} files, so it cannot say the harnesses \
             are clean",
            read.len()
        );
    }
    let mut unexpected = Vec::new();
    for (file, line, text) in &found {
        let allowed = ALLOWED.iter().any(|(f, t, _)| f == file && t == text);
        if !allowed {
            unexpected.push(format!("{file}:{line}: {text}"));
        }
    }
    assert!(
        unexpected.is_empty(),
        "these lines build fcvm's state directory from a data root; ask the binary instead \
         (`fcvm paths --json`, reqbench.fcvm_paths in Python, jq on its output in shell):\n{}",
        unexpected.join("\n")
    );
    for (file, text, why) in ALLOWED {
        assert!(!why.is_empty(), "allow-list entry {file} gives no reason");
        assert!(
            found.iter().any(|(f, _, t)| f == file && t == text),
            "stale allow-list entry: {file} no longer has the line {text:?}"
        );
    }
}

/// The patterns catch each shape a harness has used, and leave alone the words a harness
/// uses for other things.
#[test]
fn the_join_patterns_match_joins_and_nothing_else() {
    let regex = join_regex();
    for joined in [
        r#"args.state_dir = os.path.join(args.data_root, "state")"#,
        r#"state_dir=os.path.join(opts.data_root, 'state'), out_dir=log_dir,"#,
        r#"self.state_dir = self.data_root / "state""#,
        r#"let dir = data_dir.join("state");"#,
        r#"STATE_DIR=/mnt/fcvm-btrfs/state"#,
        r#"STATE_DIR = Path("/mnt/fcvm-btrfs/state")"#,
        r#"--state-dir "$data/state" --vm-dir "$data/vm-disks""#,
        r#"ls "$DATA_ROOT/state/vms""#,
        r#"rm -rf ${DATA_ROOT}/state"#,
        r#"path = f"{data_root}/state""#,
        r#"STATE_DIR="$(dirname "$x")/state""#,
    ] {
        assert!(regex.is_match(joined), "not caught: {joined}");
    }
    for other in [
        r#"--state-file "$R/state.json""#,
        r#"fleet coord --state-file $OUT/state.json"#,
        r#"self.assertEqual(stat.state, "R")"#,
        r#"if state.get("pid") == pid:"#,
        r#"args.state_dir = fcvm_state_dir(args.fcvm)"#,
        r#"STATE_DIR=$(jq -er '.state_dir' <<<"$paths")"#,
        r#"--exclude=./vm-disks --exclude=./statefile"#,
    ] {
        assert!(!regex.is_match(other), "caught by mistake: {other}");
    }
}
