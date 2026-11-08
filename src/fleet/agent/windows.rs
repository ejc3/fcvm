//! Windows mode (`--windows N`): N disposable Windows sandboxes on an x86_64 host, each a Cloud
//! Hypervisor clone of the host's gold snapshot with its bridge (docs/fleet/windows-sandboxes.md).
//!
//! The agent drives fcvm through its CLI, one child per VM, and runs one bridge container per
//! sandbox; each child is armed with `PR_SET_PDEATHSIG` ([`Vm::spawn_args`]), so a dead agent
//! leaves no VM or bridge behind.
//!
//! The gold flow runs once per image id when the agent starts, unless the snapshot and the
//! desktop reference it leaves are already there: cold boot `win-gold`, wait until fcvm reports
//! it healthy (Remote Desktop answers and the guest agent acknowledged its clock), log on with
//! the bridge (its `logon` mode, which saves the desktop it reached as the reference and
//! disconnects), wait until the VM's processes use under `--windows-idle-cpu` of one core on
//! average over 20 s, `fcvm snapshot create --stop`. Every sandbox then restores from it at once.
//!
//! One task per sandbox ([`supervise`]) alone starts, watches and stops its VM and bridge. A
//! sandbox relaunches when its desired generation moves (a reset, a borrow ending), and after a
//! failure with a backoff of 1 s doubling to 30 s.

use super::slot::{frame_url, AgentCfg};
use super::vm::Vm;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use fleet_core::protocol::{AgentMsg, AssignSlot, AssignWindows, SharedScene, SlotReport};
use fleet_core::windows::{Gold, WindowsAction, WindowsImage};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// A sandbox VM's size (docs/fleet/windows-sandboxes.md, "A raw VM in fcvm").
pub const CPUS: &str = "2";
pub const MEM_MIB: &str = "4096";
/// Remote Desktop's port in the guest, which fcvm's health probe asks.
pub const HEALTH_PORT: &str = "3389";
/// The gold VM's name.
pub const GOLD_VM: &str = "win-gold";
/// Where the bridge container sees what it is given.
const IN_RDP: &str = "/run/windows/rdp.sock";
const IN_PASSWORD: &str = "/run/windows/sandbox-password";
const IN_REF: &str = "/run/windows/desktop-ref.ppm";
const IN_STATE: &str = "/run/windows/state";
/// The bridge's HTTP port in its container; the host publishes it on the sandbox's port.
const BRIDGE_PORT: u16 = 8080;
/// The standard user the bridge logs on as (fleet/windows/autounattend.xml.in).
const RDP_USER: &str = "sandbox";

/// What the image directory's manifest.json says (fleet/windows/build-image.sh).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Manifest {
    pub id: String,
    pub edition: String,
    pub build: String,
    pub installed_at: String,
    pub eval_expires: String,
}

impl Manifest {
    pub fn read(dir: &Path) -> Result<Manifest> {
        let path = dir.join("manifest.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let m: Manifest = serde_json::from_str(&text)
            .with_context(|| format!("{} is not an image manifest", path.display()))?;
        // The id names the snapshot and a directory: hex, as build-image.sh makes it.
        if m.id.is_empty() || m.id.len() > 64 || !m.id.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{}: id {:?} is not a hex image id", path.display(), m.id);
        }
        m.installed()?;
        m.expires()?;
        Ok(m)
    }

    pub fn installed(&self) -> Result<DateTime<Utc>> {
        parse_time(&self.installed_at).context("manifest installed_at")
    }

    pub fn expires(&self) -> Result<DateTime<Utc>> {
        parse_time(&self.eval_expires).context("manifest eval_expires")
    }
}

fn parse_time(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("{s:?} is not an RFC 3339 time"))?
        .with_timezone(&Utc))
}

/// The image is this old when the agent warns, and when it stops starting sandboxes from it;
/// it also stops this close to the evaluation's recorded expiry.
pub const WARN_DAYS: i64 = 150;
pub const REFUSE_DAYS: i64 = 170;
pub const REFUSE_BEFORE_EXPIRY_DAYS: i64 = 7;

/// The evaluation rules' verdict on an image at `now` (docs/fleet/windows-sandboxes.md,
/// "Edition and licensing"): its age in whole days, a warning from 150 days, and a refusal
/// from 170 days or from 7 days before its expiry, whichever comes first.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub age_days: u64,
    pub warning: Option<String>,
    pub refused: Option<String>,
}

pub fn verdict(installed: DateTime<Utc>, expires: DateTime<Utc>, now: DateTime<Utc>) -> Verdict {
    let age = (now - installed).num_days().max(0);
    let day = |t: DateTime<Utc>| t.format("%Y-%m-%d").to_string();
    let refuse_at = (installed + chrono::Duration::days(REFUSE_DAYS))
        .min(expires - chrono::Duration::days(REFUSE_BEFORE_EXPIRY_DAYS));
    let refused = (now >= refuse_at).then(|| {
        format!(
            "the Windows image installed {} is {age} days old and its evaluation expires {}; no sandbox starts from an image {REFUSE_DAYS} days old or within {REFUSE_BEFORE_EXPIRY_DAYS} days of its expiry (from {}): rebuild it from fresh evaluation media",
            day(installed),
            day(expires),
            day(refuse_at)
        )
    });
    let warning = (age >= WARN_DAYS).then(|| {
        format!(
            "the Windows image installed {} is {age} days old; sandboxes stop starting from it on {}: rebuild it from fresh evaluation media",
            day(installed),
            day(refuse_at)
        )
    });
    Verdict {
        age_days: age as u64,
        warning,
        refused,
    }
}

/// What names one installation of an image: its id, and a hash of its manifest (which holds
/// the installation and expiry dates) and of its sandbox password. The id hashes the build's
/// recipe only, so a rebuild of the same recipe is a new installation with the same id; a
/// gold snapshot of the old one would log on with the wrong password and be aged by the
/// wrong dates.
pub fn gold_key(image_dir: &Path, manifest: &Manifest) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for f in ["manifest.json", "sandbox-password"] {
        let p = image_dir.join(f);
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(format!(
        "{}-{}",
        manifest.id,
        &hex::encode(h.finalize())[..12]
    ))
}

/// The gold snapshot's tag for an image: one per image id, so a new image makes a new one and
/// an agent that restarts reuses the one it made.
pub fn gold_tag(image_id: &str) -> String {
    format!("win-gold-{image_id}")
}

/// The gold desktop the bridge compares a restored sandbox's screen with.
pub fn desktop_ref(state_dir: &Path, image_id: &str) -> PathBuf {
    state_dir.join(image_id).join("desktop-ref.ppm")
}

/// A gold snapshot is reused when its configuration and its desktop reference are both
/// there (fcvm publishes a snapshot's directory with its config.json in it, atomically).
pub fn gold_ready(snapshot_dir: &Path, state_dir: &Path, image_id: &str) -> bool {
    snapshot_dir
        .join(gold_tag(image_id))
        .join("config.json")
        .is_file()
        && desktop_ref(state_dir, image_id).is_file()
}

fn strings(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// `fcvm vm run` of the image: the gold VM's cold boot.
pub fn cold_boot_argv(name: &str, image_dir: &Path) -> Vec<String> {
    strings(&[
        "vm",
        "run",
        "--name",
        name,
        "--firmware",
        &image_dir.join("CLOUDHV.fd").to_string_lossy(),
        "--disk",
        &image_dir.join("disk.raw").to_string_lossy(),
        "--cpu",
        CPUS,
        "--mem",
        MEM_MIB,
        "--guest",
        "windows",
        "--health-port",
        HEALTH_PORT,
    ])
}

/// `fcvm snapshot create` of the gold VM, which stops it once the snapshot is written.
pub fn snapshot_argv(pid: u32, tag: &str) -> Vec<String> {
    strings(&[
        "snapshot",
        "create",
        "--pid",
        &pid.to_string(),
        "--tag",
        tag,
        "--stop",
    ])
}

/// `fcvm snapshot run` of the gold snapshot: one sandbox.
pub fn restore_argv(tag: &str, name: &str) -> Vec<String> {
    strings(&["snapshot", "run", "--snapshot", tag, "--name", name])
}

/// How the bridge is run: logging on for the gold snapshot, or serving a sandbox.
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeMode {
    /// The bridge's `logon` mode: log on, wait for the desktop, save it to `ref_out` (in
    /// `state_dir`), disconnect, exit.
    Logon {
        state_dir: PathBuf,
        ref_out: PathBuf,
    },
    /// The bridge's `serve` mode: connect to the session, stream it, carry out actions.
    Serve {
        desktop_ref: PathBuf,
        /// The sandbox's HTTP port on `publish_ip`, and its TLS port there and on its own
        /// IPv6 address.
        publish_ip: String,
        port: u16,
        tls_port: Option<u16>,
        ipv6: Option<std::net::Ipv6Addr>,
        /// The certificate directory (`--tls-dir`), when the agent has a certificate.
        tls_dir: Option<PathBuf>,
    },
}

/// Every bridge container carries this label, so an agent that starts finds and removes the
/// containers an agent before it left behind: a SIGKILLed agent stops nothing, and podman does
/// not pass SIGKILL from its client on to the container, which keeps its published ports.
pub const BRIDGE_LABEL: &str = "fcvm.fleet.windows-bridge=1";

/// Remove every bridge container a previous agent left (`BRIDGE_LABEL`). Runs before the gold
/// flow, so before any bridge of this agent exists.
pub async fn reclaim_bridges(cfg: &WindowsCfg) {
    let filter = format!("label={BRIDGE_LABEL}");
    let listed = tokio::time::timeout(
        Duration::from_secs(60),
        child(&cfg.podman)
            .args(["ps", "-a", "-q", "--filter", &filter])
            .output(),
    )
    .await;
    let ids: Vec<String> = match listed {
        Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        Ok(Ok(o)) => {
            tracing::warn!(status = %o.status, out = %String::from_utf8_lossy(&o.stderr).trim(), "podman could not list leftover bridges");
            return;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "podman did not start to list leftover bridges");
            return;
        }
        Err(_) => {
            tracing::warn!("podman timed out listing leftover bridges");
            return;
        }
    };
    if ids.is_empty() {
        return;
    }
    tracing::warn!(containers = ?ids, "removing bridge containers a previous agent left");
    let mut rm = vec!["rm".to_string(), "-f".to_string()];
    rm.extend(ids);
    match tokio::time::timeout(
        Duration::from_secs(60),
        child(&cfg.podman).args(&rm).output(),
    )
    .await
    {
        Ok(Ok(o)) if o.status.success() => {}
        Ok(Ok(o)) => {
            tracing::warn!(status = %o.status, out = %String::from_utf8_lossy(&o.stderr).trim(), "podman could not remove leftover bridges")
        }
        Ok(Err(e)) => tracing::warn!(error = %e, "podman did not start to remove leftover bridges"),
        Err(_) => tracing::warn!("podman timed out removing leftover bridges"),
    }
}

