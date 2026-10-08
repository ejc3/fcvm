//! The `burn` guest action: keep every online CPU away from SCHED_OTHER tasks, kernel
//! workers included, for a fixed time.
//!
//! A guest vsock connect completes only when a kernel worker processes the host's
//! RESPONSE: the virtio-vsock RX interrupt hands the packet to a per-CPU workqueue, whose
//! kworkers are SCHED_OTHER. A burn armed right before the connect keeps those kworkers off
//! every CPU, so the connect cannot complete until the burn ends. That is how a VM test
//! shows a vsock connect completing at a measured duration above Linux's 2 s default
//! (#1080).
//!
//! Three things would otherwise let SCHED_OTHER work run. The burn changes each one for its
//! duration and puts back the value it found:
//!  * RT throttling: `/proc/sys/kernel/sched_rt_runtime_us` is set to -1.
//!  * The fair server (Linux 6.12+), which runs SCHED_OTHER tasks for 50 ms of every
//!    1000 ms on each CPU however much RT load there is. Only its per-CPU debugfs `runtime`
//!    file turns it off, and the burn writes 0 there for every online CPU. It reaches those
//!    files through a debugfs mount of its own that is attached nowhere (fsopen and
//!    fsmount): the mount exists only as a file descriptor, so the guest's mount table is
//!    never changed, and closing the descriptor releases it.
//!  * The hitting thread's policy: it is raised to SCHED_FIFO above the burners, so it can
//!    still run to issue its operation, and it sleeps in that operation while they spin.
//!
//! Then one thread per online CPU pins itself there, sets itself SCHED_FIFO, reports, and
//! waits. The hitting thread releases them as the last thing the hit does before it returns,
//! and they spin until the deadline. Everything that allocates or prints (the ready reports,
//! the `BURN started` line, the guard's name) happens before the release. musl's allocator
//! lock and std's stderr lock have no priority inheritance, so taking one while the burners
//! spin would leave the hitting thread asleep behind whichever starved SCHED_OTHER thread
//! held it, through the whole burn.
//!
//! A step that fails aborts the burn with a `BURN ABORTED` marker naming the cause, and
//! what was already changed is restored, so a test that expected the stall fails at its
//! assertion instead of passing on an operation nothing stalled. The restore puts a value
//! back only while it still reads what the burn wrote; a value something else changed is
//! left as found and, like a failed write, printed as `BURN RESTORE FAILED`.
//!
//! Root-only (fc-agent runs as root in the guest). Never arm it on a host: it starves every
//! CPU of the machine it runs on, and fcvm's own arming (`arm_from_env`) refuses it.

use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

const SCHED_RT_RUNTIME_US: &str = "/proc/sys/kernel/sched_rt_runtime_us";
const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";
/// The fair server's directory inside a debugfs mount.
const FAIR_SERVER: &str = "sched/fair_server";

/// What the burn writes. The restore puts the old value back only while a setting still
/// reads this.
const RT_RUNTIME_OFF: &str = "-1";
const FAIR_SERVER_OFF: &str = "0";

/// SCHED_FIFO priority of the burners, and (one higher) of the hitting thread, so the
/// hitting thread preempts a burner whenever it is runnable.
const BURNER_PRIO: libc::c_int = 1;
const CALLER_PRIO: libc::c_int = 2;

/// Set while a burn owns the scheduler settings (see [`Claim`]).
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Ownership of the scheduler settings for one burn. At most one exists, so a burn that
/// starts while another runs cannot save the first one's -1 and 0 as the values it later
/// puts back. Dropping it releases the settings.
struct Claim(PhantomData<()>);

impl Claim {
    fn take() -> Option<Claim> {
        ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Claim(PhantomData))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

/// A scheduling policy and its static priority, as the kernel reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Policy {
    policy: libc::c_int,
    priority: libc::c_int,
}

const CALLER_FIFO: Policy = Policy {
    policy: libc::SCHED_FIFO,
    priority: CALLER_PRIO,
};
const BURNER_FIFO: Policy = Policy {
    policy: libc::SCHED_FIFO,
    priority: BURNER_PRIO,
};

