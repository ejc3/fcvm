use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use super::{types::generate_mac, NetworkConfig, NetworkManager, PortMapping, Protocol};
use crate::paths;
use crate::state::truncate_id;

/// Guest network addressing — pasta provides L2↔L4 translation via bridge
const GUEST_IP: &str = "10.0.2.100";
const GUEST_GATEWAY: &str = "10.0.2.2";
/// Namespace IP on bridge — enables nsenter health checks to route to guest
const NAMESPACE_IP: &str = "10.0.2.1";
/// Fixed MAC for the bridge, so the guest can hold an AUTHORITATIVE neighbour
/// entry for NAMESPACE_IP instead of racing for one.
///
/// Both the bridge and pasta answer ARP for 10.0.2.1: the bridge because it
/// owns the address, pasta because it answers for the subnet it routes. Whoever
/// wins that race decides where the guest sends its replies. When pasta won,
/// the guest sent its SYN-ACK to pasta's MAC, pasta correctly reset a
/// connection it had never opened, and the published port went silent with no
/// drop recorded anywhere (2026-08-15). Measured directly on the wire:
///
///   SYN      br0-mac  > guest-mac   10.0.2.1 > 10.0.2.100  [S]
///   SYN-ACK  guest-mac > 9a:55:...  10.0.2.100 > 10.0.2.1  [S.]   <- pasta, not br0
///   RST      9a:55:... > guest-mac  10.0.2.1 > 10.0.2.100  [R]
///
/// A fixed MAC lets fc-agent install a PERMANENT neighbour entry, which ARP
/// replies cannot override, so the race has no outcome to win. pasta already
/// uses a fixed MAC (9a:55:9a:55:9a:55) for the same kind of reason.
pub const NAMESPACE_MAC: &str = "02:fc:00:00:02:01";

/// Guest IPv6 addressing (pasta copies host IPv6 with fd00::/64 fallback)
const GUEST_IPV6: &str = "fd00::100";
const GUEST_IPV6_GATEWAY: &str = "fd00::2";

/// Bridge device name
const BRIDGE_DEVICE: &str = "br0";

/// TAP device name for pasta
const PASTA_DEVICE_NAME: &str = "pasta0";

/// Timeout for waiting for pasta PID file (readiness signal)
const PASTA_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for waiting for pasta's TAP device to appear in the namespace
const PASTA_DEVICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Budget for the post-restore readiness check: the guest must answer, and its
/// published ports must accept, within this window.
const GUEST_ANSWER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Number of recent pasta stderr lines kept for error reporting
const PASTA_STDERR_TAIL_LINES: usize = 20;

/// Upper bound on waiting for the stderr reader to reach EOF after pasta exits,
/// so the failure error can include what it actually printed. The wait ends on
/// EOF (pasta's exit closed the write end); this only bounds the pathological
/// case of an inherited, still-open pipe.
const PASTA_STDERR_EOF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether `ip neigh show` proves that the bridge resolved the guest's MAC.
///
/// This is an L2 fact and nothing more. A resolved entry is necessary for
/// readiness but NOT sufficient: the neighbour table keeps a REACHABLE entry
/// for a guest that has stopped answering, so this predicate cannot fail when
/// the guest goes silent. [`wait_for_guest_to_answer`] pairs it with a TCP
/// answer from the guest, which is the part that observes the guest itself.
fn neighbor_is_resolved(output: &str) -> bool {
    let fields: Vec<&str> = output.split_whitespace().collect();
    fields
        .windows(2)
        .any(|w| w[0] == "lladdr" && !w[1].is_empty())
        && fields.iter().any(|field| {
            matches!(
                *field,
                "PERMANENT" | "NOARP" | "REACHABLE" | "STALE" | "DELAY" | "PROBE"
            )
        })
}

/// The two namespace observations the restore readiness loop makes of a guest.
///
/// Both are `nsenter` invocations in production, which makes the readiness
/// decision untestable without a namespace and a live VM. Keeping the loop
/// generic over this boundary lets a scripted guest — one that answers, one
/// that stays silent behind a resolved neighbour entry — drive the decision
/// deterministically in unit tests, the same way [`crate::utils::DirEventSource`]
/// drives the PID-file wait.
trait GuestProbe {
    /// Ask the guest's IP stack to answer a TCP SYN on `port` and report whether
    /// anything came back.
    ///
    /// ANY response proves the guest: a SYN-ACK means the service is up, and an
    /// RST (connection refused) means the guest's kernel processed the segment
    /// and answered — which is all readiness needs. `Ok(false)` means silence:
    /// the guest may simply not be up yet, which is the case the loop retries.
    /// `Err` means the probe itself could not run.
    ///
    /// TCP rather than ICMP echo, deliberately: `net.ipv4.icmp_echo_ignore_all=1`
    /// is a legitimate guest policy, and main's
    /// `test_clone_port_forward_rootless` snapshots a guest with exactly that
    /// policy (a42eda55) to pin the contract that readiness "must prove ARP/L2
    /// resolution and the forwarded TCP path itself, rather than requiring an
    /// unrelated ping reply". A published port is the one part of the guest the
    /// operator has declared traffic will arrive on, so probing it never tests a
    /// policy the guest is entitled to refuse.
    async fn answers_tcp(&mut self, port: u16, budget: std::time::Duration) -> Result<TcpAnswer>;

    /// Read the guest's neighbour entry from the bridge, as `ip neigh` prints it.
    async fn neighbor(&mut self, budget: std::time::Duration) -> Result<String>;
}

/// One TCP probe: whether the guest answered, and what the prober said if not.
struct TcpAnswer {
    answered: bool,
    /// Diagnostic detail for the failure message; empty when the guest answered.
    detail: String,
}

/// Total wall-clock budget for the post-failure forensic dump.
const FORENSICS_BUDGET_SECS: u64 = 4;

/// Describe a probe that got no answer, so the reason is never blank.
///
/// `timeout` kills the probing bash mid-connect, so a silent guest leaves
/// stderr EMPTY — indistinguishable from "the prober itself printed nothing",
/// a different failure. Exit 124 means no SYN-ACK and no RST were OBSERVED
/// before the deadline, and that is all it means: a dropped SYN, a guest
/// firewall DROP, and a lost reply are equally consistent with it, so this
/// must not claim the guest never received the packet.
fn describe_silent_probe(code: Option<i32>, stderr: &str) -> String {
    let code = code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let meaning = if code == "124" {
        "timeout: no SYN-ACK or RST observed before the deadline"
    } else {
        "probe error"
    };
    if stderr.is_empty() {
        format!("exit={code} ({meaning}), prober said nothing")
    } else {
        format!("exit={code} ({meaning}): {stderr}")
    }
}

/// Production [`GuestProbe`]: runs the probes inside the VM's network namespace
/// via the holder PID.
struct NsenterGuestProbe {
    nsenter_prefix: Vec<String>,
}

impl NsenterGuestProbe {
    fn new(nsenter_prefix: Vec<String>) -> Self {
        Self { nsenter_prefix }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.nsenter_prefix[0]);
        command
            .args(&self.nsenter_prefix[1..])
            .args(args)
            .kill_on_drop(true);
        command
    }
}

impl GuestProbe for NsenterGuestProbe {
    async fn answers_tcp(&mut self, port: u16, budget: std::time::Duration) -> Result<TcpAnswer> {
        // `bash -c 'exec 3<>/dev/tcp/…'` is a plain connect(2) with bash's
        // error reporting, run single-threaded through nsenter — which matters:
        // joining the holder's USER namespace (required to enter its net
        // namespace without root) is impossible from this multithreaded
        // process, so the probe must be a subprocess. `timeout` bounds the
        // no-answer case; refused comes back in one RTT.
        let probe_timeout = budget.as_secs_f64().clamp(0.05, 0.5);
        let script = format!("exec 3<>/dev/tcp/{GUEST_IP}/{port}");
        let mut command = self.command(&[
            "env",
            "LC_ALL=C",
            "timeout",
            // SIGKILL shortly after the TERM. `kill_on_drop` reaps only the
            // direct child, so an abandoned attempt's `bash` is reachable only
            // through this timeout; if TERM alone did not land, one process per
            // retry would sit in the namespace holding a socket.
            "-k",
            "0.1",
            &format!("{probe_timeout:.2}"),
            "bash",
            "-c",
            &script,
        ]);
        command.stdout(Stdio::null()).stderr(Stdio::piped());
        let output = tokio::time::timeout(budget, command.output())
            .await
            .context("TCP probe exceeded pasta's readiness deadline")?
            .context("running the TCP probe via nsenter in namespace")?;

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // Exit 0: connected (SYN-ACK). "Connection refused": the guest's kernel
        // sent an RST — it is alive, nothing is bound on the port yet, and that
        // is still an answer. Exit 124 is `timeout` reporting silence; anything
        // else (no route, probe misconfiguration) is treated as silence and
        // retried, with the detail carried into the deadline error.
        let answered = output.status.success() || stderr.contains("Connection refused");
        // Always name the exit status. `timeout` kills bash mid-connect, so a
        // silent guest leaves stderr EMPTY — and an empty detail then reads
        // identically to "the probe itself produced nothing", which is a
        // different failure. Distinguishing them is the whole diagnosis: exit
        // 124 with no RST means packets are not reaching the guest's TCP stack
        // (an L3/L4 path problem), whereas an RST would have counted as an
        // answer and passed. Observed 2026-08-15 as `detail=` with no way to
        // tell the two apart.
        let detail = if answered {
            String::new()
        } else {
            describe_silent_probe(output.status.code(), &stderr)
        };
        Ok(TcpAnswer { answered, detail })
    }

