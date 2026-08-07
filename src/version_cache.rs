//! Cached `<binary> --version` probes.
//!
//! `firecracker --version` runs on the VM-launch / snapshot-clone hot path —
//! `find_firecracker()` probes it and is called more than once per launch — and
//! each probe is a fork+exec of a multi-megabyte binary. The answer only changes
//! when the binary itself changes, so it is memoised per binary *identity*
//! (path + mtime + size) in two layers:
//!
//! * a process-local map, so repeat probes inside one fcvm process are free;
//! * `assets_dir/version-cache/<key>.json`, so a *freshly spawned* fcvm (the
//!   clone case: one short-lived process per VM) skips the exec too.
//!
//! **Version gating still holds.** Rebuilding or replacing the binary changes
//! its mtime and usually its size, which changes the key, so a changed binary is
//! re-probed instead of inheriting the old version. A different path is a
//! different key for the same reason.
//!
//! Only *successful* probes are cached. A binary that fails to run (wrong arch,
//! missing loader) re-runs every time and keeps producing its real error, which
//! is what the fallback logic in `find_cloud_hypervisor()` depends on.
//!
//! Cache IO never fails a probe: an unreadable or unwritable cache degrades to
//! running the binary (so a root-created cache dir does not break unprivileged
//! runs, and vice versa — they just fall back to the in-process layer).
//!
//! Entries are ~250 bytes and one is added per distinct binary build ever
//! probed, so the directory is bounded by how many firecracker /
//! cloud-hypervisor builds a host has seen; no eviction is needed.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tracing::debug;

/// Persisted probe result. `path`/`mtime_ns`/`len` are re-verified on read so a
/// hash collision or a hand-edited file can never hand back another binary's
/// version string.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CachedVersion {
    path: PathBuf,
    mtime_ns: i128,
    len: u64,
    stdout: String,
}

/// Identity of a binary: absolute-ish path plus mtime and size.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryIdentity {
    path: PathBuf,
    mtime_ns: i128,
    len: u64,
}

impl BinaryIdentity {
    fn of(bin: &Path) -> Option<Self> {
        let meta = std::fs::metadata(bin).ok()?;
        let mtime = meta.modified().ok()?;
        let mtime_ns = match mtime.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            // Pre-epoch mtime (possible on odd filesystems): keep it distinct.
            Err(e) => -(e.duration().as_nanos() as i128),
        };
        Some(Self {
            path: bin.to_path_buf(),
            mtime_ns,
            len: meta.len(),
        })
    }

    /// Stable file name for this identity's on-disk cache entry.
    fn key(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.path.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
        hasher.update(self.mtime_ns.to_le_bytes());
        hasher.update(self.len.to_le_bytes());
        hex::encode(&hasher.finalize()[..16])
    }

    fn matches(&self, entry: &CachedVersion) -> bool {
        entry.path == self.path && entry.mtime_ns == self.mtime_ns && entry.len == self.len
    }
}

fn memo() -> &'static Mutex<HashMap<String, String>> {
    static MEMO: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// On-disk cache directory, or `None` when the asset paths were never
/// initialised (then only the process-local layer is used).
fn disk_cache_dir() -> Option<PathBuf> {
    crate::paths::assets_dir_if_initialized().map(|d| d.join("version-cache"))
}

/// Run `<bin> --version` and return its stdout, reusing a cached result when the
/// binary has not changed. See the module docs for the caching contract.
pub fn version_output(bin: &Path) -> Result<String> {
    version_output_in(bin, disk_cache_dir().as_deref())
}

/// [`version_output`] against an explicit cache directory (`None` disables the
/// on-disk layer). Split out so tests can exercise the cache without touching
/// the process-global asset paths.
fn version_output_in(bin: &Path, cache_dir: Option<&Path>) -> Result<String> {
    let identity = BinaryIdentity::of(bin);

    if let Some(id) = &identity {
        let key = id.key();
        if let Ok(map) = memo().lock() {
            if let Some(hit) = map.get(&key) {
                return Ok(hit.clone());
            }
        }
        if let Some(dir) = cache_dir {
            if let Some(hit) = read_disk_entry(dir, &key, id) {
                debug!(bin = %bin.display(), "version cache hit (disk)");
                if let Ok(mut map) = memo().lock() {
                    map.insert(key, hit.clone());
                }
                return Ok(hit);
            }
        }
    }

    let output = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run `{} --version`", bin.display()))?;
    if !output.status.success() {
        bail!(
            "`{} --version` failed (exit {}): {}",
            bin.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    if let Some(id) = &identity {
        let key = id.key();
        if let Ok(mut map) = memo().lock() {
            map.insert(key.clone(), stdout.clone());
        }
        if let Some(dir) = cache_dir {
            write_disk_entry(dir, &key, id, &stdout);
        }
    }

    Ok(stdout)
}

fn read_disk_entry(dir: &Path, key: &str, id: &BinaryIdentity) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(format!("{}.json", key))).ok()?;
    let entry: CachedVersion = serde_json::from_str(&raw).ok()?;
    id.matches(&entry).then_some(entry.stdout)
}

