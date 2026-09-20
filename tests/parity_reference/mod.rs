//! How the differential exec test (`tests/test_exec_podman_parity.rs`) reads what host
//! podman says about its reference container.
//!
//! Text in, verdict out: no podman and no VM, so `tests/test_parity_reference.rs` can
//! pin each rule.

// Two test binaries include this module and each uses a part of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

/// `podman inspect --format` for the reference container: its state, then where podman
/// says its root is mounted.
pub const INSPECT_FORMAT: &str = "{{.State.Status}}|{{.GraphDriver.Data.MergedDir}}";

/// What separates the two fields of `INSPECT_FORMAT`. A state has no `|` in it, so the
/// first one ends it, whatever the path holds.
pub const INSPECT_SEPARATOR: &str = "|";

/// `podman info --format` for where the store keeps its runtime state.
pub const STORE_FORMAT: &str = "{{.Store.RunRoot}}|{{.Store.GraphDriverName}}";

/// Why an answer is podman's own error and not the command's, or `None` when it is the
/// command's.
///
/// podman reports its own failure as a line starting with `Error:`, after any lines of
/// its log. An ask that `run` had to kill at the case's timeout has no exit code, and
/// no answer of the command's either.
pub fn own_error(stderr: &[u8], exit: Option<i32>) -> Option<String> {
    if exit.is_none() {
        return Some("no answer before the case's timeout".to_owned());
    }
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().find(|line| !is_podman_log_line(line))?;
    line.starts_with("Error:").then(|| line.to_owned())
}

/// A line of podman's log, which goes to stderr ahead of its answer:
/// `time="..." level=warning msg="..."`.
fn is_podman_log_line(line: &str) -> bool {
    line.starts_with("time=\"") && line.contains(" level=")
}

/// Where podman says a container's root is mounted.
#[derive(Debug, PartialEq)]
pub enum Root<'a> {
    /// An absolute path.
    Path(&'a str),
    /// podman has no mounted root on record for the container. It prints `<no value>`.
    NotMounted,
    /// Anything else, as printed.
    Unexpected(&'a str),
}

/// One line of `podman inspect --format INSPECT_FORMAT`.
#[derive(Debug, PartialEq)]
pub struct Inspected<'a> {
    pub status: &'a str,
    pub root: Root<'a>,
}

/// `None` when the output is not `<state>|<root>`.
pub fn parse_inspect(stdout: &str) -> Option<Inspected<'_>> {
    let (status, root) = stdout.lines().next()?.split_once(INSPECT_SEPARATOR)?;
    let root = match root {
        "" | "<no value>" => Root::NotMounted,
        path if path.starts_with('/') => Root::Path(path),
        other => Root::Unexpected(other),
    };
    Some(Inspected { status, root })
}

/// Whether a file was there when something looked for it.
#[derive(Debug, PartialEq)]
pub enum Looked {
    Present,
    Absent,
    /// The look itself failed, so there is no answer.
    CouldNotLook(String),
}

impl std::fmt::Display for Looked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Looked::Present => write!(f, "present"),
            Looked::Absent => write!(f, "ABSENT"),
            Looked::CouldNotLook(why) => write!(f, "could not look: {why}"),
        }
    }
}

/// What `podman unshare test -e <path>` found, from how it ended. `test` answers with 0
/// and 1. Every other end is podman unshare's own, or the timeout.
pub fn unshare_test_verdict(exit: Option<i32>, stderr: &[u8]) -> Looked {
    match exit {
        Some(0) => Looked::Present,
        Some(1) => Looked::Absent,
        Some(code) => Looked::CouldNotLook(format!(
            "exit {code}: {}",
            String::from_utf8_lossy(stderr)
                .lines()
                .next()
                .unwrap_or("nothing on stderr")
        )),
        None => Looked::CouldNotLook("no answer before the timeout".to_owned()),
    }
}

/// Whether `path` exists, seen from this process. A symlink counts as present wherever
/// it points: an absolute one inside a container's root would resolve on the host.
pub fn host_looked(path: &Path) -> Looked {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Looked::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Looked::Absent,
        Err(error) => Looked::CouldNotLook(error.to_string()),
    }
}

/// The file in which the store records which layers are mounted and where, from one
/// line of `podman info --format STORE_FORMAT`.
pub fn mount_record_path(info_stdout: &str) -> Option<PathBuf> {
    let (runroot, driver) = info_stdout.lines().next()?.rsplit_once('|')?;
    (runroot.starts_with('/') && !driver.is_empty()).then(|| {
        Path::new(runroot)
            .join(format!("{driver}-layers"))
            .join("mountpoints.json")
    })
}

#[derive(serde::Deserialize)]
struct MountedLayer {
    id: String,
    path: String,
    count: i64,
}

/// What the mount record says about the layer mounted at `merged`.
pub fn mount_record_entry(record: &str, merged: &str) -> String {
    match serde_json::from_str::<Vec<MountedLayer>>(record) {
        Ok(layers) => match layers.iter().find(|layer| layer.path == merged) {
            Some(layer) => format!(
                "the record has layer {} mounted there, count {}",
                layer.id, layer.count
            ),
            None => format!(
                "the record has no layer mounted there ({} mounted elsewhere)",
                layers.len()
            ),
        },
        Err(error) => format!("the record is not the JSON this expects: {error}"),
    }
}

/// Characters of the mount record that `shown_record` keeps.
const SHOWN_RECORD_CHARS: usize = 2000;

/// The mount record for the failure text: whole when short, else its head.
pub fn shown_record(record: &str) -> String {
    let total = record.chars().count();
    if total <= SHOWN_RECORD_CHARS {
        return record.trim_end().to_owned();
    }
    let head: String = record.chars().take(SHOWN_RECORD_CHARS).collect();
    format!("{head} ({total} characters, first {SHOWN_RECORD_CHARS} shown)")
}

/// `argv` as one line that a shell reads back as the same words, so a command in the
/// failure text can be pasted. The inspect format has a `|` in it.
pub fn shell_words(argv: &[&str]) -> String {
    argv.iter()
        .map(|word| {
            let plain = !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
            if plain {
                word.to_string()
            } else {
                format!("'{}'", word.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Lines of each stream that `render_probe` shows.
const SHOWN_LINES: usize = 3;

/// One bounded host command and how it ended, as text for the failure message. A
/// command that gave no answer says so: silence would read as a clean result.
pub fn render_probe(
    command: &str,
    exit: Option<i32>,
    timeout: Duration,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let mut text = format!("$ {command}\n");
    text += &match exit {
        Some(code) => format!("  exit {code}\n"),
        None => format!("  no answer in {timeout:?}, killed\n"),
    };
    for (stream, bytes) in [("stdout", stdout), ("stderr", stderr)] {
        let content = String::from_utf8_lossy(bytes);
        let total = content.lines().count();
        for line in content.lines().take(SHOWN_LINES) {
            text += &format!("  {stream} | {line}\n");
        }
        if total > SHOWN_LINES {
            text += &format!("  {stream} | ({total} lines, first {SHOWN_LINES} shown)\n");
        }
    }
    text
}
