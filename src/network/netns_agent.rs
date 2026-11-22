//! The rootless namespace holder: `fcvm __netns-agent`.
//!
//! One process per rootless VM namespace. It creates the user and network namespaces, keeps
//! them alive for as long as it runs, and serves its owner over the socket it was started
//! with. A holder that can act inside its namespaces saves every other process the
//! `nsenter` fork and exec it would need to get there.
//!
//! The agent is one thread. `unshare(CLONE_NEWUSER)` refuses a process with more, so a
//! binary's `main` calls [`main_if_agent`] before it does anything else, and the serve loop
//! never blocks on anything but its owner's next request.
//!
//! Start, with the owner being the process that spawned the agent:
//!
//! | agent | owner |
//! |---|---|
//! | `unshare(CLONE_NEWUSER \| CLONE_NEWNET)`, arm `PR_SET_PDEATHSIG`, send `unshared` | |
//! | | write the uid and gid maps, send `mapped` |
//! | check it is uid 0 in the namespace, send `ready` | |
//!
//! Then requests, one message each way:
//!
//! | request | reply |
//! |---|---|
//! | `tcp4`, `tcp6` | `ok` with a non-blocking, unconnected TCP socket made in the namespace |
//!
//! A socket belongs to the network namespace it was made in for its whole life, so the
//! owner connects it to a guest address without entering the namespace.
//!
//! The agent exits when the owner's end of the channel closes (the owner dropped it or
//! exited) and the kernel kills it when the thread that spawned it exits
//! (`PR_SET_PDEATHSIG`). The namespaces go when their last process does.

use anyhow::{bail, Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

/// The first argument of an agent's command line: `fcvm __netns-agent <channel fd> <owner
/// pid>`.
pub const SUBCOMMAND: &str = "__netns-agent";

/// The agent's `comm`, so `ps`, `pgrep` and a process tree name it apart from the fcvm that
/// owns it.
pub const COMM: &std::ffi::CStr = c"fcvm-netns";

const UNSHARED: &[u8] = b"unshared";
const MAPPED: &[u8] = b"mapped";
const READY: &[u8] = b"ready";
const OK: &[u8] = b"ok";
const TCP4: &[u8] = b"tcp4";
const TCP6: &[u8] = b"tcp6";

/// The longest message either side sends. Every message is one `sendmsg` of fewer bytes
/// than this on a `SOCK_SEQPACKET` socket, so one `recvmsg` returns exactly one message.
const MAX_MESSAGE: usize = 256;

/// A connected pair of `SOCK_SEQPACKET` sockets, both close-on-exec: the owner's end and
/// the agent's. Sequenced packets keep message boundaries, and a closed peer reads as
/// end-of-file, which a datagram pair does not report.
pub fn channel() -> std::io::Result<(UnixStream, UnixStream)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is two descriptors long, which is what socketpair writes.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: socketpair returned two new descriptors nothing else owns.
    Ok(unsafe {
        (
            UnixStream::from_raw_fd(fds[0]),
            UnixStream::from_raw_fd(fds[1]),
        )
    })
}