/// The bridge container's `podman run` arguments. It reaches Remote Desktop only through the
/// VM's relay socket, reads the password from the image directory, and is given nothing else
/// of the host: no network of its own beyond its published ports.
pub fn bridge_argv(
    name: &str,
    image: &str,
    rdp_socket: &Path,
    password_file: &Path,
    mode: &BridgeMode,
) -> Vec<String> {
    let mut a = strings(&[
        "run",
        "--rm",
        "--replace",
        "--pull=never",
        "--label",
        BRIDGE_LABEL,
        "--name",
        name,
        "--volume",
        &format!("{}:{IN_RDP}", rdp_socket.display()),
        "--volume",
        &format!("{}:{IN_PASSWORD}:ro", password_file.display()),
        "--env",
        &format!("RDP_SOCKET={IN_RDP}"),
        "--env",
        &format!("RDP_USER={RDP_USER}"),
        "--env",
        &format!("RDP_PASSWORD_FILE={IN_PASSWORD}"),
    ]);
    match mode {
        BridgeMode::Logon { state_dir, ref_out } => {
            let file = ref_out
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            a.extend(strings(&[
                "--volume",
                &format!("{}:{IN_STATE}", state_dir.display()),
                "--env",
                &format!("DESKTOP_REF_OUT={IN_STATE}/{file}"),
                image,
                "logon",
            ]));
        }
        BridgeMode::Serve {
            desktop_ref,
            publish_ip,
            port,
            tls_port,
            ipv6,
            tls_dir,
        } => {
            let ip = bracket(publish_ip);
            a.extend(strings(&[
                "--volume",
                &format!("{}:{IN_REF}:ro", desktop_ref.display()),
                "--env",
                &format!("DESKTOP_REF={IN_REF}"),
                "--env",
                &format!("PORT={BRIDGE_PORT}"),
                "--publish",
                &format!("{ip}:{port}:{BRIDGE_PORT}"),
            ]));
            if let Some(dir) = tls_dir {
                use super::tls::{FILE, GUEST_DIR, PORT};
                a.extend(strings(&[
                    "--volume",
                    &format!("{}:{GUEST_DIR}:ro", dir.display()),
                    "--env",
                    &format!("TLS_CERT_FILE={GUEST_DIR}/{FILE}"),
                    "--env",
                    &format!("TLS_PORT={PORT}"),
                ]));
                if let Some(p) = tls_port {
                    a.extend(strings(&["--publish", &format!("{ip}:{p}:{PORT}")]));
                }
                if let Some(v6) = ipv6 {
                    a.extend(strings(&["--publish", &format!("[{v6}]:{PORT}:{PORT}")]));
                }
            }
            // The image's entrypoint is the bridge; its argument is the mode.
            a.extend(strings(&[image, "serve"]));
        }
    }
    a
}

fn bracket(ip: &str) -> String {
    if ip.contains(':') && !ip.starts_with('[') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
}

/// What `fcvm ls --json` says about one VM.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub healthy: bool,
    pub rdp_socket: Option<PathBuf>,
}

/// The entry of `fcvm ls --json` for the VM named `name` whose fcvm process is `pid` (the
/// child the agent started; an older entry of the same name is another process).
pub fn listed(ls_json: &str, name: &str, pid: u32) -> Option<Listed> {
    let v: serde_json::Value = serde_json::from_str(ls_json).ok()?;
    let e = v.as_array()?.iter().find(|e| {
        e.get("name").and_then(|n| n.as_str()) == Some(name)
            && e.get("pid").and_then(|p| p.as_u64()) == Some(u64::from(pid))
    })?;
    Some(Listed {
        healthy: e.get("health_status").and_then(|h| h.as_str()) == Some("healthy")
            && e.pointer("/config/guest").and_then(|g| g.as_str()) == Some("windows"),
        rdp_socket: e
            .pointer("/config/rdp_socket")
            .and_then(|s| s.as_str())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute()),
    })
}

/// The default `--windows-idle-cpu`: over the measured gold desktop's floor of 0.14 to 0.17 of
/// a core, well under the 2 cores of its logon.
pub const DEFAULT_IDLE_CPU: f64 = 0.25;
/// The window the gold VM's average share is taken over.
pub const IDLE_WINDOW: Duration = Duration::from_secs(20);

/// Whether a VM has gone idle: over the last `window` it used under `threshold` of one core on
/// average. An average, not every second: the measured gold desktop sits at 0.14 to 0.17 of a
/// core (its fcvm, the namespace helper and Cloud Hypervisor) with a busy second every half
/// minute or so, which a per-second test never lets through. Fed cumulative CPU seconds; pure,
/// so it is tested on synthetic and measured samples.
#[derive(Debug, Clone)]
pub struct IdleDetector {
    threshold: f64,
    window: f64,
    /// Samples (t, cpu) from the newest one at or before `t - window` on.
    samples: std::collections::VecDeque<(f64, f64)>,
    /// The share of a core over the last window, or since the first sample before one has
    /// passed, for the failure message.
    pub last_share: Option<f64>,
}

impl IdleDetector {
    pub fn new(threshold: f64, window: Duration) -> IdleDetector {
        IdleDetector {
            threshold,
            window: window.as_secs_f64(),
            samples: std::collections::VecDeque::new(),
            last_share: None,
        }
    }

    /// One sample at `t` seconds of `cpu` seconds used so far. Returns whether the VM is idle.
    pub fn sample(&mut self, t: f64, cpu: f64) -> bool {
        if self.samples.back().is_some_and(|&(t1, _)| t <= t1) {
            return false;
        }
        self.samples.push_back((t, cpu));
        while self.samples.len() > 2 && self.samples[1].0 <= t - self.window {
            self.samples.pop_front();
        }
        let (t0, c0) = self.samples[0];
        if t0 == t {
            return false;
        }
        let share = ((cpu - c0) / (t - t0)).max(0.0);
        self.last_share = Some(share);
        t - t0 >= self.window && share < self.threshold
    }
}

/// `pid`, its parent, and the CPU time it used (utime + stime, clock ticks) from one
/// `/proc/<pid>/stat` line. The command name is in parentheses and may hold anything, so the
/// fields are counted after its last `)`.
pub fn parse_stat(line: &str) -> Option<(u32, u32, u64)> {
    let open = line.find('(')?;
    let pid = line[..open].trim().parse().ok()?;
    let rest = &line[line.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // f[0] is the state (field 3): the parent is field 4, utime and stime fields 14 and 15.
    let ppid = f.get(1)?.parse().ok()?;
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some((pid, ppid, utime + stime))
}

/// CPU seconds used so far by `root` and every process below it: the VMM fcvm starts, its
/// namespace helpers, and fcvm itself.
pub fn tree_cpu_secs(proc_root: &Path, root: u32) -> Option<f64> {
    let mut procs: Vec<(u32, u32, u64)> = Vec::new();
    for e in std::fs::read_dir(proc_root).ok()?.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Some(p) = std::fs::read_to_string(e.path().join("stat"))
            .ok()
            .and_then(|l| parse_stat(&l))
        {
            procs.push(p);
        }
    }
    if !procs.iter().any(|p| p.0 == root) {
        return None;
    }
    let mut tree = std::collections::BTreeSet::from([root]);
    loop {
        let before = tree.len();
        for (pid, ppid, _) in &procs {
            if tree.contains(ppid) {
                tree.insert(*pid);
            }
        }
        if tree.len() == before {
            break;
        }
    }
    let ticks: u64 = procs
        .iter()
        .filter(|p| tree.contains(&p.0))
        .map(|p| p.2)
        .sum();
    // SAFETY: sysconf reads a constant.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    Some(ticks as f64 / hz)
}

/// The actions this agent's sandboxes carry out, for its hello: every action of this build
/// but `reset`, which the coordinator turns into a relaunch.
pub fn actions() -> Vec<String> {
    WindowsAction::ALL
        .into_iter()
        .filter(|a| *a != WindowsAction::Reset)
        .map(|a| a.as_str().to_string())
        .collect()
}

/// The bridge's `POST /action` body for an action; `None` for `reset` and for an action this
/// build does not know. The name is one of the enumerated ones, never text from a viewer.
pub fn action_body(action: WindowsAction) -> Option<serde_json::Value> {
    match action {
        WindowsAction::Reset | WindowsAction::Unknown => None,
        a => Some(serde_json::json!({"name": a.as_str()})),
    }
}

/// The bridge's times (`/health` `windows.times_unix_ms`), host Unix ms.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BridgeTimes {
    pub rdp: Option<u64>,
    pub first_frame: Option<u64>,
    pub desktop: Option<u64>,
}

/// The bridge's state and times from its `/health` reply.
pub fn bridge_state(health: &serde_json::Value) -> (Option<&str>, BridgeTimes) {
    let t = |k: &str| {
        health
            .pointer(&format!("/windows/times_unix_ms/{k}"))
            .and_then(|v| v.as_u64())
    };
    (
        health.pointer("/windows/state").and_then(|s| s.as_str()),
        BridgeTimes {
            rdp: t("rdp"),
            first_frame: t("first_frame"),
            desktop: t("desktop"),
        },
    )
}

/// A launch's timings from the agent's clock and the bridge's (both the host's): the restore
/// to healthy, healthy to Remote Desktop connected, to its first frame, to the desktop, the
/// whole relaunch, and from the coordinator's click to the desktop.
pub fn launch_timings(
    teardown_ms: u64,
    restore_ms: u64,
    healthy_at: u64,
    started_at: u64,
    clicked_at: Option<u64>,
    b: BridgeTimes,
) -> BTreeMap<String, u64> {
    let mut t = BTreeMap::from([
        ("teardown_ms".to_string(), teardown_ms),
        ("restore_ms".to_string(), restore_ms),
    ]);
    let span = |a: Option<u64>, b: Option<u64>| Some(b?.saturating_sub(a?));
    let mut put = |k: &str, v: Option<u64>| {
        if let Some(v) = v {
            t.insert(k.to_string(), v);
        }
    };
    put("connect_ms", span(Some(healthy_at), b.rdp));
    put("first_frame_ms", span(b.rdp, b.first_frame));
    put("desktop_ms", span(b.first_frame, b.desktop));
    put("total_ms", span(Some(started_at), b.desktop));
    put(
        "click_to_desktop_ms",
        span(
            clicked_at.filter(|c| b.desktop.is_some_and(|d| *c <= d)),
            b.desktop,
        ),
    );
    t
}

