//! `fcvm netpool serve`: build namespaces, lend them, reset them, replace the ones that fail.
//!
//! Everything runs on the thread that called [`serve`], in one [`tokio::task::LocalSet`].
//! The kernel sends a child its parent-death signal when the thread that forked it exits,
//! so the pool's holders and pastas are forked from a thread that lives as long as the
//! pool does, and from no other.
//!
//! One task per namespace takes it through its life ([`slot_task`]): build, wait for a
//! lease, hand it over, wait for the lease's end, reset, and around again; or destroy and
//! build again when the reset does not leave it as it was built or one of its processes
//! died. One task per connection answers a request ([`answer`]). They share the slot
//! table ([`super::slots`]), which decides who may have which namespace.

use super::proto::{self, Grant, Reply, Request, Stats};
use super::slots::{Claimed, SlotId, State, Table};
use crate::network::{netns_agent, NetworkManager, PastaNetwork};
use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::oneshot;
use tracing::{info, warn};

/// The least time between two namespaces destroyed. The kernel destroys the namespaces that
/// died since its cleanup worker last ran as one batch under `rtnl_mutex`; this keeps a
/// batch at one namespace, which is one hold of 1.1 to 1.5 ms, or of 15 to 16 ms on a
/// kernel that still has the bridge's `rcu_barrier()`
/// (`bench/start-rate/pieces/RESULTS.md` changelog 9).
pub const DESTROY_SPACING: Duration = Duration::from_millis(50);

/// Namespaces built at once. A build is bound by `rtnl_mutex`, so more builders wait for
/// each other and for every clone that is starting.
const BUILD_CONCURRENCY: usize = 8;

/// Namespaces reset at once. A reset starts a pasta and waits for its PID file with an
/// inotify instance, of which a user has 128 by default; past that the wait polls.
const RESET_CONCURRENCY: usize = 64;

/// How long a connection has to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a reset waits for the namespace to read as it did when it was built: what
/// trails a link change (a route's link-down flag, pasta's TAP coming up again) settles
/// well within it.
const VERIFY_BUDGET: Duration = Duration::from_secs(2);

/// How long a failed build waits before the next attempt.
const REBUILD_DELAY: Duration = Duration::from_secs(1);
/// The pause after a failed `accept`.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);
/// How often, and how far apart, a pool that is starting asks again for the host's IPv6
/// address when the probe fails.
const HOST_PROBE_TRIES: u32 = 20;
const HOST_PROBE_PAUSE: Duration = Duration::from_millis(250);
/// The least time between two reports of a claim for another key.
const KEY_MISS_REPORT: Duration = Duration::from_secs(30);

/// A lease handed to a namespace's task: the claimer's connection and what the table
/// recorded for it.
struct Offer {
    connection: UnixStream,
    claimed: Claimed,
}

struct Pool {
    dir: PathBuf,
    /// The key of the network every namespace of this pool is built from.
    key: String,
    table: RefCell<Table>,
    /// For each ready namespace, where to send its next lease.
    offers: RefCell<HashMap<SlotId, oneshot::Sender<Offer>>>,
    counters: RefCell<Stats>,
    builds: tokio::sync::Semaphore,
    resets: tokio::sync::Semaphore,
    /// Held by the one task that is destroying a namespace.
    destroying: tokio::sync::Mutex<()>,
    /// When a claim for another key was last reported.
    key_miss_reported: std::cell::Cell<Option<std::time::Instant>>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl Pool {
    fn stats(&self) -> Stats {
        let counts = self.table.borrow().counts();
        Stats {
            building: counts.building,
            ready: counts.ready,
            leased: counts.leased,
            resetting: counts.resetting,
            destroying: counts.destroying,
            ..*self.counters.borrow()
        }
    }

    fn stopping(&self) -> bool {
        *self.stop.borrow()
    }
}

/// One built namespace: its holder, its pasta, and what a lease is given.
struct Namespace {
    holder: tokio::process::Child,
    holder_pid: u32,
    agent: std::sync::Arc<netns_agent::Client>,
    network: PastaNetwork,
    user_namespace: std::fs::File,
    net_namespace: std::fs::File,
    /// The queue the TAP is attached by: a copy of the one the holder keeps. Every lease
    /// gets a copy for its VMM.
    tap: OwnedFd,
    /// The fingerprint taken when the build finished.
    built_as: String,
    pid_dir: PathBuf,
}

/// The network every namespace of a pool is built from: a restored clone's, with no
/// published port. Its [`PastaNetwork::pool_key`] is the pool's key.
fn slot_network(vm_id: String, pid_dir: PathBuf) -> PastaNetwork {
    PastaNetwork::new(vm_id, proto::TAP.to_string(), Vec::new())
        .with_pid_dir(pid_dir)
        .with_attached_tap()
        .with_control_socket()
}

impl Namespace {
    /// Build a namespace the way an unpooled clone's is built, through the same holder,
    /// the same link builder and the same pasta start.
    async fn build(pid_dir: PathBuf, vm_id: String) -> Result<Self> {
        Self::build_with(pid_dir, vm_id, true).await
    }

