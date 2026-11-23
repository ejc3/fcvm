use super::{types::generate_mac, NetworkConfig, NetworkManager, PortMapping, Protocol};
use crate::paths;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

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

/// [`NAMESPACE_MAC`] as bytes.
fn namespace_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    for (byte, text) in mac.iter_mut().zip(NAMESPACE_MAC.split(':')) {
        *byte = u8::from_str_radix(text, 16).expect("NAMESPACE_MAC is six hex bytes");
    }
    mac
}

/// Guest IPv6 addressing (pasta copies host IPv6 with fd00::/64 fallback)
const GUEST_IPV6: &str = "fd00::100";
const GUEST_IPV6_GATEWAY: &str = "fd00::2";

/// Bridge device name
const BRIDGE_DEVICE: &str = "br0";

/// MTU of every link between the guest and pasta: the guest's eth0, the VM's TAP,
/// `br0` and pasta's TAP. pasta ends the guest's TCP connections and opens its own
/// on the host, so nothing past pasta sees these frames, and 65520 is pasta's own
/// default. At 1500 each frame pasta writes carries 1460 bytes, and a restored
/// guest receiving a bulk transfer is bound by frames per second: a web-server clone
/// whose daemons pulled about 1 GB from host services right after a restore took
/// 45,000 to 80,000 frames a second on one vCPU, and its TAP dropped 75 to 86
/// percent of the frames pasta sent (2026-11-16). At 65520 the first 2 GiB
/// download into a freshly restored guest took 3.2 s with no drop, against
/// 69.5 s and 9.7 million drops at 1500. The guest takes this value from the
/// boot plan and from each restore document (`NetworkConfig::guest_mtu`); the
/// host's links are set to it whatever the guest does, so a guest that keeps
/// 1500 (a snapshot made before this) only sends and receives smaller frames.
pub const GUEST_MTU: u32 = 65520;

/// TAP device name for pasta
const PASTA_DEVICE_NAME: &str = "pasta0";

/// What a VM in a pooled namespace reports when the pool ends its lease.
const LEASE_ENDED: &str = "the namespace pool ended this VM's lease: the pool exited, or the \
     holder or pasta of the VM's namespace did";

/// Timeout for waiting for pasta PID file (readiness signal)
const PASTA_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Budget for the post-restore readiness check: the guest must answer, and its
/// published ports must accept, within this window.
const GUEST_ANSWER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Upper bound on waiting for the stderr reader to reach EOF after pasta exits,
/// so the failure error can include what it actually printed. The wait ends on
/// EOF (pasta's exit closed the write end); this only bounds the pathological
/// case of an inherited, still-open pipe.
const PASTA_STDERR_EOF_TIMEOUT: std::time::Duration = crate::companion::STDERR_EOF_TIMEOUT;

/// Bounded window for a dying pasta to become reapable when a network-setup
/// step fails. pasta drops its TAP before it can be reaped, so the failing
/// step routinely observes the vanished device a beat before `try_wait`
/// reports the exit; sampling once would drop the diagnosis exactly when it
/// is needed.
const PASTA_EXIT_CONTEXT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

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

/// Run a bounded probe command, killing its whole process GROUP on expiry.
///
/// `kill_on_drop` reaps only the immediate child. The probe is
/// `nsenter -> env -> timeout -> bash`, so dropping the future kills the
/// leader and leaves `bash` with nobody to deliver the KILL that `timeout -k`
/// promised. Each expiry could then strand a process holding a reference to
/// the VM's network namespace, once per retry.
///
/// The child leads its own process group (`process_group(0)`, so pgid == pid),
/// which makes the whole subtree addressable with one `killpg`.
async fn run_probe_bounded(
    command: Command,
    budget: std::time::Duration,
    what: &str,
) -> Result<std::process::Output> {
    run_probe_until(command, tokio::time::sleep(budget), what).await
}

/// Kills a probe's process group when dropped, unless the probe ran to completion first.
///
/// The kill is in a guard, not in the branch that sees the expiry, because that branch does
/// not run when the caller stops polling first. The readiness loop wraps each attempt in its
/// own `tokio::time::timeout` of the same length, whose timer starts before the probe's and
/// can fire one timer tick earlier, and a cancelled task drops the probe the same way.
struct ProbeGroup(Option<i32>);

