//! How the differential exec test (`tests/test_exec_podman_parity.rs`) reads what host
//! podman says about its reference container.
//!
//! Text in, verdict out: no podman and no VM, so `tests/test_parity_reference.rs` can
//! pin each rule.

// Two test binaries include this module and each uses a part of it.
#![allow(dead_code)]

/// `podman inspect --format` for the reference container: its state, then where podman
/// says its root is mounted.
pub const INSPECT_FORMAT: &str = "{{.State.Status}} {{.GraphDriver.Data.MergedDir}}";

/// What separates the two fields of `INSPECT_FORMAT`.
pub const INSPECT_SEPARATOR: &str = " ";

/// Why an answer is podman's own error and not the command's, or `None` when it is the
/// command's.
pub fn own_error(stderr: &[u8], _exit: Option<i32>) -> Option<String> {
    stderr.starts_with(b"Error:").then(|| {
        String::from_utf8_lossy(stderr)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    })
}

/// Where podman says a container's root is mounted.
#[derive(Debug, PartialEq)]
pub enum Root<'a> {
    /// An absolute path.
    Path(&'a str),
    /// podman has no mounted root on record for the container.
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

pub fn parse_inspect(stdout: &str) -> Option<Inspected<'_>> {
    let mut fields = stdout.split_whitespace();
    let status = fields.next()?;
    let root = match fields.next() {
        Some(dir) if dir.starts_with('/') => Root::Path(dir),
        other => Root::Unexpected(other.unwrap_or_default()),
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

/// What `podman unshare test -e <path>` found, from how it ended.
pub fn unshare_test_verdict(exit: Option<i32>, _stderr: &[u8]) -> Looked {
    match exit {
        Some(0) => Looked::Present,
        Some(_) => Looked::Absent,
        None => Looked::CouldNotLook("it did not run to its end".to_owned()),
    }
}
