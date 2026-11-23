//! The `tap-attach` piece: what a VMM does to a TAP that already exists in a namespace that
//! already has its pasta and bridge. A recycled namespace keeps everything else, so this is
//! the network work left on a clone's start (attach) and stop (detach).
//!
//! The attach is the three ioctls Firecracker makes on a snapshot load with a network
//! override: `TUNSETIFF` on a fresh `/dev/net/tun` descriptor (`Tap::open_named`),
//! `TUNSETVNETHDRSZ` (`Net::new`) and `TUNSETOFFLOAD` (`Net::restore`). The detach is the
//! close of that descriptor at the VMM's exit. Each takes `rtnl_mutex`; the close also waits
//! for an expedited RCU grace period under it (`__tun_detach` -> `synchronize_net`), and the
//! carrier change each one causes is handled later by the kernel's linkwatch worker, also
//! under `rtnl_mutex` (`tap.bt` measures those).
//!
//! The ioctls run in a helper process that lives in the holder's user and network
//! namespaces, as the VMM does, so the timed op forks and execs nothing.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Stdio;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};

/// The helper's `comm`, which `tap.bt` filters on.
pub const HELPER_COMM: &std::ffi::CStr = c"tap-helper";

const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const TUNSETOFFLOAD: libc::c_ulong = 0x4004_54d0;
const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;
const IFF_VNET_HDR: libc::c_short = 0x4000;
/// `TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_UFO`: what Firecracker sets for a guest that
/// negotiated checksum and segmentation offload.
const OFFLOAD: libc::c_uint = 0x01 | 0x02 | 0x04 | 0x10;
/// `size_of::<virtio_net_hdr_v1>()`.
const VNET_HDR_SIZE: libc::c_int = 12;

/// `struct ifreq`: the name, then a union this code uses only as the flags.
#[repr(C)]
struct IfReq {
    name: [u8; libc::IFNAMSIZ],
    flags: libc::c_short,
    pad: [u8; 22],
}

fn ioctl<T>(fd: &OwnedFd, request: libc::c_ulong, arg: *const T, what: &str) -> Result<()> {
    // The request's type is the libc's choice (c_ulong on glibc, c_int on musl).
    if unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg) } != 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn us(since: Instant) -> u128 {
    since.elapsed().as_micros()
}