fn send(channel: &UnixStream, message: &[u8]) -> std::io::Result<()> {
    // SAFETY: the buffer outlives the call; MSG_NOSIGNAL turns a closed peer into EPIPE.
    let sent = unsafe {
        libc::send(
            channel.as_raw_fd(),
            message.as_ptr().cast(),
            message.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// One message and the descriptor that came with it, or `None` at end-of-file.
fn receive(channel: &UnixStream) -> std::io::Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    let mut buffer = [0u8; MAX_MESSAGE];
    let (len, file) = channel
        .recv_with_fd(&mut buffer)
        .map_err(|error| std::io::Error::from_raw_os_error(error.errno()))?;
    if len == 0 {
        return Ok(None);
    }
    Ok(Some((buffer[..len].to_vec(), file.map(OwnedFd::from))))
}

/// A TCP socket made in the calling process's network namespace: non-blocking and
/// close-on-exec, not yet connected.
fn tcp_socket(family: libc::c_int) -> std::io::Result<OwnedFd> {
    // SAFETY: socket takes no pointers.
    let fd = unsafe {
        libc::socket(
            family,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: socket returned a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Answer the owner's requests until its end of the channel closes.
///
/// A request the agent cannot serve is answered with `err <reason>` and the agent keeps
/// serving: one bad request must not take the namespace down.
pub fn serve(channel: &UnixStream) -> Result<()> {
    while let Some((request, _)) = receive(channel).context("reading a request")? {
        let family = match request.as_slice() {
            TCP4 => libc::AF_INET,
            TCP6 => libc::AF_INET6,
            other => {
                let reply = format!("err unknown request {:?}", String::from_utf8_lossy(other));
                send(channel, reply.as_bytes()).context("answering an unknown request")?;
                continue;
            }
        };
        match tcp_socket(family) {
            Ok(socket) => {
                channel
                    .send_with_fd(OK, socket.as_raw_fd())
                    .map_err(|error| std::io::Error::from_raw_os_error(error.errno()))
                    .context("sending a socket")?;
            }
            Err(error) => {
                send(channel, format!("err socket: {error}").as_bytes())
                    .context("answering a failed socket")?;
            }
        }
    }
    Ok(())
}

/// Set once [`main_if_agent`] has run in this process: its executable can be an agent.
static CAN_BE_AGENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The first call of a binary's `main`. When the process was started as an agent this runs
/// it and returns its result for `main` to return; otherwise it returns `None` and records
/// that this executable is one [`executable`] can start agents from.
pub fn main_if_agent() -> Option<Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(SUBCOMMAND)) {
        CAN_BE_AGENT.store(true, std::sync::atomic::Ordering::Relaxed);
        return None;
    }
    let number = |what: &str, arg: Option<std::ffi::OsString>| {
        arg.as_deref()
            .and_then(std::ffi::OsStr::to_str)
            .and_then(|text| text.parse::<u32>().ok())
            .with_context(|| format!("{SUBCOMMAND}: missing or malformed {what}"))
    };
    Some((|| {
        let channel = number("channel descriptor", args.next())?;
        let owner = number("owner pid", args.next())?;
        run(channel as RawFd, owner)
    })())
}

/// The executable to start an agent from: this one when its `main` begins with
/// [`main_if_agent`], otherwise the `fcvm` beside it or one directory up (a test binary in
/// `target/<profile>/deps`).
pub fn executable() -> Result<std::path::PathBuf> {
    let current = std::env::current_exe().context("finding this executable")?;
    if CAN_BE_AGENT.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(current);
    }
    let mut tried = Vec::new();
    for dir in current.ancestors().skip(1).take(2) {
        let candidate = dir.join("fcvm");
        if candidate.is_file() {
            return Ok(candidate);
        }
        tried.push(candidate.display().to_string());
    }
    bail!(
        "no fcvm binary to start the namespace agent from: {} does not run one itself, and \
         none of {} exists (build fcvm first)",
        current.display(),
        tried.join(", ")
    )
}

/// `fcvm __netns-agent <channel fd> <owner pid>`: create the namespaces, hold them, serve
/// the owner. Runs on the process's only thread.
pub fn run(channel: RawFd, owner: u32) -> Result<()> {
    // First, so the process is findable by name for its whole life.
    // SAFETY: COMM is a NUL-terminated string that outlives the call.
    unsafe { libc::prctl(libc::PR_SET_NAME, COMM.as_ptr()) };
    // SAFETY: the owner left the agent's end of the channel open across the exec on this
    // descriptor, and nothing else in this process has opened it.
    let channel = unsafe { UnixStream::from_raw_fd(channel) };
    // SAFETY: fcntl with F_SETFD takes no pointers. The owner cleared the flag for the exec.
    unsafe { libc::fcntl(channel.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: unshare takes no pointers.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
        bail!(
            "unshare(CLONE_NEWUSER | CLONE_NEWNET): {} (max_user_namespaces={})",
            std::io::Error::last_os_error(),
            std::fs::read_to_string("/proc/sys/user/max_user_namespaces")
                .unwrap_or_default()
                .trim()
        );
    }
    // After the unshare, so no credential change can clear it, and checked against the
    // owner: an owner that died before this line has already reparented the agent, and no
    // signal would come.
    // SAFETY: prctl with these arguments takes no pointers.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        bail!(
            "arming the parent-death signal: {}",
            std::io::Error::last_os_error()
        );
    }
    if std::os::unix::process::parent_id() != owner {
        bail!("the owner ({owner}) exited before the agent started");
    }
    send(&channel, UNSHARED).context("telling the owner the namespaces exist")?;
    match receive(&channel).context("waiting for the owner to write the id maps")? {
        Some((message, _)) if message == MAPPED => {}
        Some((message, _)) => bail!(
            "the owner sent {:?} instead of the id maps",
            String::from_utf8_lossy(&message)
        ),
        None => return Ok(()),
    }
    // SAFETY: geteuid takes no arguments.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        bail!("the id maps leave the agent as uid {euid} in its namespace, not 0");
    }
    send(&channel, READY).context("telling the owner the agent is ready")?;
    serve(&channel)
}

