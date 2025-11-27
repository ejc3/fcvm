//! End-to-end throughput benchmarks.
//!
//! Measures actual request/response throughput over Unix sockets.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use fuse_pipe::{VolumeRequest, VolumeResponse, WireRequest, WireResponse};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;

/// Spawn a simple echo server that reads requests and sends responses.
fn spawn_echo_server(socket_path: &str) -> thread::JoinHandle<()> {
    let path = socket_path.to_string();
    thread::spawn(move || {
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (mut stream, _) = listener.accept().unwrap();

        let mut len_buf = [0u8; 4];
        loop {
            // Read request
            if stream.read_exact(&mut len_buf).is_err() {
                break;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut req_buf = vec![0u8; len];
            if stream.read_exact(&mut req_buf).is_err() {
                break;
            }

            // Parse and respond
            let wire_req: WireRequest = match bincode::deserialize(&req_buf) {
                Ok(r) => r,
                Err(_) => break,
            };

            // Create response based on request
            let response = match &wire_req.request {
                VolumeRequest::Read { size, .. } => VolumeResponse::Data {
                    data: vec![0u8; *size as usize],
                },
                VolumeRequest::Getattr { .. } => VolumeResponse::Attr {
                    attr: fuse_pipe::FileAttr::new(1),
                    ttl_secs: 60,
                },
                _ => VolumeResponse::Ok,
            };

            let wire_resp = WireResponse::new(wire_req.unique, wire_req.reader_id, response);
            let resp_buf = bincode::serialize(&wire_resp).unwrap();
            let resp_len = (resp_buf.len() as u32).to_be_bytes();

            if stream.write_all(&resp_len).is_err() || stream.write_all(&resp_buf).is_err() {
                break;
            }
        }
    })
}

fn bench_request_response_latency(c: &mut Criterion) {
    let socket_path = "/tmp/fuse-pipe-bench-latency.sock";
    let _ = std::fs::remove_file(socket_path);

    let handle = spawn_echo_server(socket_path);
    thread::sleep(std::time::Duration::from_millis(50));

    let mut stream = UnixStream::connect(socket_path).unwrap();

    let req = WireRequest::new(
        1,
        0,
        VolumeRequest::Getattr { ino: 1 },
    );

    c.bench_function("request_response_latency", |b| {
        b.iter(|| {
            let req_buf = bincode::serialize(&req).unwrap();
            let len_bytes = (req_buf.len() as u32).to_be_bytes();
            stream.write_all(&len_bytes).unwrap();
            stream.write_all(&req_buf).unwrap();

            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut resp_buf = vec![0u8; len];
            stream.read_exact(&mut resp_buf).unwrap();

            black_box(bincode::deserialize::<WireResponse>(&resp_buf).unwrap())
        })
    });

    drop(stream);
    let _ = std::fs::remove_file(socket_path);
    let _ = handle.join();
}

fn bench_read_throughput_4kb(c: &mut Criterion) {
    let socket_path = "/tmp/fuse-pipe-bench-read4k.sock";
    let _ = std::fs::remove_file(socket_path);

    let handle = spawn_echo_server(socket_path);
    thread::sleep(std::time::Duration::from_millis(50));

    let mut stream = UnixStream::connect(socket_path).unwrap();

    let req = WireRequest::new(
        1,
        0,
        VolumeRequest::Read {
            ino: 2,
            fh: 3,
            offset: 0,
            size: 4096,
        },
    );

    let mut group = c.benchmark_group("read_throughput");
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("4kb", |b| {
        b.iter(|| {
            let req_buf = bincode::serialize(&req).unwrap();
            let len_bytes = (req_buf.len() as u32).to_be_bytes();
            stream.write_all(&len_bytes).unwrap();
            stream.write_all(&req_buf).unwrap();

            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut resp_buf = vec![0u8; len];
            stream.read_exact(&mut resp_buf).unwrap();

            black_box(bincode::deserialize::<WireResponse>(&resp_buf).unwrap())
        })
    });
    group.finish();

    drop(stream);
    let _ = std::fs::remove_file(socket_path);
    let _ = handle.join();
}

fn bench_read_throughput_64kb(c: &mut Criterion) {
    let socket_path = "/tmp/fuse-pipe-bench-read64k.sock";
    let _ = std::fs::remove_file(socket_path);

    let handle = spawn_echo_server(socket_path);
    thread::sleep(std::time::Duration::from_millis(50));

    let mut stream = UnixStream::connect(socket_path).unwrap();

    let req = WireRequest::new(
        1,
        0,
        VolumeRequest::Read {
            ino: 2,
            fh: 3,
            offset: 0,
            size: 65536,
        },
    );

    let mut group = c.benchmark_group("read_throughput");
    group.throughput(Throughput::Bytes(65536));
    group.bench_function("64kb", |b| {
        b.iter(|| {
            let req_buf = bincode::serialize(&req).unwrap();
            let len_bytes = (req_buf.len() as u32).to_be_bytes();
            stream.write_all(&len_bytes).unwrap();
            stream.write_all(&req_buf).unwrap();

            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut resp_buf = vec![0u8; len];
            stream.read_exact(&mut resp_buf).unwrap();

            black_box(bincode::deserialize::<WireResponse>(&resp_buf).unwrap())
        })
    });
    group.finish();

    drop(stream);
    let _ = std::fs::remove_file(socket_path);
    let _ = handle.join();
}

criterion_group!(
    benches,
    bench_request_response_latency,
    bench_read_throughput_4kb,
    bench_read_throughput_64kb,
);

criterion_main!(benches);
