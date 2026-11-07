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
    /// A run loop is waiting on this companion's exit to report it
    /// ([`registered_exit`]), so [`register`] does not prune it.
    claimed: bool,
}

/// Captures of companions that are handed around as a bare [`Child`] (the
/// namespace holder), by pid. A process's pid is unique while it is unreaped,
/// and an entry is removed when its death is reported or its owner reaps it.
/// One whose owner reaped it without saying so (a failed setup) is pruned by the
/// next [`register`]: its reader has reached EOF and its pid is gone.
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
        captures.retain(|pid, capture| !abandoned(*pid, capture));
        let capture = Capture {
            tail,
            reader,
            claimed: false,
        };
        if let Some(old) = captures.insert(pid, capture) {
            if let Some(reader) = old.reader {
                reader.abort();
            }
        }
    }
}

/// Whether nobody can report `capture` any more: no run loop claimed it, its reader
/// is done, and its process has been reaped (a reaped pid has no /proc entry; an
/// unreaped one, zombie included, does).
fn abandoned(pid: u32, capture: &Capture) -> bool {
    !capture.claimed
        && capture
            .reader
            .as_ref()
            .is_none_or(|reader| reader.is_finished())
        && !std::path::Path::new(&format!("/proc/{pid}")).exists()
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

/// Wait until the registered companion `child` exits. A `None` child never resolves,
/// so the caller can select on this unconditionally. Cancel-safe: it only waits, and
/// the pid is read before the wait, which reaps the child only when it returns. The
/// caller describes the exit with [`describe_registered`] once its arm has won, so
/// draining stderr is never cut short by another arm.
pub async fn registered_exit(
    child: &mut Option<Child>,
) -> (Option<u32>, std::io::Result<ExitStatus>) {
    let Some(child) = child.as_mut() else {
        return std::future::pending().await;
    };
    let pid = child.id();
    if let Some(pid) = pid {
        if let Some(capture) = captures()
            .lock()
            .ok()
            .as_mut()
            .and_then(|c| c.get_mut(&pid))
        {
            capture.claimed = true;
        }
    }
    (pid, child.wait().await)
}

/// The registered companion `child` if it has already exited, without waiting.
pub fn registered_exit_now(
    child: &mut Option<Child>,
) -> Option<(Option<u32>, std::io::Result<ExitStatus>)> {
    let child = child.as_mut()?;
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => Some((pid, Ok(status))),
        Ok(None) => None,
        Err(error) => Some((pid, Err(error))),
    }
}

/// Describe the exit [`registered_exit`] returned, with what the companion printed.
pub async fn describe_registered(
    name: &str,
    pid: Option<u32>,
    status: std::io::Result<ExitStatus>,
) -> String {
    match status {
        Ok(status) => {
            let tail = match pid {
                Some(pid) => take_tail(pid).await,
                None => StderrTail::new(),
            };
            death_message(name, pid, &status, &tail)
        }
        Err(error) => {
            if let Some(pid) = pid {
                forget(pid);
            }
            format!("{name} could not be watched: {error}")
        }
    }
}