// The policy calls are raw syscalls on the calling thread (tid 0). musl's
// sched_getscheduler, sched_getparam and sched_setscheduler are stubs that fail with ENOSYS,
// because Linux applies them per thread and POSIX defines them per process.

/// The calling thread's policy.
fn current_policy() -> std::io::Result<Policy> {
    // SAFETY: sched_getscheduler takes a tid and reads nothing from memory.
    let policy = unsafe { libc::syscall(libc::SYS_sched_getscheduler, 0 as libc::pid_t) };
    if policy < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: an all-zero sched_param is valid. The kernel writes its one int field, which
    // is the first field of libc's struct, and `param` outlives the call.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sched_getparam,
            0 as libc::pid_t,
            &mut param as *mut libc::sched_param,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Policy {
        policy: policy as libc::c_int,
        priority: param.sched_priority,
    })
}

/// Put the calling thread under `policy`.
fn set_policy(policy: Policy) -> std::io::Result<()> {
    // SAFETY: an all-zero sched_param is valid; only the priority is set. The kernel reads
    // its one int field, and `param` outlives the call.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = policy.priority;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sched_setscheduler,
            0 as libc::pid_t,
            policy.policy,
            &param as *const libc::sched_param,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Pin the calling thread to `cpu`.
fn pin_self_to(cpu: usize) -> std::io::Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(std::io::Error::other(format!(
            "CPU {cpu} is beyond the {} a cpu_set_t holds",
            libc::CPU_SETSIZE
        )));
    }
    // SAFETY: a zeroed cpu_set_t is valid and CPU_SET writes inside it (`cpu` is in range).
    // sched_setaffinity reads `set` for its size, and `set` outlives the call.
    let rc = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set)
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Mount debugfs attached nowhere, returning the mount's root. The descriptor is the only
/// way to reach the mount, and closing it releases the mount, so the guest's mount table is
/// never changed. The constants are from include/uapi/linux/mount.h.
fn open_detached_debugfs() -> std::io::Result<OwnedFd> {
    const FSOPEN_CLOEXEC: libc::c_uint = 0x1;
    const FSCONFIG_CMD_CREATE: libc::c_uint = 6;
    const FSMOUNT_CLOEXEC: libc::c_uint = 0x1;
    // SAFETY: the filesystem name is a NUL-terminated string that outlives the call.
    let context = unsafe { libc::syscall(libc::SYS_fsopen, c"debugfs".as_ptr(), FSOPEN_CLOEXEC) };
    if context < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fsopen returned a new descriptor that nothing else owns.
    let context = unsafe { OwnedFd::from_raw_fd(context as libc::c_int) };
    // SAFETY: FSCONFIG_CMD_CREATE reads no key, value, or aux argument, so they are null
    // and zero.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_fsconfig,
            context.as_raw_fd(),
            FSCONFIG_CMD_CREATE,
            std::ptr::null::<libc::c_char>(),
            std::ptr::null::<libc::c_void>(),
            0 as libc::c_int,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `context` holds a created superblock; no mount attributes are requested.
    let mount = unsafe {
        libc::syscall(
            libc::SYS_fsmount,
            context.as_raw_fd(),
            FSMOUNT_CLOEXEC,
            0 as libc::c_uint,
        )
    };
    if mount < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fsmount returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(mount as libc::c_int) })
}

/// Parse a kernel CPU list such as `0-3,6,8-9` (the format of
/// `/sys/devices/system/cpu/online`).
fn parse_cpu_list(list: &str) -> Result<Vec<usize>, String> {
    let list = list.trim();
    if list.is_empty() {
        return Err("empty CPU list".to_string());
    }
    let mut cpus = Vec::new();
    for range in list.split(',') {
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        let parse = |cpu: &str| {
            cpu.parse::<usize>()
                .map_err(|e| format!("bad CPU {cpu:?} in {list:?}: {e}"))
        };
        let (first, last) = (parse(first)?, parse(last)?);
        if first > last {
            return Err(format!("CPU range {range:?} runs backwards in {list:?}"));
        }
        cpus.extend(first..=last);
    }
    Ok(cpus)
}

