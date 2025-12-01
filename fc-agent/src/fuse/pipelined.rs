//! Pipelined FUSE filesystem with async vsock I/O.
//!
//! Architecture for multi-reader support:
//! 1. Multiple reader threads read FUSE requests from cloned /dev/fuse fds
//! 2. Each reader sends request to shared I/O thread and blocks waiting for response
//! 3. Single I/O thread handles all vsock communication (pipelining)
//! 4. I/O thread routes responses back to waiting reader threads
//! 5. Each reader completes its own Reply on its own thread
//!
//! This ensures responses are written from the same thread context that received
//! the request, which the FUSE kernel requires for FUSE_DEV_IOC_CLONE multi-reader.

use crate::fuse::protocol::{self, VolumeRequest, VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE};
use fuser::{
    FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use metrics::{counter, gauge, histogram};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender, Receiver};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Host CID for vsock (always 2)
const HOST_CID: u32 = 2;

/// A request sent to the I/O thread, with a channel to receive the response
struct PendingRequest {
    wire_req: WireRequest,
    response_tx: Sender<VolumeResponse>,
    sent_at: Instant,
    op_name: String,
}

/// Shared inner state for multi-reader FUSE filesystem.
struct PipelinedFsInner {
    /// Channel to send requests to the I/O thread
    request_tx: Sender<PendingRequest>,
    /// Next unique ID for request/response matching
    next_id: AtomicU64,
    /// Write end of notification pipe (wakes up I/O thread)
    notify_fd: RawFd,
}

impl Drop for PipelinedFsInner {
    fn drop(&mut self) {
        unsafe { libc::close(self.notify_fd); }
    }
}

/// Shareable FUSE filesystem for multi-reader support.
/// Each clone shares the same vsock connection but sends requests synchronously.
#[derive(Clone)]
pub struct SharedFs {
    inner: Arc<PipelinedFsInner>,
}

impl SharedFs {
    /// Create a new shared filesystem connected to the given vsock port.
    pub fn new_vsock(port: u32) -> anyhow::Result<Self> {
        eprintln!("[fc-agent] creating shared pipelined FUSE connection to port {}", port);

        let stream = connect_vsock(HOST_CID, port)?;

        // Create notification pipe
        let mut pipe_fds: [libc::c_int; 2] = [0; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
            anyhow::bail!("Failed to create pipe: {}", std::io::Error::last_os_error());
        }
        let pipe_read_fd = pipe_fds[0];
        let pipe_write_fd = pipe_fds[1];

        unsafe {
            let flags = libc::fcntl(pipe_read_fd, libc::F_GETFL);
            libc::fcntl(pipe_read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let (request_tx, request_rx) = mpsc::channel::<PendingRequest>();

        thread::spawn(move || {
            if let Err(e) = vsock_io_thread(stream, request_rx, pipe_read_fd) {
                eprintln!("[fc-agent] vsock I/O thread error: {}", e);
            }
            unsafe { libc::close(pipe_read_fd); }
        });

        eprintln!("[fc-agent] shared pipelined FUSE ready on port {}", port);

        Ok(Self {
            inner: Arc::new(PipelinedFsInner {
                request_tx,
                next_id: AtomicU64::new(1),
                notify_fd: pipe_write_fd,
            }),
        })
    }

    fn next_unique(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a request and block until response arrives.
    /// This is called from reader threads and blocks until the I/O thread
    /// sends back the response. The reader thread then completes its own Reply.
    fn send_request_sync(&self, request: VolumeRequest) -> VolumeResponse {
        let unique = self.next_unique();
        let sent_at = Instant::now();

        // Extract operation name for metrics
        let op_name = match &request {
            VolumeRequest::Lookup { .. } => "lookup",
            VolumeRequest::Getattr { .. } => "getattr",
            VolumeRequest::Setattr { .. } => "setattr",
            VolumeRequest::Mkdir { .. } => "mkdir",
            VolumeRequest::Rmdir { .. } => "rmdir",
            VolumeRequest::Create { .. } => "create",
            VolumeRequest::Open { .. } => "open",
            VolumeRequest::Read { .. } => "read",
            VolumeRequest::Write { .. } => "write",
            VolumeRequest::Release { .. } => "release",
            VolumeRequest::Flush { .. } => "flush",
            VolumeRequest::Fsync { .. } => "fsync",
            VolumeRequest::Readdir { .. } => "readdir",
            VolumeRequest::Unlink { .. } => "unlink",
            VolumeRequest::Rename { .. } => "rename",
            VolumeRequest::Symlink { .. } => "symlink",
            VolumeRequest::Readlink { .. } => "readlink",
            VolumeRequest::Link { .. } => "link",
            VolumeRequest::Access { .. } => "access",
            VolumeRequest::Statfs { .. } => "statfs",
        };

        let wire_req = WireRequest { unique, request };

        // Create oneshot channel for response
        let (response_tx, response_rx) = mpsc::channel();

        let pending_req = PendingRequest {
            wire_req,
            response_tx,
            sent_at,
            op_name: op_name.to_string(),
        };

        // Send to I/O thread
        if self.inner.request_tx.send(pending_req).is_err() {
            counter!("fuse.errors", "reason" => "send_failed").increment(1);
            return VolumeResponse::Error { errno: libc::EIO };
        }

        // Notify I/O thread
        unsafe {
            libc::write(self.inner.notify_fd, [1u8].as_ptr() as *const libc::c_void, 1);
        }

        // Block waiting for response (this is the key difference from async approach)
        match response_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(response) => {
                let latency_us = sent_at.elapsed().as_micros() as u64;
                histogram!("fuse.request.latency_us", "op" => op_name.to_string()).record(latency_us as f64);

                if matches!(&response, VolumeResponse::Error { .. }) {
                    counter!("fuse.errors", "reason" => "response_error").increment(1);
                }
                response
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                counter!("fuse.errors", "reason" => "timeout").increment(1);
                VolumeResponse::Error { errno: libc::EIO }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                counter!("fuse.errors", "reason" => "disconnected").increment(1);
                VolumeResponse::Error { errno: libc::EIO }
            }
        }
    }
}

/// Connect to vsock using raw socket API.
fn connect_vsock(cid: u32, port: u32) -> anyhow::Result<UnixStream> {
    use std::os::unix::io::FromRawFd;

    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        anyhow::bail!("Failed to create vsock socket: {}", std::io::Error::last_os_error());
    }

    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0u8; 4],
    };

    let result = unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as u32,
        )
    };

    if result < 0 {
        unsafe { libc::close(fd) };
        anyhow::bail!("Failed to connect vsock to CID {} port {}: {}", cid, port, std::io::Error::last_os_error());
    }

    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Metadata stored per pending request for metrics
