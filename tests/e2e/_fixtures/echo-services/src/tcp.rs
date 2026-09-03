//! TCP large-download server.
//!
//! Protocol (one request per connection):
//!
//! ```text
//! C → S:  GET /<bytes>\n
//! S → C:  HTTP/1.1 200 OK\r\n
//!         Content-Length: <bytes>\r\n
//!         X-SHA256: <hex>\r\n
//!         \r\n
//!         <bytes-of-deterministic-stream>
//! ```
//!
//! The stream is deterministic — `byte i = ((i.wrapping_mul(0x9e3779b9))
//! ^ ((bytes >> 8) as u32)) as u8` — so the client can verify the SHA-256
//! supplied in the header without comparing every byte.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::counters::Counters;

/// Stream chunk size when writing the body. 64 KiB is small enough to keep
/// memory bounded during multi-MiB downloads but big enough that the
/// per-syscall overhead is negligible.
const CHUNK_SIZE: usize = 64 * 1024;

/// Refuse downloads larger than this. 1 GiB is far more than any test should
/// realistically request and protects against a malformed `GET /<huge>\n`.
const MAX_BYTES: u64 = 1024 * 1024 * 1024;

pub async fn serve(addr: SocketAddr, counters: Counters) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(addr = %bound, "tcp large-download listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        counters.inc_tcp_connections();
        let counters = counters.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, counters).await {
                tracing::debug!(?peer, error = %e, "tcp connection closed with error");
            }
        });
    }
}

async fn handle(stream: TcpStream, counters: Counters) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Read first line — at most a small request line.
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .context("read request line")?;
    if n == 0 {
        return Ok(());
    }

    let bytes = match parse_request_line(line.trim_end_matches(['\r', '\n'])) {
        Some(n) if n <= MAX_BYTES => n,
        _ => {
            // Malformed or oversized — return 400 and close.
            let _ = write_half
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            return Ok(());
        }
    };

    // Compute SHA-256 of the deterministic stream up front so we can put it
    // in the response header. Streaming the same bytes twice (once to the
    // hasher, once to the wire) keeps memory bounded — we never buffer the
    // full body.
    let digest = compute_digest(bytes);

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {bytes}\r\nX-SHA256: {hex}\r\n\r\n",
        hex = hex_lower(&digest),
    );
    write_half
        .write_all(header.as_bytes())
        .await
        .context("write response header")?;

    // Stream the body in CHUNK_SIZE pieces.
    let mut chunk = vec![0u8; CHUNK_SIZE];
    let mut written: u64 = 0;
    while written < bytes {
        let remaining = bytes - written;
        let take = remaining.min(CHUNK_SIZE as u64) as usize;
        fill_stream_chunk(&mut chunk[..take], written, bytes);
        write_half
            .write_all(&chunk[..take])
            .await
            .context("write body chunk")?;
        written += take as u64;
    }
    write_half.flush().await.ok();
    counters.add_tcp_bytes_out(written);
    Ok(())
}

/// Parse `GET /<integer>` (the trailing newline is already stripped).
fn parse_request_line(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("GET /")?;
    rest.parse::<u64>().ok()
}

/// Deterministic byte at offset `i` for a download of `total` bytes.
///
/// Mixing in `total` means two requests of different sizes produce different
/// content even at the same offset, which is harmless but makes accidental
/// caching/replay easier to spot.
fn stream_byte(i: u64, total: u64) -> u8 {
    let mixed = (i as u32).wrapping_mul(0x9e37_79b9) ^ ((total >> 8) as u32);
    mixed as u8
}

fn fill_stream_chunk(buf: &mut [u8], offset: u64, total: u64) {
    for (k, slot) in buf.iter_mut().enumerate() {
        *slot = stream_byte(offset + k as u64, total);
    }
}

fn compute_digest(total: u64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; CHUNK_SIZE];
    let mut written: u64 = 0;
    while written < total {
        let remaining = total - written;
        let take = remaining.min(CHUNK_SIZE as u64) as usize;
        fill_stream_chunk(&mut chunk[..take], written, total);
        hasher.update(&chunk[..take]);
        written += take as u64;
    }
    hasher.finalize().into()
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_request_line() {
        assert_eq!(parse_request_line("GET /1024"), Some(1024));
        assert_eq!(parse_request_line("GET /0"), Some(0));
    }

    #[test]
    fn rejects_malformed_request_line() {
        assert_eq!(parse_request_line("POST /1024"), None);
        assert_eq!(parse_request_line("GET 1024"), None);
        assert_eq!(parse_request_line("GET /-1"), None);
        assert_eq!(parse_request_line("GET /abc"), None);
    }

    #[test]
    fn stream_byte_is_deterministic() {
        // Same (i, total) → same byte.
        assert_eq!(stream_byte(42, 1024), stream_byte(42, 1024));
        // Different offset, same total → typically different byte. The
        // multiplier 0x9e3779b9 is odd, so consecutive offsets differ in
        // the low byte by an odd number, which never folds to zero.
        assert_ne!(stream_byte(42, 1024), stream_byte(43, 1024));
    }

    #[test]
    fn digest_matches_streamed_bytes() {
        let total = 4096u64;
        let mut buf = vec![0u8; total as usize];
        fill_stream_chunk(&mut buf, 0, total);

        let mut h = Sha256::new();
        h.update(&buf);
        let direct: [u8; 32] = h.finalize().into();

        assert_eq!(direct, compute_digest(total));
    }
}
