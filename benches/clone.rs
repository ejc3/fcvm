//! Clone benchmarks measuring snapshot restore and execution latency.
//!
//! Measures clone startup time with both exec and HTTP workloads.
//! Uses rootless networking only (bridged has dnsmasq timing issues under load).
//!
//! Run with: cargo bench --bench clone
//! Or: make bench-clone

use criterion::{criterion_group, criterion_main, Criterion};
use serde::Deserialize;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TEST_IMAGE: &str = "public.ecr.aws/nginx/nginx:alpine";

/// VM state from fcvm ls --json
#[derive(Deserialize)]
struct VmLsEntry {
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
    health_check_port: Option<u16>,
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

/// Snapshot/clone fixture - baseline VM + serve process
struct CloneFixture {
    baseline_pid: u32,
    serve_pid: u32,
}

impl CloneFixture {
    /// Create a baseline VM, snapshot it, and start serve process
    fn setup(name: &str, network: &str) -> Self {
        let fcvm = find_fcvm_binary();
        let snapshot_name = format!("bench-snap-{}", name);
        let baseline_name = format!("bench-baseline-{}", name);

        // Start baseline VM
        eprintln!("  Starting baseline VM...");
        let child = Command::new(&fcvm)
            .args([
                "podman", "run", "--name", &baseline_name,
                "--network", network, TEST_IMAGE,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn baseline VM");

        let baseline_pid = child.id();

        // Wait for healthy
        let start = Instant::now();
        let timeout = Duration::from_secs(180);
        loop {
            if start.elapsed() > timeout {
                panic!("Baseline VM failed to become healthy");
            }
            let output = Command::new(&fcvm)
                .args(["ls", "--json", "--pid", &baseline_pid.to_string()])
                .output()
                .expect("failed to run fcvm ls");

            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(vms) = serde_json::from_str::<Vec<VmLsEntry>>(&stdout) {
                    if vms.first().map(|v| v.health_status == "healthy").unwrap_or(false) {
                        eprintln!("  Baseline healthy after {:.1}s", start.elapsed().as_secs_f64());
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
                "snapshot", "create",
                "--pid", &baseline_pid.to_string(),
                "--tag", &snapshot_name,
            ])
            .output()
            .expect("failed to create snapshot");

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("Snapshot creation failed: {}", stderr);
        }

        // Start serve process
        eprintln!("  Starting serve process...");
        let serve_child = Command::new(&fcvm)
            .args(["snapshot", "serve", &snapshot_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
            baseline_pid,
            serve_pid,
        }
    }

    /// Run a clone with --exec and measure total time
    fn clone_exec(&self, cmd: &str, network: &str) -> Duration {
        let fcvm = find_fcvm_binary();
        let start = Instant::now();

        let output = Command::new(&fcvm)
            .args([
                "snapshot", "run",
                "--pid", &self.serve_pid.to_string(),
                "--network", network,
                "--exec", cmd,
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
    fn clone_http(&self, network: &str) -> Duration {
        let fcvm = find_fcvm_binary();
        let start = Instant::now();

        // Spawn clone (without --exec so it stays running)
        let mut child = Command::new(&fcvm)
            .args([
                "snapshot", "run",
                "--pid", &self.serve_pid.to_string(),
                "--network", network,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn clone");

        let clone_pid = child.id();

        // Poll for healthy and get loopback IP
        let (loopback_ip, health_port) = loop {
            if start.elapsed() > Duration::from_secs(30) {
                let _ = Command::new("kill").args(["-9", &clone_pid.to_string()]).output();
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
                            let ip = vm.config.network.loopback_ip.clone()
                                .expect("no loopback_ip for healthy clone");
                            let port = vm.config.network.health_check_port.unwrap_or(8080);
                            break (ip, port);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        // Make HTTP request to nginx
        let addr = format!("{}:{}", loopback_ip, health_port);
        let mut stream = TcpStream::connect(&addr).expect("failed to connect to nginx");
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();

        let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        stream.write_all(request.as_bytes()).expect("failed to send HTTP request");

        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);

        // Verify we got a valid response
        let response_str = String::from_utf8_lossy(&response);
        if !response_str.contains("200 OK") {
            panic!("unexpected HTTP response: {}", &response_str[..std::cmp::min(200, response_str.len())]);
        }

        // Kill the clone
        let _ = Command::new("kill").args(["-TERM", &clone_pid.to_string()]).output();

        // Wait for child to exit
        let kill_start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if kill_start.elapsed() > Duration::from_secs(5) {
                        let _ = Command::new("kill").args(["-9", &clone_pid.to_string()]).output();
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

    fn kill(&self) {
        let _ = Command::new("kill").args(["-TERM", &self.serve_pid.to_string()]).output();
        let _ = Command::new("kill").args(["-TERM", &self.baseline_pid.to_string()]).output();
        std::thread::sleep(Duration::from_secs(2));
        let _ = Command::new("kill").args(["-9", &self.serve_pid.to_string()]).output();
        let _ = Command::new("kill").args(["-9", &self.baseline_pid.to_string()]).output();
    }
}

impl Drop for CloneFixture {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Benchmark clone latency with both exec and HTTP workloads
fn bench_clone(c: &mut Criterion) {
    eprintln!("\n=== Setting up snapshot for clone benchmarks ===");

    // Only test rootless - bridged clones have dnsmasq binding issues under rapid iteration
    let fixture = CloneFixture::setup("clone", "rootless");

    let mut group = c.benchmark_group("clone");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    // Clone + exec + cleanup
    group.bench_function("exec/echo", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += fixture.clone_exec("echo hello", "rootless");
            }
            total
        })
    });

    // Clone + wait healthy + HTTP GET + cleanup
    group.bench_function("http/nginx", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += fixture.clone_http("rootless");
            }
            total
        })
    });

    group.finish();
    eprintln!("\n=== Cleaning up clone fixtures ===");
}

criterion_group!(benches, bench_clone);
criterion_main!(benches);
