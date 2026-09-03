//! UDP echo service with XOR-fold checksum validation.
//!
//! Wire format: `<payload-bytes...><checksum:4 BE u32>`. The checksum is
//! computed over the leading payload bytes by XOR-folding into a `u32`:
//!
//! ```text
//! acc = 0
//! for byte b in leading_bytes:
//!     acc = acc.rotate_left(8) ^ (b as u32)
//! ```
//!
//! On every datagram we increment `udp_packets_received`. If the trailing
//! 4 bytes match the computed fold we increment `udp_valid_packets`,
//! otherwise `udp_checksum_errors`. Either way we echo the full payload
//! back unchanged. Datagrams shorter than 4 bytes are echoed without
//! contributing to the valid/error counters — there's no checksum to
//! validate against.

use std::net::SocketAddr;

use anyhow::Result;
use tokio::net::UdpSocket;

use crate::counters::Counters;

/// Generous UDP receive buffer — enough for a jumbo frame plus padding.
/// 65 KiB matches the maximum theoretical UDP payload.
const RECV_BUF: usize = 65 * 1024;

pub async fn serve(addr: SocketAddr, counters: Counters) -> Result<()> {
    let socket = UdpSocket::bind(addr).await?;
    let bound = socket.local_addr()?;
    tracing::info!(addr = %bound, "udp echo listening");

    let mut buf = vec![0u8; RECV_BUF];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "udp recv_from error");
                continue;
            }
        };
        counters.inc_udp_packets_received();

        if n >= 4 {
            let payload = &buf[..n - 4];
            let trailer = &buf[n - 4..n];
            // SAFETY: trailer is exactly 4 bytes by slice arithmetic above.
            let expected = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
            let actual = xor_fold(payload);
            if expected == actual {
                counters.inc_udp_valid_packets();
            } else {
                counters.inc_udp_checksum_errors();
            }
        }

        if let Err(e) = socket.send_to(&buf[..n], peer).await {
            tracing::debug!(?peer, error = %e, "udp send_to error");
        }
    }
}

/// XOR-fold a byte slice into a u32 by rotating the accumulator one byte
/// left for each input byte and XOR-ing the byte into the low 8 bits.
pub(crate) fn xor_fold(payload: &[u8]) -> u32 {
    let mut acc: u32 = 0;
    for &b in payload {
        acc = acc.rotate_left(8) ^ (b as u32);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_folds_to_zero() {
        assert_eq!(xor_fold(&[]), 0);
    }

    #[test]
    fn single_byte_lands_in_low_byte() {
        // rotate_left(8) on 0 stays 0, then XOR with 0xab → 0x0000_00ab.
        assert_eq!(xor_fold(&[0xab]), 0x0000_00ab);
    }

    #[test]
    fn four_distinct_bytes_pack_into_u32() {
        // [a, b, c, d] →
        //   acc=0; rot8 → 0;     ^a → 0x0000_00a
        //   rot8 → 0xa00;        ^b → 0x0000_a00 | b
        //   rot8 → 0xa0_b00;     ^c → 0x000a_b00 | c → 0x000a_bc
        // After 4 bytes you get 0xab_cd_ef_01-style packing: each byte
        // ends up in its own slot, last byte at the bottom.
        assert_eq!(xor_fold(&[0xab, 0xcd, 0xef, 0x01]), 0xabcd_ef01);
    }

    #[test]
    fn fifth_byte_wraps_back_into_high_byte() {
        // After 4 bytes the high byte has b0; rot8 cycles b0 back to bits
        // [7..0], then XOR with 5th byte folds them together.
        let packed4 = xor_fold(&[0xab, 0xcd, 0xef, 0x01]);
        let with5 = xor_fold(&[0xab, 0xcd, 0xef, 0x01, 0x10]);
        // The high byte after rotation should have been 0xab, XOR'd with
        // 0x10 in the low slot → 0xbb. The middle bytes shift up.
        assert_eq!(with5, packed4.rotate_left(8) ^ 0x10);
    }
}
