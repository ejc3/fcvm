use std::io::{BufRead, BufReader};
use std::os::fd::IntoRawFd;

use crate::types::{ExecRequest, ExecResponse};
use crate::vsock;

/// Run the exec server. Sends ready signal when listening.
pub async fn run_server(ready_tx: tokio::sync::oneshot::Sender<()>) {
    eprintln!(
        "[fc-agent] starting exec server on vsock port {}",
        vsock::EXEC_PORT
    );

    let listener = match vsock::VsockListener::bind(vsock::EXEC_PORT) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[fc-agent] ERROR: failed to bind exec server: {}", e);
            return;
        }
    };

    eprintln!(
        "[fc-agent] exec server listening on vsock port {}",
        vsock::EXEC_PORT
    );

    tokio::task::yield_now().await;
    let _ = ready_tx.send(());

    loop {
        match listener.accept().await {
            Ok(client_fd) => {
                // Consume OwnedFd into raw fd so handle_connection owns the fd.
                // The TTY path closes it via File::from_raw_fd, and the non-TTY
                // path closes it explicitly with libc::close.
                let raw_fd = client_fd.into_raw_fd();
                tokio::task::spawn_blocking(move || {
                    handle_connection(raw_fd);
                });
            }
            Err(e) => {
                eprintln!("[fc-agent] exec server accept error: {}", e);
            }
        }
    }
}

fn write_line_to_fd(fd: i32, data: &str) {
    let bytes = format!("{}\n", data);
    let mut written = 0;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
            )
        };
        if n <= 0 {
            break;
        }
        written += n as usize;
    }
}

fn handle_connection(fd: i32) {
    const MAX_EXEC_LINE_LENGTH: usize = 1_048_576;
    let mut line = String::new();
    let mut buf = [0u8; 1];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n <= 0 {
            unsafe { libc::close(fd) };
            return;
        }
        if buf[0] == b'\n' {
            break;
        }
        if line.len() >= MAX_EXEC_LINE_LENGTH {
            eprintln!(
                "[fc-agent] exec request line exceeds {} bytes, rejecting",
                MAX_EXEC_LINE_LENGTH
            );
            unsafe { libc::close(fd) };
            return;
        }
        line.push(buf[0] as char);
    }

    let request: ExecRequest = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let response = ExecResponse::Error(format!("Invalid request: {}", e));
            write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());
            unsafe { libc::close(fd) };
            return;
        }
    };

    if request.command.is_empty() {
        let response = ExecResponse::Error("Empty command".to_string());
        write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());
        unsafe { libc::close(fd) };
        return;
    }

    // TTY path: run_with_pty_fd takes ownership of fd via File::from_raw_fd
    if request.tty || request.interactive {
        let command = if request.in_container {
            let mut cmd = vec!["podman".to_string(), "exec".to_string()];
            if request.interactive {
                cmd.push("-i".to_string());
            }
            if request.tty {
                cmd.push("-t".to_string());
            }
            for (key, value) in crate::system::read_proxy_settings() {
                cmd.push("-e".to_string());
                cmd.push(format!("{}={}", key, value));
            }
            cmd.push("--latest".to_string());
            cmd.extend(request.command.iter().cloned());
            cmd
        } else {
            request.command.clone()
        };

        let _exit_code =
            crate::tty::run_with_pty_fd(fd, &command, request.tty, request.interactive);
    } else {
        handle_pipe(fd, &request);
    }
}

fn handle_pipe(fd: i32, request: &ExecRequest) {
    let proxy_settings = crate::system::read_proxy_settings();

    let mut cmd = if request.in_container {
        let mut cmd = std::process::Command::new("podman");
        cmd.arg("exec");
        if request.interactive {
            cmd.arg("-i");
        }
        for (key, value) in &proxy_settings {
            cmd.arg("-e").arg(format!("{}={}", key, value));
        }
        cmd.arg("--latest");
        cmd.args(&request.command);
        cmd
    } else {
        let mut cmd = std::process::Command::new(&request.command[0]);
        cmd.args(&request.command[1..]);
        for (key, value) in &proxy_settings {
            cmd.env(key, value);
        }
        cmd
    };

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if request.interactive {
        cmd.stdin(std::process::Stdio::piped());
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let response = ExecResponse::Error(format!("Failed to spawn: {}", e));
            write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());
            unsafe { libc::close(fd) };
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Mutex protects the fd so stdout/stderr threads don't interleave writes
    let fd_mu = std::sync::Arc::new(std::sync::Mutex::new(fd));

    let fd_stdout = fd_mu.clone();
    let stdout_thread = stdout.map(|stdout| {
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let response = ExecResponse::Stdout(format!("{}\n", line));
                let fd = fd_stdout.lock().unwrap();
                write_line_to_fd(*fd, &serde_json::to_string(&response).unwrap());
            }
        })
    });

    let fd_stderr = fd_mu.clone();
    let stderr_thread = stderr.map(|stderr| {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let response = ExecResponse::Stderr(format!("{}\n", line));
                let fd = fd_stderr.lock().unwrap();
                write_line_to_fd(*fd, &serde_json::to_string(&response).unwrap());
            }
        })
    });

    let exit_status = child.wait();

    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    let exit_code = match exit_status {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            let response = ExecResponse::Error(format!("Wait failed: {}", e));
            write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());
            1
        }
    };

    let response = ExecResponse::Exit(exit_code);
    write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());

    // Close the fd — in non-TTY mode we own it (TTY mode transfers
    // ownership to File::from_raw_fd inside run_with_pty_fd).
    unsafe { libc::close(fd) };
}