    async fn neighbor(&mut self, budget: std::time::Duration) -> Result<String> {
        let mut command =
            self.command(&["ip", "neigh", "show", "to", GUEST_IP, "dev", BRIDGE_DEVICE]);
        command.stderr(Stdio::piped());
        let output = tokio::time::timeout(budget, command.output())
            .await
            .context("neighbor query exceeded pasta's readiness deadline")?
            .context("reading guest neighbour entry via nsenter in namespace")?;
        if !output.status.success() {
            anyhow::bail!(
                "failed to inspect ARP entry for guest {} on {}: {}",
                GUEST_IP,
                BRIDGE_DEVICE,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// How long the readiness loop waits between probe rounds.
const GUEST_ANSWER_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

/// Cap on ONE probe attempt, so a single stuck attempt cannot consume the whole
/// readiness budget.
///
/// Run 31906708922 failed with `neighbour: ""; probe: (silence)` after the full
/// 5s, and the artifacts show that reading as a lie: fc-agent had finished the
/// restore 5s earlier with `gate=open`, the namespace held a TIME-WAIT socket
/// from `10.0.2.1 -> 10.0.2.100:80` (the probe's own connect, so the guest DID
/// answer), the neighbour entry was REACHABLE, and the host was 95% idle with a
/// run queue of 1. What actually happened is that the loop emitted ZERO
/// per-round debug lines in those 5s: one attempt was handed `remaining` and
/// never came back, so no round ever finished, `last_neighbor` was never
/// assigned, and the empty strings got reported as guest silence.
///
/// An attempt that outlives this is abandoned and RETRIED. The loop must not
/// depend on a subprocess honouring its own budget -- the probe already asks
/// `timeout` to bound `bash`, and that still did not bound the call.
const GUEST_PROBE_ATTEMPT_BUDGET: std::time::Duration = std::time::Duration::from_secs(1);

/// Wait until the guest itself answers, or the deadline expires.
///
/// Readiness requires BOTH a TCP answer from the guest and a resolved neighbour
/// entry, and the TCP answer is the load-bearing half. A neighbour entry
/// survives the guest going quiet, so gating on it alone cannot fail when the
/// guest is silent — a 808-clone benchmark declared 5 silent clones ready and 3
/// of those 5 then hung at the client's own ~100s deadline. Requiring an answer
/// turns that into a named failure here, inside the existing budget.
///
/// The probe is a TCP SYN to a published guest port, not an ICMP echo: an echo
/// requirement fails a guest that legitimately sets
/// `net.ipv4.icmp_echo_ignore_all=1`, which main's
/// `test_clone_port_forward_rootless` bakes into its snapshot (a42eda55)
/// precisely to pin that contract. An RST counts as an answer — the guest's
/// kernel spoke, which is what readiness observes; whether a service is bound
/// yet is the health check's question, not this one's.
///
/// Retries until the deadline. Time is `tokio::time` throughout so tests drive
/// the loop under a paused clock instead of racing real windows.
async fn wait_for_guest_to_answer<P: GuestProbe>(
    probe: &mut P,
    probe_port: u16,
    deadline: tokio::time::Instant,
) -> Result<()> {
    // Carried out of the loop so the deadline message can distinguish "the guest
    // never appeared at L2" from "the guest is at L2 but never answered".
    let mut last_neighbor = String::new();
    let mut last_detail = String::new();
    let started = std::time::Instant::now();
    let mut rounds: u32 = 0;
    // Attempts abandoned for exceeding GUEST_PROBE_ATTEMPT_BUDGET. Reported in
    // the deadline error: it is the difference between "the guest said nothing"
    // and "this check never got an answer out of its own subprocess".
    let mut stalled_attempts: u32 = 0;

    loop {
        rounds += 1;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(guest_unanswered_error(
                &last_neighbor,
                &last_detail,
                rounds,
                stalled_attempts,
            ));
        }
        // Each attempt is bounded separately from the deadline, so one stuck
        // attempt costs a round instead of the whole budget.
        let attempt = remaining.min(GUEST_PROBE_ATTEMPT_BUDGET);
        let answer =
            match tokio::time::timeout(attempt, probe.answers_tcp(probe_port, attempt)).await {
                Ok(Ok(answer)) => answer,
                // The probe ran out of ITS budget, or never returned within ours.
                // Both mean this round failed, not that the guest is silent, so
                // retry and carry the distinction into the deadline error. A
                // stalled check is never again reported as an unreachable guest.
                Ok(Err(error)) if error.to_string().contains("readiness deadline") => {
                    stalled_attempts += 1;
                    TcpAnswer {
                        answered: false,
                        detail: format!("TCP probe attempt exceeded its {attempt:?} budget"),
                    }
                }
                Ok(Err(error)) => return Err(error),
                Err(_elapsed) => {
                    stalled_attempts += 1;
                    TcpAnswer {
                        answered: false,
                        detail: format!("TCP probe attempt did not return within {attempt:?}"),
                    }
                }
            };
        last_detail = answer.detail;
        if !answer.answered {
            debug!(
                guest_ip = GUEST_IP,
                probe_port,
                detail = %last_detail,
                "guest did not answer the readiness TCP probe"
            );
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(guest_unanswered_error(
                &last_neighbor,
                &last_detail,
                rounds,
                stalled_attempts,
            ));
        }
        let attempt = remaining.min(GUEST_PROBE_ATTEMPT_BUDGET);
        // A stalled query keeps the previous reading for the ERROR MESSAGE only:
        // an empty string means "no entry", and a query that never returned has
        // not learned that. It must not count as evidence of readiness. Both
        // halves have to hold in the SAME round, or a neighbour seen REACHABLE
        // in round 1 could pair with round 2's TCP answer and declare a guest
        // ready whose entry has since gone FAILED, which is precisely the
        // stale-evidence failure this function exists to prevent.
        let neighbor_is_fresh;
        match tokio::time::timeout(attempt, probe.neighbor(attempt)).await {
            Ok(Ok(neighbor)) => {
                last_neighbor = neighbor;
                neighbor_is_fresh = true;
            }
            Ok(Err(error)) if error.to_string().contains("readiness deadline") => {
                stalled_attempts += 1;
                neighbor_is_fresh = false;
            }
            Ok(Err(error)) => return Err(error),
            Err(_elapsed) => {
                stalled_attempts += 1;
                neighbor_is_fresh = false;
            }
        }
        let resolved = neighbor_is_fresh && neighbor_is_resolved(&last_neighbor);

        if answer.answered && resolved {
            // Report HOW LONG readiness took, on success as well as failure.
            // Without this number a deadline miss is unattributable: "slow" and
            // "broken" look identical from one timed-out run, and the difference
            // is whether the successful runs crowd the deadline or sit two
            // orders of magnitude below it (they sit at ~84ms).
            info!(
                guest_ip = GUEST_IP,
                probe_port,
                readiness_ms = started.elapsed().as_secs_f64() * 1000.0,
                rounds,
                stalled_attempts,
                neighbor = %last_neighbor.trim(),
                "guest answered and its MAC is resolved"
            );
            return Ok(());
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(guest_unanswered_error(
                &last_neighbor,
                &last_detail,
                rounds,
                stalled_attempts,
            ));
        }
        debug!(
            guest_ip = GUEST_IP,
            probe_port,
            answered = answer.answered,
            neighbor_resolved = resolved,
            "guest not ready yet, retrying"
        );
        tokio::time::sleep(remaining.min(GUEST_ANSWER_RETRY_DELAY)).await;
    }
}

/// Deadline error for [`wait_for_guest_to_answer`], naming which half was missing.
/// The host's load at the instant a readiness probe gave up.
///
/// A readiness timeout is unattributable without this. "The box was busy" is the
/// reflex explanation for any deadline miss, and it is worth nothing unless the
/// number sits beside the failure. On the run this was added for, the host was
/// 95% idle with a run queue of 1. `/proc/loadavg`'s fourth field is
/// `runnable/total`, which separates "many tasks exist" from "many want CPU".
fn host_load_snapshot() -> String {
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(raw) => {
            let fields: Vec<&str> = raw.split_whitespace().collect();
            match fields.as_slice() {
                [one, five, fifteen, runnable, ..] => {
                    format!("load {one}/{five}/{fifteen}, runnable {runnable}")
                }
                _ => format!("load (unparsable: {})", raw.trim()),
            }
        }
        Err(error) => format!("load (unavailable: {error})"),
    }
}

fn guest_unanswered_error(
    neighbor: &str,
    detail: &str,
    rounds: u32,
    stalled_attempts: u32,
) -> anyhow::Error {
    let neighbor = neighbor.trim();
    let detail = if detail.is_empty() {
        "(silence)"
    } else {
        detail
    };
    if neighbor_is_resolved(neighbor) {
        // The failure this whole path exists to catch: pasta can reach the
        // guest's MAC, so every host-side check passes, but nothing is answering
        // behind it. Report it here rather than letting a client discover it.
        anyhow::anyhow!(
            "guest {} resolved to a MAC but never answered a TCP probe within {:?}: \
             the guest is not reachable even though its neighbour entry is present; \
             neighbour: {}; probe: {}; rounds {} ({} stalled); host {}",
            GUEST_IP,
            GUEST_ANSWER_DEADLINE,
            neighbor,
            detail,
            rounds,
            stalled_attempts,
            host_load_snapshot()
        )
    } else {
        anyhow::anyhow!(
            "guest {} never appeared at L2 within {:?}: no resolved neighbour entry \
             and no TCP answer; neighbour: {:?}; probe: {}; rounds {} ({} stalled); \
             host {}",
            GUEST_IP,
            GUEST_ANSWER_DEADLINE,
            neighbor,
            detail,
            rounds,
            stalled_attempts,
            host_load_snapshot()
        )
    }
}

/// Heredoc delimiter separating the batched `ip` commands from the shell script.
const IP_BATCH_DELIMITER: &str = "FCVM_IP_BATCH";

/// A list of `ip` commands rendered as a single shell script.
///
/// Configuring one VM's namespace takes ~10 `ip` invocations, and each one is a
/// fork+exec. `ip -batch` reads commands from a stream and applies them all in
/// ONE process, so a whole phase costs `nsenter` + `bash` + `ip` instead of a
/// process per command.
///
/// Semantics are unchanged: `ip -batch` runs the commands in order and, without
/// `-force`, **aborts at the first failure** — nothing after it is applied, just
/// as `set -e` did for the one-command-per-line form. On failure it prints
/// `Command failed -:<line>`, and [`IpBatchScript::describe_failure`] maps that
/// line back to the step's description so an error still names the step that
/// failed.
#[derive(Debug, Clone)]
pub struct IpBatchScript {
    /// (human description, `ip` arguments) — one entry per batch line, in order.
    steps: Vec<(String, String)>,
    script: String,
}

impl IpBatchScript {
    /// Render `steps` as one `ip -batch` invocation, followed by `trailing_shell`
    /// lines (for things `ip` cannot do, e.g. writing a sysctl — those are shell
    /// builtins and cost no extra process).
    fn new(steps: Vec<(String, String)>, trailing_shell: &[&str]) -> Self {
        // One step per physical line, no blanks or comments inside the heredoc:
        // `ip -batch` reports the physical line number, so line N is step N.
        let batch: String = steps
            .iter()
            .map(|(_, args)| format!("{}\n", args))
            .collect();
        let mut script = format!(
            "set -e\nip -batch - <<'{delim}'\n{batch}{delim}\n",
            delim = IP_BATCH_DELIMITER,
            batch = batch,
        );
        for line in trailing_shell {
            script.push_str(line);
            script.push('\n');
        }
        Self { steps, script }
    }

    /// The shell script to run under `bash -c` inside the namespace.
    pub fn script(&self) -> &str {
        &self.script
    }

    /// One-line summary of the steps, for debug logging.
    pub fn summary(&self) -> String {
        self.steps
            .iter()
            .map(|(_, args)| format!("ip {}", args))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Turn a failed run's stderr into a message naming the step that failed.
    ///
    /// `ip -batch` prints `Command failed -:<line>` for the aborting command;
    /// anything else (nsenter/bash failures, or an `ip` too old to report a
    /// line) falls back to the raw stderr so no diagnostic is ever swallowed.
    pub fn describe_failure(&self, stderr: &str) -> String {
        let stderr = stderr.trim();
        for token in stderr.split_whitespace() {
            let Some(line_no) = token.strip_prefix("-:") else {
                continue;
            };
            let Ok(line_no) = line_no.trim_end_matches(':').parse::<usize>() else {
                continue;
            };
            if let Some((desc, args)) = line_no.checked_sub(1).and_then(|i| self.steps.get(i)) {
                return format!(
                    "step {}/{} ({}) failed running `ip {}`: {}",
                    line_no,
                    self.steps.len(),
                    desc,
                    args,
                    if stderr.is_empty() {
                        "(no stderr)"
                    } else {
                        stderr
                    }
                );
            }
        }
        if stderr.is_empty() {
            "(no stderr)".to_string()
        } else {
            stderr.to_string()
        }
    }
}

/// Rootless networking using pasta with bridge architecture
///
/// This mode uses user namespaces and pasta (from passt project) for true
/// unprivileged operation. No sudo/root required — everything runs in user
/// namespace via nsenter.
///
/// Architecture (L2 Bridge + L4 translation):
/// ```text
/// Host                    | User Namespace (unshare --user --net)
///                         |
/// pasta  <----------------+-- pasta0 --+
///   (L2↔L4 translation,   |            |
///    splice zero-copy)     |           br0 (L2 bridge)
///                         |            |
///                         |          tap-fc ---> Firecracker VM
///                         |                      (guest: 10.0.2.100)
/// ```
///
/// pasta uses L4 translation for efficient networking without a userspace TCP/IP stack.
/// Outbound traffic goes through pasta's L2 TAP path (userspace processing).
/// Inbound port forwarding uses splice(2) for zero-copy socket-to-socket transfer:
/// pasta binds on the host, splices directly into the namespace, where the kernel
/// routes to the VM via br0 → tap-fc.
///
/// Setup sequence:
/// 1. Spawn holder process: `unshare --user --net -- sleep infinity`
/// 2. Run pre-setup via nsenter: create Firecracker TAP only
/// 3. Start pasta: creates pasta0 TAP in namespace with L2↔L4 translation
/// 4. Run post-setup via nsenter: create bridge, add both TAPs, enable ip_forward
/// 5. Run Firecracker via nsenter: `nsenter -t HOLDER_PID -U -n -- firecracker ...`
/// 6. Health checks via nsenter: `nsenter -t HOLDER_PID -U -n -- curl guest_ip:80`
pub struct PastaNetwork {
    vm_id: String,
    tap_device: String,   // TAP device for Firecracker (tap-fc)
    pasta_device: String, // TAP device created by pasta (pasta0)
    port_mappings: Vec<PortMapping>,

    // Network addressing (IPv4) — guest uses 10.0.2.x via bridge
    guest_ip: String, // Guest VM IP (10.0.2.100)

    // Network addressing (IPv6)
    guest_ipv6: String, // fd00::100

    // State (populated during setup)
    pasta_process: Option<Child>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>, // last few pasta stderr lines, for failure attribution
    // Reader task for pasta's stderr pipe. Awaited (bounded) before rendering
    // `stderr_tail` on the paths that report a dead pasta, so the attribution
    // carries what pasta printed instead of racing the reader task.
    stderr_reader: Option<tokio::task::JoinHandle<()>>,
    pid_file: Option<PathBuf>,
    loopback_ip: Option<String>, // Unique loopback IP for port forwarding (127.x.y.z)
    holder_pid: Option<u32>,     // Namespace PID (set in post_start)
    restore_mode: bool,          // Skip port probe in post_start (VM not loaded yet)
}

impl PastaNetwork {
    /// Dump where the packets stopped, from inside the VM's own network
    /// namespace, while it still exists.
    ///
    /// Ordered L2 -> L4 so the reader can see how far a packet got: the
    /// neighbour table (did we ever learn the guest's MAC), the tap and bridge
    /// counters (did frames actually leave the host), the namespace's routes
    /// and addresses, and the sockets pasta is holding. Every command is
    /// bounded and its failure is logged rather than swallowed, because a
    /// silently missing section reads as "nothing was wrong" — the same
    /// fail-open shape this project keeps paying for.
    async fn dump_unreachable_guest_forensics(&self, holder_pid: u32, probe_port: u16) {
        let prefix = self.build_nsenter_prefix(holder_pid);
        let probes: [(&str, Vec<&str>); 7] = [
            ("neighbours", vec!["ip", "neigh", "show"]),
            ("links + counters", vec!["ip", "-s", "link"]),
            ("addresses", vec!["ip", "-o", "addr", "show"]),
            ("routes", vec!["ip", "route", "show"]),
            ("sockets", vec!["ss", "-tanp"]),
            ("nat rules", vec!["iptables", "-t", "nat", "-S"]),
            // Conntrack last and deliberately: a restored guest keeps the
            // snapshot's conntrack table (restore destroys SOCKETS via
            // cookie-bound SOCK_DESTROY but never flushes conntrack), and the
            // guest's loopback containment DROPS eth0 traffic to 127.0.0.0/8
            // unless conntrack records that WE translated it. A stale entry
            // colliding with a fresh probe's 5-tuple would therefore produce
            // exactly the silence observed, and nothing else in this dump can
            // tell that apart from a lost packet.
            (
                "conntrack",
                vec!["conntrack", "-L", "-p", "tcp", "--dport", "9222"],
            ),
        ];
        warn!(
            guest_ip = GUEST_IP,
            probe_port,
            holder_pid,
            "guest never answered; dumping namespace forensics before teardown"
        );
        // ONE budget for the whole dump, not one per command: six serial 2s
        // commands could add 12s to the failure path before teardown, changing
        // the timing of the very flow being diagnosed.
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(FORENSICS_BUDGET_SECS);
        for (label, args) in probes {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                warn!(
                    section = label,
                    "forensics budget exhausted; section skipped"
                );
                continue;
            }
            let mut command = Command::new(&prefix[0]);
            command
                .args(&prefix[1..])
                .args(&args)
                .kill_on_drop(true)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match tokio::time::timeout(remaining, command.output()).await {
                Ok(Ok(output)) => {
                    // Report a non-zero exit even when the command wrote to
                    // stdout: `ss` and `iptables` can print a partial answer and
                    // still fail, and rendering that as success is how a
                    // diagnostic starts lying.
                    if !output.status.success() {
                        let err = String::from_utf8_lossy(&output.stderr);
                        warn!(
                            section = label,
                            status = %output.status,
                            stderr = %err.trim(),
                            "forensics command exited non-zero"
                        );
                    }
                    let text = String::from_utf8_lossy(&output.stdout);
                    let text = text.trim();
                    if text.is_empty() {
                        if output.status.success() {
                            warn!(section = label, "forensics section empty");
                        }
                    } else {
                        for line in text.lines().take(40) {
                            warn!(section = label, "{}", line);
                        }
                    }
                }
                Ok(Err(error)) => warn!(section = label, %error, "forensics command failed"),
                Err(_) => warn!(section = label, "forensics command timed out"),
            }
        }
    }

    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            pasta_device: PASTA_DEVICE_NAME.to_string(),
            port_mappings,
            guest_ip: GUEST_IP.to_string(),
            guest_ipv6: GUEST_IPV6.to_string(),
            pasta_process: None,
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            stderr_reader: None,
            pid_file: None,
            loopback_ip: None,
            holder_pid: None,
            restore_mode: false,
        }
    }