    /// [`Self::build`], with pasta started or not. A unit test builds without it: pasta is
    /// a setup asset, and the unit lanes run no setup.
    async fn build_with(pid_dir: PathBuf, vm_id: String, pasta: bool) -> Result<Self> {
        tokio::fs::create_dir_all(&pid_dir)
            .await
            .with_context(|| format!("creating {}", pid_dir.display()))?;
        let mut network = slot_network(vm_id, pid_dir.clone());
        network.setup().await.context("configuring the network")?;
        let (mut holder, holder_pid) = crate::commands::common::spawn_namespace_holder()
            .await
            .context("starting the holder")?;
        let built = async {
            network
                .build_namespace(holder_pid)
                .await
                .context("building the links")?;
            if pasta {
                network
                    .post_start(holder_pid)
                    .await
                    .context("starting pasta")?;
            }
            let agent = netns_agent::client(holder_pid)
                .context("the holder's channel is not registered")?;
            let open = |kind: &str| {
                let path = format!("/proc/{holder_pid}/ns/{kind}");
                std::fs::File::open(&path).with_context(|| format!("opening {path}"))
            };
            let (user_namespace, net_namespace) = (open("user")?, open("net")?);
            let tap = agent.queue().await.context("getting the TAP's queue")?;
            let built_as = agent
                .fingerprint(proto::TAP)
                .await
                .context("taking the fingerprint")?;
            Ok::<_, anyhow::Error>((agent, user_namespace, net_namespace, tap, built_as))
        }
        .await;
        match built {
            Ok((agent, user_namespace, net_namespace, tap, built_as)) => Ok(Self {
                holder,
                holder_pid,
                agent,
                network,
                user_namespace,
                net_namespace,
                tap,
                built_as,
                pid_dir,
            }),
            Err(error) => {
                let _ = network.cleanup().await;
                netns_agent::forget(holder_pid);
                crate::companion::forget(holder_pid);
                let _ = holder.kill().await;
                Err(error)
            }
        }
    }

    /// Resolves when the holder or pasta exits, with which and how.
    async fn died(&mut self) -> String {
        tokio::select! {
            status = self.holder.wait() => format!("its holder (PID {}) exited: {status:?}", self.holder_pid),
            status = self.network.companion_exited() => {
                self.network.describe_companion_exit(status).await
            }
        }
    }

    /// Hand the namespace to the claimer on `offer`: a channel to the holder of its own,
    /// and the grant with the namespace descriptors. The connection comes back as the
    /// lease to watch.
    async fn grant(&self, slot: SlotId, offer: Offer) -> Result<AsyncFd<UnixStream>> {
        let (holder_end, lessee_end) =
            netns_agent::channel().context("creating the lessee's channel")?;
        self.agent
            .adopt(&holder_end)
            .await
            .context("giving the holder the lessee's channel")?;
        drop(holder_end);
        let reply = serde_json::to_vec(&Reply::Ok(Grant {
            holder_pid: self.holder_pid,
            tap: proto::TAP.to_string(),
            generation: offer.claimed.generation,
            slot,
        }))?;
        proto::send(
            &offer.connection,
            &reply,
            &[
                self.user_namespace.as_raw_fd(),
                self.net_namespace.as_raw_fd(),
                lessee_end.as_raw_fd(),
                self.tap.as_raw_fd(),
            ],
        )
        .context("sending the grant")?;
        proto::watch(offer.connection).context("watching the lease")
    }

