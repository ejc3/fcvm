//! The Lambda tile tunnel (fleet/PROTOCOL.md, "Lambda tile", "The tunnel").
//!
//! A Lambda execution environment takes no inbound connection, so a Lambda tile dials out:
//! one control connection to the coordinator's tunnel endpoint, and one data connection per
//! stream the coordinator asks for. A viewer's connection to the tile, a scene post and a
//! health read are each one stream, and each data connection carries its stream's bytes
//! unchanged, so TLS for a viewer still ends in the renderer and every renderer route works as
//! it does on an EC2 tile.
//!
//! Every connection is TLS to the endpoint's name and starts with one JSON line, a [`Hello`];
//! the endpoint answers one [`Reply`] line. On the control connection the endpoint then sends
//! [`ToTile`] lines (open a stream, ping) and the tile answers pings with [`FromTile`] lines.
//! A data connection carries nothing but its stream after the reply.
//!
//! The endpoint keeps one session per slot. Of two environments that dial with the same
//! generation (Lambda replaces an environment on its own, and the old one keeps running for
//! a while), the one that started later holds the tile, whatever order they dial in, so a
//! coordinator restart does not change which one serves. A connection for an older
//! generation than the one the endpoint wants is refused.
//!
//! The credential a tile presents is minted by the coordinator for one slot, generation and
//! expiry ([`mint`]) with a key derived from the fleet's scene token ([`derive_key`]), and
//! handed to the function in its configuration.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

/// The protocol version a [`Hello`] names.
pub const VERSION: u32 = 1;
/// The longest line either side reads before the stream starts.
pub const MAX_LINE: usize = 4096;
/// How long a peer may take to send its hello or reply line.
pub const LINE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the endpoint pings a control connection.
pub const PING_EVERY: Duration = Duration::from_secs(10);
/// A control connection that delivers nothing for this long is dropped, by either side.
pub const SILENCE: Duration = Duration::from_secs(30);
/// How long [`Endpoint::open`] waits for the tile's data connection.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Streams waiting for their data connection, per tile. One past it is refused at once.
pub const MAX_PENDING_PER_TILE: usize = 256;

/// Which of the renderer's listeners a stream goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Port {
    /// The renderer's own TLS listener: viewer connections, relayed byte for byte.
    Tls,
    /// Its plain HTTP listener: the coordinator's scene and budget posts and health reads.
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Control,
    Data,
}

/// The first line of every connection a tile makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub v: u32,
    pub kind: Kind,
    /// The tile's slot, `lam-<n>`.
    pub slot: String,
    pub generation: u64,
    /// The execution environment: a random id the environment draws at start.
    pub env: String,
    /// When the environment started (Unix ms), which orders two environments of one
    /// generation.
    pub started_ms: u64,
    pub credential: String,
    /// The stream a data connection carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<u64>,
    /// The managed instance the environment runs on: a hash of the instance's DMI board asset
    /// tag (the instance id on Nitro), so the coordinator can tell an environment that landed
    /// on an instance it has seen before from one on a newly launched instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// When that instance's kernel booted (Unix ms, `btime` in `/proc/stat`). Environments on
    /// one instance share it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_boot_ms: Option<u64>,
}

/// Why the endpoint refused a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The hello did not parse, or names another protocol version.
    BadHello,
    /// The credential is not one the coordinator minted for this slot and generation, or it
    /// expired.
    Unauthorized,
    /// The endpoint wants a newer generation of this slot.
    StaleGeneration,
    /// An environment of the same generation that started later holds the tile.
    Superseded,
    /// No stream with this id is waiting for this tile.
    UnknownStream,
}

impl Refusal {
    /// Whether the tile should stop dialing for this generation: dialing again would be
    /// refused again for as long as this environment lives.
    pub fn is_final(self) -> bool {
        matches!(self, Refusal::StaleGeneration | Refusal::Superseded)
    }
}

/// The endpoint's answer to a hello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Refusal>,
}

/// What the endpoint sends on a control connection after its reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToTile {
    /// Dial a data connection for `stream` and connect it to the renderer's `port`.
    Open {
        stream: u64,
        port: Port,
    },
    Ping {
        n: u64,
    },
    /// The session ends: another environment holds the tile, or a newer generation is
    /// wanted. The tile stops dialing for this generation.
    Close {
        reason: Refusal,
    },
}

/// What a tile sends on its control connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromTile {
    Pong { n: u64 },
}

/// The tunnel key: HMAC-SHA256 keyed with the fleet's scene token, so every coordinator of
/// the fleet derives the same key and a credential survives a coordinator restart.
pub fn derive_key(scene_token: &str) -> ring::hmac::Key {
    let root = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, scene_token.as_bytes());
    let derived = ring::hmac::sign(&root, b"fcvm fleet lambda tile tunnel v1");
    ring::hmac::Key::new(ring::hmac::HMAC_SHA256, derived.as_ref())
}

fn message(slot: &str, generation: u64, expires_ms: u64) -> Vec<u8> {
    format!("v1\n{slot}\n{generation}\n{expires_ms}").into_bytes()
}

/// The credential for one slot and generation, valid until `expires_ms`:
/// `<expires_ms>.<hex HMAC>`.
pub fn mint(key: &ring::hmac::Key, slot: &str, generation: u64, expires_ms: u64) -> String {
    let tag = ring::hmac::sign(key, &message(slot, generation, expires_ms));
    format!("{expires_ms}.{}", hex::encode(tag.as_ref()))
}