/// The report of a network helper's death (pasta), naming the namespace holder
/// first when it has died too. pasta exits when the holder of the namespace it
/// serves dies, so the holder's exit is the cause and pasta's the consequence.
pub async fn network_death_report(network_message: String, holder: &mut Option<Child>) -> String {
    match registered_exit_now(holder) {
        Some((pid, status)) => {
            let holder_message = describe_registered("namespace holder", pid, status).await;
            format!("{holder_message}; then {network_message}")
        }
        None => network_message,
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
        let (exited_pid, status) = registered_exit(&mut child).await;
        let message = describe_registered("namespace holder", exited_pid, status).await;
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

    /// #852 review: a run loop whose other arm wakes while the death watcher drains
    /// stderr must still report the companion's pid and last words. Against the
    /// earlier registered_death, which drained inside the arm, the same loop lost both.
    #[tokio::test]
    async fn an_interrupted_watcher_still_reports_the_pid_and_stderr() {
        // The grandchild keeps the stderr pipe open for 600 ms after the child exits.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "echo 'last words' >&2; (sleep 0.6) & exit 4"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        register(&mut child, |_| {});
        let mut child = Some(child);
        let message = loop {
            tokio::select! {
                (exited_pid, status) = registered_exit(&mut child) => {
                    break describe_registered("namespace holder", exited_pid, status).await;
                }
                _ = tokio::time::sleep(Duration::from_millis(20)) => continue,
            }
        };
        assert!(
            message.starts_with(&format!(
                "namespace holder (pid {pid}) exited: exit status: 4"
            )),
            "{message}"
        );
        assert!(message.ends_with("last words"), "{message}");
    }

    /// #852 review: a holder whose owner reaped it without forgetting it (a failed
    /// setup) does not stay in the registry once another companion registers.
    #[tokio::test]
    async fn a_reaped_unreported_companion_is_pruned_at_the_next_registration() {
        let spawn = || {
            tokio::process::Command::new("sh")
                .args(["-c", "echo gone >&2"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let first_pid = first.id().unwrap();
        register(&mut first, |_| {});
        first.wait().await.unwrap();
        // Its reader reaches EOF once the pipe closes.
        for _ in 0..200 {
            let done = captures()
                .lock()
                .unwrap()
                .get(&first_pid)
                .is_some_and(|c| c.reader.as_ref().is_none_or(|r| r.is_finished()));
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut second = spawn();
        let second_pid = second.id().unwrap();
        register(&mut second, |_| {});
        assert!(
            captures().lock().unwrap().get(&first_pid).is_none(),
            "the reaped companion's capture is still registered"
        );
        second.wait().await.unwrap();
        forget(second_pid);
    }

    /// #852 review: pasta exits when the namespace holder dies, so a network helper's
    /// death is reported with the holder first when the holder has died too.
    #[tokio::test]
    async fn a_network_helper_death_names_a_dead_holder_first() {
        let mut holder = tokio::process::Command::new("sh")
            .args(["-c", "echo 'holder last words' >&2; kill -KILL $$"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = holder.id().unwrap();
        register(&mut holder, |_| {});
        // Wait for the holder's death without reaping it, as pasta's exit follows it.
        while !std::path::Path::new(&format!("/proc/{pid}/stat")).exists()
            || !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .unwrap_or_default()
                .contains(") Z ")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut holder = Some(holder);
        let message = network_death_report(
            "pasta (pid 1) exited: exit status: 1".to_string(),
            &mut holder,
        )
        .await;
        assert!(
            message.starts_with(&format!(
                "namespace holder (pid {pid}) exited: signal: 9 (SIGKILL)"
            )),
            "{message}"
        );
        assert!(
            message.ends_with("; then pasta (pid 1) exited: exit status: 1"),
            "{message}"
        );
        let mut alive = Some(
            tokio::process::Command::new("sleep")
                .arg("5")
                .kill_on_drop(true)
                .spawn()
                .unwrap(),
        );
        assert_eq!(
            network_death_report("pasta exited".to_string(), &mut alive).await,
            "pasta exited"
        );
    }

    /// #852 review: both run loops report a network helper's death through
    /// network_death_report, so a holder that died first is named first.
    #[test]
    fn both_run_loops_name_a_dead_holder_before_pasta() {
        for (file, source) in [
            (
                "src/commands/podman/mod.rs",
                include_str!("commands/podman/mod.rs"),
            ),
            (
                "src/commands/snapshot.rs",
                include_str!("commands/snapshot.rs"),
            ),
        ] {
            let arm = source
                .find(".companion_exited() =>")
                .unwrap_or_else(|| panic!("{file} no longer waits on the network's helper"));
            let body = &source[arm..arm + 600];
            assert!(
                body.contains("network_death_report("),
                "{file} reports a network helper's death without checking the holder first"
            );
        }
    }
}
