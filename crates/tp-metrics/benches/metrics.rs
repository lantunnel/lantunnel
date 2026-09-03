use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;
use std::time::Duration;
use tp_metrics::MetricsManager;

fn seeded_metrics(clients: usize, conns: usize) -> std::sync::Arc<MetricsManager> {
    let m = MetricsManager::new();
    for i in 0..clients {
        m.update_client_heartbeat(&format!("client-{i}"), "group");
    }
    for i in 0..conns {
        let cid = format!("client-{}", i % clients.max(1));
        m.create_connection(&format!("conn-{i}"), &cid, "127.0.0.1:80");
    }
    m
}

fn bench_snapshots(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_snapshot");
    for (clients, conns) in [(1_000usize, 10_000usize), (10_000, 100_000)] {
        let m = seeded_metrics(clients, conns);
        group.bench_with_input(
            BenchmarkId::new("all_clients", format!("{clients}c_{conns}n")),
            &m,
            |b, m| b.iter(|| black_box(m.all_clients())),
        );
        group.bench_with_input(
            BenchmarkId::new("one_client_connections", format!("{clients}c_{conns}n")),
            &m,
            |b, m| b.iter(|| black_box(m.connections_for_client("client-0"))),
        );
    }
    group.finish();
}

fn bench_cleanup(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_cleanup");
    for (clients, conns) in [(1_000usize, 10_000usize), (10_000, 100_000)] {
        group.bench_with_input(
            BenchmarkId::new("no_expiry", format!("{clients}c_{conns}n")),
            &(clients, conns),
            |b, &(clients, conns)| {
                b.iter_batched(
                    || seeded_metrics(clients, conns),
                    |m| m.cleanup(Duration::from_secs(3600)),
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_snapshots, bench_cleanup);
criterion_main!(benches);
