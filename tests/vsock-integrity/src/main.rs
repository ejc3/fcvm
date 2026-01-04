//! Vsock data integrity test for NV2 nested virtualization.
//!
//! This tests whether large vsock writes get corrupted under nested
//! virtualization due to cache coherency issues in double S2 translation.
//!
//! Usage:
//!   vsock-integrity file-test <fuse-path>     # Test file I/O through FUSE
//!   vsock-integrity server <unix-socket-path>  # Raw vsock echo server
//!   vsock-integrity client-unix <socket-path>  # Raw vsock client (Unix socket)
//!   vsock-integrity client-vsock <cid> <port>  # Raw vsock client (vsock)

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

const TEST_SIZES: &[usize] = &[
    32 * 1024,     // 32KB - under vsock packet limit
    64 * 1024,     // 64KB - exactly one packet
    128 * 1024,    // 128KB - 2 packets (fragmentation)
    256 * 1024,    // 256KB - 4 packets
    512 * 1024,    // 512KB - 8 packets
    1024 * 1024,   // 1MB - stress test
    2 * 1024 * 1024, // 2MB - stress test
];

/// Number of iterations to run per thread (stress test)
const ITERATIONS: usize = 10;

/// Number of parallel threads for stress test (mimics FUSE reader count)
const PARALLEL_THREADS: usize = 64;

/// Generate a pattern that's easy to detect corruption in.
/// Each 4KB block starts with its block number.
fn generate_pattern(size: usize) -> Vec<u8> {
    let mut data = vec![0xAA_u8; size];
    for (i, chunk) in data.chunks_mut(4096).enumerate() {
        let marker = (i as u32).to_le_bytes();
        chunk[..4].copy_from_slice(&marker);
    }
    data
}

/// Check for corruption in the pattern.
fn check_pattern(data: &[u8], expected: &[u8]) -> Result<(), String> {
    if data.len() != expected.len() {
        return Err(format!(
            "Length mismatch: got {}, expected {}",
            data.len(),
            expected.len()
        ));
    }

    for (i, (a, b)) in data.iter().zip(expected.iter()).enumerate() {
        if a != b {
            let zeros = data.iter().filter(|&&b| b == 0).count();
            return Err(format!(
                "Corruption at offset {}: expected 0x{:02X}, got 0x{:02X}\n\
                 Zero bytes in data: {} ({:.1}%)",
                i,
                b,
                a,
                zeros,
                (zeros as f64 / data.len() as f64) * 100.0
            ));
        }
    }
    Ok(())
}

/// Test file I/O integrity through FUSE mount.
/// This is the simplest way to test vsock data integrity since FUSE uses vsock.
fn run_file_test(fuse_path: &str) -> std::io::Result<()> {
    let test_dir = Path::new(fuse_path).join(format!("vsock-test-{}", std::process::id()));
    fs::create_dir_all(&test_dir)?;

    println!("Testing file I/O integrity at {}", test_dir.display());
    println!("Test sizes: {:?} bytes", TEST_SIZES);

    let mut passed = 0;
    let mut failed = 0;

    for &size in TEST_SIZES {
        let filename = test_dir.join(format!("test_{}kb.bin", size / 1024));
        let expected = generate_pattern(size);

        // Write file
        {
            let mut f = fs::File::create(&filename)?;
            f.write_all(&expected)?;
            f.sync_all()?;
        }

        // Read back
        let actual = fs::read(&filename)?;

        // Verify
        match check_pattern(&actual, &expected) {
            Ok(()) => {
                println!("✓ {}KB: OK", size / 1024);
                passed += 1;
            }
            Err(e) => {
                eprintln!("✗ {}KB: CORRUPTION\n  {}", size / 1024, e);
                failed += 1;
            }
        }

        // Cleanup
        fs::remove_file(&filename).ok();
    }

    // Cleanup test directory
    fs::remove_dir(&test_dir).ok();

    println!("\nResults: {} passed, {} failed", passed, failed);

    if failed > 0 {
        eprintln!("\nCORRUPTION DETECTED!");
        eprintln!("This indicates vsock fragmentation issues under NV2 nested virtualization.");
        std::process::exit(1);
    }

    println!("VSOCK_INTEGRITY_OK");
    Ok(())
}

fn handle_client(mut stream: UnixStream, client_id: usize) {
    println!("[client {}] connected", client_id);

    // Handle multiple messages on the same connection
    loop {
        // Read size header (4 bytes, little endian)
        let mut size_buf = [0u8; 4];
        match stream.read_exact(&mut size_buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                println!("[client {}] disconnected", client_id);
                break;
            }
            Err(e) => {
                eprintln!("[client {}] read error: {}", client_id, e);
                break;
            }
        }
        let size = u32::from_le_bytes(size_buf) as usize;

        // Read data
        let mut data = vec![0u8; size];
        if let Err(e) = stream.read_exact(&mut data) {
            eprintln!("[client {}] read data error: {}", client_id, e);
            break;
        }

        // Echo back
        if let Err(e) = stream.write_all(&size_buf) {
            eprintln!("[client {}] write error: {}", client_id, e);
            break;
        }
        if let Err(e) = stream.write_all(&data) {
            eprintln!("[client {}] write data error: {}", client_id, e);
            break;
        }
        if let Err(e) = stream.flush() {
            eprintln!("[client {}] flush error: {}", client_id, e);
            break;
        }
    }
}

