//! A clone's side of the pool: claim a namespace and hold its lease.

use super::proto::{self, Grant, Reply, Request, Stats, GRANT_FDS};
use crate::network::netns_agent;
use anyhow::{bail, Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;

/// How long a clone waits for the pool's answer before it builds its own namespace. The
/// pool answers a claim from memory; this bounds a pool that is stopped or wedged.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(2);

/// A namespace lent to this process. Dropping it ends the lease, unless another process
/// was given the connection ([`Lease::fd`]) and still runs.
pub struct Lease {
    /// The connection to the pool. It is the lease.
    connection: AsyncFd<UnixStream>,
    user_namespace: OwnedFd,
    net_namespace: OwnedFd,
    /// This process's channel to the namespace's holder, for sockets in the namespace.
    pub agent: Arc<netns_agent::Client>,
    pub grant: Grant,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease").field("grant", &self.grant).finish()
    }
}

impl Lease {
    /// The lease's descriptor, for the VMM to inherit
    /// ([`crate::utils::NamespaceParams::keep_fds`]).
    pub fn fd(&self) -> RawFd {
        self.connection.get_ref().as_raw_fd()
    }

    /// A path that names the namespace's user namespace for as long as this lease is held:
    /// the descriptor the pool sent, not `/proc/<holder>/ns/user`, so it cannot come to
    /// name another process's namespace.
    pub fn user_namespace_path(&self) -> std::path::PathBuf {
        format!("/proc/self/fd/{}", self.user_namespace.as_raw_fd()).into()
    }

    /// As [`Self::user_namespace_path`], for the network namespace.
    pub fn net_namespace_path(&self) -> std::path::PathBuf {
        format!("/proc/self/fd/{}", self.net_namespace.as_raw_fd()).into()
    }

    /// Resolves when the pool ends the lease: the pool exited, or the namespace's holder or
    /// pasta did and the pool took the namespace back. Never resolves otherwise.
    pub async fn ended(&self) {
        loop {
            let Ok(mut guard) = self.connection.readable().await else {
                return;
            };
            match guard.try_io(|connection| proto::receive(connection.get_ref())) {
                // The pool sends nothing after the grant, so anything read is its end.
                Ok(_) => return,
                Err(_would_block) => continue,
            }
        }
    }
}

pub(super) fn connect(path: &std::path::Path) -> std::io::Result<Option<UnixStream>> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: socket takes no pointers.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: socket returned a new descriptor nothing else owns.
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: sockaddr_un is plain data, valid when zeroed.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the pool's socket path is too long",
        ));
    }
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    // SAFETY: `address` is a sockaddr_un that outlives the call.
    let rc = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            // No pool has run here, or the one that did is gone; or its backlog is full,
            // which a connect on a blocking socket would wait out for as long as the pool
            // is stopped.
            Some(libc::ENOENT) | Some(libc::ECONNREFUSED) | Some(libc::EAGAIN) => Ok(None),
            _ => Err(error),
        };
    }
    Ok(Some(socket))
}

