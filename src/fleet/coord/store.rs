//! The state file: read once at start, rewritten whole after changes (temporary file,
//! fsync, rename), by one writer task.

use anyhow::{Context, Result};
use fleet_core::State;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;

pub fn load(path: &Path) -> Result<Option<State>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("renaming onto {}", path.display()))?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Write each new state the actor publishes. Only this task writes the file.
pub async fn writer(path: PathBuf, mut rx: watch::Receiver<Arc<Vec<u8>>>) {
    while rx.changed().await.is_ok() {
        let bytes = rx.borrow_and_update().clone();
        let p = path.clone();
        match tokio::task::spawn_blocking(move || write_atomic(&p, &bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(path = %path.display(), error = %format!("{e:#}"), "state file not written")
            }
            Err(e) => tracing::error!(error = %e, "state writer panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::Config;

    #[test]
    fn a_written_state_loads_back_and_resumes_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        assert!(load(&path).unwrap().is_none());
        let mut s = State::new(Config::default(), 1000);
        s.version = 41;
        s.fleet.settings.auto_relaunch_secs = Some(60);
        write_atomic(&path, &serde_json::to_vec(&s).unwrap()).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back, s);
        let resumed = back.resumed(Config::default());
        assert_eq!(resumed.version, 42);
        assert_eq!(resumed.fleet.settings.auto_relaunch_secs, Some(60));
    }
}