impl Drop for ProbeGroup {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            // SAFETY: pgid is the pid of the probe's child, which leads its own group.
            // The child is reaped only once both of its pipes have reached their end,
            // with nothing awaited between the reap and the disarm, and this guard is
            // declared after the child handle, so it is dropped first. While the guard
            // is armed the child is therefore running or a zombie, its pid has not been
            // given to another process, and the signal reaches this probe's group or
            // nothing.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

/// The command that starts pasta: `pasta_bin` with `args`, its stdout and stdin closed and
/// its stderr piped, in a process group of its own and armed to die with fcvm.
fn pasta_command(pasta_bin: &Path, args: &[OsString]) -> Command {
    let mut cmd = Command::new(pasta_bin);
    cmd.args(args);

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Out of fcvm's process group, so a terminal's Ctrl+C reaches only fcvm.
    crate::utils::own_process_group(&mut cmd);

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
    // pasta DOES change credentials after this exec: it setns()es into the holder's
    // user namespace (measured CapEff 0x1ffffffffff -> 0x2014c2), and the signal
    // survives it. `commit_creds()` clears it when uid/gid change or
    // `cred_cap_issubset(old, new)` fails. Across user namespaces that check compares no
    // capabilities: it passes when the new namespace's parent is the old one and its
    // owner is the old euid, which holds for the holder fcvm creates, as root or not, and
    // `--runas 0:0` keeps uid/gid at 0:0 under root. Measured as root, as an unprivileged
    // user and as root inside rootless podman: with pasta stopped, so that passt's PID
    // watch cannot run, killing pasta's parent killed pasta; without the pdeathsig it
    // stayed stopped. `test_sigkill_reaps_rootless_vm_tree` checks the same thing on
    // fcvm's own pasta in every lane it runs in. pasta does not fork either:
    // `--foreground` is single-threaded and its own -P pidfile reports the PID armed here.
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
    cmd
}

/// [`run_probe_bounded`] with the expiry as a future: the probe runs until it exits or
/// `abandon` completes, whichever comes first. A test passes the condition it waits for,
/// so its child has no duration to beat. Dropping the returned future abandons the probe
/// too ([`ProbeGroup`]).
async fn run_probe_until(
    mut command: Command,
    abandon: impl std::future::Future<Output = ()>,
    what: &str,
) -> Result<std::process::Output> {
    command.process_group(0);
    // Both streams MUST be piped here, exactly as `Command::output()` does it.
    // Spawning directly does not imply it: tokio's default is INHERIT, so a
    // caller that piped only stderr (the neighbour query does) would have its
    // stdout go to the parent's and be captured as empty. That reads as "no
    // neighbour entry" for every probe, which is indistinguishable from the
    // failure this whole path is diagnosing.
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {what}"))?;
    let mut group = ProbeGroup(child.id().map(|id| id as i32));
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let wait = async {
        // Both streams are read to their end before the child is reaped, so for as long
        // as the guard can fire the child is running or a zombie and its pid, which is
        // the group's id, is still its own. They are read at the same time, so a probe
        // that fills one pipe is not left blocked on it.
        let mut out = Vec::new();
        let mut err = Vec::new();
        let read_out = async {
            if let Some(handle) = stdout.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_end(handle, &mut out).await;
            }
        };
        let read_err = async {
            if let Some(handle) = stderr.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_end(handle, &mut err).await;
            }
        };
        tokio::join!(read_out, read_err);
        let status = child.wait().await?;
        Ok::<_, std::io::Error>(std::process::Output {
            status,
            stdout: out,
            stderr: err,
        })
    };
    tokio::select! {
        // The probe's own exit wins when both are ready, as with `tokio::time::timeout`.
        biased;
        result = wait => {
            // The child has exited and been reaped, so its pid can be reused from here on.
            if result.is_ok() {
                group.0 = None;
            }
            result.with_context(|| format!("running {what}"))
        }
        () = abandon => anyhow::bail!("{what} exceeded pasta's readiness deadline"),
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
        let output = run_probe_bounded(command, budget, "the TCP probe").await?;

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
        let output = run_probe_bounded(command, budget, "the neighbour query").await?;
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
    // Whether the guest's TCP probe answered on the LAST completed attempt. An
    // answered probe leaves `detail` empty, and rendering "empty" as silence is
    // what made run 31906708922 read as a dead guest when the guest had in fact
    // answered (its own TIME-WAIT socket was in the namespace dump). Carry the
    // state rather than inferring it from an absence.
    let mut last_answered = false;
    // Whether `last_neighbor` came from a query that COMPLETED this round. A
    // reading kept across a stalled query is useful context and must not be
    // presented as current state: a resolved entry from an earlier round would
    // otherwise steer the error into the "resolved to a MAC but never answered"
    // arm while TCP had in fact answered, making one message contradict itself.
    let mut last_neighbor_fresh = false;
    // Whether ANY neighbour query completed, across every round. This separates
    // "we looked and the entry was not resolved" from "we never managed to
    // look", which the error message already distinguished in words while the
    // verdict treated them identically. See the deadline arm below.
    let mut neighbor_ever_read = false;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(guest_unanswered_error(
                &last_neighbor,
                last_neighbor_fresh,
                &last_detail,
                last_answered,
                rounds,
                stalled_attempts,
            ));
        }
        // Counted here, not at the top of the loop: a round that finds the
        // deadline already spent probed nothing, and a reported count that
        // includes it overstates the work by one. The number is a diagnostic,
        // so it has to be the number of attempts actually made.
        rounds += 1;
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
        last_answered = answer.answered;
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
                last_neighbor_fresh,
                &last_detail,
                last_answered,
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
                last_neighbor_fresh = true;
                neighbor_ever_read = true;
            }
            Ok(Err(error)) if error.to_string().contains("readiness deadline") => {
                stalled_attempts += 1;
                neighbor_is_fresh = false;
                last_neighbor_fresh = false;
            }
            Ok(Err(error)) => return Err(error),
            Err(_elapsed) => {
                stalled_attempts += 1;
                neighbor_is_fresh = false;
                last_neighbor_fresh = false;
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
            // A stalled `ip neigh` is a failure to OBSERVE, not an observation
            // of failure. The TCP answer is direct end-to-end evidence over the
            // same path the forwarded traffic takes; the neighbour entry is a
            // proxy for it. When the direct evidence is positive and the proxy
            // was never readable at all, failing the guest reports a HOST
            // scheduling problem as guest unreachability.
            //
            // Observed 2026-08-16 killing a restored clone in
            // test_fuzz_lifecycle_chaos on two unrelated PRs, with the host at
            // load 155.57 and 7618 tasks: "the guest ANSWERED TCP but its
            // neighbour entry never resolved; neighbour: "" (never read: every
            // neighbour query stalled); rounds 5 (5 stalled)".
            //
            // Deliberately narrow. It requires the probe to have answered AND
            // every neighbour query across every round to have stalled. A
            // neighbour entry that was READ and found unresolved still fails,
            // and so does a guest that never answered — including the
            // resolved-MAC-but-silent case this function exists to catch.
            if last_answered && !neighbor_ever_read {
                warn!(
                    guest_ip = GUEST_IP,
                    probe_port,
                    rounds,
                    stalled_attempts,
                    host = %host_load_snapshot(),
                    "guest answered TCP but the neighbour table was never readable; \
                     accepting on the TCP evidence. This means the host was too loaded \
                     to run `ip neigh` within the budget, not that the guest was slow"
                );
                return Ok(());
            }
            return Err(guest_unanswered_error(
                &last_neighbor,
                last_neighbor_fresh,
                &last_detail,
                last_answered,
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

/// The deadline for the port-forward wait, which is NOT the remainder of the
/// guest-answer wait.
///
/// The two stages prove different things in sequence: that the guest's stack
/// answers, then that pasta's forward is wired. They used to share one
/// `GUEST_ANSWER_DEADLINE`, so a slow first stage starved the second — and the
/// first stage is slowest on exactly the hosts where the second one also needs
/// time.
///
/// That became reachable the moment `wait_for_guest_to_answer` learned to
/// accept on TCP evidence when every neighbour query stalled: it now returns Ok
/// AT the deadline instead of Err, so the port-forward wait was handed zero.
/// Observed on a CI runner at load 155:
///
/// ```text
/// WARN  guest answered TCP but the neighbour table was never readable; accepting on the TCP evidence
/// ERROR pasta port forward not ready within caller readiness budget 0ns: 127.0.0.7:27972
/// ```
///
/// A working VM was torn down for a budget it was never given. Each stage gets
/// its own, so the worst case is two budgets rather than one shared one — which
/// is the cost of not killing a guest that answered.
fn port_forward_deadline(now: tokio::time::Instant) -> tokio::time::Instant {
    now + GUEST_ANSWER_DEADLINE
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
    neighbor_fresh: bool,
    detail: &str,
    answered: bool,
    rounds: u32,
    stalled_attempts: u32,
) -> anyhow::Error {
    let neighbor = neighbor.trim();
    // An answered probe leaves `detail` empty, so empty means two opposite
    // things and must never be rendered as one. Reporting "(silence)" for a
    // guest that answered is what made run 31906708922 read as a dead guest
    // while its own TIME-WAIT socket to the prober sat in the namespace dump.
    let detail = match (answered, detail.is_empty()) {
        (true, _) => "answered (SYN-ACK or RST)",
        (false, true) => "(silence)",
        (false, false) => detail,
    };
    // Only a reading from THIS round describes the current state. A stale one
    // is labelled as such rather than silently standing in for a fresh answer.
    let neighbor_note = if neighbor_fresh {
        ""
    } else if neighbor.is_empty() {
        " (never read: every neighbour query stalled)"
    } else {
        " (STALE: this round's query stalled; reading is from an earlier round)"
    };
    if neighbor_fresh && neighbor_is_resolved(neighbor) && !answered {
        // The failure this whole path exists to catch: pasta can reach the
        // guest's MAC, so every host-side check passes, but nothing is answering
        // behind it. Report it here rather than letting a client discover it.
        anyhow::anyhow!(
            "guest {} resolved to a MAC but never answered a TCP probe within {:?}: \
             the guest is not reachable even though its neighbour entry is present; \
             neighbour: {}{}; probe: {}; rounds {} ({} stalled); host {}",
            GUEST_IP,
            GUEST_ANSWER_DEADLINE,
            neighbor,
            neighbor_note,
            detail,
            rounds,
            stalled_attempts,
            host_load_snapshot()
        )
    } else {
        anyhow::anyhow!(
            "guest {} never appeared at L2 within {:?}: {}; neighbour: {:?}{}; \
             probe: {}; rounds {} ({} stalled); host {}",
            GUEST_IP,
            GUEST_ANSWER_DEADLINE,
            if answered {
                // The guest is alive and talking; only its neighbour entry is
                // missing. Naming that separately points at ARP or the bridge
                // rather than at a dead guest.
                "the guest ANSWERED TCP but its neighbour entry never resolved"
            } else {
                "no resolved neighbour entry and no TCP answer"
            },
            neighbor,
            neighbor_note,
            detail,
            rounds,
            stalled_attempts,
            host_load_snapshot()
        )
    }
}

fn pasta_pid_file(vm_id: &str) -> PathBuf {
    paths::vm_runtime_dir(vm_id).join("pasta.pid")
}

/// Whether pasta has written its PID into `pid_file`: a decimal number and a newline.
fn pid_file_is_written(pid_file: &Path) -> bool {
    std::fs::read_to_string(pid_file)
        .is_ok_and(|text| text.ends_with('\n') && text.trim().parse::<u32>().is_ok())
}

/// What a pool key holds of the host and of the user, beside the network's own shape.
struct PoolHost {
    /// The host's global IPv6 address, which pasta is started with.
    ipv6: Option<String>,
    /// The ids the holder's user namespace maps (`holder_id_inputs`).
    id_maps: String,
    /// A digest of /etc/resolv.conf, which pasta reads once, at its start.
    resolv_conf: String,
}

impl PoolHost {
    fn of(ipv6: Option<String>) -> Self {
        use sha2::Digest;
        let resolv_conf = match std::fs::read(super::ETC_RESOLV_CONF) {
            Ok(text) => hex::encode(sha2::Sha256::digest(&text)),
            Err(error) => format!("unreadable: {}", error.kind()),
        };
        Self {
            ipv6,
            id_maps: crate::commands::common::holder_id_inputs(),
            resolv_conf,
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
/// Host                    | User Namespace (the holder's)
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
/// 1. Spawn the holder: `fcvm __netns-agent` (`netns_agent`), which unshares both namespaces
/// 2. The holder builds every link on one request from fcvm (`build_namespace`,
///    `netns_agent::build`; several netlink requests, see there):
///    both TAPs as persistent devices, the bridge, its MAC and address
/// 3. Start pasta, which attaches to its TAP
/// 4. Start the VMM in the holder's namespaces (`install_namespace_pre_exec`); it attaches
///    to its TAP
/// 5. Health checks connect to guest_ip:80 from a socket the holder made in the namespace
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
    /// pasta's pid as `companion_exited` last read it, before the wait that reaps it.
    exited_pid: Option<u32>,
    stderr_tail: crate::companion::StderrTail, // last few pasta stderr lines, for failure attribution
    // Reader task for pasta's stderr pipe. Awaited (bounded) before rendering
    // `stderr_tail` on the paths that report a dead pasta, so the attribution
    // carries what pasta printed instead of racing the reader task.
    stderr_reader: Option<tokio::task::JoinHandle<()>>,
    pid_file: Option<PathBuf>,
    loopback_ip: Option<String>, // Unique loopback IP for port forwarding (127.x.y.z)
    holder_pid: Option<u32>,     // Namespace PID (set in post_start)
    restore_mode: bool,          // Skip port probe in post_start (VM not loaded yet)
    // No pasta and no route out: the namespace, its TAP and the bridge with 10.0.2.1 only
    // (`isolated`, a raw VM's network).
    isolated: bool,
    // The resolver this launch bakes into the guest's resolv.conf (--dns, or
    // the snapshot's baked value on a restore). None for the default rootless
    // guest, which takes pasta's own DNS handling.
    dns_server: Option<String>,
    // Where pasta's PID file goes when not in the VM's runtime directory: a pooled
    // namespace is no VM's.
    pid_dir: Option<PathBuf>,
    // Whether this VM may ask a namespace pool for its namespace (`with_pooling`).
    pooling: bool,
    // The pooled namespace this VM runs in, when it claimed one (`crate::netpool`). The pool
    // owns that namespace's holder and pasta; this network starts and stops neither.
    lease: Option<crate::netpool::client::Lease>,
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
            exited_pid: None,
            stderr_tail: crate::companion::StderrTail::new(),
            stderr_reader: None,
            pid_file: None,
            loopback_ip: None,
            holder_pid: None,
            restore_mode: false,
            isolated: false,
            dns_server: None,
            pid_dir: None,
            pooling: false,
            lease: None,
        }
    }

    /// Let this VM run in a pooled namespace when one is to be had ([`Self::poolable`]).
    /// Off unless the caller turns it on: the caller knows what will run in the namespace.
    pub fn with_pooling(mut self, pooling: bool) -> Self {
        self.pooling = pooling;
        self
    }

    /// Keep pasta's PID file in `dir` instead of the VM's runtime directory.
    pub fn with_pid_dir(mut self, dir: PathBuf) -> Self {
        self.pid_dir = Some(dir);
        self
    }

    /// The pooled namespace this VM runs in, once [`NetworkManager::setup`] has claimed one.
    pub fn lease(&self) -> Option<&crate::netpool::client::Lease> {
        self.lease.as_ref()
    }

    /// What a namespace must be like for this VM, as one string: the links
    /// ([`Self::shape`]) and pasta's command line, without what names one VM or one
    /// process (the TAP's name, the PID file, the holder's PID). The pool lends a namespace
    /// only to a VM whose key equals the key of the network the pool builds
    /// ([`crate::netpool`]), so a pooled namespace is the one this VM would have built.
    pub fn pool_key(&self) -> String {
        self.pool_key_of(&PoolHost::of(Self::detect_host_ipv6()))
    }

    /// [`Self::pool_key`] with the host probed now. A pool lives for days: what it found
    /// when it started (the host's IPv6 address, its resolver file) may no longer be what a
    /// clone that starts now finds.
    ///
    /// `None` when the host's address could not be probed: a key made from a failed probe
    /// would say the host has no IPv6 address.
    pub fn pool_key_now(&self) -> Option<String> {
        match Self::probe_host_ipv6() {
            Ok(ipv6) => Some(self.pool_key_of(&PoolHost::of(ipv6))),
            Err(error) => {
                warn!(%error, "probing the host's IPv6 address failed");
                None
            }
        }
    }

    fn pool_key_of(&self, host: &PoolHost) -> String {
        use sha2::Digest;
        let mut shape = self.shape();
        shape.tap = String::new();
        let args = self.build_pasta_args(
            Path::new("/"),
            0,
            host.ipv6.as_deref(),
            nix::unistd::geteuid().is_root(),
        );
        let mut hash = sha2::Sha256::new();
        hash.update(serde_json::to_vec(&shape).expect("a shape is plain data"));
        for arg in &args {
            hash.update([0u8]);
            hash.update(arg.as_encoded_bytes());
        }
        // Who the holder's user namespace maps: a namespace a root pool built for one sudo
        // invoker does not map another's uid, and that clone's VMM could not open what
        // its own namespace would let it.
        hash.update([0u8]);
        hash.update(host.id_maps.as_bytes());
        // The resolver file pasta read when it started.
        hash.update([0u8]);
        hash.update(host.resolv_conf.as_bytes());
        hex::encode(hash.finalize())
    }

    /// Whether this VM asks a pool for its namespace: its caller allowed it
    /// ([`Self::with_pooling`]), and it is a restored VM with pasta and no published port.
    /// A pooled pasta has no forwards, and a cold boot keeps its own namespace.
    fn poolable(&self) -> bool {
        self.pooling && self.restore_mode && !self.isolated && self.port_mappings.is_empty()
    }

    /// A namespace with no way out, for a raw VM (`fcvm vm run`): the holder's user and
    /// network namespaces, the VM's TAP and the bridge with 10.0.2.1 and the pinned
    /// [`NAMESPACE_MAC`], and nothing else. No pasta starts, so the namespace has no
    /// default route, no gateway and no published port: the guest reaches the
    /// namespace's own address and nothing beyond it, whatever is set inside it.
    pub fn isolated(vm_id: String, tap_device: String) -> Self {
        Self {
            isolated: true,
            ..Self::new(vm_id, tap_device, Vec::new())
        }
    }

    /// Whether this namespace has no pasta and no route out ([`Self::isolated`]).
    pub fn is_isolated(&self) -> bool {
        self.isolated
    }

    /// The MTU of this namespace's links and of the guest's eth0: [`GUEST_MTU`] behind
    /// pasta, none (1500) in an isolated namespace, which has no pasta behind its bridge.
    /// The TAP, the guest (`setup`'s `guest_mtu`) and the bridge script's early return
    /// for an isolated namespace all follow this one decision.
    fn links_mtu(&self) -> Option<u32> {
        (!self.isolated).then_some(GUEST_MTU)
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

    /// Set the resolver this launch bakes into the guest's resolv.conf.
    ///
    /// Rootless mode sets no resolver of its own, so this is `--dns` on the
    /// boot path and the snapshot's baked value on a restore.
    pub fn with_dns_server(mut self, dns_server: Option<String>) -> Self {
        self.dns_server = dns_server;
        self
    }

    /// Get the loopback IP assigned to this VM for port forwarding
    pub fn loopback_ip(&self) -> Option<&str> {
        self.loopback_ip.as_deref()
    }

    /// The links of this VM's namespace: its TAP and, unless isolated, pasta's TAP on `br0`,
    /// which holds 10.0.2.1/24 under the pinned [`NAMESPACE_MAC`]. An isolated namespace
    /// ([`Self::isolated`]) has no pasta TAP and the kernel's default MTU.
    pub fn shape(&self) -> super::netns_agent::Shape {
        super::netns_agent::Shape {
            tap: self.tap_device.clone(),
            pasta_tap: (!self.isolated).then(|| self.pasta_device.clone()),
            bridge: BRIDGE_DEVICE.to_string(),
            bridge_mac: namespace_mac(),
            bridge_address: NAMESPACE_IP
                .parse()
                .expect("NAMESPACE_IP is an IPv4 address"),
            bridge_prefix: 24,
            mtu: self.links_mtu(),
        }
    }

    /// Build this VM's namespace through its holder: every link of [`Self::shape`] in one
    /// request, with no process started. pasta is started afterwards ([`Self::post_start`])
    /// and attaches to the TAP made for it here.
    ///
    /// In restore mode with a published UDP port, the gate that drops datagrams to those
    /// guest ports until the restore is acknowledged ([`super::udp_gate`]) goes onto the TAP
    /// here too. No pasta exists yet, so no forwarded datagram can reach the TAP ahead of it.
    pub async fn build_namespace(&self, holder_pid: u32) -> Result<()> {
        let agent = super::netns_agent::client(holder_pid).with_context(|| {
            format!("this process holds no channel to the namespace holder (PID {holder_pid})")
        })?;
        agent
            .build(&self.shape())
            .await
            .context("building the namespace's links")?;
        let gate = self.restore_udp_gate_script();
        if !gate.is_empty() {
            let prefix = self.build_nsenter_prefix(holder_pid);
            let output = Command::new(&prefix[0])
                .args(&prefix[1..])
                .args(["sh", "-c", &gate])
                .output()
                .await
                .context("running tc to close the clone's UDP gate")?;
            if !output.status.success() {
                anyhow::bail!(
                    "closing the clone's UDP gate on {} failed: {}",
                    self.tap_device,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(())
    }

    /// In restore mode, the shell lines that drop datagrams to the published UDP guest
    /// ports on the clone's TAP until the restore is acknowledged
    /// ([`super::udp_gate`]). Empty outside restore mode or without a UDP mapping.
    fn restore_udp_gate_script(&self) -> String {
        if !self.restore_mode {
            return String::new();
        }
        super::udp_gate::close_script(
            &self.tap_device,
            &super::udp_gate::published_udp_guest_ports(&self.port_mappings),
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
        "holder(fcvm __netns-agent) + nsenter for setup, setns for firecracker".to_string()
    }

    /// Detect host's global IPv6 address for pasta outbound traffic.
    ///
    /// Memoised for the process lifetime: `setup()` and `start_pasta()` both need
    /// it, and the answer cannot change during one launch. Only an answer is kept: a
    /// probe that failed is "no address" for this call and is made again at the next
    /// ([`Self::remembered`]).
    fn detect_host_ipv6() -> Option<String> {
        static HOST_IPV6: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        Self::remembered(&HOST_IPV6, Self::probe_host_ipv6)
    }

    /// `probe`'s answer, kept in `kept` once there is one. A probe that fails is logged
    /// and answered as `None` without being kept, so a failure that passes (the address
    /// dump interrupted four times in a row on a host that is changing its addresses) does
    /// not become "this host has no IPv6 address" for the rest of a long-lived process.
    fn remembered(
        kept: &std::sync::OnceLock<Option<String>>,
        probe: impl FnOnce() -> std::io::Result<Option<String>>,
    ) -> Option<String> {
        if let Some(answer) = kept.get() {
            return answer.clone();
        }
        match probe() {
            Ok(answer) => kept.get_or_init(|| answer).clone(),
            Err(error) => {
                warn!(%error, "probing the host's IPv6 address failed; none is assumed for now");
                None
            }
        }
    }

    /// The first global-scope IPv6 address of this network namespace's interfaces that is
    /// not a unique local one (fd00::/8), from one netlink dump of the IPv6 addresses: no
    /// process is started. Only addresses are asked for. A dump of the links as well, which
    /// getifaddrs makes, waits for `rtnl_mutex` on a host that is starting many VMs.
    ///
    /// An error is the probe failing (no netlink socket, a dump the kernel kept
    /// interrupting), which is not the same as a host without such an address.
    fn probe_host_ipv6() -> std::io::Result<Option<String>> {
        let mut rtnl = super::rtnl::Rtnl::open()?;
        // struct ifaddrmsg: family, prefix length, flags, scope, index.
        let mut header = [0u8; 8];
        header[0] = libc::AF_INET6 as u8;
        let entries = rtnl.dump(super::rtnl::RTM_GETADDR, &header)?;
        const RT_SCOPE_UNIVERSE: u8 = 0;
        Ok(Self::first_global_ipv6(
            entries
                .iter()
                .filter(|entry| entry.payload.get(3) == Some(&RT_SCOPE_UNIVERSE))
                .filter_map(|entry| Self::own_ipv6_address(&entry.attrs(8))),
        )
        .map(|address| address.to_string()))
    }

    /// This host's own address in one dumped IPv6 address entry: `IFA_LOCAL` when the entry
    /// has one, else `IFA_ADDRESS`. On a point-to-point link `IFA_ADDRESS` is the peer's.
    fn own_ipv6_address(attrs: &[(u16, &[u8])]) -> Option<std::net::Ipv6Addr> {
        const IFA_ADDRESS: u16 = 1;
        const IFA_LOCAL: u16 = 2;
        let value = [IFA_LOCAL, IFA_ADDRESS].iter().find_map(|wanted| {
            attrs
                .iter()
                .find(|(kind, _)| kind == wanted)
                .map(|(_, value)| *value)
        })?;
        <[u8; 16]>::try_from(value)
            .ok()
            .map(std::net::Ipv6Addr::from)
    }

    /// The first of `addresses` that `ip -6 addr show scope global` would list and that is
    /// not in fd00::/8: not loopback, link-local (fe80::/10) or site-local (fec0::/10).
    fn first_global_ipv6(
        addresses: impl IntoIterator<Item = std::net::Ipv6Addr>,
    ) -> Option<std::net::Ipv6Addr> {
        addresses.into_iter().find(|address| {
            let first = address.segments()[0];
            !address.is_loopback()
                && !address.is_unspecified()
                && first & 0xffc0 != 0xfe80
                && first & 0xffc0 != 0xfec0
                && first & 0xff00 != 0xfd00
        })
    }

    /// Detect the host's proxy for one scheme: the first of `names` that is set.
    ///
    /// On IPv6-only hosts, traffic must go through a proxy.
    /// Returns the proxy URL with IPv6 address resolved from hostname.
    fn detect_http_proxy(names: [&str; 4]) -> Option<String> {
        Self::detect_http_proxy_in(names, |name| std::env::var(name).ok())
    }

    /// [`Self::detect_http_proxy`] over `var`, the environment's lookup.
    fn detect_http_proxy_in(
        names: [&str; 4],
        var: impl Fn(&str) -> Option<String>,
    ) -> Option<String> {
        let proxy_url = names.into_iter().find_map(var)?;

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

    /// Is `dns_server` the address pasta answers for as the guest's gateway?
    ///
    /// Parsed rather than matched as text, so the IPv6 gateway is recognised
    /// in any of its spellings.
    fn resolver_is_pasta_gateway(dns_server: Option<&str>) -> bool {
        let Some(addr) = dns_server.and_then(|s| s.trim().parse::<IpAddr>().ok()) else {
            return false;
        };
        [GUEST_GATEWAY, GUEST_IPV6_GATEWAY]
            .iter()
            .filter_map(|gw| gw.parse::<IpAddr>().ok())
            .any(|gw| gw == addr)
    }

    /// The complete pasta argument vector for this launch, in invocation order.
    ///
    /// Split out from the spawn so a unit test can assert the wiring: these
    /// arguments decide where the guest's packets land, and a flag that is
    /// computed but never passed looks exactly like one that works.
    fn build_pasta_args(
        &self,
        pid_file: &Path,
        namespace_pid: u32,
        host_ipv6: Option<&str>,
        run_as_root: bool,
    ) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "--foreground".into(),
            "--quiet".into(),
            "-P".into(),
            pid_file.as_os_str().to_os_string(),
        ];
        let mut push = |arg: &str| args.push(OsString::from(arg));

        // When running as root (e.g., sudo in tests), pasta drops to nobody by
        // default and then can't access the user namespace. Tell it to stay as root.
        if run_as_root {
            push("--runas");
            push("0:0");
        }

        // Don't use --config-net: it sets an IP on pasta0's kernel interface, which
        // conflicts with the bridge (kernel responds to ARP for that IP via bridge's
        // weak host model, stealing traffic from pasta's userspace L2 handler).
        // Instead, the holder makes the TAP and brings it up (`netns_agent::build`).
        //
        // -a must be the VM's actual IP (GUEST_IP), not the gateway. pasta uses -a
        // as the "guest address" and ignores ARP requests for it (don't resolve self).
        // If -a == gateway, pasta ignores ARP for the gateway and the VM can't route.
        // On a host with no IPv4 interface pasta keeps -a, -n and -g only from
        // passt 4e8aa70379a3 on; the pinned binary is checked there by
        // `does_not_answer_arp_for_the_guest_address_in_local_mode`.
        push("--ns-ifname");
        push(&self.pasta_device);
        push("-a");
        push(GUEST_IP); // VM's actual IP — pasta ignores ARP for this address
        push("-n");
        push("255.255.255.0");
        push("-g");
        push(GUEST_GATEWAY); // Gateway — pasta responds to ARP for this
        push("--no-dhcp");
        // The MTU pasta advertises over DHCP and NDP. fcvm addresses the guest itself,
        // but this is pinned to the links' MTU rather than left to pasta's default (the
        // same value today), so nothing pasta advertises disagrees with the links.
        push("-m");
        push(&GUEST_MTU.to_string());

        // If host has global IPv6, configure pasta for IPv6 outbound
        if let Some(ipv6) = host_ipv6 {
            // Add IPv6 guest address and gateway so pasta handles IPv6 L2↔L4 translation.
            // -a/-g can each be specified twice (once IPv4, once IPv6).
            // Unlike ARP, pasta answers NDP for every address, the guest's included.
            // Nothing in the namespace has an address or a route in the guest's IPv6
            // prefix, so nothing there resolves the guest's IPv6 address.
            push("-a");
            push(GUEST_IPV6);
            push("-g");
            push(GUEST_IPV6_GATEWAY);
            push("-o");
            push(ipv6); // Outbound source address for IPv6

            // Keep NDP enabled: the guest needs NDP Neighbor Solicitation/Advertisement
            // to resolve the IPv6 gateway's MAC address (like ARP for IPv4).
            // Disable only RA (router advertisements) and DHCPv6 — we configure the
            // guest's IPv6 address statically via kernel cmdline, not SLAAC.
            push("--no-ra");
            push("--no-dhcpv6");
        } else {
            // No host IPv6: pasta runs IPv6 in local mode, with no outbound address, so
            // the guest reaches the gateway and nothing beyond it. The gateway is mapped
            // to the host's ::1 explicitly: local mode maps nothing by itself, and the
            // IPv4 gateway keeps its default mapping to 127.0.0.1. A host service that
            // listens on ::1 alone is then reachable (#1026).
            push("-a");
            push(GUEST_IPV6);
            push("-g");
            push(GUEST_IPV6_GATEWAY);
            push("--map-host-loopback");
            push(GUEST_IPV6_GATEWAY);
            push("--no-ra");
            push("--no-dhcpv6");
        }

        // pasta reads the HOST's /etc/resolv.conf, and when its first
        // nameserver is a loopback address it claims the gateway for DNS:
        // conf.c's add_dns_resolv4() sets dns_match = map_host_loopback, which
        // defaults to the gateway passed above, and fwd_nat_from_tap() in
        // fwd.c applies that redirect BEFORE the gateway-to-loopback
        // translation. A guest told to resolve at 10.0.2.2 then reaches
        // whatever the host resolves with (127.0.0.53 under systemd-resolved,
        // 127.0.0.1 under dnsmasq) rather than the service bound on host
        // 127.0.0.1:53, which is what --dns 10.0.2.2 names. The result is
        // host-dependent: the corpus replay benchmark worked on a dnsmasq box
        // and silently resolved against the live internet on a
        // systemd-resolved one.
        //
        // -D none clears dns_match, so port 53 goes through the same
        // gateway-to-loopback translation as every other port. Only launches
        // that named the gateway as their resolver get it; a default rootless
        // guest keeps pasta's DNS handling.
        if Self::resolver_is_pasta_gateway(self.dns_server.as_deref()) {
            push("-D");
            push("none");
        }

        // Port forwarding: pasta binds on host, L2 frames go through bridge to VM
        if self.port_mappings.is_empty() {
            push("-t");
            push("none");
            push("-u");
            push("none");
        } else {
            let mut tcp_specs = Vec::new();
            let mut udp_specs = Vec::new();

            for mapping in &self.port_mappings {
                let bind_addr = match &mapping.host_ip {
                    Some(ip) => ip.as_str(),
                    None => self.loopback_ip.as_deref().unwrap_or("127.0.0.1"),
                };

                // pasta spec: "bind_addr/host_port:guest_port". An IPv6 mapping names
                // the guest's address as its target ("…:fd00::100/guest_port"): pasta
                // splices a connection from a loopback client into the namespace and
                // connects to the guest from there, which works over IPv4 (the bridge
                // holds 10.0.2.1/24) and not over IPv6, where the namespace has no
                // address and the connection was reset. A rule with a target address is
                // never spliced, so the connection reaches the guest over pasta's link,
                // from the IPv6 gateway (pasta maps the host's loopback to it).
                // An IPv4-mapped address is an IPv4 bind to pasta, which refuses an
                // IPv6 target for it, and is spliced over the namespace's IPv4.
                let spec = if bind_addr
                    .parse::<std::net::Ipv6Addr>()
                    .is_ok_and(|address| address.to_ipv4_mapped().is_none())
                {
                    format!(
                        "{}/{}:{}/{}",
                        bind_addr, mapping.host_port, self.guest_ipv6, mapping.guest_port
                    )
                } else {
                    format!("{}/{}:{}", bind_addr, mapping.host_port, mapping.guest_port)
                };

                match mapping.proto {
                    Protocol::Tcp => tcp_specs.push(spec),
                    Protocol::Udp => udp_specs.push(spec),
                }

                // The address and port are separate fields: joined with ':' an IPv6
                // address and its port cannot be told apart.
                info!(
                    proto = ?mapping.proto,
                    host_ip = %bind_addr,
                    host_port = mapping.host_port,
                    guest = %format!("{}:{}", self.guest_ip, mapping.guest_port),
                    "adding port forward"
                );
            }

            if tcp_specs.is_empty() {
                push("-t");
                push("none");
            } else {
                for spec in &tcp_specs {
                    push("-t");
                    push(spec);
                }
            }
            if udp_specs.is_empty() {
                push("-u");
                push("none");
            } else {
                for spec in &udp_specs {
                    push("-u");
                    push(spec);
                }
            }
        }

        // Disable host→namespace port forwarding (reverse direction).
        // These don't affect outbound traffic — pasta's L2↔L4 translation handles
        // that independently. Matches Podman's invocation pattern.
        push("-T");
        push("none");
        push("-U");
        push("none");

        // Attach to the holder's namespace
        push(&namespace_pid.to_string());

        args
    }

    /// Start pasta process attached to the namespace
    ///
    /// pasta attaches to the TAP the holder made for it ([`Self::build_namespace`]) and
    /// provides L2↔L4 translation to the host. Its PID file signals readiness.
    pub async fn start_pasta(&mut self, namespace_pid: u32) -> Result<()> {
        let pid_file = match &self.pid_dir {
            Some(dir) => dir.join("pasta.pid"),
            None => pasta_pid_file(&self.vm_id),
        };
        let pid_dir = pid_file
            .parent()
            .context("the pasta PID file has no directory")?
            .to_path_buf();

        // Register the inotify watch BEFORE the stale-file cleanup and spawn:
        // pasta's PID-file write after this point always produces an event, so
        // readiness can never be missed (zero-race: watch → check → wait).
        // Init failure (e.g. fs.inotify.max_user_instances exhausted by many
        // concurrent clone starts as one user) must not fail start_pasta — the
        // old poll never had that failure mode. Degrade to the 250ms safety
        // tick below, same as the vm.rs API-socket watch.
        let mut pid_file_watch = match crate::utils::DirWatch::new(&pid_dir) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    error = %e,
                    "inotify unavailable for pasta PID-file wait; degrading to 250ms polling"
                );
                None
            }
        };

        // Same rule as cleanup(): no exists() pre-check, because
        // `Path::exists()` maps metadata errors to false and would silently
        // keep a stale file that the readiness wait then mistakes for pasta
        // being ready. Only NotFound means there was nothing to remove.
        match tokio::fs::remove_file(&pid_file).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("removing stale pasta PID file {}", pid_file.display())
                })
            }
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
        // [pasta] config section the content-addressed upstream build is
        // required (a distro pasta would reintroduce the addr_seen inbound
        // poisoning, #661); without one, PATH is used as before.
        let (config, _, _) =
            crate::setup::rootfs::load_config(None).context("loading config for pasta")?;
        let pasta_bin = crate::setup::get_pasta_for_config(config.pasta.as_ref())?;
        info!(pasta_bin = %pasta_bin.display(), "resolved pasta binary");

        let args = self.build_pasta_args(
            &pid_file,
            namespace_pid,
            host_ipv6.as_deref(),
            nix::unistd::geteuid().is_root(),
        );
        debug!(pasta_args = ?args, "pasta arguments");
        let mut cmd = pasta_command(&pasta_bin, &args);
        debug!(cmd = ?cmd, "pasta command");
        let stderr_tail = self.begin_stderr_attempt();
        let mut child = cmd.spawn().context("failed to spawn pasta")?;

        // Stream pasta's stderr: log every line and keep a tail so error paths
        // can show what pasta actually printed. Without this, pasta's output is
        // silently discarded and a dead pasta only surfaces later as an
        // unrelated bridge setup failure.
        //
        // The target must live under `fcvm::` — every documented invocation
        // filters with `RUST_LOG=fcvm=...`, and a bare `pasta` target is
        // dropped by all of them, so a fatal line like "Listen failed for
        // HOST TCP port ...: Address already in use" never reached any log.
        if let Some(stderr) = child.stderr.take() {
            self.stderr_reader = Some(crate::companion::capture(
                stderr,
                stderr_tail,
                |line| warn!(target: "fcvm::pasta", "{}", line),
            ));
        }

        self.adopt_and_await_pasta(child, pid_file, &mut pid_file_watch)
            .await
    }

    /// Take ownership of the spawned pasta and its PID path, then wait for
    /// readiness.
    ///
    /// Both fields are recorded even when the wait fails: pasta can write its
    /// PID file and exit before the wait returns, and cleanup() can only reap
    /// the child and remove the file if `self` owns them on every error path.
    /// The retry loop in `post_start` reaps `self.pasta_process` between
    /// attempts for the same reason.
    async fn adopt_and_await_pasta<E>(
        &mut self,
        mut child: Child,
        pid_file: PathBuf,
        pid_file_events: &mut E,
    ) -> Result<()>
    where
        E: crate::utils::DirEventSource,
    {
        self.pid_file = Some(pid_file.clone());
        let readiness = self
            .wait_for_pid_file(&mut child, &pid_file, pid_file_events)
            .await;
        self.pasta_process = Some(child);
        readiness
    }

    /// Start an attempt-local stderr capture.
    ///
    /// A failed attempt's pipe reader can outlive the child (for example, when a
    /// descendant inherited stderr). Merely clearing the shared deque leaves a
    /// race where that old reader appends after the next attempt starts. Give
    /// every attempt a distinct tail and cancel any reader we no longer own so
    /// stale output is structurally unable to enter the current diagnostic.
    fn begin_stderr_attempt(&mut self) -> crate::companion::StderrTail {
        if let Some(previous_reader) = self.stderr_reader.take() {
            previous_reader.abort();
        }

        let stderr_tail = crate::companion::StderrTail::new();
        self.stderr_tail = stderr_tail.clone();
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
            // pasta creates the file while it reads its options and writes its PID into it
            // last, after it has attached to its TAP and opened its sockets. The PID is the
            // readiness signal; the empty file is not.
            if pid_file_is_written(pid_file) {
                // bench/chromium/reqstages.py reads this line.
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

    /// pasta's exit status and captured stderr, for appending to
    /// network-setup errors.
    ///
    /// A pasta that cannot bind a forwarded port exits, and the real cause
    /// ("Listen failed ...: Address already in use") is only in its stderr.
    /// A dying pasta drops its TAP's carrier before it becomes
    /// reapable, so a single `try_wait` sample can race the exit; wait a
    /// bounded moment for it, and when pasta is genuinely still running,
    /// still report any stderr it produced rather than nothing.
    async fn pasta_exit_context(&mut self) -> String {
        let deadline = tokio::time::Instant::now() + PASTA_EXIT_CONTEXT_WAIT;
        loop {
            let Some(child) = self.pasta_process.as_mut() else {
                return String::new();
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    // pasta's exit closed the pipe; drain it so the tail is
                    // complete.
                    crate::utils::wait_for_stderr_eof(
                        &mut self.stderr_reader,
                        PASTA_STDERR_EOF_TIMEOUT,
                    )
                    .await;
                    return format!(" (pasta exited: {}{})", status, self.stderr_tail_message());
                }
                Ok(None) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                _ => {
                    let tail = self.stderr_tail_message();
                    if tail.contains("no stderr output captured") {
                        return String::new();
                    }
                    return format!(" (pasta still running{})", tail);
                }
            }
        }
    }

    /// Render the captured pasta stderr tail for error messages.
    fn stderr_tail_message(&self) -> String {
        self.stderr_tail.render("pasta")
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
        let readiness_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
        let loopback = self.loopback_ip.as_deref().unwrap_or("127.0.0.1");
        let pasta_pid = self.pasta_process.as_ref().and_then(|child| child.id());

        for mapping in &self.port_mappings {
            if mapping.proto != Protocol::Tcp {
                continue;
            }

            let bind_addr = match &mapping.host_ip {
                Some(ip) => ip.as_str(),
                None => loopback,
            };
            let sock_addr = readiness_address(bind_addr, mapping.host_port)?;
            let addr = sock_addr.to_string();

            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(port_forward_deadline_error(&addr, readiness_budget));
                }
                match tokio::time::timeout(remaining, port_forward_listener_ready(sock_addr)).await
                {
                    Ok(true) => {
                        // pasta binds every forwarded port before it writes its PID
                        // file and exits when a bind fails, so the exact listener is
                        // pasta's only while pasta runs. Another process can hold the
                        // exact address too.
                        if pasta_pid.is_some_and(process_has_exited) {
                            anyhow::bail!("pasta exited before it listened on {addr}");
                        }
                        debug!(addr = %addr, "port forward ready");
                        break;
                    }
                    Ok(false) => {
                        if pasta_pid.is_some_and(process_has_exited) {
                            anyhow::bail!("pasta exited before it listened on {addr}");
                        }
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

/// Whether `addr` has pasta's listener, not merely something that accepts there.
///
/// A connect to `addr` also succeeds against a listener on the wildcard address and
/// the same port. In port_conflict_failure_names_pastas_own_error that was the test's
/// own squatter: pasta had failed to bind, and fcvm booted a VM with no port forward.
/// pasta binds the exact address it forwards, and it marks itself non-dumpable, so its
/// sockets show in this network namespace's table while its file descriptors do not.
async fn port_forward_listener_ready(addr: SocketAddr) -> bool {
    tokio::net::TcpStream::connect(addr).await.is_ok() && listening_on_exactly(addr)
}

/// True when a TCP socket in this network namespace listens on exactly `addr`:
/// state 0A in /proc/net/tcp or /proc/net/tcp6, with that local address.
/// Where pasta listens for a mapping bound to `bind_addr`. pasta binds an IPv4-mapped
/// IPv6 address as its IPv4 address, so its listener is in /proc/net/tcp under that.
fn readiness_address(bind_addr: &str, port: u16) -> Result<SocketAddr> {
    let ip: IpAddr = bind_addr
        .parse()
        .with_context(|| format!("port forward host address {bind_addr}"))?;
    Ok(SocketAddr::new(ip.to_canonical(), port))
}

fn listening_on_exactly(addr: SocketAddr) -> bool {
    let table = if addr.is_ipv4() {
        "/proc/net/tcp"
    } else {
        "/proc/net/tcp6"
    };
    let Ok(text) = std::fs::read_to_string(table) else {
        return false;
    };
    text.lines().skip(1).any(|line| {
        let mut fields = line.split_whitespace();
        let local = fields.nth(1);
        let state = fields.nth(1);
        state == Some("0A") && local.and_then(parse_proc_net_socket) == Some(addr)
    })
}

/// Parses a /proc/net/tcp or tcp6 address such as `0100007F:1F90` (127.0.0.1:8080).
/// The kernel prints each 32-bit word of the address in host byte order.
fn parse_proc_net_socket(field: &str) -> Option<SocketAddr> {
    let (ip_hex, port_hex) = field.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    let word = |hex: &str| u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes);
    let ip = match ip_hex.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(ip_hex)?)),
        32 => {
            let mut bytes = [0u8; 16];
            for (i, chunk) in bytes.chunks_mut(4).enumerate() {
                chunk.copy_from_slice(&word(&ip_hex[i * 8..i * 8 + 8])?);
            }
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// True when `pid` has exited: gone, or a zombie its parent has not reaped yet.
/// `utils::is_process_alive` checks only that /proc/<pid> exists, so it reports an
/// unreaped pasta, the case this has to catch, as alive.
fn process_has_exited(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                == Some("Z")
        }
        Err(_) => true,
    }
}

fn port_forward_deadline_error(addr: &str, budget: std::time::Duration) -> anyhow::Error {
    anyhow::anyhow!(
        "pasta port forward not ready within caller readiness budget {budget:?}: {addr}"
    )
}

#[async_trait::async_trait]
impl NetworkManager for PastaNetwork {
    async fn companion_exited(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(lease) = &self.lease {
            lease.ended().await;
            return Err(std::io::Error::other(LEASE_ENDED));
        }
        let Some(child) = self.pasta_process.as_mut() else {
            return std::future::pending().await;
        };
        // Read before the wait, which reaps pasta only when it returns.
        self.exited_pid = child.id();
        child.wait().await
    }

    async fn describe_companion_exit(
        &mut self,
        status: std::io::Result<std::process::ExitStatus>,
    ) -> String {
        match status {
            Ok(status) => {
                // pasta's exit closed the pipe; drain it so the tail is complete.
                crate::utils::wait_for_stderr_eof(
                    &mut self.stderr_reader,
                    PASTA_STDERR_EOF_TIMEOUT,
                )
                .await;
                crate::companion::death_message(
                    "pasta",
                    self.exited_pid,
                    &status,
                    &self.stderr_tail,
                )
            }
            Err(error) if self.lease.is_some() => error.to_string(),
            Err(error) => format!("pasta could not be watched: {error}"),
        }
    }

    async fn setup(&mut self) -> Result<NetworkConfig> {
        if self.isolated {
            info!(vm_id = %self.vm_id, guest_ip = %self.guest_ip, "setting up an isolated namespace (no pasta, no route out)");
            return Ok(NetworkConfig {
                tap_device: self.tap_device.clone(),
                guest_mac: generate_mac(),
                guest_ip: Some(format!("{}/24", self.guest_ip)),
                ..Default::default()
            });
        }
        info!(vm_id = %self.vm_id, "setting up rootless networking with pasta (bridge mode)");

        // A namespace from the pool, when one serves this data directory and has a free
        // namespace built the way this VM's would be. Here, before anything names the TAP:
        // a pooled namespace's TAP has the pool's name, not this VM's.
        if self.poolable() {
            // A clone is never worse off because a pool exists: a pool that does not
            // answer, answers wrongly or is stopping has lent this clone nothing, and the
            // clone builds its own namespace as it would with no pool.
            let claimed = match crate::netpool::client::claim(&self.pool_key()).await {
                Ok(claimed) => claimed,
                Err(error) => {
                    warn!(
                        vm_id = %self.vm_id,
                        error = %format!("{error:#}"),
                        "the namespace pool gave no answer to use; building this clone's own namespace"
                    );
                    None
                }
            };
            if let Some(lease) = claimed {
                info!(
                    vm_id = %self.vm_id,
                    slot = lease.grant.slot,
                    generation = lease.grant.generation,
                    holder_pid = lease.grant.holder_pid,
                    "running in a pooled namespace"
                );
                self.tap_device = lease.grant.tap.clone();
                self.lease = Some(lease);
                failpoint::hit_async("pool.post_claim").await;
            }
        }

        info!(
            guest_ip = %self.guest_ip,
            gateway = %GUEST_GATEWAY,
            loopback_ip = ?self.loopback_ip,
            "network configuration (pasta bridge mode, nsenter health checks)"
        );

        let guest_mac = generate_mac();

        // The guest always gets IPv6 to its gateway, which pasta maps to the host's ::1.
        // Beyond the gateway only when the host has a global IPv6 address; without one
        // pasta runs IPv6 in local mode and the guest gets no default route
        // (`build_pasta_args`).
        let (guest_ipv6, host_ipv6) = (
            Some(self.guest_ipv6.clone()),
            Some(GUEST_IPV6_GATEWAY.to_string()),
        );
        let guest_ipv6_local_only = Self::detect_host_ipv6().is_none();

        // Each scheme gets its own proxy; one without a variable uses the other's, as
        // the guest's single URL did before they could differ.
        let http_proxy =
            Self::detect_http_proxy(["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"]);
        let https_proxy =
            Self::detect_http_proxy(["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]);
        if let Some(ref proxy) = http_proxy {
            info!(proxy = %proxy, "detected HTTP proxy for IPv6-only network");
        }
        if https_proxy != http_proxy {
            if let Some(ref proxy) = https_proxy {
                info!(proxy = %proxy, "detected HTTPS proxy for IPv6-only network");
            }
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
            guest_ipv6_local_only,
            dns_search: None,
            http_proxy,
            https_proxy,
            namespace_name: None,
            guest_mtu: self.links_mtu(),
        })
    }

    async fn post_start(&mut self, holder_pid: u32) -> Result<()> {
        self.holder_pid = Some(holder_pid);
        if let Some(lease) = &self.lease {
            // The pool built the namespace and runs its pasta. What this process needs is
            // its channel to the holder, where every other user of a holder looks for it.
            super::netns_agent::register(holder_pid, lease.agent.clone());
            info!(holder_pid = holder_pid, "pasta + bridge setup complete");
            return Ok(());
        }
        if self.isolated {
            // An isolated namespace is its links, which the holder has built
            // (`build_namespace`); there is no pasta to start.
            return Ok(());
        }

        info!(
            holder_pid = holder_pid,
            "starting pasta for rootless networking"
        );

        // Start pasta, with a bounded retry on a transient startup failure.
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
            match self.start_pasta(holder_pid).await {
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

        // Verify port forwarding is actually working. The health checks reach the guest
        // over the bridge, so without this check "healthy" doesn't mean port forwarding
        // works.
        //
        // Skip in restore mode: during snapshot restore, post_start() runs BEFORE
        // the VM snapshot is loaded. Probing ports now forces pasta to attempt L2
        // forwarding to a non-existent guest, poisoning its connection state and
        // causing subsequent connections to return 0 bytes. The port check happens
        // later via verify_port_forwarding() after the VM is actually running.
        if !self.restore_mode && !self.port_mappings.is_empty() {
            if let Err(error) = self.wait_for_port_forwarding().await {
                anyhow::bail!("{error:#}{}", self.pasta_exit_context().await);
            }
        }

        // The marker bench/start-rate/startrate.py and bench/clone-latency-report.py split a
        // start's stages on.
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
        if let Some(lease) = self.lease.take() {
            // Giving the namespace back is closing the lease. The pool resets it once the
            // VMM, which holds the lease too, is gone.
            super::netns_agent::forget(lease.grant.holder_pid);
            info!(vm_id = %self.vm_id, slot = lease.grant.slot, "returned the pooled namespace");
            return Ok(());
        }
        info!(vm_id = %self.vm_id, "cleaning up pasta resources");
        let mut errors = Vec::new();

        // `kill()` is start_kill + wait, so this stays correct whether or not
        // `start_kill_processes` already signalled pasta (re-signalling a not-yet-reaped
        // process is a no-op); when it did, the wait below has nothing left to wait for.
        // `kill()` returning Ok also means the child was reaped, so propagating its
        // error is the process-gone confirmation: a surviving pasta keeps the tap
        // device and the forwarded loopback ports bound, and callers such as
        // `cleanup_vm_verified` treat cleanup's Ok as proof those resources are gone.
        if let Some(mut process) = self.pasta_process.take() {
            if let Err(e) = process.kill().await {
                warn!(vm_id = %self.vm_id, error = %e, "failed to kill pasta");
                errors.push(format!("killing pasta: {}", e));
                // `kill_on_drop` is off for pasta, so dropping the handle here
                // would leave a possibly-live process that nothing can signal
                // again. Put it back so the caller's next cleanup() can retry.
                self.pasta_process = Some(process);
            }
        }

        // The PID file goes only after the kill is confirmed: while pasta may
        // still be alive, the file is the on-disk record of that process. No
        // exists() pre-check either: `Path::exists()` maps metadata errors
        // (EACCES, ENOTDIR, ...) to false, which would skip the removal and
        // report success without knowing whether the file remains. NotFound is
        // the one removal error that already proves the goal state.
        if errors.is_empty() {
            if let Some(ref pid_file) = self.pid_file {
                match tokio::fs::remove_file(pid_file).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        warn!(vm_id = %self.vm_id, error = %e, "failed to remove pasta PID file");
                        errors.push(format!(
                            "removing pasta PID file {}: {}",
                            pid_file.display(),
                            e
                        ));
                    }
                }
            }
        }

        if errors.is_empty() {
            info!(vm_id = %self.vm_id, "pasta cleanup complete");
            Ok(())
        } else {
            anyhow::bail!(
                "pasta cleanup had {} error(s): {}",
                errors.len(),
                errors.join("; ")
            )
        }
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    async fn open_restore_udp_gate(&mut self) -> Result<()> {
        if self.restore_udp_gate_script().is_empty() {
            return Ok(());
        }
        let holder_pid = self
            .holder_pid
            .context("opening the clone's UDP gate: the namespace holder was never started")?;
        let prefix = self.build_nsenter_prefix(holder_pid);
        let output = Command::new(&prefix[0])
            .args(&prefix[1..])
            .arg("tc")
            .args(super::udp_gate::open_args(&self.tap_device))
            .output()
            .await
            .context("running tc to open the clone's UDP gate")?;
        if !output.status.success() {
            anyhow::bail!(
                "opening the clone's UDP gate on {} failed: {}",
                self.tap_device,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        info!(tap = %self.tap_device, "opened the published UDP ports after the restore ACK");
        Ok(())
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
        self.wait_for_port_forwarding_until(port_forward_deadline(tokio::time::Instant::now()))
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    /// pasta is started out of fcvm's process group, so a terminal's Ctrl+C reaches only
    /// fcvm (`test_ctrlc_via_terminal`). `sleep` stands in for the pasta binary.
    #[tokio::test(flavor = "current_thread")]
    async fn a_terminal_interrupt_does_not_reach_pasta() {
        let interrupt = crate::utils::TerminalInterrupt::arm();
        let mut cmd = super::pasta_command(std::path::Path::new("sleep"), &["60".into()]);
        cmd.kill_on_drop(true);
        let mut pasta = cmd.spawn().expect("spawning the stand-in pasta");
        interrupt.assert_misses(&mut pasta, "pasta").await;
    }

    /// A rootless guest is given the host's HTTP and HTTPS proxies each for its own
    /// scheme (#1464). The variables are a map, not the process environment, which a
    /// test must not change while sibling threads read it.
    #[test]
    fn the_host_proxy_is_read_per_scheme() {
        use super::PastaNetwork;
        const HTTP: [&str; 4] = ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"];
        const HTTPS: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];
        let env = |vars: &[(&str, &str)]| {
            let vars: std::collections::HashMap<String, String> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |name: &str| vars.get(name).cloned()
        };
        assert_eq!(PastaNetwork::detect_http_proxy_in(HTTP, env(&[])), None);
        let https_only = env(&[("HTTPS_PROXY", "http://192.0.2.9:3129")]);
        assert_eq!(
            PastaNetwork::detect_http_proxy_in(HTTP, https_only).as_deref(),
            Some("http://192.0.2.9:3129")
        );
        let both = [
            ("HTTPS_PROXY", "http://192.0.2.9:3129"),
            ("HTTP_PROXY", "http://192.0.2.8:3128"),
        ];
        assert_eq!(
            PastaNetwork::detect_http_proxy_in(HTTP, env(&both)).as_deref(),
            Some("http://192.0.2.8:3128")
        );
        assert_eq!(
            PastaNetwork::detect_http_proxy_in(HTTPS, env(&both)).as_deref(),
            Some("http://192.0.2.9:3129")
        );
    }

    use super::*;
    use std::collections::VecDeque;

    /// Render an argument vector as strings for assertions.
    fn arg_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Does `args` contain `needle` as a consecutive run?
    fn contains_run(args: &[String], needle: &[&str]) -> bool {
        args.windows(needle.len()).any(|w| w == needle)
    }

    /// The argument vector pasta is actually spawned with, for a plain rootless
    /// launch. Pins the flags the guest's routing depends on: the gateway pasta
    /// answers ARP for, the guest address it must not answer for, and the
    /// trailing namespace PID that decides which namespace is connected.
    #[test]
    fn pasta_args_carry_the_guest_addressing_and_the_namespace_pid() {
        let network = PastaNetwork::new("args-plain".into(), "tap0".into(), vec![]);
        let args = arg_strings(&network.build_pasta_args(
            Path::new("/run/pasta-args-plain.pid"),
            4242,
            None,
            false,
        ));

        assert!(contains_run(&args, &["-a", "10.0.2.100"]), "{args:?}");
        assert!(contains_run(&args, &["-g", "10.0.2.2"]), "{args:?}");
        assert!(contains_run(&args, &["-n", "255.255.255.0"]), "{args:?}");
        assert!(contains_run(&args, &["--no-dhcp"]), "{args:?}");
        assert!(
            contains_run(&args, &["-m", &GUEST_MTU.to_string()]),
            "{args:?}"
        );
        // No host IPv6: IPv6 in local mode, the gateway mapped to the host's ::1, and no
        // outbound address (#1026).
        assert!(contains_run(&args, &["-a", "fd00::100"]), "{args:?}");
        assert!(contains_run(&args, &["-g", "fd00::2"]), "{args:?}");
        assert!(
            contains_run(&args, &["--map-host-loopback", "fd00::2"]),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--ipv4-only" || a == "-o"),
            "{args:?}"
        );
        assert!(
            contains_run(&args, &["-P", "/run/pasta-args-plain.pid"]),
            "{args:?}"
        );
        assert!(
            contains_run(&args, &["-t", "none", "-u", "none"]),
            "{args:?}"
        );
        assert!(
            contains_run(&args, &["-T", "none", "-U", "none"]),
            "{args:?}"
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("4242"),
            "the namespace PID is pasta's trailing positional argument: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--runas"),
            "--runas is for the root launch only: {args:?}"
        );
    }

    /// Root launches keep uid/gid at 0:0 (pasta otherwise drops to nobody and
    /// cannot reach the holder's user namespace), and a host with global IPv6
    /// gets the second -a/-g pair plus the outbound source address.
    #[test]
    fn pasta_args_cover_the_root_and_ipv6_launches() {
        let network = PastaNetwork::new("args-v6".into(), "tap0".into(), vec![]);
        let args = arg_strings(&network.build_pasta_args(
            Path::new("/run/pasta-args-v6.pid"),
            7,
            Some("2001:db8::1"),
            true,
        ));

        assert!(contains_run(&args, &["--runas", "0:0"]), "{args:?}");
        assert!(contains_run(&args, &["-a", "fd00::100"]), "{args:?}");
        assert!(contains_run(&args, &["-g", "fd00::2"]), "{args:?}");
        assert!(contains_run(&args, &["-o", "2001:db8::1"]), "{args:?}");
        assert!(contains_run(&args, &["--no-ra", "--no-dhcpv6"]), "{args:?}");
        assert!(
            !args.iter().any(|a| a == "--ipv4-only"),
            "an IPv6-capable host must not disable IPv6: {args:?}"
        );
    }

    /// Port mappings become one -t/-u spec each, bound to the VM's loopback IP
    /// when the mapping named no host address.
    #[test]
    fn pasta_args_bind_port_mappings_to_the_vm_loopback_ip() {
        let network = PastaNetwork::new(
            "args-ports".into(),
            "tap0".into(),
            vec![
                PortMapping {
                    host_ip: None,
                    host_port: 8080,
                    guest_port: 80,
                    proto: Protocol::Tcp,
                    loopback_client: false,
                },
                PortMapping {
                    host_ip: Some("127.0.0.9".into()),
                    host_port: 5300,
                    guest_port: 53,
                    proto: Protocol::Udp,
                    loopback_client: false,
                },
            ],
        )
        .with_loopback_ip("127.0.0.5".into());
        let args = arg_strings(&network.build_pasta_args(
            Path::new("/run/pasta-args-ports.pid"),
            9,
            None,
            false,
        ));

        assert!(
            contains_run(&args, &["-t", "127.0.0.5/8080:80"]),
            "{args:?}"
        );
        assert!(
            contains_run(&args, &["-u", "127.0.0.9/5300:53"]),
            "{args:?}"
        );
    }

    /// The readiness check looks for an IPv4-mapped mapping's listener under its IPv4
    /// address, where pasta binds it; under the mapped IPv6 address it was never found
    /// and the launch failed after its readiness budget.
    #[test]
    fn an_ipv4_mapped_mapping_is_awaited_on_its_ipv4_address() {
        assert_eq!(
            readiness_address("::ffff:127.0.0.1", 8080).unwrap(),
            "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            readiness_address("::1", 8080).unwrap(),
            "[::1]:8080".parse::<SocketAddr>().unwrap()
        );
    }

    /// An IPv6 mapping names the guest's IPv6 address as its target, so pasta does not
    /// splice a loopback client's connection into the namespace, which has no IPv6
    /// address to connect from: that connection was reset.
    #[test]
    fn pasta_args_send_ipv6_mappings_to_the_guests_address() {
        let network = PastaNetwork::new(
            "args-ports-v6".into(),
            "tap0".into(),
            vec![
                PortMapping {
                    host_ip: Some("::1".into()),
                    host_port: 8080,
                    guest_port: 80,
                    proto: Protocol::Tcp,
                    loopback_client: false,
                },
                PortMapping {
                    host_ip: Some("::".into()),
                    host_port: 5300,
                    guest_port: 53,
                    proto: Protocol::Udp,
                    loopback_client: false,
                },
            ],
        );
        let args = arg_strings(&network.build_pasta_args(
            Path::new("/run/pasta-args-ports-v6.pid"),
            9,
            None,
            false,
        ));
        assert!(
            contains_run(&args, &["-t", "::1/8080:fd00::100/80"]),
            "{args:?}"
        );
        assert!(
            contains_run(&args, &["-u", "::/5300:fd00::100/53"]),
            "{args:?}"
        );

        // An IPv4-mapped address is an IPv4 bind to pasta, which refuses an IPv6
        // target for it.
        for proto in [Protocol::Tcp, Protocol::Udp] {
            let network = PastaNetwork::new(
                "args-ports-mapped".into(),
                "tap0".into(),
                vec![PortMapping {
                    host_ip: Some("::ffff:127.0.0.1".into()),
                    host_port: 8080,
                    guest_port: 80,
                    proto,
                    loopback_client: false,
                }],
            );
            let args = arg_strings(&network.build_pasta_args(
                Path::new("/run/pasta-args-ports-mapped.pid"),
                9,
                None,
                false,
            ));
            let flag = if proto == Protocol::Tcp { "-t" } else { "-u" };
            assert!(
                contains_run(&args, &[flag, "::ffff:127.0.0.1/8080:80"]),
                "{args:?}"
            );
        }
    }

    /// pasta reads the HOST's /etc/resolv.conf, and when its first nameserver
    /// is a loopback address (systemd-resolved's 127.0.0.53, dnsmasq's
    /// 127.0.0.1) it silently claims the gateway for DNS: conf.c's
    /// `add_dns_resolv4()` sets `dns_match = map_host_loopback`, and
    /// `fwd_nat_from_tap()` in fwd.c tests that BEFORE the gateway-to-loopback
    /// translation. Guest queries to 10.0.2.2:53 then go to whatever the host
    /// resolves with, not to the service bound on host 127.0.0.1:53 that
    /// `--dns 10.0.2.2` names.
    ///
    /// `-D none` clears `dns_match`, so port 53 falls through to the same
    /// translation as every other port. Only the launches that asked for the
    /// gateway as their resolver get it; a default rootless guest still uses
    /// pasta's DNS handling, and never names the gateway anyway
    /// (`is_usable_nameserver` drops loopback resolvers from the host list).
    ///
    /// This test sees the argument vector and nothing else.
    /// `scripts/probe-pasta-dns-gateway.sh` runs the pinned pasta binary and
    /// asks which server actually answered, with the flag and without it.
    #[test]
    fn a_gateway_resolver_disables_pastas_dns_special_case() {
        for resolver in ["10.0.2.2", "fd00::2", "fd00:0:0:0:0:0:0:2"] {
            let network = PastaNetwork::new("args-dns".into(), "tap0".into(), vec![])
                .with_dns_server(Some(resolver.to_string()));
            let args = arg_strings(&network.build_pasta_args(
                Path::new("/run/pasta-args-dns.pid"),
                11,
                None,
                false,
            ));
            assert!(
                contains_run(&args, &["-D", "none"]),
                "resolver {resolver} is the pasta gateway, so pasta's DNS \
                 special case must be off: {args:?}"
            );
        }
    }

    /// Every other resolver leaves pasta's DNS handling alone. A default
    /// rootless guest (no --dns) reaches the host's resolver through pasta,
    /// and that must keep working.
    #[test]
    fn other_resolvers_leave_pastas_dns_handling_alone() {
        for resolver in [None, Some("8.8.8.8"), Some("127.0.0.1"), Some("not-an-ip")] {
            let network = PastaNetwork::new("args-dns-other".into(), "tap0".into(), vec![])
                .with_dns_server(resolver.map(str::to_string));
            let args = arg_strings(&network.build_pasta_args(
                Path::new("/run/pasta-args-dns-other.pid"),
                12,
                None,
                false,
            ));
            assert!(
                !args.iter().any(|a| a == "-D"),
                "resolver {resolver:?} is not the pasta gateway; pasta's DNS \
                 handling must be left as it is: {args:?}"
            );
        }
    }

    /// Both rootless launch paths must hand pasta the resolver the guest will
    /// actually use, or the decision above is made from a `None` that nothing
    /// set and pasta keeps its DNS special case. A boot passes `--dns`; a
    /// restore passes the value the snapshot baked. Asserted at the source
    /// because neither call site can be reached from a unit test: both sit
    /// inside functions that spawn a VM.
    #[test]
    fn both_rootless_launch_paths_hand_pasta_the_guest_resolver() {
        for (label, src) in [
            ("podman run", include_str!("../commands/podman/mod.rs")),
            ("snapshot run", include_str!("../commands/snapshot.rs")),
        ] {
            let sites: Vec<_> = src.match_indices("PastaNetwork::new").collect();
            assert_eq!(
                sites.len(),
                1,
                "{label} builds {} PastaNetworks; every one of them needs the \
                 guest's resolver, so this test must cover each",
                sites.len()
            );
            let start = sites[0].0;
            let end = start
                + src[start..]
                    .find(';')
                    .expect("a construction statement ends in a semicolon");
            assert!(
                src[start..end].contains(".with_dns_server("),
                "{label} builds its PastaNetwork without the guest's resolver, so \
                 pasta keeps its DNS special case and a --dns naming the gateway \
                 reaches the host's own resolver instead: {}",
                &src[start..end]
            );
        }
    }

    /// A failed cleanup step must surface as an Err. Callers treat cleanup's
    /// Ok as proof the host resources are gone: `cleanup_vm_verified`
    /// (src/commands/common.rs) records the result so prepare can refuse to
    /// publish a snapshot while pasta still holds the tap device and the
    /// forwarded loopback ports, or while its PID file remains.
    ///
    /// A directory at the PID-file path makes `remove_file` fail with EISDIR
    /// for every uid, so this needs neither root nor permission games.
    #[tokio::test]
    async fn cleanup_fails_closed_when_the_pid_file_cannot_be_removed() {
        let dir = tempfile::tempdir().expect("creating tempdir");
        let mut network = PastaNetwork::new("cleanup-pidfile-test".into(), "tap0".into(), vec![]);
        network.pid_file = Some(dir.path().to_path_buf());

        let error = network
            .cleanup()
            .await
            .expect_err("cleanup must report the PID-file removal failure, not Ok");
        assert!(
            format!("{error:#}").contains("removing pasta PID file"),
            "error must attribute the failed step: {error:#}"
        );
    }

    /// `Path::exists()` maps metadata errors (EACCES, ENOTDIR, ...) to false,
    /// so an exists() pre-check skips the removal for a PID path it could not
    /// even stat, and cleanup reports Ok without knowing whether the file
    /// remains. A regular file as the parent component makes both the stat and
    /// the removal fail with ENOTDIR for every uid, so this needs neither root
    /// nor permission games.
    #[tokio::test]
    async fn cleanup_fails_closed_when_the_pid_path_cannot_be_checked() {
        let dir = tempfile::tempdir().expect("creating tempdir");
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").expect("creating blocker file");
        let mut network = PastaNetwork::new("cleanup-enotdir-test".into(), "tap0".into(), vec![]);
        network.pid_file = Some(blocker.join("pasta.pid"));

        let error = network
            .cleanup()
            .await
            .expect_err("cleanup must report the PID-file removal failure, not Ok");
        assert!(
            format!("{error:#}").contains("removing pasta PID file"),
            "error must attribute the failed step: {error:#}"
        );
    }

    /// An absent PID file is cleanup success, not an error: NotFound is the
    /// one removal outcome that already proves the goal state. This pins the
    /// boundary of the direct-removal path so it does not fail VMs whose PID
    /// file was never written or was already collected.
    #[tokio::test]
    async fn cleanup_treats_a_missing_pid_file_as_already_clean() {
        let dir = tempfile::tempdir().expect("creating tempdir");
        let mut network = PastaNetwork::new("cleanup-absent-test".into(), "tap0".into(), vec![]);
        network.pid_file = Some(dir.path().join("never-written.pid"));

        network
            .cleanup()
            .await
            .expect("NotFound means the PID file is already gone");
    }

    /// A failed kill must leave the handle in place. `kill_on_drop` is off for
    /// pasta, so dropping the Child would leave a possibly-live pasta that
    /// nothing can signal again, and a later cleanup() call could not retry.
    /// The PID file must also survive the failed attempt: it is the on-disk
    /// record that pasta may still hold the tap device and forwarded ports,
    /// and it may only be removed once the process is confirmed gone.
    ///
    /// Failure vehicle: reap the child with waitpid(2) behind tokio's back.
    /// The Child still believes the process exists, so kill() sends SIGKILL to
    /// a freed PID and gets ESRCH. Reuse of that PID inside this test would
    /// require a full pid-space wraparound between the waitpid and the kill,
    /// so the signal cannot land on a live process.
    #[tokio::test]
    async fn a_failed_kill_keeps_the_handle_and_the_pid_file() {
        let dir = tempfile::tempdir().expect("creating tempdir");
        let pid_file = dir.path().join("pasta.pid");
        std::fs::write(&pid_file, b"123\n").expect("writing PID file");

        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawning true");
        let pid = child.id().expect("child PID") as libc::pid_t;
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(reaped, pid, "waitpid must reap the child");

        let mut network = PastaNetwork::new("cleanup-kill-fail-test".into(), "tap0".into(), vec![]);
        network.pasta_process = Some(child);
        network.pid_file = Some(pid_file.clone());

        let error = network
            .cleanup()
            .await
            .expect_err("an unconfirmed kill must fail cleanup");
        assert!(
            format!("{error:#}").contains("killing pasta"),
            "error must attribute the failed step: {error:#}"
        );
        assert!(
            network.pasta_process.is_some(),
            "the handle must survive a failed kill so the next cleanup can retry"
        );
        assert!(
            pid_file.exists(),
            "the PID file outlives an unconfirmed kill"
        );
    }

    /// A readiness wait that fails still leaves real state behind: pasta can
    /// write its PID file and exit before the wait returns, and the child
    /// needs reaping either way. Both must be owned by `self` on the error
    /// path, or cleanup() skips them and reports Ok while the PID file
    /// remains on disk.
    #[tokio::test]
    async fn a_failed_readiness_wait_still_owns_the_child_and_pid_path() {
        let dir = tempfile::tempdir().expect("creating tempdir");
        let pid_file = dir.path().join("pasta.pid");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .spawn()
            .expect("spawn exiting child");
        // Reap up front so the wait's child-exit arm fires deterministically
        // instead of racing the PID-file deadline.
        child.wait().await.expect("reap exited child");

        let mut no_watch = None::<crate::utils::DirWatch>;
        let mut net = PastaNetwork::new("adopt-test".to_string(), "tap0".to_string(), vec![]);
        let error = net
            .adopt_and_await_pasta(child, pid_file.clone(), &mut no_watch)
            .await
            .expect_err("an exited pasta cannot become ready");
        assert!(
            format!("{error:#}").contains("exited before becoming ready"),
            "{error:#}"
        );
        assert_eq!(
            net.pid_file.as_deref(),
            Some(pid_file.as_path()),
            "the PID path must be recorded even when readiness fails, or cleanup cannot remove it"
        );
        assert!(
            net.pasta_process.is_some(),
            "the child must be stored even when readiness fails, or cleanup cannot reap it"
        );
    }

    /// An already-reaped pasta is not a cleanup failure. `kill()` returning Ok
    /// is the process-gone confirmation (tokio's `start_kill` on a reaped
    /// child returns Ok since 1.44), so cleanup must not fabricate an error
    /// for a process that no longer exists. This pins the boundary of the
    /// fail-closed contract: Err means "could not confirm gone", never
    /// "was already gone".
    #[tokio::test]
    async fn cleanup_reports_success_for_an_already_reaped_pasta() {
        let mut network = PastaNetwork::new("cleanup-reaped-test".into(), "tap0".into(), vec![]);
        let mut child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawning true");
        child.wait().await.expect("waiting for true");
        network.pasta_process = Some(child);

        network
            .cleanup()
            .await
            .expect("a reaped pasta means the process is gone, which is cleanup success");
    }

    /// A guest-answer wait that consumed its whole budget must not starve the
    /// port-forward wait that follows it.
    ///
    /// RED WITHOUT THE FIX: the two shared one deadline, so when the first
    /// stage ran to it — which is what the accept-on-TCP-evidence branch does
    /// by construction — the second was handed 0ns and failed immediately.
    /// Observed in CI as `pasta port forward not ready within caller readiness
    /// budget 0ns`, one line after the WARN saying the guest had answered.
    #[test]
    fn the_port_forward_wait_gets_its_own_budget() {
        let start = tokio::time::Instant::now();
        let guest_answer_deadline = start + GUEST_ANSWER_DEADLINE;
        // Stage one used every microsecond it had.
        let after_stage_one = guest_answer_deadline;

        let deadline = port_forward_deadline(after_stage_one);
        let budget = deadline.saturating_duration_since(after_stage_one);

        assert_eq!(
            budget, GUEST_ANSWER_DEADLINE,
            "the port-forward wait inherited the exhausted guest-answer deadline, \
             so a guest that ANSWERED is torn down for a budget it never got"
        );
        assert!(
            !budget.is_zero(),
            "zero budget: the wait fails before it can make a single attempt"
        );
    }

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

    use crate::test_child::{spawn_sleep, Pinned, TestChild};
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

    /// A child that stays alive for as long as a test holds it, and no longer: the
    /// [`TestChild`] guard ends it when the test returns, panics, or is killed.
    fn live_child() -> TestChild<Child> {
        // Must stay alive with NO stdin dependency: tokio's Child::wait()
        // drops the child's stdin handle on its first poll (deadlock
        // avoidance), so a `cat` with piped stdin exits 0 the moment the
        // select polls the wait arm — and whether that real OS exit beats the
        // paused clock's auto-advance to the safety tick is a scheduler race
        // (observed as a TRY 1 FAIL of the safety-tick test under a loaded
        // full-suite run, 2026-08-13).
        spawn_sleep(3600)
    }

    /// Runs `body` as a test body of its own: on a named thread, inside a runtime, because
    /// tokio spawns a child only inside one.
    fn run_as_a_test_body<F>(thread_name: &str, body: F) -> std::thread::Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        std::thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("building the test body's runtime")
                    .block_on(body)
            })
            .expect("spawning the test body's thread")
            .join()
    }

    /// A test that fails between `live_child()` and the end of its body must not leave the
    /// child running.
    ///
    /// Each `wait_for_pid_file` test below has an `expect` or an assert between the spawn and
    /// the end of its body. A bare tokio `Child` is not killed when it is dropped, so a
    /// failure there left `sleep 3600` behind as an orphan, and the orphan held the build's
    /// lock on the cargo target directory for up to an hour.
    ///
    /// The thread below is such a test body. The pidfd names that exact process and reads
    /// gone only once the process has been reaped, so a child that was killed and left as a
    /// zombie fails this too.
    #[test]
    fn a_live_child_does_not_outlive_a_test_that_panics() {
        let (pinned_tx, pinned_rx) = std::sync::mpsc::channel();
        let failing_test = run_as_a_test_body("panics-on-purpose", async move {
            let child = live_child();
            let pinned = Pinned::new(child.id().expect("live child PID"));
            pinned_tx.send(pinned).expect("handing the pidfd over");
            panic!("stand-in for an expect that fails before the child is ended");
        });
        assert!(failing_test.is_err(), "the test body must panic");
        let pinned = pinned_rx
            .recv()
            .expect("the child was pinned before the panic");

        // Read the verdict, then clean up BEFORE asserting on it: where the child does
        // outlive the panic, this test must not leak the orphan it exists to catch.
        let outlived = pinned.is_present();
        if outlived {
            pinned.kill();
            let _ = pinned.reap();
        }
        assert!(
            !outlived,
            "the live child (pid {}) was still there after the test that spawned it panicked, \
             and nothing else would have ended it for the rest of its 3600 s",
            pinned.pid()
        );
    }

    /// The exit no drop guard covers is the test process being killed outright, which is
    /// what nextest does to a test that outlives its timeout: nothing unwinds, so nothing is
    /// dropped. `forget` stands in for that here. What is left is the kernel, which must end
    /// the child when the thread that spawned it goes away.
    #[test]
    fn a_live_child_whose_guard_never_runs_dies_with_the_thread_that_spawned_it() {
        let (pinned_tx, pinned_rx) = std::sync::mpsc::channel();
        run_as_a_test_body("forgets-its-child", async move {
            let child = live_child();
            let pinned = Pinned::new(child.id().expect("live child PID"));
            pinned_tx.send(pinned).expect("handing the pidfd over");
            std::mem::forget(child);
        })
        .expect("the spawning thread returns");
        let pinned = pinned_rx.recv().expect("the child was pinned");

        let died = pinned.terminated_within(std::time::Duration::from_secs(5));
        if !died {
            // Never leave behind the orphan this test exists to catch.
            pinned.kill();
        }
        // Its handle was forgotten, so nothing else reaps it.
        let ended_by = pinned
            .reap()
            .expect("reaping the live child through its pidfd");

        assert!(
            died,
            "the live child (pid {}) was still running 5 s after the thread that spawned it \
             returned without dropping it; a test process that is killed outright would \
             leave it behind for the rest of its 3600 s",
            pinned.pid()
        );
        assert_eq!(
            ended_by,
            Some(libc::SIGTERM),
            "the kernel must end it with the pdeath signal"
        );
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
        let stderr_tail = net.stderr_tail.clone();
        net.stderr_reader = Some(tokio::spawn(async move {
            let _ = reader_started_tx.send(());
            let _ = release_reader_rx.await;
            stderr_tail.push("final pasta stderr".to_string());
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
        let stderr_tail = net.stderr_tail.clone();
        net.stderr_reader = Some(tokio::spawn(async move {
            let _ = reader_started_tx.send(());
            let _ = release_reader_rx.await;
            stderr_tail.push("stderr written immediately before timeout kill".to_string());
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
        previous_attempt.push("previous attempt before retry".to_string());

        let current_attempt = net.begin_stderr_attempt();
        previous_attempt.push("previous attempt after retry".to_string());
        current_attempt.push("current attempt output".to_string());

        let message = net.stderr_tail_message();
        assert!(message.contains("current attempt output"), "{message}");
        assert!(!message.contains("previous attempt"), "{message}");
    }

    fn default_clone() -> PastaNetwork {
        PastaNetwork::new("clone".into(), "tap-clone".into(), vec![])
            .with_loopback_ip("127.0.0.9".into())
            .with_restore_mode()
            .with_pooling(true)
    }

    /// The pool builds its namespaces from a network with the pool's TAP name and no
    /// loopback address. A default restored clone's key is that network's: neither the
    /// TAP's name nor the clone's loopback address is part of what the namespace is.
    #[test]
    fn a_default_restored_clone_has_the_key_of_a_pooled_namespace() {
        let pooled = PastaNetwork::new("slot".into(), "tap-pool".into(), vec![]);
        assert_eq!(default_clone().pool_key(), pooled.pool_key());
        assert!(default_clone().poolable());
    }

    /// The key holds who the holder's user namespace maps, the host's IPv6 address and its
    /// resolver file: a pool built for one sudo invoker, or before the host's address or
    /// resolvers changed, has another key than a clone that starts now.
    #[test]
    fn the_key_holds_the_id_maps_and_the_hosts_address_and_resolvers() {
        let host = |id_maps: &str, ipv6: Option<&str>, resolv_conf: &str| PoolHost {
            ipv6: ipv6.map(str::to_string),
            id_maps: id_maps.to_string(),
            resolv_conf: resolv_conf.to_string(),
        };
        let network = default_clone();
        let key = |host: &PoolHost| network.pool_key_of(host);
        let base = host("uid=0 gid=0 invoker=1000:1000", None, "aa");
        assert_eq!(
            key(&base),
            key(&host("uid=0 gid=0 invoker=1000:1000", None, "aa"))
        );
        assert_ne!(
            key(&base),
            key(&host("uid=0 gid=0 invoker=1001:1001", None, "aa"))
        );
        assert_ne!(
            key(&base),
            key(&host("uid=0 gid=0 invoker=none", None, "aa"))
        );
        assert_ne!(
            key(&base),
            key(&host("uid=0 gid=0 invoker=1000:1000", None, "bb"))
        );
        assert_ne!(
            key(&base),
            key(&host(
                "uid=0 gid=0 invoker=1000:1000",
                Some("2001:db8::1"),
                "aa"
            ))
        );
        // In one process the two public forms agree while the host does not change.
        assert_eq!(Some(network.pool_key()), network.pool_key_now());
    }

    /// A clone whose pasta would be started differently does not match: here one that
    /// bakes its own resolver.
    #[test]
    fn a_clone_with_its_own_resolver_has_another_key() {
        let own_dns = default_clone().with_dns_server(Some("10.0.2.2".into()));
        assert_ne!(own_dns.pool_key(), default_clone().pool_key());
    }

    /// A pooled pasta has no forwards, so a clone that publishes a port builds its own
    /// namespace, as do a cold boot and an isolated namespace.
    #[test]
    fn only_a_restored_clone_without_published_ports_is_poolable() {
        let mapping = crate::network::PortMapping {
            host_ip: None,
            host_port: 8080,
            guest_port: 80,
            proto: crate::network::Protocol::Tcp,
            loopback_client: false,
        };
        let publishing = PastaNetwork::new("clone".into(), "tap-clone".into(), vec![mapping])
            .with_loopback_ip("127.0.0.9".into())
            .with_restore_mode()
            .with_pooling(true);
        assert!(!publishing.poolable());
        let cold = PastaNetwork::new("vm".into(), "tap-vm".into(), vec![]).with_pooling(true);
        assert!(!cold.poolable());
        let isolated = PastaNetwork::isolated("raw".into(), "tap-raw".into()).with_pooling(true);
        assert!(!isolated.poolable());
        // And a clone whose caller did not allow it: a Cloud Hypervisor restore.
        let not_allowed =
            PastaNetwork::new("clone".into(), "tap-clone".into(), vec![]).with_restore_mode();
        assert!(!not_allowed.poolable());
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

    /// A guest that answers TCP while every neighbour query stalls.
    ///
    /// The shape observed on a saturated x64 CI runner (load 155.57, 7618
    /// tasks): the probe answered every round, and `ip neigh` never once
    /// returned inside its budget.
    struct UnobservableNeighborGuest {
        rounds: u32,
        stall: std::time::Duration,
    }

    impl GuestProbe for UnobservableNeighborGuest {
        async fn answers_tcp(
            &mut self,
            _port: u16,
            _budget: std::time::Duration,
        ) -> Result<TcpAnswer> {
            self.rounds += 1;
            Ok(TcpAnswer {
                answered: true,
                detail: String::new(),
            })
        }

        async fn neighbor(&mut self, _budget: std::time::Duration) -> Result<String> {
            tokio::time::sleep(self.stall).await;
            Ok("10.0.2.100 dev br0 lladdr 02:2c:77:3e:ae:5a REACHABLE".to_string())
        }
    }

    /// A guest that ANSWERS must not be failed because the host could not be
    /// scheduled to look at its ARP cache.
    ///
    /// RED BEFORE THE FIX: success required `answered && resolved`, and
    /// `resolved` requires a FRESH neighbour read, so a host too loaded to run
    /// `ip neigh` inside the budget failed a guest that was demonstrably
    /// reachable. Observed on 2026-08-16 killing a restored clone in
    /// test_fuzz_lifecycle_chaos on two unrelated PRs:
    ///
    ///   port forwarding verification failed after snapshot restore: guest
    ///   10.0.2.100 never appeared at L2 within 5s: the guest ANSWERED TCP but
    ///   its neighbour entry never resolved; neighbour: "" (never read: every
    ///   neighbour query stalled); probe: answered (SYN-ACK or RST); rounds 5
    ///   (5 stalled); host load 155.57/56.96/27.93, runnable 8/7618
    ///
    /// A stalled query is a failure to OBSERVE, not an observation of failure.
    /// The TCP answer is direct end-to-end evidence over the same path the
    /// forwarded traffic takes; the neighbour entry is a proxy for it. When the
    /// direct evidence is positive and the proxy is unreadable, the proxy must
    /// not override it.
    ///
    /// The opposite case is untouched and still fails: a guest whose neighbour
    /// entry IS readable and resolved but which never answers is caught by
    /// `a_resolved_neighbour_that_never_answers_is_reported_as_such`.
    #[tokio::test(start_paused = true)]
    async fn an_answering_guest_survives_an_unreadable_neighbour_table() {
        let mut probe = UnobservableNeighborGuest {
            rounds: 0,
            // Every neighbour query outlives its per-attempt budget.
            stall: GUEST_ANSWER_DEADLINE * 6,
        };
        let deadline = tokio::time::Instant::now() + GUEST_ANSWER_DEADLINE;

        let result = wait_for_guest_to_answer(&mut probe, PROBE_PORT, deadline).await;

        assert!(
            result.is_ok(),
            "the guest answered every round; failing it because `ip neigh` could \
             not be scheduled reports a host-side observability failure as guest \
             unreachability: {:?}",
            result.err()
        );
        assert!(
            probe.rounds >= 2,
            "the loop must keep probing while the neighbour query stalls, but \
             made only {} round(s)",
            probe.rounds
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

        let error = result.expect_err(
            "readiness must not be declared from a neighbour reading taken in an \
             earlier round when this round's query never returned",
        );
        let text = error.to_string();

        // The verdict was right; the DIAGNOSTIC must be too. A stale resolved
        // entry must not steer the message into "resolved to a MAC but never
        // answered" while TCP did answer, which would make one error contradict
        // itself, and must never be presented as the current reading.
        assert!(
            text.contains("STALE") || text.contains("never read"),
            "a neighbour reading kept across a stalled query must be labelled, not \
             presented as this round's state: {text}"
        );
        assert!(
            !text.contains("never answered a TCP probe"),
            "TCP answered, so the error must not select the never-answered arm on \
             the strength of a stale neighbour entry: {text}"
        );
    }

    /// The deadline error must never call an answering guest silent.
    ///
    /// A probe that answered leaves `detail` empty, so "empty" carries two
    /// opposite meanings and cannot be rendered as one. Run 31906708922 reported
    /// `no TCP answer` and `probe: (silence)` for a guest that had demonstrably
    /// answered: its own TIME-WAIT socket to the prober was in the namespace
    /// dump and its neighbour entry was REACHABLE. Hours went into looking for a
    /// dead guest that was never dead.
    #[tokio::test(start_paused = true)]
    async fn an_answering_guest_is_never_reported_as_silent() {
        // Answers TCP every time; its neighbour entry never resolves.
        let mut probe = ScriptedGuest {
            answers: [true].into_iter().collect(),
            last_answer: true,
            neighbor: "10.0.2.100 dev br0 INCOMPLETE".to_string(),
            probes: 0,
        };

        let error = wait_for_guest_to_answer(&mut probe, PROBE_PORT, paused_deadline())
            .await
            .expect_err("an unresolved neighbour must still fail readiness");
        let text = error.to_string();

        assert!(
            !text.contains("no TCP answer"),
            "the guest answered TCP, so the error must not claim otherwise: {text}"
        );
        assert!(
            !text.contains("(silence)"),
            "an answered probe leaves `detail` empty; rendering that as silence \
             inverts the meaning: {text}"
        );
        assert!(
            text.contains("ANSWERED TCP"),
            "the error must say which half actually failed, so the reader looks at \
             ARP and the bridge rather than at the guest: {text}"
        );
    }

    /// A bounded probe must capture stdout, not inherit it.
    ///
    /// `Command::output()` forces both streams to piped; spawning directly does
    /// not. The neighbour query pipes only stderr, so when this function
    /// replaced `output()` its stdout went to the parent and every reading came
    /// back empty. Four clone and port-forward tests failed in CI within 15s,
    /// all reporting a neighbour entry that never resolved, because the reading
    /// was being thrown away rather than missing.
    #[tokio::test]
    async fn a_bounded_probe_captures_stdout_even_when_the_caller_piped_only_stderr() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo neighbour-line"]);
        // Exactly what neighbor() does, and what broke it.
        command.stderr(Stdio::piped());

        let output = run_probe_bounded(command, std::time::Duration::from_secs(5), "a test probe")
            .await
            .expect("the probe must run");

        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "neighbour-line",
            "stdout was not captured. An inherited stdout reads as an empty result, \
             which is indistinguishable from the guest having no neighbour entry."
        );
    }

    /// `rounds` must equal the number of attempts actually made.
    ///
    /// It is reported in the deadline error as evidence of how much work the
    /// check did. Incrementing at the top of the loop counted a round that
    /// found the budget already spent and probed nothing, overstating it by
    /// one. A diagnostic that is off by one is a diagnostic nobody can reason
    /// from, which is the whole failure this PR is about.
    #[tokio::test(start_paused = true)]
    async fn the_reported_round_count_matches_the_probes_actually_made() {
        let mut probe = ScriptedGuest::silent_behind_resolved_neighbor();

        let error = wait_for_guest_to_answer(&mut probe, PROBE_PORT, paused_deadline())
            .await
            .expect_err("a silent guest must fail readiness");

        let text = error.to_string();
        let reported: usize = text
            .split("rounds ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no `rounds N` field in the error: {text}"));

        assert_eq!(
            reported, probe.probes,
            "the error reports {reported} rounds but the probe was called {} times. \
             The count must describe attempts made, not loop iterations entered.\n{text}",
            probe.probes
        );
    }

    /// An abandoned attempt must leave no descendant behind.
    ///
    /// `kill_on_drop` reaps only the immediate child. The production probe is
    /// nsenter -> env -> timeout -> bash, so killing the leader can leave bash
    /// alive holding a reference to the VM's network namespace, once per retry.
    /// Each attempt therefore leads its own process group and the group is
    /// killed on expiry. This drives that with a child that IGNORES SIGTERM, so
    /// only a group KILL can end it.
    #[tokio::test]
    async fn an_abandoned_probe_kills_its_whole_process_group() {
        let marker = std::env::temp_dir().join(format!("fcvm-probe-pg-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let mut command = Command::new("bash");
        command.args([
            "-c",
            &format!(
                "trap '' TERM; sleep 300 & echo $! > {}; wait",
                marker.display()
            ),
        ]);

        // The probe is abandoned once the grandchild's pid is on disk. A fixed budget
        // has to outlast bash starting, and 400 ms did not on a loaded host.
        let recorded_pid = || {
            std::fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
        };
        let recorded = async {
            while recorded_pid().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            run_probe_until(command, recorded, "a group-kill probe"),
        )
        .await
        .expect("bash records its child's pid well within 60 s");
        assert!(
            result.is_err(),
            "the probe must be abandoned to exercise the kill"
        );

        // The grandchild wrote its pid before sleeping; it must now be gone.
        let pid = recorded_pid().expect("the pid file holds the grandchild's pid");
        let _ = std::fs::remove_file(&marker);
        assert!(
            gone_within_a_second(pid).await,
            "grandchild {pid} survived the abandoned attempt. It ignores SIGTERM, so \
             only a process-group kill ends it; without one, every retry strands a \
             process holding the VM's network namespace."
        );
    }

    /// A probe that is dropped is killed as a group too. The readiness loop's own timeout
    /// around an attempt drops the probe without its expiry branch ever running, and so does
    /// a cancelled task. The expiry here never comes, so only the drop can end the child.
    #[tokio::test]
    async fn a_dropped_probe_kills_its_whole_process_group() {
        let marker = std::env::temp_dir().join(format!("fcvm-probe-drop-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let mut command = Command::new("bash");
        command.args([
            "-c",
            &format!(
                "trap '' TERM; sleep 300 & echo $! > {}; wait",
                marker.display()
            ),
        ]);
        let recorded_pid = || {
            std::fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
        };

        // Poll the probe until the grandchild's pid is on disk, then drop it mid-flight.
        // If the pid never appears, the panic drops the probe as well.
        let mut probe = Box::pin(run_probe_until(
            command,
            std::future::pending(),
            "a dropped probe",
        ));
        let pid = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                tokio::select! {
                    biased;
                    result = &mut probe => panic!("the probe ended by itself: {result:?}"),
                    () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                }
                if let Some(pid) = recorded_pid() {
                    break pid;
                }
            }
        })
        .await
        .expect("bash records its child's pid well within 60 s");
        drop(probe);
        let _ = std::fs::remove_file(&marker);

        assert!(
            gone_within_a_second(pid).await,
            "grandchild {pid} survived its probe being dropped. Nothing else kills it: \
             the expiry branch never ran, and it ignores SIGTERM."
        );
    }

    /// The guard signals the group by the leader's pid, so that pid has to stay the leader's
    /// for as long as the guard can fire. Here the leader exits at once and its child keeps
    /// both pipes open, so the probe is still running: the leader must still be there as a
    /// zombie, not reaped, or its pid could by then name another process.
    #[tokio::test]
    async fn a_probe_leader_is_not_reaped_while_its_group_can_be_killed() {
        let marker = std::env::temp_dir().join(format!("fcvm-probe-reap-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let mut command = Command::new("bash");
        command.args(["-c", &format!("sleep 300 & echo $! > {}", marker.display())]);
        let recorded_pid = || {
            std::fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
        };
        // The fields of /proc/<pid>/stat after the command name: state, ppid, pgrp.
        let stat = |pid: i32| -> Option<Vec<String>> {
            let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let rest = text.rsplit_once(')')?.1;
            Some(rest.split_whitespace().map(str::to_string).collect())
        };
        let state = |pid: i32| stat(pid).and_then(|fields| fields.first().cloned());

        let mut probe = Box::pin(run_probe_until(
            command,
            std::future::pending(),
            "a probe whose leader exits first",
        ));
        // Poll the probe until the leader has exited: a zombie, or gone once it is reaped.
        let (pid, leader, exited_as) =
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                let mut ids = None;
                loop {
                    tokio::select! {
                        biased;
                        result = &mut probe => panic!("the probe ended by itself: {result:?}"),
                        () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                    }
                    if ids.is_none() {
                        ids = recorded_pid().and_then(|pid| {
                            let leader = stat(pid)?.get(2)?.parse::<i32>().ok()?;
                            Some((pid, leader))
                        });
                    }
                    if let Some((pid, leader)) = ids {
                        match state(leader) {
                            Some(running) if running != "Z" => {}
                            exited => break (pid, leader, exited),
                        }
                    }
                }
            })
            .await
            .expect("bash starts sleep and exits well within 60 s");
        // Give a reap that is going to happen the time to happen, with the probe polled.
        let ended = tokio::time::timeout(std::time::Duration::from_millis(300), &mut probe).await;
        let later = state(leader);
        drop(probe);
        let _ = std::fs::remove_file(&marker);
        let gone = gone_within_a_second(pid).await;

        assert!(ended.is_err(), "the probe ended by itself: {ended:?}");
        assert_eq!(
            (exited_as.as_deref(), later.as_deref()),
            (Some("Z"), Some("Z")),
            "leader {leader} was reaped while its probe was still running, so the group id \
             the guard signals was free to be given to another process"
        );
        assert!(gone, "grandchild {pid} survived its probe being dropped");
    }

    /// Whether `pid` stops existing within a second. A process still there is killed, so a
    /// failing test leaves nothing behind.
    async fn gone_within_a_second(pid: i32) -> bool {
        for _ in 0..50 {
            // SAFETY: signal 0 only tests for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // SAFETY: the pid is the test's own grandchild, still alive.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        false
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

    /// Behind pasta the namespace is both TAPs on `br0`, every link at the guest's MTU, and
    /// the bridge under the pinned MAC with 10.0.2.1/24.
    #[test]
    fn the_shape_puts_both_taps_on_the_pinned_bridge() {
        let net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        assert_eq!(
            net.shape(),
            crate::network::netns_agent::Shape {
                tap: "tap-fc".to_string(),
                pasta_tap: Some("pasta0".to_string()),
                bridge: "br0".to_string(),
                bridge_mac: [0x02, 0xfc, 0x00, 0x00, 0x02, 0x01],
                bridge_address: "10.0.2.1".parse().unwrap(),
                bridge_prefix: 24,
                mtu: Some(GUEST_MTU),
            }
        );
    }

    /// An isolated namespace (a raw VM) has the VM's TAP on the bridge with the pinned MAC
    /// and 10.0.2.1, and nothing that leads out: no pasta device, no gateway or resolver
    /// handed to the guest, no published port.
    #[tokio::test]
    async fn an_isolated_namespace_has_no_way_out() {
        let mut net = PastaNetwork::isolated("vm-raw123".to_string(), "tap-fc".to_string());
        assert!(net.is_isolated());
        let shape = net.shape();
        assert_eq!(shape.pasta_tap, None);
        assert_eq!(shape.mtu, None, "an isolated namespace's links keep 1500");
        assert_eq!(
            (shape.tap.as_str(), shape.bridge.as_str()),
            ("tap-fc", "br0")
        );
        assert_eq!(shape.bridge_mac, namespace_mac());
        assert_eq!(shape.bridge_address.to_string(), NAMESPACE_IP);

        let config = net.setup().await.unwrap();
        assert_eq!(config.guest_mtu, None, "an isolated guest keeps 1500");
        assert_eq!(config.guest_ip.as_deref(), Some("10.0.2.100/24"));
        assert_eq!(config.host_ip, None, "an isolated guest has no gateway");
        assert_eq!(config.dns_server, None);
        assert_eq!(config.guest_ipv6, None);
        assert_eq!(config.loopback_ip, None);
        assert!(net.port_mappings.is_empty());
        // post_start returns before anything starts pasta.
        let source = include_str!("pasta.rs");
        let start = source
            .find("    async fn post_start(&mut self, holder_pid: u32)")
            .unwrap();
        let body = &source[start..];
        let isolated = body.find("if self.isolated {").unwrap();
        let pasta = body.find("self.start_pasta(holder_pid)").unwrap();
        assert!(isolated < pasta);
        assert!(body[isolated..pasta].contains("return Ok(());"));
    }

    /// The guest is told to take the namespace links' MTU, the one the holder gives the
    /// TAP, pasta0 and br0.
    #[tokio::test]
    async fn a_rootless_guest_takes_the_namespace_link_mtu() {
        let mut net = PastaNetwork::new("vm-test123".to_string(), "tap-fc".to_string(), vec![]);
        assert_eq!(net.setup().await.unwrap().guest_mtu, Some(GUEST_MTU));
        assert_eq!(net.shape().mtu, Some(GUEST_MTU));
    }

    /// The host address pasta sends IPv6 from is the first global one that is not unique
    /// local, as `ip -6 addr show scope global` listed them.
    #[test]
    fn the_host_ipv6_is_the_first_global_address_that_is_not_unique_local() {
        let pick = |list: &[&str]| {
            PastaNetwork::first_global_ipv6(list.iter().map(|a| a.parse().unwrap()))
                .map(|a| a.to_string())
        };
        assert_eq!(
            pick(&[
                "::1",
                "fe80::1",
                "fd00::5",
                "fec0::1",
                "2001:db8::7",
                "2001:db8::8"
            ]),
            Some("2001:db8::7".to_string())
        );
        assert_eq!(pick(&["::1", "fe80::1", "febf::1", "fdff::1"]), None);
        assert_eq!(pick(&[]), None);
    }

    /// On a point-to-point interface the kernel reports the peer in `IFA_ADDRESS` and this
    /// host in `IFA_LOCAL`; elsewhere only `IFA_ADDRESS` is present and is this host's.
    #[test]
    fn the_hosts_own_ipv6_address_is_the_local_one() {
        let own: std::net::Ipv6Addr = "2001:db8::1".parse().unwrap();
        let peer: std::net::Ipv6Addr = "2001:db8::2".parse().unwrap();
        let (own_bytes, peer_bytes) = (own.octets(), peer.octets());
        let point_to_point = [(1u16, &peer_bytes[..]), (2u16, &own_bytes[..])];
        assert_eq!(PastaNetwork::own_ipv6_address(&point_to_point), Some(own));
        let ordinary = [(1u16, &own_bytes[..])];
        assert_eq!(PastaNetwork::own_ipv6_address(&ordinary), Some(own));
        assert_eq!(PastaNetwork::own_ipv6_address(&[]), None);
    }

    /// The netlink dump finds the address getifaddrs finds on this host.
    #[test]
    fn the_host_ipv6_probe_agrees_with_getifaddrs() {
        let listed = nix::ifaddrs::getifaddrs()
            .unwrap()
            .filter_map(|interface| Some(interface.address?.as_sockaddr_in6()?.ip()));
        assert_eq!(
            PastaNetwork::probe_host_ipv6().unwrap(),
            PastaNetwork::first_global_ipv6(listed).map(|address| address.to_string())
        );
    }

    /// A probe that fails is not remembered as "no address": the next call probes again,
    /// and the first answer is the one kept.
    #[test]
    fn a_failed_probe_of_the_host_address_is_not_remembered() {
        let kept = std::sync::OnceLock::new();
        let interrupted = || Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
        assert_eq!(PastaNetwork::remembered(&kept, interrupted), None);
        assert!(kept.get().is_none(), "a failed probe was kept");
        let found = || Ok(Some("2001:db8::1".to_string()));
        assert_eq!(
            PastaNetwork::remembered(&kept, found).as_deref(),
            Some("2001:db8::1")
        );
        let never = || -> std::io::Result<Option<String>> { panic!("probed again") };
        assert_eq!(
            PastaNetwork::remembered(&kept, never).as_deref(),
            Some("2001:db8::1")
        );
        // A host without an address is an answer, and is kept.
        let none = std::sync::OnceLock::new();
        assert_eq!(PastaNetwork::remembered(&none, || Ok(None)), None);
        assert_eq!(none.get(), Some(&None));
    }

    /// pasta creates its PID file while it reads its options and writes the PID last, so
    /// only a written file means pasta is ready.
    #[test]
    fn an_empty_pid_file_is_not_a_ready_pasta() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pasta.pid");
        assert!(!pid_file_is_written(&pid_file), "no file");
        std::fs::write(&pid_file, "").unwrap();
        assert!(!pid_file_is_written(&pid_file), "created, not written");
        std::fs::write(&pid_file, "4242").unwrap();
        assert!(!pid_file_is_written(&pid_file), "half written");
        std::fs::write(&pid_file, "4242\n").unwrap();
        assert!(pid_file_is_written(&pid_file));
    }

    #[test]
    fn proc_net_addresses_parse_in_host_byte_order() {
        assert_eq!(
            parse_proc_net_socket("0100007F:1F90"),
            Some("127.0.0.1:8080".parse().unwrap())
        );
        assert_eq!(
            parse_proc_net_socket("00000000000000000000000001000000:0050"),
            Some("[::1]:80".parse().unwrap())
        );
        assert_eq!(parse_proc_net_socket("0100007F"), None);
    }

    /// A listener on the wildcard address answers a connect to any address on its
    /// port, so a connect alone reported pasta's forward ready when only the test's
    /// squatter was listening.
    #[tokio::test]
    async fn a_wildcard_squatter_does_not_make_a_port_forward_ready() {
        let squatter = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let squatted: SocketAddr = format!("127.0.0.1:{}", squatter.local_addr().unwrap().port())
            .parse()
            .unwrap();
        assert!(
            tokio::net::TcpStream::connect(squatted).await.is_ok(),
            "the squatter should answer a connect"
        );
        assert!(
            !port_forward_listener_ready(squatted).await,
            "a wildcard squatter was taken for pasta's listener"
        );

        let exact = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(port_forward_listener_ready(exact.local_addr().unwrap()).await);
    }

    /// A listener on the exact forwarded address proves nothing while pasta is dead.
    /// When another process already holds that address, pasta exits on the bind
    /// conflict and the other listener still answers, so readiness has to see pasta
    /// running too.
    #[tokio::test]
    async fn an_exact_address_squatter_does_not_make_an_exited_pastas_forward_ready() {
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = squatter.local_addr().unwrap().port();
        let mut network = PastaNetwork::new(
            "exact-squatter".into(),
            "tap0".into(),
            vec![PortMapping {
                host_ip: Some("127.0.0.1".into()),
                host_port: port,
                guest_port: 80,
                proto: Protocol::Tcp,
                loopback_client: false,
            }],
        );
        // Left unreaped, so its PID cannot be reused while the check runs.
        let exited = tokio::process::Command::new("true")
            .spawn()
            .expect("spawning true");
        let pid = exited.id().expect("child PID");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !process_has_exited(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "true did not exit within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        network.pasta_process = Some(exited);

        let error = network
            .wait_for_port_forwarding_until(
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            )
            .await
            .expect_err("an exited pasta's forward was reported ready");
        assert!(
            format!("{error:#}").contains("pasta exited before it listened on"),
            "unexpected error: {error:#}"
        );
    }

    /// Checks that run the pasta binary `fcvm setup` built. They need setup to have
    /// run, so they sit behind the feature that `make test-fast` and `make test-root`
    /// enable and `make test-unit` does not.
    #[cfg(feature = "integration-fast")]
    mod pinned_pasta {
        use super::super::{
            PastaNetwork, GUEST_GATEWAY, GUEST_IP, NAMESPACE_IP, PASTA_DEVICE_NAME,
        };
        use crate::test_child::{die_with_the_spawning_thread, TestChild};
        use std::io::BufRead;
        use std::path::Path;
        use std::process::{Child, Command, Output, Stdio};
        use std::time::{Duration, Instant};

        /// Spawn `command` as a child that cannot outlive the test.
        fn spawn(command: &mut Command) -> TestChild<Child> {
            die_with_the_spawning_thread(command);
            let child = command
                .spawn()
                .unwrap_or_else(|e| panic!("spawning {command:?}: {e}"));
            TestChild::new(child)
        }

        /// A process holding a fresh user and network namespace open, for five
        /// minutes at most.
        struct Holder {
            _process: TestChild<Child>,
            pid: u32,
            /// Command prefix that runs a program inside these namespaces.
            enter: Vec<String>,
        }

        impl Holder {
            /// `parent` is the prefix that enters the namespaces the new pair is
            /// created below. Empty creates the pair below the test's own.
            fn start(parent: &[String]) -> Self {
                let mut argv = parent.to_vec();
                argv.extend(
                    [
                        "unshare",
                        "--user",
                        "--map-root-user",
                        "--net",
                        "--",
                        "sh",
                        "-c",
                        "echo $$; exec sleep 300",
                    ]
                    .map(String::from),
                );
                let mut command = Command::new(&argv[0]);
                command
                    .args(&argv[1..])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped());
                let mut process = spawn(&mut command);
                // The holder prints its PID once its namespaces exist.
                let mut line = String::new();
                std::io::BufReader::new(process.stdout.take().expect("holder stdout"))
                    .read_line(&mut line)
                    .expect("reading the holder's PID");
                let pid: u32 = line.trim().parse().unwrap_or_else(|_| {
                    panic!(
                        "BLOCKED: {argv:?} did not create a user and network namespace \
                         (it printed {line:?}). Rootless fcvm needs the same namespaces."
                    )
                });
                // Dropping the guard has to end the holder, so the holder must be the
                // spawned child itself and not a process that child forked.
                assert_eq!(
                    pid,
                    process.id(),
                    "{argv:?} forked on its way to the holder"
                );
                let mut enter = parent.to_vec();
                enter.extend(
                    ["nsenter", "--preserve-credentials", "-U", "-n", "-t"].map(String::from),
                );
                enter.push(pid.to_string());
                enter.push("--".into());
                Self {
                    _process: process,
                    pid,
                    enter,
                }
            }

            /// Run `argv` inside these namespaces and wait for it.
            fn run(&self, argv: &[&str]) -> Output {
                Command::new(&self.enter[0])
                    .args(&self.enter[1..])
                    .args(argv)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap_or_else(|e| panic!("running {argv:?} beside PID {}: {e}", self.pid))
            }
        }

        /// What the neighbour table holds for `ip`: `None` without an entry,
        /// `Some(None)` for an entry nobody has answered for, and otherwise the
        /// link-layer address that answered.
        fn neighbour(table: &serde_json::Value, ip: &str) -> Option<Option<String>> {
            table
                .as_array()?
                .iter()
                .find(|entry| entry["dst"] == ip)
                .map(|entry| entry["lladdr"].as_str().map(str::to_owned))
        }

        /// Run `pasta_bin` with fcvm's arguments between two scratch namespaces, the
        /// outer one without IPv4, and say who answered ARP for the guest's address.
        /// `None` means nobody did. `host_ipv6` is the global address the outer
        /// namespace holds, if it holds one.
        fn who_answers_arp_for_the_guest(
            pasta_bin: &Path,
            host_ipv6: Option<&str>,
        ) -> Option<String> {
            let outer = Holder::start(&[]);
            if let Some(address) = host_ipv6 {
                // An interface that holds the global address and the default
                // route. Both ends of the pair stay inside the namespace.
                let made = outer.run(&[
                    "sh",
                    "-c",
                    &format!(
                        "ip link add veth0 type veth peer name veth1 \
                         && ip -6 addr add {address}/64 dev veth0 nodad \
                         && ip link set veth0 up && ip link set veth1 up \
                         && ip -6 route add default via 2001:db8::2 dev veth0"
                    ),
                ]);
                assert!(
                    made.status.success(),
                    "giving the outer namespace IPv6: {}",
                    String::from_utf8_lossy(&made.stderr)
                );
            }
            let inner = Holder::start(&outer.enter);

            let dir = tempfile::tempdir().expect("creating tempdir");
            let pid_file = dir.path().join("pasta.pid");
            let stderr_path = dir.path().join("pasta.stderr");
            let network = PastaNetwork::new("local-mode".into(), "tap0".into(), vec![]);
            let args = network.build_pasta_args(
                &pid_file,
                inner.pid,
                host_ipv6,
                nix::unistd::geteuid().is_root(),
            );
            let mut command = Command::new(&outer.enter[0]);
            command
                .args(&outer.enter[1..])
                .arg(pasta_bin)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&stderr_path).expect("creating pasta's stderr"));
            let mut pasta = spawn(&mut command);
            let launch = || {
                format!(
                    "{} {:?} printed {:?}",
                    pasta_bin.display(),
                    super::arg_strings(&args),
                    std::fs::read_to_string(&stderr_path).unwrap_or_default()
                )
            };

            // pasta creates its PID file empty while it reads its options and writes
            // its PID into it once its device exists, so wait for the content.
            let deadline = Instant::now() + Duration::from_secs(30);
            while !std::fs::metadata(&pid_file).is_ok_and(|file| file.len() > 0) {
                if let Some(status) = pasta.try_wait().expect("polling pasta") {
                    panic!("pasta left with {status} before it was ready: {}", launch());
                }
                assert!(
                    Instant::now() < deadline,
                    "pasta wrote no PID file within 30 s: {}",
                    launch()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            let up = inner.run(&[
                "sh",
                "-c",
                &format!(
                    "ip link set {PASTA_DEVICE_NAME} up \
                     && ip addr add {NAMESPACE_IP}/24 dev {PASTA_DEVICE_NAME}"
                ),
            ]);
            assert!(
                up.status.success(),
                "bringing {PASTA_DEVICE_NAME} up in the inner namespace: {}; {}",
                String::from_utf8_lossy(&up.stderr),
                launch()
            );

            // Each round asks for the guest first and the gateway second. Nothing
            // listens on either address: the connections only make the namespace
            // send ARP, and how they end is not the evidence. pasta takes frames
            // from its device in order, so once it has answered for the gateway
            // it has already dealt with the request for the guest. The gateway's
            // answer is also what shows that pasta answers ARP on this link.
            let deadline = Instant::now() + Duration::from_secs(30);
            let (table, pasta_mac) = loop {
                let mut attempts = String::new();
                for ip in [GUEST_IP, GUEST_GATEWAY] {
                    let attempt = inner.run(&[
                        "timeout",
                        "1",
                        "bash",
                        "-c",
                        &format!("exec 3<>/dev/tcp/{ip}/80"),
                    ]);
                    let said = String::from_utf8_lossy(&attempt.stderr);
                    // 1 is a refused or unreachable connection and 124 is the timeout.
                    // Anything else means `timeout` or `bash` did not run.
                    assert!(
                        matches!(attempt.status.code(), Some(0 | 1 | 124)),
                        "the connection attempt to {ip} could not run ({}): {said}",
                        attempt.status
                    );
                    attempts.push_str(&format!("{ip}: {} {}; ", attempt.status, said.trim()));
                }
                let listed =
                    inner.run(&["ip", "-j", "-4", "neigh", "show", "dev", PASTA_DEVICE_NAME]);
                assert!(
                    listed.status.success(),
                    "listing neighbours: {}",
                    String::from_utf8_lossy(&listed.stderr)
                );
                let table: serde_json::Value =
                    serde_json::from_slice(&listed.stdout).expect("`ip -j neigh` prints JSON");
                if let Some(Some(mac)) = neighbour(&table, GUEST_GATEWAY) {
                    break (table, mac);
                }
                assert!(
                    Instant::now() < deadline,
                    "pasta did not answer ARP for its gateway {GUEST_GATEWAY} within 30 s, \
                     so this run cannot say who answers for the guest. Neighbours: {table}. \
                     Connection attempts: {attempts}pasta: {}",
                    launch()
                );
            };
            match neighbour(&table, GUEST_IP) {
                Some(None) => None,
                None => panic!(
                    "the namespace holds no neighbour entry for {GUEST_IP}, so it sent no \
                     ARP for the guest and this run proves nothing. Neighbours: {table}"
                ),
                Some(Some(mac)) => Some(format!(
                    "ARP for the guest's address {GUEST_IP} was answered by {mac}, which is \
                     {} the address pasta answers for the gateway with. pasta is the only \
                     other station on this link, so the namespace would send it every \
                     packet meant for the guest. Neighbours: {table}. pasta: {}",
                    if mac == pasta_mac { "also" } else { "not" },
                    launch()
                )),
            }
        }

        /// fcvm passes the guest's address to pasta as `-a`, and the namespace
        /// reaches the guest across the bridge only because pasta leaves ARP for
        /// that address unanswered. A pasta that answers takes every packet the
        /// namespace sends the guest, so the HTTP health probe is refused by pasta
        /// while the guest serves the same URL to itself.
        ///
        /// passt before commit 4e8aa70379a3 answered on a host with no IPv4
        /// interface to copy its configuration from. It entered local mode there,
        /// replaced `-a` and `-g` with 169.254.2.1 and 169.254.2.2, and so no
        /// longer took 10.0.2.100 for the guest's address.
        ///
        /// Both namespaces here are scratch ones and the outer has no IPv4, so
        /// pasta is in local mode on any host this runs on. No VM is started.
        #[test]
        fn does_not_answer_arp_for_the_guest_address_in_local_mode() {
            // The binary a rootless launch runs, through the same two calls as
            // `start_pasta`.
            let (config, _, _) =
                crate::setup::rootfs::load_config(None).expect("loading the fcvm config");
            // That config is the one `./target/release/fcvm` last generated. It has
            // to carry this tree's pin, or the binary it resolves is another
            // commit's and the result says nothing about this tree.
            let tree: toml::Value = toml::from_str(include_str!("../../rootfs-config.toml"))
                .expect("parsing this tree's rootfs-config.toml");
            let pinned = |key: &str| tree["pasta"][key].as_str().expect("the [pasta] pin");
            assert_eq!(
                config
                    .pasta
                    .as_ref()
                    .map(|pasta| (pasta.repo.as_str(), pasta.commit.as_str())),
                Some((pinned("repo"), pinned("commit"))),
                "BLOCKED: the active fcvm config does not carry this tree's pasta pin, so \
                 the pasta it resolves is not the one under test. Run `make build` and \
                 `make setup-fcvm` in this checkout first."
            );
            let pasta_bin = crate::setup::get_pasta_for_config(config.pasta.as_ref())
                .expect("resolving the pasta binary");
            println!("pinned pasta: {}", pasta_bin.display());

            // The two address shapes `build_pasta_args` has: a host without a
            // global IPv6 address, and a host with one. Both run, so a failure
            // names every shape pasta answers in.
            let answered: Vec<String> = [None, Some("2001:db8::1")]
                .into_iter()
                .filter_map(|host_ipv6| who_answers_arp_for_the_guest(&pasta_bin, host_ipv6))
                .collect();
            assert!(answered.is_empty(), "{}", answered.join("\n\n"));
        }

        /// A loopback address and a port on it that nothing listens on, for one test.
        fn free_loopback_port() -> (String, u16) {
            let address = format!("127.88.{}.7", std::process::id() % 250 + 1);
            let port = std::net::TcpListener::bind((address.as_str(), 0))
                .expect("binding a loopback port")
                .local_addr()
                .unwrap()
                .port();
            (address, port)
        }

        fn link_state(holder_pid: u32, name: &str) -> serde_json::Value {
            let output = Command::new("nsenter")
                .args(["--preserve-credentials", "-U", "-n", "-t"])
                .arg(holder_pid.to_string())
                .args(["--", "ip", "-j", "-d", "link", "show", "dev", name])
                .output()
                .expect("running ip in the holder's namespaces");
            assert!(
                output.status.success(),
                "ip link show {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("ip -j prints JSON")
                [0]
            .clone()
        }

        fn has_carrier(link: &serde_json::Value) -> bool {
            link["flags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|flag| flag == "LOWER_UP")
        }

        /// A namespace built by the holder, with pasta started in it and `mappings`
        /// published, and what is needed to take it down.
        struct Running {
            network: PastaNetwork,
            holder: tokio::process::Child,
            holder_pid: u32,
            runtime_dir: std::path::PathBuf,
        }

        impl Running {
            async fn start(
                name: &str,
                loopback: &str,
                mappings: Vec<crate::network::PortMapping>,
            ) -> (Self, anyhow::Result<()>) {
                let vm_id = format!("{name}-{}", std::process::id());
                let runtime_dir = crate::paths::vm_runtime_dir(&vm_id);
                std::fs::create_dir_all(&runtime_dir).expect("creating the runtime directory");
                let (holder, holder_pid) = crate::commands::common::spawn_namespace_holder()
                    .await
                    .expect("spawning the namespace holder");
                let mut network = PastaNetwork::new(vm_id, "tap-fc".into(), mappings)
                    .with_loopback_ip(loopback.to_string())
                    // No guest exists here, so nothing may probe through the forward.
                    .with_restore_mode();
                network
                    .build_namespace(holder_pid)
                    .await
                    .expect("building the namespace");
                let started =
                    crate::network::NetworkManager::post_start(&mut network, holder_pid).await;
                (
                    Self {
                        network,
                        holder,
                        holder_pid,
                        runtime_dir,
                    },
                    started,
                )
            }

            async fn stop(mut self) {
                let _ = crate::network::NetworkManager::cleanup(&mut self.network).await;
                let _ = self.holder.start_kill();
                let _ = self.holder.wait().await;
                crate::companion::forget(self.holder_pid);
                crate::network::netns_agent::forget(self.holder_pid);
                let _ = std::fs::remove_dir_all(&self.runtime_dir);
            }
        }

        fn mapping(
            host_port: u16,
            guest_port: u16,
            proto: crate::network::Protocol,
        ) -> crate::network::PortMapping {
            crate::network::PortMapping {
                host_ip: None,
                host_port,
                guest_port,
                proto,
                loopback_client: false,
            }
        }

        /// pasta attaches to the TAP the holder made for it, so the device is the same one
        /// before pasta, under it and after it: only its carrier follows pasta.
        #[tokio::test]
        async fn pasta_attaches_to_the_holders_tap_and_leaves_it_behind() {
            use crate::network::Protocol;
            let (loopback, port) = free_loopback_port();
            let mappings = vec![
                mapping(port, 80, Protocol::Tcp),
                mapping(port, 53, Protocol::Udp),
            ];
            let (running, started) = Running::start("pasta-attach", &loopback, mappings).await;
            started.expect("starting pasta");

            let under = link_state(running.holder_pid, PASTA_DEVICE_NAME);
            assert!(has_carrier(&under), "pasta is not attached: {under}");
            assert_eq!(under["master"], "br0");
            assert_eq!(under["linkinfo"]["info_data"]["persist"], true);

            let address: std::net::SocketAddr = format!("{loopback}:{port}").parse().unwrap();
            assert!(super::super::listening_on_exactly(address));

            let holder_pid = running.holder_pid;
            let ifindex = under["ifindex"].clone();
            let Running {
                mut network,
                holder,
                runtime_dir,
                ..
            } = running;
            crate::network::NetworkManager::cleanup(&mut network)
                .await
                .expect("stopping pasta");
            let after = link_state(holder_pid, PASTA_DEVICE_NAME);
            assert_eq!(after["ifindex"], ifindex, "pasta's exit replaced its TAP");
            assert_eq!(
                after["master"], "br0",
                "pasta's exit took its TAP off the bridge"
            );
            assert!(
                !has_carrier(&after),
                "pasta0 has carrier with no pasta: {after}"
            );
            assert!(!super::super::listening_on_exactly(address));
            Running {
                network,
                holder,
                holder_pid,
                runtime_dir,
            }
            .stop()
            .await;
        }
    }
}
