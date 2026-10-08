//! Deterministic failpoints for lifecycle interleaving tests.
//!
//! # Purpose
//!
//! Lifecycle transitions (VM pause for snapshot, snapshot restore, the Healthy
//! state-file persist) race against in-flight client work (exec handshakes,
//! port-forward curls, serial writes). Those races have windows measured in
//! microseconds, so stress tests hit them probabilistically. A failpoint turns a
//! chosen window into a deterministic, individually replayable interleaving: the
//! harness arms a named point, the code path holds there (sleep or block on a
//! file), and the harness drives the *other* side of the race into the widened
//! window.
//!
//! This is test-only instrumentation following the existing `FCVM_*` env-gated
//! pattern (`FCVM_FUSE_TRACE_RATE`, `FCVM_KVM_TRACE`, ...). Unarmed — the only
//! state production runs ever see — [`hit`] is a single static lookup returning
//! `None`.
//!
//! # Marker-line contract
//!
//! Harnesses sequence on stderr marker lines:
//!
//! ```text
//! FAILPOINT <name> reached action=<action>
//! FAILPOINT <name> released
//! ```
//!
//! "reached" is printed before the action starts, "released" after it completes;
//! a harness that sees "reached" knows the code path is parked inside the window
//! and may act. A `burn` hit with [`hit_scoped`] completes when the returned
//! guard is dropped, so its hitting thread runs its next operation between the
//! two markers. A `block_until_file` point that hits its 300 s hard cap prints a
//! loud `RELEASED BY TIMEOUT` line first — a wedged harness must not wedge the
//! VM forever. Arming prints `FAILPOINT armed spec=<spec>` once.
//!
//! Guest caveat: fc-agent's console output is buffered while the console is
//! quiesced for a snapshot (see fc-agent's `console` module), so a guest marker
//! printed inside a quiesce window (e.g. `cache_ready.pre_send`) reaches the
//! host log only after the guard drops. Sequence on host-side markers, or use
//! guest sleeps for timing rather than treating guest markers as sync points.
//!
//! # Spec grammar
//!
//! A spec is comma-separated entries, each `<name>:<action>:<arg>`:
//!
//! * `<name>:sleep:<ms>` — hold for `<ms>` milliseconds.
//! * `<name>:burn:<ms>`: keep every SCHED_OTHER task in the guest, kernel workers
//!   included, off every CPU for `<ms>` milliseconds, so an operation that needs a
//!   kernel worker to complete (a vsock connect) cannot complete until the burn ends.
//!   Hit it with [`hit_scoped`] so the hitting thread's next operation runs during the
//!   burn; [`hit`] waits the burn out. Capped at 15 s. Guest-only and root-only
//!   (failpoint/src/burn.rs).
//! * `<name>:block_until_file:<path>` — poll every 10ms until `<path>` exists,
//!   hard-capped at 300 s. Host-only (see below). `<path>` may contain `:`.
//!
//! # Arming
//!
//! * Host: `fcvm` calls [`arm_from_env`] once at startup; set `FCVM_FAILPOINT`. A host
//!   spec cannot hold `burn`, which would starve every CPU of the host.
//! * Guest: fc-agent cannot read host env. `fcvm` forwards `FCVM_GUEST_FAILPOINT`
//!   onto the kernel cmdline as `fcvm_failpoint=<spec>` (the `fuse_trace_rate`
//!   pattern); fc-agent parses `/proc/cmdline` at startup and calls
//!   [`arm_from_str`]. Guest specs take `sleep` and `burn` only and are whitespace-free:
//!   `block_until_file` is host-only (the harness cannot create files inside the
//!   guest, and a file-blocked guest holds VM-global state for the full cap) —
//!   enforced host-side by [`validate_guest_spec`] before the VM boots.
//!
//! Malformed specs panic: a silently un-armed failpoint would make a
//! determinism test vacuously pass, which is worse than failing loudly.
//!
//! # Guest failpoints and snapshot cache keys
//!
//! Guest failpoints ride the kernel cmdline, and the armed spec is baked into
//! the guest's boot (and therefore into any snapshot taken of it). The runtime
//! boot-args string itself is deliberately excluded from the snapshot cache key,
//! so `FirecrackerConfig` carries a dedicated `guest_failpoint` field (populated
//! from `FCVM_GUEST_FAILPOINT`, skip-serialized when unset) that feeds the key:
//! a run with guest failpoints armed computes a different snapshot key than a
//! normal run. Fuzz VMs therefore never pollute normal snapshot caches — they
//! create and restore their own entries — and normal runs can never restore a
//! snapshot with failpoints armed.

