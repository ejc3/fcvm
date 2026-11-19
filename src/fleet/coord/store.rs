//! The state file: read once at start, rewritten whole after changes (temporary file,
//! fsync, rename), by one writer task.

use anyhow::{Context, Result};
use fleet_core::State;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

/// Logged (as `marker`) when the stored state object does not parse and the coordinator
/// starts from an earlier version of it: an alarm matches this string.
pub const UNREADABLE_STATE_MARKER: &str = "FLEET_STATE_UNREADABLE";
/// Logged (as `marker`) when another writer replaced the state object since this coordinator
/// last read or wrote it: an alarm matches this string. The coordinator stops.
pub const LOST_WRITE_MARKER: &str = "FLEET_STATE_WRITE_LOST";

/// A write refused because the stored object is no longer the one this writer last read or
/// wrote: another coordinator writes it now.
#[derive(Debug)]
pub struct LostWrite(pub String);

impl std::fmt::Display for LostWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LostWrite {}

/// Most earlier versions of the state object tried when the newest does not parse.
const EARLIER_VERSIONS_TRIED: usize = 20;
/// Reads of the state object at start, when another writer replaces it while an unreadable
/// one is being recovered.
const LOAD_ATTEMPTS: usize = 3;

/// What recovering from an unreadable object found.
enum Recovered {
    /// The newest earlier version that parses, and its version id.
    From(String, Box<State>),
    /// The newest listed version is not the one read: another writer replaced the object
    /// since, so the object is read again.
    Moved { listed: Option<String> },
}

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

/// One state to write; `ack` is answered once that write (and every earlier one) is done,
/// with whether the newest state the writer was given is stored. Without `bytes` it writes
/// nothing new: it retries a state whose write failed, and waits for the writes queued
/// before it.
pub struct StoreCmd {
    pub bytes: Option<Arc<Vec<u8>>>,
    pub ack: Option<oneshot::Sender<bool>>,
}

pub type StoreTx = mpsc::UnboundedSender<StoreCmd>;

/// Start the one task that writes `path`. States are written in the order sent; when
/// several are queued only the newest is written, and every ack among them is answered
/// after that write. A state whose write failed is kept and written again with the next
/// command, unless a newer state replaces it.
pub fn spawn_writer(path: PathBuf) -> StoreTx {
    let (tx, _lost) = spawn_store(move |bytes| {
        let p = path.clone();
        async move {
            let shown = p.display().to_string();
            tokio::task::spawn_blocking(move || write_atomic(&p, &bytes))
                .await
                .context("the state writer panicked")?
                .with_context(|| format!("writing {shown}"))
        }
    });
    tx
}

/// The writer task both stores share: `write` stores one state. A write that fails with
/// [`LostWrite`] ends this writer's writes: every later command is answered "not stored"
/// without writing, and the returned receiver turns true.
fn spawn_store<F, Fut>(mut write: F) -> (StoreTx, watch::Receiver<bool>)
where
    F: FnMut(Arc<Vec<u8>>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<StoreCmd>();
    let (lost_tx, lost_rx) = watch::channel(false);
    tokio::spawn(async move {
        // The newest state not stored yet: a write that failed is kept for the next try.
        let mut unwritten: Option<Arc<Vec<u8>>> = None;
        let mut lost = false;
        while let Some(first) = rx.recv().await {
            let mut acks: Vec<oneshot::Sender<bool>> = first.ack.into_iter().collect();
            if first.bytes.is_some() {
                unwritten = first.bytes;
            }
            while let Ok(more) = rx.try_recv() {
                if more.bytes.is_some() {
                    unwritten = more.bytes;
                }
                acks.extend(more.ack);
            }
            if lost {
                for a in acks {
                    let _ = a.send(false);
                }
                continue;
            }
            if let Some(bytes) = unwritten.clone() {
                match write(bytes).await {
                    Ok(()) => unwritten = None,
                    Err(e) if e.downcast_ref::<LostWrite>().is_some() => {
                        tracing::error!(
                            marker = LOST_WRITE_MARKER,
                            error = %format!("{e:#}"),
                            "another writer replaced the state; this coordinator writes no more"
                        );
                        lost = true;
                        let _ = lost_tx.send(true);
                    }
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "state not written; kept for the next write")
                    }
                }
            }
            for a in acks {
                let _ = a.send(!lost && unwritten.is_none());
            }
        }
    });
    (tx, lost_rx)
}

/// The state as one S3 object (the coordinator's task has no writable disk). One PUT
/// replaces it whole, which S3 makes atomic; the bucket keeps versions.
///
/// Every PUT is conditional on the object being the one this coordinator last read or wrote
/// (`If-Match` on that ETag, or `If-None-Match: *` when there was none), so a second
/// coordinator writing the same object makes one of them lose, loudly, instead of the two
/// overwriting each other's acknowledged changes.
pub struct S3Object {
    pub bucket: String,
    pub key: String,
    pub region: String,
    base: String,
    chain: super::aws::CredentialChain,
    http: reqwest::Client,
    /// The ETag of the object as this coordinator last read or wrote it; `None`: there was
    /// no object.
    etag: std::sync::Mutex<Option<String>>,
    /// SHA-256 of each body sent since the last write S3 answered (see `put`).
    unanswered: std::sync::Mutex<Vec<[u8; 32]>>,
}

