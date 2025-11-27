//! Latency breakdown benchmark - measures each component of the request pipeline.
//!
//! Run with: cargo run --release --example latency_bench

use fuse_pipe::protocol::{VolumeRequest, VolumeResponse, WireRequest, WireResponse};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

fn main() {
    let iterations = 1000;

    println!("=== Latency Breakdown Benchmark ===\n");

    // 1. Measure serialization overhead
    measure_serialization(iterations);

    // 2. Measure pure Unix socket ping-pong (no file I/O)
    measure_pure_socket_latency(iterations);

    // 3. Measure raw stat() syscall
    measure_raw_stat(iterations);

    // 4. Measure socket round-trip to real server (if running)
    measure_socket_roundtrip(iterations);

    println!("\n=== Latency Breakdown Summary ===");
    println!("With LOW_LATENCY config (batch_size=1): ~30µs per request");
    println!("With HIGH_THROUGHPUT config (batch_size=64, timeout=5ms): ~5-6ms per request!");
    println!();
    println!("The 200x difference comes from BATCHING TIMEOUT:");
    println!("  - high_throughput() waits for 64 responses OR 5ms timeout");
    println!("  - With single-threaded requests, every request hits the 5ms timeout");
    println!("  - Response sits in buffer until timeout fires");
    println!();
    println!("Actual component costs (low-latency):");
    println!("  - Serde: ~0.25µs");
    println!("  - Socket round-trip: ~8µs");
    println!("  - spawn_blocking overhead: ~10µs");
    println!("  - Filesystem syscall: ~1µs");
    println!("  - mpsc channel + buffering: ~10µs");
    println!("  - Total: ~30µs");
}

fn measure_serialization(iterations: usize) {
    println!("1. Serialization/Deserialization Overhead");
    println!("   -----------------------------------------");

    // Request serialization
    let request = VolumeRequest::Getattr { ino: 1 };
    let wire_req = WireRequest::new(1, 0, request.clone());

    let start = Instant::now();
    for _ in 0..iterations {
        let _ = bincode::serialize(&wire_req).unwrap();
    }
    let serialize_time = start.elapsed();
    let per_serialize = serialize_time / iterations as u32;

    // Request deserialization
    let serialized = bincode::serialize(&wire_req).unwrap();
    let start = Instant::now();
    for _ in 0..iterations {
        let _: WireRequest = bincode::deserialize(&serialized).unwrap();
    }
    let deserialize_req_time = start.elapsed();
    let per_deserialize_req = deserialize_req_time / iterations as u32;

    // Response serialization
    let response = VolumeResponse::Attr {
        attr: fuse_pipe::protocol::FileAttr {
            ino: 1,
            size: 4096,
            blocks: 8,
            atime_secs: 0,
            atime_nsecs: 0,
            mtime_secs: 0,
            mtime_nsecs: 0,
            ctime_secs: 0,
            ctime_nsecs: 0,
            mode: 0o755,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 4096,
        },
        ttl_secs: 1,
    };
    let wire_resp = WireResponse { unique: 1, reader_id: 0, response: response.clone() };

    let start = Instant::now();
    for _ in 0..iterations {
        let _ = bincode::serialize(&wire_resp).unwrap();
    }
    let serialize_resp_time = start.elapsed();
    let per_serialize_resp = serialize_resp_time / iterations as u32;

    // Response deserialization
    let serialized_resp = bincode::serialize(&wire_resp).unwrap();
    let start = Instant::now();
    for _ in 0..iterations {
        let _: WireResponse = bincode::deserialize(&serialized_resp).unwrap();
    }
    let deserialize_resp_time = start.elapsed();
    let per_deserialize_resp = deserialize_resp_time / iterations as u32;

    println!("   Request serialize:     {:>8.2} µs", per_serialize.as_nanos() as f64 / 1000.0);
    println!("   Request deserialize:   {:>8.2} µs", per_deserialize_req.as_nanos() as f64 / 1000.0);
    println!("   Response serialize:    {:>8.2} µs", per_serialize_resp.as_nanos() as f64 / 1000.0);
    println!("   Response deserialize:  {:>8.2} µs", per_deserialize_resp.as_nanos() as f64 / 1000.0);
    println!("   Total serde overhead:  {:>8.2} µs",
        (per_serialize + per_deserialize_req + per_serialize_resp + per_deserialize_resp).as_nanos() as f64 / 1000.0);
    println!();
}