/// Whether `credential` was minted with `key` for this slot and generation and is unexpired
/// at `now_ms`.
pub fn verify(
    key: &ring::hmac::Key,
    slot: &str,
    generation: u64,
    credential: &str,
    now_ms: u64,
) -> bool {
    let Some((expires, tag)) = credential.split_once('.') else {
        return false;
    };
    let Ok(expires_ms) = expires.parse::<u64>() else {
        return false;
    };
    let Ok(tag) = hex::decode(tag) else {
        return false;
    };
    expires_ms > now_ms
        && ring::hmac::verify(key, &message(slot, generation, expires_ms), &tag).is_ok()
}

/// Read one line (without its newline) of at most [`MAX_LINE`] bytes. A line cut off by the
/// limit or by the end of the stream is an error.
pub async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> Result<String> {
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        bail!("the connection closed");
    }
    if buf.last() != Some(&b'\n') {
        bail!("no line within {MAX_LINE} bytes");
    }
    buf.pop();
    String::from_utf8(buf).context("the line is not UTF-8")
}

/// Reads lines from a reader in a `select!` loop: the bytes of a line read so far stay in
/// `buf` when another branch wins and the read is dropped, so the next read goes on from them.
#[derive(Default)]
pub struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    /// The next line, without its newline. Cancel-safe: see [`Lines`].
    pub async fn next<R: tokio::io::AsyncBufRead + Unpin>(&mut self, r: &mut R) -> Result<String> {
        loop {
            if let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=end).collect();
                line.pop();
                return String::from_utf8(line).context("the line is not UTF-8");
            }
            if self.buf.len() > MAX_LINE {
                bail!("no line within {MAX_LINE} bytes");
            }
            // Bytes read by `read_until` are appended to `buf` as they arrive, so a read
            // dropped mid-line loses nothing.
            let room = (MAX_LINE + 1 - self.buf.len()) as u64;
            let n = (&mut *r)
                .take(room)
                .read_until(b'\n', &mut self.buf)
                .await?;
            if n == 0 {
                bail!("the connection closed");
            }
        }
    }
}

/// How long one control message may take to write: a peer that stops reading ends its
/// session, rather than stalling the loop (and its silence timer) behind a blocked write.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// [`write_line`], at most [`WRITE_TIMEOUT`].
async fn write_control<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, write_line(w, v))
        .await
        .context("the peer did not take a control message in time")?
}

/// Write `v` as one JSON line.
pub async fn write_line<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> Result<()> {
    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

/// A byte stream the endpoint hands out: one data connection.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Stream = Box<dyn Io>;

/// What the endpoint knows of a tile's current session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub generation: u64,
    pub env: String,
    pub started_ms: u64,
    /// When the session's control connection was accepted (Unix ms).
    pub since_ms: u64,
    /// The instance and its boot time, as the environment reported them ([`Hello`]).
    pub instance: Option<String>,
    pub instance_boot_ms: Option<u64>,
}

struct Live {
    info: Session,
    epoch: u64,
    to_tile: mpsc::Sender<ToTile>,
}

/// A stream waiting in [`Inner::pending`] for its data connection, removed when dropped.
struct PendingEntry<'a> {
    endpoint: &'a Endpoint,
    stream: u64,
}

impl Drop for PendingEntry<'_> {
    fn drop(&mut self) {
        self.endpoint
            .inner
            .lock()
            .unwrap()
            .pending
            .remove(&self.stream);
    }
}

impl Inner {
    /// Fail the streams `slot`'s session `epoch` was asked for (every epoch: `None`): that
    /// session no longer holds the tile, so its data connections would be refused, and an
    /// opener would otherwise wait out [`OPEN_TIMEOUT`] for a stream that cannot come.
    fn drop_pending(&mut self, slot: &str, epoch: Option<u64>) {
        self.pending
            .retain(|_, (s, e, _)| s != slot || epoch.is_some_and(|ep| *e != ep));
    }
}

#[derive(Default)]
struct Inner {
    live: HashMap<String, Live>,
    /// The lowest generation the endpoint accepts per slot.
    want: HashMap<String, u64>,
    /// Streams waiting for their data connection: stream id to the slot, the epoch of the
    /// session asked for it, and the opener.
    pending: HashMap<u64, (String, u64, oneshot::Sender<Stream>)>,
    next_stream: u64,
    next_epoch: u64,
}

