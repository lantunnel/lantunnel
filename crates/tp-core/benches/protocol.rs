//! Baseline benchmarks for the binary wire protocol.
//!
//! Covers the two functions on the TCP / UDP hot paths —
//! `pack` (every outbound frame) and `unpack` (every inbound frame).
//! The `Data` variant is the highest-frequency case; `Heartbeat` is
//! the smallest, useful as a no-payload floor. Run with
//! `cargo bench -p tp-core`.

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use tp_core::protocol::{pack, unpack, unpack_bytes, BinaryMessage};

fn bench_pack(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack");

    // Heartbeat — minimal variant, no payload.
    group.bench_function("Heartbeat", |b| {
        let msg = BinaryMessage::Heartbeat {
            client_id: "client-bench".into(),
            timestamp: 1_700_000_000,
        };
        b.iter(|| pack(black_box(&msg)));
    });

    // Data — steady-state TCP pipe hot path. Sweep payload sizes so the
    // regression surface covers typical game/stream packet ranges.
    for size in [64usize, 1_400, 16 * 1024, 65 * 1024] {
        let payload = Bytes::from(vec![0xABu8; size]);
        let msg = BinaryMessage::Data {
            conn_id: "abcdefghijkl".into(),
            payload: payload.clone(),
        };
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("Data", size), &msg, |b, m| {
            b.iter(|| pack(black_box(m)));
        });
    }

    group.finish();
}

fn bench_unpack(c: &mut Criterion) {
    let mut group = c.benchmark_group("unpack");

    // Heartbeat round-trip: pack to bytes once, then measure unpack cost.
    let hb = BinaryMessage::Heartbeat {
        client_id: "client-bench".into(),
        timestamp: 1_700_000_000,
    };
    let hb_bytes = pack(&hb).to_bytes();
    group.bench_function("Heartbeat", |b| {
        b.iter(|| unpack(black_box(&hb_bytes)).expect("valid"))
    });

    for size in [64usize, 1_400, 16 * 1024, 65 * 1024] {
        let msg = BinaryMessage::Data {
            conn_id: "abcdefghijkl".into(),
            payload: Bytes::from(vec![0xABu8; size]),
        };
        let bytes = pack(&msg).to_bytes();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("Data", size), &bytes, |b, frame| {
            b.iter(|| unpack(black_box(frame)).expect("valid"));
        });
    }

    group.finish();
}

fn bench_unpack_bytes(c: &mut Criterion) {
    let mut group = c.benchmark_group("unpack_bytes");
    for size in [64usize, 1_400, 16 * 1024, 65 * 1024] {
        let msg = BinaryMessage::UdpData {
            conn_id: "abcdefghijkl".into(),
            payload: Bytes::from(vec![0xABu8; size]),
        };
        let bytes = pack(&msg).to_bytes();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("UdpData", size), &bytes, |b, frame| {
            b.iter(|| unpack_bytes(black_box(frame.clone())).expect("valid"));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_pack, bench_unpack, bench_unpack_bytes);
criterion_main!(benches);