fn read_setting(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|value| value.trim().to_string())
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Put `old` back at `path` if it still reads `wrote`, the value the burn wrote there.
fn restore_if_unchanged(path: &Path, wrote: &str, old: &str) -> Result<(), String> {
    let now = read_setting(path)?;
    if now != wrote {
        return Err(format!(
            "it reads {now}, not the {wrote} the burn wrote, so it is left as found"
        ));
    }
    std::fs::write(path, old.as_bytes()).map_err(|e| e.to_string())
}

/// The fair server the burn turned off: the detached debugfs mount it is reached through,
/// and the runtime each CPU's fair server had.
struct FairServer {
    debugfs: OwnedFd,
    old: Vec<(usize, String)>,
}

impl FairServer {
    /// A CPU's fair-server `runtime` file, reached through the detached mount.
    fn runtime(&self, cpu: usize) -> PathBuf {
        PathBuf::from(format!(
            "/proc/self/fd/{}/{FAIR_SERVER}/cpu{cpu}/runtime",
            self.debugfs.as_raw_fd()
        ))
    }
}

/// What a burn changed and the value to put back. Dropping it restores each setting in the
/// reverse order of the changes, on the thread that drops it (which must be the hitting
/// thread, whose own policy it restores), then releases the [`Claim`].
struct Settings {
    name: String,
    rt_runtime: Option<String>,
    fair_server: Option<FairServer>,
    caller_policy: Option<Policy>,
    _claim: Claim,
}

impl Drop for Settings {
    fn drop(&mut self) {
        let name = &self.name;
        let mut restored = Vec::new();
        let failed = |what: String, e: &dyn std::fmt::Display| {
            eprintln!("FAILPOINT {name} BURN RESTORE FAILED: {what}: {e}");
        };
        if let Some(policy) = self.caller_policy.take() {
            let what = format!("hitting thread policy {policy:?}");
            match current_policy() {
                Ok(now) if now == CALLER_FIFO => match set_policy(policy) {
                    Ok(()) => restored.push(what),
                    Err(e) => failed(what, &e),
                },
                Ok(now) => failed(
                    what,
                    &format!(
                        "the thread is {now:?}, not the {CALLER_FIFO:?} the burn set, so it is \
                         left as found"
                    ),
                ),
                Err(e) => failed(what, &e),
            }
        }
        if let Some(mut fair) = self.fair_server.take() {
            for (cpu, value) in std::mem::take(&mut fair.old).into_iter().rev() {
                let what = format!("{FAIR_SERVER}/cpu{cpu}/runtime = {value}");
                match restore_if_unchanged(&fair.runtime(cpu), FAIR_SERVER_OFF, &value) {
                    Ok(()) => restored.push(what),
                    Err(e) => failed(what, &e),
                }
            }
            // `fair` drops here, closing the detached debugfs mount.
        }
        if let Some(value) = self.rt_runtime.take() {
            let what = format!("{SCHED_RT_RUNTIME_US} = {value}");
            match restore_if_unchanged(Path::new(SCHED_RT_RUNTIME_US), RT_RUNTIME_OFF, &value) {
                Ok(()) => restored.push(what),
                Err(e) => failed(what, &e),
            }
        }
        if !restored.is_empty() {
            eprintln!("FAILPOINT {name} BURN restored: {}", restored.join(", "));
        }
        // `_claim` drops after this body, so the settings are released only once restored.
    }
}

/// Where the burners are. They wait while it reads WAITING, spin once it reads RELEASED,
/// and exit without spinning once it reads ABORTED.
const WAITING: u8 = 0;
const RELEASED: u8 = 1;
const ABORTED: u8 = 2;

/// What the burners share with the hitting thread.
struct Latch {
    state: AtomicU8,
    /// Burners that ran their set-up step, failed or not.
    reported: AtomicUsize,
    /// The set-up failures they reported.
    failures: Mutex<Vec<String>>,
    /// Burners that were released and spun.
    spun: AtomicUsize,
}

