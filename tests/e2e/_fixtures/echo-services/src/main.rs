//! E2E echo services binary.
//!
//! Spawns three services bound to the same IP for E2E proxy tests:
//!
//! - HTTP on 18999 — echoes the request body back with a `#<seq> size=<n>\n`
//!   header line, plus a `GET /stats` endpoint that dumps the atomic counters.
//! - TCP on 18998 — large-download server. Reads `GET /<bytes>\n`, responds
//!   with deterministic bytes and a `X-SHA256: <hex>` header.
//! - UDP on 18997 — datagram echo with 4-byte trailing big-endian XOR-folded
//!   checksum validation.
//!
//! Run with:
//!   tp-e2e-echo-services [--bind 127.0.0.1] [--http-port 18999]
//!                        [--tcp-port 18998]  [--udp-port 18997]
//!
//! The fixture binary is intentionally minimal — full behaviour lives in the
//! per-service modules so integration tests can call the public `serve` fns
//! directly without spawning a child process.

use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use clap::Parser;
use tp_e2e_echo_services::{counters, http, tcp, udp};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "tp-e2e-echo-services",
    about = "HTTP/TCP/UDP echo fixture for Lantunnel E2E tests"
)]
struct Args {
    /// Bind IP for all three services. Defaults to loopback for safety.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,

    /// HTTP echo port.
    #[arg(long, default_value_t = 18999)]
    http_port: u16,

    /// TCP large-download port.
    #[arg(long, default_value_t = 18998)]
    tcp_port: u16,

    /// UDP echo port.
    #[arg(long, default_value_t = 18997)]
    udp_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let args = Args::parse();
    tracing::info!(?args, "starting echo services");

    let counters = counters::Counters::new();

    let http_addr = SocketAddr::new(args.bind, args.http_port);
    let tcp_addr = SocketAddr::new(args.bind, args.tcp_port);
    let udp_addr = SocketAddr::new(args.bind, args.udp_port);

    tokio::try_join!(
        http::serve(http_addr, counters.clone()),
        tcp::serve(tcp_addr, counters.clone()),
        udp::serve(udp_addr, counters.clone()),
    )?;

    Ok(())
}