/// Bodies remembered while writes go unanswered; the writer sends at most one a second.
const UNANSWERED_KEPT: usize = 64;

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
            etag: std::sync::Mutex::new(None),
            unanswered: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn path(&self) -> String {
        format!("/{}/{}", self.bucket, self.key)
    }

    fn shown(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.key)
    }

    async fn get(
        &self,
        path: &str,
        query: &str,
    ) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
        super::aws::signed_with_headers(
            &self.chain,
            &self.http,
            &self.base,
            reqwest::Method::GET,
            path,
            query,
            &self.region,
            "s3",
            &[],
            Vec::new(),
        )
        .await
    }

    /// The stored state, or `None` when there is none yet. S3 answers 404 for a missing key
    /// only to a caller that may list the bucket; a 403 is an error, never "no state".
    ///
    /// A newest version that does not parse is logged with [`UNREADABLE_STATE_MARKER`] and
    /// the newest earlier version that parses is returned instead: the coordinator starts
    /// without the changes stored after that version, rather than not at all. With no
    /// version that parses it is an error. Writes stay conditional on the newest version.
    pub async fn load(&self) -> Result<Option<State>> {
        for attempt in 1..=LOAD_ATTEMPTS {
            if let Some(loaded) = self.load_once(attempt).await? {
                return Ok(loaded);
            }
        }
        anyhow::bail!(
            "{} was replaced while an unreadable version of it was recovered, {LOAD_ATTEMPTS} times",
            self.shown()
        )
    }

    /// One read of the object: `None` when another writer replaced it while an unreadable
    /// version was being recovered (read it again).
    async fn load_once(&self, attempt: usize) -> Result<Option<Option<State>>> {
        let (status, headers, bytes) = self.get(&self.path(), "").await?;
        match status.as_u16() {
            200 => {
                *self.etag.lock().unwrap() = etag_of(&headers);
                match serde_json::from_slice::<State>(&bytes) {
                    Ok(s) => Ok(Some(Some(s))),
                    Err(e) => {
                        tracing::error!(
                            marker = UNREADABLE_STATE_MARKER,
                            object = %self.shown(),
                            error = %e,
                            "the stored state does not parse; starting from the newest earlier version that does"
                        );
                        // Recovery starts below the version this GET read, so it needs that
                        // version's id.
                        let read = headers
                            .get("x-amz-version-id")
                            .and_then(|v| v.to_str().ok())
                            .filter(|v| !v.is_empty() && *v != "null")
                            .with_context(|| {
                                format!(
                                    "{} does not parse ({e}), and its read named no version, so there is no earlier version to recover from",
                                    self.shown()
                                )
                            })?
                            .to_string();
                        let found = self.earlier_version(&read).await.with_context(|| {
                            format!(
                                "{} does not parse ({e}), and no earlier version does",
                                self.shown()
                            )
                        })?;
                        let (version, s) = match found {
                            Recovered::From(version, s) => (version, *s),
                            Recovered::Moved { listed } => {
                                tracing::warn!(
                                    object = %self.shown(),
                                    read = %read,
                                    listed = ?listed,
                                    attempt,
                                    "the state object was replaced while it was recovered; reading it again"
                                );
                                return Ok(None);
                            }
                        };
                        tracing::error!(
                            marker = UNREADABLE_STATE_MARKER,
                            object = %self.shown(),
                            version_id = %version,
                            state_version = s.version,
                            "resuming from an earlier version of the state; changes stored after it are lost"
                        );
                        Ok(Some(Some(s)))
                    }
                }
            }
            404 => {
                *self.etag.lock().unwrap() = None;
                Ok(Some(None))
            }
            403 => anyhow::bail!(
                "reading {} was refused (403): a missing object reads as 403 without s3:ListBucket, so the coordinator will not start from an empty state",
                self.shown()
            ),
            s => anyhow::bail!("reading {} answered {s}", self.shown()),
        }
    }

    /// The newest version of the object older than `read` (the version a GET read and could
    /// not parse) that parses as a state, and its version id; or `Moved` when the newest
    /// listed version is not `read`, since recovering below it would skip a newer version.
    async fn earlier_version(&self, read: &str) -> Result<Recovered> {
        let query = format!("prefix={}&versions=", super::sigv4::uri_encode(&self.key));
        let (status, _, body) = self.get(&format!("/{}", self.bucket), &query).await?;
        anyhow::ensure!(
            status.is_success(),
            "listing the versions of {} answered {status}",
            self.shown()
        );
        let ids = versions(&String::from_utf8_lossy(&body), &self.key);
        if ids.first().map(String::as_str) != Some(read) {
            return Ok(Recovered::Moved {
                listed: ids.first().cloned(),
            });
        }
        for id in ids.iter().skip(1).take(EARLIER_VERSIONS_TRIED) {
            let q = format!("versionId={}", super::sigv4::uri_encode(id));
            let (status, _, bytes) = self.get(&self.path(), &q).await?;
            // A version that is gone is passed over. One that could not be read now may hold
            // changes the older ones lack, so recovery stops rather than fall back past it.
            if status == reqwest::StatusCode::NOT_FOUND {
                tracing::warn!(version_id = %id, "an earlier state version is gone");
                continue;
            }
            anyhow::ensure!(
                status.is_success(),
                "reading version {id} of {} answered {status}; not falling back past it",
                self.shown()
            );
            match serde_json::from_slice::<State>(&bytes) {
                Ok(s) => return Ok(Recovered::From(id.clone(), Box::new(s))),
                Err(e) => {
                    tracing::warn!(version_id = %id, error = %e, "an earlier state version does not parse")
                }
            }
        }
        anyhow::bail!(
            "{} versions listed, and none of the earlier ones parses",
            ids.len()
        )
    }

    /// Replace the object, if it is still the one this coordinator last read or wrote.
    /// Another writer's object fails the write with [`LostWrite`].
    ///
    /// A PUT whose answer was lost may still have been stored, and then the next one is
    /// refused (412) by this coordinator's own write. So a 412 is checked against the bodies
    /// sent since the last answered write: when the object is one of them, its ETag is taken
    /// and the write is sent once more.
    pub async fn put(&self, bytes: &[u8]) -> Result<()> {
        use sha2::{Digest, Sha256};
        {
            let mut sent = self.unanswered.lock().unwrap();
            sent.push(Sha256::digest(bytes).into());
            if sent.len() > UNANSWERED_KEPT {
                sent.remove(0);
            }
        }
        match self.put_once(bytes).await {
            Err(e) if e.downcast_ref::<LostWrite>().is_some() => {
                let (status, headers, current) = self.get(&self.path(), "").await?;
                let digest: [u8; 32] = Sha256::digest(&current).into();
                let mine = status.is_success() && self.unanswered.lock().unwrap().contains(&digest);
                if !mine {
                    return Err(e);
                }
                tracing::warn!(
                    object = %self.shown(),
                    "a write whose answer was lost was stored; writing again on it"
                );
                *self.etag.lock().unwrap() = etag_of(&headers);
                self.put_once(bytes).await
            }
            r => r,
        }
    }

    async fn put_once(&self, bytes: &[u8]) -> Result<()> {
        let condition = match self.etag.lock().unwrap().clone() {
            Some(e) => ("if-match", e),
            None => ("if-none-match", "*".to_string()),
        };
        let (status, headers, body) = super::aws::signed_with_headers(
            &self.chain,
            &self.http,
            &self.base,
            reqwest::Method::PUT,
            &self.path(),
            "",
            &self.region,
            "s3",
            &[
                ("content-type", "application/json".into()),
                (condition.0, condition.1.clone()),
            ],
            bytes.to_vec(),
        )
        .await?;
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(LostWrite(format!(
                "writing {} answered 412 to {}: {}: another writer replaced it since this coordinator last read or wrote it",
                self.shown(),
                condition.0,
                condition.1
            ))
            .into());
        }
        if !status.is_success() {
            anyhow::bail!(
                "writing {} answered {status}: {}",
                self.shown(),
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(300)
                    .collect::<String>()
            );
        }
        let etag = etag_of(&headers);
        anyhow::ensure!(
            etag.is_some(),
            "writing {} answered no ETag, so the next write cannot be conditional",
            self.shown()
        );
        *self.etag.lock().unwrap() = etag;
        self.unanswered.lock().unwrap().clear();
        Ok(())
    }
}

fn etag_of(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// The version ids of `key` in a ListObjectVersions answer, newest first; delete markers
/// are left out.
pub fn versions(xml: &str, key: &str) -> Vec<String> {
    xml.split("<Version>")
        .skip(1)
        .filter_map(|v| v.split("</Version>").next())
        .filter(|v| super::aws::xml_tag(v, "Key").as_deref() == Some(key))
        .filter_map(|v| super::aws::xml_tag(v, "VersionId"))
        .collect()
}

/// Start the one task that writes the S3 object, with the file writer's ordering, acks and
/// retries. The receiver turns true once a write found another writer's object; the writer
/// writes nothing after that.
pub fn spawn_s3_writer(obj: S3Object) -> (StoreTx, watch::Receiver<bool>) {
    let obj = Arc::new(obj);
    spawn_store(move |bytes| {
        let obj = obj.clone();
        async move { obj.put(&bytes).await }
    })
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
                report: None,
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
