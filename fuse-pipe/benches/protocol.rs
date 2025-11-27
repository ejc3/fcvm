//! Protocol serialization benchmarks.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use fuse_pipe::{VolumeRequest, VolumeResponse, WireRequest, WireResponse};

fn bench_serialize_lookup_request(c: &mut Criterion) {
    let req = WireRequest::new(
        1,
        0,
        VolumeRequest::Lookup {
            parent: 1,
            name: "test.txt".to_string(),
        },
    );

    c.bench_function("serialize_lookup_request", |b| {
        b.iter(|| black_box(req.encode().unwrap()))
    });
}

fn bench_serialize_read_response_4kb(c: &mut Criterion) {
    let resp = WireResponse::new(
        1,
        0,
        VolumeResponse::Data {
            data: vec![0u8; 4096],
        },
    );

    let mut group = c.benchmark_group("serialize_read_response");
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("4kb", |b| {
        b.iter(|| black_box(resp.encode().unwrap()))
    });
    group.finish();
}

fn bench_serialize_read_response_64kb(c: &mut Criterion) {
    let resp = WireResponse::new(
        1,
        0,
        VolumeResponse::Data {
            data: vec![0u8; 65536],
        },
    );

    let mut group = c.benchmark_group("serialize_read_response");
    group.throughput(Throughput::Bytes(65536));
    group.bench_function("64kb", |b| {
        b.iter(|| black_box(resp.encode().unwrap()))
    });
    group.finish();
}

fn bench_roundtrip_wire_request(c: &mut Criterion) {
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

    c.bench_function("roundtrip_wire_request", |b| {
        b.iter(|| {
            let encoded = req.encode().unwrap();
            black_box(WireRequest::decode(&encoded[4..]).unwrap())
        })
    });
}

criterion_group!(
    benches,
    bench_serialize_lookup_request,
    bench_serialize_read_response_4kb,
    bench_serialize_read_response_64kb,
    bench_roundtrip_wire_request,
);

criterion_main!(benches);