    /// Set a unique loopback IP for port forwarding (127.x.y.z)
    ///
    /// Each VM gets a unique loopback IP so multiple VMs can forward the same
    /// port numbers (e.g., all VMs can have -p 8080:80).
    ///
    /// On Linux, the entire 127.0.0.0/8 range routes to loopback without needing
    /// `ip addr add`. We just bind directly to 127.0.0.2:8080, 127.0.0.3:8080, etc.
    /// This is fully rootless!
    pub fn with_loopback_ip(mut self, loopback_ip: String) -> Self {
        self.loopback_ip = Some(loopback_ip);
        self
    }

    /// Skip port forwarding probe in post_start() for snapshot restore.
    ///
    /// During snapshot restore, post_start() runs BEFORE the VM snapshot is loaded
    /// into Firecracker. Probing ports at that point forces pasta to attempt L2
    /// forwarding to a non-existent guest, which can poison pasta's internal
    /// connection tracking and cause subsequent connections to return 0 bytes.
    /// The proper verification happens later via verify_port_forwarding() after
    /// the VM is resumed and fc-agent has sent its gratuitous ARP.
    pub fn with_restore_mode(mut self) -> Self {
        self.restore_mode = true;
        self
    }

    /// Get the loopback IP assigned to this VM for port forwarding
    pub fn loopback_ip(&self) -> Option<&str> {
        self.loopback_ip.as_deref()
    }

    /// Build the holder command for creating the namespace
    ///
    /// Returns command to spawn a holder process that keeps the namespace alive.
    /// The holder runs `sleep infinity` which blocks forever until killed.
    /// Note: We use sleep instead of cat because cat requires stdin management.
    ///
    /// UID/GID mapping is handled by setup_namespace_mappings() in common.rs after
    /// the namespace is created (tries newuidmap first, falls back to single-UID mapping).
    pub fn build_holder_command(&self) -> Vec<String> {
        vec![
            "unshare".to_string(),
            "--user".to_string(),
            "--net".to_string(),
            "--".to_string(),
            "sleep".to_string(),
            "infinity".to_string(),
        ]
    }

    /// Build the pre-pasta setup script to run inside the namespace via nsenter.
    ///
    /// Creates the Firecracker TAP device, brings loopback up, and verifies the
    /// TAP exists — every `ip` command in ONE `ip -batch` process (see
    /// [`IpBatchScript`]). The bridge and pasta0 TAP are set up after pasta
    /// starts (pasta creates its own TAP).
    ///
    /// Run via: nsenter -t HOLDER_PID -U -n -- bash -c '<script()>'
    pub fn build_setup_script(&self) -> IpBatchScript {
        IpBatchScript::new(
            vec![
                (
                    format!("create TAP device {} for Firecracker", self.tap_device),
                    format!("tuntap add {} mode tap", self.tap_device),
                ),
                (
                    format!("bring TAP device {} up", self.tap_device),
                    format!("link set {} up", self.tap_device),
                ),
                (
                    "bring loopback up".to_string(),
                    "link set lo up".to_string(),
                ),
                // Verification is the last batch step rather than a second
                // nsenter+ip process: `ip -batch` already stops at the first
                // failure, so reaching this line means every step above applied.
                (
                    format!("verify TAP device {} exists", self.tap_device),
                    format!("link show {}", self.tap_device),
                ),
            ],
            &[],
        )
    }

    /// Build the post-pasta setup script that creates the bridge after pasta is ready
    ///
    /// Connects pasta's TAP and Firecracker's TAP via an L2 bridge.
    /// Port forwarding: pasta splices inbound loopback connections directly into the
    /// namespace, where they route via br0 → tap-fc → VM. Outbound traffic goes
    /// through pasta's L2 translation: tap-fc → br0 → pasta0 → pasta → host.
    ///
    /// The caller (post_start) waits for pasta's TAP device to exist via
    /// wait_for_pasta_device() before running this script.
    ///
    /// All six `ip` commands run in ONE `ip -batch` process; enabling IP
    /// forwarding is a shell redirect (no extra process).
    pub fn build_bridge_script(&self) -> IpBatchScript {
        let bridge = BRIDGE_DEVICE;
        IpBatchScript::new(
            vec![
                (
                    format!(
                        "bring {} up (pasta creates it but leaves it down without --config-net)",
                        self.pasta_device
                    ),
                    format!("link set {} up", self.pasta_device),
                ),
                (
                    format!("create L2 bridge {}", bridge),
                    format!("link add {} type bridge", bridge),
                ),
                (
                    format!("pin bridge {} MAC to {}", bridge, NAMESPACE_MAC),
                    format!("link set {} address {}", bridge, NAMESPACE_MAC),
                ),
                (
                    format!("bring bridge {} up", bridge),
                    format!("link set {} up", bridge),
                ),
                (
                    format!("add pasta TAP {} to bridge {}", self.pasta_device, bridge),
                    format!("link set {} master {}", self.pasta_device, bridge),
                ),
                (
                    format!(
                        "add Firecracker TAP {} to bridge {}",
                        self.tap_device, bridge
                    ),
                    format!("link set {} master {}", self.tap_device, bridge),
                ),
                (
                    format!(
                        "add health-check IP {}/24 to bridge {}",
                        NAMESPACE_IP, bridge
                    ),
                    format!("addr add {}/24 dev {}", NAMESPACE_IP, bridge),
                ),
            ],
            // Enable IP forwarding — a bash redirect, not another process.
            &["echo 1 > /proc/sys/net/ipv4/ip_forward"],
        )
    }

    /// Build the nsenter prefix command for running processes in the namespace
    ///
    /// Returns: ["nsenter", "-t", "PID", "-U", "-n", "--preserve-credentials", "--"]
    /// The --preserve-credentials flag keeps UID/GID/groups (including kvm) for KVM access.
    /// Append command and args after this.
    pub fn build_nsenter_prefix(&self, holder_pid: u32) -> Vec<String> {
        vec![
            "nsenter".to_string(),
            "-t".to_string(),
            holder_pid.to_string(),
            "-U".to_string(),
            "-n".to_string(),
            "--preserve-credentials".to_string(),
            "--".to_string(),
        ]
    }

    /// Get a human-readable representation of the rootless networking flow
    pub fn rootless_flow_string(&self) -> String {
        "holder(unshare --user --net) + nsenter for setup/firecracker".to_string()
    }

    /// Detect host's global IPv6 address for pasta outbound traffic.
    ///
    /// Memoised for the process lifetime: `setup()` and `start_pasta()` both need
    /// it, so without this every VM launch pays two `ip -6 addr show` execs for
    /// an answer that cannot change during one launch.
    fn detect_host_ipv6() -> Option<String> {
        static HOST_IPV6: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        HOST_IPV6.get_or_init(Self::probe_host_ipv6).clone()
    }