/// Write the entry atomically (unique temp + rename): several fcvm processes can
/// probe the same binary at once, and a reader must never see a partial file.
fn write_disk_entry(dir: &Path, key: &str, id: &BinaryIdentity, stdout: &str) {
    let entry = CachedVersion {
        path: id.path.clone(),
        mtime_ns: id.mtime_ns,
        len: id.len,
        stdout: stdout.to_string(),
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // uuid, not pid — separate PID namespaces reuse numbers.
    let tmp = dir.join(format!(".{}.{}.tmp", key, uuid::Uuid::new_v4()));
    let Ok(json) = serde_json::to_vec(&entry) else {
        return;
    };
    if std::fs::write(&tmp, &json).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, dir.join(format!("{}.json", key))).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Argument that makes a fake binary exit immediately without touching the
    /// witness file — used to pre-flight execability, see [`wait_until_executable`].
    const EXEC_PROBE: &str = "--fcvm-exec-probe";

    /// A fake `--version` binary that records every invocation in `witness`, so a
    /// test can prove an exec was skipped rather than merely inferring it.
    fn fake_binary(dir: &Path, name: &str, witness: &Path, text: &str) -> PathBuf {
        write_fake(dir.join(name), witness, &format!("echo '{}'", text))
    }

    fn write_fake(path: PathBuf, witness: &Path, body: &str) -> PathBuf {
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ \"$1\" = \"{probe}\" ] && exit 0\necho ran >> '{witness}'\n{body}\n",
                probe = EXEC_PROBE,
                witness = witness.display(),
                body = body,
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        wait_until_executable(&path);
        path
    }

    /// Wait until a just-written script can actually be exec'd.
    ///
    /// A multithreaded test process that writes an executable and then runs it
    /// can hit `ETXTBSY`: a *different* test's `Command::spawn` forks in the
    /// window where our write fd is still open, the child inherits it, and
    /// `execve` refuses any file that is open for writing anywhere. That is an
    /// artifact of creating executables inside the test process — production
    /// code only ever execs binaries it did not just write — so the helper waits
    /// the window out instead of leaking a retry into `version_output`.
    fn wait_until_executable(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match std::process::Command::new(path).arg(EXEC_PROBE).status() {
                Ok(status) if status.success() => return,
                Ok(status) => panic!("fake binary {} probe failed: {status}", path.display()),
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "fake binary {} stayed ETXTBSY for 10s",
                        path.display()
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("running fake binary {}: {e}", path.display()),
            }
        }
    }

    fn run_count(witness: &Path) -> usize {
        std::fs::read_to_string(witness)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn cache_hit_skips_the_exec_in_this_process_and_the_next() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let witness = tmp.path().join("runs.txt");
        let bin = fake_binary(tmp.path(), "probe-reuse", &witness, "Firecracker v1.15.0");

        assert_eq!(
            version_output_in(&bin, Some(&cache)).unwrap().trim(),
            "Firecracker v1.15.0"
        );
        assert_eq!(run_count(&witness), 1, "first probe must exec the binary");

        // Same process: served by the in-process memo.
        version_output_in(&bin, Some(&cache)).unwrap();
        assert_eq!(run_count(&witness), 1, "memo hit must not exec");

        // Simulate a freshly spawned fcvm: the on-disk layer must still hit.
        memo().lock().unwrap().clear();
        assert_eq!(
            version_output_in(&bin, Some(&cache)).unwrap().trim(),
            "Firecracker v1.15.0"
        );
        assert_eq!(run_count(&witness), 1, "disk cache hit must not exec");
    }

    #[test]
    fn changed_binary_is_reprobed() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let witness = tmp.path().join("runs.txt");
        let bin = fake_binary(tmp.path(), "probe-changed", &witness, "Firecracker v1.13.1");
        assert_eq!(
            version_output_in(&bin, Some(&cache)).unwrap().trim(),
            "Firecracker v1.13.1"
        );
        assert_eq!(run_count(&witness), 1);

        // Rebuild in place: different bytes => different size and mtime => new key.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let bin = fake_binary(
            tmp.path(),
            "probe-changed",
            &witness,
            "Firecracker v1.15.0 rebuilt",
        );
        assert_eq!(
            version_output_in(&bin, Some(&cache)).unwrap().trim(),
            "Firecracker v1.15.0 rebuilt",
            "a rebuilt binary must be re-probed, not served from cache"
        );
        assert_eq!(run_count(&witness), 2, "rebuilt binary must be re-execed");
    }

    #[test]
    fn different_path_is_a_different_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let witness = tmp.path().join("runs.txt");
        let a = fake_binary(tmp.path(), "fc-a", &witness, "Firecracker v1.13.1");
        let b = fake_binary(tmp.path(), "fc-b", &witness, "Firecracker v1.15.0");
        assert_eq!(
            version_output_in(&a, Some(&cache)).unwrap().trim(),
            "Firecracker v1.13.1"
        );
        assert_eq!(
            version_output_in(&b, Some(&cache)).unwrap().trim(),
            "Firecracker v1.15.0"
        );
        assert_eq!(run_count(&witness), 2);
    }

    #[test]
    fn failing_binary_is_not_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let witness = tmp.path().join("runs.txt");
        let path = write_fake(tmp.path().join("fc-fail"), &witness, "exit 3");

        let err = version_output_in(&path, Some(&cache)).unwrap_err();
        assert!(
            err.to_string().contains("--version` failed"),
            "unexpected error: {err}"
        );
        assert!(
            !cache.exists() || std::fs::read_dir(&cache).unwrap().count() == 0,
            "a failing probe must not be cached"
        );
        // ...and it must keep failing (not silently succeed from a stale entry).
        assert!(version_output_in(&path, Some(&cache)).is_err());
        assert_eq!(run_count(&witness), 2, "failing probe must re-exec");
    }

    #[test]
    fn no_disk_cache_dir_still_works() {
        let tmp = tempfile::tempdir().unwrap();
        let witness = tmp.path().join("runs.txt");
        let bin = fake_binary(tmp.path(), "fc-nodisk", &witness, "Firecracker v1.15.0");
        assert_eq!(
            version_output_in(&bin, None).unwrap().trim(),
            "Firecracker v1.15.0"
        );
    }
}
