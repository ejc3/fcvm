//! Companion processes: the helpers a VM depends on for its whole life (pasta,
//! the rootless namespace holder).
//!
//! When one of them dies the VM is broken, and what explains it is the
//! companion's own exit status and the last thing it printed. Without that the
//! failure surfaces later as whatever its absence breaks next: pasta that died
//! on a port conflict reported itself only as `Cannot find device "pasta0"`
//! (#851). This module holds the one way every companion records its stderr and
//! the one message a death is reported with (#852).

use std::collections::{HashMap, VecDeque};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Child;
use tokio::task::JoinHandle;

/// Lines of a companion's stderr kept for its failure report.
pub const TAIL_LINES: usize = 20;

/// Upper bound on waiting for a dead companion's stderr to reach EOF, so its
/// report carries what it printed. Its exit closed the write end; this bounds
/// only an inherited pipe that a descendant still holds.
pub const STDERR_EOF_TIMEOUT: Duration = Duration::from_secs(2);

/// The last [`TAIL_LINES`] lines a companion wrote to stderr.
#[derive(Clone, Debug, Default)]
pub struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl StderrTail {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep `line`, dropping the oldest once the tail is full.
    pub fn push(&self, line: String) {
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() >= TAIL_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|lines| lines.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The tail as an error suffix: `; last <name> stderr output:` and the
    /// lines, or a statement that the companion printed nothing.
    pub fn render(&self, name: &str) -> String {
        let lines = self.lines();
        if lines.is_empty() {
            format!("; no stderr output captured from {name}")
        } else {
            format!("; last {name} stderr output:\n  {}", lines.join("\n  "))
        }
    }
}

/// Read `stderr` to EOF into `tail`, passing each line to `log` first.
pub fn capture<R>(
    stderr: R,
    tail: StderrTail,
    log: impl Fn(&str) + Send + 'static,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log(&line);
            tail.push(line);
        }
    })
}

/// How a companion that died is reported, wherever its death is noticed.
pub fn death_message(
    name: &str,
    pid: Option<u32>,
    status: &ExitStatus,
    tail: &StderrTail,
) -> String {
    let pid = pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default();
    format!("{name}{pid} exited: {status}{}", tail.render(name))
}

/// A companion's stderr capture: its tail and the task reading into it.
struct Capture {
    tail: StderrTail,
    reader: Option<JoinHandle<()>>,
}

/// Captures of companions that are handed around as a bare [`Child`] (the
/// namespace holder), by pid. A process's pid is unique while it is unreaped,
/// and an entry is removed when its death is reported or its owner reaps it.
fn captures() -> &'static Mutex<HashMap<u32, Capture>> {
    static CAPTURES: OnceLock<Mutex<HashMap<u32, Capture>>> = OnceLock::new();
    CAPTURES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Start capturing `child`'s stderr under its pid. `child` must have been
/// spawned with a piped stderr; without one nothing is captured and its
/// report says so.
pub fn register(child: &mut Child, log: impl Fn(&str) + Send + 'static) {
    let Some(pid) = child.id() else { return };
    let tail = StderrTail::new();
    let reader = child
        .stderr
        .take()
        .map(|stderr| capture(stderr, tail.clone(), log));
    if let Ok(mut captures) = captures().lock() {
        if let Some(old) = captures.insert(pid, Capture { tail, reader }) {
            if let Some(reader) = old.reader {
                reader.abort();
            }
        }
    }
}

/// What the registered companion `pid` has printed so far.
pub fn tail_of(pid: u32) -> StderrTail {
    captures()
        .lock()
        .ok()
        .and_then(|captures| captures.get(&pid).map(|capture| capture.tail.clone()))
        .unwrap_or_default()
}

/// Forget the capture of `pid`, for an owner that reaps it on purpose.
pub fn forget(pid: u32) {
    if let Some(capture) = captures().lock().ok().and_then(|mut c| c.remove(&pid)) {
        if let Some(reader) = capture.reader {
            reader.abort();
        }
    }
}

/// The registered capture of `pid`, after its reader drained the pipe.
async fn take_tail(pid: u32) -> StderrTail {
    let capture = captures().lock().ok().and_then(|mut c| c.remove(&pid));
    let Some(mut capture) = capture else {
        return StderrTail::new();
    };
    crate::utils::wait_for_stderr_eof(&mut capture.reader, STDERR_EOF_TIMEOUT).await;
    capture.tail
}

/// Wait until the registered companion `child` exits, and describe it. A
/// `None` child never resolves, so the caller can select on this unconditionally.
pub async fn registered_death(name: &str, child: &mut Option<Child>) -> String {
    let Some(child) = child.as_mut() else {
        return std::future::pending().await;
    };
    let pid = child.id();
    match child.wait().await {
        Ok(status) => {
            let tail = match pid {
                Some(pid) => take_tail(pid).await,
                None => StderrTail::new(),
            };
            death_message(name, pid, &status, &tail)
        }
        Err(error) => format!("{name} could not be watched: {error}"),
    }
}

/// Start an in-process companion (the egress proxy): `run` is meant to run for
/// the VM's whole life, and its error is kept for [`task_death`] to report.
pub fn spawn_task<F>(name: &'static str, run: F) -> JoinHandle<Option<String>>
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        match run.await {
            Ok(()) => None,
            Err(error) => {
                tracing::warn!("{name} error: {error:#}");
                Some(format!("{error:#}"))
            }
        }
    })
}

/// Wait until the in-process companion `handle` ends, and describe it. Takes
/// the handle, which cannot be awaited again. A `None` handle never resolves.
pub async fn task_death(name: &str, handle: &mut Option<JoinHandle<Option<String>>>) -> String {
    let Some(running) = handle.as_mut() else {
        return std::future::pending().await;
    };
    let result = running.await;
    *handle = None;
    match result {
        Ok(Some(error)) => format!("{name} stopped: {error}"),
        Ok(None) => format!("{name} stopped without an error"),
        Err(error) => format!("{name} task failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn the_tail_keeps_the_last_lines_and_says_when_there_are_none() {
        let tail = StderrTail::new();
        assert_eq!(
            tail.render("pasta"),
            "; no stderr output captured from pasta"
        );
        for i in 0..TAIL_LINES + 3 {
            tail.push(format!("line {i}"));
        }
        let lines = tail.lines();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], "line 3");
        assert!(tail
            .render("pasta")
            .ends_with(&format!("line {}", TAIL_LINES + 2)));
    }

    /// A registered companion that prints and then dies is reported with its
    /// name, pid, exit status and last stderr line.
    #[tokio::test]
    async fn a_registered_companion_death_names_it_and_its_last_words() {
        let mut child = tokio::process::Command::new("sh")
            .args([
                "-c",
                "echo first >&2; echo 'bind: Address already in use' >&2; exit 3",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        register(&mut child, |_| {});
        let mut child = Some(child);
        let message = registered_death("namespace holder", &mut child).await;
        assert!(
            message.starts_with(&format!(
                "namespace holder (pid {pid}) exited: exit status: 3"
            )),
            "{message}"
        );
        assert!(
            message.ends_with("bind: Address already in use"),
            "{message}"
        );
        assert!(captures().lock().unwrap().get(&pid).is_none());
    }
}
