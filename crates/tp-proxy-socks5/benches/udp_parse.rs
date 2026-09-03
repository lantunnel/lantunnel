use bytes::{BufMut, BytesMut};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

fn udp_request(payload_len: usize) -> bytes::Bytes {
    let mut buf = BytesMut::with_capacity(10 + payload_len);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0x01);
    buf.put_slice(&[127, 0, 0, 1]);
    buf.put_u16(53);
    buf.put_slice(&vec![0xAB; payload_len]);
    buf.freeze()
}

fn bench_udp_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("socks5_udp_parse");
    for size in [64usize, 600, 1_400] {
        let packet = udp_request(size);
        group.throughput(Throughput::Bytes(packet.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &packet, |b, packet| {
            b.iter(|| tp_proxy_socks5::parse_udp_request_for_bench(black_box(packet)).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, bench_udp_parse);
criterion_main!(benches);