/// How the agent runs its sandboxes.
#[derive(Debug, Clone)]
pub struct WindowsCfg {
    pub count: u32,
    pub image_dir: PathBuf,
    pub manifest: Manifest,
    pub bridge_image: String,
    pub podman: PathBuf,
    pub state_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub proc_root: PathBuf,
    /// Idle: under this share of one core for `idle_for`.
    pub idle_cpu: f64,
    pub idle_for: Duration,
    pub gold_healthy_deadline: Duration,
    pub logon_deadline: Duration,
    pub idle_deadline: Duration,
    pub snapshot_deadline: Duration,
    /// A sandbox's restore to its desktop.
    pub start_timeout: Duration,
    pub poll: Duration,
    /// The certificate directory, when the agent has a certificate (`--tls-secret`).
    pub tls_dir: Option<PathBuf>,
    /// The installation the gold snapshot is made of and reused for ([`gold_key`]).
    pub gold_key: String,
    /// How often a running sandbox checks that its image may still run.
    pub image_check: Duration,
    /// `podman stop -t`: how long a bridge container has to stop on SIGTERM.
    pub bridge_stop: Duration,
}

impl WindowsCfg {
    fn password(&self) -> PathBuf {
        self.image_dir.join("sandbox-password")
    }

    fn image(&self, gold: &GoldState, now: DateTime<Utc>) -> WindowsImage {
        let m = &self.manifest;
        let v = match (m.installed(), m.expires()) {
            (Ok(i), Ok(e)) => verdict(i, e, now),
            _ => Verdict {
                age_days: 0,
                warning: None,
                refused: Some("the image's manifest has no valid dates".into()),
            },
        };
        let (gold, gold_error) = match gold {
            GoldState::Building => (Gold::Building, None),
            GoldState::Ready(_) => (Gold::Ready, None),
            GoldState::Failed(e) => (Gold::Failed, Some(e.clone())),
        };
        WindowsImage {
            id: m.id.clone(),
            edition: m.edition.clone(),
            build: m.build.clone(),
            installed_at: m.installed_at.clone(),
            eval_expires: m.eval_expires.clone(),
            age_days: v.age_days,
            warning: v.warning,
            refused: v.refused,
            gold,
            gold_error,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GoldState {
    Building,
    /// The snapshot's tag.
    Ready(String),
    Failed(String),
}

/// The ports and address of each sandbox, by its index on the host.
#[derive(Debug, Clone, Copy)]
pub struct Place {
    pub port: u16,
    pub ipv6: Option<std::net::Ipv6Addr>,
    pub tls_port: Option<u16>,
}

/// What a sandbox should be running.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub generation: u64,
    pub clicked_at: Option<u64>,
    pub actions: Vec<fleet_core::windows::WindowsActionReq>,
}

impl Desired {
    fn of(generation: u64, w: &AssignWindows) -> Desired {
        Desired {
            generation,
            clicked_at: w.clicked_at,
            actions: w.actions.clone(),
        }
    }
}

struct Handle {
    index: u32,
    desired: watch::Sender<Desired>,
    task: JoinHandle<()>,
}

/// The host's sandboxes: the gold flow and one task per assigned sandbox.
pub struct Windows {
    cfg: Arc<WindowsCfg>,
    agent: Arc<AgentCfg>,
    places: Vec<Place>,
    out: mpsc::UnboundedSender<AgentMsg>,
    http: reqwest::Client,
    gold: watch::Receiver<GoldState>,
    /// The wall's shared scene: its clock and admission keys, which every bridge is given.
    scene: watch::Receiver<Option<SharedScene>>,
    sandboxes: BTreeMap<String, Handle>,
    /// Supervisors told to stop, by the host index they held: a new sandbox on that index
    /// starts only once its old one has stopped.
    retired: Vec<(u32, JoinHandle<()>)>,
    /// Tells the gold flow the agent is stopping.
    stop: watch::Sender<bool>,
    flow: JoinHandle<()>,
    reports: JoinHandle<()>,
}

impl Windows {
    /// Start the gold flow and the image reports; sandboxes start as they are assigned.
    pub fn start(
        cfg: Arc<WindowsCfg>,
        agent: Arc<AgentCfg>,
        places: Vec<Place>,
        out: mpsc::UnboundedSender<AgentMsg>,
        http: reqwest::Client,
        scene: watch::Receiver<Option<SharedScene>>,
    ) -> Windows {
        let (gold_tx, gold) = watch::channel(GoldState::Building);
        let (stop, stop_rx) = watch::channel(false);
        let flow = {
            let cfg = cfg.clone();
            let agent = agent.clone();
            tokio::spawn(async move {
                reclaim_bridges(&cfg).await;
                let r = gold_flow(&cfg, &agent, stop_rx.clone()).await;
                let state = match r {
                    Ok(tag) => {
                        tracing::info!(%tag, "the gold snapshot is ready");
                        GoldState::Ready(tag)
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        tracing::error!(error = %e, "the gold flow failed; no sandbox starts");
                        GoldState::Failed(e)
                    }
                };
                gold_tx.send_replace(state);
                // Kept until the agent stops, so the receivers see the outcome, not a close.
                tokio::select! {
                    _ = gold_tx.closed() => {}
                    _ = stopping(stop_rx) => {}
                }
            })
        };
        let reports = {
            let cfg = cfg.clone();
            let out = out.clone();
            let mut gold = gold.clone();
            tokio::spawn(async move {
                let mut sent: Option<WindowsImage> = None;
                let mut tick = tokio::time::interval(Duration::from_secs(60));
                loop {
                    let image = cfg.image(&gold.borrow_and_update().clone(), Utc::now());
                    if sent.as_ref() != Some(&image) {
                        if let Some(w) = &image.warning {
                            tracing::warn!(warning = %w, "the Windows image is old");
                        }
                        if out.send(AgentMsg::WindowsImage(image.clone())).is_err() {
                            return;
                        }
                        sent = Some(image);
                    }
                    tokio::select! {
                        _ = tick.tick() => {}
                        r = gold.changed() => if r.is_err() { return },
                    }
                }
            })
        };
        Windows {
            cfg,
            agent,
            places,
            out,
            http,
            gold,
            scene,
            sandboxes: BTreeMap::new(),
            retired: Vec::new(),
            stop,
            flow,
            reports,
        }
    }

    /// Reconcile to the assignment's sandboxes: start the new ones, tell the others their
    /// desired generation and actions, stop those no longer listed.
    pub fn assign(&mut self, list: &[AssignSlot]) {
        let keep: HashMap<&str, u32> = list
            .iter()
            .filter_map(|a| Some((a.id.as_str(), a.windows.as_ref()?.index)))
            .collect();
        let gone: Vec<String> = self
            .sandboxes
            .iter()
            .filter(|(id, h)| keep.get(id.as_str()) != Some(&h.index))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            let h = self.sandboxes.remove(&id).unwrap();
            drop(h.desired);
            self.retired.push((h.index, h.task));
        }
        self.retired.retain(|(_, t)| !t.is_finished());
        for a in list {
            let Some(w) = &a.windows else { continue };
            let want = Desired::of(a.generation, w);
            if let Some(h) = self.sandboxes.get(&a.id) {
                h.desired.send_if_modified(|d| {
                    // Generations only grow; an older assignment never moves a sandbox back.
                    let next = Desired {
                        generation: d.generation.max(want.generation),
                        ..want.clone()
                    };
                    let changed = *d != next;
                    *d = next;
                    changed
                });
                continue;
            }
            let Some(place) = self.places.get(w.index as usize).copied() else {
                tracing::warn!(sandbox = %a.id, index = w.index, "assigned a sandbox this host does not run");
                continue;
            };
            let (tx, rx) = watch::channel(want);
            let ctx = Sandbox {
                id: a.id.clone(),
                place,
                cfg: self.cfg.clone(),
                agent: self.agent.clone(),
                out: self.out.clone(),
                http: self.http.clone(),
                scene: self.scene.clone(),
            };
            // The index's ports and address are free once its old sandbox has stopped.
            let (before, kept) = std::mem::take(&mut self.retired)
                .into_iter()
                .partition(|(i, _)| *i == w.index);
            self.retired = kept;
            let before = before.into_iter().map(|(_, t)| t).collect();
            let task = tokio::spawn(supervise(ctx, rx, self.gold.clone(), before));
            self.sandboxes.insert(
                a.id.clone(),
                Handle {
                    index: w.index,
                    desired: tx,
                    task,
                },
            );
        }
    }

    /// Stop every sandbox and wait until each VM and bridge has exited; a gold flow in
    /// progress stops with its VM.
    pub async fn stop(mut self) {
        self.stop.send_replace(true);
        for (_, h) in std::mem::take(&mut self.sandboxes) {
            drop(h.desired);
            self.retired.push((h.index, h.task));
        }
        for (_, t) in self.retired {
            let _ = t.await;
        }
        // The gold flow stops its VM and bridge itself; aborted only past its budget.
        let budget = teardown_budget(&self.cfg, &self.agent);
        let mut flow = self.flow;
        if tokio::time::timeout(budget, &mut flow).await.is_err() {
            tracing::warn!(
                "the gold flow did not stop in {} s; aborting it",
                budget.as_secs()
            );
            flow.abort();
            let _ = flow.await;
        }
        self.reports.abort();
        let _ = self.reports.await;
    }
}

/// A command for a child that is not a VM or a bridge (`fcvm ls`, `fcvm snapshot create`,
/// `podman stop` and `rm`), armed with PR_SET_PDEATHSIG as every VM is, so none outlives a
/// killed agent.
fn child(program: &Path) -> tokio::process::Command {
    let mut c = std::process::Command::new(program);
    c.stdin(std::process::Stdio::null());
    super::vm::arm_pdeathsig(&mut c);
    let mut c = tokio::process::Command::from(c);
    c.kill_on_drop(true);
    c
}

/// Run `fcvm ls --json` and find the VM.
async fn ls(fcvm: &Path, name: &str, pid: u32) -> Option<Listed> {
    let run = child(fcvm).args(["ls", "--json"]).output();
    let out = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    listed(&String::from_utf8_lossy(&out.stdout), name, pid)
}

/// Wait until fcvm reports the VM healthy with its RDP relay socket; the socket's path.
async fn wait_healthy(
    fcvm: &Path,
    vm: &mut Vm,
    name: &str,
    deadline: Duration,
    poll: Duration,
) -> Result<PathBuf> {
    let until = tokio::time::sleep(deadline);
    tokio::pin!(until);
    let mut tick = tokio::time::interval(poll);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Some(Listed { healthy: true, rdp_socket: Some(s) }) = ls(fcvm, name, vm.pid).await {
                    return Ok(s);
                }
            }
            status = vm.exited() => bail!("fcvm exited ({status}) before {name} was healthy; fcvm said: {}", vm.tail()),
            _ = &mut until => bail!("{name} was not healthy {} s after fcvm started; fcvm said: {}", deadline.as_secs(), vm.tail()),
        }
    }
}