    /// After a lease: have pasta forget the guest, have the holder remove what a VM's use
    /// leaves, and check the namespace against its fingerprint. An error means it must not
    /// be lent again.
    ///
    /// The lease's end is what says the VMM is gone. The TAP is attached by its holder
    /// for the namespace's whole life, so its carrier cannot say whether a VMM still has the
    /// queue, as it could while a VMM attached by name. The queue goes to the VMM alone,
    /// together with the lease, and is close-on-exec in every other process.
    async fn reset(&mut self) -> Result<()> {
        failpoint::hit_async("pool.pre_reset").await;
        // The namespace first, pasta second. A frame of the last tenant can still be in
        // the kernel on its way to pasta when the lease ends; one that reaches a pasta
        // already reset opens a new flow, and the host's answer would be framed to the
        // next guest. With pasta reset last, such a frame has the whole of the holder's
        // reset to arrive in and be forgotten with the rest. That is a margin, not a
        // proof: nothing here waits for the kernel's queues to be empty.
        self.agent
            .reset()
            .await
            .context("resetting the namespace")?;
        self.network
            .reset_pasta()
            .await
            .context("resetting pasta")?;
        let deadline = tokio::time::Instant::now() + VERIFY_BUDGET;
        let mut pause = Duration::from_millis(2);
        loop {
            let now = self
                .agent
                .fingerprint(proto::TAP)
                .await
                .context("taking the fingerprint")?;
            if now == self.built_as {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "the namespace does not read as it was built, {VERIFY_BUDGET:?} after its \
                     lease ended:\n{}",
                    difference(&self.built_as, &now)
                );
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(100));
        }
    }

    /// Stop pasta and the holder. The kernel destroys the namespace once they are gone.
    async fn destroy(mut self) {
        if let Err(error) = self.network.cleanup().await {
            warn!(holder_pid = self.holder_pid, error = %format!("{error:#}"), "stopping a pooled pasta");
        }
        netns_agent::forget(self.holder_pid);
        crate::companion::forget(self.holder_pid);
        drop(self.agent);
        let _ = self.holder.kill().await;
        let _ = tokio::fs::remove_dir_all(&self.pid_dir).await;
    }
}

/// The lines of two fingerprints that differ: `-` as built, `+` as found.
fn difference(built: &str, found: &str) -> String {
    let (built_lines, found_lines): (Vec<&str>, Vec<&str>) =
        (built.lines().collect(), found.lines().collect());
    let mut out = Vec::new();
    for line in &built_lines {
        if !found_lines.contains(line) {
            out.push(format!("- {line}"));
        }
    }
    for line in &found_lines {
        if !built_lines.contains(line) {
            out.push(format!("+ {line}"));
        }
    }
    out.join("\n")
}

impl Pool {
    /// Say, at most once in [`KEY_MISS_REPORT`], that a clone asked for namespaces this pool
    /// does not have, and whether the pool itself is out of date: its key is from its
    /// start, and the host's IPv6 address, its resolver file or the sudo invoker may have
    /// changed since. A pool whose key no clone asks for serves nothing, and without this
    /// line nothing says so.
    fn report_key_miss(&self, asked: &str) {
        let now = std::time::Instant::now();
        if self
            .key_miss_reported
            .get()
            .is_some_and(|last| now.duration_since(last) < KEY_MISS_REPORT)
        {
            return;
        }
        self.key_miss_reported.set(Some(now));
        // A probe that failed says nothing about the host: the plain report is made.
        let current = slot_network(String::new(), self.dir.clone()).pool_key_now();
        if let Some(current) = current.filter(|current| *current != self.key) {
            warn!(
                built_for = %self.key,
                host_now = %current,
                asked,
                "the host has changed since this pool started (its IPv6 address, \
                 /etc/resolv.conf, /etc/subuid, /etc/subgid or the invoking user): the \
                 pool's namespaces match no clone started now. Restart the pool"
            );
        } else {
            warn!(
                pool = %self.key,
                asked,
                "a clone asked for a namespace built otherwise than this pool's (another \
                 resolver, MTU or user); it builds its own"
            );
        }
    }
}

/// Resolves at the lease's end: every process that held the connection has exited.
async fn lease_ended(lease: &AsyncFd<UnixStream>) {
    loop {
        let Ok(mut guard) = lease.readable().await else {
            return;
        };
        match guard.try_io(|connection| proto::receive(connection.get_ref())) {
            // End-of-file or an error ends the lease. A lessee has nothing to say after
            // its claim, so a packet is dropped and the wait goes on, an empty or cut one
            // (`InvalidData`) included: neither is the end of the connection.
            Ok(Ok(Some(_))) => continue,
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::InvalidData => continue,
            Ok(_) => return,
            Err(_would_block) => continue,
        }
    }
}

