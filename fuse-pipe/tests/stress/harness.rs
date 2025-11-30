//! Stress test orchestration for multi-reader FUSE testing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerResult {
    pub worker_id: usize,
    pub ops_completed: usize,
    pub errors: usize,
    pub duration_ms: u64,
    pub ops_breakdown: OpsBreakdown,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpsBreakdown {
    pub getattr: usize,
    pub lookup: usize,
    pub read: usize,
    pub readdir: usize,
    pub write: usize,
    pub create: usize,
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub total_ops: usize,
    pub total_errors: usize,
    pub duration_secs: f64,
    pub ops_per_sec: f64,
    pub breakdown: OpsBreakdown,
}

pub fn run_stress_test(
    workers: usize,
    ops_per_worker: usize,
    data_dir: &PathBuf,
    mount_dir: &PathBuf,
    num_readers: usize,
    trace_rate: u64,
) -> anyhow::Result<()> {
    let pid = std::process::id();

    let socket = match prepare_socket("/tmp/fuse-stress.sock", pid) {
        Ok(s) => s,
        Err(e) => return Err(anyhow::anyhow!("failed to prepare socket path: {}", e)),
    };

    let data_dir = prepare_dir(data_dir, "fuse-stress-data", pid)?;
    let mount_dir = prepare_dir(mount_dir, "fuse-stress-mount", pid)?;

    cleanup_stale_state(&mount_dir, &socket)?;

    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║              FUSE Multi-Reader Stress Test                    ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Workers:      {:>6}                                         ║",
        workers
    );
    println!(
        "║  Ops/worker:   {:>6}                                         ║",
        ops_per_worker
    );
    println!(
        "║  Total ops:    {:>6}                                         ║",
        workers * ops_per_worker
    );
    println!(
        "║  FUSE readers: {:>6}                                         ║",
        num_readers
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");
    println!();

    setup_test_files(&data_dir, workers)?;

    let exe = std::env::current_exe()?;

    // Phase 1: Bare filesystem
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PHASE 1: Bare Filesystem (baseline)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    let bare_result = run_workers(&exe, workers, ops_per_worker, &data_dir)
        .context("bare filesystem phase")?;
    print_results("BARE FILESYSTEM", &bare_result);

    setup_test_files(&data_dir, workers)?;

    // Phase 2: FUSE
    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PHASE 2: FUSE over Unix Socket ({} readers)", num_readers);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    println!("[fuse] Starting server...");
    let mut server = Command::new(&exe)
        .args([
            "server",
            "--socket",
            socket.to_str().unwrap(),
            "--root",
            data_dir.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;

    std::thread::sleep(Duration::from_millis(500));

    println!("[fuse] Starting client with {} readers...", num_readers);
    let mut client = Command::new(&exe)
        .args([
            "client",
            "--socket",
            socket.to_str().unwrap(),
            "--mount",
            mount_dir.to_str().unwrap(),
            "--readers",
            &num_readers.to_string(),
            "--trace-rate",
            &trace_rate.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;

    std::thread::sleep(Duration::from_millis(1000));

    let fuse_result = run_workers(&exe, workers, ops_per_worker, &mount_dir)
        .context("fuse phase")?;

    let _ = client.kill();
    std::thread::sleep(Duration::from_millis(500));
    let _ = server.kill();
    let _ = fs::remove_file(&socket);

    print_results(&format!("FUSE ({} readers)", num_readers), &fuse_result);

    // Comparison
    println!();
    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║                       COMPARISON                              ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");

    let overhead = if bare_result.ops_per_sec > 0.0 {
        (bare_result.ops_per_sec - fuse_result.ops_per_sec) / bare_result.ops_per_sec * 100.0
    } else {
        0.0
    };

    let ratio = if fuse_result.ops_per_sec > 0.0 {
        bare_result.ops_per_sec / fuse_result.ops_per_sec
    } else {
        0.0
    };

    println!(
        "║  Bare filesystem:  {:>10.1} ops/sec                        ║",
        bare_result.ops_per_sec
    );
    println!(
        "║  FUSE ({} readers): {:>10.1} ops/sec                        ║",
        num_readers, fuse_result.ops_per_sec
    );
    println!(
        "║  FUSE overhead:    {:>10.1}%                                ║",
        overhead
    );
    println!(
        "║  Slowdown factor:  {:>10.2}x                                ║",
        ratio
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if fuse_result.total_errors > fuse_result.total_ops / 2 {
        anyhow::bail!(
            "FAIL: Too many errors ({}/{})",
            fuse_result.total_errors,
            fuse_result.total_ops
        );
    }

    println!("\n✅ STRESS TEST COMPLETE");
    println!(
        "   Bare: {} ops at {:.1} ops/sec",
        bare_result.total_ops, bare_result.ops_per_sec
    );
    println!(
        "   FUSE: {} ops at {:.1} ops/sec ({} readers)",
        fuse_result.total_ops, fuse_result.ops_per_sec, num_readers
    );

    Ok(())
}

fn run_workers(
    exe: &PathBuf,
    workers: usize,
    ops_per_worker: usize,
    target_dir: &PathBuf,
) -> anyhow::Result<TestResult> {
    let results_dir = PathBuf::from("/tmp/fuse-stress-results");
    fs::remove_dir_all(&results_dir).ok();
    fs::create_dir_all(&results_dir)?;

    println!(
        "[test] Spawning {} workers against {}...",
        workers,
        target_dir.display()
    );
    let start = Instant::now();

    let mut worker_handles = Vec::new();
    for i in 0..workers {
        let results_file = results_dir.join(format!("worker-{}.json", i));
        let handle = Command::new(exe)
            .args([
                "stress-worker",
                "--id",
                &i.to_string(),
                "--ops",
                &ops_per_worker.to_string(),
                "--mount",
                target_dir.to_str().unwrap(),
                "--results",
                results_file.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        worker_handles.push(handle);
    }

    for mut handle in worker_handles {
        handle.wait()?;
    }

    let total_duration = start.elapsed();

    let mut total_ops = 0usize;
    let mut total_errors = 0usize;
    let mut breakdown = OpsBreakdown::default();

    for i in 0..workers {
        let results_file = results_dir.join(format!("worker-{}.json", i));
        if let Ok(content) = fs::read_to_string(&results_file) {
            if let Ok(result) = serde_json::from_str::<WorkerResult>(&content) {
                total_ops += result.ops_completed;
                total_errors += result.errors;
                breakdown.getattr += result.ops_breakdown.getattr;
                breakdown.lookup += result.ops_breakdown.lookup;
                breakdown.read += result.ops_breakdown.read;
                breakdown.readdir += result.ops_breakdown.readdir;
                breakdown.write += result.ops_breakdown.write;
                breakdown.create += result.ops_breakdown.create;
            }
        }
    }

    let duration_secs = total_duration.as_secs_f64();
    let ops_per_sec = if duration_secs > 0.0 {
        total_ops as f64 / duration_secs
    } else {
        0.0
    };

    Ok(TestResult {
        total_ops,
        total_errors,
        duration_secs,
        ops_per_sec,
        breakdown,
    })
}

fn print_results(label: &str, result: &TestResult) {
    println!();
    println!("┌─────────────────────────────────────────────────────────────────┐");
    println!("│  {}  ", label);
    println!("├─────────────────────────────────────────────────────────────────┤");
    println!(
        "│  Total ops:      {:>10}                                    │",
        result.total_ops
    );
    println!(
        "│  Errors:         {:>10}                                    │",
        result.total_errors
    );
    println!(
        "│  Duration:       {:>10.3}s                                  │",
        result.duration_secs
    );
    println!(
        "│  Throughput:     {:>10.1} ops/sec                           │",
        result.ops_per_sec
    );
    println!("├─────────────────────────────────────────────────────────────────┤");
    println!("│  Op breakdown:                                                  │");
    println!(
        "│    getattr:      {:>10}                                    │",
        result.breakdown.getattr
    );
    println!(
        "│    lookup:       {:>10}                                    │",
        result.breakdown.lookup
    );
    println!(
        "│    read:         {:>10}                                    │",
        result.breakdown.read
    );
    println!(
        "│    readdir:      {:>10}                                    │",
        result.breakdown.readdir
    );
    println!(
        "│    write:        {:>10}                                    │",
        result.breakdown.write
    );
    println!(
        "│    create:       {:>10}                                    │",
        result.breakdown.create
    );
    println!("└─────────────────────────────────────────────────────────────────┘");
}

fn cleanup_stale_state(mount_dir: &PathBuf, socket: &PathBuf) -> anyhow::Result<()> {
    if let Some(mount) = mount_dir.to_str() {
        // Try fusermount3 first (user unmount), then fall back to umount -f
        let fusermount = Command::new("fusermount3")
            .args(["-u", mount])
            .output()
            .ok();

        let success = fusermount.as_ref().map_or(false, |o| o.status.success());

        if !success {
            let output = Command::new("umount").args(["-f", mount]).output()?;
            if !output.status.success() {
                eprintln!(
                    "[stress] warning: failed to unmount {} (code {:?})",
                    mount,
                    output.status.code()
                );
            }
        }
    }

    let _ = fs::remove_file(socket);
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}

fn prepare_dir(original: &Path, label: &str, pid: u32) -> anyhow::Result<PathBuf> {
    let candidate = original.join(format!("{}-{}", label, pid));

    // Best effort cleanup in case the base path is a stale mount.
    cleanup_mount_point(original);

    if make_writable_dir(&candidate).is_ok() {
        return Ok(candidate);
    }

    // Try once more after cleanup (handles ENOTCONN/permission issues on stale mounts).
    cleanup_mount_point(original);
    if make_writable_dir(&candidate).is_ok() {
        return Ok(candidate);
    }

    // Final fallback to a temp location so the test surfaces real errors from the workload,
    // not from an old mount point.
    let fallback = PathBuf::from(format!("/tmp/{}-{}", label, pid));
    make_writable_dir(&fallback)?;
    eprintln!(
        "[stress] warning: using fallback dir {} (could not use {})",
        fallback.display(),
        candidate.display()
    );
    Ok(fallback)
}

fn make_writable_dir(path: &Path) -> std::io::Result<()> {
    fs::remove_dir_all(path).ok();
    fs::create_dir_all(path)?;
    let probe = path.join(".fuse-stress-probe");
    fs::File::create(&probe)?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

fn cleanup_mount_point(path: &Path) {
    if path.exists() {
        let _ = Command::new("fusermount3").args(["-u", path.to_str().unwrap_or("")]).output();
        let _ = Command::new("umount").args(["-f", path.to_str().unwrap_or("")]).output();
    }
}

fn prepare_socket(default: &str, pid: u32) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(default);
    match fs::remove_file(&path) {
        Ok(_) => Ok(path),
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let fallback = PathBuf::from(format!("{}-{}", default, pid));
            eprintln!(
                "[stress] warning: falling back to socket {} (permission denied on {})",
                fallback.display(),
                path.display()
            );
            Ok(fallback)
        }
        Err(e) => Err(e.into()),
    }
}

fn setup_test_files(data_dir: &PathBuf, workers: usize) -> anyhow::Result<()> {
    // Ensure we can write; if not, bubble up so caller can decide.
    fs::create_dir_all(data_dir)?;

    for i in 0..workers {
        let worker_dir = data_dir.join(format!("worker-{}", i));
        fs::remove_dir_all(&worker_dir).ok();
        fs::create_dir_all(&worker_dir)?;

        for j in 0..10 {
            let file = worker_dir.join(format!("file-{}.txt", j));
            let content: String = (0..1024)
                .map(|k| char::from(b'a' + ((j + k) % 26) as u8))
                .collect();
            fs::write(&file, content)?;
        }
    }

    Ok(())
}