/// Attach to the existing TAP `name` the way Firecracker does. Returns the descriptor and
/// the microseconds each ioctl took.
fn attach(name: &str) -> Result<(OwnedFd, [u128; 3])> {
    let raw = unsafe {
        libc::open(
            c"/dev/net/tun".as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        bail!("opening /dev/net/tun: {}", std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    anyhow::ensure!(name.len() < libc::IFNAMSIZ, "TAP name {name} is too long");
    let mut req = IfReq {
        name: [0; libc::IFNAMSIZ],
        flags: IFF_TAP | IFF_NO_PI | IFF_VNET_HDR,
        pad: [0; 22],
    };
    req.name[..name.len()].copy_from_slice(name.as_bytes());
    let t = Instant::now();
    ioctl(&fd, TUNSETIFF, &req, "TUNSETIFF")?;
    let setiff = us(t);
    let t = Instant::now();
    ioctl(&fd, TUNSETVNETHDRSZ, &VNET_HDR_SIZE, "TUNSETVNETHDRSZ")?;
    let hdrsz = us(t);
    let t = Instant::now();
    // TUNSETOFFLOAD takes its flags by value.
    if unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETOFFLOAD as _, OFFLOAD as libc::c_ulong) } != 0 {
        bail!("TUNSETOFFLOAD: {}", std::io::Error::last_os_error());
    }
    Ok((fd, [setiff, hdrsz, us(t)]))
}

fn setns(pid: u32, name: &str, kind: libc::c_int) -> Result<()> {
    let path = format!("/proc/{pid}/ns/{name}");
    let file = std::fs::File::open(&path).with_context(|| format!("opening {path}"))?;
    if unsafe { libc::setns(file.as_raw_fd(), kind) } != 0 {
        bail!("setns {path}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// The helper process: enter the holder's namespaces, then attach and detach on request.
/// Single-threaded, because `setns(CLONE_NEWUSER)` refuses a process with more than one
/// thread. Requests are lines on stdin: `attach` answers `ok <setiff> <hdrsz> <offload>`
/// (microseconds), `detach` answers `ok <close>`.
pub fn helper(parent: u32, holder_pid: u32, tap: &str) -> Result<()> {
    setns(holder_pid, "user", libc::CLONE_NEWUSER)?;
    setns(holder_pid, "net", libc::CLONE_NEWNET)?;
    // After the user namespace: a credential change clears the parent-death signal.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        bail!(
            "arming the parent-death signal: {}",
            std::io::Error::last_os_error()
        );
    }
    if std::os::unix::process::parent_id() != parent {
        bail!("the instance ({parent}) is not this helper's parent any more");
    }
    unsafe { libc::prctl(libc::PR_SET_NAME, HELPER_COMM.as_ptr()) };
    let mut held: Option<OwnedFd> = None;
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let reply = match line?.as_str() {
            "attach" => {
                anyhow::ensure!(held.is_none(), "attach while attached");
                let (fd, [setiff, hdrsz, offload]) = attach(tap)?;
                held = Some(fd);
                format!("ok {setiff} {hdrsz} {offload}")
            }
            "detach" => {
                let fd = held.take().context("detach while detached")?;
                let t = Instant::now();
                drop(fd);
                format!("ok {}", us(t))
            }
            other => bail!("unknown request '{other}'"),
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

/// The instance's handle on its helper.
pub struct Helper {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: Lines<BufReader<tokio::process::ChildStdout>>,
}

impl Helper {
    pub fn spawn(holder_pid: u32, tap: &str) -> Result<Self> {
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("tap-helper")
            .arg(std::process::id().to_string())
            .arg(holder_pid.to_string())
            .arg(tap)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting the TAP helper")?;
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Ok(Self {
            child,
            stdin,
            lines,
        })
    }

    /// Send one request; the reply's numbers are microseconds, returned as milliseconds.
    async fn request(&mut self, word: &str) -> Result<Vec<f64>> {
        self.stdin.write_all(format!("{word}\n").as_bytes()).await?;
        self.stdin.flush().await?;
        let line = self
            .lines
            .next_line()
            .await?
            .with_context(|| format!("the TAP helper exited before answering '{word}'"))?;
        let rest = line
            .strip_prefix("ok")
            .with_context(|| format!("the TAP helper answered '{line}' to '{word}'"))?;
        rest.split_whitespace()
            .map(|n| Ok(n.parse::<f64>()? / 1000.0))
            .collect()
    }

    /// One clone life's worth: attach, stay attached for `hold` (so the carrier-on is handled
    /// as its own link event, as it is under a running VM), detach.
    pub async fn cycle(
        &mut self,
        hold: std::time::Duration,
        stages: &mut crate::pieces::Stages,
    ) -> Result<()> {
        let attach = self.request("attach").await?;
        anyhow::ensure!(attach.len() == 3, "short attach reply");
        stages.push(("tunsetiff", attach[0]));
        stages.push(("vnethdrsz", attach[1]));
        stages.push(("offload", attach[2]));
        tokio::time::sleep(hold).await;
        let detach = self.request("detach").await?;
        anyhow::ensure!(detach.len() == 1, "short detach reply");
        stages.push(("close", detach[0]));
        stages.push(("attach+detach", attach.iter().sum::<f64>() + detach[0]));
        Ok(())
    }

    pub async fn stop(mut self) {
        drop(self.stdin);
        let _ = self.child.wait().await;
    }
}

/// `tap.bt`'s keys for the helper's four calls, in the order a clone life makes them.
const CALLS: [(&str, &str); 4] = [
    ("1074025674", "TUNSETIFF"),
    ("1074025688", "TUNSETVNETHDRSZ"),
    ("1074025680", "TUNSETOFFLOAD"),
    ("1", "close"),
];

/// The scalar maps of a bpftrace text dump: `@name[key]: n` and `@name: n` lines, added to
/// `into` under `name[key]`. Histogram rows and everything else are skipped.
fn add_scalars(text: &str, into: &mut std::collections::BTreeMap<String, f64>) {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix('@') else {
            continue;
        };
        let Some((key, value)) = rest.rsplit_once(": ") else {
            continue;
        };
        if let Ok(value) = value.trim().parse::<f64>() {
            *into.entry(key.to_string()).or_default() += value;
        }
    }
}

/// Carrier-on and carrier-off link events the worker must have handled per cycle, each, for
/// [`summarize`] to charge one of each to a cycle.
const LINK_EVENTS_FLOOR: f64 = 0.5;

/// What one attach and detach cost in rtnl_mutex hold time and grace periods, from the
/// `tap.bt` dumps of a tap-attach cell (its timed and steady windows together).
pub fn summarize(maps: &std::collections::BTreeMap<String, f64>) -> Result<serde_json::Value> {
    let get = |key: &str| maps.get(key).copied().unwrap_or(0.0);
    let cycles = get("calls[1]");
    anyhow::ensure!(
        cycles > 0.0,
        "the tracer saw no close by the helper: it was not attached during the timed phase"
    );
    let mut calls = serde_json::Map::new();
    let mut helper_hold_us = 0.0;
    for (key, name) in CALLS {
        let n = get(&format!("calls[{key}]"));
        anyhow::ensure!(n > 0.0, "the tracer saw no {name} by the helper");
        // Each call takes the mutex once. `tap.bt` sums a hold from a return probe on
        // rtnl_lock, and the kernel keeps a fixed number of return-probe instances (twice
        // the CPUs): when more helpers than that sleep in rtnl_lock at once, the holds of
        // the rest are not summed while their calls are still counted, and every hold
        // column reads short (0.77 of the calls at C=512 on 192 CPUs). A call that
        // straddles a window's edge accounts for a difference of one.
        let holds = get(&format!("helper_holds[{key}]"));
        anyhow::ensure!(
            (holds - n).abs() <= (n * 0.005).max(1.0),
            "the tracer summed {holds} rtnl_mutex holds for {n} {name} calls: it dropped \
             some (return-probe instances ran out), so the hold columns would read short"
        );
        let hold = get(&format!("helper_hold_us[{key}]")) / n;
        helper_hold_us += hold;
        calls.insert(
            name.into(),
            serde_json::json!({
                "calls": n,
                "rtnl_holds_per_call": holds / n,
                "rtnl_hold_us": hold,
                "rtnl_wait_us": get(&format!("helper_wait_us[{key}]")) / n,
                "expedited_gps_per_call": get(&format!("helper_gp[{key}]")) / n,
                "expedited_gp_us_per_call": get(&format!("helper_gp_us[{key}]")) / n,
                "synchronize_rcu_per_call": get(&format!("helper_rcu_sync[{key}]")) / n,
            }),
        );
    }
    // Link events of TAP devices, by the carrier state the worker found: 1 on, 0 off. A cycle
    // causes one of each. The carrier-off is handled up to a second later, so some fall
    // between the two dumps: each kind is averaged over the events seen, not over cycles.
    let (on, off) = (get("lw_tap_devs[1]"), get("lw_tap_devs[0]"));
    anyhow::ensure!(
        on > 0.0 && off > 0.0,
        "the tracer saw {on} carrier-on and {off} carrier-off link events of TAP devices; \
         it needs both"
    );
    let on_us = get("lw_tap_dev_us[1]") / on;
    let off_us = get("lw_tap_dev_us[0]") / off;
    // Link events of every other device, which a cycle can cause too: in a namespace with
    // no pasta the TAP is the bridge's only port, so the bridge's carrier follows it. These
    // are per cycle as seen, since nothing says how many a cycle should cause.
    let others = get("lw_other_devs");
    let other_us_per_cycle = get("lw_other_dev_us") / cycles;
    let link_hold_us = on_us + off_us + other_us_per_cycle;
    // The per-cycle hold charges one carrier-on and one carrier-off to every cycle. That
    // is a cycle's cost only when the worker handled about one of each per cycle. In a
    // namespace with no pasta it handled the TAP's carrier-on in 1% of the cycles: the
    // helper was gone before the worker ran, and the mean of the few events seen is not
    // what the other cycles held. Below half an event per cycle the sums are left out, and
    // `window_rtnl_hold_us_per_cycle` is the cell's figure.
    let events_per_cycle_seen = (on / cycles).min(off / cycles) >= LINK_EVENTS_FLOOR;
    let per_cycle = |us: f64| events_per_cycle_seen.then_some(us);
    let runs = get("lw_holds[1]");
    Ok(serde_json::json!({
        "cycles_traced": cycles,
        "calls": calls,
        "helper_rtnl_hold_us_per_cycle": helper_hold_us,
        "link_events": {
            "carrier_on_seen_per_cycle": on / cycles,
            "carrier_off_seen_per_cycle": off / cycles,
            "carrier_on_us": on_us,
            "carrier_off_us": off_us,
            "worker_runs": runs,
            "tap_events_per_worker_run": if runs > 0.0 { (on + off) / runs } else { 0.0 },
            "worker_hold_us_per_run": if runs > 0.0 { get("lw_hold_us[1]") / runs } else { 0.0 },
            "expedited_gps_per_event": get("lw_gp") / (on + off),
            "expedited_gp_us": if get("lw_gp") > 0.0 { get("lw_gp_us") / get("lw_gp") } else { 0.0 },
            "other_devices_seen_per_cycle": others / cycles,
            "other_devices_us": if others > 0.0 { get("lw_other_dev_us") / others } else { 0.0 },
            "other_devices_us_per_cycle": other_us_per_cycle,
            "rtnl_hold_us_per_cycle": per_cycle(link_hold_us),
        },
        // Every hold of the mutex on the host in the cell's two windows, per cycle: what
        // the cycles held and whatever else ran. On a quiet host it checks the sum below,
        // which counts one carrier-on and one carrier-off per cycle whether or not the
        // worker handled both inside the windows.
        "window_rtnl_hold_us_per_cycle": get("hold_us_total") / cycles,
        "rtnl_hold_us_per_cycle": per_cycle(helper_hold_us + link_hold_us),
        "rtnl_ceiling_cycles_per_s": per_cycle(1e6 / (helper_hold_us + link_hold_us)),
    }))
}

/// `start-pieces tap-summary CELL`: read the cell's `perfstat-*.bt` dumps and print the
/// summary, with the cell's own ops/s and stage times beside it.
pub fn summary(cell: &std::path::Path) -> Result<()> {
    let mut maps = std::collections::BTreeMap::new();
    for name in ["perfstat-timed.bt", "perfstat-steady.bt"] {
        let path = cell.join(name);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        add_scalars(&text, &mut maps);
    }
    let mut out = summarize(&maps)?;
    let cell_summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cell.join("summary.json"))?)?;
    for key in ["count", "concurrency", "alive"] {
        out[key] = cell_summary[key].clone();
    }
    out["ops_per_s"] = cell_summary["timed"]["ops_per_s"].clone();
    out["stages_ms"] = cell_summary["timed"]["stages_ms"].clone();
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two windows of one cell: 4 cycles, each call once per cycle, one grace period per
    /// close, and each cycle's carrier-on and carrier-off handled by the link worker.
    const TIMED: &str = "Attaching 14 probes...\n\n\
        @calls[1]: 3\n@calls[1074025674]: 4\n@calls[1074025688]: 4\n@calls[1074025680]: 4\n\
        @helper_hold_us[1074025674]: 400\n@helper_hold_us[1074025688]: 8\n\
        @helper_hold_us[1074025680]: 40\n@helper_hold_us[1]: 900\n\
        @helper_holds[1]: 3\n@helper_holds[1074025674]: 4\n\
        @helper_holds[1074025688]: 4\n@helper_holds[1074025680]: 4\n\
        @helper_gp[1]: 3\n@helper_gp_us[1]: 600\n\
        @helper_hold_us_hist[1]: \n[256, 512)             3 |@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@|\n\n\
        @lw_tap_devs[1]: 4\n@lw_tap_dev_us[1]: 200\n@lw_holds[1]: 4\n@lw_hold_us[1]: 220\n\
        @other_hold_us[ip]: 77\n";
    /// The steady window caught 2 of the 4 carrier-offs: the tracer was not attached for
    /// the other two.
    const STEADY: &str = "@calls[1]: 1\n@helper_hold_us[1]: 300\n@helper_holds[1]: 1\n\
        @helper_gp[1]: 1\n@helper_gp_us[1]: 200\n\
        @lw_tap_devs[0]: 2\n@lw_tap_dev_us[0]: 200\n@lw_holds[1]: 1\n@lw_hold_us[1]: 210\n\
        @lw_gp: 3\n@lw_gp_us: 240\n@lw_other_devs: 2\n@lw_other_dev_us: 80\n\
        @hold_us_total: 2400\n";

    #[test]
    fn a_cycle_is_the_four_calls_and_its_two_link_events() {
        let mut maps = std::collections::BTreeMap::new();
        add_scalars(TIMED, &mut maps);
        add_scalars(STEADY, &mut maps);
        let s = summarize(&maps).unwrap();
        assert_eq!(s["cycles_traced"], 4.0);
        assert_eq!(s["calls"]["TUNSETIFF"]["rtnl_hold_us"], 100.0);
        assert_eq!(s["calls"]["close"]["rtnl_hold_us"], 300.0);
        assert_eq!(s["calls"]["close"]["expedited_gps_per_call"], 1.0);
        assert_eq!(s["calls"]["close"]["expedited_gp_us_per_call"], 200.0);
        assert_eq!(
            s["helper_rtnl_hold_us_per_cycle"],
            100.0 + 2.0 + 10.0 + 300.0
        );
        assert_eq!(s["link_events"]["carrier_on_seen_per_cycle"], 1.0);
        assert_eq!(s["link_events"]["carrier_off_seen_per_cycle"], 0.5);
        assert_eq!(s["link_events"]["carrier_on_us"], 50.0);
        assert_eq!(s["link_events"]["carrier_off_us"], 100.0);
        assert_eq!(s["link_events"]["expedited_gp_us"], 80.0);
        assert_eq!(s["link_events"]["expedited_gps_per_event"], 0.5);
        // One carrier-on and one carrier-off per cycle, each at its own average, and the
        // other devices' events as seen: 2 of 40 us in 4 cycles.
        assert_eq!(s["link_events"]["other_devices_seen_per_cycle"], 0.5);
        assert_eq!(s["link_events"]["other_devices_us"], 40.0);
        assert_eq!(s["link_events"]["rtnl_hold_us_per_cycle"], 170.0);
        assert_eq!(s["rtnl_hold_us_per_cycle"], 582.0);
        assert_eq!(s["window_rtnl_hold_us_per_cycle"], 600.0);
    }

    /// A cell whose calls outnumber the holds summed for them is refused: the tracer ran
    /// out of return-probe instances and every hold column would be short by that share.
    #[test]
    fn holds_the_tracer_dropped_are_refused() {
        let mut maps = std::collections::BTreeMap::new();
        add_scalars(TIMED, &mut maps);
        add_scalars(STEADY, &mut maps);
        // 1000 TUNSETIFF calls, 770 holds summed for them.
        maps.insert("calls[1074025674]".into(), 1000.0);
        maps.insert("helper_holds[1074025674]".into(), 770.0);
        let refused = summarize(&maps).unwrap_err().to_string();
        assert!(
            refused.contains("770") && refused.contains("TUNSETIFF") && refused.contains("dropped"),
            "{refused}"
        );
        // One call that straddles a window's edge is not a drop.
        maps.insert("helper_holds[1074025674]".into(), 999.0);
        summarize(&maps).unwrap();
    }

    /// A cell where the worker handled a link event in under half the cycles gets no
    /// per-cycle hold and no ceiling: one event's mean is not what every cycle held. The
    /// host's own total per cycle stays.
    #[test]
    fn link_events_seen_in_few_cycles_give_no_per_cycle_hold() {
        let mut maps = std::collections::BTreeMap::new();
        add_scalars(TIMED, &mut maps);
        add_scalars(STEADY, &mut maps);
        let cycles = maps["calls[1]"];
        for kind in ["lw_tap_devs[1]", "lw_tap_devs[0]"] {
            let mut few = maps.clone();
            few.insert(kind.into(), cycles * 0.25);
            let s = summarize(&few).unwrap();
            assert!(s["rtnl_hold_us_per_cycle"].is_null(), "{kind}: {s}");
            assert!(s["rtnl_ceiling_cycles_per_s"].is_null(), "{kind}: {s}");
            assert!(
                s["link_events"]["rtnl_hold_us_per_cycle"].is_null(),
                "{kind}: {s}"
            );
            assert!(
                s["window_rtnl_hold_us_per_cycle"].as_f64().unwrap() > 0.0,
                "{kind}: {s}"
            );
            assert!(
                s["helper_rtnl_hold_us_per_cycle"].as_f64().unwrap() > 0.0,
                "{kind}: {s}"
            );
        }
        // Half an event per cycle, each kind, is enough.
        let mut half = maps.clone();
        half.insert("lw_tap_devs[1]".into(), cycles * 0.5);
        half.insert("lw_tap_devs[0]".into(), cycles * 0.5);
        assert!(summarize(&half).unwrap()["rtnl_hold_us_per_cycle"].is_f64());
    }

    #[test]
    fn a_window_the_tracer_missed_is_an_error_not_a_zero() {
        let mut maps = std::collections::BTreeMap::new();
        add_scalars(
            "Attaching 14 probes...\n@other_hold_us[ip]: 77\n",
            &mut maps,
        );
        assert!(summarize(&maps).is_err());
        // A dump that saw closes but no TUNSETIFF lost the start of the timed phase.
        add_scalars("@calls[1]: 3\n", &mut maps);
        assert!(summarize(&maps).is_err());
        // Every call seen, and no link event of a TAP device: half the cost is missing.
        let mut maps = std::collections::BTreeMap::new();
        add_scalars(TIMED, &mut maps);
        assert!(summarize(&maps).is_err());
    }
}