/// The coordinator's half of the tunnel: the sessions, and the streams opened over them. The
/// caller terminates TLS and hands each connection to [`Endpoint::serve`].
pub struct Endpoint {
    key: ring::hmac::Key,
    inner: Mutex<Inner>,
    sessions: watch::Sender<BTreeMap<String, Session>>,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl Endpoint {
    pub fn new(key: ring::hmac::Key) -> Arc<Self> {
        Endpoint::with_clock(key, Box::new(crate::fleet::now_ms))
    }

    pub fn with_clock(key: ring::hmac::Key, now: Box<dyn Fn() -> u64 + Send + Sync>) -> Arc<Self> {
        Arc::new(Endpoint {
            key,
            inner: Mutex::new(Inner {
                next_stream: 1,
                next_epoch: 1,
                ..Inner::default()
            }),
            sessions: watch::channel(BTreeMap::new()).0,
            now,
        })
    }

    /// The current session of every tile, updated whenever one arrives or goes.
    pub fn sessions(&self) -> watch::Receiver<BTreeMap<String, Session>> {
        self.sessions.subscribe()
    }

    fn publish(&self, inner: &Inner) {
        let now: BTreeMap<String, Session> = inner
            .live
            .iter()
            .map(|(slot, l)| (slot.clone(), l.info.clone()))
            .collect();
        self.sessions.send_if_modified(|cur| {
            let changed = *cur != now;
            *cur = now;
            changed
        });
    }

    /// Accept only `generation` or newer for `slot` from now on, and end a session of an
    /// older generation.
    pub fn want(&self, slot: &str, generation: u64) {
        let mut inner = self.inner.lock().unwrap();
        let w = inner.want.entry(slot.to_string()).or_insert(0);
        *w = (*w).max(generation);
        let w = *w;
        if inner.live.get(slot).is_some_and(|l| l.info.generation < w) {
            let old = inner.live.remove(slot).unwrap();
            inner.drop_pending(slot, Some(old.epoch));
            let _ = old.to_tile.try_send(ToTile::Close {
                reason: Refusal::StaleGeneration,
            });
            self.publish(&inner);
        }
    }

    /// The slot left the assignment: end its session and forget it.
    pub fn forget(&self, slot: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.want.remove(slot);
        inner.drop_pending(slot, None);
        if let Some(old) = inner.live.remove(slot) {
            let _ = old.to_tile.try_send(ToTile::Close {
                reason: Refusal::StaleGeneration,
            });
        }
        self.publish(&inner);
    }

    /// Open a stream to `slot`'s renderer `port`: ask the tile for a data connection and wait
    /// for it, at most [`OPEN_TIMEOUT`].
    pub async fn open(&self, slot: &str, port: Port) -> Result<Stream> {
        let (tx, rx) = oneshot::channel();
        let (stream, to_tile) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(live) = inner.live.get(slot) else {
                bail!("tile {slot} has no tunnel");
            };
            let to_tile = live.to_tile.clone();
            let epoch = live.epoch;
            let waiting = inner.pending.values().filter(|(s, _, _)| s == slot).count();
            if waiting >= MAX_PENDING_PER_TILE {
                bail!("tile {slot} has {waiting} streams waiting for their connection");
            }
            let stream = inner.next_stream;
            inner.next_stream += 1;
            inner.pending.insert(stream, (slot.to_string(), epoch, tx));
            (stream, to_tile)
        };
        // The entry goes when this call ends, also when its caller drops it mid-wait: a
        // cancelled open would otherwise hold one of the tile's MAX_PENDING_PER_TILE places
        // for as long as its session lives.
        let _pending = PendingEntry {
            endpoint: self,
            stream,
        };
        let asked = to_tile.send(ToTile::Open { stream, port }).await.is_ok();
        let got = if asked {
            tokio::time::timeout(OPEN_TIMEOUT, rx)
                .await
                .ok()
                .and_then(|r| r.ok())
        } else {
            None
        };
        got.with_context(|| format!("tile {slot} did not connect stream {stream}"))
    }

    fn check(&self, hello: &Hello) -> Result<(), Refusal> {
        if hello.v != VERSION {
            return Err(Refusal::BadHello);
        }
        if !verify(
            &self.key,
            &hello.slot,
            hello.generation,
            &hello.credential,
            (self.now)(),
        ) {
            return Err(Refusal::Unauthorized);
        }
        Ok(())
    }

