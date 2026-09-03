//! TUIC Packet fragmentation / reassembly.
//!
//! Split out of `lib.rs`. Owns the
//! encoder (`build_packet_fragments` + `build_single`), the per-connection
//! `FragAssembler`, and the exhaustive fragmentation/TTL test suites.

use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use dashmap::DashMap;

use crate::addr::{encode_addr_bytes, format_addr, Addr, ADDR_NONE};
use crate::{CMD_PACKET, TUIC_VER};

/// TUIC Packet fixed header length (VER, CMD, ASSOC_ID, PKT_ID, FRAG_TOTAL,
/// FRAG_ID, SIZE) — ADDR follows.
pub(crate) const PACKET_FIXED_HDR: usize = 1 + 1 + 2 + 2 + 1 + 1 + 2;

const FRAG_TTL: Duration = Duration::from_secs(30);

struct FragBuffer {
    parts: Vec<Option<Bytes>>,
    target: Option<String>,
    received: u8,
    first_seen: Instant,
}

/// Per-connection assembler for multi-fragment TUIC Packet messages.
/// Only the first fragment carries the target address; successors may carry
/// `ADDR_NONE`, in which case we fall back to the cached target.
pub(crate) struct FragAssembler {
    buffers: DashMap<(u16, u16), FragBuffer>,
}

impl FragAssembler {
    pub(crate) fn new() -> Self {
        Self {
            buffers: DashMap::new(),
        }
    }

    /// Returns `Some((target, full_payload))` once the final fragment arrives.
    pub(crate) fn accept(
        &self,
        assoc_id: u16,
        pkt_id: u16,
        frag_id: u8,
        frag_total: u8,
        target: Addr,
        payload: Bytes,
    ) -> Option<(String, Bytes)> {
        self.gc_if_full();
        let key = (assoc_id, pkt_id);
        let target_str = match target {
            Addr::None => None,
            ref a => Some(format_addr(a)),
        };
        let complete = {
            let mut entry = self.buffers.entry(key).or_insert_with(|| FragBuffer {
                parts: vec![None; frag_total as usize],
                target: None,
                received: 0,
                first_seen: Instant::now(),
            });
            // Accept only if the declared total matches what we already had.
            if entry.parts.len() != frag_total as usize {
                return None;
            }
            if entry.parts[frag_id as usize].is_some() {
                // Duplicate fragment — ignore.
                return None;
            }
            entry.parts[frag_id as usize] = Some(payload);
            entry.received += 1;
            if entry.target.is_none() {
                entry.target = target_str;
            }
            entry.received == frag_total
        };
        if !complete {
            return None;
        }
        let (_, buf) = self.buffers.remove(&key)?;
        let total_len: usize = buf
            .parts
            .iter()
            .map(|p| p.as_ref().map(|b| b.len()).unwrap_or(0))
            .sum();
        let mut out = BytesMut::with_capacity(total_len);
        for part in buf.parts.into_iter().flatten() {
            out.put_slice(&part);
        }
        let target = buf.target.unwrap_or_else(|| "0.0.0.0:0".into());
        Some((target, out.freeze()))
    }

    /// Evict stale buffers. Runs inline before each insert — keeps the
    /// assembler honest without a dedicated background task.
    ///
    /// Previous implementation returned early when `buffers.len() < 1024`,
    /// so TTL only kicked in once the store was already saturated; long
    /// quiet-flow connections accumulated stale incomplete packets forever.
    /// Memory growth is still bounded because a flood of non-completing
    /// fragments is dropped by TTL after FRAG_TTL; under attack the ceiling
    /// is (arrival-rate × FRAG_TTL × max-payload).
    fn gc_if_full(&self) {
        self.gc_at(Instant::now());
    }

    /// TTL sweep with an injected clock; separated out purely so tests can
    /// simulate time passing without sleeping.
    fn gc_at(&self, now: Instant) {
        let cutoff = match now.checked_sub(FRAG_TTL) {
            Some(c) => c,
            None => return,
        };
        self.buffers.retain(|_, v| v.first_seen >= cutoff);
    }
}