mod burn;

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Poll interval for `block_until_file`.
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Hard cap on `block_until_file`: a wedged harness must not wedge the VM
/// forever. It MUST exceed every harness budget that can elapse while a point
/// is held (the largest today is a 180s snapshot-create wait) — a cap below
/// them fires first and silently rewrites the interleaving under test into
/// exactly the race the hold was supposed to remove.
const BLOCK_CAP: Duration = Duration::from_secs(300);

/// Cap on `burn`: above the restore ACK's 10 s connect deadline, the longest wait a burn
/// exists to outlast, and short enough that a typo cannot hold every guest CPU for minutes.
const BURN_CAP: Duration = Duration::from_secs(15);

/// What an armed failpoint does when hit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Hold for the given duration.
    Sleep(Duration),
    /// Poll every [`BLOCK_POLL_INTERVAL`] until the file exists, capped at
    /// [`BLOCK_CAP`]. Host-only (see crate docs).
    BlockUntilFile(PathBuf),
    /// Keep every SCHED_OTHER task off every CPU for the given duration. Guest-only and
    /// root-only (see the crate docs and the `burn` module).
    Burn(Duration),
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Sleep(d) => write!(f, "sleep:{}ms", d.as_millis()),
            Action::BlockUntilFile(p) => write!(f, "block_until_file:{}", p.display()),
            Action::Burn(d) => write!(f, "burn:{}ms", d.as_millis()),
        }
    }
}

/// Armed failpoints. Never set = unarmed; `Some(None)` = armed with nothing
/// (env var unset). Either way [`hit`] is one static lookup on the fast path.
static ARMED: OnceLock<Option<HashMap<String, Action>>> = OnceLock::new();

fn armed_action(name: &str) -> Option<&'static Action> {
    match ARMED.get() {
        Some(Some(map)) => map.get(name),
        _ => None,
    }
}

/// Parse a failpoint spec (see crate docs for the grammar).
fn parse_spec(spec: &str) -> Result<HashMap<String, Action>, String> {
    let mut map = HashMap::new();
    for entry in spec.split(',') {
        let mut parts = entry.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        let kind = parts.next();
        let arg = parts.next();
        if name.is_empty() {
            return Err(format!("entry {entry:?}: empty failpoint name"));
        }
        let action = match (kind, arg) {
            (Some("sleep"), Some(ms)) => {
                let ms: u64 = ms
                    .parse()
                    .map_err(|e| format!("entry {entry:?}: bad sleep milliseconds: {e}"))?;
                // Same cap as block_until_file: a typo'd hold must not wedge the
                // VMM or guest beyond the harness's patience. Reject, don't clamp
                // — a silently shortened hold would make interleavings lie.
                if Duration::from_millis(ms) > BLOCK_CAP {
                    return Err(format!(
                        "entry {entry:?}: sleep exceeds the {}s cap",
                        BLOCK_CAP.as_secs()
                    ));
                }
                Action::Sleep(Duration::from_millis(ms))
            }
            (Some("burn"), Some(ms)) => {
                let ms: u64 = ms
                    .parse()
                    .map_err(|e| format!("entry {entry:?}: bad burn milliseconds: {e}"))?;
                // Reject, don't clamp, as for sleep.
                if Duration::from_millis(ms) > BURN_CAP {
                    return Err(format!(
                        "entry {entry:?}: burn exceeds the {}s cap",
                        BURN_CAP.as_secs()
                    ));
                }
                Action::Burn(Duration::from_millis(ms))
            }
            (Some("block_until_file"), Some(path)) if !path.is_empty() => {
                Action::BlockUntilFile(PathBuf::from(path))
            }
            (Some("block_until_file"), _) => {
                return Err(format!("entry {entry:?}: block_until_file needs a path"));
            }
            (Some(other), _) => {
                return Err(format!(
                    "entry {entry:?}: unknown action {other:?} (expected sleep, burn, or block_until_file)"
                ));
            }
            (None, _) => {
                return Err(format!("entry {entry:?}: expected <name>:<action>:<arg>"));
            }
        };
        if map.insert(name.to_string(), action).is_some() {
            return Err(format!("failpoint {name:?} specified twice"));
        }
    }
    Ok(map)
}