/// Lend the namespace of `slot` until it must be destroyed, and say why.
async fn lend(pool: &Pool, slot: SlotId, namespace: &mut Namespace) -> String {
    let mut stop = pool.stop.subscribe();
    loop {
        // Ready: the offer's receiver exists before the table says so, with no await
        // between, so a claim that gets this namespace always finds where to send it.
        let (offer, offered) = oneshot::channel();
        pool.offers.borrow_mut().insert(slot, offer);
        let offer = tokio::select! {
            // An offer that has arrived is taken, whatever else is ready: it holds a
            // claimer's connection, and dropping it would fail a claim the table already
            // counted.
            biased;
            offer = offered => offer,
            reason = namespace.died() => {
                pool.offers.borrow_mut().remove(&slot);
                return reason;
            }
            _ = stop.wait_for(|stopping| *stopping) => {
                pool.offers.borrow_mut().remove(&slot);
                return "the pool is stopping".to_string();
            }
        };
        let Ok(offer) = offer else {
            return "its offer was dropped".to_string();
        };
        // The stop and an offer can both be ready when this task runs. A grant made now
        // would be revoked in the next statement's select, and a clone that has a grant
        // and loses it fails, where one that is refused builds its own namespace. Dropping
        // the offer closes the claimer's connection, which is that refusal.
        if pool.stopping() {
            return "the pool is stopping".to_string();
        }
        let claimed = offer.claimed;
        let lease = match namespace.grant(slot, offer).await {
            Ok(lease) => lease,
            // The claimer may have a channel to the holder already: this namespace is not
            // known to be as it was built.
            Err(error) => return format!("its grant failed: {error:#}"),
        };
        {
            let mut counters = pool.counters.borrow_mut();
            counters.grants += 1;
            if claimed.generation >= 2 {
                counters.recycled_grants += 1;
            }
        }
        tokio::select! {
            _ = lease_ended(&lease) => {}
            reason = namespace.died() => return format!("{reason}, while it was lent"),
            _ = stop.wait_for(|stopping| *stopping) => return "the pool is stopping".to_string(),
        }
        drop(lease);
        let ended = std::time::Instant::now();
        if let Err(refused) = pool.table.borrow_mut().released(slot, claimed.lease) {
            return refused.to_string();
        }
        let reset = {
            let _permit = pool.resets.acquire().await;
            namespace.reset().await
        };
        if let Err(error) = reset {
            return format!("{error:#}");
        }
        pool.counters
            .borrow_mut()
            .record_reset(ended.elapsed().as_micros() as u64);
        if let Err(refused) = pool.table.borrow_mut().reset_done(slot) {
            return refused.to_string();
        }
    }
}

/// One namespace's life, again and again until the pool stops.
async fn slot_task(pool: Rc<Pool>, slot: SlotId) {
    loop {
        if pool.stopping() {
            return;
        }
        let built = {
            let _permit = pool.builds.acquire().await;
            Namespace::build(
                pool.dir.join("slots").join(slot.to_string()),
                format!("netpool-{}-{slot}", std::process::id()),
            )
            .await
        };
        let mut namespace = match built {
            Ok(namespace) => namespace,
            Err(error) => {
                warn!(slot, error = %format!("{error:#}"), "building a pooled namespace failed");
                tokio::time::sleep(REBUILD_DELAY).await;
                continue;
            }
        };
        pool.counters.borrow_mut().builds += 1;
        if let Err(refused) = pool.table.borrow_mut().built(slot) {
            warn!(slot, %refused, "the slot table refused a built namespace");
        }
        let reason = lend(&pool, slot, &mut namespace).await;
        // Whatever state it was in, it is not lent again; a lease it had ended when the
        // connection was dropped in `lend`.
        if pool.table.borrow().state(slot) != State::Destroying {
            let _ = pool.table.borrow_mut().condemn(slot);
        }
        if !pool.stopping() {
            warn!(slot, reason, "destroying a pooled namespace");
        }
        {
            // One at a time, and not faster than the kernel cleans up after each.
            let _turn = pool.destroying.lock().await;
            namespace.destroy().await;
            tokio::time::sleep(DESTROY_SPACING).await;
        }
        pool.counters.borrow_mut().destroys += 1;
        let _ = pool.table.borrow_mut().destroyed(slot);
    }
}

async fn receive_request(connection: &AsyncFd<UnixStream>) -> Result<Request> {
    loop {
        let mut guard = connection.readable().await?;
        match guard.try_io(|connection| proto::receive(connection.get_ref())) {
            Ok(received) => {
                let (message, _) = received?.context("the client closed the connection")?;
                return serde_json::from_slice(&message)
                    .with_context(|| format!("reading {:?}", String::from_utf8_lossy(&message)));
            }
            Err(_would_block) => continue,
        }
    }
}