    fn probe_host_ipv6() -> Option<String> {
        let output = std::process::Command::new("ip")
            .args(["-6", "addr", "show", "scope", "global"])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if line.starts_with("inet6 ") {
                if let Some(addr_part) = line.strip_prefix("inet6 ") {
                    if let Some(addr) = addr_part.split('/').next() {
                        // Skip link-local (fe80::) and ULA (fd00::)
                        if !addr.starts_with("fe80:") && !addr.starts_with("fd") {
                            return Some(addr.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    /// Detect HTTP proxy from host environment
    ///
    /// On IPv6-only hosts, traffic must go through a proxy.
    /// Returns the proxy URL with IPv6 address resolved from hostname.
    fn detect_http_proxy() -> Option<String> {
        let proxy_url = std::env::var("HTTP_PROXY")
            .or_else(|_| std::env::var("http_proxy"))
            .or_else(|_| std::env::var("HTTPS_PROXY"))
            .or_else(|_| std::env::var("https_proxy"))
            .ok()?;

        if let Some(rest) = proxy_url.strip_prefix("http://") {
            let host_port = rest.trim_end_matches('/');

            if host_port.starts_with('[') {
                return Some(proxy_url);
            }

            if let Some((host, port)) = host_port.rsplit_once(':') {
                if let Ok(output) = std::process::Command::new("getent")
                    .args(["hosts", host])
                    .output()
                {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if let Some(ipv6) = stdout.split_whitespace().next() {
                        if ipv6.contains(':') {
                            return Some(format!("http://[{}]:{}", ipv6, port));
                        }
                    }
                }
                return Some(proxy_url);
            }
        }

        Some(proxy_url)
    }

    /// Start pasta process attached to the namespace
    ///
    /// pasta creates its own TAP device (pasta0) in the namespace and provides
    /// L2↔L4 translation to the host. Uses PID file for readiness signaling.
    pub async fn start_pasta(&mut self, namespace_pid: u32) -> Result<()> {
        let pid_file = paths::data_dir().join(format!("pasta-{}.pid", truncate_id(&self.vm_id, 8)));

        // Register the inotify watch BEFORE the stale-file cleanup and spawn:
        // pasta's PID-file write after this point always produces an event, so
        // readiness can never be missed (zero-race: watch → check → wait).
        // Init failure (e.g. fs.inotify.max_user_instances exhausted by many
        // concurrent clone starts as one user) must not fail start_pasta — the
        // old poll never had that failure mode. Degrade to the 250ms safety
        // tick below, same as the vm.rs API-socket watch.
        let mut pid_file_watch = match crate::utils::DirWatch::new(&paths::data_dir()) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    error = %e,
                    "inotify unavailable for pasta PID-file wait; degrading to 250ms polling"
                );
                None
            }
        };

        if pid_file.exists() {
            tokio::fs::remove_file(&pid_file).await?;
        }

        let host_ipv6 = Self::detect_host_ipv6();

        info!(
            namespace_pid = namespace_pid,
            pasta_tap = %self.pasta_device,
            pid_file = %pid_file.display(),
            host_ipv6 = ?host_ipv6,
            port_mappings = self.port_mappings.len(),
            "starting pasta for rootless networking"
        );

        // Resolve the pasta binary through the pinned-build machinery: with a
        // [pasta] config section the content-addressed patched build is
        // required (a distro pasta would reintroduce the addr_seen inbound
        // poisoning, #661); without one, PATH is used as before.
        let (config, _, _) =
            crate::setup::rootfs::load_config(None).context("loading config for pasta")?;
        let pasta_bin = crate::setup::get_pasta_for_config(config.pasta.as_ref())?;
        info!(pasta_bin = %pasta_bin.display(), "resolved pasta binary");

        let mut cmd = Command::new(&pasta_bin);
        cmd.arg("--foreground")
            .arg("--quiet")
            .arg("-P")
            .arg(&pid_file);

        // When running as root (e.g., sudo in tests), pasta drops to nobody by
        // default and then can't access the user namespace. Tell it to stay as root.
        if nix::unistd::geteuid().is_root() {
            cmd.arg("--runas").arg("0:0");
        }

        // Don't use --config-net: it sets an IP on pasta0's kernel interface, which
        // conflicts with the bridge (kernel responds to ARP for that IP via bridge's
        // weak host model, stealing traffic from pasta's userspace L2 handler).
        // Instead, pasta creates the TAP but we bring it up in build_bridge_script().
        //
        // -a must be the VM's actual IP (GUEST_IP), not the gateway. pasta uses -a
        // as the "guest address" and ignores ARP requests for it (don't resolve self).
        // If -a == gateway, pasta ignores ARP for the gateway and the VM can't route.
        cmd.arg("--ns-ifname")
            .arg(&self.pasta_device)
            .arg("-a")
            .arg(GUEST_IP) // VM's actual IP — pasta ignores ARP for this address
            .arg("-n")
            .arg("255.255.255.0")
            .arg("-g")
            .arg(GUEST_GATEWAY) // Gateway — pasta responds to ARP for this
            .arg("--no-dhcp");

        // If host has global IPv6, configure pasta for IPv6 outbound
        if let Some(ref ipv6) = host_ipv6 {
            // Add IPv6 guest address and gateway so pasta handles IPv6 L2↔L4 translation.
            // -a/-g can each be specified twice (once IPv4, once IPv6).
            cmd.arg("-a")
                .arg(GUEST_IPV6) // Guest IPv6 address — pasta ignores NDP for this
                .arg("-g")
                .arg(GUEST_IPV6_GATEWAY) // IPv6 gateway — pasta responds to NDP for this
                .arg("-o")
                .arg(ipv6); // Outbound source address for IPv6

            // Keep NDP enabled: the guest needs NDP Neighbor Solicitation/Advertisement
            // to resolve the IPv6 gateway's MAC address (like ARP for IPv4).
            // Disable only RA (router advertisements) and DHCPv6 — we configure the
            // guest's IPv6 address statically via kernel cmdline, not SLAAC.
            cmd.arg("--no-ra").arg("--no-dhcpv6");
        } else {
            // No host IPv6 — disable IPv6 entirely
            cmd.arg("--ipv4-only")
                // NDP/RA/DHCPv6 are moot with --ipv4-only, but be explicit
                .arg("--no-ndp")
                .arg("--no-dhcpv6")
                .arg("--no-ra");
        }

        // Port forwarding: pasta binds on host, L2 frames go through bridge to VM
        if self.port_mappings.is_empty() {
            cmd.arg("-t").arg("none").arg("-u").arg("none");
        } else {
            let mut tcp_specs = Vec::new();
            let mut udp_specs = Vec::new();

            for mapping in &self.port_mappings {
                let bind_addr = match &mapping.host_ip {
                    Some(ip) => ip.as_str(),
                    None => self.loopback_ip.as_deref().unwrap_or("127.0.0.1"),
                };

                // pasta spec: "bind_addr/host_port:guest_port"
                let spec = format!("{}/{}:{}", bind_addr, mapping.host_port, mapping.guest_port);

                match mapping.proto {
                    Protocol::Tcp => tcp_specs.push(spec),
                    Protocol::Udp => udp_specs.push(spec),
                }

                info!(
                    proto = ?mapping.proto,
                    host = %format!("{}:{}", bind_addr, mapping.host_port),
                    guest = %format!("{}:{}", self.guest_ip, mapping.guest_port),
                    "adding port forward"
                );
            }

            if tcp_specs.is_empty() {
                cmd.arg("-t").arg("none");
            } else {
                for spec in &tcp_specs {
                    cmd.arg("-t").arg(spec);
                }
            }
            if udp_specs.is_empty() {
                cmd.arg("-u").arg("none");
            } else {
                for spec in &udp_specs {
                    cmd.arg("-u").arg(spec);
                }
            }
        }

        // Disable host→namespace port forwarding (reverse direction).
        // These don't affect outbound traffic — pasta's L2↔L4 translation handles
        // that independently. Matches Podman's invocation pattern.
        cmd.arg("-T").arg("none").arg("-U").arg("none");

        // Attach to the holder's namespace
        cmd.arg(namespace_pid.to_string());

        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        // Kill pasta if fcvm dies, including from SIGKILL — the same per-hop guarantee
        // `install_namespace_pre_exec` gives the VMM and `spawn_namespace_holder` gives the
        // holder, and the one hop that did not have it.
        //
        // This is hardening, not a fix for an observed leak: pasta joins the holder BY PID,
        // and passt terminates itself when the PID whose namespaces it joined exits, so
        // today pasta already dies whenever the holder does. Measured both ways on this
        // branch — with the pre_exec removed, pasta still exited on fcvm SIGKILL (via the
        // holder), and it still exited even with the holder's netns pinned open by an
        // external fd, confirming the trigger is passt's PID watch rather than namespace
        // teardown.
        //
        // Arming pdeathsig directly is still worth its ten lines. That chain runs through
        // TWO things fcvm does not own: the holder's own pdeathsig, and an undocumented
        // passt behaviour (`--no-netns-quit` documents only the filesystem-bound case) in a
        // binary this repo pins to a patched fork and periodically rebases. AGENTS.md is
        // explicit that teardown is per-hop and that one unprotected hop orphans everything
        // below it; this makes pasta's death depend on nothing but the kernel.
        //
        // Must be the LAST pre_exec (there is only this one, but keep the invariant
        // explicit): a credential change zeroes `task->pdeath_signal`, so anything that
        // switches namespaces or identity first would silently drop it.
        //
        // pasta DOES change credentials after this exec — it setns()es into the holder's
        // user namespace (measured CapEff 0x1ffffffffff -> 0x2014c2) — and the signal
        // survives it. `commit_creds()` clears it only when uid/gid change or
        // `cred_cap_issubset(old, new)` FAILS; under sudo pasta's prior creds are full root
        // and `--runas 0:0` holds uid/gid at 0:0, so the new set is a strict SUBSET and that
        // branch is never taken — the signal survives because pasta LOSES privilege on entry.
        // pasta does not fork either: `--foreground` is single-threaded and its own -P
        // pidfile reports the PID armed here. Verified with the holder held ALIVE so passt's
        // PID watch cannot fire — armed pasta dies 2-54ms after fcvm is SIGKILLed, unarmed
        // pasta survives 5s.
        //
        // PRECONDITION: this holds only while fcvm runs as ROOT. Unprivileged, entering the
        // userns is a capability GAIN, `cred_cap_issubset` fails, the kernel zeroes the
        // signal, and pasta silently reverts to passt's 1-second PID watch with no symptom
        // until something leaks. Argued from the kernel rule, NOT measured — the harness
        // that verified the above was itself root.
        //
        // The `getppid` re-check closes the fork/exec window: a parent that dies after
        // fork() but before the prctl above would leave the signal unarmed and pasta
        // orphaned — the very outcome this is here to prevent. If we have already been
        // reparented, fail the exec so no pasta exists to leak.
        //
        // SAFETY: pre_exec runs between fork() and exec(). `prctl` and `getppid` are both
        // async-signal-safe, `fcvm_pid` is copied in, and NEITHER return path allocates:
        // both errors use the `Repr::Os` errno representation of `io::Error`, which stores
        // a bare i32. A message-carrying error (`io::Error::other`) would box its payload,
        // and malloc after fork(2) in a multi-threaded process can deadlock outright if
        // another thread held the allocator lock at fork — which would hang the child in
        // exactly the parent-death race the getppid check exists to handle. ESRCH ("no such
        // process") is the errno for that case and needs no message.
        let fcvm_pid = std::process::id() as libc::pid_t;
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != fcvm_pid {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }

        debug!(cmd = ?cmd, "pasta command");
        let stderr_tail = self.begin_stderr_attempt();
        let mut child = cmd.spawn().context("failed to spawn pasta")?;

        // Stream pasta's stderr: log every line and keep a tail so error paths
        // can show what pasta actually printed. Without this, pasta's output is
        // silently discarded and a dead pasta only surfaces later as an
        // unrelated bridge setup failure.
        if let Some(stderr) = child.stderr.take() {
            self.stderr_reader = Some(tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!(target: "pasta", "{}", line);
                    if let Ok(mut tail) = stderr_tail.lock() {
                        if tail.len() >= PASTA_STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                }
            }));
        }

        self.wait_for_pid_file(&mut child, &pid_file, &mut pid_file_watch)
            .await?;

        self.pasta_process = Some(child);
        self.pid_file = Some(pid_file);

        Ok(())
    }

    /// Start an attempt-local stderr capture.
    ///
    /// A failed attempt's pipe reader can outlive the child (for example, when a
    /// descendant inherited stderr). Merely clearing the shared deque leaves a
    /// race where that old reader appends after the next attempt starts. Give
    /// every attempt a distinct tail and cancel any reader we no longer own so
    /// stale output is structurally unable to enter the current diagnostic.
    fn begin_stderr_attempt(&mut self) -> Arc<Mutex<VecDeque<String>>> {
        if let Some(previous_reader) = self.stderr_reader.take() {
            previous_reader.abort();
        }

        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        self.stderr_tail = Arc::clone(&stderr_tail);
        stderr_tail
    }

    /// Wait until pasta publishes its readiness PID file while supervising the
    /// child and retaining a bounded polling fallback.
    ///
    /// The event source is a narrow injection boundary for deterministic tests;
    /// production passes the inotify watch registered before pasta was spawned.
    async fn wait_for_pid_file<E>(
        &mut self,
        child: &mut Child,
        pid_file: &std::path::Path,
        pid_file_events: &mut E,
    ) -> Result<()>
    where
        E: crate::utils::DirEventSource,
    {
        // Event-driven: the inotify watch registered before spawn wakes us the
        // instant the file lands (the old 50ms poll noticed a +2.3ms file at
        // +52.8ms on every rootless clone). pasta death interrupts the wait via
        // child.wait(); the deadline still bounds everything, and a coarse
        // safety tick re-checks the file so even a lost/overflowed inotify
        // event can only add latency, never hang.
        let deadline = tokio::time::Instant::now() + PASTA_READY_TIMEOUT;
        loop {
            if pid_file.exists() {
                info!("pasta ready (PID file created)");
                return Ok(());
            }

            tokio::select! {
                status = child.wait() => {
                    // pasta died during startup. Wait for the stderr reader to hit
                    // EOF — pasta's exit closed the write end, so this returns as
                    // soon as the pipe is drained — and the error names the cause.
                    crate::utils::wait_for_stderr_eof(
                        &mut self.stderr_reader,
                        PASTA_STDERR_EOF_TIMEOUT,
                    )
                    .await;
                    match status {
                        Ok(status) => anyhow::bail!(
                            "pasta exited before becoming ready (status: {}){}",
                            status,
                            self.stderr_tail_message()
                        ),
                        Err(e) => anyhow::bail!("failed to check pasta status: {}", e),
                    }
                }
                event = pid_file_events.next_event() => {
                    // Filesystem activity in the data dir — loop re-checks the
                    // PID file. A watch error degrades to the safety tick.
                    if event.is_err() {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.kill().await;
                    // kill() reaped the child and closed its pipe; wait for the
                    // reader to drain it before rendering the diagnostic tail.
                    crate::utils::wait_for_stderr_eof(
                        &mut self.stderr_reader,
                        PASTA_STDERR_EOF_TIMEOUT,
                    )
                    .await;
                    anyhow::bail!(
                        "pasta did not become ready within {:?}{}",
                        PASTA_READY_TIMEOUT,
                        self.stderr_tail_message()
                    );
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                    // Safety tick: re-check the condition even without events.
                }
            }
        }
    }

    /// Render the captured pasta stderr tail for error messages.
    fn stderr_tail_message(&self) -> String {
        let lines: Vec<String> = self
            .stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default();
        if lines.is_empty() {
            "; no stderr output captured from pasta".to_string()
        } else {
            format!("; last pasta stderr output:\n  {}", lines.join("\n  "))
        }
    }

    /// Wait for pasta's TAP device to appear in the namespace, supervising pasta itself.
    ///
    /// pasta writes its PID file before the device is visible in the namespace,
    /// and under load that window can stretch out — or pasta can die right after
    /// startup, in which case the device never appears at all. Polling here
    /// (instead of inside the bridge script) lets every iteration also check the
    /// pasta child, so a dead pasta fails fast with its own exit status and
    /// stderr instead of a generic "Cannot find device" from the bridge setup.
    async fn wait_for_pasta_device(&mut self, holder_pid: u32) -> Result<()> {
        let deadline = std::time::Instant::now() + PASTA_DEVICE_TIMEOUT;
        let nsenter_prefix = self.build_nsenter_prefix(holder_pid);

        // Fine-grained backoff, not a fixed 100ms: pasta creates the TAP within
        // a few ms of writing its PID file, and now that the PID file is
        // noticed event-driven (+2ms instead of +52ms) this probe regularly
        // arrives BEFORE the device exists — a fixed 100ms retry put a +100ms
        // mode on 4/10 benched clones. Each probe is a full nsenter+ip exec
        // (~1-3ms), so short gaps just keep probing continuously through the
        // handful of ms until the device appears; the deadline still bounds it.
        let mut retry_delay = std::time::Duration::from_millis(1);

        loop {
            let output = Command::new(&nsenter_prefix[0])
                .args(&nsenter_prefix[1..])
                .args(["ip", "link", "show", &self.pasta_device])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await
                .context("checking for pasta TAP device via nsenter")?;

            if output.status.success() {
                debug!(device = %self.pasta_device, "pasta TAP device present in namespace");
                return Ok(());
            }

            // Device not visible yet — if pasta has exited, attribute the failure
            // to pasta instead of letting the bridge setup fail later.
            let pasta_exit = match self.pasta_process.as_mut() {
                Some(process) => process
                    .try_wait()
                    .context("checking pasta process status")?,
                None => None,
            };
            if let Some(status) = pasta_exit {
                self.pasta_process = None;
                // Wait for the stderr reader to hit EOF so the error includes what
                // pasta actually printed before dying.
                crate::utils::wait_for_stderr_eof(
                    &mut self.stderr_reader,
                    PASTA_STDERR_EOF_TIMEOUT,
                )
                .await;
                anyhow::bail!(
                    "pasta exited (status: {}) before its TAP device {} appeared in the namespace{}",
                    status,
                    self.pasta_device,
                    self.stderr_tail_message()
                );
            }

            if std::time::Instant::now() > deadline {
                anyhow::bail!(
                    "pasta is still running but its TAP device {} did not appear in the namespace within {:?}{}",
                    self.pasta_device,
                    PASTA_DEVICE_TIMEOUT,
                    self.stderr_tail_message()
                );
            }

            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(std::time::Duration::from_millis(50));
        }
    }

    /// Get guest IP address for kernel boot args
    pub fn guest_ip(&self) -> &str {
        &self.guest_ip
    }

    /// Get gateway IP for guest (pasta gateway)
    pub fn gateway_ip(&self) -> &str {
        GUEST_GATEWAY
    }

    /// Wait for pasta to bind each mapped host port.
    ///
    /// Pasta binds ports asynchronously after startup. The PID file just means
    /// the process is running, not that ports are listening. Without this check,
    /// the health monitor may declare the VM "healthy" (via nsenter/bridge) before
    /// pasta is even listening.
    ///
    /// This is a host-side check only, and it CANNOT tell you the guest is
    /// reachable. pasta is a userspace stack: its listener completes the TCP
    /// handshake itself, before and independently of any L2 forwarding to the
    /// guest, so this connect succeeds against a guest that is silent or absent.
    /// In the run that motivated [`wait_for_guest_to_answer`] it reported "port
    /// forward ready" 95 MICROseconds after the readiness log line, on clones
    /// whose guest was not answering.
    ///
    /// It is kept as-is rather than made end-to-end because fcvm does not know
    /// what protocol a published port speaks — 9222 happens to be CDP, but a
    /// mapping is just a number — so there are no bytes it could send that would
    /// constitute a valid request. Guest liveness is established before this
    /// runs, by the guest's TCP answer in [`wait_for_guest_to_answer`].
    async fn wait_for_port_forwarding_until(&self, deadline: tokio::time::Instant) -> Result<()> {
        use tokio::net::TcpStream;

        let readiness_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
        let loopback = self.loopback_ip.as_deref().unwrap_or("127.0.0.1");

        for mapping in &self.port_mappings {
            if mapping.proto != Protocol::Tcp {
                continue;
            }

            let bind_addr = match &mapping.host_ip {
                Some(ip) => ip.as_str(),
                None => loopback,
            };
            let addr = format!("{}:{}", bind_addr, mapping.host_port);

            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(port_forward_deadline_error(&addr, readiness_budget));
                }
                match tokio::time::timeout(remaining, TcpStream::connect(&addr)).await {
                    Ok(Ok(_)) => {
                        debug!(addr = %addr, "port forward ready");
                        break;
                    }
                    Ok(Err(_)) => {
                        let remaining =
                            deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            return Err(port_forward_deadline_error(&addr, readiness_budget));
                        }
                        tokio::time::sleep(remaining.min(std::time::Duration::from_millis(50)))
                            .await;
                    }
                    Err(_) => return Err(port_forward_deadline_error(&addr, readiness_budget)),
                }
            }
        }