    /// Serve one connection a tile made (TLS already terminated): read its hello, then run
    /// its control session or hand its data stream to the opener waiting for it.
    pub async fn serve<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        self: &Arc<Self>,
        conn: S,
    ) -> Result<()> {
        let mut conn = BufReader::new(conn);
        let line = tokio::time::timeout(LINE_TIMEOUT, read_line(&mut conn))
            .await
            .context("no hello in time")??;
        let hello: Hello = match serde_json::from_str(&line) {
            Ok(h) => h,
            Err(_) => {
                refuse(&mut conn, Refusal::BadHello).await;
                bail!("a tunnel hello that does not parse");
            }
        };
        if let Err(r) = self.check(&hello) {
            refuse(&mut conn, r).await;
            bail!(
                "refused {:?} for {} generation {}",
                r,
                hello.slot,
                hello.generation
            );
        }
        match hello.kind {
            Kind::Data => self.data(hello, conn).await,
            Kind::Control => self.control(hello, conn).await,
        }
    }

    async fn data<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        &self,
        hello: Hello,
        mut conn: BufReader<S>,
    ) -> Result<()> {
        let opener = {
            let mut inner = self.inner.lock().unwrap();
            let holds = inner
                .live
                .get(&hello.slot)
                .filter(|l| l.info.env == hello.env && l.info.generation == hello.generation)
                .map(|l| l.epoch);
            match (hello.stream, holds) {
                (Some(id), Some(epoch))
                    if inner
                        .pending
                        .get(&id)
                        .is_some_and(|(s, e, _)| *s == hello.slot && *e == epoch) =>
                {
                    inner.pending.remove(&id).map(|(_, _, tx)| tx)
                }
                _ => None,
            }
        };
        let Some(opener) = opener else {
            refuse(&mut conn, Refusal::UnknownStream).await;
            bail!(
                "{} sent a data connection for no waiting stream",
                hello.slot
            );
        };
        write_line(
            &mut conn,
            &Reply {
                ok: true,
                error: None,
            },
        )
        .await?;
        // The opener gone (it timed out meanwhile) drops the connection.
        let _ = opener.send(Box::new(conn));
        Ok(())
    }

    /// Make `hello`'s environment the tile's session, or say why not: a generation older than
    /// the one wanted or the one held, or the same generation from an environment that started
    /// before the one holding it. Returns the new session's epoch and the session it replaced.
    fn take(
        &self,
        hello: &Hello,
        tx: mpsc::Sender<ToTile>,
    ) -> Result<(u64, Option<Live>), Refusal> {
        let mut inner = self.inner.lock().unwrap();
        let want = inner.want.get(&hello.slot).copied().unwrap_or(0);
        if hello.generation < want {
            return Err(Refusal::StaleGeneration);
        }
        match inner.live.get(&hello.slot) {
            Some(l) if l.info.generation > hello.generation => {
                return Err(Refusal::StaleGeneration)
            }
            Some(l)
                if l.info.generation == hello.generation
                    && (l.info.started_ms, l.info.env.as_str())
                        > (hello.started_ms, hello.env.as_str()) =>
            {
                return Err(Refusal::Superseded)
            }
            _ => {}
        }
        let epoch = inner.next_epoch;
        inner.next_epoch += 1;
        let info = Session {
            generation: hello.generation,
            env: hello.env.clone(),
            started_ms: hello.started_ms,
            since_ms: (self.now)(),
            instance: hello.instance.clone(),
            instance_boot_ms: hello.instance_boot_ms,
        };
        let replaced = inner.live.insert(
            hello.slot.clone(),
            Live {
                info,
                epoch,
                to_tile: tx,
            },
        );
        if let Some(old) = &replaced {
            inner.drop_pending(&hello.slot, Some(old.epoch));
        }
        self.publish(&inner);
        Ok((epoch, replaced))
    }

    async fn control<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        &self,
        hello: Hello,
        conn: BufReader<S>,
    ) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<ToTile>(1024);
        let taken = self.take(&hello, tx);
        let (epoch, replaced) = match taken {
            Ok(t) => t,
            Err(r) => {
                let mut conn = conn;
                refuse(&mut conn, r).await;
                bail!(
                    "refused {:?} for {} generation {}",
                    r,
                    hello.slot,
                    hello.generation
                );
            }
        };
        if let Some(old) = replaced {
            let reason = if old.info.generation < hello.generation {
                Refusal::StaleGeneration
            } else {
                Refusal::Superseded
            };
            let _ = old.to_tile.try_send(ToTile::Close { reason });
            tracing::info!(slot = %hello.slot, generation = hello.generation, old_env = %old.info.env, env = %hello.env, "a Lambda tile's environment was replaced");
        } else {
            tracing::info!(slot = %hello.slot, generation = hello.generation, env = %hello.env, "a Lambda tile's environment connected");
        }
        let (read, mut write) = tokio::io::split(conn);
        let mut read = BufReader::new(read);
        let result: Result<()> = async {
            write_line(
                &mut write,
                &Reply {
                    ok: true,
                    error: None,
                },
            )
            .await?;
            let mut ping = tokio::time::interval(PING_EVERY);
            let mut n = 0u64;
            let mut silent = Box::pin(tokio::time::sleep(SILENCE));
            let mut lines = Lines::default();
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        let Some(msg) = msg else { return Ok(()) };
                        let close = matches!(msg, ToTile::Close { .. });
                        write_control(&mut write, &msg).await?;
                        if close {
                            return Ok(());
                        }
                    }
                    _ = ping.tick() => {
                        n += 1;
                        write_control(&mut write, &ToTile::Ping { n }).await?;
                    }
                    line = lines.next(&mut read) => {
                        let line = line?;
                        match serde_json::from_str::<FromTile>(&line) {
                            Ok(FromTile::Pong { .. }) => {}
                            Err(_) => bail!("a line that is not a tunnel message"),
                        }
                        silent.as_mut().reset(tokio::time::Instant::now() + SILENCE);
                    }
                    _ = &mut silent => bail!("nothing from the tile for {SILENCE:?}"),
                }
            }
        }
        .await;
        let mut inner = self.inner.lock().unwrap();
        if inner
            .live
            .get(&hello.slot)
            .is_some_and(|l| l.epoch == epoch)
        {
            inner.live.remove(&hello.slot);
            inner.drop_pending(&hello.slot, Some(epoch));
            self.publish(&inner);
            tracing::info!(slot = %hello.slot, generation = hello.generation, env = %hello.env, "a Lambda tile's environment disconnected");
        }
        result
    }
}

/// How long the client waits before dialing again after a lost connection: doubling from
/// [`REDIAL_FIRST`] to [`REDIAL_MAX`], back to the first after a session that came up.
pub const REDIAL_FIRST: Duration = Duration::from_secs(1);
pub const REDIAL_MAX: Duration = Duration::from_secs(30);

/// Dials the coordinator's tunnel endpoint: one connection, TLS to the endpoint's name.
#[async_trait::async_trait]
pub trait Dialer: Send + Sync {
    async fn dial(&self) -> Result<Stream>;
}

/// Connects to one of the renderer's listeners inside the environment.
#[async_trait::async_trait]
pub trait Local: Send + Sync {
    async fn connect(&self, port: Port) -> Result<Stream>;
}

/// Who the tile is: everything its hellos carry but `kind` and `stream`.
#[derive(Debug, Clone)]
pub struct Identity {
    pub slot: String,
    pub generation: u64,
    pub env: String,
    pub started_ms: u64,
    pub credential: String,
    pub instance: Option<String>,
    pub instance_boot_ms: Option<u64>,
}

