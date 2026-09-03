//! P2P direct-connection support: cert fingerprint, hole-punching, scheduler,
//! state machine, listener.

pub mod announce;
pub mod bootstrap;
pub mod cert;
pub mod expected;
pub mod flow_scheduler;
pub mod installer;
pub mod listener;
pub mod manager;
pub mod mapping_probe;
pub mod multi_sender;
pub mod punch;
pub(crate) mod replica;
pub mod scheduler;
pub mod session;
pub mod tls;

/// Direct P2P QUIC needs a short keepalive, but its idle timeout must survive
/// bulk TCP backpressure. Large Filestash uploads can fill the receiver-side
/// application queue while the target drains slowly; treating that temporary
/// transport silence as a dead P2P path truncates in-flight uploads.
pub(crate) fn p2p_quic_tuning() -> tp_transport::quic::QuicTuning {
    tp_transport::quic::QuicTuning {
        congestion: "bbr".into(),
        initial_congestion_window_bytes: None,
        keep_alive_secs: 3,
        max_idle_secs: 120,
        ..tp_transport::quic::QuicTuning::game_streaming()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn p2p_quic_tuning_tolerates_bulk_tcp_backpressure() {
        let tuning = super::p2p_quic_tuning();

        assert_eq!(tuning.keep_alive_secs, 3);
        assert!(
            tuning.max_idle_secs >= 120,
            "P2P QUIC idle timeout must survive large TCP uploads when target-side backpressure temporarily stalls the reliable stream"
        );
        assert_eq!(
            tuning.congestion,
            "bbr",
            "Direct P2P should use BBR for sustained realtime UDP without building a loss-based send queue"
        );
        assert_eq!(
            tuning.initial_congestion_window_bytes, None,
            "Direct P2P should preserve BBR's controller-specific initial window instead of forcing the shared 4 MiB override"
        );
        assert_eq!(
            tuning.initial_mtu,
            tp_transport::quic::QuicTuning::game_streaming().initial_mtu
        );
        assert_eq!(
            tuning.min_mtu,
            tp_transport::quic::QuicTuning::default().min_mtu
        );
    }
}