/// Split a UDP response payload into one-or-more TUIC Packet messages that
/// each fit under `max_dg`. Returns an empty `Vec` when even a single
/// fragment cannot hold one byte of payload, or when the split would exceed
/// `u8::MAX` fragments (TUIC v5's `frag_total` is u8).
///
/// Per the TUIC v5 spec, only the first fragment carries the real ADDR;
/// subsequent fragments carry `ADDR_NONE` (0xFF).
pub(crate) fn build_packet_fragments(
    assoc_id: u16,
    pkt_id: u16,
    from: &str,
    payload: &[u8],
    max_dg: usize,
) -> Vec<Bytes> {
    let addr_first = encode_addr_bytes(from);
    // Subsequent fragments: single ADDR_NONE byte.
    let addr_rest_len = 1usize;

    // Single-fragment fast path — preserves the old zero-alloc behavior.
    let first_total = PACKET_FIXED_HDR + addr_first.len() + payload.len();
    if first_total <= max_dg {
        return vec![build_single(
            assoc_id,
            pkt_id,
            1,
            0,
            Some(&addr_first),
            payload,
        )];
    }

    // Capacity checks for at least one byte of payload per fragment.
    if max_dg <= PACKET_FIXED_HDR + addr_first.len() || max_dg <= PACKET_FIXED_HDR + addr_rest_len {
        return Vec::new();
    }
    let first_chunk_max = max_dg - PACKET_FIXED_HDR - addr_first.len();
    let rest_chunk_max = max_dg - PACKET_FIXED_HDR - addr_rest_len;

    // Compute how many total fragments we need.
    let mut remaining = payload.len().saturating_sub(first_chunk_max);
    let rest_fragments = remaining.div_ceil(rest_chunk_max);
    let frag_total_usize = 1 + rest_fragments;
    if frag_total_usize > u8::MAX as usize {
        return Vec::new();
    }
    let frag_total = frag_total_usize as u8;

    let mut out = Vec::with_capacity(frag_total_usize);

    // First fragment carries the full ADDR.
    let first_end = first_chunk_max.min(payload.len());
    out.push(build_single(
        assoc_id,
        pkt_id,
        frag_total,
        0,
        Some(&addr_first),
        &payload[..first_end],
    ));

    // Subsequent fragments — ADDR_NONE.
    let mut cursor = first_end;
    let mut frag_id: u8 = 1;
    while cursor < payload.len() {
        let end = (cursor + rest_chunk_max).min(payload.len());
        out.push(build_single(
            assoc_id,
            pkt_id,
            frag_total,
            frag_id,
            None,
            &payload[cursor..end],
        ));
        cursor = end;
        frag_id = frag_id.saturating_add(1);
        remaining = remaining.saturating_sub(rest_chunk_max);
        if remaining == 0 {
            break;
        }
    }
    out
}

/// Build a single TUIC Packet message. When `addr_full` is `None` the
/// fragment uses `ADDR_NONE` (0xFF) — per spec, only the first fragment
/// carries a concrete address.
fn build_single(
    assoc_id: u16,
    pkt_id: u16,
    frag_total: u8,
    frag_id: u8,
    addr_full: Option<&[u8]>,
    payload: &[u8],
) -> Bytes {
    let addr_len = addr_full.map(|a| a.len()).unwrap_or(1);
    let mut out = BytesMut::with_capacity(PACKET_FIXED_HDR + addr_len + payload.len());
    out.put_u8(TUIC_VER);
    out.put_u8(CMD_PACKET);
    out.put_u16(assoc_id);
    out.put_u16(pkt_id);
    out.put_u8(frag_total);
    out.put_u8(frag_id);
    out.put_u16(payload.len() as u16);
    match addr_full {
        Some(bytes) => out.put_slice(bytes),
        None => out.put_u8(ADDR_NONE),
    }
    out.put_slice(payload);
    out.freeze()
}

#[cfg(test)]
mod tuic_fragmentation_tests {
    use super::*;

