//! Execute commands in a running VM or its container
//!
//! Uses Firecracker's vsock to connect from host to guest.
//! The guest (fc-agent) listens on vsock port 4998.
//! The host connects via the vsock.sock Unix socket using the CONNECT protocol.

use crate::cli::ExecArgs;
use crate::paths;
use crate::state::StateManager;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use tracing::info;

/// Vsock port for exec commands (fc-agent listens on this)
pub const EXEC_VSOCK_PORT: u32 = 4998;

pub async fn cmd_exec(args: ExecArgs) -> Result<()> {
    // Find the VM by name or PID
    let state_manager = StateManager::new(paths::state_dir());
    state_manager.init().await?;

    let vm_state = if let Some(pid) = args.pid {
        // Look up by PID
        state_manager
            .load_state_by_pid(pid)
            .await
            .with_context(|| format!("No VM found with PID {}", pid))?
    } else if let Some(name) = &args.name {
        // Look up by name
        state_manager
            .load_state_by_name(name)
            .await
            .with_context(|| format!("No VM found with name '{}'", name))?
    } else {
        bail!("Either --pid or name is required");
    };

    // Get the vsock socket path for this VM
    let vm_dir = paths::vm_runtime_dir(&vm_state.vm_id);
    let vsock_socket = vm_dir.join("vsock.sock");

    info!(
        vm_id = %vm_state.vm_id,
        socket = %vsock_socket.display(),
        port = EXEC_VSOCK_PORT,
        "connecting to VM exec server via vsock"
    );

    // Connect to the vsock Unix socket
    let mut stream = UnixStream::connect(&vsock_socket).with_context(|| {
        format!(
            "Failed to connect to vsock socket at {}.\n\
             Make sure the VM is running.",
            vsock_socket.display()
        )
    })?;

    // Set timeouts (longer for TTY mode)
    let timeout = if args.tty { 3600 } else { 300 };
    stream.set_read_timeout(Some(Duration::from_secs(timeout)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    // Send CONNECT command to Firecracker's vsock proxy
    // Format: "CONNECT <port>\n"
    let connect_cmd = format!("CONNECT {}\n", EXEC_VSOCK_PORT);
    stream
        .write_all(connect_cmd.as_bytes())
        .context("sending CONNECT command to vsock")?;

    // Read the response - should be "OK <port>\n" on success
    let mut response = [0u8; 32];
    let n = stream.read(&mut response).context("reading CONNECT response")?;
    let response_str = String::from_utf8_lossy(&response[..n]);

    if !response_str.starts_with("OK ") {
        bail!(
            "Failed to connect to guest exec server: {}. \
             Make sure fc-agent is running with exec server enabled.",
            response_str.trim()
        );
    }

    info!("connected to guest exec server");

    // Check if stdin is a TTY
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };

    // Auto-detect: if running a shell and stdin is a TTY, enable -it
    let is_shell = args
        .command
        .first()
        .map(|cmd| {
            let basename = std::path::Path::new(cmd)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(cmd);
            matches!(basename, "bash" | "sh" | "zsh" | "fish" | "ash" | "dash" | "ksh" | "csh" | "tcsh")
        })
        .unwrap_or(false);

    // Determine effective flags:
    // - If explicitly set, use those
    // - If running a shell with TTY stdin, auto-enable -it
    let (interactive, tty) = if args.interactive || args.tty {
        // User explicitly specified flags
        (args.interactive, args.tty)
    } else if is_shell && stdin_is_tty {
        // Auto-detect: shell + TTY stdin = interactive mode
        info!("auto-detected shell with TTY, enabling -it");
        (true, true)
    } else {
        (false, false)
    };

    // Build the exec request
    // Default is to exec in container, --vm flag runs in VM instead
    let request = ExecRequest {
        command: args.command.clone(),
        in_container: !args.vm,
        interactive,
        tty,
    };

    // Send request as JSON followed by newline
    let request_json = serde_json::to_string(&request)?;
    writeln!(stream, "{}", request_json)?;
    stream.flush()?;

    info!(
        command = ?args.command,
        in_container = !args.vm,
        interactive,
        tty,
        "sent exec request"
    );

    if tty {
        run_tty_mode(stream)
    } else {
        run_line_mode(stream)
    }
}

/// Run in line-buffered mode (non-TTY)
fn run_line_mode(stream: UnixStream) -> Result<()> {
    let reader = BufReader::new(stream);
    let mut exit_code = 0i32;

    for line in reader.lines() {
        let line = line.context("reading from exec socket")?;

        // Parse the line as JSON
        if let Ok(response) = serde_json::from_str::<ExecResponse>(&line) {
            match response {
                ExecResponse::Stdout(data) => {
                    print!("{}", data);
                }
                ExecResponse::Stderr(data) => {
                    eprint!("{}", data);
                }
                ExecResponse::Exit(code) => {
                    exit_code = code;
                    break;
                }
                ExecResponse::Error(msg) => {
                    eprintln!("Error: {}", msg);
                    exit_code = 1;
                    break;
                }
            }
        }
    }

    // Exit with the command's exit code
    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Run in TTY mode with raw terminal
fn run_tty_mode(stream: UnixStream) -> Result<()> {
    use std::io::{stdin, stdout};
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Check if stdin is a TTY
    let stdin_fd = stdin().as_raw_fd();
    let is_tty = unsafe { libc::isatty(stdin_fd) == 1 };

    if !is_tty {
        bail!("TTY mode requires a terminal. Use without -t for non-interactive mode.");
    }

    // Save original terminal settings
    let mut orig_termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(stdin_fd, &mut orig_termios) } != 0 {
        bail!("Failed to get terminal attributes");
    }

    // Set raw mode
    let mut raw_termios = orig_termios;
    unsafe {
        libc::cfmakeraw(&mut raw_termios);
    }
    if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw_termios) } != 0 {
        bail!("Failed to set raw terminal mode");
    }

    // Flag to track if we should restore terminal
    let done = Arc::new(AtomicBool::new(false));
    let done_clone = done.clone();

    // Restore terminal on panic
    let orig_termios_copy = orig_termios;
    let restore_terminal = move || {
        unsafe {
            libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios_copy);
        }
    };

    // Clone stream for reader/writer
    let read_stream = stream.try_clone().context("cloning stream for reader")?;
    let write_stream = stream;

    // Spawn thread to read from socket and write to stdout
    let reader_done = done.clone();
    let reader_thread = std::thread::spawn(move || {
        let mut stdout = stdout().lock();
        let mut read_stream = read_stream;
        let mut buf = [0u8; 4096];

        loop {
            if reader_done.load(Ordering::Relaxed) {
                break;
            }

            match read_stream.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    // Check for exit message (JSON line with exit code)
                    let data = &buf[..n];

                    // Try to find exit message in the data
                    if let Some(exit_code) = try_parse_exit(data) {
                        reader_done.store(true, Ordering::Relaxed);
                        return Some(exit_code);
                    }

                    // Write raw data to stdout
                    let _ = stdout.write_all(data);
                    let _ = stdout.flush();
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        break;
                    }
                }
            }
        }
        None
    });

    // Spawn thread to read from stdin and write to socket
    let writer_done = done.clone();
    let writer_thread = std::thread::spawn(move || {
        let stdin = stdin();
        let mut stdin = stdin.lock();
        let mut write_stream = write_stream;
        let mut buf = [0u8; 1024];

        loop {
            if writer_done.load(Ordering::Relaxed) {
                break;
            }

            match stdin.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if write_stream.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = write_stream.flush();
                }
                Err(_) => break,
            }
        }
    });

    // Wait for reader to finish (it detects exit)
    let exit_code = reader_thread.join().ok().flatten().unwrap_or(0);

    // Signal writer to stop
    done_clone.store(true, Ordering::Relaxed);

    // Restore terminal
    restore_terminal();

    // Don't wait for writer - it may be blocked on stdin
    drop(writer_thread);

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Try to parse an exit message from raw data
fn try_parse_exit(data: &[u8]) -> Option<i32> {
    // Look for JSON exit message at end of data
    let s = String::from_utf8_lossy(data);
    for line in s.lines() {
        if let Ok(response) = serde_json::from_str::<ExecResponse>(line) {
            if let ExecResponse::Exit(code) = response {
                return Some(code);
            }
        }
    }
    None
}

/// Request sent to fc-agent exec server
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub in_container: bool,
    /// Keep STDIN open (-i)
    #[serde(default)]
    pub interactive: bool,
    /// Allocate a pseudo-TTY (-t)
    #[serde(default)]
    pub tty: bool,
}

/// Response from fc-agent exec server
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ExecResponse {
    #[serde(rename = "stdout")]
    Stdout(String),
    #[serde(rename = "stderr")]
    Stderr(String),
    #[serde(rename = "exit")]
    Exit(i32),
    #[serde(rename = "error")]
    Error(String),
}