/// Arm failpoints from the `FCVM_FAILPOINT` env var (the host path). Unset arms
/// nothing (and keeps the fast path). Called once at process startup; panics on a
/// malformed spec (see crate docs), on a `burn` entry, or if failpoints are already
/// armed.
pub fn arm_from_env() {
    match std::env::var("FCVM_FAILPOINT") {
        Ok(spec) => arm_host(&spec),
        Err(_) => {
            let _ = ARMED.set(None);
        }
    }
}

/// [`arm_from_str`] for the host: a `burn` entry is refused like a malformed one, because
/// a burn armed in fcvm would starve every CPU of the host.
fn arm_host(spec: &str) {
    let map = parse_spec(spec).and_then(|map| {
        let burn = map
            .iter()
            .find_map(|(name, action)| matches!(action, Action::Burn(_)).then_some(name));
        match burn {
            Some(name) => Err(format!(
                "failpoint {name:?}: burn is guest-only (FCVM_GUEST_FAILPOINT); armed in fcvm \
                 it would starve every CPU of the host"
            )),
            None => Ok(map),
        }
    });
    arm(spec, map);
}

/// Arm failpoints from a spec string (the guest path: fc-agent passes the
/// `fcvm_failpoint=` kernel cmdline value). Panics on a malformed spec (see
/// crate docs) or if failpoints are already armed.
pub fn arm_from_str(spec: &str) {
    arm(spec, parse_spec(spec));
}

fn arm(spec: &str, map: Result<HashMap<String, Action>, String>) {
    let map = map.unwrap_or_else(|e| panic!("invalid failpoint spec {spec:?}: {e}"));
    if ARMED.set(Some(map)).is_err() {
        panic!("failpoints already armed (arm_from_env/arm_from_str called twice)");
    }
    eprintln!("FAILPOINT armed spec={spec}");
}

/// Validate a spec for forwarding to the guest kernel cmdline: parseable,
/// whitespace-free (the cmdline is space-delimited), and `sleep` or `burn` only
/// (`block_until_file` is host-only — see crate docs).
pub fn validate_guest_spec(spec: &str) -> Result<(), String> {
    if spec.chars().any(|c| c.is_whitespace()) {
        return Err(
            "guest failpoint spec must not contain whitespace (kernel cmdline is space-delimited)"
                .to_string(),
        );
    }
    for (name, action) in parse_spec(spec)? {
        if matches!(action, Action::BlockUntilFile(_)) {
            return Err(format!(
                "failpoint {name:?}: block_until_file is host-only; guest failpoints support sleep and burn only"
            ));
        }
    }
    Ok(())
}

/// Hit a failpoint from a sync context (plain threads, `spawn_blocking`).
/// Zero-cost when unarmed. When armed for `name`: print the "reached" marker,
/// perform the action (blocking this thread), print "released". A burn holds this
/// thread until it ends.
pub fn hit(name: &str) {
    if let Some(action) = armed_action(name) {
        perform_sync(name, action, BLOCK_CAP);
    }
}

/// Hit a failpoint from a sync context and keep a burn running after the return.
///
/// Every other action is performed as by [`hit`] before this returns, and its "released"
/// marker is printed when the guard drops. A burn is only
/// started: the hitting thread runs its next operation while the burn keeps SCHED_OTHER
/// work off every CPU, and dropping the returned guard waits for the burn's deadline,
/// restores what it changed (including this thread's raised priority), and prints
/// "released". The guard cannot leave the thread, because the priority it restores is
/// that thread's own.
///
/// The burners start spinning as this returns, so between the return and the operation
/// the caller must not allocate, print, or take a lock: none of those locks has priority
/// inheritance, and a starved SCHED_OTHER thread holding one would keep this thread asleep
/// through the burn.
#[must_use = "the burn ends when the guard is dropped, so the operation to stall must run while it is held"]
pub fn hit_scoped(name: &str) -> Hold {
    match armed_action(name) {
        Some(action) => begin_sync(name, action, BLOCK_CAP),
        None => Hold {
            name: None,
            burn: None,
            _thread: PhantomData,
        },
    }
}

/// Returned by [`hit_scoped`]: ends a running burn and prints "released" when dropped.
pub struct Hold {
    /// The armed failpoint, if any; `None` when the hit was unarmed.
    name: Option<String>,
    burn: Option<burn::Burn>,
    /// Not Send: dropping the guard restores the hitting thread's own scheduling policy.
    _thread: PhantomData<*const ()>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        drop(self.burn.take());
        if let Some(name) = &self.name {
            eprintln!("FAILPOINT {name} released");
        }
    }
}