struct PendingMeta {
    response_tx: Sender<VolumeResponse>,
    sent_at: Instant,
    op_name: String,
}

/// Background thread that handles all vsock I/O.
/// Receives requests from reader threads, sends to host, routes responses back.
///
/// Uses blocking I/O with poll() for multiplexing request channel and socket.
/// This is simpler and more reliable than mixed blocking/non-blocking.
fn vsock_io_thread(
    mut stream: UnixStream,
    request_rx: Receiver<PendingRequest>,
    pipe_read_fd: RawFd,
) -> anyhow::Result<()> {
    eprintln!("[fc-agent] vsock I/O thread started (blocking I/O)");

    // Keep socket in BLOCKING mode always - simpler and more reliable
    stream.set_nonblocking(false)?;

    // Set read timeout to avoid infinite blocking (allows checking for new requests)
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;

    // Map of unique ID -> response channel + metadata
    let mut pending: HashMap<u64, PendingMeta> = HashMap::new();
    let mut max_pending: usize = 0;

    let mut len_buf = [0u8; 4];
    let mut drain_buf = [0u8; 64];

    loop {
        // First, drain any pending requests and send them
        // Drain notification pipe
        while unsafe { libc::read(pipe_read_fd, drain_buf.as_mut_ptr() as *mut libc::c_void, drain_buf.len()) } > 0 {}

        // Process all queued requests
        loop {
            match request_rx.try_recv() {
                Ok(pending_req) => {
                    let unique = pending_req.wire_req.unique;
                    let op_name = pending_req.op_name.clone();

                    // Store response channel and metadata
                    pending.insert(unique, PendingMeta {
                        response_tx: pending_req.response_tx,
                        sent_at: pending_req.sent_at,
                        op_name: pending_req.op_name,
                    });

                    // Update metrics
                    counter!("fuse.requests.total").increment(1);
                    counter!("fuse.requests", "op" => op_name).increment(1);
                    gauge!("fuse.pending.current").set(pending.len() as f64);

                    if pending.len() > max_pending {
                        max_pending = pending.len();
                        gauge!("fuse.pending.max").set(max_pending as f64);
                    }

                    // Send request to host (blocking write)
                    let req_buf = bincode::serialize(&pending_req.wire_req)?;
                    let req_len = (req_buf.len() as u32).to_be_bytes();

                    stream.write_all(&req_len)?;
                    stream.write_all(&req_buf)?;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    eprintln!("[fc-agent] request channel closed, I/O thread exiting");
                    return Ok(());
                }
            }
        }

        // If we have pending requests, try to read responses
        if !pending.is_empty() {
            // Try to read response length (with timeout)
            match stream.read_exact(&mut len_buf) {
                Ok(_) => {
                    let len = u32::from_be_bytes(len_buf) as usize;
                    if len > MAX_MESSAGE_SIZE {
                        eprintln!("[fc-agent] response too large: {} bytes", len);
                        continue;
                    }

                    // Read response body (blocking)
                    let mut resp_buf = vec![0u8; len];
                    stream.read_exact(&mut resp_buf)?;

                    let wire_resp: WireResponse = bincode::deserialize(&resp_buf)?;

                    counter!("fuse.responses.total").increment(1);

                    // Route response to waiting reader thread
                    if let Some(meta) = pending.remove(&wire_resp.unique) {
                        // Record server-side latency (time from send to receive in I/O thread)
                        let server_latency_us = meta.sent_at.elapsed().as_micros() as f64;
                        histogram!("fuse.io_thread.latency_us", "op" => meta.op_name).record(server_latency_us);

                        gauge!("fuse.pending.current").set(pending.len() as f64);
                        let _ = meta.response_tx.send(wire_resp.response);
                    } else {
                        counter!("fuse.errors", "reason" => "orphan_response").increment(1);
                        eprintln!("[fc-agent] WARNING: no pending request for ID {}", wire_resp.unique);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                    // Timeout - go back to check for new requests
                    continue;
                }
                Err(e) => {
                    eprintln!("[fc-agent] vsock read error: {}", e);
                    return Err(e.into());
                }
            }
        } else {
            // No pending requests - wait for new ones using poll
            let mut pollfds = [
                libc::pollfd { fd: pipe_read_fd, events: libc::POLLIN, revents: 0 },
            ];

            let poll_result = unsafe {
                libc::poll(pollfds.as_mut_ptr(), 1, 100) // 100ms timeout
            };

            if poll_result < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                anyhow::bail!("poll() failed: {}", err);
            }
            // Loop back to check for requests
        }
    }
}