impl Identity {
    fn hello(&self, kind: Kind, stream: Option<u64>) -> Hello {
        Hello {
            v: VERSION,
            kind,
            slot: self.slot.clone(),
            generation: self.generation,
            env: self.env.clone(),
            started_ms: self.started_ms,
            credential: self.credential.clone(),
            stream,
            instance: self.instance.clone(),
            instance_boot_ms: self.instance_boot_ms,
        }
    }
}

/// Why [`run_client`] returned: the endpoint ended this environment's tunnel for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// A refusal or close that dialing again would repeat: a newer generation is wanted, an
    /// environment that started later holds the tile, or the credential is not accepted.
    Refused(Refusal),
    /// The caller asked it to stop.
    Stopped,
}

/// Send a hello on a fresh connection and read the endpoint's reply.
async fn greet(dialer: &dyn Dialer, hello: &Hello) -> Result<Result<BufReader<Stream>, Refusal>> {
    let mut conn = BufReader::new(dialer.dial().await?);
    write_line(&mut conn, hello).await?;
    let line = tokio::time::timeout(LINE_TIMEOUT, read_line(&mut conn))
        .await
        .context("no reply in time")??;
    let reply: Reply = serde_json::from_str(&line).context("a reply that does not parse")?;
    if reply.ok {
        Ok(Ok(conn))
    } else {
        Ok(Err(reply.error.unwrap_or(Refusal::BadHello)))
    }
}

/// Carry one stream: dial its data connection and join it to the renderer's `port`.
async fn carry(
    id: Identity,
    dialer: Arc<dyn Dialer>,
    local: Arc<dyn Local>,
    stream: u64,
    port: Port,
) {
    let result: Result<()> = async {
        let conn = match greet(&*dialer, &id.hello(Kind::Data, Some(stream))).await? {
            Ok(c) => c,
            Err(r) => bail!("stream {stream} refused: {r:?}"),
        };
        let mut renderer = local.connect(port).await?;
        // Bytes the endpoint sent right behind its reply are in the reader's buffer: forward
        // them before joining the two connections.
        let buffered = conn.buffer().to_vec();
        let mut conn = conn.into_inner();
        if !buffered.is_empty() {
            renderer.write_all(&buffered).await?;
        }
        tokio::io::copy_bidirectional(&mut conn, &mut renderer).await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::debug!(stream, error = %e, "a tunnel stream ended with an error");
    }
}

/// One control session: hello, then answer pings and open streams until the connection
/// ends. `Ok(Some(_))` ends the tunnel for good, `Ok(None)` means dial again.
async fn session(
    id: &Identity,
    dialer: &Arc<dyn Dialer>,
    local: &Arc<dyn Local>,
    up: &watch::Sender<bool>,
) -> Result<Option<Ended>> {
    let conn = match greet(&**dialer, &id.hello(Kind::Control, None)).await? {
        Ok(c) => c,
        Err(r) => return Ok(Some(Ended::Refused(r))),
    };
    up.send_replace(true);
    let (read, mut write) = tokio::io::split(conn);
    let mut read = BufReader::new(read);
    let result = async {
        loop {
            let line = tokio::time::timeout(SILENCE, read_line(&mut read))
                .await
                .context("nothing from the endpoint")??;
            match serde_json::from_str::<ToTile>(&line)
                .context("a line that is not a tunnel message")?
            {
                ToTile::Ping { n } => write_control(&mut write, &FromTile::Pong { n }).await?,
                ToTile::Open { stream, port } => {
                    tokio::spawn(carry(
                        id.clone(),
                        dialer.clone(),
                        local.clone(),
                        stream,
                        port,
                    ));
                }
                ToTile::Close { reason } => return Ok(Some(Ended::Refused(reason))),
            }
        }
    }
    .await;
    up.send_replace(false);
    result
}

/// The tile's side of the tunnel: keep a control session to the endpoint, dialing again after
/// a lost connection (backoff [`REDIAL_FIRST`] doubling to [`REDIAL_MAX`]), until the endpoint
/// ends it for good or `stop` turns true. `up` says whether a session is up right now.
pub async fn run_client(
    id: Identity,
    dialer: Arc<dyn Dialer>,
    local: Arc<dyn Local>,
    up: watch::Sender<bool>,
    mut stop: watch::Receiver<bool>,
) -> Ended {
    let mut wait = REDIAL_FIRST;
    loop {
        if *stop.borrow() {
            return Ended::Stopped;
        }
        let came_up = {
            let mut up_rx = up.subscribe();
            up_rx.mark_unchanged();
            let r = tokio::select! {
                r = session(&id, &dialer, &local, &up) => r,
                _ = stop.wait_for(|s| *s) => {
                    up.send_replace(false);
                    return Ended::Stopped;
                }
            };
            let came_up = up_rx.has_changed().unwrap_or(false);
            match r {
                Ok(Some(Ended::Refused(reason)))
                    if reason.is_final() || reason == Refusal::Unauthorized =>
                {
                    tracing::info!(slot = %id.slot, generation = id.generation, ?reason, "the tunnel endpoint ended this environment's tunnel");
                    return Ended::Refused(reason);
                }
                Ok(Some(other)) => {
                    tracing::warn!(slot = %id.slot, ?other, "the tunnel endpoint refused the hello; dialing again");
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::info!(slot = %id.slot, error = %e, "the tunnel connection was lost; dialing again")
                }
            }
            came_up
        };
        if came_up {
            wait = REDIAL_FIRST;
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|s| *s) => return Ended::Stopped,
        }
        wait = (wait * 2).min(REDIAL_MAX);
    }
}