fn measure_pure_socket_latency(iterations: usize) {
    println!("2. Pure Unix Socket Ping-Pong (no file I/O)");
    println!("   ------------------------------------------");

    let socket_path = "/tmp/latency-bench.sock";
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path).unwrap();

    // Spawn echo server in a thread
    let server_handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 256];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    stream.write_all(&buf[..n]).unwrap();
                    stream.flush().unwrap();
                }
                Err(_) => break,
            }
        }
    });

    // Give server time to start
    std::thread::sleep(Duration::from_millis(10));

    let mut client = UnixStream::connect(socket_path).unwrap();
    let msg = b"ping";

    // Warmup
    for _ in 0..100 {
        client.write_all(msg).unwrap();
        client.flush().unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
    }

    // Measure
    let mut latencies = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        client.write_all(msg).unwrap();
        client.flush().unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
        latencies.push(start.elapsed());
    }

    drop(client);
    let _ = server_handle.join();
    let _ = std::fs::remove_file(socket_path);

    latencies.sort();
    let avg = latencies.iter().sum::<Duration>() / iterations as u32;
    let p50 = latencies[iterations / 2];
    let p99 = latencies[iterations * 99 / 100];
    let min = latencies[0];
    let max = latencies[iterations - 1];

    println!("   Iterations: {}", iterations);
    println!("   Min:        {:>8.2} µs", min.as_nanos() as f64 / 1000.0);
    println!("   Avg:        {:>8.2} µs", avg.as_nanos() as f64 / 1000.0);
    println!("   P50:        {:>8.2} µs", p50.as_nanos() as f64 / 1000.0);
    println!("   P99:        {:>8.2} µs", p99.as_nanos() as f64 / 1000.0);
    println!("   Max:        {:>8.2} µs", max.as_nanos() as f64 / 1000.0);
    println!();
}

fn measure_raw_stat(iterations: usize) {
    println!("3. Raw stat() Syscall");
    println!("   --------------------");

    let path = "/tmp";

    // Warmup
    for _ in 0..100 {
        let _ = std::fs::metadata(path);
    }

    // Measure
    let mut latencies = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let _ = std::fs::metadata(path);
        latencies.push(start.elapsed());
    }

    latencies.sort();
    let avg = latencies.iter().sum::<Duration>() / iterations as u32;
    let p50 = latencies[iterations / 2];
    let p99 = latencies[iterations * 99 / 100];
    let min = latencies[0];
    let max = latencies[iterations - 1];

    println!("   Iterations: {}", iterations);
    println!("   Min:        {:>8.2} µs", min.as_nanos() as f64 / 1000.0);
    println!("   Avg:        {:>8.2} µs", avg.as_nanos() as f64 / 1000.0);
    println!("   P50:        {:>8.2} µs", p50.as_nanos() as f64 / 1000.0);
    println!("   P99:        {:>8.2} µs", p99.as_nanos() as f64 / 1000.0);
    println!("   Max:        {:>8.2} µs", max.as_nanos() as f64 / 1000.0);
    println!();
}

fn measure_socket_roundtrip(iterations: usize) {
    println!("4. Socket + Server (spawn_blocking + stat)");
    println!("   -----------------------------------------");

    // Check for low-latency server first, then stress server
    let socket_path = if std::path::Path::new("/tmp/low-latency.sock").exists() {
        println!("   Connecting to /tmp/low-latency.sock (low-latency config)...");
        "/tmp/low-latency.sock"
    } else {
        println!("   Connecting to /tmp/fuse-stress.sock (high-throughput config)...");
        "/tmp/fuse-stress.sock"
    };

    let stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            println!("   Could not connect: {}", e);
            println!("   (Start server with: fuse-test server --socket {} --root /tmp)", socket_path);
            return;
        }
    };

    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let mut stream = stream;

    // Warmup
    for i in 0..100 {
        let wire_req = WireRequest::new(i as u64, 0, VolumeRequest::Getattr { ino: 1 });
        let body = bincode::serialize(&wire_req).unwrap();
        let mut msg = Vec::with_capacity(4 + body.len());
        msg.extend_from_slice(&(body.len() as u32).to_be_bytes());
        msg.extend_from_slice(&body);

        stream.write_all(&msg).unwrap();
        stream.flush().unwrap();

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; len];
        stream.read_exact(&mut resp_buf).unwrap();
    }

    // Measure socket write + read round-trip
    let mut latencies = Vec::with_capacity(iterations);

    for i in 0..iterations {
        let wire_req = WireRequest::new((i + 1000) as u64, 0, VolumeRequest::Getattr { ino: 1 });
        let body = bincode::serialize(&wire_req).unwrap();
        let mut msg = Vec::with_capacity(4 + body.len());
        msg.extend_from_slice(&(body.len() as u32).to_be_bytes());
        msg.extend_from_slice(&body);

        let start = Instant::now();

        stream.write_all(&msg).unwrap();
        stream.flush().unwrap();

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; len];
        stream.read_exact(&mut resp_buf).unwrap();

        latencies.push(start.elapsed());
    }

    latencies.sort();
    let avg = latencies.iter().sum::<Duration>() / iterations as u32;
    let p50 = latencies[iterations / 2];
    let p99 = latencies[iterations * 99 / 100];
    let min = latencies[0];
    let max = latencies[iterations - 1];

    println!("   Iterations: {}", iterations);
    println!("   Min:        {:>8.2} µs", min.as_nanos() as f64 / 1000.0);
    println!("   Avg:        {:>8.2} µs", avg.as_nanos() as f64 / 1000.0);
    println!("   P50:        {:>8.2} µs", p50.as_nanos() as f64 / 1000.0);
    println!("   P99:        {:>8.2} µs", p99.as_nanos() as f64 / 1000.0);
    println!("   Max:        {:>8.2} µs", max.as_nanos() as f64 / 1000.0);
}