fn send_reply(connection: &UnixStream, reply: &Reply) {
    if let Ok(reply) = serde_json::to_vec(reply) {
        // A client that left gets no answer.
        let _ = proto::send(connection, &reply, &[]);
    }
}

/// Answer one connection: lend a namespace, or say why not, or report the counters.
async fn answer(pool: Rc<Pool>, connection: UnixStream) {
    let Ok(connection) = proto::watch(connection) else {
        return;
    };
    let request = match tokio::time::timeout(REQUEST_TIMEOUT, receive_request(&connection)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => {
            warn!(error = %format!("{error:#}"), "a pool client sent no request");
            return;
        }
        Err(_) => return,
    };
    let mut connection = connection.into_inner();
    let key = match request {
        Request::Stats => return send_reply(&connection, &Reply::Stats(Box::new(pool.stats()))),
        Request::Claim { key } => key,
    };
    let miss = |connection: &UnixStream, reason: &str| {
        pool.counters.borrow_mut().misses += 1;
        send_reply(
            connection,
            &Reply::Miss {
                reason: reason.to_string(),
            },
        );
    };
    if key != pool.key {
        pool.report_key_miss(&key);
        return miss(
            &connection,
            "the pool's namespaces are not built as this clone's would be",
        );
    }
    if pool.stopping() {
        return miss(&connection, "the pool is stopping");
    }
    loop {
        let Some(claimed) = pool.table.borrow_mut().claim() else {
            return miss(&connection, "no namespace is free");
        };
        let offer = pool.offers.borrow_mut().remove(&claimed.slot);
        let Some(offer) = offer else {
            // A ready namespace always has an offer registered (`lend`).
            warn!(
                slot = claimed.slot,
                "a ready namespace had no task waiting; skipping it"
            );
            let _ = pool.table.borrow_mut().condemn(claimed.slot);
            continue;
        };
        match offer.send(Offer {
            connection,
            claimed,
        }) {
            Ok(()) => return,
            // Its task stopped waiting in this instant (a process of the namespace died);
            // the task condemns it. Try the next one.
            Err(returned) => connection = returned.connection,
        }
    }
}

/// Have the host's IPv6 address probed before the pool makes its key or builds a
/// namespace: both use the address as this process first found it, for as long as the pool
/// runs. A probe that failed would leave a pool whose key says the host has no address,
/// which no clone on a host that has one ever matches. So the pool waits for an answer,
/// five seconds at most, and does not start without one.
async fn host_ipv6_probed() -> Result<()> {
    let mut tries = 0;
    loop {
        match PastaNetwork::host_ipv6_known() {
            Ok(()) => return Ok(()),
            Err(error) if tries < HOST_PROBE_TRIES => {
                tries += 1;
                warn!(%error, tries, "probing the host's IPv6 address; trying again");
                tokio::time::sleep(HOST_PROBE_PAUSE).await;
            }
            Err(error) => {
                return Err(error)
                    .context("probing the host's IPv6 address, which the pool's key holds")
            }
        }
    }
}

fn listen(path: &Path) -> Result<AsyncFd<UnixStream>> {
    use std::os::unix::ffi::OsStrExt;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("removing {}", path.display())),
    }
    // SAFETY: socket takes no pointers.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("creating the pool's socket");
    }
    // SAFETY: socket returned a new descriptor nothing else owns. A listening socket is
    // held in a `UnixStream` only as an owner of the descriptor; it is never read.
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: sockaddr_un is plain data, valid when zeroed.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() {
        bail!("the pool's socket path {} is too long", path.display());
    }
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    // SAFETY: `address` is a sockaddr_un that outlives both calls.
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        ) == 0
            && libc::listen(socket.as_raw_fd(), 4096) == 0
    };
    if !bound {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("listening on {}", path.display()));
    }
    proto::watch(socket).context("registering the pool's socket")
}