/// Hit a failpoint from an async context. Same contract as [`hit`], but the
/// hold suspends the task (tokio sleep) instead of blocking the thread.
pub async fn hit_async(name: &str) {
    if let Some(action) = armed_action(name) {
        perform_async(name, action, BLOCK_CAP).await;
    }
}

fn perform_sync(name: &str, action: &Action, cap: Duration) {
    drop(begin_sync(name, action, cap));
}

/// Print "reached" and perform `action`, except that a burn is only started. The returned
/// guard ends the burn and prints "released" when dropped.
fn begin_sync(name: &str, action: &Action, cap: Duration) -> Hold {
    eprintln!("FAILPOINT {name} reached action={action}");
    let mut burn = None;
    match action {
        Action::Sleep(d) => std::thread::sleep(*d),
        Action::BlockUntilFile(path) => {
            let deadline = Instant::now() + cap;
            loop {
                if path.exists() {
                    break;
                }
                if Instant::now() >= deadline {
                    release_by_timeout(name, path, cap);
                    break;
                }
                std::thread::sleep(BLOCK_POLL_INTERVAL);
            }
        }
        Action::Burn(d) => burn = burn::start(name, *d),
    }
    let hold = Hold {
        name: Some(name.to_string()),
        burn,
        _thread: PhantomData,
    };
    // Last act before the return: everything above may allocate and print, which nothing
    // may do once the burners spin.
    if let Some(burn) = &hold.burn {
        burn.release();
    }
    hold
}

async fn perform_async(name: &str, action: &Action, cap: Duration) {
    eprintln!("FAILPOINT {name} reached action={action}");
    match action {
        Action::Sleep(d) => tokio::time::sleep(*d).await,
        Action::BlockUntilFile(path) => {
            let deadline = Instant::now() + cap;
            loop {
                if path.exists() {
                    break;
                }
                if Instant::now() >= deadline {
                    release_by_timeout(name, path, cap);
                    break;
                }
                tokio::time::sleep(BLOCK_POLL_INTERVAL).await;
            }
        }
        // A burn raises the hitting thread above its burners, and an async task does not
        // own its thread, so it is refused here instead of raising a runtime worker.
        Action::Burn(_) => eprintln!(
            "FAILPOINT {name} BURN ABORTED: a burn raises the thread that hits it, so it needs \
             failpoint::hit or failpoint::hit_scoped on a blocking thread, not hit_async"
        ),
    }
    eprintln!("FAILPOINT {name} released");
}