/// Make (or reuse) the gold snapshot. Every way out, a failure, success and the agent
/// stopping included, stops the gold VM and the logon bridge and waits for both, escalating
/// to SIGKILL, so none is left running unsupervised.
async fn gold_flow(
    cfg: &WindowsCfg,
    agent: &AgentCfg,
    stop: watch::Receiver<bool>,
) -> Result<String> {
    let tag = gold_tag(&cfg.gold_key);
    if gold_ready(&cfg.snapshot_dir, &cfg.state_dir, &cfg.gold_key) {
        tracing::info!(%tag, "reusing the gold snapshot");
        return Ok(tag);
    }
    let mut kids = GoldChildren::default();
    let r = tokio::select! {
        r = gold_steps(cfg, agent, &tag, &mut kids) => r,
        _ = stopping(stop) => Err(anyhow::anyhow!("the agent is stopping")),
    };
    if let Some(b) = kids.bridge.as_mut() {
        b.stop(cfg).await;
    }
    if let Some(vm) = kids.vm.as_mut() {
        vm.stop(agent.stop_grace).await;
    }
    r
}

async fn gold_steps(
    cfg: &WindowsCfg,
    agent: &AgentCfg,
    tag: &str,
    kids: &mut GoldChildren,
) -> Result<String> {
    let id = &cfg.gold_key;
    let tag = tag.to_string();
    let dir = cfg.state_dir.join(id);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let ref_out = desktop_ref(&cfg.state_dir, id);
    let _ = std::fs::remove_file(&ref_out);
    let t0 = Instant::now();
    tracing::info!(%tag, image = %cfg.image_dir.display(), "making the gold snapshot");
    let vm = kids.vm.insert(Vm::spawn_args(
        &agent.fcvm,
        &cold_boot_argv(GOLD_VM, &cfg.image_dir),
        GOLD_VM,
    )?);
    let rdp = wait_healthy(
        &agent.fcvm,
        vm,
        GOLD_VM,
        cfg.gold_healthy_deadline,
        cfg.poll,
    )
    .await?;
    tracing::info!(
        secs = t0.elapsed().as_secs(),
        "the gold VM is healthy; logging on"
    );
    let bridge = kids.bridge.insert(Bridge::spawn(
        cfg,
        "fleet-win-gold-bridge",
        &rdp,
        &BridgeMode::Logon {
            state_dir: dir.clone(),
            ref_out: ref_out.clone(),
        },
    )?);
    tokio::select! {
        status = bridge.client.exited() => {
            if !status.success() {
                bail!("the bridge's logon failed ({status}): {}", bridge.client.tail());
            }
        }
        status = vm.exited() => bail!("fcvm exited ({status}) during the logon; fcvm said: {}", vm.tail()),
        _ = tokio::time::sleep(cfg.logon_deadline) => {
            bail!("the bridge did not reach the desktop in {} s: {}", cfg.logon_deadline.as_secs(), bridge.client.tail());
        }
    }
    // It exited 0: its container is gone (`--rm`).
    kids.bridge = None;
    if !ref_out.is_file() {
        bail!(
            "the bridge's logon saved no desktop reference at {}",
            ref_out.display()
        );
    }
    tracing::info!(
        secs = t0.elapsed().as_secs(),
        "logged on; waiting for the desktop to go idle"
    );
    let mut idle = IdleDetector::new(cfg.idle_cpu, cfg.idle_for);
    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            status = vm.exited() => bail!("fcvm exited ({status}) while the desktop settled; fcvm said: {}", vm.tail()),
        }
        let root = cfg.proc_root.clone();
        let pid = vm.pid;
        let cpu = tokio::task::spawn_blocking(move || tree_cpu_secs(&root, pid))
            .await
            .ok()
            .flatten();
        if let Some(cpu) = cpu {
            if idle.sample(started.elapsed().as_secs_f64(), cpu) {
                break;
            }
        }
        if started.elapsed() >= cfg.idle_deadline {
            bail!(
                "the gold desktop did not go idle in {} s: its VM used {} of a core over the last {} s (idle is under {})",
                cfg.idle_deadline.as_secs(),
                idle.last_share.map_or("an unknown share".into(), |s| format!("{s:.2}")),
                cfg.idle_for.as_secs(),
                cfg.idle_cpu
            );
        }
    }
    tracing::info!(
        secs = t0.elapsed().as_secs(),
        "the desktop is idle; taking the snapshot"
    );
    let run = child(&agent.fcvm)
        .args(snapshot_argv(vm.pid, &tag))
        .output();
    let out = tokio::time::timeout(cfg.snapshot_deadline, run)
        .await
        .with_context(|| {
            format!(
                "fcvm snapshot create took over {} s",
                cfg.snapshot_deadline.as_secs()
            )
        })?
        .context("running fcvm snapshot create")?;
    if !out.status.success() {
        bail!(
            "fcvm snapshot create failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // `--stop` ends the gold VM once the snapshot is written.
    vm.stop(agent.stop_grace).await;
    if !gold_ready(&cfg.snapshot_dir, &cfg.state_dir, id) {
        bail!(
            "fcvm snapshot create succeeded but {} has no config.json",
            cfg.snapshot_dir.join(&tag).display()
        );
    }
    tracing::info!(%tag, secs = t0.elapsed().as_secs(), "gold snapshot made");
    Ok(tag)
}

/// A bridge container and its `podman run` client. The container is podman's (conmon keeps
/// it), so killing the client does not stop it: it is stopped through podman by its name, one
/// of its own per launch.
struct Bridge {
    client: Vm,
    name: String,
}

impl Bridge {
    fn spawn(cfg: &WindowsCfg, prefix: &str, rdp: &Path, mode: &BridgeMode) -> Result<Bridge> {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("{prefix}-{}", &nonce[..8]);
        let argv = bridge_argv(&name, &cfg.bridge_image, rdp, &cfg.password(), mode);
        Ok(Bridge {
            client: Vm::spawn_args(&cfg.podman, &argv, &name)?,
            name,
        })
    }

    /// `podman stop -t <bridge_stop>` (SIGTERM, then SIGKILL), `podman rm -f`, then the client
    /// is awaited, and killed if it still has not exited. Returns once the client is reaped.
    async fn stop(&mut self, cfg: &WindowsCfg) {
        let secs = cfg.bridge_stop.as_secs().max(1).to_string();
        let budget = cfg.bridge_stop + PODMAN_SLACK;
        for args in [
            vec!["stop", "-t", secs.as_str(), self.name.as_str()],
            vec!["rm", "-f", self.name.as_str()],
        ] {
            let run = child(&cfg.podman).args(&args).output();
            match tokio::time::timeout(budget, run).await {
                Ok(Ok(o)) if o.status.success() => {}
                Ok(Ok(o)) => {
                    tracing::debug!(bridge = %self.name, cmd = args[0], status = %o.status, out = %String::from_utf8_lossy(&o.stderr).trim(), "podman refused")
                }
                Ok(Err(e)) => {
                    tracing::warn!(bridge = %self.name, cmd = args[0], error = %e, "podman did not start")
                }
                Err(_) => {
                    tracing::warn!(bridge = %self.name, cmd = args[0], "podman timed out")
                }
            }
        }
        self.client.stop(CLIENT_GRACE).await;
    }
}

/// How much longer than its own `-t` a podman command may take, and how long the `podman run`
/// client has to exit once its container is gone.
const PODMAN_SLACK: Duration = Duration::from_secs(20);
const CLIENT_GRACE: Duration = Duration::from_secs(5);

/// How long reaping a child takes once it is sent SIGKILL.
const KILL_REAP: Duration = Duration::from_secs(10);

/// How long a sandbox's teardown may take: the sum of its steps' bounds, from the constants
/// the steps use (`Sandbox::stop`: the bridge's podman stop and rm, its client, then the VM's
/// SIGTERM grace, each escalating to SIGKILL), and some for reaping. A replacement, or the agent
/// stopping, waits this long before it gives up on a teardown.
fn teardown_budget(cfg: &WindowsCfg, agent: &AgentCfg) -> Duration {
    (cfg.bridge_stop + PODMAN_SLACK) * 2 + CLIENT_GRACE + agent.stop_grace + KILL_REAP * 2
}

/// What the gold flow has started, stopped and awaited on every way out of it.
#[derive(Default)]
struct GoldChildren {
    vm: Option<Vm>,
    bridge: Option<Bridge>,
}

/// Resolves once the agent is stopping (or nothing can tell it to any more).
async fn stopping(mut stop: watch::Receiver<bool>) {
    while !*stop.borrow_and_update() {
        if stop.changed().await.is_err() {
            return;
        }
    }
}

struct Sandbox {
    id: String,
    place: Place,
    cfg: Arc<WindowsCfg>,
    agent: Arc<AgentCfg>,
    out: mpsc::UnboundedSender<AgentMsg>,
    http: reqwest::Client,
    scene: watch::Receiver<Option<SharedScene>>,
}

struct Running {
    vm: Vm,
    bridge: Bridge,
    generation: u64,
    started: Instant,
    actions_done: u64,
    /// The scene the bridge holds: without one a bridge with a certificate serves no frames,
    /// and its admission keys are the ones a viewer's direct token is checked against.
    scene: Option<BridgeScene>,
}

/// What a bridge takes of the shared scene (`POST /scene`): the wall's clock and the
/// admission keys.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BridgeScene {
    pub epoch_unix_ms: u64,
    pub admit_keys: Vec<String>,
}