async fn accept(listener: &AsyncFd<UnixStream>) -> std::io::Result<UnixStream> {
    loop {
        let mut guard = listener.readable().await?;
        let accepted = guard.try_io(|listener| {
            // SAFETY: accept4 with null address pointers takes no other pointers.
            let fd = unsafe {
                libc::accept4(
                    listener.get_ref().as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: accept4 returned a new descriptor nothing else owns.
            Ok(unsafe { UnixStream::from_raw_fd(fd) })
        });
        match accepted {
            Ok(connection) => return connection,
            Err(_would_block) => continue,
        }
    }
}

/// Run a pool of `slots` namespaces for this user and data directory until SIGTERM or
/// SIGINT, then destroy them one at a time. A second signal ends the process at once, and
/// the kernel then kills every holder and pasta with it.
pub async fn serve(slots: usize) -> Result<()> {
    anyhow::ensure!(slots > 0, "a pool needs at least one namespace");
    let dir = proto::dir()?;
    let lock_path = proto::lock_path(&dir);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    let _lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, errno)| {
            anyhow::anyhow!(
                "another pool serves this data directory for this user ({}: {errno})",
                lock_path.display()
            )
        })?;
    // What a killed pool left: its PID files. The lock says no other pool uses them.
    match std::fs::remove_dir_all(dir.join("slots")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing a previous pool's slot directories"),
    }
    host_ipv6_probed().await?;
    let key = slot_network(String::new(), dir.clone()).pool_key();
    let socket_path = proto::socket_path(&dir);
    let listener = listen(&socket_path)?;
    let (stop, _) = tokio::sync::watch::channel(false);
    let pool = Rc::new(Pool {
        dir,
        key,
        table: RefCell::new(Table::new(slots)),
        offers: RefCell::new(HashMap::new()),
        counters: RefCell::new(Stats::default()),
        builds: tokio::sync::Semaphore::new(BUILD_CONCURRENCY),
        resets: tokio::sync::Semaphore::new(RESET_CONCURRENCY),
        destroying: tokio::sync::Mutex::new(()),
        key_miss_reported: std::cell::Cell::new(None),
        stop,
    });
    info!(slots, socket = %socket_path.display(), key = %pool.key, "namespace pool starting");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let tasks: Vec<_> = (0..slots as SlotId)
                .map(|slot| tokio::task::spawn_local(slot_task(pool.clone(), slot)))
                .collect();
            loop {
                tokio::select! {
                    accepted = accept(&listener) => match accepted {
                        Ok(connection) => {
                            tokio::task::spawn_local(answer(pool.clone(), connection));
                        }
                        Err(error) => {
                            // Out of descriptors, most likely, and the listener stays
                            // readable: without a pause this loop would spin on it.
                            warn!(%error, "accepting a pool connection");
                            tokio::time::sleep(ACCEPT_RETRY).await;
                        }
                    },
                    _ = sigterm.recv() => break,
                    _ = sigint.recv() => break,
                }
            }
            info!("namespace pool stopping: destroying its namespaces one at a time");
            // Stop listening before the drain, which takes 50 ms a namespace: a clone that
            // connects from here on is refused at once and builds its own namespace, and
            // one waiting in the backlog sees the connection close.
            let _ = std::fs::remove_file(&socket_path);
            drop(listener);
            pool.stop.send_replace(true);
            let drained = async {
                for task in tasks {
                    let _ = task.await;
                }
            };
            tokio::select! {
                _ = drained => info!(stats = ?pool.stats(), "namespace pool stopped"),
                _ = sigterm.recv() => warn!("second signal: leaving the rest to the kernel"),
                _ = sigint.recv() => warn!("second signal: leaving the rest to the kernel"),
            }
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A claim that reaches a namespace's task together with the pool's stop is refused,
    /// not granted and then revoked: the claimer's connection closes with nothing sent on
    /// it. The offer and the stop are both made ready between two polls of `lend`, which
    /// is the one schedule where its `biased` select takes the offer of a stopping pool.
    #[tokio::test(flavor = "current_thread")]
    async fn a_claim_that_arrives_with_the_stop_is_refused_not_granted() {
        let scratch = tempfile::tempdir().unwrap();
        // Without pasta, a setup asset the unit lanes do not have: nothing here needs it.
        let mut namespace =
            Namespace::build_with(scratch.path().join("slot"), "stop-claim".into(), false)
                .await
                .expect("building a namespace as the pool does");
        let (stop, _) = tokio::sync::watch::channel(false);
        let pool = Pool {
            dir: scratch.path().to_path_buf(),
            key: String::new(),
            table: RefCell::new(Table::new(1)),
            offers: RefCell::new(HashMap::new()),
            counters: RefCell::new(Stats::default()),
            builds: tokio::sync::Semaphore::new(1),
            resets: tokio::sync::Semaphore::new(1),
            destroying: tokio::sync::Mutex::new(()),
            key_miss_reported: std::cell::Cell::new(None),
            stop,
        };
        pool.table.borrow_mut().built(0).unwrap();
        let (claimer, pools_end) = netns_agent::channel().unwrap();
        let reason = {
            let lending = lend(&pool, 0, &mut namespace);
            tokio::pin!(lending);
            // One poll: `lend` registers its offer and waits.
            tokio::select! {
                biased;
                reason = &mut lending => panic!("lend ended before a claim: {reason}"),
                _ = std::future::ready(()) => {}
            }
            let claimed = pool
                .table
                .borrow_mut()
                .claim()
                .expect("the ready namespace");
            let offer = pool
                .offers
                .borrow_mut()
                .remove(&claimed.slot)
                .expect("the namespace's offer");
            assert!(offer
                .send(Offer {
                    connection: pools_end,
                    claimed,
                })
                .is_ok());
            pool.stop.send_replace(true);
            lending.await
        };
        assert_eq!(reason, "the pool is stopping");
        assert_eq!(
            pool.counters.borrow().grants,
            0,
            "a stopping pool granted a namespace"
        );
        assert!(
            matches!(proto::receive(&claimer), Ok(None)),
            "the claimer was sent something before its connection closed"
        );
        namespace.destroy().await;
    }

    /// The pool has the host's address probed, with the probe's failure an error, before it
    /// makes its key: a key from a failed probe would say the host has no IPv6 address for
    /// the pool's life.
    #[test]
    fn the_pool_probes_the_host_before_it_makes_its_key() {
        let source = include_str!("server.rs");
        let start = source.find("pub async fn serve(slots: usize)").unwrap();
        let body = &source[start..];
        let probed = body.find("host_ipv6_probed().await?;").expect("no probe");
        let key = body.find(".pool_key()").expect("no key");
        assert!(probed < key, "the key is made before the host is probed");
        let probe = &source[source.find("async fn host_ipv6_probed()").unwrap()..];
        assert!(probe[..probe.find("\nfn listen(").unwrap()]
            .contains("PastaNetwork::host_ipv6_known()"));
    }

    /// A pooled namespace is built by the code an unpooled clone's is, so the two read the
    /// same but for the TAP's name. This fails when the pool gets a build of its own.
    ///
    /// Neither side starts pasta: what is compared is the links, and pasta is a setup
    /// asset the unit lanes do not have.
    #[tokio::test(flavor = "current_thread")]
    async fn a_pooled_namespace_reads_as_a_clones_own_would() {
        let scratch = tempfile::tempdir().unwrap();
        let pooled =
            Namespace::build_with(scratch.path().join("pooled"), "pooled-build".into(), false)
                .await
                .expect("building a namespace as the pool does");

        let own_dir = scratch.path().join("own");
        std::fs::create_dir_all(&own_dir).unwrap();
        let (mut holder, holder_pid) = crate::commands::common::spawn_namespace_holder()
            .await
            .expect("spawning a clone's holder");
        let mut own = PastaNetwork::new("own-build".into(), "tap-own".into(), Vec::new())
            .with_pid_dir(own_dir)
            .with_restore_mode();
        own.build_namespace(holder_pid)
            .await
            .expect("building the links");
        // A bridge is created with a carrier and loses it once it has ports and none of
        // them forwards, which the kernel works out through its link watch some time after
        // the ports are added (0.1 to 2.1 s in six builds on an idle host). The clone's own
        // namespace has no attached TAP yet, so its bridge settles without one: read it
        // then, not during the time it still shows the carrier it was created with.
        let own_agent = netns_agent::client(holder_pid).unwrap();
        let settle_by = tokio::time::Instant::now() + Duration::from_secs(20);
        let own_fingerprint = loop {
            let now = own_agent
                .fingerprint("tap-own")
                .await
                .expect("fingerprinting the clone's own namespace");
            if !bridge_has_carrier(&now) {
                break now;
            }
            assert!(
                tokio::time::Instant::now() < settle_by,
                "the clone's own bridge kept a carrier with no attached port:\n{now}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        assert!(
            pooled.built_as.contains("link tap-pool "),
            "{}",
            pooled.built_as
        );
        assert!(
            pooled.built_as.contains("link pasta0 "),
            "{}",
            pooled.built_as
        );
        assert!(
            pooled.built_as.contains("addr br0 10.0.2.1/24"),
            "{}",
            pooled.built_as
        );
        // The one difference: the pooled TAP is attached from its build on, by its holder,
        // and has a carrier; a clone's own is attached when its VMM starts. Each bridge
        // follows its ports: the pooled one has a carrier, the clone's own has none.
        assert!(tap_flags(&pooled.built_as, proto::TAP) & CARRIER == CARRIER);
        assert!(tap_flags(&own_fingerprint, "tap-own") & CARRIER == 0);
        assert!(bridge_has_carrier(&pooled.built_as), "{}", pooled.built_as);
        assert!(!bridge_has_carrier(&own_fingerprint), "{own_fingerprint}");
        assert_eq!(
            comparable(&pooled.built_as, proto::TAP),
            comparable(&own_fingerprint, "tap-own"),
        );

        let _ = own.cleanup().await;
        let _ = holder.kill().await;
        netns_agent::forget(holder_pid);
        crate::companion::forget(holder_pid);
        pooled.destroy().await;
    }

    /// IFF_LOWER_UP: the flag of a link with a carrier that a fingerprint keeps.
    const CARRIER: u32 = 0x1_0000;

    /// The flags on a fingerprint's `link` line, when it is one.
    fn link_flags(line: &str) -> Option<u32> {
        let flags = line.strip_prefix("link ")?.split("flags=0x").nth(1)?;
        u32::from_str_radix(flags.split_whitespace().next()?, 16).ok()
    }

    /// Whether a fingerprint's bridge shows a carrier.
    fn bridge_has_carrier(fingerprint: &str) -> bool {
        let bridge = fingerprint
            .lines()
            .find(|line| line.ends_with("kind=bridge"))
            .unwrap_or_else(|| panic!("no bridge: {fingerprint}"));
        link_flags(bridge).unwrap() & CARRIER != 0
    }

    /// The flags on the fingerprint's line for the link `tap`.
    fn tap_flags(fingerprint: &str, tap: &str) -> u32 {
        fingerprint
            .lines()
            .find(|line| line.starts_with(&format!("link {tap} ")))
            .and_then(link_flags)
            .unwrap_or_else(|| panic!("no link {tap}: {fingerprint}"))
    }

    /// A fingerprint with what differs between any two namespaces taken out: the VMM's
    /// TAP's name, the MAC the kernel drew for each TAP, whether the VMM's TAP has a
    /// carrier yet, and with it the bridge's, which follows its ports'.
    fn comparable(fingerprint: &str, tap: &str) -> String {
        let flags = tap_flags(fingerprint, tap);
        let mut text = fingerprint
            .replace(
                &format!("link {tap} flags={flags:#x} "),
                &format!("link {tap} flags={:#x} ", flags & !CARRIER),
            )
            .replace(tap, "TAP")
            .lines()
            .map(|line| match link_flags(line) {
                Some(flags) if line.ends_with("kind=bridge") => line.replacen(
                    &format!("flags={flags:#x} "),
                    &format!("flags={:#x} ", flags & !CARRIER),
                    1,
                ),
                _ => line.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        if fingerprint.ends_with('\n') {
            text.push('\n');
        }
        let drawn: Vec<(String, String)> = text
            .lines()
            .filter(|line| line.starts_with("link ") && line.ends_with("kind=tun"))
            .filter_map(|line| {
                let name = line.split_whitespace().nth(1)?;
                let mac = line.split("mac=").nth(1)?.split_whitespace().next()?;
                Some((mac.to_string(), format!("MAC-OF-{name}")))
            })
            .collect();
        assert_eq!(drawn.len(), 2, "two TAPs: {fingerprint}");
        for (mac, name) in drawn {
            text = text.replace(&mac, &name);
        }
        text
    }

    /// A claimer talks only to a pool that runs as its own user: the reply names namespaces
    /// it then enters.
    #[tokio::test(flavor = "current_thread")]
    async fn a_pool_socket_served_by_another_user_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool.sock");
        let _listener = listen(&path).unwrap();
        let connection = super::super::client::connect(&path)
            .unwrap()
            .expect("a listening socket");
        let own = nix::unistd::geteuid().as_raw();
        super::super::client::require_peer(&connection, own).unwrap();
        let refused = super::super::client::require_peer(&connection, own + 1).unwrap_err();
        assert!(
            format!("{refused:#}").contains("not by this user"),
            "{refused:#}"
        );
    }

    #[test]
    fn a_difference_names_what_left_and_what_came() {
        let built = "addr br0 10.0.2.1/24\nlink tap-pool flags=0x1003\n";
        let found = "addr br0 10.0.2.1/24\nlink tap-pool flags=0x11003\nroute 192.0.2.0/24\n";
        assert_eq!(
            difference(built, found),
            "- link tap-pool flags=0x1003\n+ link tap-pool flags=0x11003\n+ route 192.0.2.0/24"
        );
    }
}
