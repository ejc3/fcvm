//! The state file: read once at start, rewritten whole after changes (temporary file,
//! fsync, rename), by one writer task.

use anyhow::{Context, Result};
use fleet_core::State;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

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

/// One state to write; `ack` is answered once that write (and every earlier one) is done.
pub struct StoreCmd {
    pub bytes: Arc<Vec<u8>>,
    pub ack: Option<oneshot::Sender<()>>,
}

pub type StoreTx = mpsc::UnboundedSender<StoreCmd>;

/// Start the one task that writes `path`. States are written in the order sent; when
/// several are queued only the newest is written, and every ack among them is answered
/// after that write.
pub fn spawn_writer(path: PathBuf) -> StoreTx {
    let (tx, mut rx) = mpsc::unbounded_channel::<StoreCmd>();
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut bytes = first.bytes;
            let mut acks: Vec<oneshot::Sender<()>> = first.ack.into_iter().collect();
            while let Ok(more) = rx.try_recv() {
                bytes = more.bytes;
                acks.extend(more.ack);
            }
            let p = path.clone();
            match tokio::task::spawn_blocking(move || write_atomic(&p, &bytes)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!(path = %path.display(), error = %format!("{e:#}"), "state file not written")
                }
                Err(e) => tracing::error!(error = %e, "state writer panicked"),
            }
            for a in acks {
                let _ = a.send(());
            }
        }
    });
    tx
}

/// The state as one S3 object (the coordinator's task has no writable disk). One PUT
/// replaces it whole, which S3 makes atomic; the bucket keeps versions.
pub struct S3Object {
    pub bucket: String,
    pub key: String,
    pub region: String,
    base: String,
    chain: super::aws::CredentialChain,
    http: reqwest::Client,
}

impl S3Object {
    pub fn new(bucket: &str, key: &str, region: &str) -> Result<Self> {
        S3Object::with_endpoint(
            bucket,
            key,
            region,
            &format!("https://s3.{region}.amazonaws.com"),
        )
    }

    /// Against another endpoint (tests).
    pub fn with_endpoint(bucket: &str, key: &str, region: &str, base: &str) -> Result<Self> {
        let plain = |s: &str| {
            !s.is_empty()
                && s.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/')
                })
        };
        anyhow::ensure!(
            plain(bucket) && plain(key),
            "the state bucket and key may use letters, digits, - _ . ~ and / only"
        );
        Ok(S3Object {
            bucket: bucket.into(),
            key: key.into(),
            region: region.into(),
            base: base.into(),
            chain: super::aws::CredentialChain::new()?,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
        })
    }

    fn path(&self) -> String {
        format!("/{}/{}", self.bucket, self.key)
    }

    /// The stored state, or `None` when there is none yet. S3 answers 404 for a missing key
    /// only to a caller that may list the bucket; a 403 is an error, never "no state".
    pub async fn load(&self) -> Result<Option<State>> {
        let (status, bytes) = super::aws::signed(
            &self.chain,
            &self.http,
            &self.base,
            reqwest::Method::GET,
            &self.path(),
            "",
            &self.region,
            "s3",
            &[],
            Vec::new(),
        )
        .await?;
        match status.as_u16() {
            200 => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("parsing s3://{}/{}", self.bucket, self.key)
            })?)),
            404 => Ok(None),
            403 => anyhow::bail!(
                "reading s3://{}/{} was refused (403): a missing object reads as 403 without s3:ListBucket, so the coordinator will not start from an empty state",
                self.bucket,
                self.key
            ),
            s => anyhow::bail!("reading s3://{}/{} answered {s}", self.bucket, self.key),
        }
    }

    pub async fn put(&self, bytes: &[u8]) -> Result<()> {
        let (status, body) = super::aws::signed(
            &self.chain,
            &self.http,
            &self.base,
            reqwest::Method::PUT,
            &self.path(),
            "",
            &self.region,
            "s3",
            &[("content-type", "application/json".into())],
            bytes.to_vec(),
        )
        .await?;
        if !status.is_success() {
            anyhow::bail!(
                "writing s3://{}/{} answered {status}: {}",
                self.bucket,
                self.key,
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(300)
                    .collect::<String>()
            );
        }
        Ok(())
    }
}

/// Start the one task that writes the S3 object, with the file writer's ordering and acks.
/// A failed write is logged and the next state is written in full.
pub fn spawn_s3_writer(obj: S3Object) -> StoreTx {
    let (tx, mut rx) = mpsc::unbounded_channel::<StoreCmd>();
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut bytes = first.bytes;
            let mut acks: Vec<oneshot::Sender<()>> = first.ack.into_iter().collect();
            while let Ok(more) = rx.try_recv() {
                bytes = more.bytes;
                acks.extend(more.ack);
            }
            if let Err(e) = obj.put(&bytes).await {
                tracing::error!(bucket = %obj.bucket, key = %obj.key, error = %format!("{e:#}"), "state not written");
            }
            for a in acks {
                let _ = a.send(());
            }
        }
    });
    tx
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
        s.fleet.race_count = 7;
        s.fleet.counters.restores = 1834;
        s.fleet.quarantined.insert(
            fleet_core::state::gpu_key("gpu-a", "0000:05:00.0"),
            fleet_core::state::Quarantined {
                host: "gpu-a".into(),
                sbdf: "0000:05:00.0".into(),
                error: "did not reset".into(),
                since: 900,
            },
        );
        write_atomic(&path, &serde_json::to_vec(&s).unwrap()).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back, s);
        let resumed = back.resumed(Config::default(), 2000);
        assert_eq!(resumed.version, 42);
        assert_eq!(resumed.fleet.race_count, 7);
        // Totals and quarantines outlive a coordinator restart.
        assert_eq!(resumed.fleet.counters.restores, 1834);
        assert_eq!(resumed.fleet.quarantined.len(), 1);
    }
}