impl BridgeScene {
    pub fn of(s: &SharedScene) -> BridgeScene {
        BridgeScene {
            epoch_unix_ms: s.epoch_unix_ms,
            admit_keys: s.admit_keys.clone(),
        }
    }
}

impl Sandbox {
    fn report(
        &self,
        phase: &str,
        generation: u64,
        timings: BTreeMap<String, u64>,
        error: Option<String>,
        retry_in: Option<Duration>,
        desktop_at: Option<u64>,
    ) {
        let c = &self.agent;
        let _ = self.out.send(AgentMsg::Slot(SlotReport {
            id: self.id.clone(),
            phase: phase.into(),
            generation,
            vm: Some(self.id.clone()),
            frame_url: Some(self.frame_url()),
            timings,
            error,
            retry_in_ms: retry_in.map(|d| d.as_millis() as u64),
            ipv6: self.place.ipv6.map(|a| a.to_string()),
            ipv4: self.place.tls_port.and(c.public_ipv4.clone()),
            v4_port: c.public_ipv4.as_ref().and(self.place.tls_port),
            place: c.place.clone(),
            desktop_at,
        }));
    }

    /// The bridge's frame WebSocket as a browser opens it, as a slot's renderer's is.
    fn frame_url(&self) -> String {
        match &self.agent.frame_zone {
            Some(zone) => match self.place.tls_port.filter(|_| self.place.ipv6.is_none()) {
                Some(p) => format!("wss://{}.{zone}:{p}/frames", self.id),
                None => format!("wss://{}.{zone}/frames", self.id),
            },
            None => frame_url(&self.agent.frame_host, self.place.port),
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "http://{}:{}{path}",
            self.agent.health_host(),
            self.place.port
        )
    }

    /// Give the bridge the current scene when it holds another; what it holds afterwards.
    async fn sync_scene(&self, held: Option<BridgeScene>) -> Option<BridgeScene> {
        let Some(want) = self.scene.borrow().as_ref().map(BridgeScene::of) else {
            return held;
        };
        if held.as_ref() == Some(&want) {
            return held;
        }
        let sent = self
            .agent
            .scene_token
            .authorize(self.http.post(self.url("/scene")))
            .timeout(Duration::from_secs(5))
            .json(&want)
            .send()
            .await;
        match sent {
            Ok(r) if r.status().is_success() => Some(want),
            Ok(r) => {
                tracing::warn!(sandbox = %self.id, status = %r.status(), "the bridge refused the scene");
                held
            }
            Err(e) => {
                tracing::debug!(sandbox = %self.id, error = %e, "the scene did not reach the bridge");
                held
            }
        }
    }

    async fn health(&self) -> Option<serde_json::Value> {
        let r = self
            .http
            .get(self.url("/health"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json().await.ok()
    }

    async fn stop(&self, r: &mut Running) {
        r.bridge.stop(&self.cfg).await;
        r.vm.stop(self.agent.stop_grace).await;
    }

    /// Post each action of the running launch not yet carried out to the bridge, oldest
    /// first, each once. A failed one is logged and not tried again.
    /// The next action of the running launch not yet carried out, marked carried out.
    fn next_action(
        r: &mut Running,
        desired: &Desired,
    ) -> Option<fleet_core::windows::WindowsActionReq> {
        let a = desired
            .actions
            .iter()
            .find(|a| a.seq > r.actions_done && a.generation == r.generation)?
            .clone();
        r.actions_done = a.seq;
        Some(a)
    }

    /// The request that carries out `action` on the bridge, or none for `reset` and for an
    /// action this build does not know.
    fn post_action(&self, action: WindowsAction) -> Option<Sending> {
        let body = action_body(action)?;
        Some(Box::pin(
            self.agent
                .scene_token
                .authorize(self.http.post(self.url("/action")))
                .timeout(ACTION_TIMEOUT)
                .json(&body)
                .send(),
        ))
    }

    fn log_action(
        &self,
        a: &fleet_core::windows::WindowsActionReq,
        sent: reqwest::Result<reqwest::Response>,
    ) {
        let (action, seq) = (a.action.as_str(), a.seq);
        match sent {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(sandbox = %self.id, action, seq, "sandbox action done")
            }
            Ok(resp) => {
                tracing::warn!(sandbox = %self.id, action, seq, status = %resp.status(), "sandbox action refused by the bridge")
            }
            Err(e) => {
                tracing::warn!(sandbox = %self.id, action, seq, error = %e, "sandbox action not delivered")
            }
        }
    }

    async fn relaunch(
        &self,
        running: &mut Option<Running>,
        tag: &str,
        want: &Desired,
        desired: &mut watch::Receiver<Desired>,
    ) -> std::result::Result<Running, Option<String>> {
        let generation = want.generation;
        let t0 = Instant::now();
        let started_at = unix_ms();
        if let Some(mut old) = running.take() {
            self.report("stopping", generation, BTreeMap::new(), None, None, None);
            self.stop(&mut old).await;
        }
        let teardown_ms = t0.elapsed().as_millis() as u64;
        self.report("restoring", generation, BTreeMap::new(), None, None, None);
        let fcvm = &self.agent.fcvm;
        let mut vm = Vm::spawn_args(fcvm, &restore_argv(tag, &self.id), &self.id)
            .map_err(|e| Some(format!("{e:#}")))?;
        let spawned = Instant::now();
        let deadline = tokio::time::sleep(self.cfg.start_timeout);
        tokio::pin!(deadline);
        let healthy = tokio::select! {
            r = wait_healthy(fcvm, &mut vm, &self.id, self.cfg.start_timeout, self.cfg.poll) => r,
            _ = removed(desired) => {
                vm.stop(self.agent.stop_grace).await;
                return Err(None);
            }
        };
        let rdp = match healthy {
            Ok(s) => s,
            Err(e) => {
                vm.stop(self.agent.stop_grace).await;
                return Err(Some(format!("{e:#}")));
            }
        };
        let restore_ms = spawned.elapsed().as_millis() as u64;
        let healthy_at = unix_ms();
        let partial = BTreeMap::from([
            ("teardown_ms".to_string(), teardown_ms),
            ("restore_ms".to_string(), restore_ms),
        ]);
        self.report("connecting", generation, partial, None, None, None);
        if let Some(a) = self.place.ipv6.filter(|_| self.cfg.tls_dir.is_some()) {
            if let Err(e) = super::addr::add_host_address(a).await {
                vm.stop(self.agent.stop_grace).await;
                return Err(Some(format!("{e:#}")));
            }
        }
        let mode = BridgeMode::Serve {
            desktop_ref: desktop_ref(&self.cfg.state_dir, &self.cfg.gold_key),
            publish_ip: self.agent.publish_ip.clone(),
            port: self.place.port,
            tls_port: self.place.tls_port,
            ipv6: self.place.ipv6,
            tls_dir: self.cfg.tls_dir.clone(),
        };
        let prefix = format!("fleet-{}-bridge-g{generation}", self.id);
        let bridge = match Bridge::spawn(&self.cfg, &prefix, &rdp, &mode) {
            Ok(b) => b,
            Err(e) => {
                vm.stop(self.agent.stop_grace).await;
                return Err(Some(format!("{e:#}")));
            }
        };
        let mut r = Running {
            vm,
            bridge,
            generation,
            started: spawned,
            actions_done: 0,
            scene: None,
        };
        let mut tick = tokio::time::interval(self.cfg.poll);
        let fail = |why: String, r: &Running| {
            Some(format!(
                "{why}; fcvm said: {} | the bridge said: {}",
                r.vm.tail(),
                r.bridge.client.tail()
            ))
        };
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let Some(h) = self.health().await else { continue };
                    r.scene = self.sync_scene(r.scene.take()).await;
                    let (state, times) = bridge_state(&h);
                    if state == Some("failed") {
                        let why = h.pointer("/windows/error").and_then(|e| e.as_str()).unwrap_or("no reason given").to_string();
                        let e = fail(format!("the bridge failed: {why}"), &r);
                        self.stop(&mut r).await;
                        return Err(e);
                    }
                    // A bridge with a certificate serves frames only once it has a scene, so the
                    // sandbox runs once its bridge holds one (or, without a certificate, when
                    // there is none yet).
                    let scene_ok = r.scene.is_some()
                        || (self.cfg.tls_dir.is_none() && self.scene.borrow().is_none());
                    if state == Some("desktop") && scene_ok {
                        if let Some(desktop) = times.desktop {
                            let timings = launch_timings(teardown_ms, restore_ms, healthy_at, started_at, want.clicked_at, times);
                            self.report("running", generation, timings, None, None, Some(desktop));
                            return Ok(r);
                        }
                    }
                }
                status = r.vm.exited() => {
                    let e = fail(format!("fcvm exited ({status}) before the desktop"), &r);
                    self.stop(&mut r).await;
                    return Err(e);
                }
                status = r.bridge.client.exited() => {
                    // Its client is gone, its container may not be: stopped through podman too.
                    let e = fail(format!("the bridge exited ({status}) before the desktop"), &r);
                    self.stop(&mut r).await;
                    return Err(e);
                }
                _ = &mut deadline => {
                    let e = fail(format!("no desktop {} s after fcvm started", self.cfg.start_timeout.as_secs()), &r);
                    self.stop(&mut r).await;
                    return Err(e);
                }
                _ = removed(desired) => {
                    self.stop(&mut r).await;
                    return Err(None);
                }
            }
        }
    }
}

/// How long the bridge may take to carry out one action.
const ACTION_TIMEOUT: Duration = Duration::from_secs(20);
/// A refused image is checked again this often (an agent restarted with a new image starts
/// its sandboxes at once; this one only reports).
const REFUSED_RECHECK: Duration = Duration::from_secs(3600);

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// An action's request to the bridge.
type Sending =
    std::pin::Pin<Box<dyn std::future::Future<Output = reqwest::Result<reqwest::Response>> + Send>>;

/// The action in flight's outcome; pending forever with none in flight.
async fn in_flight(
    f: &mut Option<(fleet_core::windows::WindowsActionReq, Sending)>,
) -> (
    fleet_core::windows::WindowsActionReq,
    reqwest::Result<reqwest::Response>,
) {
    match f {
        Some((a, send)) => (a.clone(), send.await),
        None => std::future::pending().await,
    }
}