fn run_server(socket_path: &str) -> std::io::Result<()> {
    // Remove existing socket
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    println!("Server listening on {} (multi-threaded)", socket_path);

    let mut client_id = 0;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                client_id += 1;
                let id = client_id;
                // Spawn a thread for each client to handle concurrent connections
                thread::spawn(move || {
                    handle_client(stream, id);
                });
            }
            Err(e) => {
                eprintln!("Accept error: {}", e);
            }
        }
    }
    Ok(())
}

fn run_client_unix(socket_path: &str) -> std::io::Result<()> {
    println!("Connecting to {}", socket_path);
    let mut stream = UnixStream::connect(socket_path)?;
    println!("Connected");

    let mut passed = 0;
    let mut failed = 0;

    for &size in TEST_SIZES {
        let expected = generate_pattern(size);

        // Send size header
        stream.write_all(&(size as u32).to_le_bytes())?;
        // Send data
        stream.write_all(&expected)?;
        stream.flush()?;

        // Read size header
        let mut size_buf = [0u8; 4];
        stream.read_exact(&mut size_buf)?;
        let recv_size = u32::from_le_bytes(size_buf) as usize;

        if recv_size != size {
            eprintln!("✗ {}KB: Size mismatch (got {})", size / 1024, recv_size);
            failed += 1;
            continue;
        }

        // Read data
        let mut actual = vec![0u8; size];
        stream.read_exact(&mut actual)?;

        match check_pattern(&actual, &expected) {
            Ok(()) => {
                println!("✓ {}KB: OK", size / 1024);
                passed += 1;
            }
            Err(e) => {
                eprintln!("✗ {}KB: CORRUPTION\n  {}", size / 1024, e);
                failed += 1;
            }
        }
    }

    println!("\nResults: {} passed, {} failed", passed, failed);
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Create a vsock connection and return as UnixStream
#[cfg(target_os = "linux")]
fn connect_vsock(cid: u32, port: u32) -> std::io::Result<UnixStream> {
    use std::os::unix::io::FromRawFd;

    // Create vsock socket
    let fd = unsafe {
        libc::socket(
            libc::AF_VSOCK,
            libc::SOCK_STREAM,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Connect
    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0; 4],
    };

    let ret = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as u32,
        )
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Convert to UnixStream for convenience (they're both sockets)
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Run a single thread's worth of tests
fn run_thread_tests(mut stream: UnixStream, thread_id: usize,
                    passed: Arc<AtomicUsize>, failed: Arc<AtomicUsize>) {
    for iteration in 0..ITERATIONS {
        for &size in TEST_SIZES {
            let expected = generate_pattern(size);

            // Send size header
            if stream.write_all(&(size as u32).to_le_bytes()).is_err() {
                failed.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            // Send data
            if stream.write_all(&expected).is_err() {
                failed.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            if stream.flush().is_err() {
                failed.fetch_add(1, Ordering::SeqCst);
                continue;
            }

            // Read size header
            let mut size_buf = [0u8; 4];
            if stream.read_exact(&mut size_buf).is_err() {
                failed.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            let recv_size = u32::from_le_bytes(size_buf) as usize;

            if recv_size != size {
                eprintln!("[thread {}] ✗ {}KB: Size mismatch (got {})", thread_id, size / 1024, recv_size);
                failed.fetch_add(1, Ordering::SeqCst);
                continue;
            }

            // Read data
            let mut actual = vec![0u8; size];
            if stream.read_exact(&mut actual).is_err() {
                failed.fetch_add(1, Ordering::SeqCst);
                continue;
            }

            match check_pattern(&actual, &expected) {
                Ok(()) => {
                    passed.fetch_add(1, Ordering::SeqCst);
                }
                Err(e) => {
                    eprintln!("[thread {}] ✗ {}KB iter {}: CORRUPTION\n  {}", thread_id, size / 1024, iteration, e);
                    failed.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }
    println!("[thread {}] finished {} tests", thread_id, TEST_SIZES.len() * ITERATIONS);
}

#[cfg(target_os = "linux")]
fn run_client_vsock(cid: u32, port: u32) -> std::io::Result<()> {
    let total_tests = TEST_SIZES.len() * ITERATIONS * PARALLEL_THREADS;
    println!("Starting {} parallel threads, {} iterations each, {} sizes",
             PARALLEL_THREADS, ITERATIONS, TEST_SIZES.len());
    println!("Total tests: {} (transferring ~{}MB total)",
             total_tests,
             (TEST_SIZES.iter().sum::<usize>() * ITERATIONS * PARALLEL_THREADS) / (1024 * 1024));

    let passed = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));

    // Spawn all threads
    let mut handles = Vec::new();
    for thread_id in 0..PARALLEL_THREADS {
        let stream = connect_vsock(cid, port)?;
        let p = passed.clone();
        let f = failed.clone();
        handles.push(thread::spawn(move || {
            run_thread_tests(stream, thread_id, p, f);
        }));
    }

    println!("All {} threads connected, running tests...", PARALLEL_THREADS);

    // Wait for all threads
    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    let final_passed = passed.load(Ordering::SeqCst);
    let final_failed = failed.load(Ordering::SeqCst);

    println!("\n=== Final Results ===");
    println!("{} passed, {} failed (out of {})", final_passed, final_failed, total_tests);
    if final_failed > 0 {
        eprintln!("\nCORRUPTION DETECTED!");
        std::process::exit(1);
    }
    println!("VSOCK_INTEGRITY_OK");
    Ok(())
}

fn run_client_with_stream(mut stream: UnixStream) -> std::io::Result<()> {
    let mut passed = 0;
    let mut failed = 0;
    let total_tests = TEST_SIZES.len() * ITERATIONS;

    println!("Running {} iterations of {} sizes ({} total tests)",
             ITERATIONS, TEST_SIZES.len(), total_tests);

    for iteration in 0..ITERATIONS {
        println!("\n--- Iteration {}/{} ---", iteration + 1, ITERATIONS);

        for &size in TEST_SIZES {
            let expected = generate_pattern(size);

            // Send size header
            stream.write_all(&(size as u32).to_le_bytes())?;
            // Send data
            stream.write_all(&expected)?;
            stream.flush()?;

            // Read size header
            let mut size_buf = [0u8; 4];
            stream.read_exact(&mut size_buf)?;
            let recv_size = u32::from_le_bytes(size_buf) as usize;

            if recv_size != size {
                eprintln!("✗ {}KB: Size mismatch (got {})", size / 1024, recv_size);
                failed += 1;
                continue;
            }

            // Read data
            let mut actual = vec![0u8; size];
            stream.read_exact(&mut actual)?;

            match check_pattern(&actual, &expected) {
                Ok(()) => {
                    println!("✓ {}KB: OK", size / 1024);
                    passed += 1;
                }
                Err(e) => {
                    eprintln!("✗ {}KB: CORRUPTION\n  {}", size / 1024, e);
                    failed += 1;
                }
            }
        }
    }

    println!("\n=== Final Results ===");
    println!("{} passed, {} failed (out of {})", passed, failed, total_tests);
    if failed > 0 {
        eprintln!("\nCORRUPTION DETECTED!");
        std::process::exit(1);
    }
    println!("VSOCK_INTEGRITY_OK");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  {} file-test <fuse-path>            # Test file I/O through FUSE", args[0]);
        eprintln!("  {} server <vsock-dir> <port>        # Echo server for fcvm --vsock-dir", args[0]);
        eprintln!("  {} server-socket <socket-path>      # Echo server on raw Unix socket", args[0]);
        eprintln!("  {} client-unix <socket-path>        # Client via Unix socket", args[0]);
        eprintln!("  {} client-vsock <cid> <port>        # Client via vsock (in guest)", args[0]);
        eprintln!();
        eprintln!("Example (L1 runs server, L2 runs client):");
        eprintln!("  # In L1: Start echo server, then start L2 VM");
        eprintln!("  {} server /tmp/vsock-test 9999 &", args[0]);
        eprintln!("  fcvm podman run --vsock-dir /tmp/vsock-test ...");
        eprintln!();
        eprintln!("  # In L2: Connect to echo server via vsock");
        eprintln!("  {} client-vsock 2 9999", args[0]);
        std::process::exit(1);
    }

    let result = match args[1].as_str() {
        "file-test" => {
            if args.len() < 3 {
                eprintln!("Missing FUSE path");
                std::process::exit(1);
            }
            run_file_test(&args[2])
        }
        "server" => {
            if args.len() < 4 {
                eprintln!("Missing vsock-dir and port");
                eprintln!("Usage: {} server <vsock-dir> <port>", args[0]);
                std::process::exit(1);
            }
            let socket_path = format!("{}/vsock.sock_{}", args[2], args[3]);
            // Create directory if needed
            let _ = std::fs::create_dir_all(&args[2]);
            run_server(&socket_path)
        }
        "server-socket" => {
            if args.len() < 3 {
                eprintln!("Missing socket path");
                std::process::exit(1);
            }
            run_server(&args[2])
        }
        "client-unix" => {
            if args.len() < 3 {
                eprintln!("Missing socket path");
                std::process::exit(1);
            }
            run_client_unix(&args[2])
        }
        #[cfg(target_os = "linux")]
        "client-vsock" => {
            if args.len() < 4 {
                eprintln!("Missing cid and port");
                std::process::exit(1);
            }
            let cid: u32 = args[2].parse().expect("Invalid CID");
            let port: u32 = args[3].parse().expect("Invalid port");
            run_client_vsock(cid, port)
        }
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
