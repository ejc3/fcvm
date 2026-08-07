//! Cached per-binary derivations: `<binary> --version` probes and other strings
//! that are a pure function of a binary's bytes (e.g. the fc-agent initrd SHA).
//!
//! `firecracker --version` runs on the VM-launch / snapshot-clone hot path —
//! `find_firecracker()` probes it and is called more than once per launch — and
//! each probe is a fork+exec of a multi-megabyte binary. Likewise the fc-agent
//! initrd SHA re-reads and SHA-256s a ~4.4MB binary per launch. The answer only
//! changes when the binary itself changes, so it is memoised per binary
//! *identity* (path + mtime + size) in two layers:
//!
//! * a process-local map, so repeat probes inside one fcvm process are free;
//! * `assets_dir/version-cache/<key>.json`, so a *freshly spawned* fcvm (the
//!   clone case: one short-lived process per VM) skips the exec/hash too.
//!
//! **Gating still holds.** Rebuilding or replacing the binary changes its mtime
//! and usually its size, which changes the key, so a changed binary is
//! re-derived instead of inheriting the old value. A different path is a
//! different key for the same reason. Distinct derivations of the same binary
//! are separated by a `namespace` string that is hashed into the key AND
//! re-verified on read (`""` is the `--version` namespace, so pre-existing
//! cache entries stay valid).
//!
//! Only *successful* derivations are cached. A binary that fails to run (wrong
//! arch, missing loader) re-runs every time and keeps producing its real error,
//! which is what the fallback logic in `find_cloud_hypervisor()` depends on.
//!
//! Cache IO never fails a derivation: an unreadable or unwritable cache degrades
//! to recomputing (so a root-created cache dir does not break unprivileged
//! runs, and vice versa — they just fall back to the in-process layer).
//!
//! Entries are ~250 bytes and one is added per distinct binary build ever
//! probed, so the directory is bounded by how many binary builds a host has
//! seen; no eviction is needed.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tracing::debug;

/// Persisted derivation result. `path`/`mtime_ns`/`len`/`namespace` are
/// re-verified on read so a hash collision or a hand-edited file can never hand
/// back another binary's (or another derivation's) value.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CachedVersion {
    path: PathBuf,
    mtime_ns: i128,
    len: u64,
    /// Which derivation this entry stores (`""` = `--version` stdout). Absent in
    /// entries written before namespaces existed, which were all `--version`.
    #[serde(default)]
    namespace: String,
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

    /// Stable file name for this identity's on-disk cache entry. The namespace
    /// is appended, so `""` (the `--version` namespace) produces byte-identical
    /// keys to the pre-namespace scheme and existing cache entries stay valid.
    fn key(&self, namespace: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.path.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
        hasher.update(self.mtime_ns.to_le_bytes());
        hasher.update(self.len.to_le_bytes());
        hasher.update(namespace.as_bytes());
        hex::encode(&hasher.finalize()[..16])
    }

    fn matches(&self, entry: &CachedVersion, namespace: &str) -> bool {
        entry.path == self.path
            && entry.mtime_ns == self.mtime_ns
            && entry.len == self.len
            && entry.namespace == namespace
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
    derived_in(bin, "", cache_dir, || {
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
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    })
}

/// Memoise an arbitrary string derived purely from a binary's bytes (plus
/// whatever the caller folds into `namespace`), keyed by binary identity.
/// `compute` runs on a miss; its value is cached only on success.
pub fn derived(
    bin: &Path,
    namespace: &str,
    compute: impl FnOnce() -> Result<String>,
) -> Result<String> {
    derived_in(bin, namespace, disk_cache_dir().as_deref(), compute)
}

fn derived_in(
    bin: &Path,
    namespace: &str,
    cache_dir: Option<&Path>,
    compute: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let identity = BinaryIdentity::of(bin);

    if let Some(id) = &identity {
        let key = id.key(namespace);
        if let Ok(map) = memo().lock() {
            if let Some(hit) = map.get(&key) {
                return Ok(hit.clone());
            }
        }
        if let Some(dir) = cache_dir {
            if let Some(hit) = read_disk_entry(dir, &key, id, namespace) {
                debug!(bin = %bin.display(), namespace, "binary derivation cache hit (disk)");
                if let Ok(mut map) = memo().lock() {
                    map.insert(key, hit.clone());
                }
                return Ok(hit);
            }
        }
    }

    let value = compute()?;

    if let Some(id) = &identity {
        let key = id.key(namespace);
        if let Ok(mut map) = memo().lock() {
            map.insert(key.clone(), value.clone());
        }
        if let Some(dir) = cache_dir {
            write_disk_entry(dir, &key, id, namespace, &value);
        }
    }

    Ok(value)
}

fn read_disk_entry(dir: &Path, key: &str, id: &BinaryIdentity, namespace: &str) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(format!("{}.json", key))).ok()?;
    let entry: CachedVersion = serde_json::from_str(&raw).ok()?;
    id.matches(&entry, namespace).then_some(entry.stdout)
}

/// Write the entry atomically (unique temp + rename): several fcvm processes can
/// probe the same binary at once, and a reader must never see a partial file.
fn write_disk_entry(dir: &Path, key: &str, id: &BinaryIdentity, namespace: &str, stdout: &str) {
    let entry = CachedVersion {
        path: id.path.clone(),
        mtime_ns: id.mtime_ns,
        len: id.len,
        namespace: namespace.to_string(),
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

    /// Different namespaces of the same binary are independent entries, and a
    /// disk hit for one namespace can never satisfy the other (both the key and
    /// the stored `namespace` field separate them).
    #[test]
    fn namespaces_are_isolated_per_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        std::fs::write(tmp.path().join("bin"), b"payload").unwrap();
        let bin = tmp.path().join("bin");

        let mut version_runs = 0;
        let mut sha_runs = 0;
        let v = derived_in(&bin, "", Some(&cache), || {
            version_runs += 1;
            Ok("v1".into())
        })
        .unwrap();
        let s = derived_in(&bin, "initrd-sha:abc", Some(&cache), || {
            sha_runs += 1;
            Ok("sha-value".into())
        })
        .unwrap();
        assert_eq!((v.as_str(), s.as_str()), ("v1", "sha-value"));
        assert_eq!((version_runs, sha_runs), (1, 1));

        // Fresh process simulation: disk layer must hit per-namespace.
        memo().lock().unwrap().clear();
        let v2 = derived_in(&bin, "", Some(&cache), || {
            version_runs += 1;
            Ok("WRONG".into())
        })
        .unwrap();
        let s2 = derived_in(&bin, "initrd-sha:abc", Some(&cache), || {
            sha_runs += 1;
            Ok("WRONG".into())
        })
        .unwrap();
        assert_eq!((v2.as_str(), s2.as_str()), ("v1", "sha-value"));
        assert_eq!(
            (version_runs, sha_runs),
            (1, 1),
            "disk hits must not recompute"
        );

        // A different namespace suffix (changed embedded scripts) recomputes.
        let s3 = derived_in(&bin, "initrd-sha:def", Some(&cache), || {
            Ok("new-scripts".into())
        })
        .unwrap();
        assert_eq!(s3, "new-scripts");
    }
}
