//! Exec command latency benchmarks comparing bridged vs rootless networking.
//!
//! Measures the time to execute simple commands in VMs via vsock.
//! VMs are started once per benchmark group to amortize startup cost.
//!
//! Run with: cargo bench --bench exec
//! Or: make bench-exec

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TEST_IMAGE: &str = "public.ecr.aws/nginx/nginx:alpine";

/// VM state from fcvm ls --json
#[derive(Deserialize)]
#[allow(dead_code)] // fields are deserialized from JSON but not all are used directly
struct VmLsEntry {
    pid: Option<u32>,
    health_status: String,
    config: VmConfigEntry,
}

#[derive(Deserialize)]
struct VmConfigEntry {
    network: NetworkConfigEntry,
}

#[derive(Deserialize)]
struct NetworkConfigEntry {
    loopback_ip: Option<String>,
}

/// Find the fcvm binary
fn find_fcvm_binary() -> PathBuf {
    let candidates = [
        "./target/release/fcvm",
        "./target/debug/fcvm",
        "/usr/local/bin/fcvm",
    ];
    for path in candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    panic!("fcvm binary not found - run: cargo build --release");
}

/// A running VM fixture for benchmarking
struct VmFixture {
    pid: u32,
    child: Child,
    _name: String,
    _network: String,
}

impl VmFixture {
    /// Start a VM and wait for it to become healthy
    fn start(name: &str, network: &str) -> Self {
        let fcvm = find_fcvm_binary();
        let log_path = format!("/tmp/fcvm-bench-{}.log", name);
        let log_file = File::create(&log_path)
            .unwrap_or_else(|e| panic!("failed to create {}: {}", log_path, e));
        let log_err = log_file.try_clone().expect("failed to clone log file");

        // Spawn VM process
        let child = Command::new(&fcvm)
            .args([
                "podman",
                "run",
                "--name",
                name,
                "--network",
                network,
                TEST_IMAGE,
            ])
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("failed to spawn fcvm");

        let pid = child.id();
        eprintln!("  Started {} VM (PID: {})", network, pid);

        // Poll for healthy (VMs can take 2+ minutes on first start)
        let start = Instant::now();
        let timeout = Duration::from_secs(180);
        loop {
            if start.elapsed() > timeout {
                // Dump logs before panicking
                if let Ok(logs) = std::fs::read_to_string(&log_path) {
                    let tail: String = logs
                        .lines()
                        .rev()
                        .take(50)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n");
                    eprintln!("=== Last 50 lines of {} ===\n{}", log_path, tail);
                }
                panic!("VM {} failed to become healthy within {:?}", name, timeout);
            }

            let output = Command::new(&fcvm)
                .args(["ls", "--json", "--pid", &pid.to_string()])
                .output()
                .expect("failed to run fcvm ls");

            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(vms) = serde_json::from_str::<Vec<VmLsEntry>>(&stdout) {
                    if vms
                        .first()
                        .map(|v| v.health_status == "healthy")
                        .unwrap_or(false)
                    {
                        eprintln!("  VM healthy after {:.1}s", start.elapsed().as_secs_f64());
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        Self {
            pid,
            child,
            _name: name.to_string(),
            _network: network.to_string(),
        }
    }

    /// Execute a command in the VM (in container by default)
    fn exec(&self, cmd: &[&str]) -> Duration {
        let fcvm = find_fcvm_binary();
        let start = Instant::now();

        let pid_str = self.pid.to_string();
        let mut args = vec!["exec", "--pid", &pid_str, "--"];
        args.extend(cmd);

        let output = Command::new(&fcvm)
            .args(&args)
            .output()
            .expect("failed to run fcvm exec");

        let elapsed = start.elapsed();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("exec failed: {}", stderr);
        }

        elapsed
    }

    /// Execute a command in the VM itself (not container)
    fn exec_vm(&self, cmd: &[&str]) -> Duration {
        let fcvm = find_fcvm_binary();
        let start = Instant::now();

        let pid_str = self.pid.to_string();
        let mut args = vec!["exec", "--pid", &pid_str, "--vm", "--"];
        args.extend(cmd);

        let output = Command::new(&fcvm)
            .args(&args)
            .output()
            .expect("failed to run fcvm exec");

        let elapsed = start.elapsed();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("exec --vm failed: {}", stderr);
        }

        elapsed
    }

    /// Kill the VM gracefully (SIGTERM first, then SIGKILL)
    fn kill(&mut self) {
        // Use centralized graceful kill to allow cleanup
        fcvm::utils::graceful_kill(self.pid, 2000);
        // Wait to reap the process and avoid zombies
        let _ = self.child.wait();
    }
}

impl Drop for VmFixture {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Benchmark exec latency: single command execution time
fn bench_exec_latency(c: &mut Criterion) {
    eprintln!("\n=== Setting up VMs for latency benchmarks ===");

    // Start VMs for both network modes
    let bridged_vm = VmFixture::start("bench-exec-bridged", "bridged");
    let rootless_vm = VmFixture::start("bench-exec-rootless", "rootless");

    let mut group = c.benchmark_group("exec_latency");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    // Simple echo command - measures vsock + exec overhead
    group.bench_function("bridged/echo", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += bridged_vm.exec(&["echo", "hello"]);
            }
            total
        })
    });

    group.bench_function("rootless/echo", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += rootless_vm.exec(&["echo", "hello"]);
            }
            total
        })
    });

    // Exec in VM (not container) - slightly faster path
    group.bench_function("bridged/echo_vm", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += bridged_vm.exec_vm(&["echo", "hello"]);
            }
            total
        })
    });

    group.bench_function("rootless/echo_vm", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += rootless_vm.exec_vm(&["echo", "hello"]);
            }
            total
        })
    });

    group.finish();

    // VMs cleaned up by Drop
    eprintln!("\n=== Cleaning up VMs ===");
}

