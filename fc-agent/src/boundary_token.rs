//! The token that ties a snapshot create's prepare to its reopens (#1941).
//!
//! The host sends `--prepare-snapshot-network` and later `--resume-snapshot-network` as
//! two execs, and both take the guest's boundary lock. The lock serializes them but does
//! not order them: a create killed after it dispatched the prepare, and before that
//! one-shot took the lock, leaves its source guard to send the reopen, and the reopen can
//! take the lock first. The prepare would then close the gate and freeze the payload
//! after the only reopen that was coming had run.
//!
//! So the host gives each create one token, passed with `--boundary-token` to its prepare
//! and to every reopen of that create. Under the boundary lock:
//!
//! - a prepare records that its token began, and refuses, changing nothing (no freeze, no
//!   thaw), when a reopen of that token already ran;
//! - a reopen that finds its token's prepare not begun records the token as cancelled.
//!
//! The record is one small file in `/run`. It names the token whose prepare began last
//! and the last [`CANCELLED_CAP`] cancelled tokens. The host's per-VM snapshot lock lets
//! only one create work on a VM at a time, so a prepare that began is followed by its own
//! reopens before any other create's prepare runs; a reopen that finds another token as
//! the last begun one records its own as cancelled, which costs nothing.

use std::collections::VecDeque;
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Where the record lives. /run is the guest's tmpfs, so it is part of a memory
/// snapshot and a clone inherits the source's record.
pub const RECORD_PATH: &str = "/run/fcvm/snapshot-boundary-tokens";

/// The flag that carries the token.
pub const FLAG: &str = "--boundary-token";

/// How many cancelled tokens the record keeps. A cancellation is written only when a
/// reopen overtakes its create's prepare (a create killed between the two, or a prepare
/// exec that failed before it ran), so a prepare is fenced unless that many such creates
/// of the same VM came after it.
pub const CANCELLED_CAP: usize = 1024;

/// The token after `--boundary-token`. The host makes it from 32 hex digits; anything
/// outside 1 to 64 characters of `[A-Za-z0-9-]` is refused, so it cannot break a line of
/// the record.
pub fn from_args(args: &[String]) -> Result<&str> {
    let token = args
        .iter()
        .position(|arg| arg == FLAG)
        .and_then(|at| args.get(at + 1))
        .with_context(|| format!("the host passed no {FLAG}"))?;
    let valid = !token.is_empty()
        && token.len() <= 64
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !valid {
        bail!("{FLAG} {token:?} is not 1 to 64 characters of [A-Za-z0-9-]");
    }
    Ok(token)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Record {
    begun: Option<String>,
    cancelled: VecDeque<String>,
}

impl Record {
    fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let mut record = Self::default();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("begun", token)) => record.begun = Some(token.to_string()),
                Some(("cancelled", token)) => record.cancelled.push_back(token.to_string()),
                _ => bail!("{} holds an unknown line {line:?}", path.display()),
            }
        }
        Ok(record)
    }

    /// Written to a temporary file and renamed over the record, so a one-shot killed while
    /// it writes leaves the old record or the new one.
    fn store(&self, path: &Path) -> Result<()> {
        let mut text = String::new();
        if let Some(token) = &self.begun {
            text.push_str(&format!("begun {token}\n"));
        }
        for token in &self.cancelled {
            text.push_str(&format!("cancelled {token}\n"));
        }
        let parent = path
            .parent()
            .context("the boundary token record has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let temp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, path)
            .with_context(|| format!("renaming {} to {}", temp.display(), path.display()))
    }
}

/// The prepare's check, with the boundary lock held: fail if a reopen of `token` already
/// ran, otherwise record that its prepare began.
fn begin_prepare(path: &Path, token: &str) -> Result<()> {
    let mut record = Record::load(path)?;
    if record.cancelled.iter().any(|t| t == token) {
        bail!(
            "a reopen of boundary token {token} already ran, so this prepare came after the \
             create it belongs to was undone; it changes nothing"
        );
    }
    record.begun = Some(token.to_string());
    record.store(path)
}

/// The reopen's step, with the boundary lock held: record `token` as cancelled unless its
/// prepare began.
fn note_reopen(path: &Path, token: &str) -> Result<()> {
    let mut record = Record::load(path)?;
    if record.begun.as_deref() == Some(token) || record.cancelled.iter().any(|t| t == token) {
        return Ok(());
    }
    record.cancelled.push_back(token.to_string());
    while record.cancelled.len() > CANCELLED_CAP {
        record.cancelled.pop_front();
    }
    record.store(path)
}

/// A prepare, fenced by its token. Call with the boundary lock held. A prepare a reopen
/// of its token overtook returns an error before `freeze`, and calls neither `freeze` nor
/// `thaw`. Otherwise it freezes, prepares, and thaws when the prepare fails (the host
/// reopens the source on its own then).
pub async fn prepare<Fut>(
    record: &Path,
    token: &str,
    freeze: impl FnOnce(),
    thaw: impl FnOnce(),
    prepare: impl FnOnce() -> Fut,
) -> Result<()>
where
    Fut: std::future::Future<Output = Result<()>>,
{
    begin_prepare(record, token)?;
    freeze();
    let prepared = prepare().await;
    if prepared.is_err() {
        thaw();
    }
    prepared
}

