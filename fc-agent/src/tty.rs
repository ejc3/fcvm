//! Unified TTY handling for fc-agent.
//!
//! This module provides a single implementation for running commands with PTY support,
//! used by both `podman run -it` and `exec -it` paths.

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;

/// Vsock port for TTY I/O (used by podman run -it)
pub const TTY_VSOCK_PORT: u32 = 4996;

/// Host CID for vsock connections
const HOST_CID: u32 = 2;

/// Run a command with PTY, connecting to host first.
///
/// Used by `podman run -it` where fc-agent initiates the connection.
pub fn run_with_pty(command: &[String], tty: bool, interactive: bool) -> i32 {
    if command.is_empty() {
        eprintln!("[fc-agent] tty: empty command");
        return 1;
    }

    // Connect to host via vsock
    let vsock_fd = match connect_vsock(TTY_VSOCK_PORT) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("[fc-agent] tty: failed to connect vsock: {}", e);
            return 1;
        }
    };

    run_with_pty_fd(vsock_fd, command, tty, interactive)
}

/// Run a command with PTY using a pre-connected fd.
///
/// Used by `exec -it` where the host has already connected to fc-agent.
/// Also called by `run_with_pty` after connecting.
pub fn run_with_pty_fd(vsock_fd: i32, command: &[String], tty: bool, interactive: bool) -> i32 {
    // Allocate PTY or pipes
    let (master_fd, slave_fd, stdin_read, stdin_write, stdout_read, stdout_write) = if tty {
        match allocate_pty() {
            Ok((m, s)) => (m, s, -1, -1, -1, -1),
            Err(e) => {
                eprintln!("[fc-agent] tty: failed to allocate PTY: {}", e);
                unsafe { libc::close(vsock_fd) };
                return 1;
            }
        }
    } else {
        match allocate_pipes() {
            Ok((sr, sw, or, ow)) => (-1, -1, sr, sw, or, ow),
            Err(e) => {
                eprintln!("[fc-agent] tty: failed to allocate pipes: {}", e);
                unsafe { libc::close(vsock_fd) };
                return 1;
            }
        }
    };

    // Fork
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("[fc-agent] tty: fork failed");
        cleanup_fds(
            tty,
            master_fd,
            slave_fd,
            stdin_read,
            stdin_write,
            stdout_read,
            stdout_write,
        );
        unsafe { libc::close(vsock_fd) };
        return 1;
    }

    if pid == 0 {
        // Child process
        unsafe { libc::close(vsock_fd) };

        if tty {
            setup_child_pty(slave_fd, master_fd);
        } else {
            setup_child_pipes(stdin_read, stdin_write, stdout_read, stdout_write);
        }

        exec_command(command);
        // exec_command never returns on success
        eprintln!("[fc-agent] tty: exec failed");
        unsafe { libc::_exit(127) };
    }

    // Parent process
    if tty {
        unsafe { libc::close(slave_fd) };
    } else {
        unsafe {
            libc::close(stdin_read);
            libc::close(stdout_write);
        }
    }

    // Create File wrappers for I/O
    let mut vsock = unsafe { std::fs::File::from_raw_fd(vsock_fd) };

    let output_fd = if tty { master_fd } else { stdout_read };

    // Spawn writer thread (only if interactive)
    let writer_thread = if interactive {
        let vsock_clone = match vsock.try_clone() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[fc-agent] tty: failed to clone vsock: {}", e);
                // Clean up FDs before returning
                if tty {
                    unsafe { libc::close(master_fd) };
                } else {
                    unsafe {
                        libc::close(stdout_read);
                        libc::close(stdin_write);
                    }
                }
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
                return 1;
            }
        };

        // For TTY, duplicate the master fd so we can use it for writing
        // while the main thread uses it for reading
        let input_file = if tty {
            let dup_fd = unsafe { libc::dup(master_fd) };
            if dup_fd < 0 {
                eprintln!("[fc-agent] tty: failed to dup master fd");
                // Clean up master_fd before returning
                unsafe { libc::close(master_fd) };
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
                return 1;
            }
            unsafe { std::fs::File::from_raw_fd(dup_fd) }
        } else {
            unsafe { std::fs::File::from_raw_fd(stdin_write) }
        };

        Some(std::thread::spawn(move || {
            writer_loop(vsock_clone, input_file);
        }))
    } else {
        // Close stdin pipe if not interactive
        if !tty && stdin_write >= 0 {
            unsafe { libc::close(stdin_write) };
        }
        None
    };

    // Reader loop in main thread
    let output_file = unsafe { std::fs::File::from_raw_fd(output_fd) };
    let exit_code = reader_loop(output_file, &mut vsock, pid);

    // Send exit message and flush to ensure it's sent before we exit
    let _ = exec_proto::write_exit(&mut vsock, exit_code);
    let _ = vsock.flush();

    // Wait for writer thread
    if let Some(handle) = writer_thread {
        let _ = handle.join();
    }

    // Drop vsock explicitly to ensure close is sent before process exits
    drop(vsock);

    exit_code
}