/// Resolves when the sandbox's desired-state sender is dropped (it was unassigned).
async fn removed(desired: &mut watch::Receiver<Desired>) {
    while desired.changed().await.is_ok() {}
}

async fn supervise(
    ctx: Sandbox,
    mut desired: watch::Receiver<Desired>,
    mut gold: watch::Receiver<GoldState>,
    before: Vec<JoinHandle<()>>,
) {
    // The sandbox that held this index stops its VM and bridge first (each stop is bounded and
    // escalates to SIGKILL); one that overruns that is aborted.
    let budget = teardown_budget(&ctx.cfg, &ctx.agent);
    for mut old in before {
        if tokio::time::timeout(budget, &mut old).await.is_err() {
            tracing::warn!(sandbox = %ctx.id, "the index's previous sandbox did not stop in {} s; aborting it", budget.as_secs());
            old.abort();
            let _ = old.await;
        }
    }
    let mut running: Option<Running> = None;
    // A running sandbox stops once its image may no longer run (day 170, or a week before
    // the evaluation expires), not only at its next relaunch.
    let mut image_tick = tokio::time::interval(ctx.cfg.image_check);
    image_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The action being carried out: one event among the others below, so a reset, the
    // sandbox's removal or the cutoff is never held back by it, and drops it.
    let mut action: Option<(fleet_core::windows::WindowsActionReq, Sending)> = None;
    let mut backoff = ctx.agent.backoff_min;
    let mut retry_after: Option<Duration> = None;
    let mut health_tick = tokio::time::interval(ctx.agent.health_interval);
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The scene's changes (new admission keys) reach the running bridge as they come.
    let mut scene = ctx.scene.clone();
    let mut scene_open = true;
    loop {
        let want = desired.borrow_and_update().clone();
        let behind = running
            .as_ref()
            .is_none_or(|r| r.generation < want.generation);
        if behind {
            action = None;
            // The gold snapshot first: every sandbox restores from it.
            let tag = loop {
                let g = gold.borrow_and_update().clone();
                match g {
                    GoldState::Ready(tag) => break Some(tag),
                    GoldState::Failed(e) => {
                        ctx.report(
                            "failed",
                            want.generation,
                            BTreeMap::new(),
                            Some(format!("no gold snapshot: {e}")),
                            None,
                            None,
                        );
                    }
                    GoldState::Building => {}
                }
                tokio::select! {
                    r = gold.changed() => if r.is_err() { break None },
                    _ = removed(&mut desired) => break None,
                }
            };
            let Some(tag) = tag else { break };
            let image = ctx.cfg.image(&GoldState::Ready(tag.clone()), Utc::now());
            if let Some(why) = image.refused {
                if let Some(mut r) = running.take() {
                    ctx.stop(&mut r).await;
                }
                ctx.report(
                    "failed",
                    want.generation,
                    BTreeMap::new(),
                    Some(why),
                    None,
                    None,
                );
                tokio::select! {
                    _ = tokio::time::sleep(REFUSED_RECHECK) => {}
                    changed = desired.changed() => if changed.is_err() { break },
                }
                continue;
            }
            if let Some(delay) = retry_after.take() {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = removed(&mut desired) => break,
                }
            }
            let want = desired.borrow_and_update().clone();
            match ctx.relaunch(&mut running, &tag, &want, &mut desired).await {
                Ok(r) => running = Some(r),
                Err(None) => break,
                Err(Some(e)) => {
                    tracing::warn!(sandbox = %ctx.id, generation = want.generation, error = %e, "sandbox relaunch failed");
                    let delay = backoff;
                    backoff = (backoff * 2).min(ctx.agent.backoff_max);
                    ctx.report(
                        "failed",
                        want.generation,
                        BTreeMap::new(),
                        Some(e),
                        Some(delay),
                        None,
                    );
                    retry_after = Some(delay);
                }
            }
            continue;
        }
        let r = running.as_mut().expect("not behind, so a sandbox runs");
        while action.is_none() {
            let Some(a) = Sandbox::next_action(r, &desired.borrow()) else {
                break;
            };
            if let Some(send) = ctx.post_action(a.action) {
                action = Some((a, send));
            }
        }
        let died = tokio::select! {
            (a, sent) = in_flight(&mut action) => {
                action = None;
                ctx.log_action(&a, sent);
                None
            }
            changed = desired.changed() => {
                if changed.is_err() {
                    break;
                }
                None
            }
            _ = image_tick.tick() => {
                if ctx.cfg.image(&GoldState::Ready(String::new()), Utc::now()).refused.is_some() {
                    // The next pass stops nothing more and reports the refusal with its date.
                    action = None;
                    let mut r = running.take().unwrap();
                    tracing::warn!(sandbox = %ctx.id, "the Windows image is refused; stopping the sandbox");
                    ctx.stop(&mut r).await;
                    continue;
                }
                None
            }
            status = r.vm.exited() => Some(format!("fcvm exited ({status}): {}", r.vm.tail())),
            status = r.bridge.client.exited() => Some(format!("the bridge exited ({status}): {}", r.bridge.client.tail())),
            _ = health_tick.tick() => {
                // A scene the bridge did not take is offered again here.
                r.scene = ctx.sync_scene(r.scene.take()).await;
                match ctx.health().await.as_ref().map(bridge_state) {
                    Some((Some("failed"), _)) => Some("the bridge lost the session".to_string()),
                    _ => None,
                }
            }
            changed = scene.changed(), if scene_open => {
                if changed.is_ok() {
                    scene.borrow_and_update();
                    r.scene = ctx.sync_scene(r.scene.take()).await;
                } else {
                    // The agent is stopping; no scene comes after this.
                    scene_open = false;
                }
                None
            }
        };
        if let Some(error) = died {
            action = None;
            let mut r = running.take().unwrap();
            let ran = r.started.elapsed();
            tracing::warn!(sandbox = %ctx.id, %error, "sandbox died");
            ctx.stop(&mut r).await;
            if ran >= ctx.agent.stable_after {
                backoff = ctx.agent.backoff_min;
            }
            let delay = backoff;
            backoff = (backoff * 2).min(ctx.agent.backoff_max);
            ctx.report(
                "failed",
                r.generation,
                BTreeMap::new(),
                Some(error),
                Some(delay),
                None,
            );
            retry_after = Some(delay);
        }
    }
    drop(action);
    if let Some(mut r) = running.take() {
        ctx.stop(&mut r).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        parse_time(s).unwrap()
    }

    /// Invariant 7 of the design: a warning from 150 days, no sandbox from 170 days or from 7
    /// days before the recorded expiry, whichever is first.
    #[test]
    fn an_image_warns_at_150_days_and_is_refused_at_170_or_a_week_before_expiry() {
        let installed = t("2026-11-07T10:00:00Z");
        // An expiry far enough out that the age rule decides.
        let expires = t("2027-12-31T00:00:00Z");
        let at = |days: i64| installed + chrono::Duration::days(days);
        let v = verdict(installed, expires, at(149));
        assert_eq!((v.age_days, v.warning, v.refused), (149, None, None));
        let v = verdict(installed, expires, at(150));
        assert_eq!(v.age_days, 150);
        assert!(
            v.warning.as_deref().unwrap().contains("150 days old"),
            "{v:?}"
        );
        assert!(
            v.warning.as_deref().unwrap().contains("2027-04-26"),
            "{v:?}"
        );
        assert_eq!(v.refused, None);
        let v = verdict(installed, expires, at(169));
        assert!(v.warning.is_some() && v.refused.is_none(), "{v:?}");
        let v = verdict(installed, expires, at(170));
        let refused = v.refused.unwrap();
        assert!(refused.contains("170 days old"), "{refused}");
        assert!(refused.contains("2026-11-07"), "{refused}");
        // The expiry rule: 180 days, so 7 days before it comes after 170 days only when the
        // expiry is late; one 100 days out is refused at 93.
        let expires = installed + chrono::Duration::days(100);
        let v = verdict(installed, expires, expires - chrono::Duration::days(8));
        assert_eq!((v.age_days, v.refused), (92, None));
        let v = verdict(installed, expires, expires - chrono::Duration::days(7));
        let refused = v.refused.unwrap();
        assert!(
            refused.contains("2027-02-15"),
            "names the expiry: {refused}"
        );
        assert!(
            refused.contains("2027-02-08"),
            "names the refusal day: {refused}"
        );
        // A second before the boundary is still allowed.
        let v = verdict(
            installed,
            expires,
            expires - chrono::Duration::days(7) - chrono::Duration::seconds(1),
        );
        assert_eq!(v.refused, None);
    }

    #[test]
    fn the_gold_snapshot_is_named_by_the_image_and_reused_only_when_whole() {
        assert_eq!(gold_tag("3f9c0a1b2c4d"), "win-gold-3f9c0a1b2c4d");
        let dir = tempfile::tempdir().unwrap();
        let snaps = dir.path().join("snapshots");
        let state = dir.path().join("state");
        assert!(!gold_ready(&snaps, &state, "abc"));
        std::fs::create_dir_all(snaps.join("win-gold-abc")).unwrap();
        std::fs::write(snaps.join("win-gold-abc/config.json"), "{}").unwrap();
        assert!(
            !gold_ready(&snaps, &state, "abc"),
            "no desktop reference yet"
        );
        let r = desktop_ref(&state, "abc");
        std::fs::create_dir_all(r.parent().unwrap()).unwrap();
        std::fs::write(&r, "P6").unwrap();
        assert!(gold_ready(&snaps, &state, "abc"));
        assert!(!gold_ready(&snaps, &state, "abd"), "another image's");
    }

    /// The image id hashes the build's recipe, so building the same recipe again gives the
    /// same id with a new installation: a new sandbox password and new evaluation dates. A
    /// gold snapshot of the old installation would log on with the wrong password and be aged
    /// by the wrong dates, so the key changes with either file.
    #[test]
    fn a_reinstalled_image_names_a_new_gold_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let write = |installed: &str, password: &str| {
            std::fs::write(
                dir.path().join("manifest.json"),
                serde_json::json!({"id": "3f9c0a1b2c4d", "edition": "e", "build": "b",
                    "installed_at": installed, "eval_expires": "2027-05-06T10:00:00+00:00"})
                .to_string(),
            )
            .unwrap();
            std::fs::write(dir.path().join("sandbox-password"), password).unwrap();
            let m = Manifest::read(dir.path()).unwrap();
            gold_key(dir.path(), &m).unwrap()
        };
        let first = write("2026-11-07T10:00:00Z", "Sbaaaa!7");
        assert_eq!(
            first,
            write("2026-11-07T10:00:00Z", "Sbaaaa!7"),
            "the same installation"
        );
        assert!(first.starts_with("3f9c0a1b2c4d-"), "{first}");
        assert_ne!(
            first,
            write("2026-11-07T10:00:00Z", "Sbbbbb!7"),
            "a new password"
        );
        assert_ne!(
            first,
            write("2026-12-01T10:00:00Z", "Sbaaaa!7"),
            "a new installation date"
        );
        assert!(gold_tag(&first).len() <= 64);
    }

    /// Every process the Windows mode starts gets PR_SET_PDEATHSIG: a VM or bridge through
    /// `Vm::spawn_args`, every other command through `child`. A raw `Command::new` here would
    /// outlive a killed agent (a snapshot create still writing, a podman stop half done).
    #[test]
    fn every_child_the_windows_agent_starts_dies_with_it() {
        let src = include_str!("windows.rs");
        let code = &src[..src.find("#[cfg(test)]").unwrap()];
        let raw: Vec<usize> = code
            .match_indices("Command::new(")
            .map(|(i, _)| i)
            .collect();
        let helper = code
            .find("fn child(")
            .expect("windows.rs has its child helper");
        let helper_end = helper + code[helper..].find("\n}\n").unwrap();
        assert!(
            raw.iter().all(|i| (helper..helper_end).contains(i)),
            "a Command::new outside `child`, at bytes {raw:?}"
        );
    }

    /// A replacement waits at least as long as every step of the old sandbox's teardown may
    /// take, from the same constants: podman stop and podman rm (each `bridge_stop` plus its
    /// slack), the client's grace, and the VM's (its SIGTERM grace, then SIGKILL). Waiting less
    /// gives up on a child that is still stopping.
    #[test]
    fn a_replacement_waits_out_every_step_of_a_teardown() {
        for (stop_grace, bridge_stop) in [(30, 10), (10, 1), (60, 30)] {
            let mut agent = test_agent();
            agent.stop_grace = Duration::from_secs(stop_grace);
            let mut cfg = test_cfg();
            cfg.bridge_stop = Duration::from_secs(bridge_stop);
            let steps = (cfg.bridge_stop + PODMAN_SLACK) * 2 + CLIENT_GRACE + agent.stop_grace;
            assert!(
                teardown_budget(&cfg, &agent) > steps,
                "{:?} for steps that may take {steps:?}",
                teardown_budget(&cfg, &agent)
            );
        }
    }

    #[test]
    fn a_manifest_needs_a_hex_id_and_both_dates() {
        let dir = tempfile::tempdir().unwrap();
        let write = |v: serde_json::Value| {
            std::fs::write(dir.path().join("manifest.json"), v.to_string()).unwrap()
        };
        let good = serde_json::json!({"id": "3f9c0a1b2c4d", "edition": "Windows Server 2025 Datacenter Evaluation (Desktop Experience)",
            "build": "26100.1742", "iso": "x.iso", "installed_at": "2026-11-07T10:00:00Z", "eval_expires": "2027-05-06T10:00:00+00:00"});
        write(good.clone());
        let m = Manifest::read(dir.path()).unwrap();
        assert_eq!(m.id, "3f9c0a1b2c4d");
        assert_eq!(m.expires().unwrap(), t("2027-05-06T10:00:00Z"));
        for (k, bad) in [
            ("id", "../etc"),
            ("id", ""),
            ("installed_at", "yesterday"),
            ("eval_expires", "5/6/2027"),
        ] {
            let mut v = good.clone();
            v[k] = bad.into();
            write(v);
            assert!(Manifest::read(dir.path()).is_err(), "{k} {bad:?}");
        }
    }

    #[test]
    fn the_vm_commands_follow_the_raw_vm_contract() {
        let dir = Path::new("/img/image-3f9c");
        assert_eq!(
            cold_boot_argv("win-gold", dir),
            [
                "vm",
                "run",
                "--name",
                "win-gold",
                "--firmware",
                "/img/image-3f9c/CLOUDHV.fd",
                "--disk",
                "/img/image-3f9c/disk.raw",
                "--cpu",
                "2",
                "--mem",
                "4096",
                "--guest",
                "windows",
                "--health-port",
                "3389"
            ]
        );
        assert_eq!(
            snapshot_argv(4242, "win-gold-3f9c"),
            [
                "snapshot",
                "create",
                "--pid",
                "4242",
                "--tag",
                "win-gold-3f9c",
                "--stop"
            ]
        );
        assert_eq!(
            restore_argv("win-gold-3f9c", "win-3"),
            [
                "snapshot",
                "run",
                "--snapshot",
                "win-gold-3f9c",
                "--name",
                "win-3"
            ]
        );
    }

    #[test]
    fn the_bridge_gets_the_socket_the_password_file_and_its_ports_and_nothing_more() {
        let rdp = Path::new("/mnt/fcvm-btrfs/vm-disks/vm-1/rdp.sock");
        let pw = Path::new("/img/sandbox-password");
        let logon = bridge_argv(
            "fleet-win-gold-bridge",
            "localhost/windows-bridge:fleet",
            rdp,
            pw,
            &BridgeMode::Logon {
                state_dir: "/st/3f9c".into(),
                ref_out: "/st/3f9c/desktop-ref.ppm".into(),
            },
        );
        let joined = logon.join(" ");
        assert!(
            joined.starts_with("run --rm --replace --pull=never --label fcvm.fleet.windows-bridge=1 --name fleet-win-gold-bridge "),
            "{joined}"
        );
        assert!(joined
            .contains("--volume /mnt/fcvm-btrfs/vm-disks/vm-1/rdp.sock:/run/windows/rdp.sock"));
        assert!(joined.contains("--volume /img/sandbox-password:/run/windows/sandbox-password:ro"));
        assert!(joined.contains("--env RDP_SOCKET=/run/windows/rdp.sock"));
        assert!(joined.contains("--env RDP_USER=sandbox"));
        assert!(joined.contains("--env RDP_PASSWORD_FILE=/run/windows/sandbox-password"));
        assert!(joined.contains("--volume /st/3f9c:/run/windows/state --env DESKTOP_REF_OUT=/run/windows/state/desktop-ref.ppm"));
        assert!(
            joined.ends_with("localhost/windows-bridge:fleet logon"),
            "{joined}"
        );
        assert!(!joined.contains("--publish"), "the logon serves nothing");

        let serve = bridge_argv(
            "fleet-win-2-bridge",
            "localhost/windows-bridge:fleet",
            rdp,
            pw,
            &BridgeMode::Serve {
                desktop_ref: "/st/3f9c/desktop-ref.ppm".into(),
                publish_ip: "2001:db8::10".into(),
                port: 20105,
                tls_port: Some(21105),
                ipv6: Some("2001:db8:1::7".parse().unwrap()),
                tls_dir: Some("/run/fcvm-fleet/tls".into()),
            },
        );
        let joined = serve.join(" ");
        assert!(joined.contains("--volume /st/3f9c/desktop-ref.ppm:/run/windows/desktop-ref.ppm:ro --env DESKTOP_REF=/run/windows/desktop-ref.ppm"));
        assert!(joined.contains("--env PORT=8080 --publish [2001:db8::10]:20105:8080"));
        assert!(joined.contains("--volume /run/fcvm-fleet/tls:/run/fleet-tls:ro --env TLS_CERT_FILE=/run/fleet-tls/tls.json --env TLS_PORT=443"));
        assert!(joined.contains("--publish [2001:db8::10]:21105:443"));
        assert!(joined.contains("--publish [2001:db8:1::7]:443:443"));
        assert!(
            joined.ends_with("localhost/windows-bridge:fleet serve"),
            "{joined}"
        );
        // Without a certificate the bridge serves plain HTTP on its port only.
        let plain = bridge_argv(
            "fleet-win-2-bridge",
            "img",
            rdp,
            pw,
            &BridgeMode::Serve {
                desktop_ref: "/r.ppm".into(),
                publish_ip: "127.0.0.1".into(),
                port: 20105,
                tls_port: Some(21105),
                ipv6: None,
                tls_dir: None,
            },
        )
        .join(" ");
        assert!(!plain.contains("TLS"), "{plain}");
        assert_eq!(plain.matches("--publish").count(), 1, "{plain}");
    }

    #[test]
    fn ls_finds_this_children_vm_and_its_rdp_socket() {
        let ls = serde_json::json!([
            {"name": "win-1", "pid": 100, "health_status": "healthy",
             "config": {"guest": "windows", "rdp_socket": "/d/old/rdp.sock"}},
            {"name": "win-1", "pid": 200, "health_status": "unknown",
             "config": {"guest": "windows", "rdp_socket": "/d/vm-2/rdp.sock"}},
            {"name": "win-2", "pid": 300, "health_status": "healthy",
             "config": {"guest": "windows", "rdp_socket": "relative.sock"}},
            {"name": "win-3", "pid": 400, "health_status": "healthy", "config": {}}
        ])
        .to_string();
        assert_eq!(
            listed(&ls, "win-1", 200),
            Some(Listed {
                healthy: false,
                rdp_socket: Some("/d/vm-2/rdp.sock".into())
            })
        );
        assert!(
            listed(&ls, "win-1", 100).unwrap().healthy,
            "the other pid's entry"
        );
        assert_eq!(listed(&ls, "win-2", 300).unwrap().rdp_socket, None);
        assert!(
            !listed(&ls, "win-3", 400).unwrap().healthy,
            "not a Windows guest"
        );
        assert_eq!(listed(&ls, "win-1", 999), None);
        assert_eq!(listed("not json", "win-1", 200), None);
    }

    /// Idle is the average over the window: a busy logon keeps it busy until the window has
    /// passed beyond it, a lone busy second on the floor does not, and a desktop that stays
    /// over the threshold never settles.
    #[test]
    fn the_desktop_is_idle_once_its_average_over_the_window_is_low() {
        let run = |shares: &dyn Fn(usize) -> f64, secs: usize| {
            let mut d = IdleDetector::new(DEFAULT_IDLE_CPU, IDLE_WINDOW);
            let mut cpu = 0.0;
            for s in 0..secs {
                if d.sample(s as f64, cpu) {
                    return (Some(s), d.last_share);
                }
                cpu += shares(s);
            }
            (None, d.last_share)
        };
        // Two cores for seconds 0 to 9 (the logon), then the floor with one busy second, 25.
        // The window [t-20, t) holds two busy seconds at t = 29 (2 x 2 + 18 x 0.15 = 6.7 / 20 =
        // 0.335) and one at t = 30 (2 + 19 x 0.15 = 4.85 / 20 = 0.2425): idle at 30 with the
        // busy second inside the window.
        let (at, share) = run(&|s| if s < 10 || s == 25 { 2.0 } else { 0.15 }, 60);
        assert_eq!(at, Some(30), "{share:?}");
        // A desktop at 0.3 of a core never settles, and says so.
        let (at, share) = run(&|_| 0.3, 120);
        assert_eq!(at, None);
        assert!((share.unwrap() - 0.3).abs() < 1e-9);
        // Nothing is idle before a whole window has been seen.
        let (at, _) = run(&|_| 0.0, 20);
        assert_eq!(at, None);
        assert_eq!(run(&|_| 0.0, 21).0, Some(20));
    }

    /// The gold VM of the fleet's image on the x86 host, per second from the end of the bridge's
    /// logon: a floor of 0.14 to 0.17 of a core (its fcvm, the namespace helper and Cloud
    /// Hypervisor) with a busy second every half minute or so. Requiring every second under
    /// 0.15 never settled on it (the gold flow failed at its 300 s deadline).
    const MEASURED_GOLD_IDLE: &[f64] = &[
        1.42, 0.48, 0.27, 0.26, 0.17, 0.17, 0.22, 0.23, 0.17, 0.18, 0.23, 0.32, 0.17, 0.17, 0.17,
        0.26, 0.17, 0.17, 0.17, 0.18, 0.17, 0.33, 0.18, 0.18, 0.17, 0.23, 0.17, 0.17, 0.39, 0.20,
        0.33, 0.18, 0.17, 0.19, 0.17, 0.17, 0.18, 0.17, 0.17, 0.25, 0.17, 0.19, 0.18, 0.18, 0.32,
        0.17, 0.30, 0.15, 0.17, 0.15, 0.17, 0.17, 0.17, 0.19, 0.86, 0.81, 0.17, 0.25, 0.20, 0.15,
        0.17, 0.17, 0.14, 0.17, 0.14, 0.15, 0.15, 0.15, 0.19, 0.14, 0.15, 0.15, 0.17, 0.19, 0.17,
        0.14, 0.18, 0.17, 0.15, 0.14, 0.15, 0.17, 0.23, 0.14, 0.17, 0.17, 0.17, 0.23, 1.12, 0.76,
        0.54, 0.25, 0.17, 0.17, 0.15, 0.17, 0.18, 0.17, 0.17, 0.17, 0.19, 0.19, 0.15, 0.17, 0.15,
        0.17, 0.17, 0.15, 0.15, 0.17, 0.17, 0.33, 0.23, 0.15, 0.15, 0.28, 0.18, 0.17, 0.17, 0.15,
        0.14, 0.17, 0.15, 0.17, 0.15,
    ];

    #[test]
    fn the_measured_gold_desktop_goes_idle() {
        let mut d = IdleDetector::new(DEFAULT_IDLE_CPU, IDLE_WINDOW);
        let mut cpu = 0.0;
        let mut idle_at = None;
        for (s, share) in MEASURED_GOLD_IDLE.iter().enumerate() {
            if d.sample(s as f64, cpu) && idle_at.is_none() {
                idle_at = Some(s);
            }
            cpu += share;
        }
        let at = idle_at.expect("the measured desktop never went idle");
        assert!(at <= 30, "idle only at {at} s");
    }

    #[test]
    fn a_stat_line_gives_its_parent_and_cpu_time_whatever_the_name() {
        let line = "4242 (cloud-hyper visor)) S 4100 4242 4242 0 -1 4194560 1 0 0 0 1500 250 0 0 20 0 3 0 1 0 0";
        assert_eq!(parse_stat(line), Some((4242, 4100, 1750)));
        assert_eq!(parse_stat("garbage"), None);
        let dir = tempfile::tempdir().unwrap();
        let p = |pid: u32, ppid: u32, ut: u64| {
            let d = dir.path().join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("stat"),
                format!("{pid} (x) S {ppid} 0 0 0 -1 0 0 0 0 0 {ut} 0 0 0 20 0 1 0 1 0 0"),
            )
            .unwrap();
        };
        p(10, 1, 100); // fcvm
        p(11, 10, 200); // its VMM
        p(12, 11, 50); // below that
        p(20, 1, 9000); // someone else's
        std::fs::create_dir_all(dir.path().join("self")).unwrap();
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        assert_eq!(tree_cpu_secs(dir.path(), 10), Some(350.0 / hz));
        assert_eq!(tree_cpu_secs(dir.path(), 99), None);
    }

    #[test]
    fn the_hello_lists_every_action_but_reset_and_reset_is_never_posted() {
        let listed = actions();
        assert_eq!(listed.len(), WindowsAction::ALL.len() - 1);
        assert!(!listed.iter().any(|a| a == "reset"));
        assert_eq!(
            WindowsAction::offered(&listed),
            WindowsAction::ALL.to_vec(),
            "the coordinator offers all of them"
        );
        assert_eq!(action_body(WindowsAction::Reset), None);
        assert_eq!(action_body(WindowsAction::Unknown), None);
        assert_eq!(
            action_body(WindowsAction::TaskManager),
            Some(serde_json::json!({"name": "task_manager"}))
        );
    }

    #[test]
    fn a_launch_is_timed_from_the_bridges_times() {
        let h = serde_json::json!({"windows": {"state": "desktop", "times_unix_ms":
            {"start": 900, "connect_started": 1010, "tcp": 1020, "rdp": 1900, "first_frame": 2200, "desktop": 2320}}});
        let (state, times) = bridge_state(&h);
        assert_eq!(state, Some("desktop"));
        let t = launch_timings(300, 4100, 1000, 0, Some(500), times);
        assert_eq!(
            t,
            BTreeMap::from([
                ("teardown_ms".into(), 300),
                ("restore_ms".into(), 4100),
                ("connect_ms".into(), 900),
                ("first_frame_ms".into(), 300),
                ("desktop_ms".into(), 120),
                ("total_ms".into(), 2320),
                ("click_to_desktop_ms".into(), 1820),
            ])
        );
        // A click the host's clock puts after the desktop gives no click time.
        let t = launch_timings(0, 1, 1000, 0, Some(5000), times);
        assert!(!t.contains_key("click_to_desktop_ms"));
        let connecting = serde_json::json!({"windows": {"state": "connecting"}});
        let (state, times) = bridge_state(&connecting);
        assert_eq!((state, times), (Some("connecting"), BridgeTimes::default()));
    }

    /// The report a running sandbox sends: the coordinator's `win-<n>` id, the phase, the
    /// launch's timings and the desktop time, and never the password or the image directory.
    #[test]
    fn a_running_sandbox_reports_its_desktop_time() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let agent = test_agent();
        let s = Sandbox {
            id: "win-3".into(),
            place: Place {
                port: 20107,
                ipv6: None,
                tls_port: None,
            },
            cfg: Arc::new(test_cfg()),
            agent: Arc::new(agent),
            out: tx,
            http: reqwest::Client::new(),
            scene: watch::channel(None).1,
        };
        s.report(
            "running",
            4,
            BTreeMap::from([("total_ms".into(), 3500)]),
            None,
            None,
            Some(1_761_134_400_000),
        );
        let v = serde_json::to_value(rx.try_recv().unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type": "slot", "id": "win-3", "phase": "running", "generation": 4,
                "vm": "win-3", "frame_url": "ws://gpu-host.example:20107/frames",
                "timings": {"total_ms": 3500}, "desktop_at": 1_761_134_400_000u64})
        );
        let image = test_cfg().image(
            &GoldState::Failed("no desktop".into()),
            t("2026-11-19T10:00:00Z"),
        );
        let v = serde_json::to_value(AgentMsg::WindowsImage(image)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type": "windows_image", "id": "3f9c0a1b2c4d", "edition": "Windows Server 2025 Datacenter Evaluation (Desktop Experience)",
                "build": "26100.1742", "installed_at": "2026-11-07T10:00:00Z", "eval_expires": "2027-05-06T10:00:00+00:00",
                "age_days": 12, "warning": null, "refused": null, "gold": "failed", "gold_error": "no desktop"})
        );
    }

    fn test_cfg() -> WindowsCfg {
        WindowsCfg {
            count: 4,
            image_dir: "/img".into(),
            manifest: Manifest {
                id: "3f9c0a1b2c4d".into(),
                edition: "Windows Server 2025 Datacenter Evaluation (Desktop Experience)".into(),
                build: "26100.1742".into(),
                installed_at: "2026-11-07T10:00:00Z".into(),
                eval_expires: "2027-05-06T10:00:00+00:00".into(),
            },
            bridge_image: "img".into(),
            podman: "podman".into(),
            state_dir: "/st".into(),
            snapshot_dir: "/snap".into(),
            proc_root: "/proc".into(),
            idle_cpu: 0.15,
            idle_for: Duration::from_secs(10),
            gold_healthy_deadline: Duration::from_secs(600),
            logon_deadline: Duration::from_secs(300),
            idle_deadline: Duration::from_secs(300),
            snapshot_deadline: Duration::from_secs(600),
            start_timeout: Duration::from_secs(300),
            poll: Duration::from_millis(100),
            tls_dir: None,
            gold_key: "3f9c0a1b2c4d".into(),
            image_check: Duration::from_secs(60),
            bridge_stop: Duration::from_secs(10),
        }
    }

    fn test_agent() -> AgentCfg {
        AgentCfg {
            fcvm: "fcvm".into(),
            image: String::new(),
            extra_args: Vec::new(),
            publish_ip: "127.0.0.1".into(),
            frame_host: "gpu-host.example".into(),
            ipv6_prefix: None,
            frame_zone: None,
            tls_publish_base: None,
            public_ipv4: None,
            place: None,
            sysfs_root: "/sys".into(),
            start_timeout: Duration::from_secs(300),
            stop_grace: Duration::from_secs(30),
            gpu_free_timeout: Duration::from_secs(30),
            health_interval: Duration::from_secs(1),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
            scrub: false,
            scrub_timeout: Duration::from_secs(120),
            quarantine_after: 3,
            phone: None,
            scene_token: Default::default(),
        }
    }
}