/// A reopen, fenced by its token. Call with the boundary lock held. It records the token
/// as cancelled when its prepare has not begun, then reopens and thaws whatever the
/// record step returned: a reopen never leaves the source closed. A record it could not
/// write fails it afterwards, because a prepare that comes later would not be fenced.
pub async fn reopen<Fut>(
    record: &Path,
    token: &str,
    thaw: impl FnOnce(),
    reopen: impl FnOnce() -> Fut,
) -> Result<()>
where
    Fut: std::future::Future<Output = Result<()>>,
{
    let noted = note_reopen(record, token).context("recording the reopen's boundary token");
    let reopened = reopen().await;
    thaw();
    match (reopened, noted) {
        (Ok(()), noted) => noted,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(noted)) => Err(anyhow::anyhow!("{error:#}; additionally {noted:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Runs the fenced prepare of `token` and returns what it did, in order.
    fn run_prepare(
        record: &Path,
        token: &str,
        outcome: Result<()>,
    ) -> (Result<()>, Vec<&'static str>) {
        let steps = RefCell::new(Vec::new());
        let result = block_on(prepare(
            record,
            token,
            || steps.borrow_mut().push("freeze"),
            || steps.borrow_mut().push("thaw"),
            || async {
                steps.borrow_mut().push("prepare");
                outcome
            },
        ));
        (result, steps.into_inner())
    }

    fn run_reopen(record: &Path, token: &str) -> (Result<()>, Vec<&'static str>) {
        let steps = RefCell::new(Vec::new());
        let result = block_on(reopen(
            record,
            token,
            || steps.borrow_mut().push("thaw"),
            || async {
                steps.borrow_mut().push("reopen");
                Ok(())
            },
        ));
        (result, steps.into_inner())
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// The race of #1941: a create is killed after it dispatched its prepare, its guard's
    /// reopen takes the boundary lock first, and the prepare arrives after it. The prepare
    /// must refuse and change nothing: no freeze, no thaw, no gate.
    #[test]
    fn a_prepare_overtaken_by_its_reopen_refuses_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("tokens");

        let (reopened, steps) = run_reopen(&record, "create-a");
        reopened.unwrap();
        assert_eq!(steps, ["reopen", "thaw"]);

        let (prepared, steps) = run_prepare(&record, "create-a", Ok(()));
        let error = format!("{:#}", prepared.expect_err("the overtaken prepare ran"));
        assert!(error.contains("create-a"), "{error}");
        assert!(steps.is_empty(), "the overtaken prepare did {steps:?}");

        // The next create of the VM is not affected.
        let (prepared, steps) = run_prepare(&record, "create-b", Ok(()));
        prepared.unwrap();
        assert_eq!(steps, ["freeze", "prepare"]);
    }

    /// The usual order: the prepare, then its reopens, then the next create's prepare.
    #[test]
    fn a_reopen_after_its_prepare_cancels_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("tokens");

        assert_eq!(
            run_prepare(&record, "create-a", Ok(())).1,
            ["freeze", "prepare"]
        );
        run_reopen(&record, "create-a").0.unwrap();
        run_reopen(&record, "create-a").0.unwrap();
        assert_eq!(Record::load(&record).unwrap().cancelled.len(), 0);
        assert_eq!(
            run_prepare(&record, "create-b", Ok(())).1,
            ["freeze", "prepare"]
        );
    }

    /// A failed prepare thaws what it froze; a reopen after it changes nothing more.
    #[test]
    fn a_failed_prepare_thaws_and_its_reopen_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("tokens");

        let (prepared, steps) = run_prepare(&record, "create-a", Err(anyhow::anyhow!("gate")));
        assert!(prepared.is_err());
        assert_eq!(steps, ["freeze", "prepare", "thaw"]);
        let (reopened, steps) = run_reopen(&record, "create-a");
        reopened.unwrap();
        assert_eq!(steps, ["reopen", "thaw"]);
    }

    /// A reopen whose record cannot be written still reopens and thaws, then fails.
    #[test]
    fn a_reopen_that_cannot_record_still_reopens() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the record's parent should be makes the write fail.
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let (reopened, steps) = run_reopen(&blocker.join("tokens"), "create-a");
        assert!(reopened.is_err());
        assert_eq!(steps, ["reopen", "thaw"]);
    }

    #[test]
    fn the_record_keeps_the_last_cancelled_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("tokens");
        for n in 0..CANCELLED_CAP + 2 {
            note_reopen(&record, &format!("t{n}")).unwrap();
        }
        let loaded = Record::load(&record).unwrap();
        assert_eq!(loaded.cancelled.len(), CANCELLED_CAP);
        assert_eq!(loaded.cancelled.front().unwrap(), "t2");
        assert!(begin_prepare(&record, "t1").is_ok());
        assert!(begin_prepare(&record, &format!("t{}", CANCELLED_CAP + 1)).is_err());
    }

    #[test]
    fn the_token_is_read_from_its_flag_and_checked() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ok = args(&[
            "fc-agent",
            "--resume-snapshot-network",
            FLAG,
            "0a1b-C",
            "--host-time-ns",
            "5",
        ]);
        assert_eq!(from_args(&ok).unwrap(), "0a1b-C");
        assert!(from_args(&args(&["fc-agent", "--prepare-snapshot-network"])).is_err());
        assert!(from_args(&args(&["fc-agent", FLAG])).is_err());
        assert!(from_args(&args(&["fc-agent", FLAG, "a b"])).is_err());
        assert!(from_args(&args(&["fc-agent", FLAG, "a\nbegun x"])).is_err());
        assert!(from_args(&args(&["fc-agent", FLAG, &"a".repeat(65)])).is_err());
    }
}