/// Connect to host via vsock
fn connect_vsock(port: u32) -> Result<i32, String> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err("socket creation failed".to_string());
    }

    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: HOST_CID,
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
        return Err(format!(
            "connect failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    Ok(fd)
}

/// Allocate PTY pair
fn allocate_pty() -> Result<(i32, i32), String> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;

    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };

    if result != 0 {
        return Err("openpty failed".to_string());
    }

    Ok((master, slave))
}

/// Allocate stdin/stdout pipes
fn allocate_pipes() -> Result<(i32, i32, i32, i32), String> {
    let mut stdin_pipe = [0i32; 2];
    let mut stdout_pipe = [0i32; 2];

    if unsafe { libc::pipe(stdin_pipe.as_mut_ptr()) } != 0 {
        return Err("stdin pipe failed".to_string());
    }

    if unsafe { libc::pipe(stdout_pipe.as_mut_ptr()) } != 0 {
        unsafe {
            libc::close(stdin_pipe[0]);
            libc::close(stdin_pipe[1]);
        }
        return Err("stdout pipe failed".to_string());
    }

    // stdin_pipe[0] = read end (child reads from this)
    // stdin_pipe[1] = write end (parent writes to this)
    // stdout_pipe[0] = read end (parent reads from this)
    // stdout_pipe[1] = write end (child writes to this)
    Ok((stdin_pipe[0], stdin_pipe[1], stdout_pipe[0], stdout_pipe[1]))
}

/// Clean up file descriptors on error
fn cleanup_fds(
    tty: bool,
    master_fd: i32,
    slave_fd: i32,
    stdin_read: i32,
    stdin_write: i32,
    stdout_read: i32,
    stdout_write: i32,
) {
    unsafe {
        if tty {
            if master_fd >= 0 {
                libc::close(master_fd);
            }
            if slave_fd >= 0 {
                libc::close(slave_fd);
            }
        } else {
            if stdin_read >= 0 {
                libc::close(stdin_read);
            }
            if stdin_write >= 0 {
                libc::close(stdin_write);
            }
            if stdout_read >= 0 {
                libc::close(stdout_read);
            }
            if stdout_write >= 0 {
                libc::close(stdout_write);
            }
        }
    }
}

/// Set up child process for PTY
fn setup_child_pty(slave_fd: i32, master_fd: i32) {
    unsafe {
        // Create new session and set controlling terminal
        libc::setsid();
        libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);

        // Redirect stdin/stdout/stderr to PTY slave
        libc::dup2(slave_fd, 0);
        libc::dup2(slave_fd, 1);
        libc::dup2(slave_fd, 2);

        // Close original fds
        if slave_fd > 2 {
            libc::close(slave_fd);
        }
        libc::close(master_fd);
    }
}

/// Set up child process for pipes
fn setup_child_pipes(stdin_read: i32, stdin_write: i32, stdout_read: i32, stdout_write: i32) {
    unsafe {
        // Redirect stdin to read end of stdin pipe
        libc::dup2(stdin_read, 0);
        // Redirect stdout/stderr to write end of stdout pipe
        libc::dup2(stdout_write, 1);
        libc::dup2(stdout_write, 2);

        // Close original pipe fds
        libc::close(stdin_read);
        libc::close(stdin_write);
        libc::close(stdout_read);
        libc::close(stdout_write);
    }
}

/// Exec the command
fn exec_command(command: &[String]) {
    use std::ffi::CString;

    let prog = match CString::new(command[0].as_str()) {
        Ok(s) => s,
        Err(_) => return,
    };

    let args: Vec<CString> = command
        .iter()
        .filter_map(|s| CString::new(s.as_str()).ok())
        .collect();

    let arg_ptrs: Vec<*const libc::c_char> = args
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    unsafe {
        libc::execvp(prog.as_ptr(), arg_ptrs.as_ptr());
    }
}

/// Writer loop: read STDIN messages from vsock, write to PTY/pipe
fn writer_loop(mut vsock: std::fs::File, mut target: std::fs::File) {
    loop {
        match exec_proto::Message::read_from(&mut vsock) {
            Ok(exec_proto::Message::Stdin(data)) => {
                if target.write_all(&data).is_err() {
                    break;
                }
                if target.flush().is_err() {
                    break;
                }
            }
            Ok(exec_proto::Message::Exit(_)) | Ok(exec_proto::Message::Error(_)) => {
                break;
            }
            Ok(_) => {
                // Unexpected message type, ignore
            }
            Err(_) => {
                break;
            }
        }
    }
    // Drop target to close pipe/PTY, signaling EOF to child
}

/// Reader loop: read from PTY/pipe, send DATA messages via exec_proto
fn reader_loop(mut source: std::fs::File, vsock: &mut std::fs::File, child_pid: i32) -> i32 {
    let mut buf = [0u8; 4096];

    loop {
        match source.read(&mut buf) {
            Ok(0) => {
                break;
            }
            Ok(n) => {
                if exec_proto::write_data(vsock, &buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }

    // Wait for child and get exit code
    let mut status: libc::c_int = 0;
    let ret = unsafe { libc::waitpid(child_pid, &mut status, 0) };

    if ret < 0 {
        eprintln!(
            "[fc-agent] tty: waitpid failed: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }

    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}
