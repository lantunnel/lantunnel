//! Phase 3 throughput test runner.
//!
//! Connects to an already-running Lantunnel Client's loopback-only SOCKS5
//! listener and always uses SOCKS5 NO AUTHENTICATION.
//!
//! Per-test plan-spec defaults:
//!   * tcp_large_download      → 1 GiB download, ≥500 Mbps loopback
//!   * udp_burst               → 1000 clients × 30 s
//!   * udp_streaming_game      → 50/1/1 Mbps × 120 s
//!   * udp_stress_multi_stream → 12 streams × 100 MiB each
//!   * tcp_half_close          → 4 KiB shutdown(write)
//!
//! Every test takes a `--bytes` / `--duration` / `--clients` /
//! `--streams` knob to shrink the run for dev iteration. Defaults
//! always equal plan-spec values so a no-flag invocation produces the
//! asserted-correctness run.

use anyhow::{anyhow, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use tp_e2e_p3_throughput::tests;

#[derive(Parser, Debug)]
#[command(name = "tp-e2e-p3", about = "Phase 3 throughput tests")]
struct Args {
    /// Test name. One of:
    ///   tcp_large_download | udp_burst | udp_streaming_game |
    ///   udp_stress_multi_stream | tcp_half_close
    #[arg(long)]
    test: String,

    /// Loopback-only Lantunnel Client SOCKS5 address.
    #[arg(long, default_value = "127.0.0.1:11080")]
    proxy: String,

    /// TCP large-download target (echo-services :18998).
    #[arg(long, default_value = "127.0.0.1:18998")]
    tcp_target: String,

    /// UDP echo target (echo-services :18997).
    #[arg(long, default_value = "127.0.0.1:18997")]
    udp_target: String,

    /// HTTP echo target (used as `tcp_echo_target` for the streaming-game
    /// TCP control channel — the HTTP echo server's POST/echo loop is
    /// the closest thing to a generic TCP echo we have without adding
    /// a new echo-services endpoint).
    #[arg(long, default_value = "127.0.0.1:18999")]
    http_target: String,

    /// `tcp_large_download`: bytes to request. Plan-spec default 1 GiB.
    /// `tcp_half_close`: bytes to send before shutdown(write). Default 4 KiB.
    #[arg(long, default_value_t = 0u64)]
    bytes: u64,

    /// `tcp_large_download`: minimum Mbps. The historical loopback default is
    /// 500; pass 0 when collecting a non-loopback trend without asserting a
    /// hardware-specific floor.
    #[arg(long, default_value_t = tests::tcp_large_download::MBPS_TARGET)]
    min_mbps: f64,

    /// `udp_burst`: concurrent client count. Plan-spec default 1000.
    #[arg(long, default_value_t = tests::udp_burst::DEFAULT_CLIENTS)]
    clients: u32,

    /// `udp_burst` / `udp_streaming_game`: duration in seconds.
    /// Plan-spec defaults: 30 (burst), 120 (streaming-game).
    #[arg(long, default_value_t = 0u64)]
    duration: u64,

    /// `udp_stress_multi_stream`: number of parallel streams.
    /// Plan-spec default 12.
    #[arg(long, default_value_t = tests::udp_stress_multi_stream::DEFAULT_STREAMS)]
    streams: u32,

    /// `udp_stress_multi_stream`: bytes per stream. Plan-spec default
    /// 100 MiB (× 12 streams = 1.2 GiB total).
    #[arg(long, default_value_t = tests::udp_stress_multi_stream::DEFAULT_BYTES_PER_STREAM)]
    bytes_per_stream: u64,

    /// JSON metric report output path. Empty string uses
    /// `throughput_<test>.json`.
    #[arg(long, default_value = "")]
    out: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();

    let args = Args::parse();
    let out = if args.out.is_empty() {
        format!("throughput_{}.json", args.test)
    } else {
        args.out.clone()
    };

    tracing::info!(test = %args.test, out = %out, "run start");

    let result = match args.test.as_str() {
        "tcp_large_download" => {
            let bytes = pick_default(args.bytes, tests::tcp_large_download::DEFAULT_BYTES);
            tests::tcp_large_download::run(tests::tcp_large_download::Args {
                proxy: &args.proxy,
                tcp_target: &args.tcp_target,
                bytes,
                min_mbps: args.min_mbps,
                out: &out,
            })
            .await
        }
        "udp_burst" => {
            let duration = pick_default(args.duration, tests::udp_burst::DEFAULT_DURATION_SECS);
            tests::udp_burst::run(tests::udp_burst::Args {
                proxy: &args.proxy,
                udp_target: &args.udp_target,
                clients: args.clients,
                duration_secs: duration,
                out: &out,
            })
            .await
        }
        "udp_streaming_game" => {
            let duration = pick_default(
                args.duration,
                tests::udp_streaming_game::DEFAULT_DURATION_SECS,
            );
            tests::udp_streaming_game::run(tests::udp_streaming_game::Args {
                proxy: &args.proxy,
                udp_target: &args.udp_target,
                tcp_echo_target: &args.http_target,
                duration_secs: duration,
                out: &out,
            })
            .await
        }
        "udp_stress_multi_stream" => {
            tests::udp_stress_multi_stream::run(tests::udp_stress_multi_stream::Args {
                proxy: &args.proxy,
                tcp_target: &args.tcp_target,
                streams: args.streams,
                bytes_per_stream: args.bytes_per_stream,
                out: &out,
            })
            .await
        }
        "tcp_half_close" => {
            let bytes = pick_default(args.bytes, tests::tcp_half_close::DEFAULT_BYTES);
            tests::tcp_half_close::run(tests::tcp_half_close::Args {
                proxy: &args.proxy,
                bytes,
                out: &out,
            })
            .await
        }
        other => Err(anyhow!(
            "unknown test {other:?} (known: tcp_large_download, udp_burst, \
             udp_streaming_game, udp_stress_multi_stream, tcp_half_close)"
        )),
    };

    if let Err(e) = &result {
        tracing::error!(test = %args.test, err = %e, "FAIL");
    }
    result
}

/// Treat `0` as "use the per-test plan-spec default". Lets a single
/// `--bytes` / `--duration` flag drive every test that needs it without
/// each test having to publish its own knob.
fn pick_default<T: PartialEq + Default + Copy>(provided: T, plan: T) -> T {
    if provided == T::default() {
        plan
    } else {
        provided
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests_main {
    use super::*;

    #[test]
    fn pick_default_zero_falls_back() {
        assert_eq!(pick_default::<u64>(0, 100), 100);
        assert_eq!(pick_default::<u64>(7, 100), 7);
    }
}
