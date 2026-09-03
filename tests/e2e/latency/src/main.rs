//! Phase 2 latency test runner.
//!
//! Connects to an already-running Lantunnel Client's loopback-only SOCKS5
//! listener and always uses SOCKS5 NO AUTHENTICATION.
//!
//! Per-test plan-spec defaults:
//!   * latency_baseline       → 5000 samples × 3 shapes
//!   * latency_gamestream_sim → 300 s real-time simulation
//!   * latency_stress_curve   → {20,50,100,200,500,1000} Mbps × 30 s
//!
//! Every test takes a `--samples`/`--duration`/`--iterations` knob to
//! shrink the run for dev iteration. Defaults always equal plan-spec
//! values so a no-flag invocation produces the asserted-correctness run.

use anyhow::{anyhow, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use tp_e2e_p2_latency::tests;

#[derive(Parser, Debug)]
#[command(name = "tp-e2e-p2", about = "Phase 2 latency tests")]
struct Args {
    /// Test name. One of:
    ///   latency_baseline | latency_gamestream_sim |
    ///   latency_stress_curve
    #[arg(long)]
    test: String,

    /// Loopback-only Lantunnel Client SOCKS5 address.
    #[arg(long, default_value = "127.0.0.1:11080")]
    proxy: String,

    /// HTTP echo target (used as the TCP-128B baseline shape & cold-start probe).
    #[arg(long, default_value = "127.0.0.1:18999")]
    target: String,

    /// UDP echo target (used as both UDP baseline shapes + gamestream-sim + stress-curve).
    #[arg(long, default_value = "127.0.0.1:18997")]
    udp_target: String,

    /// `latency_baseline`: per-shape sample count. Plan-spec default 5000.
    #[arg(long, default_value_t = 5000u32)]
    samples: u32,

    /// `latency_gamestream_sim`: simulation duration in seconds.
    /// Plan-spec default 300 (5 min real-time game session).
    #[arg(long, default_value_t = 300u64)]
    duration: u64,

    /// `latency_gamestream_sim`: merged-stream P50 latency budget in microseconds.
    #[arg(
        long = "p50-budget-us",
        default_value_t = tests::latency_gamestream_sim::P50_BUDGET_US
    )]
    p50_budget_us: u64,

    /// `latency_gamestream_sim`: merged-stream P99 latency budget in microseconds.
    #[arg(
        long = "p99-budget-us",
        default_value_t = tests::latency_gamestream_sim::P99_BUDGET_US
    )]
    p99_budget_us: u64,

    /// `latency_gamestream_sim`: allowed per-stream packet loss percentage.
    #[arg(long = "loss-budget-pct", default_value_t = 0.0)]
    loss_budget_pct: f64,

    /// `latency_stress_curve`: comma-separated Mbps step list. Plan-spec
    /// default `20,50,100,200,500,1000`.
    #[arg(long, default_value = "20,50,100,200,500,1000")]
    steps: String,

    /// `latency_stress_curve`: UDP payload bytes. Keep 1400 as the historical
    /// default; Lantunnel 2.0 uses 1385 to verify the sealed frame remains one
    /// datagram at the canonical 1452-byte runtime MTU.
    #[arg(long, default_value_t = tests::latency_stress_curve::PACKET_BYTES)]
    packet_bytes: usize,

    /// `latency_stress_curve`: per-step duration in seconds. Plan-spec default 30.
    #[arg(long, default_value_t = 30u64)]
    step_duration: u64,

    /// JSON metric report output path. Empty string uses `<test>.json`.
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
        format!("{}.json", args.test)
    } else {
        args.out.clone()
    };

    tracing::info!(test = %args.test, out = %out, "run start");

    let result = match args.test.as_str() {
        "latency_baseline" => {
            tests::latency_baseline::run(tests::latency_baseline::Args {
                proxy: &args.proxy,
                http_target: &args.target,
                udp_target: &args.udp_target,
                samples: args.samples,
                out: &out,
            })
            .await
        }
        "latency_gamestream_sim" => {
            tests::latency_gamestream_sim::run(tests::latency_gamestream_sim::Args {
                proxy: &args.proxy,
                udp_target: &args.udp_target,
                duration_secs: args.duration,
                p50_budget_us: args.p50_budget_us,
                p99_budget_us: args.p99_budget_us,
                loss_budget_pct: args.loss_budget_pct,
                out: &out,
            })
            .await
        }
        "latency_stress_curve" => {
            let steps = parse_steps(&args.steps)?;
            tests::latency_stress_curve::run(tests::latency_stress_curve::Args {
                proxy: &args.proxy,
                udp_target: &args.udp_target,
                steps_mbps: &steps,
                packet_bytes: args.packet_bytes,
                step_duration_secs: args.step_duration,
                out: &out,
            })
            .await
        }
        other => Err(anyhow!(
            "unknown test {other:?} (known: latency_baseline, latency_gamestream_sim, \
             latency_stress_curve)"
        )),
    };

    if let Err(e) = &result {
        tracing::error!(test = %args.test, err = %e, "FAIL");
    }
    result
}

/// Parse `"20,50,100,200,500,1000"` into a sorted `Vec<u32>`. Empty entries
/// rejected; non-numeric tokens bail with a clear message.
fn parse_steps(raw: &str) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for tok in raw.split(',') {
        let s = tok.trim();
        if s.is_empty() {
            return Err(anyhow!("empty step in --steps {raw:?}"));
        }
        let v: u32 = s
            .parse()
            .map_err(|e| anyhow!("invalid Mbps step {s:?} in --steps {raw:?}: {e}"))?;
        if v == 0 {
            return Err(anyhow!("step 0 Mbps not allowed in --steps {raw:?}"));
        }
        out.push(v);
    }
    if out.is_empty() {
        return Err(anyhow!("--steps must contain at least one Mbps value"));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests_main {
    use super::*;

    #[test]
    fn parse_steps_default_list() {
        assert_eq!(
            parse_steps("20,50,100,200,500,1000").unwrap(),
            vec![20, 50, 100, 200, 500, 1000]
        );
    }

    #[test]
    fn parse_steps_rejects_empty_and_zero() {
        assert!(parse_steps("").is_err());
        assert!(parse_steps("20,,50").is_err());
        assert!(parse_steps("0,10").is_err());
    }

    #[test]
    fn gamestream_latency_budget_flags_parse() {
        let parsed = <Args as clap::Parser>::try_parse_from([
            "tp-e2e-p2",
            "--test",
            "latency_gamestream_sim",
            "--p50-budget-us",
            "100000",
            "--p99-budget-us",
            "300000",
        ]);

        assert!(
            parsed.is_ok(),
            "custom gamestream latency budget flags should parse: {parsed:?}"
        );
        let args = parsed.unwrap();
        assert_eq!(args.p50_budget_us, 100_000);
        assert_eq!(args.p99_budget_us, 300_000);
    }

    #[test]
    fn gamestream_loss_budget_flag_parse() {
        let parsed = <Args as clap::Parser>::try_parse_from([
            "tp-e2e-p2",
            "--test",
            "latency_gamestream_sim",
            "--loss-budget-pct",
            "1.5",
        ]);

        assert!(
            parsed.is_ok(),
            "custom gamestream loss budget flag should parse: {parsed:?}"
        );
        let args = parsed.unwrap();
        assert_eq!(args.loss_budget_pct, 1.5);
    }
}