    #[test]
    fn fits_in_one_fragment() {
        let frags = build_packet_fragments(1, 1, "1.2.3.4:53", b"hello", 1200);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0][6], 1); // frag_total at offset 6
        assert_eq!(frags[0][7], 0); // frag_id
    }

    #[test]
    fn splits_oversize_into_multiple_fragments() {
        // Tight cap to force splitting.
        let payload = vec![0xAAu8; 3000];
        let frags = build_packet_fragments(1, 42, "1.2.3.4:53", &payload, 200);
        assert!(frags.len() > 1);
        let expected_total = frags[0][6];
        assert!(expected_total as usize >= 3);
        for (i, f) in frags.iter().enumerate() {
            assert_eq!(f[6], expected_total, "frag_total stable across fragments");
            assert_eq!(f[7], i as u8, "frag_id is 0-based ascending");
        }
        // Reassemble payload and compare.
        let mut reassembled = Vec::with_capacity(payload.len());
        for (i, f) in frags.iter().enumerate() {
            let size = u16::from_be_bytes([f[8], f[9]]) as usize;
            let addr_len = if i == 0 {
                // v4 addr: 1 (ATYP) + 4 (IPv4) + 2 (port) = 7
                7
            } else {
                1 // ADDR_NONE
            };
            let payload_start = PACKET_FIXED_HDR + addr_len;
            reassembled.extend_from_slice(&f[payload_start..payload_start + size]);
        }
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn near_1400_payload_splits_when_max_datagram_is_1200() {
        let payload = vec![0xAAu8; 1375];
        let frags = build_packet_fragments(1, 42, "1.2.3.4:53", &payload, 1200);
        assert_eq!(
            frags.len(),
            2,
            "1375B payload plus TUIC Packet header exceeds Quinn's default 1200B initial MTU"
        );
    }

    #[test]
    fn returns_empty_when_header_overflows_cap() {
        // Domain addr pushes header past cap.
        let frags =
            build_packet_fragments(1, 1, "a-very-long-host-name-that-wont-fit:53", b"x", 20);
        assert!(frags.is_empty());
    }
}

#[cfg(test)]
mod frag_assembler_tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn addr_v4() -> Addr {
        Addr::V4(Ipv4Addr::new(1, 2, 3, 4), 53)
    }

    #[test]
    fn single_fragment_completes_immediately() {
        let asm = FragAssembler::new();
        let out = asm.accept(1, 1, 0, 1, addr_v4(), Bytes::from_static(b"hello"));
        let (target, payload) = out.expect("one-frag accept should complete");
        assert_eq!(target, "1.2.3.4:53");
        assert_eq!(&payload[..], b"hello");
        assert_eq!(asm.buffers.len(), 0, "completed entry must be removed");
    }

    #[test]
    fn two_fragments_assemble_in_order() {
        let asm = FragAssembler::new();
        assert!(asm
            .accept(7, 9, 0, 2, addr_v4(), Bytes::from_static(b"AAA"))
            .is_none());
        let out = asm
            .accept(7, 9, 1, 2, Addr::None, Bytes::from_static(b"BB"))
            .expect("second frag should complete");
        assert_eq!(out.0, "1.2.3.4:53");
        assert_eq!(&out.1[..], b"AAABB");
    }

    /// Regression for the "TTL never runs" bug: a partial fragment that sits
    /// past FRAG_TTL must be evicted even when the buffer count is well
    /// below the old FRAG_MAX_BUFFERS cap. Previously `gc_if_full` returned
    /// early in that case and the entry leaked until the store reached 1024
    /// items.
    #[test]
    fn ttl_evicts_stale_partial_under_cap() {
        let asm = FragAssembler::new();
        assert!(asm
            .accept(1, 1, 0, 2, addr_v4(), Bytes::from_static(b"x"))
            .is_none());
        assert_eq!(asm.buffers.len(), 1);

        // Simulate FRAG_TTL + a bit passing.
        let future = Instant::now() + FRAG_TTL + Duration::from_secs(1);
        asm.gc_at(future);
        assert_eq!(asm.buffers.len(), 0, "stale partial must be evicted by TTL");
    }

    #[test]
    fn ttl_keeps_fresh_partial() {
        let asm = FragAssembler::new();
        assert!(asm
            .accept(1, 1, 0, 2, addr_v4(), Bytes::from_static(b"x"))
            .is_none());
        asm.gc_at(Instant::now());
        assert_eq!(asm.buffers.len(), 1, "fresh partial must not be evicted");
    }
}
