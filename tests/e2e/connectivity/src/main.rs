//! Stateless V2 connectivity probe runner.
//!
//! The probe connects to an already-running Lantunnel Client's loopback-only
//! SOCKS5 listener. That listener always uses SOCKS5 NO AUTHENTICATION.

use anyhow::{anyhow, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use tp_e2e_p1_connectivity::tests;

#[derive(Parser, Debug)]
#[command(name = "tp-e2e-p1", about = "Phase 1 connectivity tests")]
struct Args {
    /// Test name (currently: socks5_tcp_connect; more added as P1 tasks land).
    #[arg(long)]
    test: String,

    /// Loopback-only Lantunnel Client SOCKS5 address.
    #[arg(long, default_value = "127.0.0.1:11080")]
    proxy: String,

    /// Echo HTTP target (host:port). Default points at tp-e2e-echo-services.
    #[arg(long, default_value = "127.0.0.1:18999")]
    target: String,

    /// Echo UDP target (host:port). Default points at tp-e2e-echo-services UDP.
    /// Used by `socks5_udp_associate`.
    #[arg(long, default_value = "127.0.0.1:18997")]
    udp_target: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();

    let args = Args::parse();

    tracing::info!(test = %args.test, "run start");

    let result = match args.test.as_str() {
        "socks5_tcp_connect" => tests::socks5_tcp_connect::run(&args.proxy, &args.target).await,
        "socks5_udp_associate" => {
            tests::socks5_udp_associate::run(&args.proxy, &args.udp_target).await
        }
        other => Err(anyhow!(
            "unknown test {other:?} (known: socks5_tcp_connect, socks5_udp_associate)"
        )),
    };

    if let Err(e) = &result {
        tracing::error!(test = %args.test, err = %e, "FAIL");
    }
    result
}