/// One burner per CPU, waiting for [`Burners::release`].
struct Burners {
    latch: Arc<Latch>,
    handles: Vec<JoinHandle<()>>,
}

impl Burners {
    /// Let the burners spin. This allocates nothing and takes no lock (one store and one
    /// futex wake per burner), so it can be the hitting thread's last act before the
    /// operation to stall.
    fn release(&self) {
        self.latch.state.store(RELEASED, Ordering::Release);
        for handle in &self.handles {
            handle.thread().unpark();
        }
    }

    /// Tell burners that were never released to exit, then join every burner (a released
    /// one exits at the deadline). Returns how many spun and how many panicked.
    fn stop_and_join(&mut self) -> (usize, usize) {
        let _ = self.latch.state.compare_exchange(
            WAITING,
            ABORTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        for handle in &self.handles {
            handle.thread().unpark();
        }
        let panicked = self
            .handles
            .drain(..)
            .map(JoinHandle::join)
            .filter(Result::is_err)
            .count();
        (self.latch.spun.load(Ordering::Acquire), panicked)
    }
}

impl Drop for Burners {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// A running burn. Dropping it waits for the deadline, joins the burners, then restores the
/// settings (see [`Settings`]). The burn always lasts its full duration, even when the
/// operation it was armed for finishes early, such as a connect that timed out.
pub(crate) struct Burn {
    started: Instant,
    burners: Burners,
    settings: Settings,
}

impl Burn {
    /// Let the burners spin: the hit's last act before it returns. From here until the
    /// guard drops, the hitting thread must not allocate, print, or take a lock.
    pub(crate) fn release(&self) {
        self.burners.release();
    }
}

impl Drop for Burn {
    fn drop(&mut self) {
        let total = self.burners.handles.len();
        let (spun, panicked) = self.burners.stop_and_join();
        eprintln!(
            "FAILPOINT {} BURN ended after {} ms; {spun} of {total} burner(s) spun{}",
            self.settings.name,
            self.started.elapsed().as_millis(),
            if panicked > 0 {
                format!(" ({panicked} panicked)")
            } else {
                String::new()
            }
        );
        // `settings` drops after this body: the restore runs once every burner has exited.
    }
}

/// Set up a burn of `dur` and return it with its burners waiting for [`Burn::release`], or
/// print `BURN ABORTED` with the cause and return `None` with every setting restored. Call
/// it on the thread that then runs the operation to be stalled; dropping the returned burn
/// must happen on that thread too. `BURN started` is printed here, before the release,
/// because nothing may print once the burners spin.
pub(crate) fn start(name: &str, dur: Duration) -> Option<Burn> {
    let Some(claim) = Claim::take() else {
        eprintln!(
            "FAILPOINT {name} BURN ABORTED: another burn still holds the scheduler settings, \
             so their current values are not the ones to restore"
        );
        return None;
    };
    let started = Instant::now();
    let mut settings = Settings {
        name: name.to_string(),
        rt_runtime: None,
        fair_server: None,
        caller_policy: None,
        _claim: claim,
    };
    match set_up(&mut settings, started + dur) {
        Ok((cpus, burners)) => {
            let fair: Vec<String> = settings
                .fair_server
                .iter()
                .flat_map(|fair| &fair.old)
                .map(|(cpu, old)| {
                    format!("{FAIR_SERVER}/cpu{cpu}/runtime {old} -> {FAIR_SERVER_OFF}")
                })
                .collect();
            eprintln!(
                "FAILPOINT {name} BURN started: {} SCHED_FIFO {BURNER_PRIO} burner(s) on CPU(s) \
                 {cpus:?} for {} ms; fair server off ({}); {SCHED_RT_RUNTIME_US} {} -> \
                 {RT_RUNTIME_OFF}; hitting thread {:?} -> {CALLER_FIFO:?}",
                burners.handles.len(),
                dur.as_millis(),
                fair.join(", "),
                settings.rt_runtime.as_deref().unwrap_or("?"),
                settings.caller_policy,
            );
            Some(Burn {
                started,
                burners,
                settings,
            })
        }
        Err(cause) => {
            eprintln!("FAILPOINT {name} BURN ABORTED: {cause}");
            drop(settings);
            None
        }
    }
}

/// Change the settings (recording each in `settings` as it is changed), start one waiting
/// burner per online CPU, and raise the hitting thread. On failure every burner already
/// started has exited without spinning and been joined.
fn set_up(settings: &mut Settings, deadline: Instant) -> Result<(Vec<usize>, Burners), String> {
    let cpus = parse_cpu_list(&read_setting(Path::new(ONLINE_CPUS))?)
        .map_err(|e| format!("{ONLINE_CPUS}: {e}"))?;

    let rt = read_setting(Path::new(SCHED_RT_RUNTIME_US))?;
    std::fs::write(SCHED_RT_RUNTIME_US, RT_RUNTIME_OFF.as_bytes())
        .map_err(|e| format!("cannot turn RT throttling off via {SCHED_RT_RUNTIME_US}: {e}"))?;
    settings.rt_runtime = Some(rt);

    let debugfs =
        open_detached_debugfs().map_err(|e| format!("cannot mount debugfs, detached: {e}"))?;
    let fair = settings.fair_server.insert(FairServer {
        debugfs,
        old: Vec::with_capacity(cpus.len()),
    });
    for &cpu in &cpus {
        let path = fair.runtime(cpu);
        let old = read_setting(&path).map_err(|e| {
            format!(
                "{e}; without {FAIR_SERVER} the fair server cannot be turned off and would run \
                 SCHED_OTHER tasks for 50 ms of every second"
            )
        })?;
        std::fs::write(&path, FAIR_SERVER_OFF.as_bytes()).map_err(|e| {
            format!("cannot turn the fair server off via {FAIR_SERVER}/cpu{cpu}/runtime: {e}")
        })?;
        fair.old.push((cpu, old));
    }

    let burners = spawn_burners(&cpus, deadline, prepare_burner)?;

    let caller = current_policy()
        .map_err(|e| format!("cannot read the hitting thread's scheduling policy: {e}"))?;
    set_policy(CALLER_FIFO)
        .map_err(|e| format!("cannot raise the hitting thread to {CALLER_FIFO:?}: {e}"))?;
    settings.caller_policy = Some(caller);
    Ok((cpus, burners))
}

/// A burner's set-up step: pin to `cpu` and run as [`BURNER_FIFO`].
fn prepare_burner(cpu: usize) -> Result<(), String> {
    pin_self_to(cpu).map_err(|e| format!("CPU {cpu}: cannot pin the burner: {e}"))?;
    set_policy(BURNER_FIFO)
        .map_err(|e| format!("CPU {cpu}: cannot set the burner to {BURNER_FIFO:?}: {e}"))
}

/// Start one burner per CPU in `cpus` and return once each has run `prepare` and is
/// waiting. They spin only once [`Burners::release`] is called, and only until `deadline`.
/// On failure every burner started has exited without spinning and been joined.
fn spawn_burners(
    cpus: &[usize],
    deadline: Instant,
    prepare: fn(usize) -> Result<(), String>,
) -> Result<Burners, String> {
    let latch = Arc::new(Latch {
        state: AtomicU8::new(WAITING),
        reported: AtomicUsize::new(0),
        failures: Mutex::new(Vec::new()),
        spun: AtomicUsize::new(0),
    });
    // Dropped on the error return, which stops and joins whatever was started.
    let mut burners = Burners {
        latch: Arc::clone(&latch),
        handles: Vec::with_capacity(cpus.len()),
    };
    let mut failures = Vec::new();
    for &cpu in cpus {
        let (latch, hitting) = (Arc::clone(&latch), std::thread::current());
        match std::thread::Builder::new()
            .name(format!("fcvm-burn{cpu}"))
            .spawn(move || burner(cpu, deadline, &latch, prepare, &hitting))
        {
            Ok(handle) => burners.handles.push(handle),
            Err(e) => {
                failures.push(format!("cannot spawn the burner for CPU {cpu}: {e}"));
                break;
            }
        }
    }
    while latch.reported.load(Ordering::Acquire) < burners.handles.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            failures.push("a burner did not report ready before the deadline".to_string());
            break;
        }
        std::thread::park_timeout(left);
    }
    failures.append(
        &mut latch
            .failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    if failures.is_empty() {
        Ok(burners)
    } else {
        Err(failures.join("; "))
    }
}

/// One burner: run `prepare`, report to the hitting thread, wait for the release, then spin
/// until `deadline`. After the report it allocates nothing and takes no lock.
fn burner(
    cpu: usize,
    deadline: Instant,
    latch: &Latch,
    prepare: fn(usize) -> Result<(), String>,
    hitting: &Thread,
) {
    let prepared = prepare(cpu);
    let ready = prepared.is_ok();
    if let Err(e) = prepared {
        latch
            .failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(e);
    }
    latch.reported.fetch_add(1, Ordering::Release);
    hitting.unpark();
    if !ready {
        return;
    }
    loop {
        match latch.state.load(Ordering::Acquire) {
            WAITING => std::thread::park(),
            RELEASED => break,
            _ => return,
        }
    }
    latch.spun.fetch_add(1, Ordering::AcqRel);
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The online-CPU list is read in the kernel's range format, not assumed to be 0..n.
    #[test]
    fn a_cpu_list_parses_in_the_kernel_range_format() {
        assert_eq!(parse_cpu_list("0\n"), Ok(vec![0]));
        assert_eq!(parse_cpu_list("0-3"), Ok(vec![0, 1, 2, 3]));
        assert_eq!(parse_cpu_list("0,2-3,7"), Ok(vec![0, 2, 3, 7]));
        assert_eq!(parse_cpu_list("4-5,9\n"), Ok(vec![4, 5, 9]));
        assert!(parse_cpu_list("").is_err());
        assert!(parse_cpu_list("3-1").is_err());
        assert!(parse_cpu_list("0,,1").is_err());
        assert!(parse_cpu_list("x").is_err());
    }

    /// A second burn cannot take the settings while a first holds them, and its refusal
    /// leaves the first one's claim in place. Only this test touches the claim.
    #[test]
    fn a_second_burn_cannot_claim_the_settings_while_one_holds_them() {
        let first = Claim::take().expect("the first claim");
        assert!(
            Claim::take().is_none(),
            "a second burn claimed settings a first burn holds"
        );
        assert!(
            Claim::take().is_none(),
            "a refused claim released the first burn's claim"
        );
        drop(first);
        assert!(
            Claim::take().is_some(),
            "a dropped claim did not release the settings"
        );
    }

    /// Burners spin only after the release, which the hit performs as its last act, so the
    /// hitting thread never allocates or prints while a burner holds a CPU. A burner that
    /// is never released exits without spinning instead of running to its deadline. The
    /// set-up step is a no-op here, so nothing is pinned or made real-time on the host.
    #[test]
    fn burners_spin_only_after_the_release() {
        let unprepared: fn(usize) -> Result<(), String> = |_| Ok(());

        let spawned = Instant::now();
        let mut unreleased =
            spawn_burners(&[0, 1], spawned + Duration::from_secs(60), unprepared).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            unreleased.latch.spun.load(Ordering::Acquire),
            0,
            "a burner spun before the release"
        );
        assert_eq!(unreleased.stop_and_join(), (0, 0));
        assert!(
            spawned.elapsed() < Duration::from_secs(30),
            "burners never released ran toward their deadline instead of exiting"
        );

        let deadline = Instant::now() + Duration::from_millis(600);
        let mut released = spawn_burners(&[0, 1], deadline, unprepared).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            released.latch.spun.load(Ordering::Acquire),
            0,
            "a burner spun before the release"
        );
        released.release();
        assert_eq!(
            released.stop_and_join(),
            (2, 0),
            "both released burners must spin"
        );
        assert!(
            Instant::now() >= deadline,
            "a released burner stopped before its deadline"
        );
    }
}