fn release_by_timeout(name: &str, path: &Path, cap: Duration) {
    eprintln!(
        "FAILPOINT {name} RELEASED BY TIMEOUT after {}s: {} never appeared \
         (wedged harness — releasing so the VM is not wedged too)",
        cap.as_secs(),
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "failpoint-test-{}-{}-{}",
            tag,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn test_parse_sleep_and_block() {
        let map = parse_spec("a.b:sleep:1500,c.d:block_until_file:/tmp/x").unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["a.b"], Action::Sleep(Duration::from_millis(1500)));
        assert_eq!(map["c.d"], Action::BlockUntilFile(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn test_parse_block_path_may_contain_colons() {
        let map = parse_spec("p:block_until_file:/tmp/a:b:c").unwrap();
        assert_eq!(
            map["p"],
            Action::BlockUntilFile(PathBuf::from("/tmp/a:b:c"))
        );
    }

    #[test]
    fn test_parse_rejects_malformed_entries() {
        assert!(parse_spec("").is_err()); // empty name
        assert!(parse_spec("noaction").is_err()); // missing action
        assert!(parse_spec("x:sleep").is_err()); // missing ms
        assert!(parse_spec("x:sleep:abc").is_err()); // non-numeric ms
        assert!(parse_spec("x:explode:1").is_err()); // unknown action
        assert!(parse_spec("x:burn").is_err()); // missing ms
        assert!(parse_spec("x:burn:abc").is_err()); // non-numeric ms
        assert!(parse_spec("x:block_until_file:").is_err()); // empty path
        assert!(parse_spec("x:sleep:1,x:sleep:2").is_err()); // duplicate name
        assert!(parse_spec(":sleep:1").is_err()); // empty name with action
    }

    #[test]
    fn test_validate_guest_spec() {
        assert!(validate_guest_spec("a:sleep:5,b:sleep:10").is_ok());
        assert!(validate_guest_spec("a:burn:4000").is_ok()); // burn is a guest action
        assert!(validate_guest_spec("a:sleep:5,b:burn:4000").is_ok());
        assert!(validate_guest_spec("a:block_until_file:/tmp/x").is_err()); // host-only
        assert!(validate_guest_spec("a:sleep:5 b:sleep:10").is_err()); // whitespace
        assert!(validate_guest_spec("garbage").is_err()); // unparseable
    }

    /// A burn is capped at 15 s, enough to outlast the restore ACK's 10 s connect
    /// deadline, so a typo cannot hold every guest CPU for minutes.
    #[test]
    fn test_burn_has_its_own_cap() {
        assert!(parse_spec("x:burn:15000").is_ok());
        assert!(parse_spec("x:burn:15001").is_err());
    }

    /// The host arming path refuses a burn: armed in fcvm (FCVM_FAILPOINT) it would starve
    /// every CPU of the host. It panics before arming, like any malformed spec.
    #[test]
    #[should_panic(expected = "burn is guest-only")]
    fn test_host_arming_refuses_burn() {
        arm_host("exec.post_connect_pre_send:burn:3000");
    }

    #[test]
    fn test_parse_burn() {
        let map = parse_spec("restore.pre_ack_connect:burn:4000").unwrap();
        assert_eq!(
            map["restore.pre_ack_connect"],
            Action::Burn(Duration::from_millis(4000))
        );
        assert_eq!(format!("{}", map["restore.pre_ack_connect"]), "burn:4000ms");
    }

    /// The one test allowed to arm the process-global map (OnceLock arms once
    /// per process): covers env → parse → arm → hit, and coarse sleep timing.
    #[test]
    fn test_arm_from_env_then_hit_sleeps() {
        std::env::set_var("FCVM_FAILPOINT", "unit.sleep:sleep:80");
        arm_from_env();
        let start = Instant::now();
        hit("unit.sleep");
        assert!(
            start.elapsed() >= Duration::from_millis(80),
            "armed sleep failpoint must hold for its duration"
        );
    }

    #[test]
    fn test_unarmed_fast_path() {
        // Never armed for this name in any test — with nextest each test is its
        // own process, and under plain `cargo test` the only arming test uses a
        // different name, so this exercises both "not set" and "map miss".
        let start = Instant::now();
        for _ in 0..1000 {
            hit("unit.never-armed");
        }
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "unarmed hit must be effectively free"
        );
    }

    #[tokio::test]
    async fn test_unarmed_fast_path_async() {
        let start = Instant::now();
        hit_async("unit.never-armed-async").await;
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn test_block_until_file_releases_when_file_appears() {
        let path = unique_path("release");
        let creator = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                std::fs::write(&path, b"go").unwrap();
            })
        };
        let start = Instant::now();
        perform_sync(
            "unit.block",
            &Action::BlockUntilFile(path.clone()),
            Duration::from_secs(10),
        );
        let elapsed = start.elapsed();
        creator.join().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(
            elapsed >= Duration::from_millis(50),
            "must block until the file appears (elapsed {elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "must release promptly once the file exists (elapsed {elapsed:?})"
        );
    }

    #[test]
    fn test_block_until_file_hard_cap() {
        let path = unique_path("never-created");
        let start = Instant::now();
        perform_sync(
            "unit.cap",
            &Action::BlockUntilFile(path),
            Duration::from_millis(150),
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "must wait out the cap (elapsed {elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "must release at the cap, not hang (elapsed {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn test_block_until_file_async_release_and_cap() {
        // Release path.
        let path = unique_path("async-release");
        let creator = {
            let path = path.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(60)).await;
                tokio::fs::write(&path, b"go").await.unwrap();
            })
        };
        let start = Instant::now();
        perform_async(
            "unit.block-async",
            &Action::BlockUntilFile(path.clone()),
            Duration::from_secs(10),
        )
        .await;
        let elapsed = start.elapsed();
        creator.await.unwrap();
        tokio::fs::remove_file(&path).await.unwrap();
        assert!(elapsed >= Duration::from_millis(50) && elapsed < Duration::from_secs(5));

        // Cap path.
        let start = Instant::now();
        perform_async(
            "unit.cap-async",
            &Action::BlockUntilFile(unique_path("async-never")),
            Duration::from_millis(150),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_async_sleep_holds() {
        let start = Instant::now();
        perform_async(
            "unit.async-sleep",
            &Action::Sleep(Duration::from_millis(80)),
            BLOCK_CAP,
        )
        .await;
        assert!(start.elapsed() >= Duration::from_millis(80));
    }
}