/// Benchmark exec with data output - measures response size impact
fn bench_exec_data(c: &mut Criterion) {
    eprintln!("\n=== Setting up VMs for data benchmarks ===");

    let bridged_vm = VmFixture::start("bench-data-bridged", "bridged");
    let rootless_vm = VmFixture::start("bench-data-rootless", "rootless");

    let mut group = c.benchmark_group("exec_data");
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(10));

    // Output ~1KB of data
    for (name, size) in [("1kb", 1024), ("4kb", 4096), ("16kb", 16384)] {
        let cmd_str = format!("head -c {} /dev/zero | base64", size);

        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("bridged", name), &cmd_str, |b, cmd| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    total += bridged_vm.exec_vm(&["sh", "-c", cmd]);
                }
                total
            })
        });

        group.bench_with_input(BenchmarkId::new("rootless", name), &cmd_str, |b, cmd| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    total += rootless_vm.exec_vm(&["sh", "-c", cmd]);
                }
                total
            })
        });
    }

    group.finish();
    eprintln!("\n=== Cleaning up VMs ===");
}

/// Benchmark sequential exec throughput - commands per second
fn bench_exec_throughput(c: &mut Criterion) {
    eprintln!("\n=== Setting up VMs for throughput benchmarks ===");

    let bridged_vm = VmFixture::start("bench-tput-bridged", "bridged");
    let rootless_vm = VmFixture::start("bench-tput-rootless", "rootless");

    let mut group = c.benchmark_group("exec_throughput");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));
    group.throughput(Throughput::Elements(10)); // 10 commands per iteration

    // Run 10 sequential commands per iteration
    group.bench_function("bridged/10_commands", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                for _ in 0..10 {
                    total += bridged_vm.exec(&["true"]);
                }
            }
            total
        })
    });

    group.bench_function("rootless/10_commands", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                for _ in 0..10 {
                    total += rootless_vm.exec(&["true"]);
                }
            }
            total
        })
    });

    group.finish();
    eprintln!("\n=== Cleaning up VMs ===");
}

/// Snapshot/clone fixture - baseline VM + serve process
#[allow(dead_code)] // snapshot_name is used in setup but not directly accessed after
struct CloneFixture {
    baseline_child: Child,
    serve_child: Child,
    serve_pid: u32,
    snapshot_name: String,
}