async fn refuse<W: AsyncWrite + Unpin>(w: &mut W, r: Refusal) {
    let _ = write_line(
        w,
        &Reply {
            ok: false,
            error: Some(r),
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ring::hmac::Key {
        derive_key("scene-token-for-tests")
    }

    #[test]
    fn a_credential_is_bound_to_its_slot_generation_and_expiry() {
        let k = key();
        let c = mint(&k, "lam-1", 7, 2_000);
        assert!(verify(&k, "lam-1", 7, &c, 1_000));
        assert!(!verify(&k, "lam-2", 7, &c, 1_000), "another slot");
        assert!(!verify(&k, "lam-1", 8, &c, 1_000), "another generation");
        assert!(!verify(&k, "lam-1", 7, &c, 2_000), "expired");
        let forged = c.replacen("2000.", "9000.", 1);
        assert!(!verify(&k, "lam-1", 7, &forged, 1_000), "a moved expiry");
        assert!(!verify(&derive_key("another token"), "lam-1", 7, &c, 1_000));
        assert!(!verify(&k, "lam-1", 7, "garbage", 1_000));
    }

    #[test]
    fn messages_have_the_documented_shape() {
        let open = serde_json::to_value(ToTile::Open {
            stream: 3,
            port: Port::Tls,
        })
        .unwrap();
        assert_eq!(
            open,
            serde_json::json!({"open": {"stream": 3, "port": "tls"}})
        );
        let ping = serde_json::to_value(ToTile::Ping { n: 1 }).unwrap();
        assert_eq!(ping, serde_json::json!({"ping": {"n": 1}}));
        let pong = serde_json::to_value(FromTile::Pong { n: 1 }).unwrap();
        assert_eq!(pong, serde_json::json!({"pong": {"n": 1}}));
        let r = serde_json::to_value(Reply {
            ok: false,
            error: Some(Refusal::StaleGeneration),
        })
        .unwrap();
        assert_eq!(
            r,
            serde_json::json!({"ok": false, "error": "stale_generation"})
        );
    }

    #[tokio::test]
    async fn a_line_past_the_limit_is_refused() {
        let long = vec![b'a'; MAX_LINE + 10];
        let mut r = BufReader::new(&long[..]);
        assert!(read_line(&mut r).await.is_err());
        let mut ok = BufReader::new(&b"{\"x\":1}\nrest"[..]);
        assert_eq!(read_line(&mut ok).await.unwrap(), "{\"x\":1}");
    }

    /// A dialer whose connections the endpoint serves in process. `drop_first` connections are
    /// closed by the "network" before the endpoint sees them.
    struct InProcess {
        ep: Arc<Endpoint>,
        dials: std::sync::atomic::AtomicUsize,
        drop_first: usize,
    }

    #[async_trait::async_trait]
    impl Dialer for InProcess {
        async fn dial(&self) -> Result<Stream> {
            let n = self.dials.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (a, b) = tokio::io::duplex(1 << 16);
            if n < self.drop_first {
                drop(b);
            } else {
                let ep = self.ep.clone();
                tokio::spawn(async move {
                    let _ = ep.serve(b).await;
                });
            }
            Ok(Box::new(a))
        }
    }

    /// The renderer stand-in: each connection answers with its port's name, then echoes.
    struct Renderer;

    #[async_trait::async_trait]
    impl Local for Renderer {
        async fn connect(&self, port: Port) -> Result<Stream> {
            let (a, mut b) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let tag: &[u8] = match port {
                    Port::Tls => b"tls:",
                    Port::Http => b"http:",
                };
                if b.write_all(tag).await.is_err() {
                    return;
                }
                let mut buf = [0u8; 1024];
                loop {
                    match b.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if b.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
            Ok(Box::new(a))
        }
    }

    const NOW: u64 = 1_000_000;

    fn endpoint() -> Arc<Endpoint> {
        Endpoint::with_clock(key(), Box::new(|| NOW))
    }

    fn identity(slot: &str, generation: u64, env: &str, started_ms: u64) -> Identity {
        Identity {
            slot: slot.into(),
            generation,
            env: env.into(),
            started_ms,
            credential: mint(&key(), slot, generation, NOW + 60_000),
            instance: Some(format!("instance-of-{env}")),
            instance_boot_ms: Some(started_ms - 500),
        }
    }

    struct Tile {
        ended: tokio::task::JoinHandle<Ended>,
        up: watch::Receiver<bool>,
        stop: watch::Sender<bool>,
        dialer: Arc<InProcess>,
    }

    fn start(ep: &Arc<Endpoint>, id: Identity, drop_first: usize) -> Tile {
        let dialer = Arc::new(InProcess {
            ep: ep.clone(),
            dials: Default::default(),
            drop_first,
        });
        let (up_tx, up) = watch::channel(false);
        let (stop, stop_rx) = watch::channel(false);
        let ended = tokio::spawn(run_client(
            id,
            dialer.clone(),
            Arc::new(Renderer),
            up_tx,
            stop_rx,
        ));
        Tile {
            ended,
            up,
            stop,
            dialer,
        }
    }

    async fn session_of(ep: &Arc<Endpoint>, slot: &str, env: &str) -> Session {
        let mut rx = ep.sessions();
        let s = tokio::time::timeout(
            Duration::from_secs(10),
            rx.wait_for(|m| m.get(slot).is_some_and(|s| s.env == env)),
        )
        .await
        .expect("the session arrives")
        .unwrap();
        s.get(slot).unwrap().clone()
    }

    async fn ended(t: Tile) -> Ended {
        tokio::time::timeout(Duration::from_secs(10), t.ended)
            .await
            .expect("the client ends")
            .unwrap()
    }

    async fn roundtrip(ep: &Arc<Endpoint>, slot: &str, port: Port, msg: &[u8]) -> Vec<u8> {
        let mut s = ep.open(slot, port).await.expect("the stream opens");
        s.write_all(msg).await.unwrap();
        let tag = match port {
            Port::Tls => 4,
            Port::Http => 5,
        };
        let mut got = vec![0u8; tag + msg.len()];
        tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut got))
            .await
            .expect("an answer")
            .unwrap();
        got
    }

    #[tokio::test]
    async fn streams_reach_the_renderers_listeners_through_the_tunnel() {
        let ep = endpoint();
        let t = start(&ep, identity("lam-1", 3, "env-a", 5_000), 0);
        let s = session_of(&ep, "lam-1", "env-a").await;
        assert_eq!(s.generation, 3);
        assert_eq!(s.instance.as_deref(), Some("instance-of-env-a"));
        assert_eq!(s.instance_boot_ms, Some(4_500));
        assert!(*t.up.borrow());
        assert_eq!(
            roundtrip(&ep, "lam-1", Port::Tls, b"hello").await,
            b"tls:hello"
        );
        assert_eq!(
            roundtrip(&ep, "lam-1", Port::Http, b"GET /").await,
            b"http:GET /"
        );
        // Two streams at once, each on its own connection.
        let (a, b) = tokio::join!(
            roundtrip(&ep, "lam-1", Port::Tls, b"one"),
            roundtrip(&ep, "lam-1", Port::Tls, b"two")
        );
        assert_eq!(
            (a.as_slice(), b.as_slice()),
            (&b"tls:one"[..], &b"tls:two"[..])
        );
        t.stop.send_replace(true);
        assert_eq!(ended(t).await, Ended::Stopped);
    }

    #[tokio::test]
    async fn an_older_generation_than_wanted_is_refused_and_stops_dialing() {
        let ep = endpoint();
        ep.want("lam-1", 5);
        let t = start(&ep, identity("lam-1", 4, "env-a", 5_000), 0);
        let dialer = t.dialer.clone();
        assert_eq!(ended(t).await, Ended::Refused(Refusal::StaleGeneration));
        assert_eq!(dialer.dials.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(ep.sessions().borrow().is_empty());
    }

    #[tokio::test]
    async fn wanting_a_newer_generation_ends_the_old_environments_tunnel() {
        let ep = endpoint();
        let t = start(&ep, identity("lam-1", 1, "env-a", 5_000), 0);
        session_of(&ep, "lam-1", "env-a").await;
        ep.want("lam-1", 2);
        assert_eq!(ended(t).await, Ended::Refused(Refusal::StaleGeneration));
        let n = start(&ep, identity("lam-1", 2, "env-b", 6_000), 0);
        session_of(&ep, "lam-1", "env-b").await;
        assert_eq!(roundtrip(&ep, "lam-1", Port::Http, b"x").await, b"http:x");
        n.stop.send_replace(true);
    }

    #[tokio::test]
    async fn of_two_environments_of_one_generation_the_later_started_holds_the_tile() {
        // The later one dials second: it takes the tile and the earlier one is closed.
        let ep = endpoint();
        let old = start(&ep, identity("lam-1", 1, "env-a", 5_000), 0);
        session_of(&ep, "lam-1", "env-a").await;
        let new = start(&ep, identity("lam-1", 1, "env-b", 9_000), 0);
        session_of(&ep, "lam-1", "env-b").await;
        assert_eq!(ended(old).await, Ended::Refused(Refusal::Superseded));
        // The later one dials first: the earlier one is refused when it dials.
        let late = start(&ep, identity("lam-1", 1, "env-c", 2_000), 0);
        assert_eq!(ended(late).await, Ended::Refused(Refusal::Superseded));
        assert_eq!(ep.sessions().borrow().get("lam-1").unwrap().env, "env-b");
        assert_eq!(roundtrip(&ep, "lam-1", Port::Tls, b"x").await, b"tls:x");
        new.stop.send_replace(true);
    }

    #[tokio::test]
    async fn a_lost_connection_is_dialed_again() {
        let ep = endpoint();
        let t = start(&ep, identity("lam-1", 1, "env-a", 5_000), 1);
        session_of(&ep, "lam-1", "env-a").await;
        assert_eq!(t.dialer.dials.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(roundtrip(&ep, "lam-1", Port::Http, b"y").await, b"http:y");
        t.stop.send_replace(true);
    }

    #[tokio::test]
    async fn a_credential_the_endpoint_did_not_mint_ends_the_tunnel() {
        let ep = endpoint();
        let mut id = identity("lam-1", 1, "env-a", 5_000);
        id.credential = mint(&derive_key("another fleet"), "lam-1", 1, NOW + 60_000);
        let t = start(&ep, id, 0);
        assert_eq!(ended(t).await, Ended::Refused(Refusal::Unauthorized));
    }

    #[tokio::test]
    async fn a_stream_for_a_tile_without_a_tunnel_fails_at_once() {
        let ep = endpoint();
        let err = ep.open("lam-9", Port::Tls).await.err().unwrap();
        assert!(err.to_string().contains("has no tunnel"), "{err}");
    }

    #[tokio::test]
    async fn a_stream_waiting_on_a_replaced_environment_fails_at_once() {
        // An environment that holds the tile but never connects the streams it is asked for.
        let ep = endpoint();
        let id = identity("lam-1", 1, "env-a", 5_000);
        let (a, b) = tokio::io::duplex(1 << 16);
        let server = ep.clone();
        tokio::spawn(async move {
            let _ = server.serve(b).await;
        });
        let mut a = BufReader::new(a);
        write_line(&mut a, &id.hello(Kind::Control, None))
            .await
            .unwrap();
        assert_eq!(read_line(&mut a).await.unwrap(), r#"{"ok":true}"#);
        session_of(&ep, "lam-1", "env-a").await;
        let opener = ep.clone();
        let open = tokio::spawn(async move {
            let t0 = std::time::Instant::now();
            let r = opener.open("lam-1", Port::Tls).await;
            (r.is_err(), t0.elapsed())
        });
        // The stream is asked of env-a; then an environment that started later takes the tile.
        loop {
            let line = read_line(&mut a).await.unwrap();
            if matches!(serde_json::from_str(&line), Ok(ToTile::Open { .. })) {
                break;
            }
        }
        let b = start(&ep, identity("lam-1", 1, "env-b", 9_000), 0);
        session_of(&ep, "lam-1", "env-b").await;
        let (failed, took) = open.await.unwrap();
        assert!(
            failed,
            "the stream was asked of an environment that no longer holds the tile"
        );
        assert!(
            took < Duration::from_secs(2),
            "it waited {took:?} instead of failing at once"
        );
        b.stop.send_replace(true);
    }

    /// A hand-driven control session on a duplex of `size` bytes: the hello is sent and the
    /// endpoint's `ok` read, and the tile side is returned.
    async fn manual_session(
        ep: &Arc<Endpoint>,
        id: &Identity,
        size: usize,
    ) -> BufReader<tokio::io::DuplexStream> {
        let (a, b) = tokio::io::duplex(size);
        let server = ep.clone();
        tokio::spawn(async move {
            let _ = server.serve(b).await;
        });
        let mut a = BufReader::new(a);
        write_line(&mut a, &id.hello(Kind::Control, None))
            .await
            .unwrap();
        assert_eq!(read_line(&mut a).await.unwrap(), r#"{"ok":true}"#);
        session_of(ep, &id.slot, &id.env).await;
        a
    }

    /// A pong that arrives in two pieces, with a stream asked of the tile between them: the
    /// endpoint's read of the first piece is dropped when the open wins its `select!`, and the
    /// line must still parse whole once the rest arrives.
    #[tokio::test]
    async fn a_control_line_split_across_reads_survives_another_branch_winning() {
        let ep = endpoint();
        let id = identity("lam-1", 1, "env-a", 5_000);
        let mut a = manual_session(&ep, &id, 1 << 16).await;
        a.get_mut().write_all(br#"{"pong":"#).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The open's message wins the endpoint's select! while it waits for the pong's rest.
        let opener = ep.clone();
        let open = tokio::spawn(async move { opener.open("lam-1", Port::Http).await.is_ok() });
        loop {
            let line = read_line(&mut a).await.unwrap();
            if matches!(serde_json::from_str(&line), Ok(ToTile::Open { .. })) {
                break;
            }
        }
        a.get_mut().write_all(b"{\"n\":1}}\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            ep.sessions().borrow().contains_key("lam-1"),
            "the split pong ended the tile's session"
        );
        open.abort();
    }

    /// An open whose caller gives up (its future dropped mid-wait) does not keep its place
    /// among the tile's waiting streams.
    #[tokio::test]
    async fn a_cancelled_open_frees_its_place() {
        let ep = endpoint();
        let id = identity("lam-1", 1, "env-a", 5_000);
        let _a = manual_session(&ep, &id, 1 << 16).await;
        let opener = ep.clone();
        let open = tokio::spawn(async move { opener.open("lam-1", Port::Http).await });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ep.inner.lock().unwrap().pending.is_empty() {
            assert!(std::time::Instant::now() < deadline, "the open waits");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        open.abort();
        let _ = open.await;
        assert!(
            ep.inner.lock().unwrap().pending.is_empty(),
            "the cancelled open kept its place"
        );
    }

    /// A tile that stops reading its control connection: the endpoint's writes back up, and
    /// a blocked write must end the session instead of stalling the loop and its silence timer.
    #[tokio::test(start_paused = true)]
    async fn a_tile_that_stops_reading_loses_its_session() {
        let ep = endpoint();
        let id = identity("lam-1", 1, "env-a", 5_000);
        // A 20-byte pipe: the reply (12 bytes) and the first ping (17) already fill it past
        // what the tile reads with the reply, so the ping at 10 s blocks, well before the
        // 30 s silence timer could end the session on its own.
        let _a = manual_session(&ep, &id, 20).await;
        let mut sessions = ep.sessions();
        tokio::time::timeout(
            Duration::from_secs(600),
            sessions.wait_for(|m| !m.contains_key("lam-1")),
        )
        .await
        .expect("the session of a tile that stopped reading ends")
        .unwrap();
    }
}
