//! Protocol serialization benchmarks.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use fuse_pipe::{FileAttr, VolumeRequest, VolumeResponse, WireRequest, WireResponse};

fn bench_serialize_lookup_request(c: &mut Criterion) {
    let req = WireRequest::new(
        1,
        0,
        VolumeRequest::Lookup {
            parent: 1,
            name: b"test.txt".to_vec(),
            uid: 1000,
            gid: 1000,
            pid: 42,
        },
    );

    c.bench_function("serialize_lookup_request", |b| {
        b.iter(|| black_box(bincode::serialize(&req).unwrap()))
    });
}

fn bench_deserialize_lookup_request(c: &mut Criterion) {
    let req = WireRequest::new(
        1,
        0,
        VolumeRequest::Lookup {
            parent: 1,
            name: b"test.txt".to_vec(),
            uid: 1000,
            gid: 1000,
            pid: 42,
        },
    );
    let encoded = bincode::serialize(&req).unwrap();

    c.bench_function("deserialize_lookup_request", |b| {
        b.iter(|| black_box(bincode::deserialize::<WireRequest>(&encoded).unwrap()))
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
        b.iter(|| black_box(bincode::serialize(&resp).unwrap()))
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
        b.iter(|| black_box(bincode::serialize(&resp).unwrap()))
    });
    group.finish();
}

fn bench_serialize_read_response_128kb(c: &mut Criterion) {
    let resp = WireResponse::new(
        1,
        0,
        VolumeResponse::Data {
            data: vec![0u8; 131072],
        },
    );

    let mut group = c.benchmark_group("serialize_read_response");
    group.throughput(Throughput::Bytes(131072));
    group.bench_function("128kb", |b| {
        b.iter(|| black_box(bincode::serialize(&resp).unwrap()))
    });
    group.finish();
}

fn bench_serialize_attr_response(c: &mut Criterion) {
    let resp = WireResponse::new(
        1,
        0,
        VolumeResponse::Attr {
            attr: FileAttr::new(1),
            ttl_secs: 60,
        },
    );

    c.bench_function("serialize_attr_response", |b| {
        b.iter(|| black_box(bincode::serialize(&resp).unwrap()))
    });
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
            uid: 0,
            gid: 0,
            pid: 0,
        },
    );

    c.bench_function("roundtrip_wire_request", |b| {
        b.iter(|| {
            let encoded = bincode::serialize(&req).unwrap();
            black_box(bincode::deserialize::<WireRequest>(&encoded).unwrap())
        })
    });
}

fn bench_roundtrip_wire_response_data(c: &mut Criterion) {
    let resp = WireResponse::new(
        1,
        0,
        VolumeResponse::Data {
            data: vec![0u8; 4096],
        },
    );

    let mut group = c.benchmark_group("roundtrip_wire_response");
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("4kb", |b| {
        b.iter(|| {
            let encoded = bincode::serialize(&resp).unwrap();
            black_box(bincode::deserialize::<WireResponse>(&encoded).unwrap())
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_serialize_lookup_request,
    bench_deserialize_lookup_request,
    bench_serialize_read_response_4kb,
    bench_serialize_read_response_64kb,
    bench_serialize_read_response_128kb,
    bench_serialize_attr_response,
    bench_roundtrip_wire_request,
    bench_roundtrip_wire_response_data,
);

criterion_main!(benches);