// ============================================================================
// Helper functions for converting responses
// ============================================================================

fn protocol_file_type_to_fuser(ft: u8) -> FileType {
    match ft {
        protocol::file_type::DIR => FileType::Directory,
        protocol::file_type::REG => FileType::RegularFile,
        protocol::file_type::LNK => FileType::Symlink,
        protocol::file_type::CHR => FileType::CharDevice,
        protocol::file_type::BLK => FileType::BlockDevice,
        protocol::file_type::FIFO => FileType::NamedPipe,
        protocol::file_type::SOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

fn to_fuser_attr(attr: &protocol::FileAttr) -> fuser::FileAttr {
    fuser::FileAttr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.blocks,
        atime: UNIX_EPOCH + Duration::new(attr.atime_secs as u64, attr.atime_nsecs),
        mtime: UNIX_EPOCH + Duration::new(attr.mtime_secs as u64, attr.mtime_nsecs),
        ctime: UNIX_EPOCH + Duration::new(attr.ctime_secs as u64, attr.ctime_nsecs),
        crtime: UNIX_EPOCH,
        kind: mode_to_file_type(attr.mode),
        perm: (attr.mode & 0o7777) as u16,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: attr.rdev,
        blksize: attr.blksize,
        flags: 0,
    }
}

fn mode_to_file_type(mode: u32) -> FileType {
    match mode & 0o170000 {
        0o140000 => FileType::Socket,
        0o120000 => FileType::Symlink,
        0o100000 => FileType::RegularFile,
        0o060000 => FileType::BlockDevice,
        0o040000 => FileType::Directory,
        0o020000 => FileType::CharDevice,
        0o010000 => FileType::NamedPipe,
        _ => FileType::RegularFile,
    }
}

// ============================================================================
// SharedFs Filesystem implementation
// Each method sends request, blocks for response, completes reply on same thread
// ============================================================================

impl Filesystem for SharedFs {
    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let response = self.send_request_sync(VolumeRequest::Lookup {
            parent,
            name: name.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry { attr, generation, ttl_secs } => {
                reply.entry(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr), generation);
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let response = self.send_request_sync(VolumeRequest::Getattr { ino });

        match response {
            VolumeResponse::Attr { attr, ttl_secs } => {
                reply.attr(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr));
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn setattr(
        &mut self,
        req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let atime_secs = atime.map(|t| match t {
            TimeOrNow::SpecificTime(st) => st.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
            TimeOrNow::Now => std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
        });
        let mtime_secs = mtime.map(|t| match t {
            TimeOrNow::SpecificTime(st) => st.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
            TimeOrNow::Now => std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
        });

        let response = self.send_request_sync(VolumeRequest::Setattr {
            ino, mode, uid, gid, size,
            atime_secs, atime_nsecs: None,
            mtime_secs, mtime_nsecs: None,
            caller_uid: req.uid(),
            caller_gid: req.gid(),
            caller_pid: req.pid(),
        });

        match response {
            VolumeResponse::Attr { attr, ttl_secs } => {
                reply.attr(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr));
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn readdir(&mut self, req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let response = self.send_request_sync(VolumeRequest::Readdir {
            ino,
            offset: offset as u64,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::DirEntries { entries } => {
                for (i, entry) in entries.iter().enumerate() {
                    let file_type = protocol_file_type_to_fuser(entry.file_type);
                    if reply.add(entry.ino, (i + 1) as i64, file_type, &entry.name) {
                        break;
                    }
                }
                reply.ok();
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn mkdir(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
        let response = self.send_request_sync(VolumeRequest::Mkdir {
            parent,
            name: name.to_string_lossy().to_string(),
            mode,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry { attr, generation, ttl_secs } => {
                reply.entry(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr), generation);
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn rmdir(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Rmdir {
            parent,
            name: name.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn create(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, flags: i32, reply: ReplyCreate) {
        let response = self.send_request_sync(VolumeRequest::Create {
            parent,
            name: name.to_string_lossy().to_string(),
            mode,
            flags: flags as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Created { attr, generation, ttl_secs, fh, flags } => {
                reply.created(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr), generation, fh, flags);
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn open(&mut self, req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let response = self.send_request_sync(VolumeRequest::Open {
            ino,
            flags: flags as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Opened { fh, flags } => reply.opened(fh, flags),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn read(&mut self, _req: &Request, ino: u64, fh: u64, offset: i64, size: u32, _flags: i32, _lock_owner: Option<u64>, reply: ReplyData) {
        let response = self.send_request_sync(VolumeRequest::Read {
            ino, fh, offset: offset as u64, size,
        });

        match response {
            VolumeResponse::Data { data } => reply.data(&data),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn write(&mut self, _req: &Request, ino: u64, fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock_owner: Option<u64>, reply: ReplyWrite) {
        let response = self.send_request_sync(VolumeRequest::Write {
            ino, fh, offset: offset as u64, data: data.to_vec(),
        });

        match response {
            VolumeResponse::Written { size } => reply.written(size),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn release(&mut self, _req: &Request, ino: u64, fh: u64, _flags: i32, _lock_owner: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Release { ino, fh });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn flush(&mut self, _req: &Request, ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Flush { ino, fh });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn fsync(&mut self, _req: &Request, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Fsync { ino, fh, datasync });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn unlink(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Unlink {
            parent,
            name: name.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn rename(&mut self, req: &Request, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Rename {
            parent,
            name: name.to_string_lossy().to_string(),
            newparent,
            newname: newname.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn symlink(&mut self, req: &Request, parent: u64, link_name: &OsStr, target: &std::path::Path, reply: ReplyEntry) {
        let response = self.send_request_sync(VolumeRequest::Symlink {
            parent,
            name: link_name.to_string_lossy().to_string(),
            target: target.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry { attr, generation, ttl_secs } => {
                reply.entry(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr), generation);
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: fuser::ReplyData) {
        let response = self.send_request_sync(VolumeRequest::Readlink { ino });

        match response {
            VolumeResponse::Symlink { target } => reply.data(target.as_bytes()),
            VolumeResponse::Data { data } => reply.data(&data),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn link(&mut self, req: &Request, ino: u64, newparent: u64, newname: &OsStr, reply: ReplyEntry) {
        let response = self.send_request_sync(VolumeRequest::Link {
            ino,
            newparent,
            newname: newname.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry { attr, generation, ttl_secs } => {
                reply.entry(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr), generation);
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn access(&mut self, req: &Request, ino: u64, mask: i32, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Access {
            ino,
            mask: mask as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn statfs(&mut self, _req: &Request, ino: u64, reply: ReplyStatfs) {
        let response = self.send_request_sync(VolumeRequest::Statfs { ino });

        match response {
            VolumeResponse::Statfs { blocks, bfree, bavail, files, ffree, bsize, namelen, frsize } => {
                reply.statfs(blocks, bfree, bavail, files, ffree, bsize, namelen, frsize);
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }
}

// ============================================================================
// PipelinedFs - single-reader version (kept for compatibility)
// ============================================================================

/// Single-reader pipelined filesystem (legacy compatibility).
pub struct PipelinedFs {
    inner: SharedFs,
}

impl PipelinedFs {
    pub fn new_vsock(port: u32) -> anyhow::Result<Self> {
        Ok(Self {
            inner: SharedFs::new_vsock(port)?,
        })
    }
}

impl Filesystem for PipelinedFs {
    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        self.inner.lookup(req, parent, name, reply)
    }

    fn getattr(&mut self, req: &Request, ino: u64, fh: Option<u64>, reply: ReplyAttr) {
        self.inner.getattr(req, ino, fh, reply)
    }

    fn setattr(&mut self, req: &Request, ino: u64, mode: Option<u32>, uid: Option<u32>, gid: Option<u32>, size: Option<u64>, atime: Option<TimeOrNow>, mtime: Option<TimeOrNow>, ctime: Option<std::time::SystemTime>, fh: Option<u64>, crtime: Option<std::time::SystemTime>, chgtime: Option<std::time::SystemTime>, bkuptime: Option<std::time::SystemTime>, flags: Option<u32>, reply: ReplyAttr) {
        self.inner.setattr(req, ino, mode, uid, gid, size, atime, mtime, ctime, fh, crtime, chgtime, bkuptime, flags, reply)
    }

    fn readdir(&mut self, req: &Request, ino: u64, fh: u64, offset: i64, reply: ReplyDirectory) {
        self.inner.readdir(req, ino, fh, offset, reply)
    }

    fn mkdir(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        self.inner.mkdir(req, parent, name, mode, umask, reply)
    }

    fn rmdir(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.inner.rmdir(req, parent, name, reply)
    }

    fn create(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, umask: u32, flags: i32, reply: ReplyCreate) {
        self.inner.create(req, parent, name, mode, umask, flags, reply)
    }

    fn open(&mut self, req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        self.inner.open(req, ino, flags, reply)
    }

    fn read(&mut self, req: &Request, ino: u64, fh: u64, offset: i64, size: u32, flags: i32, lock_owner: Option<u64>, reply: ReplyData) {
        self.inner.read(req, ino, fh, offset, size, flags, lock_owner, reply)
    }

    fn write(&mut self, req: &Request, ino: u64, fh: u64, offset: i64, data: &[u8], write_flags: u32, flags: i32, lock_owner: Option<u64>, reply: ReplyWrite) {
        self.inner.write(req, ino, fh, offset, data, write_flags, flags, lock_owner, reply)
    }

    fn release(&mut self, req: &Request, ino: u64, fh: u64, flags: i32, lock_owner: Option<u64>, flush: bool, reply: ReplyEmpty) {
        self.inner.release(req, ino, fh, flags, lock_owner, flush, reply)
    }

    fn flush(&mut self, req: &Request, ino: u64, fh: u64, lock_owner: u64, reply: ReplyEmpty) {
        self.inner.flush(req, ino, fh, lock_owner, reply)
    }

    fn fsync(&mut self, req: &Request, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        self.inner.fsync(req, ino, fh, datasync, reply)
    }

    fn unlink(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.inner.unlink(req, parent, name, reply)
    }

    fn rename(&mut self, req: &Request, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, flags: u32, reply: ReplyEmpty) {
        self.inner.rename(req, parent, name, newparent, newname, flags, reply)
    }

    fn symlink(&mut self, req: &Request, parent: u64, link_name: &OsStr, target: &std::path::Path, reply: ReplyEntry) {
        self.inner.symlink(req, parent, link_name, target, reply)
    }

    fn readlink(&mut self, req: &Request, ino: u64, reply: fuser::ReplyData) {
        self.inner.readlink(req, ino, reply)
    }

    fn link(&mut self, req: &Request, ino: u64, newparent: u64, newname: &OsStr, reply: ReplyEntry) {
        self.inner.link(req, ino, newparent, newname, reply)
    }

    fn access(&mut self, req: &Request, ino: u64, mask: i32, reply: ReplyEmpty) {
        self.inner.access(req, ino, mask, reply)
    }

    fn statfs(&mut self, req: &Request, ino: u64, reply: ReplyStatfs) {
        self.inner.statfs(req, ino, reply)
    }
}