        Ok(())
    }

    async fn wait_for_port_forwarding(&self) -> Result<()> {
        self.wait_for_port_forwarding_until(tokio::time::Instant::now() + GUEST_ANSWER_DEADLINE)
            .await
    }
}

fn port_forward_deadline_error(addr: &str, budget: std::time::Duration) -> anyhow::Error {
    anyhow::anyhow!(
        "pasta port forward not ready within caller readiness budget {budget:?}: {addr}"
    )
}

#[async_trait::async_trait]
impl NetworkManager for PastaNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up rootless networking with pasta (bridge mode)");

        info!(
            guest_ip = %self.guest_ip,
            gateway = %GUEST_GATEWAY,
            loopback_ip = ?self.loopback_ip,
            "network configuration (pasta bridge mode, nsenter health checks)"
        );

        let guest_mac = generate_mac();

        // Check if host has IPv6 — pasta handles it natively
        let (guest_ipv6, host_ipv6) = if Self::detect_host_ipv6().is_some() {
            (
                Some(self.guest_ipv6.clone()),
                Some(GUEST_IPV6_GATEWAY.to_string()),
            )
        } else {
            (None, None)
        };

        let http_proxy = Self::detect_http_proxy();
        if let Some(ref proxy) = http_proxy {
            info!(proxy = %proxy, "detected HTTP proxy for IPv6-only network");
        }

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(format!("{}/24", self.guest_ip)),
            host_ip: Some(GUEST_GATEWAY.to_string()),
            host_veth: None,
            loopback_ip: self.loopback_ip.clone(),
            // Don't use pasta's DNS forwarder (10.0.2.3) — it's unreachable from the VM
            // through the bridge. Instead, pass host DNS servers directly; the guest
            // reaches them via pasta's L4 translation (same path as all other traffic).
            dns_server: None,
            guest_ipv6,
            host_ipv6,
            dns_search: None,
            http_proxy,
            namespace_name: None,
        })
    }

    async fn post_start(&mut self, holder_pid: u32) -> Result<()> {
        self.holder_pid = Some(holder_pid);

        info!(
            holder_pid = holder_pid,
            "starting pasta for rootless networking"
        );

        // Phases 1+2: start pasta and wait for its TAP device, with a bounded
        // retry on a transient startup failure.
        //
        // pasta has a netlink startup race: it subscribes to route/neighbour
        // notifications and then issues request/response netlink calls during
        // setup; a notification (sequence 0) arriving mid-sequence makes
        // nl_status() die() with "netlink: Unexpected sequence number". Upstream
        // d00255bd fixed the neighbour-sync path, but the race still recurs under
        // heavy parallelism (many pastas starting while veth/tap/bridge churn
        // generates netlink traffic), so pasta exits before its TAP appears.
        // It is transient — a fresh start almost always succeeds — so retry a few
        // times rather than failing the whole VM. (The remaining race is reported
        // upstream; this keeps us resilient until it lands and the pin is bumped.)
        const PASTA_START_ATTEMPTS: u32 = 4;
        let mut last_err = None;
        for attempt in 1..=PASTA_START_ATTEMPTS {
            let result = match self.start_pasta(holder_pid).await {
                Ok(()) => self.wait_for_pasta_device(holder_pid).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    warn!(
                        attempt,
                        max = PASTA_START_ATTEMPTS,
                        error = %e,
                        "pasta startup failed (likely the transient netlink race), retrying"
                    );
                    // Reap the dead pasta (if start_pasta got far enough to store it)
                    // so the next attempt starts clean; start_pasta also removes a
                    // stale PID file at its start.
                    if let Some(mut p) = self.pasta_process.take() {
                        let _ = p.kill().await;
                    }
                    last_err = Some(e);
                    if attempt < PASTA_START_ATTEMPTS {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e.context(format!(
                "pasta failed to start after {PASTA_START_ATTEMPTS} attempts"
            )));
        }

        // Phase 3: Create bridge connecting pasta0 and Firecracker's TAP
        let bridge_script = self.build_bridge_script();
        let nsenter_prefix = self.build_nsenter_prefix(holder_pid);

        debug!(
            holder_pid = holder_pid,
            script = %bridge_script.summary(),
            "running bridge setup script"
        );

        let output = Command::new(&nsenter_prefix[0])
            .args(&nsenter_prefix[1..])
            .arg("bash")
            .arg("-c")
            .arg(bridge_script.script())
            .output()
            .await
            .context("running bridge setup via nsenter")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "bridge setup failed: {}",
                bridge_script.describe_failure(&stderr)
            );
        }

        // Phase 4: Verify port forwarding is actually working
        // The PID file only means pasta spawned, not that ports are bound.
        // Health checks use nsenter (bridge path), so without this check
        // "healthy" doesn't mean port forwarding works.
        //
        // Skip in restore mode: during snapshot restore, post_start() runs BEFORE
        // the VM snapshot is loaded. Probing ports now forces pasta to attempt L2
        // forwarding to a non-existent guest, poisoning its connection state and
        // causing subsequent connections to return 0 bytes. The port check happens
        // later via verify_port_forwarding() after the VM is actually running.
        if !self.restore_mode && !self.port_mappings.is_empty() {
            self.wait_for_port_forwarding().await?;
        }

        info!(holder_pid = holder_pid, "pasta + bridge setup complete");
        Ok(())
    }

    fn start_kill_processes(&mut self) {
        if let Some(process) = self.pasta_process.as_mut() {
            if let Err(e) = process.start_kill() {
                warn!(vm_id = %self.vm_id, error = %e, "failed to signal pasta");
            }
        }
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up pasta resources");

        // `kill()` is start_kill + wait, so this stays correct whether or not
        // `start_kill_processes` already signalled pasta (re-signalling a not-yet-reaped
        // process is a no-op); when it did, the wait below has nothing left to wait for.
        if let Some(mut process) = self.pasta_process.take() {
            if let Err(e) = process.kill().await {
                warn!("failed to kill pasta: {}", e);
            }
        }

        if let Some(ref pid_file) = self.pid_file {
            if pid_file.exists() {
                if let Err(e) = tokio::fs::remove_file(pid_file).await {
                    warn!("failed to remove pasta PID file: {}", e);
                }
            }
        }

        info!(vm_id = %self.vm_id, "pasta cleanup complete");
        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    /// Verify the guest is reachable after snapshot restore, then that its
    /// published ports accept.
    ///
    /// After snapshot restore, pasta needs the guest's MAC address to forward L2
    /// frames. The TCP probe serves two purposes: it triggers the ARP exchange
    /// that populates the neighbour table (with arp_accept=0, the Linux default,
    /// the guest's gratuitous arping only updates existing entries, so the
    /// outbound packet is what creates a resolved one), and its answer — SYN-ACK
    /// or RST — is the only signal here that the guest itself is alive.
    ///
    /// Both halves are required. See [`wait_for_guest_to_answer`] for why the
    /// neighbour entry alone declared silent guests ready.
    async fn verify_port_forwarding(&self) -> Result<()> {
        if self.port_mappings.is_empty() {
            return Ok(());
        }

        let holder_pid = match self.holder_pid {
            Some(pid) => pid,
            None => {
                anyhow::bail!("cannot verify pasta port forwarding without a namespace holder PID")
            }
        };

        // Probe the first published TCP port: it is the one address the operator
        // has declared traffic will arrive on, so the guest cannot legitimately
        // be silent there — an RST counts (see GuestProbe::answers_tcp).
        let probe_port = match self
            .port_mappings
            .iter()
            .find(|m| m.proto == crate::network::Protocol::Tcp)
        {
            Some(mapping) => mapping.guest_port,
            // UDP-only mappings: nothing the guest is obliged to answer on, so
            // fall back to the host-side pasta checks alone, as before.
            None => {
                return self
                    .wait_for_port_forwarding_until(
                        tokio::time::Instant::now() + GUEST_ANSWER_DEADLINE,
                    )
                    .await
            }
        };

        let deadline = tokio::time::Instant::now() + GUEST_ANSWER_DEADLINE;
        let mut probe = NsenterGuestProbe::new(self.build_nsenter_prefix(holder_pid));

        if let Err(error) = wait_for_guest_to_answer(&mut probe, probe_port, deadline).await {
            // The namespace is about to be torn down, taking every piece of
            // evidence with it. A once-in-hundreds failure that destroys its
            // own diagnosis costs an entire benchmark campaign and teaches
            // nothing (observed 2026-08-15), so dump the L2/L3/L4 state that
            // says WHERE the packets stopped before returning the error.
            self.dump_unreachable_guest_forensics(holder_pid, probe_port)
                .await;
            return Err(error);
        }
        self.wait_for_port_forwarding_until(deadline).await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End to end: the readiness ERROR must carry the probe's exit status.
    ///
    /// The pure describer tests above cannot see whether answers_tcp actually
    /// uses it, and the failure a reader gets is the error string, not the
    /// TcpAnswer. This drives the real readiness loop with a guest that is
    /// silent behind a resolved neighbour — the captured shape — and requires
    /// the exit status to survive into the message. Before this change the
    /// same path produced "probe: (silence)".
    #[tokio::test(start_paused = true)]
    async fn the_readiness_error_carries_the_probe_exit_status() {
        let mut guest = ScriptedGuest::silent_behind_resolved_neighbor();
        let error = wait_for_guest_to_answer(&mut guest, PROBE_PORT, paused_deadline())
            .await
            .expect_err("a silent guest must fail readiness");
        let text = format!("{error:#}");
        assert!(text.contains("exit=124"), "{text}");
        assert!(text.contains("no SYN-ACK or RST"), "{text}");
        assert!(
            !text.contains("(silence)"),
            "the blank detail is gone: {text}"
        );
    }

    /// A silent probe must name its exit status, because `timeout` kills the
    /// probing bash mid-connect and stderr comes back EMPTY.
    ///
    /// RED before this change: the detail was the raw stderr, so a guest that
    /// never answered and a prober that printed nothing both produced `""`,
    /// and the readiness error said `probe: (silence)` either way. That is the
    /// distinction the whole diagnosis turns on.
    #[test]
    fn a_silent_probe_reports_its_exit_status_not_an_empty_string() {
        let detail = describe_silent_probe(Some(124), "");
        assert!(detail.contains("exit=124"), "{detail}");
        assert!(detail.contains("no SYN-ACK or RST"), "{detail}");
        assert!(!detail.is_empty());
    }

    /// Exit 124 says only what was OBSERVED. A dropped SYN, a guest firewall
    /// DROP and a lost reply are all consistent with it, so the wording must
    /// not assert the guest never received the packet (review finding).
    #[test]
    fn the_timeout_wording_claims_only_what_was_observed() {
        let detail = describe_silent_probe(Some(124), "");
        assert!(
            !detail.contains("never replied") && !detail.contains("never received"),
            "must not infer where the packet was lost: {detail}"
        );
    }

    /// A prober that failed for its own reasons is not the guest's silence.
    #[test]
    fn a_prober_error_is_distinguishable_from_guest_silence() {
        let detail = describe_silent_probe(Some(127), "bash: line 1: nsenter: not found");
        assert!(detail.contains("exit=127"), "{detail}");
        assert!(detail.contains("probe error"), "{detail}");
        assert!(detail.contains("nsenter: not found"), "{detail}");
    }

    /// A probe killed by a signal has no exit code; it still must not be blank.
    #[test]
    fn a_signalled_probe_still_reports_something() {
        let detail = describe_silent_probe(None, "");
        assert!(detail.contains("signal"), "{detail}");
    }

    use crate::utils::{DirEventSource, ProcessWatch};
    use std::future::{poll_fn, Future};
    use std::pin::Pin;
    use std::task::Poll;

    enum EventStep {
        Wake,
        ReadError,
    }

    struct PublishingEvents {
        pid_file: PathBuf,
        steps: VecDeque<EventStep>,
        calls: usize,
    }

    impl PublishingEvents {
        fn new(pid_file: PathBuf, steps: impl IntoIterator<Item = EventStep>) -> Self {
            Self {
                pid_file,
                steps: steps.into_iter().collect(),
                calls: 0,
            }
        }
    }

    impl DirEventSource for PublishingEvents {
        fn is_available(&self) -> bool {
            true
        }

        async fn next_event(&mut self) -> anyhow::Result<()> {
            self.calls += 1;
            let step = self.steps.pop_front().expect("event script exhausted");
            let pid_file = self.pid_file.clone();
            std::fs::write(pid_file, b"123\n").expect("publish pasta PID file");
            match step {
                EventStep::Wake => Ok(()),
                EventStep::ReadError => Err(anyhow::anyhow!("injected inotify read failure")),
            }
        }
    }

    fn live_child() -> Child {
        // Must stay alive with NO stdin dependency: tokio's Child::wait()
        // drops the child's stdin handle on its first poll (deadlock
        // avoidance), so a `cat` with piped stdin exits 0 the moment the
        // select polls the wait arm — and whether that real OS exit beats the
        // paused clock's auto-advance to the safety tick is a scheduler race
        // (observed as a TRY 1 FAIL of the safety-tick test under a loaded
        // full-suite run, 2026-08-13).
        let mut command = Command::new("sleep");
        command
            .arg("3600")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command.spawn().expect("spawn live child")
    }

    async fn assert_pending_once<F>(mut future: Pin<&mut F>)
    where
        F: Future,
    {
        poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("future completed before its required signal"),
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn pid_file_event_queued_after_check_wakes_without_clock_fallback() {
        let dir = tempfile::tempdir().expect("create PID-file directory");
        let pid_file = dir.path().join("pasta.pid");
        let mut events = PublishingEvents::new(pid_file.clone(), [EventStep::Wake]);
        let mut child = live_child();
        let mut net = PastaNetwork::new("wait-test".to_string(), "tap0".to_string(), vec![]);
        let before = tokio::time::Instant::now();

        net.wait_for_pid_file(&mut child, &pid_file, &mut events)
            .await
            .expect("queued PID-file event should trigger a condition recheck");

        assert_eq!(events.calls, 1);
        assert_eq!(tokio::time::Instant::now(), before, "safety tick fired");
        child.kill().await.expect("stop live child");
    }

    #[tokio::test(start_paused = true)]
    async fn pid_file_unavailable_watch_uses_safety_tick_fallback() {
        let dir = tempfile::tempdir().expect("create PID-file directory");
        let pid_file = dir.path().join("pasta.pid");
        let publish_path = pid_file.clone();
        let mut no_watch = None::<crate::utils::DirWatch>;
        let mut child = live_child();
        let mut net = PastaNetwork::new("wait-test".to_string(), "tap0".to_string(), vec![]);
        let before = tokio::time::Instant::now();

        let wait = net.wait_for_pid_file(&mut child, &pid_file, &mut no_watch);
        let publish = async move {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            std::fs::write(publish_path, b"123\n").expect("publish pasta PID file");
        };
        let (result, ()) = tokio::join!(wait, publish);
        result.expect("safety tick should recheck without inotify");

        assert_eq!(
            tokio::time::Instant::now() - before,
            std::time::Duration::from_millis(250)
        );
        child.kill().await.expect("stop live child");
    }

    #[tokio::test(start_paused = true)]
    async fn pid_file_inotify_read_error_uses_poll_fallback() {
        let dir = tempfile::tempdir().expect("create PID-file directory");
        let pid_file = dir.path().join("pasta.pid");
        let mut events = PublishingEvents::new(pid_file.clone(), [EventStep::ReadError]);
        let mut child = live_child();
        let mut net = PastaNetwork::new("wait-test".to_string(), "tap0".to_string(), vec![]);
        let before = tokio::time::Instant::now();

        net.wait_for_pid_file(&mut child, &pid_file, &mut events)
            .await
            .expect("watch read failure should degrade to polling");

        assert_eq!(events.calls, 1);
        assert_eq!(
            tokio::time::Instant::now() - before,
            std::time::Duration::from_millis(50)
        );
        child.kill().await.expect("stop live child");
    }

    #[tokio::test(start_paused = true)]
    async fn pid_file_child_exit_waits_for_complete_stderr() {
        let dir = tempfile::tempdir().expect("create PID-file directory");
        let pid_file = dir.path().join("pasta.pid");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 17")
            .spawn()
            .expect("spawn exited child");
        let status = child.wait().await.expect("reap exited child");
        assert_eq!(status.code(), Some(17));

        let mut net = PastaNetwork::new("wait-test".to_string(), "tap0".to_string(), vec![]);
        let (reader_started_tx, reader_started_rx) = tokio::sync::oneshot::channel();
        let (release_reader_tx, release_reader_rx) = tokio::sync::oneshot::channel();
        let stderr_tail = Arc::clone(&net.stderr_tail);
        net.stderr_reader = Some(tokio::spawn(async move {
            let _ = reader_started_tx.send(());
            let _ = release_reader_rx.await;
            stderr_tail
                .lock()
                .expect("stderr tail lock")
                .push_back("final pasta stderr".to_string());
        }));
        reader_started_rx.await.expect("stderr reader parked");

        let mut no_watch = None::<crate::utils::DirWatch>;
        let mut wait = Box::pin(net.wait_for_pid_file(&mut child, &pid_file, &mut no_watch));
        assert_pending_once(wait.as_mut()).await;
        release_reader_tx.send(()).expect("release stderr reader");
        let err = wait.await.expect_err("exited pasta cannot become ready");
        let message = format!("{err:#}");

        assert!(message.contains("status: exit status: 17"), "{message}");
        assert!(message.contains("final pasta stderr"), "{message}");
    }

    #[tokio::test(start_paused = true)]
    async fn pid_file_timeout_waits_for_complete_stderr() {
        let dir = tempfile::tempdir().expect("create PID-file directory");
        let pid_file = dir.path().join("pasta.pid");
        let mut no_watch = None::<crate::utils::DirWatch>;
        let mut child = live_child();
        let child_pid = child.id().expect("live child PID");
        let mut child_exit = ProcessWatch::open(child_pid)
            .expect("open child pidfd")
            .expect("child remains alive");

        let mut net = PastaNetwork::new("wait-test".to_string(), "tap0".to_string(), vec![]);
        let (reader_started_tx, reader_started_rx) = tokio::sync::oneshot::channel();
        let (release_reader_tx, release_reader_rx) = tokio::sync::oneshot::channel();
        let stderr_tail = Arc::clone(&net.stderr_tail);
        net.stderr_reader = Some(tokio::spawn(async move {
            let _ = reader_started_tx.send(());
            let _ = release_reader_rx.await;
            stderr_tail
                .lock()
                .expect("stderr tail lock")
                .push_back("stderr written immediately before timeout kill".to_string());
        }));
        reader_started_rx.await.expect("stderr reader parked");

        let mut wait = Box::pin(net.wait_for_pid_file(&mut child, &pid_file, &mut no_watch));
        assert_pending_once(wait.as_mut()).await;
        tokio::time::advance(PASTA_READY_TIMEOUT).await;
        // Drive the deadline arm far enough to signal the child, then use the
        // pidfd edge instead of sleeping/retrying for process exit.
        assert_pending_once(wait.as_mut()).await;
        child_exit.exited().await;

        // Even after kill/reap completes, the diagnostic must remain blocked on
        // the stderr EOF reader. This assertion is red if the timeout branch
        // formats the tail immediately after `child.kill()`.
        assert_pending_once(wait.as_mut()).await;
        release_reader_tx.send(()).expect("release stderr reader");
        let err = wait.await.expect_err("missing PID file must time out");
        let message = format!("{err:#}");

        assert!(message.contains("did not become ready"), "{message}");
        assert!(
            message.contains("stderr written immediately before timeout kill"),
            "{message}"
        );
    }

    /// Every bridge the guest reaches through NAMESPACE_IP must carry
    /// NAMESPACE_MAC.
    ///
    /// The guest pins ONE permanent neighbour entry for 10.0.2.1 and cannot
    /// tell which networking mode it booted under. Rootless mode reaches that
    /// address on pasta's bridge; ROUTED mode uses the very same address as the
    /// guest's DEFAULT GATEWAY on a bridge of its own. Pinning the MAC in only
    /// one of them sends every reply in the other to an address nothing owns --
    /// which is not a degraded published port there, it is all IPv4 egress.
    /// That regression shipped once and CI caught it as eight routed-mode
    /// failures across both architectures.
    #[test]
    fn every_bridge_on_the_namespace_address_pins_the_same_mac() {
        let routed = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/network/routed.rs"),
        )
        .expect("reading src/network/routed.rs");

        assert!(
            routed.contains("NAMESPACE_MAC"),
            "routed mode does not pin its bridge to NAMESPACE_MAC. Its guests \
             reach {} as their default gateway and hold a permanent neighbour \
             entry for {}; an unpinned bridge MAC breaks all of their IPv4.",
            NAMESPACE_IP,
            NAMESPACE_MAC
        );
        assert!(
            routed.contains("\"address\","),
            "routed mode references NAMESPACE_MAC but never sets a link address \
             with it"
        );
        assert!(
            routed.contains(&format!("GUEST_GATEWAY: &str = \"{}\"", NAMESPACE_IP)),
            "routed mode's gateway is no longer {}; re-check whether the guest's \
             pinned neighbour entry still names the right address",
            NAMESPACE_IP
        );
    }

    /// fc-agent hardcodes the same pair (it has no boot-plan field for them),
    /// so a change here that is not mirrored there silently reopens the ARP
    /// race that made published ports go silent. Read the guest's copy rather
    /// than trusting a comment.
    #[test]
    fn namespace_neighbour_matches_host_constants() {
        let agent = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fc-agent/src/network.rs"),
        )
        .expect("reading fc-agent/src/network.rs");

        for (label, value) in [
            ("NAMESPACE_IP", NAMESPACE_IP),
            ("NAMESPACE_MAC", NAMESPACE_MAC),
        ] {
            let needle = format!("const {}: &str = \"{}\";", label, value);
            assert!(
                agent.contains(&needle),
                "fc-agent does not define {} as {:?}. The guest pins a permanent \
                 neighbour entry using its own copy of this constant; if the two \
                 disagree, the entry points at the wrong MAC and inbound \
                 connections are reset by pasta. Expected line: {}",
                label,
                value,
                needle
            );
        }

        assert!(
            agent.contains("nud"),
            "fc-agent no longer installs a permanent neighbour entry; a dynamic \
             one loses to pasta's ARP reply about 1 time in 10"
        );
    }

    #[test]
    fn pasta_retry_stderr_isolated_from_late_previous_attempt_output() {
        let mut net = PastaNetwork::new("wait-test".to_string(), "tap0".to_string(), vec![]);

        let previous_attempt = net.begin_stderr_attempt();
        previous_attempt
            .lock()
            .expect("previous stderr tail lock")
            .push_back("previous attempt before retry".to_string());

        let current_attempt = net.begin_stderr_attempt();
        previous_attempt
            .lock()
            .expect("previous stderr tail lock")
            .push_back("previous attempt after retry".to_string());
        current_attempt
            .lock()
            .expect("current stderr tail lock")
            .push_back("current attempt output".to_string());

        let message = net.stderr_tail_message();
        assert!(
            !Arc::ptr_eq(&previous_attempt, &current_attempt),
            "a retry must publish a fresh tail so a detached old reader cannot append to it"
        );
        assert!(message.contains("current attempt output"), "{message}");
        assert!(!message.contains("previous attempt"), "{message}");
    }

    #[test]
    fn test_network_creation() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![]);

        assert_eq!(net.tap_device, "tap0");
        assert_eq!(net.pasta_device, "pasta0");
        assert_eq!(net.guest_ip, "10.0.2.100");
        assert_eq!(net.gateway_ip(), "10.0.2.2");
    }

    /// A guest whose TCP answers are scripted.
    ///
    /// Each entry is one attempt; the last entry repeats for every attempt after
    /// it, so a guest can be made to answer late or never.
    struct ScriptedGuest {
        answers: VecDeque<bool>,
        last_answer: bool,
        neighbor: String,
        probes: usize,
    }

    impl ScriptedGuest {
        /// The shape of the 5 failing clones: `ip neigh` has the guest's MAC and
        /// reports REACHABLE, and the guest answers nothing. The neighbour line
        /// is verbatim from the captured failure log, `dev` field included by
        /// omission — `ip neigh show ... dev br0` does not repeat the device.
        fn silent_behind_resolved_neighbor() -> Self {
            Self {
                answers: VecDeque::new(),
                last_answer: false,
                neighbor: "10.0.2.100 lladdr 02:c4:f0:3b:67:bd REACHABLE".to_string(),
                probes: 0,
            }
        }

        /// A healthy clone: answers the first probe. 803 of 808 did.
        fn answering() -> Self {
            let mut guest = Self::silent_behind_resolved_neighbor();
            guest.last_answer = true;
            guest
        }

        /// A clone still coming up: silent for `attempts` probes, then answers.
        fn answering_after(attempts: usize) -> Self {
            let mut guest = Self::silent_behind_resolved_neighbor();
            guest.answers = std::iter::repeat_n(false, attempts).collect();
            guest.answers.push_back(true);
            guest
        }

        /// A guest that has not appeared at L2 at all.
        fn absent() -> Self {
            let mut guest = Self::silent_behind_resolved_neighbor();
            guest.neighbor = String::new();
            guest
        }
    }

    impl GuestProbe for ScriptedGuest {
        async fn answers_tcp(
            &mut self,
            _port: u16,
            _budget: std::time::Duration,
        ) -> Result<TcpAnswer> {
            self.probes += 1;
            if let Some(answered) = self.answers.pop_front() {
                self.last_answer = answered;
            }
            Ok(TcpAnswer {
                // The captured failure logged nothing on stderr: the probe simply
                // timed out waiting for an answer, it did not fail to run. The
                // real prober turns that empty stderr into an exit-status
                // description, so the scripted one uses the same function
                // rather than a hand-written string that could drift from it.
                answered: self.last_answer,
                detail: if self.last_answer {
                    String::new()
                } else {
                    describe_silent_probe(Some(124), "")
                },
            })
        }

        async fn neighbor(&mut self, _budget: std::time::Duration) -> Result<String> {
            Ok(self.neighbor.clone())
        }
    }

    /// A probe whose FIRST attempt never returns, and which ignores the budget
    /// it is handed. That is what the production probe did in run 31906708922,
    /// where an inner `timeout` failed to bound the call.
    struct StallingGuest {
        attempts: u32,
        stall: std::time::Duration,
    }

    impl GuestProbe for StallingGuest {
        async fn answers_tcp(
            &mut self,
            _port: u16,
            _budget: std::time::Duration,
        ) -> Result<TcpAnswer> {
            self.attempts += 1;
            if self.attempts == 1 {
                tokio::time::sleep(self.stall).await;
            }
            Ok(TcpAnswer {
                answered: true,
                detail: String::new(),
            })
        }

        async fn neighbor(&mut self, _budget: std::time::Duration) -> Result<String> {
            Ok("10.0.2.100 dev br0 lladdr 02:2c:77:3e:ae:5a REACHABLE".to_string())
        }
    }

    /// One stuck attempt must not consume the whole readiness budget.
    ///
    /// Without the per-attempt bound the loop awaits the first attempt for the
    /// entire deadline, so it completes ZERO rounds, never queries the
    /// neighbour, and returns the deadline error, while the guest was ready the
    /// whole time. That is the observed failure exactly: empty neighbour,
    /// "(silence)", and a namespace that simultaneously held a TIME-WAIT socket
    /// to the guest and a REACHABLE neighbour entry for it.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_probe_attempt_is_abandoned_and_retried() {
        let mut probe = StallingGuest {
            attempts: 0,
            // Outlives the deadline several times over: the loop cannot pass
            // this test by waiting the attempt out.
            stall: GUEST_ANSWER_DEADLINE * 6,
        };
        let deadline = tokio::time::Instant::now() + GUEST_ANSWER_DEADLINE;

        let result = wait_for_guest_to_answer(&mut probe, PROBE_PORT, deadline).await;

        assert!(
            result.is_ok(),
            "a single stalled attempt must be abandoned and retried, not reported \
             as the guest never answering: {:?}",
            result.err()
        );
        assert!(
            probe.attempts >= 2,
            "the loop must retry after abandoning the stalled attempt, but made \
             only {} attempt(s)",
            probe.attempts
        );
    }

    /// A guest whose neighbour query answers once and then stalls forever,
    /// while its TCP probe answers from the second round on.
    struct StaleNeighborGuest {
        rounds: u32,
    }

    impl GuestProbe for StaleNeighborGuest {
        async fn answers_tcp(
            &mut self,
            _port: u16,
            _budget: std::time::Duration,
        ) -> Result<TcpAnswer> {
            self.rounds += 1;
            // Silent on round 1, answering afterwards: this is what makes the
            // stale reading the only "resolved" evidence available.
            Ok(TcpAnswer {
                answered: self.rounds > 1,
                detail: String::new(),
            })
        }

        async fn neighbor(&mut self, budget: std::time::Duration) -> Result<String> {
            if self.rounds <= 1 {
                return Ok("10.0.2.100 dev br0 lladdr 02:2c:77:3e:ae:5a REACHABLE".to_string());
            }
            // Never returns within the attempt budget again.
            tokio::time::sleep(budget * 4).await;
            Ok(String::new())
        }
    }

    /// Both halves must hold in the SAME round.
    ///
    /// Keeping the last neighbour reading across a stalled query is right for
    /// the error message and wrong for the verdict: pairing round 1's REACHABLE
    /// with round 2's TCP answer would declare ready a guest whose entry may
    /// since have gone FAILED. That is the stale-evidence failure this function
    /// was written to prevent, so a stalled query must not satisfy the
    /// neighbour half.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_neighbor_query_cannot_satisfy_readiness() {
        let mut probe = StaleNeighborGuest { rounds: 0 };
        let deadline = tokio::time::Instant::now() + GUEST_ANSWER_DEADLINE;

        let result = wait_for_guest_to_answer(&mut probe, PROBE_PORT, deadline).await;

        assert!(
            result.is_err(),
            "readiness must not be declared from a neighbour reading taken in an \
             earlier round when this round's query never returned"
        );
    }

    /// Any port; the scripted guest ignores it.
    const PROBE_PORT: u16 = 80;

    /// A deadline under tokio's PAUSED clock: `start_paused` tests advance time
    /// only when every task is idle, so the loop's retry sleeps pass instantly
    /// and deterministically — no real window to race under parallel nextest
    /// load, and the deadline genuinely expires for the silent-guest cases.
    fn paused_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_millis(100)
    }

    #[tokio::test(start_paused = true)]
    async fn silent_guest_behind_a_resolved_neighbor_is_not_ready() {
        // The bug: readiness gated on the neighbour entry, which stays REACHABLE
        // after the guest goes quiet and so cannot fail. Of 808 restored clones,
        // the 5 whose readiness probe went unanswered were all declared ready and
        // 3 of them then failed at the client's own ~100s deadline.
        let mut guest = ScriptedGuest::silent_behind_resolved_neighbor();

        let error = wait_for_guest_to_answer(&mut guest, PROBE_PORT, paused_deadline())
            .await
            .expect_err("a guest that never answers must not be declared ready");
        let message = format!("{error:#}");
        assert!(
            message.contains("never answered") && message.contains("neighbour entry is present"),
            "the deadline error must name the caught failure — MAC resolved, guest silent — \
             so the operator is not sent hunting through host-side checks that all passed: \
             {message}"
        );
        assert!(
            guest.probes > 1,
            "the loop must RETRY a silent guest until the deadline, not fail on attempt 1: \
             a clone that is merely slow to come up would otherwise be killed \
             (probes: {})",
            guest.probes
        );
    }

    #[tokio::test(start_paused = true)]
    async fn answering_guest_is_ready_on_the_first_attempt() {
        let mut guest = ScriptedGuest::answering();
        wait_for_guest_to_answer(&mut guest, PROBE_PORT, paused_deadline())
            .await
            .expect("an answering guest with a resolved neighbour is ready");
        assert_eq!(guest.probes, 1, "no retries for a healthy guest");
    }

    #[tokio::test(start_paused = true)]
    async fn guest_that_answers_late_is_ready_without_waiting_out_the_deadline() {
        let mut guest = ScriptedGuest::answering_after(2);
        wait_for_guest_to_answer(&mut guest, PROBE_PORT, paused_deadline())
            .await
            .expect("a guest that answers on attempt 3 is ready");
        assert_eq!(
            guest.probes, 3,
            "the loop must keep probing until the guest answers, then stop"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn guest_absent_from_the_neighbour_table_still_reports_arp_failure() {
        // Both halves are required: an answering guest whose neighbour entry is
        // missing is not ready, and the error must say which half failed.
        let mut guest = ScriptedGuest::absent();
        guest.last_answer = true;

        let error = wait_for_guest_to_answer(&mut guest, PROBE_PORT, paused_deadline())
            .await
            .expect_err("no resolved neighbour entry must not be ready");
        let message = format!("{error:#}");
        assert!(
            message.contains("never appeared at L2"),
            "the error must name the missing half (L2), got: {message}"
        );
    }

    #[test]
    fn neighbour_predicate_reads_l2_resolution_only() {
        // The predicate is L2-only and says nothing about the guest answering;
        // `wait_for_guest_to_answer` supplies that half. Every assertion here is
        // unchanged from when this predicate alone gated readiness.
        assert!(neighbor_is_resolved(
            "10.0.2.100 dev br0 lladdr 02:aa:bb:cc:dd:ee REACHABLE"
        ));
        assert!(neighbor_is_resolved(
            "10.0.2.100 dev br0 lladdr 02:aa:bb:cc:dd:ee STALE"
        ));
        assert!(!neighbor_is_resolved("10.0.2.100 dev br0 INCOMPLETE"));
        assert!(!neighbor_is_resolved("10.0.2.100 dev br0 FAILED"));
        assert!(!neighbor_is_resolved(""));
    }

    #[tokio::test]
    async fn published_ports_without_a_namespace_holder_fail_closed() {
        let mapping = PortMapping::parse("9222:9222").unwrap();
        let net = PastaNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![mapping]);
        let error = net
            .verify_port_forwarding()
            .await
            .expect_err("missing holder PID must not declare forwarding ready");
        assert!(
            error.to_string().contains("namespace holder PID"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn port_forward_deadline_reports_the_callers_budget() {
        let error =
            port_forward_deadline_error("127.0.0.1:9222", std::time::Duration::from_millis(37));
        assert_eq!(
            error.to_string(),
            "pasta port forward not ready within caller readiness budget 37ms: 127.0.0.1:9222"
        );
        assert!(!error.to_string().contains("5s"));
    }

    #[tokio::test]
    async fn expired_port_forward_deadline_reports_zero_caller_budget() {
        let mapping = PortMapping::parse("9222:9222").unwrap();
        let net = PastaNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![mapping]);
        let error = net
            .wait_for_port_forwarding_until(
                tokio::time::Instant::now() - std::time::Duration::from_millis(1),
            )
            .await
            .expect_err("an expired caller deadline must fail immediately");
        assert!(error.to_string().contains("budget 0ns"), "{error:#}");
        assert!(!error.to_string().contains("5s"));
    }

    /// Batch lines are exactly the heredoc body, in order, one per line.
    fn batch_lines(script: &str) -> Vec<&str> {
        let open = format!("<<'{}'\n", IP_BATCH_DELIMITER);
        let body = script
            .split_once(&open)
            .expect("script must open a batch heredoc")
            .1;
        let body = body
            .split_once(&format!("\n{}\n", IP_BATCH_DELIMITER))
            .expect("script must close the batch heredoc")
            .0;
        body.lines().collect()
    }

    #[test]
    fn setup_script_batches_every_ip_command_into_one_process() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        let script = net.build_setup_script();

        // Exactly one `ip` process for the whole phase: the only shell line that
        // invokes `ip` is the batch itself.
        let ip_lines: Vec<&str> = script
            .script()
            .lines()
            .filter(|l| l.trim_start().starts_with("ip "))
            .collect();
        assert_eq!(
            ip_lines,
            vec![format!("ip -batch - <<'{}'", IP_BATCH_DELIMITER)],
            "no per-command `ip` invocations should remain outside the batch"
        );

        assert_eq!(
            batch_lines(script.script()),
            vec![
                "tuntap add tap-fc mode tap",
                "link set tap-fc up",
                "link set lo up",
                "link show tap-fc",
            ],
            "TAP verification must be the last batch step, not a second exec"
        );
    }

    #[test]
    fn bridge_script_batches_every_ip_command_and_keeps_ip_forward() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        let script = net.build_bridge_script();

        assert_eq!(script.script().matches("ip -batch -").count(), 1);
        assert_eq!(
            batch_lines(script.script()),
            vec![
                "link set pasta0 up",
                "link add br0 type bridge",
                // Pinned so the guest can hold an authoritative neighbour entry
                // for the health-check address instead of racing pasta for one.
                "link set br0 address 02:fc:00:00:02:01",
                "link set br0 up",
                "link set pasta0 master br0",
                "link set tap-fc master br0",
                "addr add 10.0.2.1/24 dev br0",
            ]
        );
        assert!(
            script
                .script()
                .contains("echo 1 > /proc/sys/net/ipv4/ip_forward"),
            "ip_forward must still be enabled (as a shell redirect, not a process)"
        );
        assert!(script.script().starts_with("set -e\n"));
    }

    #[test]
    fn batch_failure_names_the_failing_step() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        let script = net.build_bridge_script();

        // `ip -batch` aborts at the first failing line and reports `-:<line>`.
        let msg = script.describe_failure("RTNETLINK answers: File exists\nCommand failed -:2\n");
        assert!(msg.contains("step 2/7"), "missing step index: {msg}");
        assert!(
            msg.contains("create L2 bridge br0"),
            "missing step name: {msg}"
        );
        assert!(
            msg.contains("ip link add br0 type bridge"),
            "missing command: {msg}"
        );
        assert!(
            msg.contains("RTNETLINK answers: File exists"),
            "lost stderr: {msg}"
        );
    }

    #[test]
    fn batch_failure_falls_back_to_raw_stderr() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        let script = net.build_setup_script();

        // nsenter-level failure: no `-:<line>` marker, so nothing may be swallowed.
        let msg = script.describe_failure("nsenter: cannot open /proc/123/ns/net: No such process");
        assert_eq!(
            msg,
            "nsenter: cannot open /proc/123/ns/net: No such process"
        );

        // Out-of-range line numbers must not panic or invent a step.
        assert_eq!(
            script.describe_failure("Command failed -:99"),
            "Command failed -:99"
        );
        assert_eq!(script.describe_failure("   "), "(no stderr)");
    }

    /// The batch really does run under `bash -c` and really does abort at the
    /// first failure — verified against the host's actual `ip` binary, in the
    /// current namespace, using read-only `link show` commands.
    #[test]
    fn ip_batch_script_runs_and_aborts_on_first_failure() {
        let ok = IpBatchScript::new(
            vec![
                ("show loopback".to_string(), "link show lo".to_string()),
                (
                    "show loopback again".to_string(),
                    "link show lo".to_string(),
                ),
            ],
            &["echo TRAILING_RAN"],
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(ok.script())
            .output()
            .expect("running batch script");
        assert!(
            out.status.success(),
            "batch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("TRAILING_RAN"));

        let bad = IpBatchScript::new(
            vec![
                ("show loopback".to_string(), "link show lo".to_string()),
                (
                    "show a device that cannot exist".to_string(),
                    "link show fcvm-no-such-dev".to_string(),
                ),
                ("never reached".to_string(), "link show lo".to_string()),
            ],
            &["echo TRAILING_RAN"],
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(bad.script())
            .output()
            .expect("running batch script");
        assert!(!out.status.success(), "batch should have failed");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = bad.describe_failure(&stderr);
        assert!(
            msg.contains("step 2/3") && msg.contains("show a device that cannot exist"),
            "failure not attributed to step 2: stderr={stderr:?} msg={msg}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("TRAILING_RAN"),
            "set -e must stop the script when the batch aborts"
        );
    }
}
