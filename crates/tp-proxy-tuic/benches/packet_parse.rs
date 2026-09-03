use bytes::{BufMut, BytesMut};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

fn packet(payload_len: usize) -> bytes::Bytes {
    let mut buf = BytesMut::with_capacity(17 + payload_len);
    buf.put_u8(0x05);
    buf.put_u8(0x02);
    buf.put_u16(7);
    buf.put_u16(42);
    buf.put_u8(1);
    buf.put_u8(0);
    buf.put_u16(payload_len as u16);
    buf.put_u8(0x01);
    buf.put_slice(&[127, 0, 0, 1]);
    buf.put_u16(53);
    buf.put_slice(&vec![0xCD; payload_len]);
    buf.freeze()
}

fn bench_packet_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("tuic_packet_parse");
    for size in [64usize, 600, 1_200] {
        let pkt = packet(size);
        group.throughput(Throughput::Bytes(pkt.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &pkt, |b, pkt| {
            b.iter(|| tp_proxy_tuic::parse_packet_for_bench(black_box(pkt.clone())).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, bench_packet_parse);
criterion_main!(benches);
