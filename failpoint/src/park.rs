//! The race gym's park protocol (`docs/race-gym.md`, section 3).
//!
//! Each fcvm process that hits a parked point owns one directory,
//! `<dir>/<actor>-<pid>`, where `<actor>` is `FCVM_RACE_ACTOR`. Every event the process
//! records takes the next number of one per-process sequence, so the controller reads a
//! process's events in the order they happened:
//!
//! * `<seq>.reached`: the hit stopped at a point. The process waits for `<seq>.go`.
//! * `<seq>.note`: a record that does not wait, such as the start of a lock wait.
//!
//! Each file is written under a temporary name and renamed, so the controller never reads
//! a partial one. Its body is `key=value` lines: `point` (or `note` and `detail`), `hit`
//! (the per-process count of that name, from 1) and `pid`.
//!
//! A park that cannot write its file panics. Running on unparked would make the controller
//! believe the process is still parked earlier, and the schedule under test would not be
//! the one it records.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How often a parked hit looks for its go file.
pub(crate) const POLL: Duration = Duration::from_millis(5);

static SEQ: AtomicU64 = AtomicU64::new(0);
static HITS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
static ACTOR_DIR: OnceLock<PathBuf> = OnceLock::new();

fn actor_dir(dir: &Path) -> &'static Path {
    ACTOR_DIR.get_or_init(|| {
        let actor = std::env::var("FCVM_RACE_ACTOR").unwrap_or_else(|_| "actor".to_string());
        let path = dir.join(format!("{actor}-{}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&path) {
            panic!("FAILPOINT park: creating {}: {e}", path.display());
        }
        path
    })
}

fn publish(dir: &Path, suffix: &str, body: &str) -> PathBuf {
    let dir = actor_dir(dir);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let name = format!("{seq:06}");
    let tmp = dir.join(format!(".{name}.{suffix}.tmp"));
    let path = dir.join(format!("{name}.{suffix}"));
    if let Err(e) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, &path)) {
        panic!("FAILPOINT park: writing {}: {e}", path.display());
    }
    dir.join(format!("{name}.go"))
}

fn next_hit(name: &str) -> u64 {
    let mut hits = HITS.lock().unwrap_or_else(|e| e.into_inner());
    let count = hits
        .get_or_insert_with(HashMap::new)
        .entry(name.to_string())
        .or_insert(0);
    *count += 1;
    *count
}

/// Record that this process stopped at `point`; returns the go file to wait for.
pub(crate) fn announce(dir: &Path, point: &str) -> PathBuf {
    let hit = next_hit(point);
    let body = format!("point={point}\nhit={hit}\npid={}\n", std::process::id());
    publish(dir, "reached", &body)
}

/// Record an event that does not wait.
pub(crate) fn note(dir: &Path, kind: &str, detail: &str) {
    let body = format!(
        "note={kind}\ndetail={}\npid={}\n",
        detail.replace('\n', " "),
        std::process::id()
    );
    publish(dir, "note", &body);
}
