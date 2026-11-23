//! Test that the guest serial console works after snapshot restore.
//!
//! Two ways the restored console used to die:
//! - systemd-journald crashes after restore (journal mid-write during the
//!   snapshot); fc-agent.service's journal+console stderr then went nowhere.
//!   Fix: fc-agent owns its console output directly (console::init() at startup
//!   routes fd 1/2 through a pipe drained by a dedicated writer thread that
//!   writes /dev/console non-blocking — see fc-agent/src/console.rs).
//! - The pre-start snapshot was captured with UART TX bytes in flight; the
//!   restored 8250 driver waits forever for a TX interrupt the re-created
//!   serial device never delivers, killing ALL restored serial output.
//!   Fix: fc-agent quiesces the console (flush + gate + TIOCOUTQ drain) BEFORE
//!   sending cache-ready, so the host's snapshot pause always captures an idle
//!   UART (container::notify_cache_ready_and_wait).
//!
//! Architecture:
//!   Guest eprintln!() → fd 2 (pipe) → console-writer thread → /dev/console
//!   → /dev/ttyS0 → Firecracker UART emulation → host stdout → tracing → log file
//!
//! A restore's progress lines do not take that path: the console-writer thread queues them
//! on the output vsock connection once the host has acknowledged the restore, fcvm logs
//! them under `fcvm::agent`, and one line on the console says so. When fc-agent logs a
//! warning or an error first, or the restore does not finish within fc-agent's window,
//! the lines go to the console instead. Either way an fc-agent line under `fcvm::console`
//! crossed the UART, and that is the proof here that the restored serial console works.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Verify serial console works after warm start (snapshot restore to new Firecracker).
///
/// Phase 1: Boot VM to ensure snapshot is cached (cold start creates cache).
/// Phase 2: Boot second VM with same config → warm start from cache.
///          Write a marker to /dev/ttyS0 and verify it appears in host log.
#[tokio::test]
async fn test_serial_console_after_restore() -> Result<()> {
    // This test requires snapshot support for the warm start path.
    if std::env::var("FCVM_NO_SNAPSHOT")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }

    println!("\nSerial console after restore test");
    println!("==================================");

    let marker = format!("UART-TEST-MARKER-{}", std::process::id());

    // Phase 1: Ensure a cached snapshot exists by running a VM to completion.
    // If the snapshot is already cached from a previous test run, this phase
    // still runs but completes faster (warm start). Either way, after this
    // phase there's a cached snapshot for Phase 2.
    println!("Phase 1: Ensuring cached snapshot exists...");
    let (vm_name_1, _, _, _) = common::unique_names("uart-p1");
    let (mut child1, pid1) =
        common::spawn_fcvm(&["podman", "run", "--name", &vm_name_1, common::TEST_IMAGE])
            .await
            .context("spawning phase 1 VM")?;

    common::poll_health_by_pid(pid1, 300).await?;
    println!("  Phase 1 VM healthy (PID: {})", pid1);

    common::kill_process(pid1).await;
    let _ = child1.wait().await;
    println!("  Phase 1 VM stopped, snapshot should be cached");

    // Brief pause to let the snapshot finalize
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Phase 2: Start VM from cached snapshot → warm start (new Firecracker process).
    // Capture logs to verify serial console output.
    println!("Phase 2: Starting VM from cached snapshot...");
    let (vm_name_2, _, _, _) = common::unique_names("uart-p2");
    let (mut child2, pid2, log_path) = common::spawn_fcvm_with_log_path(
        &["podman", "run", "--name", &vm_name_2, common::TEST_IMAGE],
        &vm_name_2,
    )
    .await
    .context("spawning phase 2 VM")?;

    common::poll_health_by_pid(pid2, 120).await?;
    println!("  Phase 2 VM healthy (PID: {})", pid2);

    // Wait for restore to complete — poll the log until fc-agent's phase
    // summary line appears (renamed when the phases gained timings).
    // The restore path includes clock sync + chronyc + gateway ping which can take
    // several seconds, so poll rather than sleep a fixed duration.
    println!("  Waiting for restore to complete...");
    let restore_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() > restore_deadline {
            anyhow::bail!(
                "timeout waiting for restore complete in log: {}",
                log_path.display()
            );
        }
        if let Ok(contents) = std::fs::read_to_string(&log_path) {
            if contents.contains("[fc-agent] restore phases complete") {
                println!("  Restore complete detected in log");
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Write a unique marker to /dev/ttyS0 from inside the VM.
    // If UART works, the marker flows through:
    //   echo → /dev/ttyS0 → Firecracker UART → Firecracker stdout → host log
    println!("  Writing marker to /dev/ttyS0: {}", marker);
    common::exec_in_vm(pid2, &[&format!("echo '{}' > /dev/ttyS0", marker)])
        .await
        .context("writing marker to /dev/ttyS0")?;

    // Brief pause for serial output to flush through the pipeline
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Stop the VM
    common::kill_process(pid2).await;
    let _ = child2.wait().await;

    // Wait for log consumers to flush
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Analyze the log file
    let contents = std::fs::read_to_string(&log_path)
        .with_context(|| format!("reading log file: {}", log_path.display()))?;

    println!(
        "  Log file: {} ({} bytes)",
        log_path.display(),
        contents.len()
    );

    // Check if this was actually a warm start (restored from cached snapshot)
    let was_warm_start = contents.contains("Pre-start snapshot hit")
        || contents.contains("Startup snapshot hit")
        || contents.contains("Restoring from cached snapshot");
    println!(
        "  Was warm start (restored from snapshot): {}",
        was_warm_start
    );

    // Count fc-agent serial console lines — these come from eprintln!() in the
    // guest, flow through /dev/ttyS0, and appear in Firecracker's stdout.
    // After warm start, fc-agent prints restore messages (e.g. "handling restore").
    let fc_agent_serial_lines: Vec<&str> = contents
        .lines()
        .filter(|line| {
            // Lines from Firecracker's serial console containing fc-agent output
            is_console_line(line) && line.contains("[fc-agent]")
        })
        .collect();
    println!(
        "  fc-agent serial console lines: {}",
        fc_agent_serial_lines.len()
    );
    for line in &fc_agent_serial_lines {
        println!("    {}", line.trim());
    }

    // Check for our explicit marker
    let found_marker = contents.lines().any(|line| {
        line.contains(&marker)
            && !line.contains("args=")
            && !line.contains("\"cmd\"")
            && !line.contains("exec")
    });
    println!("  UART marker found in host log: {}", found_marker);

    // Where the restore's own lines went (only relevant on warm start)
    let restore_log = RestoreLog::read(&contents);
    println!("  restore log: {restore_log:?}");

    // Assertions
    if was_warm_start {
        // On warm start an fc-agent line crosses the console: the one line that says the
        // restore's progress was queued for the host, or that progress itself when the
        // restore fell back to the console. If UART works, it is in the host log.
        restore_log.assert_console_after_restore(&log_path);

        assert!(
            found_marker,
            "UART marker '{}' not found in host log after warm start — \
             serial console is broken after snapshot restore.\n\
             Log: {}",
            marker,
            log_path.display()
        );
    } else {
        // Cold start — UART should definitely work
        assert!(
            found_marker,
            "UART marker '{}' not found even on cold start — \
             serial console is fundamentally broken.\n\
             Log: {}",
            marker,
            log_path.display()
        );
        println!("  NOTE: This was a cold start. Run the test again to test the restore path.");
    }

    println!("✅ SERIAL CONSOLE TEST PASSED!");
    Ok(())
}

/// Poll `log` until a line satisfies `found`, or `limit` passes.
async fn wait_for_log_line(
    log: &std::path::Path,
    limit: Duration,
    found: impl Fn(&str) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Ok(contents) = std::fs::read_to_string(log) {
            if contents.lines().any(&found) {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The source snapshot boundary holds the agent's console quiet from the prepare to
/// the resume, and a clone of a snapshot taken under the hold has a live console.
///
/// A snapshot of a running VM (a startup snapshot, `fcvm snapshot create`) is paused
/// right after `--prepare-snapshot-network`, and the agent logs into that window: the
/// prepared gate refuses the restore watcher's metadata fetches and the first five
/// refusals are logged. Two startup snapshots taken with those lines on the console
/// restored with dead consoles (CI runs 41036024614 on arm64, seven restores of
/// one snapshot; 41018542774 on x64). The pre-start snapshot was already protected by
/// the cache handshake's quiesce (#708).
///
/// The test runs the boundary's one-shots by hand, without a pause, and writes a
/// line into the agent's own stderr pipe in between, so the line takes the path the
/// hold governs (the agent's console writer).
///
/// RED BEFORE THE FIX: the held line reached the console while the boundary was
/// prepared.
#[tokio::test]
async fn the_snapshot_boundary_holds_the_agent_console_until_the_resume() -> Result<()> {
    let (vm_name, clone_name, snap_tag, _) = common::unique_names("uart-hold");
    let held = format!("UART-HELD-{}", std::process::id());

    let (mut child, pid, log) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning the source VM")?;

    let on_console =
        |marker: String| move |line: &str| is_console_line(line) && line.contains(&marker);
    let outcome = async {
        common::poll_health_by_pid(pid, 300).await?;
        common::exec_in_vm(pid, &["/usr/local/bin/fc-agent --prepare-snapshot-network"])
            .await
            .context("preparing the snapshot boundary")?;
        let written = common::exec_in_vm(
            pid,
            &[&format!(
                "P=$(pgrep -o -x fc-agent) && echo {held} > /proc/$P/fd/2"
            )],
        )
        .await;
        let leaked = match written {
            Ok(_) => {
                wait_for_log_line(&log, Duration::from_secs(3), on_console(held.clone())).await
            }
            Err(e) => {
                let _ =
                    common::exec_in_vm(pid, &["/usr/local/bin/fc-agent --resume-snapshot-network"])
                        .await;
                return Err(e.context("writing into the agent's stderr"));
            }
        };
        common::exec_in_vm(pid, &["/usr/local/bin/fc-agent --resume-snapshot-network"])
            .await
            .context("resuming the snapshot boundary")?;
        let released =
            wait_for_log_line(&log, Duration::from_secs(10), on_console(held.clone())).await;

        // A clone inherits the hold its snapshot was taken under and must release it.
        common::create_snapshot_by_pid(pid, &snap_tag).await?;
        let (mut clone, clone_pid, clone_log) = common::spawn_fcvm_with_log_path(
            &[
                "snapshot",
                "run",
                "--snapshot",
                &snap_tag,
                "--name",
                &clone_name,
            ],
            &clone_name,
        )
        .await
        .context("restoring the clone")?;
        let clone_console = async {
            common::poll_health_by_pid(clone_pid, 120).await?;
            // Any fc-agent line that crossed the clone's console proves its UART.
            let on_console = wait_for_log_line(&clone_log, Duration::from_secs(30), |l| {
                l.contains(AGENT_CONSOLE_LINE)
            })
            .await;
            // The restore's last progress line, by either channel, so the log read
            // below holds the whole restore. The verdict is on that read, not here.
            let _ = wait_for_log_line(&clone_log, Duration::from_secs(10), |l| {
                l.contains(RESTORE_PROGRESS_LINES[3])
            })
            .await;
            anyhow::Ok(on_console)
        }
        .await;
        common::kill_process(clone_pid).await;
        let _ = clone.wait().await;
        anyhow::Ok((leaked, released, clone_console?, clone_log))
    }
    .await;

    common::kill_process(pid).await;
    let _ = child.wait().await;
    let _ = common::delete_snapshot(&snap_tag).await;

    let (leaked, released, clone_console, clone_log) = outcome?;
    assert!(
        !leaked,
        "a line the agent wrote while the boundary was prepared reached the console; a pause \
         there can capture the UART mid-transmit; log at {}",
        log.display()
    );
    assert!(
        released,
        "the held line never reached the console after the resume; log at {}",
        log.display()
    );
    assert!(
        clone_console,
        "no fc-agent line reached the clone's console; log at {}",
        clone_log.display()
    );
    let contents = std::fs::read_to_string(&clone_log)
        .with_context(|| format!("reading {}", clone_log.display()))?;
    // The clone's VMM restored and did nothing else, so with no fallback the hand-off
    // line is the first line of fc-agent's on its console: the restore wrote no other.
    RestoreLog::read(&contents).assert_console_after_restore(&clone_log);
    Ok(())
}

/// What fc-agent writes to the serial console when it queues a restore's progress lines on
/// the output connection: `[fc-agent] restore: N progress lines queued for the host over vsock`.
const RESTORE_HAND_OFF_LINE: &str = "progress lines queued for the host over vsock";

/// How fcvm logs a line of fc-agent's that crossed the serial console.
const AGENT_CONSOLE_LINE: &str = "fcvm::console: [fc-agent]";

/// Lines every restore logs on its way to readiness.
const RESTORE_PROGRESS_LINES: [&str; 4] = [
    "[fc-agent] detected restore-epoch",
    "[fc-agent] handling restore",
    "[fc-agent] restore phase=tcp-cleanup",
    "[fc-agent] restore phases complete",
];

/// What a line of fc-agent's starts with, after `[fc-agent] `, when fc-agent's console
/// writer puts it on the console at once and ends the restore's hand-off (`URGENT_WORDS`
/// in fc-agent/src/console.rs; the reader's test below pins the two lists together).
const URGENT_WORDS: [&str; 5] = ["FATAL", "ERROR", "Error: ", "WARNING", "VITALS"];

/// Where a restored VM's log has the restore's own lines.
#[derive(Debug)]
struct RestoreLog {
    /// fc-agent's lines on the serial console.
    console_agent_lines: usize,
    /// fc-agent's lines on the serial console ahead of its hand-off line (all of them when
    /// there is none).
    console_lines_before_hand_off: Vec<String>,
    /// The hand-off lines on the serial console.
    console_hand_off_lines: usize,
    /// The restore progress lines on the serial console.
    progress_on_console: Vec<String>,
    /// The restore progress lines that came over the output connection, each of which
    /// says when the guest logged it.
    progress_from_agent: usize,
    /// The first console line that shows the restore put its lines on the console, as
    /// fc-agent documents it does: a line with `held_ms=` (a line it had held), or an
    /// urgent line of its own, ahead of any hand-off line.
    fallback: Option<String>,
}

impl RestoreLog {
    fn read(log: &str) -> Self {
        let is_progress = |line: &str| RESTORE_PROGRESS_LINES.iter().any(|p| line.contains(p));
        let is_urgent = |line: &str| {
            line.split_once(&format!("{AGENT_CONSOLE_LINE} "))
                .is_some_and(|(_, message)| URGENT_WORDS.iter().any(|w| message.starts_with(w)))
        };
        let agent_console_lines: Vec<&str> = log
            .lines()
            .filter(|line| {
                is_console_line(line)
                    && line.contains("[fc-agent]")
                    && !line.contains("[fcvm-vitals]")
            })
            .collect();
        let console_lines_before_hand_off: Vec<String> = agent_console_lines
            .iter()
            .take_while(|line| !line.contains(RESTORE_HAND_OFF_LINE))
            .map(|line| line.to_string())
            .collect();
        Self {
            console_agent_lines: agent_console_lines.len(),
            fallback: console_lines_before_hand_off
                .iter()
                .find(|line| line.contains(" held_ms=") || is_urgent(line))
                .cloned(),
            console_lines_before_hand_off,
            console_hand_off_lines: agent_console_lines
                .iter()
                .filter(|line| line.contains(RESTORE_HAND_OFF_LINE))
                .count(),
            progress_on_console: agent_console_lines
                .iter()
                .filter(|line| is_progress(line))
                .map(|line| line.to_string())
                .collect(),
            progress_from_agent: log
                .lines()
                .filter(|line| {
                    line.contains("fcvm::agent:") && line.contains(" logged=") && is_progress(line)
                })
                .count(),
        }
    }

    /// What a restored VM's log must show about its console.
    ///
    /// Always: an fc-agent line crossed the serial console. None means the restored UART
    /// is dead.
    ///
    /// When the restore handed its lines to the host: no progress line on the console,
    /// one hand-off line, nothing of fc-agent's ahead of it, and the progress lines in
    /// the log from the output connection.
    ///
    /// When it fell back to the console (`fallback`): only the first. fc-agent puts a
    /// restore's lines on the console when it logs a warning or an error, and when the
    /// restore is not acknowledged within its window, and those lines crossing the
    /// console are themselves the proof of the UART. 6 of 3,000 clones in a 128-way ramp
    /// logged `WARNING: chronyd pid N survived SIGKILL for 2s` during their restore, so a
    /// test that demanded the hand-off would fail about once in 500 runs on a loaded
    /// runner, and would blame the UART.
    fn assert_console_after_restore(&self, log: &std::path::Path) {
        assert!(
            self.console_agent_lines > 0,
            "no fc-agent line crossed the serial console after the restore: the serial \
             console (UART) is broken after snapshot restore. fc-agent IS running (the VM \
             is healthy, vsock works), but its output to /dev/ttyS0 never reaches \
             Firecracker's stdout. {self:#?}\nlog at {}",
            log.display()
        );
        match &self.fallback {
            Some(line) => println!(
                "  the restore put its lines on the console, not the output connection; \
                 the first that shows it: {}",
                line.trim()
            ),
            None => self.assert_progress_stayed_off_the_console(log),
        }
    }

    /// A restore writes one fc-agent line to the serial console, the hand-off line, and
    /// its progress lines reach fcvm's log over the output connection. Each byte on the
    /// 8250 is an exit to the VMM, and the progress lines were about 2.7 KB of them per
    /// restore.
    ///
    /// RED WITHOUT THE RESTORE LOG WINDOW (fc-agent/src/console.rs): every progress line
    /// was on the console and none came over the output connection.
    fn assert_progress_stayed_off_the_console(&self, log: &std::path::Path) {
        assert!(
            self.progress_on_console.is_empty(),
            "a restore wrote its progress to the serial console, with no warning, error \
             or held line ahead of it that would explain it: {self:#?}\nlog at {}",
            log.display()
        );
        assert_eq!(
            self.console_hand_off_lines,
            1,
            "a restore's one console line is missing or repeated: {self:#?}\nlog at {}",
            log.display()
        );
        assert!(
            self.console_lines_before_hand_off.is_empty(),
            "the console has fc-agent lines ahead of the restore's one: {self:#?}\nlog at {}",
            log.display()
        );
        assert!(
            self.progress_from_agent >= RESTORE_PROGRESS_LINES.len(),
            "the restore's progress lines did not reach fcvm's log over the output \
             connection: {self:#?}\nlog at {}",
            log.display()
        );
    }
}

/// The reader and its verdict on a log of each kind, without a VM.
///
/// The VM tests above cannot show that `assert_progress_stayed_off_the_console` fails on
/// a restore that wrote its progress to the console, or that a restore which fell back
/// to the console passes without it. This does.
///
/// RED BEFORE THE FALLBACK WAS READ: the two fallen-back logs failed the verdict, as a
/// VM test whose clone logged a warning during its restore did.
#[test]
fn the_restore_log_reader_tells_the_console_from_the_output_connection() {
    let stamp = "2026-01-01T00:00:10.120000Z  INFO";
    let progress = [
        "[fc-agent] detected restore-epoch: e",
        "[fc-agent] handling restore (epoch=e)",
        "[fc-agent] restore phase=tcp-cleanup epoch=e begin",
        "[fc-agent] restore phases complete (epoch=e): exec + egress ready elapsed_ms=9.8",
    ];
    let hand_off = format!(
        "{stamp} fcvm::console: [fc-agent] restore: 4 {RESTORE_HAND_OFF_LINE}\n\
         {stamp} firecracker: [fcvm-vitals] MemAvailable: 1 kB thread [fc-agent]\n"
    );
    let on_console = |lines: &[&str], suffix: &str| -> String {
        lines
            .iter()
            .map(|line| format!("{stamp} fcvm::console: {line}{suffix}\n"))
            .collect()
    };
    let from_agent: String = progress
        .iter()
        .map(|line| format!("{stamp} fcvm::agent: {line} logged=2026-01-01T00:00:10.100000Z\n"))
        .collect();
    let fails = |log: &RestoreLog| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            log.assert_console_after_restore(std::path::Path::new("canned"))
        }))
        .is_err()
    };

    // Every line on the console and nothing that explains it, as before the restore log.
    let log = RestoreLog::read(&on_console(&progress, ""));
    assert_eq!(log.progress_on_console.len(), 4, "{log:#?}");
    assert_eq!(
        (log.console_hand_off_lines, log.progress_from_agent),
        (0, 0)
    );
    assert_eq!(log.fallback, None);
    assert!(fails(&log), "progress on the console passed: {log:#?}");

    // Queued for the host: one console line, the rest under fcvm::agent with logged=.
    let log = RestoreLog::read(&(hand_off.clone() + &from_agent));
    assert!(log.progress_on_console.is_empty(), "{log:#?}");
    assert!(log.console_lines_before_hand_off.is_empty(), "{log:#?}");
    assert_eq!(
        (log.console_hand_off_lines, log.progress_from_agent),
        (1, 4)
    );
    assert_eq!((log.console_agent_lines, &log.fallback), (1, &None));
    assert!(!fails(&log), "a handed-over restore failed: {log:#?}");

    // The hand-off line alone: nothing proves the progress lines reached the log.
    let log = RestoreLog::read(&hand_off);
    assert!(fails(&log), "a log with no progress lines passed: {log:#?}");

    // An fc-agent line ahead of the hand-off line that is neither held nor urgent.
    let log = RestoreLog::read(&(on_console(&progress[..1], "") + &hand_off + &from_agent));
    assert!(
        fails(&log),
        "a progress line ahead of the hand-off passed: {log:#?}"
    );

    // No fc-agent line on the console at all: the UART is dead, whatever came over vsock.
    let log = RestoreLog::read(&from_agent);
    assert_eq!(log.console_agent_lines, 0);
    assert!(fails(&log), "a dead console passed: {log:#?}");

    // Fell back when its window ran out: the held lines are on the console with held_ms.
    let log = RestoreLog::read(&on_console(&progress, " held_ms=12.5"));
    assert_eq!(log.progress_on_console.len(), 4, "{log:#?}");
    assert!(
        log.fallback
            .as_deref()
            .is_some_and(|l| l.contains(progress[0])),
        "{log:#?}"
    );
    assert!(!fails(&log), "a fallen-back restore failed: {log:#?}");

    // Fell back on a warning: two held lines, the warning, the rest as they were logged.
    let warning = "[fc-agent] WARNING: chronyd pid 327 survived SIGKILL for 2s";
    let log = RestoreLog::read(
        &(on_console(&progress[..2], " held_ms=2427.9")
            + &on_console(&[warning], "")
            + &on_console(&progress[2..], "")),
    );
    assert!(
        !fails(&log),
        "a restore that logged a warning failed: {log:#?}"
    );
    // And on a warning with nothing held yet: the warning is what shows it.
    let log = RestoreLog::read(&(on_console(&[warning], "") + &on_console(&progress, "")));
    assert_eq!(
        log.fallback.as_deref().map(str::trim),
        Some(format!("{stamp} fcvm::console: {warning}").as_str())
    );
    assert!(!fails(&log), "{log:#?}");
    // A warning after the hand-off is no fallback: the restore's lines were handed over.
    let log = RestoreLog::read(&(hand_off.clone() + &from_agent + &on_console(&[warning], "")));
    assert_eq!(log.fallback, None);
    assert!(!fails(&log), "{log:#?}");
    // Each urgent word counts, and no other line of fc-agent's does.
    for word in URGENT_WORDS {
        let line = format!("[fc-agent] {word} x");
        let log = RestoreLog::read(&(on_console(&[&line], "") + &on_console(&progress, "")));
        assert!(log.fallback.is_some(), "{word:?} is not read as urgent");
    }
    // The words are fc-agent's own.
    let agent = include_str!("../fc-agent/src/console.rs");
    assert!(
        agent.contains(&format!(
            "const URGENT_WORDS: [&str; 5] = {URGENT_WORDS:?};"
        )),
        "fc-agent's urgent words are no longer {URGENT_WORDS:?}"
    );
}

/// Whether a line of fcvm's log is a guest console line: fc-agent's lines are logged under
/// `fcvm::console` (#1104), every other console line under the VMM's own `firecracker` target.
/// fc-agent's lines that came over the output connection are under `fcvm::agent` and are not
/// console lines.
fn is_console_line(line: &str) -> bool {
    line.contains("fcvm::console:") || line.contains("firecracker:")
}