impl CloneFixture {
    /// Create a baseline VM, snapshot it, and start serve process
    fn setup(name: &str, network: &str) -> Self {
        let fcvm = find_fcvm_binary();
        let snapshot_name = format!("bench-snap-{}", name);
        let baseline_name = format!("bench-baseline-{}", name);

        // Start baseline VM
        eprintln!("  Starting baseline VM...");
        let log_path = format!("/tmp/fcvm-bench-{}.log", baseline_name);
        let log_file =
            File::create(&log_path).unwrap_or_else(|e| panic!("create {}: {}", log_path, e));
        let log_err = log_file.try_clone().expect("clone log file");
        let baseline_child = Command::new(&fcvm)
            .args([
                "podman",
                "run",
                "--name",
                &baseline_name,
                "--network",
                network,
                TEST_IMAGE,
            ])
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("failed to spawn baseline VM");

        let baseline_pid = baseline_child.id();

        // Wait for healthy
        let start = Instant::now();
        let timeout = Duration::from_secs(180);
        loop {
            if start.elapsed() > timeout {
                if let Ok(logs) = std::fs::read_to_string(&log_path) {
                    let tail: Vec<&str> = logs.lines().rev().take(50).collect();
                    eprintln!("=== Last 50 lines of {} ===", log_path);
                    for line in tail.into_iter().rev() {
                        eprintln!("{}", line);
                    }
                }
                panic!("Baseline VM failed to become healthy");
            }
            let output = Command::new(&fcvm)
                .args(["ls", "--json", "--pid", &baseline_pid.to_string()])
                .output()
                .expect("failed to run fcvm ls");

            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(vms) = serde_json::from_str::<Vec<VmLsEntry>>(&stdout) {
                    if vms
                        .first()
                        .map(|v| v.health_status == "healthy")
                        .unwrap_or(false)
                    {
                        eprintln!(
                            "  Baseline healthy after {:.1}s",
                            start.elapsed().as_secs_f64()
                        );
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        // Create snapshot
        eprintln!("  Creating snapshot...");
        let output = Command::new(&fcvm)
            .args([
                "snapshot",
                "create",
                "--pid",
                &baseline_pid.to_string(),
                "--tag",
                &snapshot_name,
            ])
            .output()
            .expect("failed to create snapshot");

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("Snapshot creation failed: {}", stderr);
        }

        // Start serve process
        eprintln!("  Starting serve process...");
        let serve_log_path = format!("/tmp/fcvm-bench-serve-{}.log", name);
        let serve_log = File::create(&serve_log_path)
            .unwrap_or_else(|e| panic!("create {}: {}", serve_log_path, e));
        let serve_log_err = serve_log.try_clone().expect("clone log file");
        let serve_child = Command::new(&fcvm)
            .args(["snapshot", "serve", &snapshot_name])
            .stdout(Stdio::from(serve_log))
            .stderr(Stdio::from(serve_log_err))
            .spawn()
            .expect("failed to spawn serve process");

        let serve_pid = serve_child.id();

        // Wait for serve socket
        let socket_path = format!("/mnt/fcvm-btrfs/uffd-{}-{}.sock", snapshot_name, serve_pid);
        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(30) {
                panic!("Serve socket never appeared: {}", socket_path);
            }
            if std::path::Path::new(&socket_path).exists() {
                eprintln!("  Serve ready (PID: {})", serve_pid);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        Self {
            baseline_child,
            serve_child,
            serve_pid,
            snapshot_name,
        }
    }

    /// Run a clone with --exec and measure total time (clone startup + exec + cleanup)
    fn clone_exec(&self, cmd: &str, network: &str) -> Duration {
        let fcvm = find_fcvm_binary();
        let start = Instant::now();

        let output = Command::new(&fcvm)
            .args([
                "snapshot",
                "run",
                "--pid",
                &self.serve_pid.to_string(),
                "--network",
                network,
                "--exec",
                cmd,
            ])
            .output()
            .expect("failed to run snapshot run --exec");

        let elapsed = start.elapsed();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("clone exec failed: {}", stderr);
        }

        elapsed
    }

    /// Spawn a clone, wait for healthy, hit nginx via HTTP, kill clone
    /// Returns total time from spawn to cleanup complete
    fn clone_http(&self, network: &str) -> Duration {
        let fcvm = find_fcvm_binary();
        let start = Instant::now();

        // Spawn clone (without --exec so it stays running)
        let health_port = 8080;
        let clone_log_path = format!("/tmp/fcvm-bench-clone-http-{}.log", network);
        let clone_log = File::create(&clone_log_path)
            .unwrap_or_else(|e| panic!("create {}: {}", clone_log_path, e));
        let clone_log_err = clone_log.try_clone().expect("clone log file");
        let mut child = Command::new(&fcvm)
            .args([
                "snapshot",
                "run",
                "--pid",
                &self.serve_pid.to_string(),
                "--network",
                network,
                "--publish",
                &format!("{}:80", health_port),
            ])
            .stdout(Stdio::from(clone_log))
            .stderr(Stdio::from(clone_log_err))
            .spawn()
            .expect("failed to spawn clone");

        let clone_pid = child.id();

        // Poll for healthy and get loopback IP
        let loopback_ip = loop {
            if start.elapsed() > Duration::from_secs(30) {
                let _ = Command::new("kill")
                    .args(["-9", &clone_pid.to_string()])
                    .output();
                panic!("clone failed to become healthy within 30s");
            }

            let output = Command::new(&fcvm)
                .args(["ls", "--json", "--pid", &clone_pid.to_string()])
                .output()
                .expect("failed to run fcvm ls");

            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(vms) = serde_json::from_str::<Vec<VmLsEntry>>(&stdout) {
                    if let Some(vm) = vms.first() {
                        if vm.health_status == "healthy" {
                            let ip = vm
                                .config
                                .network
                                .loopback_ip
                                .clone()
                                .expect("no loopback_ip for healthy clone");
                            break ip;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        // Make HTTP request to nginx
        let addr = format!("{}:{}", loopback_ip, health_port);
        let mut stream = TcpStream::connect(&addr).expect("failed to connect to nginx");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        stream
            .write_all(request.as_bytes())
            .expect("failed to send HTTP request");

        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);

        // Verify we got a valid response
        let response_str = String::from_utf8_lossy(&response);
        if !response_str.contains("200 OK") {
            panic!(
                "unexpected HTTP response: {}",
                &response_str[..std::cmp::min(200, response_str.len())]
            );
        }

        // Kill the clone
        let _ = Command::new("kill")
            .args(["-TERM", &clone_pid.to_string()])
            .output();

        // Wait for child to exit (with timeout)
        let kill_start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if kill_start.elapsed() > Duration::from_secs(5) {
                        let _ = Command::new("kill")
                            .args(["-9", &clone_pid.to_string()])
                            .output();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }

        start.elapsed()
    }

    fn kill(&mut self) {
        // Gracefully kill both processes (SIGTERM first, then SIGKILL)
        // Kill serve first (it will cascade to clones)
        fcvm::utils::graceful_kill(self.serve_pid, 2000);
        fcvm::utils::graceful_kill(self.baseline_child.id(), 2000);
        // Wait to reap and avoid zombies
        let _ = self.serve_child.wait();
        let _ = self.baseline_child.wait();
    }
}

impl Drop for CloneFixture {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Benchmark clone exec latency: spawn clone + exec + cleanup
/// Uses rootless networking only (bridged has dnsmasq timing issues under load)
fn bench_clone_exec(c: &mut Criterion) {
    eprintln!("\n=== Setting up snapshot for clone exec benchmarks ===");

    // Only test rootless - bridged clones have dnsmasq binding issues under rapid iteration
    let fixture = CloneFixture::setup("clone-exec", "rootless");

    let mut group = c.benchmark_group("clone_exec");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    // Clone + exec + cleanup (total round-trip)
    group.bench_function("rootless/echo", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += fixture.clone_exec("echo hello", "rootless");
            }
            total
        })
    });

    group.finish();
    eprintln!("\n=== Cleaning up clone exec fixtures ===");
}

/// Benchmark clone HTTP latency: spawn clone + wait healthy + HTTP request + cleanup
/// Measures raw network latency without podman exec overhead
fn bench_clone_http(c: &mut Criterion) {
    eprintln!("\n=== Setting up snapshot for clone HTTP benchmarks ===");

    let fixture = CloneFixture::setup("clone-http", "rootless");

    let mut group = c.benchmark_group("clone_http");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    // Clone + wait healthy + HTTP GET + cleanup
    group.bench_function("rootless/nginx", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += fixture.clone_http("rootless");
            }
            total
        })
    });

    group.finish();
    eprintln!("\n=== Cleaning up clone HTTP fixtures ===");
}

criterion_group!(
    benches,
    bench_exec_latency,
    bench_exec_data,
    bench_exec_throughput,
    bench_clone_exec,
    bench_clone_http,
);

criterion_main!(benches);