/// The owner's end of an agent's channel. Requests are serialized: the agent answers one
/// at a time, so a second caller waits for the first reply.
pub struct Client {
    channel: tokio::io::unix::AsyncFd<UnixStream>,
    turn: tokio::sync::Mutex<()>,
}

impl Client {
    /// Wrap the owner's end. Must be called inside a tokio runtime.
    pub fn new(channel: UnixStream) -> std::io::Result<Self> {
        channel.set_nonblocking(true)?;
        // SAFETY: the `AsyncFd` owns the stream, which closes only on drop, and nothing
        // replaces it through `get_mut`.
        let channel =
            unsafe { tokio::io::unix::AsyncFd::register(channel) }.map_err(std::io::Error::from)?;
        Ok(Self {
            channel,
            turn: tokio::sync::Mutex::new(()),
        })
    }

    async fn send(&self, message: &[u8]) -> std::io::Result<()> {
        loop {
            let mut guard = self.channel.writable().await?;
            match guard.try_io(|channel| send(channel.get_ref(), message)) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// The agent's next message. End-of-file is an error here: the agent is gone.
    async fn receive(&self) -> std::io::Result<(Vec<u8>, Option<OwnedFd>)> {
        loop {
            let mut guard = self.channel.readable().await?;
            match guard.try_io(|channel| receive(channel.get_ref())) {
                Ok(result) => {
                    return result?.ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "the namespace agent closed its channel",
                        )
                    })
                }
                Err(_would_block) => continue,
            }
        }
    }

    /// Wait for the agent to say its namespaces exist.
    pub async fn unshared(&self) -> Result<()> {
        self.expect(UNSHARED).await
    }

    /// Tell the agent its id maps are written and wait for it to confirm it is ready.
    pub async fn mapped(&self) -> Result<()> {
        self.send(MAPPED).await.context("sending 'mapped'")?;
        self.expect(READY).await
    }

    async fn expect(&self, word: &[u8]) -> Result<()> {
        let (message, _) = self.receive().await.with_context(|| {
            format!(
                "waiting for the agent's '{}'",
                String::from_utf8_lossy(word)
            )
        })?;
        if message != word {
            bail!(
                "the namespace agent said {:?} instead of '{}'",
                String::from_utf8_lossy(&message),
                String::from_utf8_lossy(word)
            );
        }
        Ok(())
    }

    /// An unconnected TCP socket made inside the agent's network namespace, for `address`'s
    /// family.
    pub async fn tcp_socket(&self, address: &std::net::IpAddr) -> Result<tokio::net::TcpSocket> {
        let request = if address.is_ipv4() { TCP4 } else { TCP6 };
        let _turn = self.turn.lock().await;
        self.send(request).await.context("asking for a socket")?;
        let (message, socket) = self.receive().await.context("waiting for a socket")?;
        if message != OK {
            bail!(
                "the namespace agent refused a socket: {}",
                String::from_utf8_lossy(&message)
            );
        }
        let socket = socket.context("the namespace agent's reply carried no socket")?;
        // SAFETY: the descriptor is a TCP socket this process now owns alone.
        Ok(unsafe {
            tokio::net::TcpSocket::from_raw_fd(std::os::fd::IntoRawFd::into_raw_fd(socket))
        })
    }
}