/// Refuse a connection whose other end does not run as `uid` (`SO_PEERCRED`: the user that
/// called `listen` on the socket).
pub(super) fn require_peer(connection: &UnixStream, uid: libc::uid_t) -> Result<()> {
    // SAFETY: ucred is plain data, valid when zeroed.
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: both pointers are to locals that outlive the call.
    let rc = unsafe {
        libc::getsockopt(
            connection.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("reading the pool's credentials");
    }
    anyhow::ensure!(
        peer.uid == uid,
        "the namespace pool's socket is served by uid {}, not by this user ({uid})",
        peer.uid
    );
    Ok(())
}

/// Claim a namespace for a clone whose network has the pool key `key`.
///
/// `None` when no pool serves this data directory for this user, when the pool has no
/// free namespace, or when its namespaces are not what `key` asks for: the clone then
/// builds its own. An error is a pool that answered wrongly or not in time; the caller
/// holds no namespace then either and builds its own (`PastaNetwork::setup`).
pub async fn claim(key: &str) -> Result<Option<Lease>> {
    let Some(dir) = proto::existing_dir()? else {
        return Ok(None);
    };
    claim_at(&proto::socket_path(&dir), key).await
}

/// [`claim`] from the pool listening on `socket`.
pub async fn claim_at(socket: &std::path::Path, key: &str) -> Result<Option<Lease>> {
    let Some((connection, reply, mut fds)) = ask(
        socket,
        &Request::Claim {
            key: key.to_string(),
        },
    )
    .await?
    else {
        return Ok(None);
    };
    let grant = match reply {
        Reply::Miss { reason } => {
            tracing::debug!(reason, "the namespace pool has nothing for this clone");
            return Ok(None);
        }
        Reply::Ok(grant) => grant,
        Reply::Stats(_) => bail!("the namespace pool answered a claim with its counters"),
    };
    if fds.len() != GRANT_FDS {
        bail!(
            "the namespace pool's grant carried {} descriptors, not {GRANT_FDS}",
            fds.len()
        );
    }
    let agent = UnixStream::from(fds.pop().unwrap());
    let net_namespace = fds.pop().unwrap();
    let user_namespace = fds.pop().unwrap();
    Ok(Some(Lease {
        connection,
        user_namespace,
        net_namespace,
        agent: Arc::new(netns_agent::Client::new(agent).context("registering the holder channel")?),
        grant,
    }))
}

/// One request to the pool listening on `socket` and its reply, or `None` when nothing
/// listens there.
async fn ask(
    socket: &std::path::Path,
    request: &Request,
) -> Result<Option<(AsyncFd<UnixStream>, Reply, Vec<OwnedFd>)>> {
    let Some(connection) = connect(socket).context("connecting to the namespace pool")? else {
        return Ok(None);
    };
    // The reply names namespaces this process then enters. A pool is its user's own
    // (`<data_dir>/netpool/<user>/`), so whatever listens here as another user is not it.
    require_peer(&connection, nix::unistd::geteuid().as_raw())?;
    let connection = proto::watch(connection).context("registering the pool connection")?;
    let request = serde_json::to_vec(request)?;
    let exchange = async {
        loop {
            let mut guard = connection.writable().await?;
            match guard.try_io(|c| proto::send(c.get_ref(), &request, &[])) {
                Ok(sent) => break sent?,
                Err(_would_block) => continue,
            }
        }
        loop {
            let mut guard = connection.readable().await?;
            match guard.try_io(|c| proto::receive(c.get_ref())) {
                Ok(received) => break received,
                Err(_would_block) => continue,
            }
        }
    };
    let (reply, fds) = tokio::time::timeout(CLAIM_TIMEOUT, exchange)
        .await
        .with_context(|| format!("the namespace pool did not answer within {CLAIM_TIMEOUT:?}"))?
        .context("talking to the namespace pool")?
        .context("the namespace pool closed the connection without answering")?;
    let reply: Reply = serde_json::from_slice(&reply).with_context(|| {
        format!(
            "the namespace pool answered {:?}",
            String::from_utf8_lossy(&reply)
        )
    })?;
    Ok(Some((connection, reply, fds)))
}

/// The counters of the pool listening on `socket`, or `None` when nothing listens there.
pub async fn stats_at(socket: &std::path::Path) -> Result<Option<Stats>> {
    match ask(socket, &Request::Stats).await? {
        None => Ok(None),
        Some((_, Reply::Stats(stats), _)) => Ok(Some(*stats)),
        Some((_, other, _)) => bail!("the namespace pool answered a stats request with {other:?}"),
    }
}

/// `fcvm netpool stats`: the running pool's counters as one JSON line.
pub async fn print_stats() -> Result<()> {
    let dir = proto::existing_dir()?.context("no namespace pool has run in this data directory")?;
    let stats = stats_at(&proto::socket_path(&dir))
        .await?
        .context("no namespace pool is running for this user in this data directory")?;
    println!("{}", serde_json::to_string(&stats)?);
    Ok(())
}