/// The channels this process holds to the agents it started, by agent PID. The agent's PID
/// is what a VM's state records (`holder_pid`), so a task that knows only the state finds the
/// channel here. An entry goes when the holder's owner reaps it ([`forget`]); one whose owner
/// reaped it without saying so (a failed setup) is pruned by the next [`register`], as
/// `companion`'s captures are.
static CLIENTS: std::sync::Mutex<Vec<(u32, std::sync::Arc<Client>)>> =
    std::sync::Mutex::new(Vec::new());

/// Record the channel to the agent `pid`.
pub fn register(pid: u32, client: std::sync::Arc<Client>) {
    let mut clients = CLIENTS.lock().unwrap();
    // A reaped pid has no /proc entry; an unreaped one, zombie included, does.
    clients.retain(|(agent, _)| {
        *agent != pid && std::path::Path::new(&format!("/proc/{agent}")).exists()
    });
    clients.push((pid, client));
}

/// The channel to the agent `pid`, when this process started it and has not forgotten it.
pub fn client(pid: u32) -> Option<std::sync::Arc<Client>> {
    let clients = CLIENTS.lock().unwrap();
    clients
        .iter()
        .find(|(agent, _)| *agent == pid)
        .map(|(_, client)| client.clone())
}

/// Drop the channel to the agent `pid`. Once every clone of it is gone the agent reads
/// end-of-file and exits.
pub fn forget(pid: u32) {
    CLIENTS.lock().unwrap().retain(|(agent, _)| *agent != pid);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The serve loop on a thread, in this process's own namespaces: the protocol without
    /// the unshare.
    fn serving() -> (Client, std::thread::JoinHandle<Result<()>>) {
        let (owner, agent) = channel().unwrap();
        let thread = std::thread::spawn(move || serve(&agent));
        (Client::new(owner).unwrap(), thread)
    }

    #[tokio::test]
    async fn a_socket_from_the_agent_connects_like_any_other() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, thread) = serving();
        let socket = client.tcp_socket(&address.ip()).await.unwrap();
        let (connected, accepted) = tokio::join!(socket.connect(address), listener.accept());
        assert_eq!(
            connected.unwrap().local_addr().unwrap(),
            accepted.unwrap().1
        );
        drop(client);
        thread.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn the_agent_stops_when_its_owner_closes_the_channel() {
        let (client, thread) = serving();
        drop(client);
        thread.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_unknown_request_is_refused_and_the_agent_keeps_serving() {
        let (client, thread) = serving();
        client.send(b"reboot").await.unwrap();
        let (reply, socket) = client.receive().await.unwrap();
        assert!(reply.starts_with(b"err unknown request"), "{reply:?}");
        assert!(socket.is_none());
        let address: std::net::IpAddr = "::1".parse().unwrap();
        client.tcp_socket(&address).await.unwrap();
        drop(client);
        thread.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn concurrent_requests_each_get_their_own_socket() {
        let (client, thread) = serving();
        let client = std::sync::Arc::new(client);
        let address: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let client = client.clone();
            tasks.spawn(async move { client.tcp_socket(&address).await });
        }
        // Held until the end: a closed descriptor's number is free to be handed out again.
        let mut sockets = Vec::new();
        while let Some(result) = tasks.join_next().await {
            sockets.push(result.unwrap().unwrap());
        }
        let fds: std::collections::BTreeSet<RawFd> =
            sockets.iter().map(|socket| socket.as_raw_fd()).collect();
        assert_eq!(fds.len(), 32, "a descriptor was handed out twice");
        drop(client);
        thread.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_dead_agent_is_an_error_not_a_hang() {
        let (owner, agent) = channel().unwrap();
        drop(agent);
        let client = Client::new(owner).unwrap();
        let address: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let error = client.tcp_socket(&address).await.unwrap_err();
        assert!(format!("{error:#}").contains("socket"), "{error:#}");
    }
}
