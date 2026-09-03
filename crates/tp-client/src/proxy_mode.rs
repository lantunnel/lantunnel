use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tp_core::protocol::{pack_tcp_flow_open_v2, BinaryMessage, TcpFlowOpenV2};
use tp_core::Protocol;

use crate::p2p::flow_scheduler::{CandidateKey, FlowKind};
use crate::p2p::multi_sender::{MultiSenderRouter, V2RelaySealContext};
use crate::p2p::scheduler::PathKind;
use crate::proxy_tunnel::{
    ProxyTunnelConn, ProxyTunnelConnHooks, ProxyTunnelDatagram, ProxyTunnelDatagramHooks,
};
use crate::status::TrafficPath;
use crate::{
    engine::{
        ProxyFlowAttemptExclude, ProxyFlowLane, RelayRouteBindKey, RelayRouteBindPending,
        UDP_FLOW_INBOUND_CHANNEL_CAP,
    },
    Engine,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const P2P_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
// Relay route-bind control shares capacity with active flows. Allow one link
// liveness window per send/ACK stage so normal queueing and retransmission do
// not disable Relay fallback; the waiter remains bound to the exact generation.
const RELAY_ROUTE_BIND_TIMEOUT: Duration = Duration::from_secs(3);

pub struct ProxyTunnelOpener {
    engine: Arc<Engine>,
    connect_timeout: Duration,
}

impl ProxyTunnelOpener {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            connect_timeout: CONNECT_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn new_with_timeout(engine: Arc<Engine>, connect_timeout: Duration) -> Self {
        Self {
            engine,
            connect_timeout,
        }
    }

    pub async fn open_tcp(&self, address: &str) -> anyhow::Result<ProxyTunnelConn> {
        let target = self.engine.resolve_proxy_target_peer(address).await?;
        let mut excludes = Vec::new();
        let mut last_retry_error = None;
        loop {
            let conn_id = new_conn_id();
            let lane = match self.engine.pick_and_record_proxy_flow_lane_for_peer(
                &conn_id,
                FlowKind::Tcp,
                &excludes,
                target.peer_id.as_deref(),
                target.v2_exact_target,
            ) {
                Some(lane) => lane,
                None => {
                    return Err(last_retry_error
                        .unwrap_or_else(|| anyhow::anyhow!("engine is not connected")));
                }
            };
            tracing::debug!(
                local_client_id = %lane.local_client_id,
                path = ?lane.path,
                "selected replica lane for TCP open"
            );

            let router = self.router_for_flow_lane(&lane);
            match self
                .open_tcp_once_on_path(
                    address,
                    target.logical_destination,
                    conn_id,
                    lane.clone(),
                    router,
                )
                .await
            {
                Ok(conn) => return Ok(conn),
                Err(err) if err.should_retry_placement && !lane.v2_exact_target => {
                    if err.timed_out_after_p2p {
                        tracing::debug!(
                            error = %err.error,
                            local_client_id = %lane.local_client_id,
                            p2p_session_id = ?lane.p2p_session_id,
                            "P2P TCP open timed out or became unavailable; falling back to relay placement"
                        );
                        excludes.push(ProxyFlowAttemptExclude::path(PathKind::P2p));
                    } else {
                        tracing::debug!(
                            error = %err.error,
                            local_client_id = %lane.local_client_id,
                            p2p_session_id = ?lane.p2p_session_id,
                            "TCP flow stream unavailable; rerunning replica placement"
                        );
                        excludes.push(lane.attempt_exclude());
                    }
                    last_retry_error = Some(err.error);
                }
                Err(err) => return Err(err.error),
            }
        }
    }

    async fn open_tcp_once_on_path(
        &self,
        address: &str,
        logical_destination: Option<SocketAddr>,
        conn_id: String,
        lane: ProxyFlowLane,
        connect_router: MultiSenderRouter,
    ) -> Result<ProxyTunnelConn, ProxyConnectAttemptError> {
        let multi = lane.multi.clone();
        let mut placement_guard = ProxyPlacementGuard::new(self.engine.clone(), conn_id.clone());
        if let Some(flow_session) = tcp_flow_session_for_lane(&lane) {
            if lane.path == PathKind::Relay {
                let peer_client_id = lane
                    .target_peer_client_id
                    .clone()
                    .expect("relay TCP flow sessions are only enabled for an exact Peer target");
                if matches!(
                    bind_relay_route_for_p2p_flow(
                        &self.engine,
                        &multi,
                        &conn_id,
                        (Some(peer_client_id), None),
                        lane.v2_exact_target,
                        Protocol::Tcp,
                        logical_destination,
                    )
                    .await,
                    RelayRouteBindResult::NotReady
                ) {
                    self.engine.remove_proxy_flow(&conn_id);
                    return Err(ProxyConnectAttemptError {
                        timed_out_after_p2p: false,
                        should_retry_placement: true,
                        error: anyhow::anyhow!(
                            "exact relay route was not ready before TCP flow stream"
                        ),
                    });
                }
            }
            let v2_sealed_flow = self.commit_v2_exact_relay_open(&conn_id, &lane)?;
            let open_started = Instant::now();
            let flow_open_timeout =
                connect_ack_timeout(lane.path, lane.p2p_session.is_some(), self.connect_timeout);
            let stream_result = if let Some(context) = v2_sealed_flow.as_ref() {
                let mut sealed_open = address.as_bytes().to_vec();
                let Some(conn_id_wire) = relay_conn_id_wire(&conn_id) else {
                    self.engine.remove_proxy_flow(&conn_id);
                    return Err(ProxyConnectAttemptError {
                        timed_out_after_p2p: false,
                        should_retry_placement: false,
                        error: anyhow::anyhow!("invalid V2 Relay connection id"),
                    });
                };
                let record = crate::relay_crypto::RelayRecordContextV2 {
                    tunnel_id: &context.tunnel_id,
                    peerlink_session_id: &context.session_id,
                    source_peer_id: &context.local_peer_id,
                    target_peer_id: &context.remote_peer_id,
                    conn_id: &conn_id_wire,
                };
                if let Err(error) = context.cipher.seal_flow(
                    record,
                    crate::relay_crypto::RelayFlowKindV2::Open,
                    &mut sealed_open,
                ) {
                    self.engine.remove_proxy_flow(&conn_id);
                    return Err(ProxyConnectAttemptError {
                        timed_out_after_p2p: false,
                        should_retry_placement: false,
                        error: anyhow::anyhow!("could not seal V2 Relay TCP OPEN: {error}"),
                    });
                }
                flow_session
                    .open_raw_tcp_flow_stream(
                        pack_tcp_flow_open_v2(&TcpFlowOpenV2 {
                            conn_id: conn_id.clone(),
                            peerlink_session_id: *context.session_id.as_bytes(),
                            sealed_open: Bytes::from(sealed_open),
                        }),
                        flow_open_timeout,
                    )
                    .await
            } else {
                flow_session
                    .open_tcp_flow_stream(conn_id.clone(), address.to_string(), flow_open_timeout)
                    .await
            };
            let mut stream = match stream_result {
                Ok(stream) => stream,
                Err(e) => {
                    let should_retry_placement =
                        matches!(e, tp_transport::TransportError::FlowStreamUnavailable);
                    let timed_out_after_p2p = lane.path == PathKind::P2p && should_retry_placement;
                    self.engine.remove_proxy_flow(&conn_id);
                    return Err(ProxyConnectAttemptError {
                        timed_out_after_p2p,
                        should_retry_placement,
                        error: anyhow::Error::from(e),
                    });
                }
            };
            if let Some(context) = v2_sealed_flow.as_ref() {
                let response = match timeout(
                    flow_open_timeout,
                    tp_transport::session::read_tcp_flow_frame(&mut stream),
                )
                .await
                {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => {
                        self.engine.remove_proxy_flow(&conn_id);
                        return Err(ProxyConnectAttemptError {
                            timed_out_after_p2p: false,
                            should_retry_placement: true,
                            error: anyhow::Error::from(error),
                        });
                    }
                    Err(_) => {
                        self.engine.remove_proxy_flow(&conn_id);
                        return Err(ProxyConnectAttemptError {
                            timed_out_after_p2p: false,
                            should_retry_placement: true,
                            error: anyhow::anyhow!("V2 Relay TCP OPEN response timed out"),
                        });
                    }
                };
                let Some(conn_id_wire) = relay_conn_id_wire(&conn_id) else {
                    self.engine.remove_proxy_flow(&conn_id);
                    return Err(ProxyConnectAttemptError {
                        timed_out_after_p2p: false,
                        should_retry_placement: false,
                        error: anyhow::anyhow!("invalid V2 Relay connection id"),
                    });
                };
                let mut response = response;
                let record = crate::relay_crypto::RelayRecordContextV2 {
                    tunnel_id: &context.tunnel_id,
                    peerlink_session_id: &context.session_id,
                    source_peer_id: &context.remote_peer_id,
                    target_peer_id: &context.local_peer_id,
                    conn_id: &conn_id_wire,
                };
                let open_result = context
                    .cipher
                    .open_flow(
                        record,
                        crate::relay_crypto::RelayFlowKindV2::OpenResponse,
                        &mut response,
                    )
                    .and_then(|()| crate::relay_crypto::RelayControlPayloadV2::decode(&response));
                match open_result {
                    Ok(crate::relay_crypto::RelayControlPayloadV2::OpenResponse {
                        success: true,
                        ..
                    }) => {}
                    Ok(crate::relay_crypto::RelayControlPayloadV2::OpenResponse {
                        success: false,
                        error,
                    }) => {
                        self.engine.remove_proxy_flow(&conn_id);
                        return Err(ProxyConnectAttemptError {
                            timed_out_after_p2p: false,
                            should_retry_placement: false,
                            error: anyhow::anyhow!(error),
                        });
                    }
                    Ok(_) => {
                        self.engine.remove_proxy_flow(&conn_id);
                        return Err(ProxyConnectAttemptError {
                            timed_out_after_p2p: false,
                            should_retry_placement: false,
                            error: anyhow::anyhow!("invalid V2 Relay TCP OPEN response"),
                        });
                    }
                    Err(error) => {
                        self.engine.remove_proxy_flow(&conn_id);
                        return Err(ProxyConnectAttemptError {
                            timed_out_after_p2p: false,
                            should_retry_placement: false,
                            error: anyhow::anyhow!(
                                "could not authenticate V2 Relay TCP OPEN response: {error}"
                            ),
                        });
                    }
                }
            }
            tracing::debug!(
                conn_id = %conn_id,
                address = %address,
                path = ?lane.path,
                local_client_id = %lane.local_client_id,
                p2p_session_id = ?lane.p2p_session_id,
                open_elapsed_ms = open_started.elapsed().as_millis(),
                "tcp flow stream opened"
            );
            self.engine.replace_proxy_flow(
                &conn_id,
                FlowKind::Tcp,
                actual_flow_candidate_key(&lane, lane.path, lane.p2p_session.as_ref()),
            );
            self.engine.mark_proxy_flow_established(&conn_id);
            let active_tcp_flow = multi.begin_tcp_flow_stream();
            let close_engine = self.engine.clone();
            let data_engine = self.engine.clone();
            let tx_progress_engine = self.engine.clone();
            let rx_progress_engine = self.engine.clone();
            let tx_multi = multi.clone();
            let rx_multi = multi.clone();
            let path_kind = lane.path;
            let traffic_path = traffic_path_for_lane(path_kind);
            let hooks = ProxyTunnelConnHooks {
                on_close: Some(Arc::new(move |conn_id| {
                    close_engine.remove_proxy_flow(conn_id);
                })),
                on_data_sent: Some(Arc::new(move |conn_id, payload_bytes| {
                    tx_multi.record_traffic_tx(
                        path_kind,
                        i64::try_from(payload_bytes).unwrap_or(i64::MAX),
                    );
                    data_engine.record_proxy_flow_outbound_payload_bytes(
                        conn_id,
                        FlowKind::Tcp,
                        payload_bytes,
                    );
                    tx_progress_engine.record_proxy_flow_link_io_progress(conn_id);
                })),
                on_data_received: Some(Arc::new(move |conn_id, payload_bytes| {
                    rx_multi.record_traffic_rx(
                        traffic_path,
                        usize::try_from(payload_bytes).unwrap_or(usize::MAX),
                    );
                    rx_progress_engine.record_proxy_flow_link_io_progress(conn_id);
                })),
            };
            let conn = match v2_sealed_flow {
                Some(context) => ProxyTunnelConn::new_with_sealed_tcp_flow_stream(
                    conn_id,
                    stream,
                    context.tunnel_id,
                    context.session_id,
                    context.local_peer_id,
                    context.remote_peer_id,
                    context.cipher,
                    hooks,
                    Some(active_tcp_flow),
                ),
                None => ProxyTunnelConn::new_with_tcp_flow_stream(
                    conn_id,
                    stream,
                    hooks,
                    Some(active_tcp_flow),
                ),
            };
            placement_guard.disarm();
            return Ok(conn);
        }
        let (rx_tx, rx_rx) = mpsc::channel::<Bytes>(64);
        let inbound_maps = vec![multi.inbound()];
        for map in &inbound_maps {
            map.insert(conn_id.clone(), rx_tx.clone());
        }
        let mut inbound_guard = ProxyOpenMapGuard::tcp(inbound_maps.clone(), conn_id.clone());
        let (done_tx, done_rx) = oneshot::channel();
        self.engine.proxy_pending().insert(conn_id.clone(), done_tx);
        let _pending_guard = ProxyPendingGuard::new(self.engine.proxy_pending(), conn_id.clone());

        if lane.path == PathKind::Relay {
            if let Some(peer_client_id) = lane.target_peer_client_id.clone() {
                if matches!(
                    bind_relay_route_for_p2p_flow(
                        &self.engine,
                        &multi,
                        &conn_id,
                        (Some(peer_client_id), None),
                        lane.v2_exact_target,
                        Protocol::Tcp,
                        logical_destination,
                    )
                    .await,
                    RelayRouteBindResult::NotReady
                ) {
                    self.engine.remove_proxy_flow(&conn_id);
                    return Err(ProxyConnectAttemptError {
                        timed_out_after_p2p: false,
                        should_retry_placement: true,
                        error: anyhow::anyhow!(
                            "exact relay route was not ready before TCP Connect"
                        ),
                    });
                }
            }
        }

        let v2_sealed_flow = self.commit_v2_exact_relay_open(&conn_id, &lane)?;
        let connect_router = match v2_sealed_flow.as_ref() {
            Some(context) => connect_router.with_v2_relay_seal(context.clone()),
            None => connect_router,
        };
        let (path, selected_p2p, send_result) = connect_router
            .send_with_path_and_session(BinaryMessage::Connect {
                conn_id: conn_id.clone(),
                network: "tcp".into(),
                address: address.into(),
            })
            .await;
        tracing::debug!(
            conn_id = %conn_id,
            address = %address,
            ?path,
            p2p_installed = multi.p2p().is_some(),
            p2p_state = ?multi.p2p_state(),
            "local proxy CONNECT routed"
        );
        if let Err(e) = send_result {
            let should_retry_placement = lane.path == PathKind::Relay && transport_error_closed(&e);
            if should_retry_placement {
                tracing::debug!(
                    conn_id = %conn_id,
                    local_client_id = %lane.local_client_id,
                    "relay TCP lane closed during send; retrying proxy placement"
                );
                self.engine
                    .unregister_relay_closed_multi_session(&lane.local_client_id, &multi);
            }
            self.engine.proxy_pending().remove(&conn_id);
            self.engine.remove_proxy_flow(&conn_id);
            return Err(ProxyConnectAttemptError {
                timed_out_after_p2p: false,
                should_retry_placement,
                error: anyhow::Error::from(e),
            });
        }
        self.engine.replace_proxy_flow(
            &conn_id,
            FlowKind::Tcp,
            actual_flow_candidate_key(&lane, path, selected_p2p.as_ref()),
        );

        let connect_timeout =
            connect_ack_timeout(path, selected_p2p.is_some(), self.connect_timeout);
        if let Err(e) =
            wait_connect_response(&self.engine, &conn_id, done_rx, connect_timeout).await
        {
            let timed_out_after_p2p =
                selected_p2p.is_some() && matches!(e, ProxyConnectWaitError::TimedOut);
            self.engine.remove_proxy_flow(&conn_id);
            return Err(ProxyConnectAttemptError {
                timed_out_after_p2p,
                should_retry_placement: timed_out_after_p2p,
                error: e.into_anyhow(&conn_id),
            });
        }
        self.engine.mark_proxy_flow_established(&conn_id);
        tracing::debug!(
            conn_id = %conn_id,
            address = %address,
            ?path,
            "local proxy CONNECT ack received"
        );
        let data_router = if let Some(p2p) = selected_p2p.clone() {
            if lane.v2_exact_target {
                MultiSenderRouter::new_pinned_p2p_no_relay_fallback(multi.clone(), p2p)
                    .with_local_client_id(lane.local_client_id.clone())
            } else {
                let (exact_target, fallback_target) = relay_route_targets_for_lane(&lane);
                match bind_relay_route_for_p2p_flow(
                    &self.engine,
                    &multi,
                    &conn_id,
                    (exact_target, fallback_target),
                    false,
                    Protocol::Tcp,
                    logical_destination,
                )
                .await
                {
                    RelayRouteBindResult::Ready => {
                        MultiSenderRouter::new_pinned_p2p(multi.clone(), p2p)
                            .with_local_client_id(lane.local_client_id.clone())
                    }
                    RelayRouteBindResult::NotReady => {
                        MultiSenderRouter::new_pinned_p2p_no_relay_fallback(multi.clone(), p2p)
                            .with_local_client_id(lane.local_client_id.clone())
                    }
                }
            }
        } else if lane.target_peer_client_id.is_some() {
            MultiSenderRouter::new_relay_only(multi.clone())
                .with_local_client_id(lane.local_client_id.clone())
        } else {
            MultiSenderRouter::new_relay_with_p2p_fallback(multi.clone())
                .with_local_client_id(lane.local_client_id.clone())
        };
        let data_router = match v2_sealed_flow {
            Some(context) => data_router.with_v2_relay_seal(context),
            None => data_router,
        };
        inbound_guard.disarm();
        let close_engine = self.engine.clone();
        let data_engine = self.engine.clone();
        let rx_progress_engine = self.engine.clone();
        let conn = ProxyTunnelConn::new_with_inbound_maps_and_hooks(
            conn_id,
            rx_rx,
            data_router,
            inbound_maps,
            ProxyTunnelConnHooks {
                on_close: Some(Arc::new(move |conn_id| {
                    close_engine.remove_proxy_flow(conn_id);
                })),
                on_data_sent: Some(Arc::new(move |conn_id, payload_bytes| {
                    data_engine.record_proxy_flow_outbound_payload_bytes(
                        conn_id,
                        FlowKind::Tcp,
                        payload_bytes,
                    );
                })),
                on_data_received: Some(Arc::new(move |conn_id, _payload_bytes| {
                    rx_progress_engine.record_proxy_flow_link_io_progress(conn_id);
                })),
            },
        );
        placement_guard.disarm();
        Ok(conn)
    }

    pub async fn open_udp(&self, address: &str) -> anyhow::Result<ProxyTunnelDatagram> {
        let target = self.engine.resolve_proxy_target_peer(address).await?;
        let mut excludes = Vec::new();
        loop {
            let conn_id = new_conn_id();
            let lane = self
                .engine
                .pick_and_record_proxy_flow_lane_for_peer(
                    &conn_id,
                    FlowKind::Udp,
                    &excludes,
                    target.peer_id.as_deref(),
                    target.v2_exact_target,
                )
                .ok_or_else(|| anyhow::anyhow!("engine is not connected"))?;
            tracing::debug!(
                local_client_id = %lane.local_client_id,
                path = ?lane.path,
                "selected replica lane for UDP open"
            );

            let router = self.router_for_flow_lane(&lane);
            match self
                .open_udp_once_on_path(
                    address,
                    target.logical_destination,
                    conn_id,
                    lane.clone(),
                    router,
                )
                .await
            {
                Ok(datagram) => return Ok(datagram),
                Err(err) if err.timed_out_after_p2p && !lane.v2_exact_target => {
                    tracing::debug!(
                        error = %err.error,
                        local_client_id = %lane.local_client_id,
                        p2p_session_id = ?lane.p2p_session_id,
                        "P2P UDP associate ack timed out; falling back to relay placement"
                    );
                    excludes.push(ProxyFlowAttemptExclude::path(PathKind::P2p));
                }
                Err(err) if err.should_retry_placement && !lane.v2_exact_target => {
                    tracing::debug!(
                        error = %err.error,
                        local_client_id = %lane.local_client_id,
                        p2p_session_id = ?lane.p2p_session_id,
                        "UDP flow stream unavailable; rerunning replica placement"
                    );
                    excludes.push(lane.attempt_exclude());
                }
                Err(err) => return Err(err.error),
            }
        }
    }

    async fn open_udp_once_on_path(
        &self,
        address: &str,
        logical_destination: Option<SocketAddr>,
        conn_id: String,
        lane: ProxyFlowLane,
        connect_router: MultiSenderRouter,
    ) -> Result<ProxyTunnelDatagram, ProxyConnectAttemptError> {
        let multi = lane.multi.clone();
        let mut placement_guard = ProxyPlacementGuard::new(self.engine.clone(), conn_id.clone());
        let (rx_tx, rx_rx) =
            tp_transport::drop_oldest_channel::<Bytes>(UDP_FLOW_INBOUND_CHANNEL_CAP);
        let inbound_maps = vec![multi.udp_inbound()];
        for map in &inbound_maps {
            map.insert(conn_id.clone(), rx_tx.clone());
        }
        let mut inbound_guard = ProxyOpenMapGuard::udp(inbound_maps.clone(), conn_id.clone());
        let (done_tx, done_rx) = oneshot::channel();
        self.engine.proxy_pending().insert(conn_id.clone(), done_tx);
        let _pending_guard = ProxyPendingGuard::new(self.engine.proxy_pending(), conn_id.clone());

        if lane.path == PathKind::Relay {
            if let Some(peer_client_id) = lane.target_peer_client_id.clone() {
                if matches!(
                    bind_relay_route_for_p2p_flow(
                        &self.engine,
                        &multi,
                        &conn_id,
                        (Some(peer_client_id), None),
                        lane.v2_exact_target,
                        Protocol::Udp,
                        logical_destination,
                    )
                    .await,
                    RelayRouteBindResult::NotReady
                ) {
                    self.engine.remove_proxy_flow(&conn_id);
                    return Err(ProxyConnectAttemptError {
                        timed_out_after_p2p: false,
                        should_retry_placement: true,
                        error: anyhow::anyhow!(
                            "exact relay route was not ready before UDP Connect"
                        ),
                    });
                }
            }
        }

        let v2_sealed_flow = self.commit_v2_exact_relay_open(&conn_id, &lane)?;
        let connect_router = match v2_sealed_flow.as_ref() {
            Some(context) => connect_router.with_v2_relay_seal(context.clone()),
            None => connect_router,
        };
        let (path, selected_p2p, send_result) = connect_router
            .send_with_path_and_session(BinaryMessage::Connect {
                conn_id: conn_id.clone(),
                network: "udp".into(),
                address: address.into(),
            })
            .await;
        tracing::debug!(
            conn_id = %conn_id,
            address = %address,
            ?path,
            p2p_installed = multi.p2p().is_some(),
            p2p_state = ?multi.p2p_state(),
            "local proxy UDP ASSOCIATE routed"
        );
        if let Err(e) = send_result {
            let should_retry_placement = lane.path == PathKind::Relay && transport_error_closed(&e);
            if should_retry_placement {
                tracing::debug!(
                    conn_id = %conn_id,
                    local_client_id = %lane.local_client_id,
                    "relay UDP lane closed during send; retrying proxy placement"
                );
                self.engine
                    .unregister_relay_closed_multi_session(&lane.local_client_id, &multi);
            }
            self.engine.proxy_pending().remove(&conn_id);
            self.engine.remove_proxy_flow(&conn_id);
            return Err(ProxyConnectAttemptError {
                timed_out_after_p2p: false,
                should_retry_placement,
                error: anyhow::Error::from(e),
            });
        }
        self.engine.replace_proxy_flow(
            &conn_id,
            FlowKind::Udp,
            actual_flow_candidate_key(&lane, path, selected_p2p.as_ref()),
        );

        let connect_timeout =
            connect_ack_timeout(path, selected_p2p.is_some(), self.connect_timeout);
        if let Err(e) =
            wait_connect_response(&self.engine, &conn_id, done_rx, connect_timeout).await
        {
            let timed_out_after_p2p =
                selected_p2p.is_some() && matches!(e, ProxyConnectWaitError::TimedOut);
            self.engine.remove_proxy_flow(&conn_id);
            return Err(ProxyConnectAttemptError {
                timed_out_after_p2p,
                should_retry_placement: timed_out_after_p2p,
                error: e.into_anyhow(&conn_id),
            });
        }
        self.engine.mark_proxy_flow_established(&conn_id);
        tracing::debug!(
            conn_id = %conn_id,
            address = %address,
            ?path,
            "local proxy UDP ASSOCIATE ack received"
        );
        let data_router = if let Some(p2p) = selected_p2p.clone() {
            if lane.v2_exact_target {
                MultiSenderRouter::new_pinned_p2p_no_relay_fallback(multi.clone(), p2p)
                    .with_local_client_id(lane.local_client_id.clone())
            } else {
                let (exact_target, fallback_target) = relay_route_targets_for_lane(&lane);
                match bind_relay_route_for_p2p_flow(
                    &self.engine,
                    &multi,
                    &conn_id,
                    (exact_target, fallback_target),
                    false,
                    Protocol::Udp,
                    logical_destination,
                )
                .await
                {
                    RelayRouteBindResult::Ready => {
                        MultiSenderRouter::new_pinned_p2p(multi.clone(), p2p)
                            .with_local_client_id(lane.local_client_id.clone())
                    }
                    RelayRouteBindResult::NotReady => {
                        MultiSenderRouter::new_pinned_p2p_no_relay_fallback(multi.clone(), p2p)
                            .with_local_client_id(lane.local_client_id.clone())
                    }
                }
            }
        } else if lane.target_peer_client_id.is_some() {
            MultiSenderRouter::new_relay_only(multi.clone())
                .with_local_client_id(lane.local_client_id.clone())
        } else {
            MultiSenderRouter::new_relay_with_p2p_fallback(multi.clone())
                .with_local_client_id(lane.local_client_id.clone())
        };
        let data_router = match v2_sealed_flow {
            Some(context) => data_router.with_v2_relay_seal(context),
            None => data_router,
        };
        inbound_guard.disarm();
        let close_engine = self.engine.clone();
        let data_engine = self.engine.clone();
        let rx_progress_engine = self.engine.clone();
        let datagram = ProxyTunnelDatagram::new_with_inbound_maps_and_hooks(
            conn_id,
            rx_rx,
            data_router,
            inbound_maps,
            ProxyTunnelDatagramHooks {
                on_close: Some(Arc::new(move |conn_id| {
                    close_engine.remove_proxy_flow(conn_id);
                })),
                on_data_sent: Some(Arc::new(move |conn_id, payload_bytes| {
                    data_engine.record_proxy_flow_outbound_payload_bytes(
                        conn_id,
                        FlowKind::Udp,
                        payload_bytes,
                    );
                })),
                on_data_received: Some(Arc::new(move |conn_id, _payload_bytes| {
                    rx_progress_engine.record_proxy_flow_link_io_progress(conn_id);
                })),
            },
        );
        placement_guard.disarm();
        Ok(datagram)
    }

    fn router_for_flow_lane(&self, lane: &ProxyFlowLane) -> MultiSenderRouter {
        match (lane.v2_exact_target, lane.path, lane.p2p_session.as_ref()) {
            (true, PathKind::P2p, Some(direct)) => {
                MultiSenderRouter::new_pinned_p2p_no_relay_fallback(
                    lane.multi.clone(),
                    direct.clone(),
                )
                .with_local_client_id(lane.local_client_id.clone())
            }
            _ => router_for_flow_lane(lane),
        }
    }

    /// Commit exact Relay authority only after RelayRouteBind succeeded and
    /// immediately before the endpoint OPEN is emitted. The returned context
    /// is owned by the open attempt so later profile/map cleanup cannot turn
    /// this V2 flow into plaintext.
    fn commit_v2_exact_relay_open(
        &self,
        conn_id: &str,
        lane: &ProxyFlowLane,
    ) -> Result<Option<V2RelaySealContext>, ProxyConnectAttemptError> {
        // This read is the open-time linearization point for a pre-profile
        // flow. If V2 became authoritative after resolution/selection, the
        // stale lane must be discarded instead of emitting a plaintext OPEN.
        if !lane.v2_exact_target && self.engine.uses_v2_peer_profile() {
            self.engine.remove_proxy_flow(conn_id);
            return Err(ProxyConnectAttemptError {
                timed_out_after_p2p: false,
                should_retry_placement: false,
                error: anyhow::anyhow!("V2 routing became active before the pre-profile flow OPEN"),
            });
        }
        if !lane.v2_exact_target || lane.path != PathKind::Relay {
            return Ok(None);
        }
        let context = lane
            .target_peer_client_id
            .as_deref()
            .and_then(|target_peer_id| {
                self.engine
                    .prepare_v2_relay_flow(conn_id, target_peer_id, lane.p2p_session_id)
            });
        match context {
            Some(context) => Ok(Some(context)),
            None => {
                self.engine.remove_proxy_flow(conn_id);
                Err(ProxyConnectAttemptError {
                    timed_out_after_p2p: false,
                    should_retry_placement: false,
                    error: anyhow::anyhow!(
                        "V2 exact Relay authority became unavailable before flow OPEN"
                    ),
                })
            }
        }
    }
}

fn valid_relay_peer_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && id != "__legacy_single_p2p_peer__"
}

fn transport_error_closed(error: &tp_transport::TransportError) -> bool {
    matches!(error, tp_transport::TransportError::Closed)
}

fn relay_route_peer_id(
    multi: &Arc<crate::p2p::session::MultiSession>,
    active_p2p_peer_client_id: Option<String>,
    peer_client_id: Option<String>,
) -> Option<String> {
    if let Some(peer_client_id) = active_p2p_peer_client_id.filter(|id| valid_relay_peer_id(id)) {
        return Some(peer_client_id.trim().to_string());
    }
    if let Some(peer_client_id) = peer_client_id.filter(|id| valid_relay_peer_id(id)) {
        return Some(peer_client_id.trim().to_string());
    }
    let peer_ids = multi.p2p_peer_ids();
    if peer_ids.len() == 1 && valid_relay_peer_id(&peer_ids[0]) {
        return peer_ids.into_iter().next();
    }
    None
}

fn relay_route_targets_for_lane(lane: &ProxyFlowLane) -> (Option<String>, Option<String>) {
    if let Some(target) = lane.target_peer_client_id.clone() {
        return (Some(target), None);
    }

    // A selected Direct session is authoritative only when all installed
    // sessions represent one Replica family.
    let families: std::collections::BTreeSet<_> = lane
        .multi
        .p2p_peer_ids()
        .into_iter()
        .map(|peer| crate::p2p::replica::replica_family_id(&peer))
        .collect();
    let selected_peer = (families.len() <= 1)
        .then(|| lane.candidate_key.peer_client_id.clone())
        .flatten();
    (selected_peer, None)
}

enum RelayRouteBindResult {
    Ready,
    NotReady,
}

async fn bind_relay_route_for_p2p_flow(
    engine: &Arc<Engine>,
    multi: &Arc<crate::p2p::session::MultiSession>,
    conn_id: &str,
    relay_targets: (Option<String>, Option<String>),
    v2_exact: bool,
    protocol: Protocol,
    logical_destination: Option<SocketAddr>,
) -> RelayRouteBindResult {
    let (active_p2p_peer_client_id, peer_client_id) = relay_targets;
    let Some(peer_client_id) =
        relay_route_peer_id(multi, active_p2p_peer_client_id, peer_client_id)
    else {
        tracing::debug!(
            %conn_id,
            "skipping relay route bind for P2P flow without exact peer id"
        );
        return RelayRouteBindResult::NotReady;
    };
    let Some(config) = engine.latest_tunnel_config() else {
        return RelayRouteBindResult::NotReady;
    };
    let source_peer_id = config.peer_id;
    let target_peer_id = if v2_exact {
        peer_client_id.clone()
    } else {
        crate::p2p::replica::replica_family_id(&peer_client_id)
    };
    let Some(logical_destination) = logical_destination else {
        tracing::warn!(%conn_id, "relay Peer route requires a literal logical destination");
        return RelayRouteBindResult::NotReady;
    };
    let replica_ids_invalid = !v2_exact
        && (crate::p2p::replica::replica_index(&source_peer_id) != Some(0)
            || crate::p2p::replica::replica_seed_for_tunnel(&config.tunnel_id, &source_peer_id)
                .is_none()
            || crate::p2p::replica::replica_index(&target_peer_id) != Some(0)
            || crate::p2p::replica::replica_seed_for_tunnel(&config.tunnel_id, &target_peer_id)
                .is_none());
    if source_peer_id.trim().is_empty()
        || target_peer_id.trim().is_empty()
        || source_peer_id == target_peer_id
        || replica_ids_invalid
    {
        tracing::warn!(%conn_id, %source_peer_id, %target_peer_id, "relay route bind lacks canonical Tunnel Peer identities");
        return RelayRouteBindResult::NotReady;
    }
    let router = MultiSenderRouter::new_relay_only(multi.clone());
    let capabilities = multi.relay().capabilities();
    if !capabilities.route_bind_control_v1 || !capabilities.relay_source_attestation_v1 {
        tracing::debug!(%conn_id, "relay source attestation handshake is unavailable");
        return RelayRouteBindResult::NotReady;
    }
    let key = RelayRouteBindKey {
        source_peer_id,
        target_peer_id,
        protocol,
        logical_destination,
    };
    let (tx, ack_rx) = oneshot::channel();
    let pending = engine.relay_route_bind_pending();
    match pending.entry(conn_id.to_string()) {
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(RelayRouteBindPending {
                key,
                relay_generation: Arc::downgrade(multi),
                response: tx,
            });
        }
        dashmap::mapref::entry::Entry::Occupied(_) => {
            tracing::warn!(%conn_id, "duplicate relay route bind conn_id rejected");
            return RelayRouteBindResult::NotReady;
        }
    }
    let _ack_guard = ProxyPendingGuard::new(pending, conn_id.to_string());
    let bind = router.send(BinaryMessage::RelayRouteBind {
        conn_id: conn_id.to_string(),
        peer_client_id: peer_client_id.clone(),
    });
    match timeout(RELAY_ROUTE_BIND_TIMEOUT, bind).await {
        Ok(Ok(())) => match timeout(RELAY_ROUTE_BIND_TIMEOUT, ack_rx).await {
            Ok(Ok(Ok(()))) => {
                tracing::debug!(
                    %conn_id,
                    peer_client_id = %peer_client_id,
                    "relay route ready for P2P fallback"
                );
                RelayRouteBindResult::Ready
            }
            Ok(Ok(Err(error))) => {
                tracing::debug!(
                    %conn_id,
                    peer_client_id = %peer_client_id,
                    %error,
                    "relay route bind rejected"
                );
                RelayRouteBindResult::NotReady
            }
            Ok(Err(_)) => {
                tracing::debug!(
                    %conn_id,
                    peer_client_id = %peer_client_id,
                    "relay route bind ack channel closed"
                );
                RelayRouteBindResult::NotReady
            }
            Err(_) => {
                tracing::debug!(
                    %conn_id,
                    peer_client_id = %peer_client_id,
                    timeout_ms = RELAY_ROUTE_BIND_TIMEOUT.as_millis(),
                    "relay route bind ack timed out"
                );
                RelayRouteBindResult::NotReady
            }
        },
        Ok(Err(e)) => {
            tracing::debug!(
                %conn_id,
                peer_client_id = %peer_client_id,
                error = %e,
                "relay route bind failed"
            );
            RelayRouteBindResult::NotReady
        }
        Err(_) => {
            tracing::debug!(
                %conn_id,
                peer_client_id = %peer_client_id,
                timeout_ms = RELAY_ROUTE_BIND_TIMEOUT.as_millis(),
                "relay route bind timed out"
            );
            RelayRouteBindResult::NotReady
        }
    }
}

struct ProxyConnectAttemptError {
    timed_out_after_p2p: bool,
    should_retry_placement: bool,
    error: anyhow::Error,
}

fn router_for_flow_lane(lane: &ProxyFlowLane) -> MultiSenderRouter {
    match (lane.path, lane.p2p_session.as_ref()) {
        (PathKind::P2p, Some(p2p)) => {
            MultiSenderRouter::new_pinned_p2p(lane.multi.clone(), p2p.clone())
                .with_local_client_id(lane.local_client_id.clone())
        }
        _ => MultiSenderRouter::new_relay_only(lane.multi.clone())
            .with_local_client_id(lane.local_client_id.clone()),
    }
}

fn tcp_flow_session_for_lane(lane: &ProxyFlowLane) -> Option<Arc<tp_transport::session::Session>> {
    if tcp_flow_stream_disabled_for_diagnostics() {
        tracing::debug!(
            path = ?lane.path,
            local_client_id = %lane.local_client_id,
            p2p_session_id = ?lane.p2p_session_id,
            "TCP flow stream disabled by diagnostic env; using multiplexed Connect/Data path"
        );
        return None;
    }
    let session = match (lane.path, lane.p2p_session.as_ref()) {
        (PathKind::P2p, Some(p2p)) => p2p.clone(),
        (PathKind::Relay, None) if lane.target_peer_client_id.is_some() => {
            lane.multi.relay().clone()
        }
        _ => return None,
    };
    session.capabilities().tcp_flow_stream_v1.then_some(session)
}

fn tcp_flow_stream_disabled_for_diagnostics() -> bool {
    std::env::var("TUNNEL_PROXY_DISABLE_TCP_FLOW_STREAM")
        .ok()
        .as_deref()
        .map(parse_truthy_env_value)
        .unwrap_or(false)
}

fn parse_truthy_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn traffic_path_for_lane(path: PathKind) -> TrafficPath {
    match path {
        PathKind::Relay => TrafficPath::Relay,
        PathKind::P2p => TrafficPath::P2p,
    }
}

fn actual_flow_candidate_key(
    lane: &ProxyFlowLane,
    path: PathKind,
    selected_p2p: Option<&Arc<tp_transport::session::Session>>,
) -> CandidateKey {
    if path == PathKind::Relay && selected_p2p.is_none() {
        return match lane.target_peer_client_id.as_deref() {
            Some(peer_client_id) => CandidateKey::relay_to_peer(
                lane.local_client_id.clone(),
                lane.candidate_key.transport_generation,
                peer_client_id,
            ),
            None => CandidateKey::relay(
                lane.local_client_id.clone(),
                lane.candidate_key.transport_generation,
            ),
        };
    }
    lane.candidate_key.clone()
}

pub struct LocalEngineSocks5Backend {
    opener: ProxyTunnelOpener,
}

impl LocalEngineSocks5Backend {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            opener: ProxyTunnelOpener::new(engine),
        }
    }
}

#[async_trait::async_trait]
impl tp_proxy_socks5::backend::Socks5Backend for LocalEngineSocks5Backend {
    async fn open_tcp(
        &self,
        _group_id: &str,
        target: &str,
    ) -> anyhow::Result<tp_proxy_socks5::backend::BoxTcpTunnel> {
        let tunnel = self.opener.open_tcp(target).await?;
        Ok(Box::pin(tunnel))
    }

    async fn open_udp(
        &self,
        _group_id: &str,
        target: &str,
    ) -> anyhow::Result<tp_proxy_socks5::backend::BoxUdpTunnel> {
        let tunnel = self.opener.open_udp(target).await?;
        Ok(Box::new(LocalEngineUdpTunnel {
            tunnel,
            engine: self.opener.engine.clone(),
        }))
    }
}

struct LocalEngineUdpTunnel {
    tunnel: ProxyTunnelDatagram,
    engine: Arc<Engine>,
}

impl tp_proxy_socks5::backend::UdpTunnel for LocalEngineUdpTunnel {
    fn split(
        self: Box<Self>,
    ) -> (
        tp_proxy_socks5::backend::BoxUdpTunnelSender,
        tp_proxy_socks5::backend::BoxUdpTunnelReceiver,
    ) {
        let conn_id = self.tunnel.conn_id().to_string();
        let (sender, receiver) = self.tunnel.split();
        (
            Box::new(LocalEngineUdpTunnelSender { sender }),
            Box::new(LocalEngineUdpTunnelReceiver {
                receiver,
                engine: self.engine.clone(),
                conn_id,
                closed: false,
            }),
        )
    }
}

struct LocalEngineUdpTunnelSender {
    sender: crate::proxy_tunnel::ProxyTunnelDatagramSender,
}

impl tp_proxy_socks5::backend::UdpTunnelSender for LocalEngineUdpTunnelSender {
    fn try_send(&self, payload: Bytes) -> Result<(), tp_transport::TrySendKind> {
        self.sender.try_send(payload)
    }
}

struct LocalEngineUdpTunnelReceiver {
    receiver: crate::proxy_tunnel::ProxyTunnelDatagramReceiver,
    engine: Arc<Engine>,
    conn_id: String,
    closed: bool,
}

#[async_trait::async_trait]
impl tp_proxy_socks5::backend::UdpTunnelReceiver for LocalEngineUdpTunnelReceiver {
    async fn recv(&mut self) -> Option<Bytes> {
        self.receiver.recv().await
    }

    fn try_recv(&mut self) -> Result<Bytes, tokio::sync::mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    fn conn_id(&self) -> &str {
        self.receiver.conn_id()
    }

    async fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.engine.remove_proxy_flow(&self.conn_id);
        }
        self.receiver.close().await;
    }
}

impl Drop for LocalEngineUdpTunnelReceiver {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            self.engine.remove_proxy_flow(&self.conn_id);
        }
    }
}

struct ProxyPlacementGuard {
    engine: Arc<Engine>,
    conn_id: String,
    active: bool,
}

impl ProxyPlacementGuard {
    fn new(engine: Arc<Engine>, conn_id: String) -> Self {
        Self {
            engine,
            conn_id,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for ProxyPlacementGuard {
    fn drop(&mut self) {
        if self.active {
            self.engine.remove_proxy_flow(&self.conn_id);
        }
    }
}

struct ProxyPendingGuard<T> {
    pending: Arc<dashmap::DashMap<String, T>>,
    conn_id: String,
}

impl<T> ProxyPendingGuard<T> {
    fn new(pending: Arc<dashmap::DashMap<String, T>>, conn_id: String) -> Self {
        Self { pending, conn_id }
    }
}

impl<T> Drop for ProxyPendingGuard<T> {
    fn drop(&mut self) {
        self.pending.remove(&self.conn_id);
    }
}

enum ProxyOpenMapGuard {
    Tcp {
        maps: Vec<Arc<dashmap::DashMap<String, mpsc::Sender<Bytes>>>>,
        conn_id: String,
        active: bool,
    },
    Udp {
        maps: Vec<Arc<dashmap::DashMap<String, tp_transport::DropOldestSender<Bytes>>>>,
        conn_id: String,
        active: bool,
    },
}

impl ProxyOpenMapGuard {
    fn tcp(maps: Vec<Arc<dashmap::DashMap<String, mpsc::Sender<Bytes>>>>, conn_id: String) -> Self {
        Self::Tcp {
            maps,
            conn_id,
            active: true,
        }
    }

    fn udp(
        maps: Vec<Arc<dashmap::DashMap<String, tp_transport::DropOldestSender<Bytes>>>>,
        conn_id: String,
    ) -> Self {
        Self::Udp {
            maps,
            conn_id,
            active: true,
        }
    }

    fn disarm(&mut self) {
        match self {
            Self::Tcp { active, .. } | Self::Udp { active, .. } => *active = false,
        }
    }
}

impl Drop for ProxyOpenMapGuard {
    fn drop(&mut self) {
        match self {
            Self::Tcp {
                maps,
                conn_id,
                active,
            } if *active => {
                for map in maps {
                    map.remove(conn_id);
                }
            }
            Self::Udp {
                maps,
                conn_id,
                active,
            } if *active => {
                for map in maps {
                    map.remove(conn_id);
                }
            }
            _ => {}
        }
    }
}

async fn wait_connect_response(
    engine: &Arc<Engine>,
    conn_id: &str,
    done_rx: oneshot::Receiver<Result<(), String>>,
    connect_timeout: Duration,
) -> Result<(), ProxyConnectWaitError> {
    match timeout(connect_timeout, done_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => {
            engine.proxy_pending().remove(conn_id);
            Err(ProxyConnectWaitError::Rejected(error))
        }
        Ok(Err(_)) => {
            engine.proxy_pending().remove(conn_id);
            Err(ProxyConnectWaitError::ChannelClosed)
        }
        Err(_) => {
            engine.proxy_pending().remove(conn_id);
            Err(ProxyConnectWaitError::TimedOut)
        }
    }
}

fn connect_ack_timeout(path: PathKind, selected_p2p: bool, configured: Duration) -> Duration {
    if selected_p2p || path == PathKind::P2p {
        configured.min(P2P_CONNECT_TIMEOUT)
    } else {
        configured
    }
}

enum ProxyConnectWaitError {
    Rejected(String),
    ChannelClosed,
    TimedOut,
}

impl ProxyConnectWaitError {
    fn into_anyhow(self, conn_id: &str) -> anyhow::Error {
        let reason = match self {
            Self::Rejected(error) => error,
            Self::ChannelClosed => "connect response channel closed".into(),
            Self::TimedOut => "connect response timed out".into(),
        };
        anyhow::anyhow!("proxy tunnel connect failed for conn_id={conn_id}: {reason}")
    }
}

fn new_conn_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..tp_core::types::CONN_ID_SIZE].to_string()
}

fn relay_conn_id_wire(conn_id: &str) -> Option<[u8; 12]> {
    let bytes = conn_id.as_bytes();
    if bytes.is_empty() || bytes.len() > 12 || !bytes.is_ascii() || bytes.contains(&0) {
        return None;
    }
    let mut wire = [0_u8; 12];
    wire[..bytes.len()].copy_from_slice(bytes);
    Some(wire)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use dashmap::DashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{mpsc, oneshot};
    use tp_core::config::{
        ClientP2pConfig, ClientRoleConfig, LocalServiceExportConfig, LocalServiceProtocolConfig,
        LocalServiceRouteKindConfig, LocalServiceSourcePolicyConfig,
    };
    use tp_core::p2p_types::SessionId;
    use tp_core::protocol::{pack, unpack, BinaryMessage, PackedMessage, TransportCapabilities};
    use tp_metrics::MetricsManager;
    use tp_proxy_socks5::backend::Socks5Backend;
    use tp_transport::session::Session;
    use tp_transport::{
        tls, AuthHandler, AuthParams, DropOldestSender, QuicClient, QuicServer, QuicTuning,
    };

    use crate::p2p::flow_scheduler::{CandidateKey, CandidatePath, FlowKind};
    use crate::p2p::scheduler::{PathKind, PathScheduler};
    use crate::p2p::session::MultiSession;
    use crate::platform::TunnelConfig;
    use crate::proxy_mode::{
        connect_ack_timeout, LocalEngineSocks5Backend, ProxyTunnelOpener, P2P_CONNECT_TIMEOUT,
    };
    use crate::status::NullListener;
    use crate::{Engine, EngineConfig};

    fn channel_session() -> (Arc<Session>, mpsc::Receiver<PackedMessage>) {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        (
            Arc::new(Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            )),
            out_rx,
        )
    }

    fn quic_session_without_datagrams() -> (Arc<Session>, mpsc::Receiver<PackedMessage>) {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        (
            Arc::new(
                Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
                    .with_udp_data_mode(tp_transport::session::UdpDataMode::QuicDatagramRequired),
            ),
            out_rx,
        )
    }

    #[tokio::test]
    async fn framed_tcp_proxy_outbound_enqueue_does_not_refresh_link_io_progress() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let key = CandidateKey::relay("client-1", 0);
        engine.record_proxy_flow_pending_for_test("framed-tcp", FlowKind::Tcp, key.clone());
        engine.mark_proxy_flow_established("framed-tcp");

        engine.record_proxy_flow_outbound_payload_bytes("framed-tcp", FlowKind::Tcp, 1024);
        tokio::time::sleep(Duration::from_millis(2)).await;
        engine.record_proxy_flow_outbound_payload_bytes("framed-tcp", FlowKind::Tcp, 1024);

        assert_eq!(
            engine.proxy_flow_last_link_io_progress_for_test(&key),
            0,
            "local framed TCP enqueue is not authenticated same-link progress"
        );
    }

    #[tokio::test]
    async fn framed_udp_proxy_outbound_enqueue_does_not_refresh_link_io_progress() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let key = CandidateKey::relay("client-1", 0);
        engine.record_proxy_flow_pending_for_test("framed-udp", FlowKind::Udp, key.clone());
        engine.mark_proxy_flow_established("framed-udp");

        engine.record_proxy_flow_outbound_payload_bytes("framed-udp", FlowKind::Udp, 1024);
        tokio::time::sleep(Duration::from_millis(2)).await;
        engine.record_proxy_flow_outbound_payload_bytes("framed-udp", FlowKind::Udp, 1024);

        assert_eq!(
            engine.proxy_flow_last_link_io_progress_for_test(&key),
            0,
            "local framed UDP enqueue is not authenticated same-link progress"
        );
    }

    struct TestSessionChannels {
        session: Arc<Session>,
        data_rx: mpsc::Receiver<PackedMessage>,
        control_rx: mpsc::Receiver<PackedMessage>,
    }

    fn channel_session_with_capabilities(
        capabilities: TransportCapabilities,
    ) -> TestSessionChannels {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (control_tx, control_rx) = mpsc::channel::<PackedMessage>(16);
        let (_control_in_tx, control_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
            .with_control_channel(
                control_tx,
                control_in_rx,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            )
            .with_capabilities(capabilities);
        TestSessionChannels {
            session: Arc::new(session),
            data_rx: out_rx,
            control_rx,
        }
    }

    fn attested_relay_session() -> TestSessionChannels {
        channel_session_with_capabilities(TransportCapabilities {
            route_bind_control_v1: true,
            tcp_flow_stream_v1: false,
            relay_source_attestation_v1: true,
            peer_mesh_v2: false,
        })
    }

    #[test]
    fn p2p_connect_ack_timeout_leaves_budget_for_relay_fallback() {
        let configured = Duration::from_secs(15);

        assert_eq!(P2P_CONNECT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(
            connect_ack_timeout(PathKind::P2p, true, configured),
            P2P_CONNECT_TIMEOUT
        );
        assert_eq!(
            connect_ack_timeout(PathKind::Relay, false, configured),
            configured
        );
        assert_eq!(
            connect_ack_timeout(PathKind::P2p, true, Duration::from_millis(10)),
            Duration::from_millis(10)
        );
    }

    fn engine_with_multi(multi: Arc<MultiSession>) -> Arc<Engine> {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_multi_session_for_test(multi);
        engine
    }

    fn configure_mesh_identity(engine: &Arc<Engine>, overlay_ipv4: &str) {
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            overlay_ipv4: overlay_ipv4.into(),
            client_id: "mesh-Local001-0".into(),
            client_ids: vec!["mesh-Local001-0".into()],
            replicas: 1,
            ..TunnelConfig::default()
        });
    }

    fn configure_v2_current_peer(
        engine: &Arc<Engine>,
        local: &tp_core::provisioning::PeerProfileV2,
        remote: &tp_core::provisioning::PeerProfileV2,
    ) {
        engine.set_active_v2_peer_profile_for_test(Arc::new(local.clone()));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: local.tunnel_id.clone(),
            peer_id: local.peer.peer_id.clone(),
            overlay_ipv4: local.peer.overlay_ip.to_string(),
            client_id: local.peer.peer_id.clone(),
            client_ids: vec![local.peer.peer_id.clone()],
            replicas: 1,
            ..TunnelConfig::default()
        });
        engine
            .install_v2_peer_membership(&remote.public_membership())
            .expect("install remote V2 membership");
        assert!(engine.commit_v2_membership_cycle(std::slice::from_ref(&remote.peer.peer_id)));
        engine.mark_v2_gateway_attached_for_test(SocketAddr::from((Ipv4Addr::LOCALHOST, 8443)));
    }

    fn configure_v2_peer_pair(
        engine: &Arc<Engine>,
        local: &tp_core::provisioning::PeerProfileV2,
        remote: &tp_core::provisioning::PeerProfileV2,
        session_id: SessionId,
    ) {
        use tp_core::p2p_types::CertFingerprint;
        use tp_core::peer_link_crypto::{P2pAnswerV2, P2pOfferV2, PeerLinkEphemeralSecretV2};

        configure_v2_current_peer(engine, local, remote);

        let local_secret = PeerLinkEphemeralSecretV2::generate();
        let remote_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = P2pOfferV2::sign(
            local,
            session_id,
            remote.peer.peer_id.clone(),
            Vec::new(),
            CertFingerprint::from_bytes([0x61; 32]),
            &local_secret,
        )
        .expect("sign test Offer");
        let answer = P2pAnswerV2::sign(
            remote,
            &offer,
            true,
            0,
            Vec::new(),
            CertFingerprint::from_bytes([0x62; 32]),
            &remote_secret,
        )
        .expect("sign test Answer");
        let keys = local_secret
            .derive_session_keys(&offer, &answer, &local.tunnel_signing_public_key)
            .expect("derive local test PeerLink keys");
        engine
            .install_v2_peer_link(remote.peer.peer_id.clone(), session_id, keys)
            .expect("install test PeerLink");
    }

    fn v2_peer_pair() -> (
        tp_core::provisioning::PeerProfileV2,
        tp_core::provisioning::PeerProfileV2,
    ) {
        use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};

        let mut owner = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        })
        .expect("generate V2 Tunnel");
        (
            owner.add_peer(None, 1, None).expect("local Peer"),
            owner.add_peer(None, 1, None).expect("remote Peer"),
        )
    }

    fn configure_overlay_export(
        engine: &Arc<Engine>,
        protocol: LocalServiceProtocolConfig,
        target: SocketAddr,
    ) {
        configure_mesh_identity(engine, &target.ip().to_string());
        engine
            .set_local_service_exports(&[LocalServiceExportConfig {
                route_kind: LocalServiceRouteKindConfig::Overlay,
                protocol,
                ingress_port: target.port(),
                source_policy: LocalServiceSourcePolicyConfig::AnyTunnelPeer,
                local_host: target.ip().to_string(),
                local_port: target.port(),
            }])
            .expect("install explicit direct test export");
    }

    fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
        let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
    }

    fn install_p2p(
        multi: &Arc<MultiSession>,
        session_id: SessionId,
        peer_client_id: &str,
        session: Arc<Session>,
    ) {
        multi
            .install_p2p_session(session_id, peer_client_id.to_string(), session)
            .expect("install p2p session");
    }

    fn install_relation_p2p(
        multi: &Arc<MultiSession>,
        session_id: SessionId,
        remote_peer_id: &str,
        remote_replica_id: &str,
        session: Arc<Session>,
    ) {
        let relation = crate::peer_link_manager::PeerRelationKey::from_canonical_initiator(
            "mesh-Local001-0",
            remote_peer_id,
        )
        .expect("canonical test Peer relation");
        multi
            .install_p2p_session_for_relation(
                session_id,
                remote_replica_id.to_string(),
                session,
                Some(relation),
            )
            .expect("install relation-bound p2p session");
    }

    fn make_multi_with_p2p_first_scheduler(relay: Arc<Session>) -> Arc<MultiSession> {
        let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        let scheduler = Arc::new(PathScheduler::from_config(&ClientP2pConfig {
            scheduler_stable_cycles: 1,
            ..ClientP2pConfig::default()
        }));
        MultiSession::new_with_existing_maps_and_scheduler(relay, inbound, udp_inbound, scheduler)
    }

    struct AllowAuth;

    #[async_trait]
    impl AuthHandler for AllowAuth {
        async fn authenticate(&self, _params: &AuthParams) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    fn test_auth(client_id: &str) -> AuthParams {
        AuthParams {
            tunnel_id: "tun-flow".into(),
            capabilities: Default::default(),
            client_id: client_id.into(),
            group_id: "group-flow".into(),
            username: client_id.into(),
            password: "pw".into(),
            group_password: "group-pw".into(),
            peer_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            role: ClientRoleConfig::App,
        }
    }

    async fn recv_msg(rx: &mut mpsc::Receiver<PackedMessage>) -> BinaryMessage {
        let packed = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for routed message")
            .expect("routed message channel closed");
        unpack(&packed.to_bytes()).expect("decode routed message")
    }

    async fn expect_relay_route_bind(
        rx: &mut mpsc::Receiver<PackedMessage>,
        expected_conn_id: &str,
        expected_peer_client_id: &str,
    ) {
        match recv_msg(rx).await {
            BinaryMessage::RelayRouteBind {
                conn_id,
                peer_client_id,
            } => {
                assert_eq!(conn_id, expected_conn_id);
                assert_eq!(peer_client_id, expected_peer_client_id);
            }
            other => panic!("expected relay route bind, got {other:?}"),
        }
    }

    fn assert_no_queued_relay_msg(rx: &mut mpsc::Receiver<PackedMessage>, label: &str) {
        if let Ok(packed) = rx.try_recv() {
            panic!(
                "{label} received unexpected relay message: {:?}",
                unpack(&packed.to_bytes()).expect("decode routed message")
            );
        }
    }

    fn assert_p2p_session_count_unchanged(multi: &Arc<MultiSession>, before: usize, label: &str) {
        assert_eq!(
            multi.p2p_session_count(),
            before,
            "{label} must not close or clear installed P2P sessions"
        );
    }

    fn assert_no_p2p_to_relay_migration_metric(metrics: &MetricsManager) {
        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_conn_id_migrations_total{direction=\"p2p_to_relay\"} 0"),
            "ack timeout retry must not record P2P-to-relay migration metrics:\n{text}"
        );
    }

    async fn assert_tcp_data(
        rx: &mut mpsc::Receiver<PackedMessage>,
        conn_id: &str,
        payload: &[u8],
    ) {
        match recv_msg(rx).await {
            BinaryMessage::Data {
                conn_id: data_conn_id,
                payload: data_payload,
            } => {
                assert_eq!(data_conn_id, conn_id);
                assert_eq!(data_payload, Bytes::copy_from_slice(payload));
            }
            other => panic!("expected TCP Data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p2p_tcp_flow_uses_one_quic_stream_per_conn_id() {
        let (certs, key) = tls::self_signed(&["localhost"]).expect("self-signed cert");
        let server = QuicServer::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            tls::server_config(certs, key).expect("server tls"),
            QuicTuning::default(),
        )
        .expect("bind quic server");
        let addr = server.local_addr().expect("server local addr");
        let (session_tx, session_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let incoming = server.accept_incoming().await.expect("incoming quic");
            let (_params, session) = QuicServer::complete_handshake(incoming, &AllowAuth)
                .await
                .expect("server handshake");
            let _ = session_tx.send(session);
            std::future::pending::<()>().await;
        });

        let client = QuicClient::new(
            tls::client_config(None, true).expect("client tls"),
            QuicTuning::default(),
        )
        .expect("client quic");
        let p2p_session = client
            .connect(addr, "localhost", test_auth("app-flow"))
            .await
            .expect("p2p client connect");
        let server_session = session_rx.await.expect("server session");
        let (_server_sender, mut server_receiver, _server_dg) = server_session.split();
        let mut flow_rx = server_receiver
            .take_tcp_flow_receiver()
            .expect("server tcp flow receiver");
        let server_flow_task = tokio::spawn(async move {
            let cases = [
                ("127.0.0.1:9001", *b"one1", *b"ack1"),
                ("127.0.0.1:9002", *b"two2", *b"ack2"),
            ];
            let mut conn_ids = Vec::new();
            for (address, request, response) in cases {
                let mut incoming = tokio::time::timeout(Duration::from_secs(2), flow_rx.recv())
                    .await
                    .expect("timed out waiting for flow")
                    .expect("flow receiver closed");
                assert_eq!(incoming.preface.address, address);
                assert!(
                    !conn_ids.contains(&incoming.preface.conn_id),
                    "each TCP flow must get its own conn_id/QUIC stream"
                );
                conn_ids.push(incoming.preface.conn_id.clone());
                incoming
                    .stream
                    .send_connect_response(true, String::new())
                    .await
                    .expect("connect response");
                let mut buf = [0u8; 4];
                incoming
                    .stream
                    .read_exact(&mut buf)
                    .await
                    .expect("read request");
                assert_eq!(buf, request);
                incoming
                    .stream
                    .write_all(&response)
                    .await
                    .expect("write response");
                incoming.stream.shutdown().await.expect("shutdown flow");
            }
            let legacy =
                tokio::time::timeout(Duration::from_millis(50), server_receiver.recv_data()).await;
            assert!(
                matches!(legacy, Err(_) | Ok(None)),
                "flow opens must not enqueue legacy Connect/Data frames"
            );
            conn_ids
        });

        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0x4f; 16]),
            "pc-flow",
            Arc::new(p2p_session),
        );
        let traffic = multi.local_traffic();
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine);

        let mut first = opener
            .open_tcp("127.0.0.1:9001")
            .await
            .expect("first tcp flow");
        first.write_all(b"one1").await.expect("write first");
        let mut first_response = [0u8; 4];
        first
            .read_exact(&mut first_response)
            .await
            .expect("read first");
        assert_eq!(&first_response, b"ack1");
        drop(first);

        let mut second = opener
            .open_tcp("127.0.0.1:9002")
            .await
            .expect("second tcp flow");
        second.write_all(b"two2").await.expect("write second");
        let mut second_response = [0u8; 4];
        second
            .read_exact(&mut second_response)
            .await
            .expect("read second");
        assert_eq!(&second_response, b"ack2");
        let stats = traffic.snapshot();
        assert!(
            stats.p2p_tx_bytes >= 8,
            "source-side TCP flow stream writes must count as P2P progress"
        );
        assert!(
            stats.p2p_rx_bytes >= 8,
            "source-side TCP flow stream reads must count as P2P progress"
        );
        assert_no_queued_relay_msg(&mut relay_rx, "relay lane for P2P TCP flow stream");

        let conn_ids = server_flow_task.await.expect("server flow task");
        assert_eq!(conn_ids.len(), 2);
        assert_ne!(conn_ids[0], conn_ids[1]);
        server_task.abort();
    }

    #[tokio::test]
    async fn open_tcp_sends_connect_and_returns_after_success_ack() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9000").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9000");
                conn_id
            }
            other => panic!("expected Connect, got {other:?}"),
        };
        assert!(engine.proxy_pending_contains_for_test(&conn_id));
        assert!(
            multi.inbound().contains_key(&conn_id),
            "inbound slot must exist before ConnectResponse to catch early Data"
        );
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _conn = open.await.expect("join").expect("open tcp");
        assert!(multi.inbound().contains_key(&conn_id));
        assert!(!engine.proxy_pending_contains_for_test(&conn_id));
    }

    #[tokio::test]
    async fn open_tcp_relay_connect_has_no_product_role_hint() {
        let (relay, mut relay_rx) = channel_session();
        let engine = engine_with_multi(make_multi(relay));
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9000").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9000");
                conn_id
            }
            other => panic!("expected Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id,
                success: true,
                error: String::new(),
            })
            .await;

        let _conn = open.await.expect("join").expect("open tcp");
    }

    #[tokio::test]
    async fn open_udp_relay_connect_has_no_product_role_hint() {
        let (relay, mut relay_rx) = channel_session();
        let engine = engine_with_multi(make_multi(relay));
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9000").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9000");
                conn_id
            }
            other => panic!("expected UDP Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id,
                success: true,
                error: String::new(),
            })
            .await;

        let _datagram = open.await.expect("join").expect("open udp");
    }

    #[tokio::test]
    async fn udp_associate_fails_when_selected_quic_path_has_no_datagram_support() {
        let (relay, mut relay_rx) = quic_session_without_datagrams();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi);
        let opener = ProxyTunnelOpener::new(engine.clone());

        let err = match opener.open_udp("127.0.0.1:5353").await {
            Ok(_) => {
                panic!("UDP ASSOCIATE must fail before selecting a QUIC path without datagrams")
            }
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("datagram"),
            "error should name missing datagram support: {err:#}"
        );
        assert_no_queued_relay_msg(&mut relay_rx, "QUIC UDP preflight");
    }

    #[tokio::test]
    async fn open_tcp_uses_p2p_for_connect_and_payload_even_when_scheduler_would_relay() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        multi
            .install_p2p_session(
                SessionId::from_bytes([0x41; 16]),
                "pc-main".into(),
                p2p.clone(),
            )
            .expect("install p2p");
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9002").await });

        let (path, packed) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                packed = p2p_rx.recv() => ("p2p", packed.expect("p2p routed message channel closed")),
                packed = relay_rx.recv() => ("relay", packed.expect("relay routed message channel closed")),
            }
        })
        .await
        .expect("timed out waiting for first routed message");
        assert_eq!(
            path, "p2p",
            "local proxy Connect must prefer installed same-replica P2P"
        );

        let conn_id = match unpack(&packed.to_bytes()).expect("decode routed message") {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9002");
                conn_id
            }
            other => panic!("expected Connect, got {other:?}"),
        };

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let mut conn = open.await.expect("join").expect("open tcp via p2p");
        conn.write_all(b"GET / HTTP/1.1\r\n\r\n")
            .await
            .expect("write");

        match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Data {
                conn_id: data_conn_id,
                payload,
            } => {
                assert_eq!(data_conn_id, conn_id);
                assert_eq!(payload, Bytes::from_static(b"GET / HTTP/1.1\r\n\r\n"));
            }
            other => panic!("expected payload Data on P2P, got {other:?}"),
        }
        assert!(
            relay_rx.try_recv().is_err(),
            "a legacy relay without attestation must not receive mesh fallback traffic"
        );
    }

    #[tokio::test]
    async fn overlay_tcp_relay_binds_exact_peer_before_connect() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let multi = make_multi(relay);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("mesh-Local001-1", multi);
        configure_mesh_identity(&engine, "198.18.1.10");
        let overlay = engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .expect("install remote Overlay route");
        let opener = ProxyTunnelOpener::new(engine.clone());
        let target = format!("{overlay}:27015");

        let open = tokio::spawn({
            let target = target.clone();
            async move { opener.open_tcp(&target).await }
        });

        let conn_id = match recv_msg(&mut relay_control_rx).await {
            BinaryMessage::RelayRouteBind {
                conn_id,
                peer_client_id,
            } => {
                assert_eq!(peer_client_id, "mesh-RemoteB1-1");
                conn_id
            }
            other => panic!("exact RelayRouteBind must precede Connect, got {other:?}"),
        };
        assert_no_queued_relay_msg(&mut relay_rx, "Connect before route bind ack");
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBindAck {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id: connect_id,
                network,
                address,
            } => {
                assert_eq!(connect_id, conn_id);
                assert_eq!(network, "tcp");
                assert_eq!(address, target);
            }
            other => panic!("expected Connect after exact route bind, got {other:?}"),
        }

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id,
                success: true,
                error: String::new(),
            })
            .await;
        open.await
            .expect("join overlay open")
            .expect("open overlay target through exact relay");
    }

    #[tokio::test]
    async fn v2_hostname_relay_routes_by_one_literal_but_keeps_the_name_for_the_peer() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_control_rx = relay_channels.control_rx;
        let multi = make_multi(relay);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let session_id = SessionId::from_bytes([0x99; 16]);
        let (local, remote) = v2_peer_pair();
        engine.install_proxy_replica_session_for_test(&local.peer.peer_id, multi);
        configure_v2_peer_pair(&engine, &local, &remote, session_id);
        engine.install_overlay_peer_for_test(&remote.peer.peer_id, Ipv4Addr::LOCALHOST);
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("localhost:27015").await });
        let conn_id = match recv_msg(&mut relay_control_rx).await {
            BinaryMessage::RelayRouteBind {
                conn_id,
                peer_client_id,
            } => {
                assert_eq!(peer_client_id, remote.peer.peer_id);
                conn_id
            }
            other => panic!("expected hostname RelayRouteBind, got {other:?}"),
        };
        let pending = engine.relay_route_bind_pending();
        let bind = pending.get(&conn_id).expect("pending exact Relay bind");
        assert_eq!(
            bind.key.logical_destination,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 27015)),
            "the source DNS result is reused for exact Relay routing"
        );
        drop(bind);

        open.abort();
        let _ = open.await;
    }

    #[tokio::test]
    async fn v2_relay_route_bind_accepts_congestion_delayed_ack() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_control_rx = relay_channels.control_rx;
        let multi = make_multi(relay);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let session_id = SessionId::from_bytes([0x9A; 16]);
        let (local, remote) = v2_peer_pair();
        engine.install_proxy_replica_session_for_test(&local.peer.peer_id, multi.clone());
        configure_v2_peer_pair(&engine, &local, &remote, session_id);

        let conn_id = "late-ack-v2";
        let bind = tokio::spawn({
            let engine = engine.clone();
            let multi = multi.clone();
            let remote_peer_id = remote.peer.peer_id.clone();
            async move {
                super::bind_relay_route_for_p2p_flow(
                    &engine,
                    &multi,
                    conn_id,
                    (Some(remote_peer_id), None),
                    true,
                    tp_core::Protocol::Tcp,
                    Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 27015))),
                )
                .await
            }
        });

        match recv_msg(&mut relay_control_rx).await {
            BinaryMessage::RelayRouteBind {
                conn_id: sent_conn_id,
                peer_client_id,
            } => {
                assert_eq!(sent_conn_id, conn_id);
                assert_eq!(peer_client_id, remote.peer.peer_id);
            }
            other => panic!("expected exact V2 RelayRouteBind, got {other:?}"),
        }

        tokio::time::sleep(Duration::from_millis(750)).await;
        assert!(
            !bind.is_finished(),
            "a healthy exact-generation bind must survive ordinary relay congestion"
        );
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBindAck {
                conn_id: conn_id.into(),
                success: true,
                error: String::new(),
            })
            .await;

        assert!(matches!(
            bind.await.expect("join delayed V2 Relay bind"),
            super::RelayRouteBindResult::Ready
        ));
    }

    #[tokio::test]
    async fn v2_exact_resolution_stays_pinned_when_profile_disappears_before_selection() {
        let relay_channels = attested_relay_session();
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let multi = make_multi(relay_channels.session);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let session_id = SessionId::from_bytes([0x9C; 16]);
        let (local, remote) = v2_peer_pair();
        engine.install_proxy_replica_session_for_test(&local.peer.peer_id, multi.clone());
        configure_v2_peer_pair(&engine, &local, &remote, session_id);

        let address = format!("{}:27015", remote.peer.overlay_ip);
        let target = engine
            .resolve_proxy_target_peer(&address)
            .await
            .expect("resolve exact V2 Peer");
        assert!(target.v2_exact_target);
        assert_eq!(
            target.peer_id.as_deref(),
            Some(remote.peer.peer_id.as_str())
        );
        engine.clear_active_v2_peer_profile_for_test();

        let conn_id = "race-tcp-02".to_string();
        let lane = engine
            .pick_and_record_proxy_flow_lane_for_peer(
                &conn_id,
                FlowKind::Tcp,
                &[],
                target.peer_id.as_deref(),
                target.v2_exact_target,
            )
            .expect("the resolved V2 lane remains pinned");
        assert_eq!(lane.path, PathKind::Relay);
        assert!(lane.v2_exact_target);
        assert_eq!(
            lane.target_peer_client_id.as_deref(),
            Some(remote.peer.peer_id.as_str()),
            "a V2 Peer ID is opaque and must not be rewritten as a Replica family"
        );

        let opener = ProxyTunnelOpener::new(engine.clone());
        let router = opener.router_for_flow_lane(&lane);
        let open = tokio::spawn({
            let address = address.clone();
            let conn_id = conn_id.clone();
            async move {
                opener
                    .open_tcp_once_on_path(
                        &address,
                        target.logical_destination,
                        conn_id,
                        lane,
                        router,
                    )
                    .await
            }
        });
        match recv_msg(&mut relay_control_rx).await {
            BinaryMessage::RelayRouteBind {
                conn_id: bound_conn_id,
                peer_client_id,
            } => {
                assert_eq!(bound_conn_id, conn_id);
                assert_eq!(peer_client_id, remote.peer.peer_id);
            }
            other => panic!("expected pinned V2 RelayRouteBind, got {other:?}"),
        }
        let pending = engine.relay_route_bind_pending();
        let (_, pending) = pending.remove(&conn_id).expect("pending route bind");
        pending.response.send(Ok(())).expect("ack route bind");

        let error = match open.await.expect("join pinned V2 open") {
            Ok(_) => panic!("profile loss must fail before a plaintext Connect"),
            Err(error) => error.error,
        };
        assert!(error.to_string().contains("authority became unavailable"));
        assert_no_queued_relay_msg(&mut relay_rx, "plain V2 TCP Connect");
    }

    #[tokio::test]
    async fn v2_activation_rejects_legacy_exact_and_generic_lanes_before_open() {
        let relay_channels = attested_relay_session();
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let multi = make_multi(relay_channels.session);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("mesh-Local001-0", multi);
        configure_mesh_identity(&engine, "198.18.1.10");
        let overlay = engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .expect("install legacy exact route");
        let address = format!("{overlay}:27015");
        let target = engine
            .resolve_proxy_target_peer(&address)
            .await
            .expect("resolve before V2 activation");
        assert!(!target.v2_exact_target);

        let conn_id = "legacy-before-v2";
        let lane = engine
            .pick_and_record_proxy_flow_lane_for_peer(
                conn_id,
                FlowKind::Tcp,
                &[],
                target.peer_id.as_deref(),
                target.v2_exact_target,
            )
            .expect("legacy exact lane before V2 activation");
        let generic_conn_id = "generic-before-v2";
        let generic_lane = engine
            .pick_and_record_proxy_flow_lane_for_peer(
                generic_conn_id,
                FlowKind::Udp,
                &[],
                None,
                false,
            )
            .expect("legacy generic lane before V2 activation");
        let (local, _remote) = v2_peer_pair();
        engine.set_active_v2_peer_profile_for_test(Arc::new(local));

        let opener = ProxyTunnelOpener::new(engine.clone());
        let error = match opener.commit_v2_exact_relay_open(conn_id, &lane) {
            Ok(_) => panic!("a stale legacy lane must not emit plaintext after V2 activation"),
            Err(error) => error,
        };
        assert!(error.error.to_string().contains("V2 routing became active"));
        let generic_error = match opener.commit_v2_exact_relay_open(generic_conn_id, &generic_lane)
        {
            Ok(_) => panic!("a stale generic lane must not emit plaintext after V2 activation"),
            Err(error) => error,
        };
        assert!(generic_error
            .error
            .to_string()
            .contains("V2 routing became active"));
        assert!(
            engine
                .pick_and_record_proxy_flow_lane_for_peer(
                    "legacy-resolve-after-v2",
                    FlowKind::Tcp,
                    &[],
                    target.peer_id.as_deref(),
                    target.v2_exact_target,
                )
                .is_none(),
            "selection must also reject a pre-V2 resolution after activation"
        );
        assert!(
            engine
                .pick_and_record_proxy_flow_lane_for_peer(
                    "generic-resolve-after-v2",
                    FlowKind::Udp,
                    &[],
                    None,
                    false,
                )
                .is_none(),
            "selection must reject a pre-V2 generic resolution after activation"
        );
        assert_no_queued_relay_msg(
            &mut relay_control_rx,
            "legacy route bind after V2 activation",
        );
        assert_no_queued_relay_msg(&mut relay_rx, "plaintext Connect after V2 activation");
    }

    #[tokio::test]
    async fn v2_exact_framed_tcp_and_udp_keep_committed_context_after_engine_cleanup() {
        let relay_channels = attested_relay_session();
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let multi = make_multi(relay_channels.session);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let session_id = SessionId::from_bytes([0x9D; 16]);
        let (local, remote) = v2_peer_pair();
        engine.install_proxy_replica_session_for_test(&local.peer.peer_id, multi);
        configure_v2_peer_pair(&engine, &local, &remote, session_id);
        let address = format!("{}:27015", remote.peer.overlay_ip);

        let tcp_open = tokio::spawn({
            let opener = ProxyTunnelOpener::new(engine.clone());
            let address = address.clone();
            async move { opener.open_tcp(&address).await }
        });
        let tcp_conn_id = match recv_msg(&mut relay_control_rx).await {
            BinaryMessage::RelayRouteBind {
                conn_id,
                peer_client_id,
            } => {
                assert_eq!(peer_client_id, remote.peer.peer_id);
                conn_id
            }
            other => panic!("expected TCP RelayRouteBind, got {other:?}"),
        };
        let pending = engine.relay_route_bind_pending();
        let (_, pending) = pending.remove(&tcp_conn_id).expect("pending TCP bind");
        pending.response.send(Ok(())).expect("ack TCP bind");
        match recv_msg(&mut relay_control_rx).await {
            BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id,
                conn_id,
                sealed,
                ..
            } => {
                assert_eq!(target_peer_id, remote.peer.peer_id);
                assert_eq!(conn_id, super::relay_conn_id_wire(&tcp_conn_id).unwrap());
                assert!(!sealed.is_empty());
            }
            other => panic!("V2 TCP Connect must be encrypted, got {other:?}"),
        }
        let pending = engine.proxy_pending();
        let (_, response) = pending.remove(&tcp_conn_id).expect("pending TCP Connect");
        response.send(Ok(())).expect("ack TCP Connect");
        let mut tcp = tcp_open.await.expect("join TCP open").expect("open TCP");
        engine.remove_proxy_flow(&tcp_conn_id);
        tcp.write_all(b"tcp-context")
            .await
            .expect("write TCP after engine cleanup");
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Data { conn_id, payload } => {
                assert_eq!(conn_id, tcp_conn_id);
                assert_ne!(payload.as_ref(), b"tcp-context");
                assert!(payload.len() > b"tcp-context".len());
            }
            other => panic!("expected sealed TCP Data, got {other:?}"),
        }

        let udp_open = tokio::spawn({
            let opener = ProxyTunnelOpener::new(engine.clone());
            let address = address.clone();
            async move { opener.open_udp(&address).await }
        });
        let udp_conn_id = match recv_msg(&mut relay_control_rx).await {
            BinaryMessage::RelayRouteBind {
                conn_id,
                peer_client_id,
            } => {
                assert_eq!(peer_client_id, remote.peer.peer_id);
                conn_id
            }
            other => panic!("expected UDP RelayRouteBind, got {other:?}"),
        };
        let pending = engine.relay_route_bind_pending();
        let (_, pending) = pending.remove(&udp_conn_id).expect("pending UDP bind");
        pending.response.send(Ok(())).expect("ack UDP bind");
        match recv_msg(&mut relay_control_rx).await {
            BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id,
                conn_id,
                sealed,
                ..
            } => {
                assert_eq!(target_peer_id, remote.peer.peer_id);
                assert_eq!(conn_id, super::relay_conn_id_wire(&udp_conn_id).unwrap());
                assert!(!sealed.is_empty());
            }
            other => panic!("V2 UDP Connect must be encrypted, got {other:?}"),
        }
        let pending = engine.proxy_pending();
        let (_, response) = pending.remove(&udp_conn_id).expect("pending UDP Connect");
        response.send(Ok(())).expect("ack UDP Connect");
        let udp = udp_open.await.expect("join UDP open").expect("open UDP");
        engine.remove_proxy_flow(&udp_conn_id);
        udp.try_send(Bytes::from_static(b"udp-context"))
            .expect("send UDP after engine cleanup");
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::UdpData { conn_id, payload } => {
                assert_eq!(conn_id, udp_conn_id);
                assert_ne!(payload.as_ref(), b"udp-context");
                assert!(payload.len() > b"udp-context".len());
            }
            other => panic!("expected sealed UDP Data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn platform_mesh_rejects_unowned_tcp_and_udp_before_any_peer_placement() {
        let (relay, mut relay_rx) = channel_session();
        let (peer_b, mut peer_b_rx) = channel_session();
        let (peer_c, mut peer_c_rx) = channel_session();
        let multi = make_multi(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0xB1; 16]),
            "mesh-RemoteB1-0",
            peer_b,
        );
        install_p2p(
            &multi,
            SessionId::from_bytes([0xC1; 16]),
            "mesh-RemoteC1-0",
            peer_c,
        );
        let engine = engine_with_multi(multi);
        let (local, remote) = v2_peer_pair();
        configure_v2_current_peer(&engine, &local, &remote);
        let opener = ProxyTunnelOpener::new_with_timeout(engine, Duration::from_millis(20));

        let tcp_error = match opener.open_tcp("192.168.50.20:27015").await {
            Ok(_) => panic!("unowned LAN target must fail closed in mesh mode"),
            Err(error) => error,
        };
        assert!(tcp_error.to_string().contains("no exact Peer route"));
        let udp_error = match opener.open_udp("203.0.113.20:27015").await {
            Ok(_) => panic!("unowned public target must fail closed in mesh mode"),
            Err(error) => error,
        };
        assert!(udp_error.to_string().contains("no exact Peer route"));

        assert!(
            peer_b_rx.try_recv().is_err(),
            "unowned traffic must not be sent to Peer B"
        );
        assert!(
            peer_c_rx.try_recv().is_err(),
            "unowned traffic must not be sent to Peer C"
        );
        assert!(
            relay_rx.try_recv().is_err(),
            "unowned traffic must not enter an unbound relay path"
        );
    }

    #[tokio::test]
    async fn relay_route_bind_waits_for_route_ready_before_fallback_data() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0x51; 16]),
            "mesh-RemoteB1-0",
            p2p.clone(),
        );
        let engine = engine_with_multi(multi);
        configure_mesh_identity(&engine, "198.18.1.10");
        let opener = ProxyTunnelOpener::new(engine.clone());

        let mut open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9002").await });

        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9002");
                conn_id
            }
            other => panic!("expected P2P Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        expect_relay_route_bind(&mut relay_control_rx, &conn_id, "mesh-RemoteB1-0").await;
        assert_no_queued_relay_msg(&mut relay_rx, "relay data before route-ready ack");
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !open.is_finished(),
            "compatible P2P open must wait for relay route-ready ack before returning a fallback-capable data router"
        );

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBindAck {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let mut conn = (&mut open)
            .await
            .expect("join")
            .expect("open tcp after route-ready ack");

        drop(p2p_rx);
        conn.write_all(b"fallback after route-ready")
            .await
            .expect("write fallback payload");
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Data {
                conn_id: data_conn_id,
                payload,
            } => {
                assert_eq!(data_conn_id, conn_id);
                assert_eq!(payload, Bytes::from_static(b"fallback after route-ready"));
            }
            other => panic!("expected relay fallback Data after ack, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn v2_direct_tcp_open_does_not_prepare_a_relay_path() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (direct, mut direct_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let session_id = SessionId::from_bytes([0x63; 16]);
        let (local, remote) = v2_peer_pair();
        install_p2p(&multi, session_id, &remote.peer.peer_id, direct);
        let engine = engine_with_multi(multi);
        configure_v2_peer_pair(&engine, &local, &remote, session_id);
        let target = format!("{}:27015", remote.peer.overlay_ip);
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn({
            let target = target.clone();
            async move { opener.open_tcp(&target).await }
        });
        let conn_id = match recv_msg(&mut direct_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, target);
                conn_id
            }
            other => panic!("expected Direct Connect, got {other:?}"),
        };
        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let mut conn = tokio::time::timeout(Duration::from_millis(100), open)
            .await
            .expect("V2 Direct open waited for Relay preparation")
            .expect("join V2 Direct open")
            .expect("open V2 Direct Flow");
        assert_no_queued_relay_msg(&mut relay_control_rx, "V2 Direct RelayRouteBind");
        assert_no_queued_relay_msg(&mut relay_rx, "V2 Direct Relay OPEN");

        conn.write_all(b"direct-only")
            .await
            .expect("write V2 Direct payload");
        assert_tcp_data(&mut direct_rx, &conn_id, b"direct-only").await;
        assert_no_queued_relay_msg(&mut relay_rx, "V2 Direct payload");
    }

    #[tokio::test]
    async fn v2_hostname_direct_routes_by_literal_ip_but_sends_the_original_name() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (direct, mut direct_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let session_id = SessionId::from_bytes([0x9a; 16]);
        let (local, remote) = v2_peer_pair();
        install_p2p(&multi, session_id, &remote.peer.peer_id, direct);
        let engine = engine_with_multi(multi);
        configure_v2_peer_pair(&engine, &local, &remote, session_id);
        engine.install_overlay_peer_for_test(&remote.peer.peer_id, Ipv4Addr::LOCALHOST);
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("localhost:27015").await });
        let conn_id = match recv_msg(&mut direct_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "localhost:27015");
                conn_id
            }
            other => panic!("expected hostname Direct Connect, got {other:?}"),
        };
        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::ConnectResponse {
                conn_id,
                success: true,
                error: String::new(),
            })
            .await;
        open.await
            .expect("join hostname Direct open")
            .expect("open hostname over exact Direct route");
        assert_no_queued_relay_msg(&mut relay_control_rx, "hostname Direct RelayRouteBind");
        assert_no_queued_relay_msg(&mut relay_rx, "hostname Direct Relay OPEN");
    }

    #[tokio::test]
    async fn v2_direct_tcp_open_failure_closes_the_flow_without_relay_migration() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (direct, direct_rx) = channel_session();
        drop(direct_rx);
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let session_id = SessionId::from_bytes([0x64; 16]);
        let (local, remote) = v2_peer_pair();
        install_p2p(&multi, session_id, &remote.peer.peer_id, direct);
        let engine = engine_with_multi(multi);
        configure_v2_peer_pair(&engine, &local, &remote, session_id);
        let target = format!("{}:27015", remote.peer.overlay_ip);
        let opener = ProxyTunnelOpener::new_with_timeout(engine, Duration::from_millis(30));

        let error = match opener.open_tcp(&target).await {
            Ok(_) => panic!("failed V2 Direct carrier must close this Flow"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("closed"),
            "unexpected Direct failure: {error:#}"
        );
        assert_no_queued_relay_msg(&mut relay_control_rx, "failed V2 Direct bind");
        assert_no_queued_relay_msg(&mut relay_rx, "failed V2 Direct migration");
    }

    #[tokio::test]
    async fn v2_direct_tcp_open_timeout_does_not_retry_the_application_flow_on_relay() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (direct, mut direct_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let session_id = SessionId::from_bytes([0x65; 16]);
        let (local, remote) = v2_peer_pair();
        install_p2p(&multi, session_id, &remote.peer.peer_id, direct);
        let engine = engine_with_multi(multi);
        configure_v2_peer_pair(&engine, &local, &remote, session_id);
        let target = format!("{}:27015", remote.peer.overlay_ip);
        let opener = ProxyTunnelOpener::new_with_timeout(engine, Duration::from_millis(20));

        let open = tokio::spawn(async move { opener.open_tcp(&target).await });
        match recv_msg(&mut direct_rx).await {
            BinaryMessage::Connect { network, .. } => assert_eq!(network, "tcp"),
            other => panic!("expected Direct Connect, got {other:?}"),
        }
        let result = tokio::time::timeout(Duration::from_millis(100), open)
            .await
            .expect("V2 Direct timeout retried this application Flow")
            .expect("join timed-out V2 Direct open");
        let error = match result {
            Ok(_) => panic!("timed-out V2 Direct Flow must close"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("timed out"));
        assert_no_queued_relay_msg(&mut relay_control_rx, "timed-out V2 Direct bind");
        assert_no_queued_relay_msg(&mut relay_rx, "timed-out V2 Direct retry");
    }

    #[tokio::test]
    async fn v2_direct_udp_open_does_not_prepare_or_migrate_to_relay() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (direct, mut direct_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let session_id = SessionId::from_bytes([0x66; 16]);
        let (local, remote) = v2_peer_pair();
        install_p2p(&multi, session_id, &remote.peer.peer_id, direct);
        let engine = engine_with_multi(multi);
        configure_v2_peer_pair(&engine, &local, &remote, session_id);
        let target = format!("{}:27015", remote.peer.overlay_ip);
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn({
            let target = target.clone();
            async move { opener.open_udp(&target).await }
        });
        let conn_id = match recv_msg(&mut direct_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, target);
                conn_id
            }
            other => panic!("expected Direct UDP Connect, got {other:?}"),
        };
        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let datagram = tokio::time::timeout(Duration::from_millis(100), open)
            .await
            .expect("V2 Direct UDP open waited for Relay preparation")
            .expect("join V2 Direct UDP open")
            .expect("open V2 Direct UDP Flow");
        assert_no_queued_relay_msg(&mut relay_control_rx, "V2 Direct UDP bind");
        assert_no_queued_relay_msg(&mut relay_rx, "V2 Direct UDP OPEN");

        datagram
            .try_send(Bytes::from_static(b"direct-udp"))
            .expect("send V2 Direct UDP payload");
        match recv_msg(&mut direct_rx).await {
            BinaryMessage::UdpData {
                conn_id: data_conn_id,
                payload,
            } => {
                assert_eq!(data_conn_id, conn_id);
                assert_eq!(payload, Bytes::from_static(b"direct-udp"));
            }
            other => panic!("expected Direct UdpData, got {other:?}"),
        }
        assert_no_queued_relay_msg(&mut relay_rx, "V2 Direct UDP payload");
    }

    #[tokio::test]
    async fn v2_direct_udp_open_timeout_does_not_retry_the_application_flow_on_relay() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (direct, mut direct_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let session_id = SessionId::from_bytes([0x67; 16]);
        let (local, remote) = v2_peer_pair();
        install_p2p(&multi, session_id, &remote.peer.peer_id, direct);
        let engine = engine_with_multi(multi);
        configure_v2_peer_pair(&engine, &local, &remote, session_id);
        let target = format!("{}:27015", remote.peer.overlay_ip);
        let opener = ProxyTunnelOpener::new_with_timeout(engine, Duration::from_millis(20));

        let open = tokio::spawn(async move { opener.open_udp(&target).await });
        match recv_msg(&mut direct_rx).await {
            BinaryMessage::Connect { network, .. } => assert_eq!(network, "udp"),
            other => panic!("expected Direct UDP Connect, got {other:?}"),
        }
        let result = tokio::time::timeout(Duration::from_millis(100), open)
            .await
            .expect("V2 Direct UDP timeout retried this application Flow")
            .expect("join timed-out V2 Direct UDP open");
        let error = match result {
            Ok(_) => panic!("timed-out V2 Direct UDP Flow must close"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("timed out"));
        assert_no_queued_relay_msg(&mut relay_control_rx, "timed-out V2 Direct UDP bind");
        assert_no_queued_relay_msg(&mut relay_rx, "timed-out V2 Direct UDP retry");
    }

    #[tokio::test]
    async fn relay_route_bind_ignores_p2p_origin_ack() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0x56; 16]),
            "mesh-RemoteB1-0",
            p2p.clone(),
        );
        let engine = engine_with_multi(multi);
        configure_mesh_identity(&engine, "198.18.1.10");
        let opener = ProxyTunnelOpener::new(engine.clone());

        let mut open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9002").await });

        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected P2P Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        expect_relay_route_bind(&mut relay_control_rx, &conn_id, "mesh-RemoteB1-0").await;
        engine
            .handle_msg_from_p2p_session_for_test(
                BinaryMessage::RelayRouteBindAck {
                    conn_id: conn_id.clone(),
                    success: true,
                    error: String::new(),
                },
                Some(p2p.clone()),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !open.is_finished(),
            "P2P-origin route bind ack must not release the relay route-ready gate"
        );

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBindAck {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let mut conn = (&mut open)
            .await
            .expect("join")
            .expect("open tcp after relay-origin route bind ack");

        drop(p2p_rx);
        conn.write_all(b"fallback after relay-origin ack")
            .await
            .expect("write fallback payload");
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Data {
                conn_id: data_conn_id,
                payload,
            } => {
                assert_eq!(data_conn_id, conn_id);
                assert_eq!(
                    payload,
                    Bytes::from_static(b"fallback after relay-origin ack")
                );
            }
            other => panic!("expected relay fallback Data after relay ack, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn relay_route_bind_ack_failure_disables_relay_fallback_after_p2p_close() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0x53; 16]),
            "mesh-RemoteB1-0",
            p2p.clone(),
        );
        let engine = engine_with_multi(multi);
        configure_mesh_identity(&engine, "198.18.1.10");
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9002").await });

        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected P2P Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        expect_relay_route_bind(&mut relay_control_rx, &conn_id, "mesh-RemoteB1-0").await;
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBindAck {
                conn_id: conn_id.clone(),
                success: false,
                error: "route validation failed".into(),
            })
            .await;
        let mut conn = open
            .await
            .expect("join")
            .expect("open tcp after route-ready rejection");

        conn.write_all(b"p2p before close")
            .await
            .expect("write p2p");
        conn.flush().await.expect("flush p2p");
        assert_tcp_data(&mut p2p_rx, &conn_id, b"p2p before close").await;
        assert_no_queued_relay_msg(&mut relay_rx, "relay data before P2P close");

        drop(p2p_rx);
        let _ = conn.write_all(b"no relay fallback after close").await;
        let _ = conn.flush().await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_no_queued_relay_msg(&mut relay_rx, "relay data after rejected route bind");
    }

    #[tokio::test]
    async fn relay_route_bind_ack_timeout_disables_relay_fallback_after_p2p_close() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0x55; 16]),
            "mesh-RemoteB1-0",
            p2p.clone(),
        );
        let engine = engine_with_multi(multi);
        configure_mesh_identity(&engine, "198.18.1.10");
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9002").await });

        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected P2P Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        expect_relay_route_bind(&mut relay_control_rx, &conn_id, "mesh-RemoteB1-0").await;
        let mut conn = tokio::time::timeout(
            super::RELAY_ROUTE_BIND_TIMEOUT + Duration::from_millis(250),
            open,
        )
        .await
        .expect("open must return after route bind ack timeout")
        .expect("join")
        .expect("open tcp after route bind ack timeout");

        conn.write_all(b"p2p after timeout")
            .await
            .expect("write p2p");
        conn.flush().await.expect("flush p2p");
        assert_tcp_data(&mut p2p_rx, &conn_id, b"p2p after timeout").await;
        assert_no_queued_relay_msg(&mut relay_rx, "relay data before timed-out P2P close");

        drop(p2p_rx);
        let _ = conn.write_all(b"no relay fallback after timeout").await;
        let _ = conn.flush().await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_no_queued_relay_msg(&mut relay_rx, "relay data after route bind timeout");
    }

    #[tokio::test]
    async fn relay_route_bind_missing_exact_peer_disables_relay_fallback_after_p2p_close() {
        let relay_channels = channel_session_with_capabilities(TransportCapabilities {
            route_bind_control_v1: true,
            tcp_flow_stream_v1: false,
            relay_source_attestation_v1: false,
            peer_mesh_v2: false,
        });
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0x54; 16]),
            "__legacy_single_p2p_peer__",
            p2p.clone(),
        );
        let engine = engine_with_multi(multi);
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9002").await });

        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected P2P Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let mut conn = open
            .await
            .expect("join")
            .expect("open tcp without exact relay route peer");
        assert_no_queued_relay_msg(&mut relay_control_rx, "relay route bind without exact peer");

        conn.write_all(b"p2p without peer")
            .await
            .expect("write p2p");
        conn.flush().await.expect("flush p2p");
        assert_tcp_data(&mut p2p_rx, &conn_id, b"p2p without peer").await;
        assert_no_queued_relay_msg(&mut relay_rx, "relay data before missing-peer P2P close");

        drop(p2p_rx);
        let _ = conn.write_all(b"no relay fallback without peer").await;
        let _ = conn.flush().await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_no_queued_relay_msg(&mut relay_rx, "relay data after missing route peer");
    }

    #[tokio::test]
    async fn legacy_relay_without_attestation_disables_mesh_fallback() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0x52; 16]),
            "pc-main",
            p2p.clone(),
        );
        let engine = engine_with_multi(multi);
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9002").await });

        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected P2P Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let mut conn = open
            .await
            .expect("join")
            .expect("open tcp through legacy peer");
        assert_no_queued_relay_msg(&mut relay_rx, "legacy relay route bind");
        conn.write_all(b"legacy p2p payload")
            .await
            .expect("write direct payload");
        conn.flush().await.expect("flush direct payload");
        assert_tcp_data(&mut p2p_rx, &conn_id, b"legacy p2p payload").await;
        drop(p2p_rx);
        let _ = conn.write_all(b"no unauthenticated fallback").await;
        let _ = conn.flush().await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_no_queued_relay_msg(&mut relay_rx, "legacy relay fallback");
    }

    #[tokio::test]
    async fn open_tcp_sticks_to_relay_when_p2p_is_installed_after_connect() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9003").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9003");
                conn_id
            }
            other => panic!("expected relay Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let mut conn = open.await.expect("join").expect("open tcp via relay");
        install_p2p(&multi, SessionId::from_bytes([0x43; 16]), "pc-main", p2p);

        conn.write_all(b"GET /still-relay HTTP/1.1\r\n\r\n")
            .await
            .expect("write");

        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Data {
                conn_id: data_conn_id,
                payload,
            } => {
                assert_eq!(data_conn_id, conn_id);
                assert_eq!(
                    payload,
                    Bytes::from_static(b"GET /still-relay HTTP/1.1\r\n\r\n")
                );
            }
            other => panic!("expected relay Data after late P2P install, got {other:?}"),
        }
        assert!(
            p2p_rx.try_recv().is_err(),
            "relay flow must not migrate to late-installed P2P"
        );
    }

    #[tokio::test]
    async fn open_udp_sticks_to_relay_when_p2p_is_installed_after_connect() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9004").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9004");
                conn_id
            }
            other => panic!("expected relay UDP Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let datagram = open.await.expect("join").expect("open udp via relay");
        install_p2p(&multi, SessionId::from_bytes([0x44; 16]), "pc-main", p2p);

        datagram
            .try_send(Bytes::from_static(b"still-relay"))
            .expect("relay UDP send");

        match recv_msg(&mut relay_rx).await {
            BinaryMessage::UdpData {
                conn_id: data_conn_id,
                payload,
            } => {
                assert_eq!(data_conn_id, conn_id);
                assert_eq!(payload, Bytes::from_static(b"still-relay"));
            }
            other => panic!("expected relay UDP data after late P2P install, got {other:?}"),
        }
        assert!(
            p2p_rx.try_recv().is_err(),
            "relay UDP flow must not migrate to late-installed P2P"
        );
    }

    #[tokio::test]
    async fn open_tcp_accepts_inbound_p2p_replies_for_existing_conn() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        multi.set_p2p(Some(p2p));
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9004").await });

        let (path, packed) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                packed = p2p_rx.recv() => ("p2p", packed.expect("p2p routed message channel closed")),
                packed = relay_rx.recv() => ("relay", packed.expect("relay routed message channel closed")),
            }
        })
        .await
        .expect("timed out waiting for routed Connect");
        assert_eq!(path, "p2p");

        let conn_id = match unpack(&packed.to_bytes()).expect("decode routed message") {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected Connect, got {other:?}"),
        };
        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let mut conn = open.await.expect("join").expect("open tcp via p2p");

        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::Data {
                conn_id,
                payload: Bytes::from_static(b"pong"),
            })
            .await;

        let mut buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(1), conn.read_exact(&mut buf))
            .await
            .expect("timed out waiting for inbound P2P data")
            .expect("read inbound P2P data");
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn open_tcp_round_robins_across_installed_p2p_replicas() {
        let (relay_a, mut relay_a_rx) = channel_session();
        let (relay_b, mut relay_b_rx) = channel_session();
        let (p2p_a, mut p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        let multi_a = make_multi_with_p2p_first_scheduler(relay_a);
        let multi_b = make_multi_with_p2p_first_scheduler(relay_b);
        multi_a.set_p2p(Some(p2p_a));
        multi_b.set_p2p(Some(p2p_b));

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("client-a", multi_a);
        engine.install_proxy_replica_session_for_test("client-b", multi_b);

        let opener = ProxyTunnelOpener::new(engine.clone());
        let first_open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9002").await });
        let first_conn_id = match recv_msg(&mut p2p_a_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9002");
                conn_id
            }
            other => panic!("expected first TCP Connect on P2P A, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: first_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let _first = first_open.await.expect("join").expect("first tcp open");

        let opener = ProxyTunnelOpener::new(engine.clone());
        let second_open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9003").await });
        let second_conn_id = match recv_msg(&mut p2p_b_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9003");
                conn_id
            }
            other => panic!("expected second TCP Connect on P2P B, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: second_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let _second = second_open.await.expect("join").expect("second tcp open");

        assert_no_queued_relay_msg(&mut relay_a_rx, "relay A");
        assert_no_queued_relay_msg(&mut relay_b_rx, "relay B");
    }

    #[tokio::test]
    async fn open_tcp_retries_next_relay_replica_after_selected_relay_closed() {
        let (relay_a, relay_a_rx) = channel_session();
        drop(relay_a_rx);
        let (relay_b, mut relay_b_rx) = channel_session();
        let multi_a = make_multi(relay_a);
        let multi_b = make_multi(relay_b);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_anchor_client_id_for_test("client-b");
        engine.install_proxy_replica_session_for_test("client-a", multi_a);
        engine.install_proxy_replica_session_for_test("client-b", multi_b);
        let opener = ProxyTunnelOpener::new(engine.clone());
        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9006").await });

        let conn_id = match recv_msg(&mut relay_b_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9006");
                conn_id
            }
            other => panic!("expected TCP Connect on retry replica, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id,
                success: true,
                error: String::new(),
            })
            .await;
        let _conn = open.await.expect("join").expect("tcp retry open");

        assert_eq!(
            engine
                .pick_proxy_relay_lane()
                .expect("remaining relay lane")
                .local_client_id,
            "client-b"
        );
    }

    #[tokio::test]
    async fn dropped_tcp_tunnel_removes_flow_placement_before_next_open() {
        let (relay_a, mut relay_a_rx) = channel_session();
        let (relay_b, mut relay_b_rx) = channel_session();
        let (relay_c, mut relay_c_rx) = channel_session();
        let (p2p_a, mut p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        let (p2p_c, mut p2p_c_rx) = channel_session();
        let multi_a = make_multi_with_p2p_first_scheduler(relay_a);
        let multi_b = make_multi_with_p2p_first_scheduler(relay_b);
        let multi_c = make_multi_with_p2p_first_scheduler(relay_c);
        multi_a.set_p2p(Some(p2p_a));
        multi_b.set_p2p(Some(p2p_b));
        multi_c.set_p2p(Some(p2p_c));

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("client-a", multi_a);
        engine.install_proxy_replica_session_for_test("client-b", multi_b);
        engine.install_proxy_replica_session_for_test("client-c", multi_c);

        let opener = ProxyTunnelOpener::new(engine.clone());
        let first_open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9020").await });
        let first_conn_id = match recv_msg(&mut p2p_a_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected first TCP Connect on P2P A, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: first_conn_id,
                success: true,
                error: String::new(),
            })
            .await;
        let first = first_open.await.expect("join").expect("first tcp open");
        drop(first);

        let opener = ProxyTunnelOpener::new(engine.clone());
        let second_open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9021").await });
        let second_conn_id = match recv_msg(&mut p2p_b_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected second TCP Connect on P2P B, got {other:?}"),
        };
        assert_no_queued_relay_msg(&mut p2p_c_rx, "p2p C");
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: second_conn_id,
                success: true,
                error: String::new(),
            })
            .await;
        let _second = second_open.await.expect("join").expect("second tcp open");

        assert_no_queued_relay_msg(&mut relay_a_rx, "relay A");
        assert_no_queued_relay_msg(&mut relay_b_rx, "relay B");
        assert_no_queued_relay_msg(&mut relay_c_rx, "relay C");
    }

    #[tokio::test]
    async fn cancelled_tcp_open_removes_pending_flow_placement() {
        let (relay, _relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        install_p2p(&multi, SessionId::from_bytes([0x91; 16]), "pc-a", p2p);

        let engine = engine_with_multi(multi);
        let opener = ProxyTunnelOpener::new(engine.clone());
        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9120").await });
        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected TCP Connect on P2P, got {other:?}"),
        };
        assert!(
            engine.proxy_flow_candidate_key_for_test(&conn_id).is_some(),
            "placement must be recorded pending before Connect ack"
        );

        open.abort();
        let _ = open.await;

        assert_eq!(
            engine.proxy_flow_candidate_key_for_test(&conn_id),
            None,
            "dropping an in-flight open must remove its pending placement"
        );
    }

    #[tokio::test]
    async fn connect_send_fallback_to_relay_rekeys_flow_placement() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, p2p_rx) = channel_session();
        drop(p2p_rx);
        let multi = make_multi_with_p2p_first_scheduler(relay);
        install_p2p(&multi, SessionId::from_bytes([0x92; 16]), "pc-a", p2p);

        let engine = engine_with_multi(multi);
        let opener = ProxyTunnelOpener::new(engine.clone());
        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9121").await });
        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected TCP Connect fallback on relay, got {other:?}"),
        };
        let key = engine
            .proxy_flow_candidate_key_for_test(&conn_id)
            .expect("placement should remain recorded after successful Connect send");
        assert_eq!(key.path, CandidatePath::Relay);
        assert_eq!(key.local_client_id, "anchor");

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id,
                success: true,
                error: String::new(),
            })
            .await;
        let _conn = open.await.expect("join").expect("tcp open");
    }

    #[tokio::test]
    async fn dropped_direct_udp_tunnel_removes_flow_placement() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let engine = engine_with_multi(multi);
        let opener = ProxyTunnelOpener::new(engine.clone());
        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9122").await });
        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected UDP Connect on relay, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let datagram = open.await.expect("join").expect("udp open");
        assert!(
            engine.proxy_flow_candidate_key_for_test(&conn_id).is_some(),
            "established direct UDP tunnel should own a placement"
        );

        drop(datagram);

        assert_eq!(
            engine.proxy_flow_candidate_key_for_test(&conn_id),
            None,
            "dropping direct UDP tunnel must remove its placement"
        );
    }

    #[tokio::test]
    async fn open_udp_binds_relay_route_after_p2p_ack_without_duplicate_connect() {
        let relay_channels = attested_relay_session();
        let relay = relay_channels.session;
        let mut relay_rx = relay_channels.data_rx;
        let mut relay_control_rx = relay_channels.control_rx;
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([0x95; 16]),
            "mesh-RemoteB1-0",
            p2p,
        );
        let engine = engine_with_multi(multi.clone());
        configure_mesh_identity(&engine, "198.18.1.10");
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9005").await });
        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9005");
                conn_id
            }
            other => panic!("expected P2P UDP Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        expect_relay_route_bind(&mut relay_control_rx, &conn_id, "mesh-RemoteB1-0").await;
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBindAck {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let _datagram = open.await.expect("join").expect("open udp via p2p");
        assert_no_queued_relay_msg(&mut relay_rx, "relay");
    }

    #[tokio::test]
    async fn open_udp_pins_payloads_to_selected_p2p_session() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p_a, mut p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        install_p2p(
            &multi,
            SessionId::from_bytes([1u8; 16]),
            "pc-replica-a",
            p2p_a,
        );
        install_p2p(
            &multi,
            SessionId::from_bytes([2u8; 16]),
            "pc-replica-b",
            p2p_b,
        );
        let engine = engine_with_multi(multi);
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9005").await });
        let (selected, conn_id) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                msg = p2p_a_rx.recv() => ("a", msg.expect("p2p A channel closed")),
                msg = p2p_b_rx.recv() => ("b", msg.expect("p2p B channel closed")),
            }
        })
        .await
        .expect("timed out waiting for P2P UDP Connect");
        let conn_id = match unpack(&conn_id.to_bytes()).expect("decode P2P connect") {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9005");
                conn_id
            }
            other => panic!("expected P2P UDP Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let datagram = open.await.expect("join").expect("open udp via p2p");
        assert_no_queued_relay_msg(&mut relay_rx, "legacy UDP relay route bind");
        datagram
            .try_send(Bytes::from_static(b"frame"))
            .expect("send UDP payload on pinned P2P session");

        let selected_rx = if selected == "a" {
            &mut p2p_a_rx
        } else {
            &mut p2p_b_rx
        };
        match recv_msg(selected_rx).await {
            BinaryMessage::UdpData {
                conn_id: got_conn_id,
                payload,
            } => {
                assert_eq!(got_conn_id, conn_id);
                assert_eq!(payload, Bytes::from_static(b"frame"));
            }
            other => panic!("expected UDP payload on selected P2P session, got {other:?}"),
        }

        if selected == "a" {
            assert_no_queued_relay_msg(&mut p2p_b_rx, "p2p B");
        } else {
            assert_no_queued_relay_msg(&mut p2p_a_rx, "p2p A");
        }
    }

    #[tokio::test]
    async fn p2p_inbound_connect_replies_via_same_replica_p2p_before_relay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind target listener");
        let target = listener.local_addr().expect("target local addr");
        let accept = tokio::spawn(async move {
            let (_stream, _peer) = listener.accept().await.expect("accept target connection");
        });

        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let p2p_handle = p2p.clone();
        let multi = make_multi(relay);
        install_relation_p2p(
            &multi,
            SessionId::from_bytes([0x71; 16]),
            "mesh-RemoteB1-0",
            "mesh-RemoteB1-1",
            p2p,
        );
        let engine = engine_with_multi(multi);
        configure_overlay_export(&engine, LocalServiceProtocolConfig::Tcp, target);
        let conn_id = "p2p-inbound-";

        engine
            .handle_msg_from_p2p_session_for_test(
                BinaryMessage::Connect {
                    conn_id: conn_id.into(),
                    network: "tcp".into(),
                    address: target.to_string(),
                },
                Some(p2p_handle),
            )
            .await;

        let (path, msg) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                msg = p2p_rx.recv() => ("p2p", msg.expect("p2p routed message channel closed")),
                msg = relay_rx.recv() => ("relay", msg.expect("relay routed message channel closed")),
            }
        })
        .await
        .expect("timed out waiting for ConnectResponse");

        assert_eq!(path, "p2p");
        match unpack(&msg.to_bytes()).expect("decode routed message") {
            BinaryMessage::ConnectResponse {
                conn_id,
                success,
                error,
            } => {
                assert_eq!(conn_id, "p2p-inbound-");
                assert!(success, "unexpected ConnectResponse error: {error}");
            }
            other => panic!("expected ConnectResponse, got {other:?}"),
        }

        accept.abort();
    }

    #[tokio::test]
    async fn p2p_inbound_udp_connect_response_prefers_same_replica_p2p() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let p2p_handle = p2p.clone();
        let multi = make_multi(relay);
        install_relation_p2p(
            &multi,
            SessionId::from_bytes([0x72; 16]),
            "mesh-RemoteB1-0",
            "mesh-RemoteB1-1",
            p2p,
        );
        let engine = engine_with_multi(multi);
        configure_overlay_export(
            &engine,
            LocalServiceProtocolConfig::Udp,
            "127.0.0.1:9".parse().expect("UDP export target"),
        );
        let conn_id = "p2p-udp-0001";

        engine
            .handle_msg_from_p2p_session_for_test(
                BinaryMessage::Connect {
                    conn_id: conn_id.into(),
                    network: "udp".into(),
                    address: "127.0.0.1:9".to_string(),
                },
                Some(p2p_handle),
            )
            .await;

        let (path, msg) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                msg = p2p_rx.recv() => ("p2p", msg.expect("p2p routed message channel closed")),
                msg = relay_rx.recv() => ("relay", msg.expect("relay routed message channel closed")),
            }
        })
        .await
        .expect("timed out waiting for UDP ConnectResponse");

        assert_eq!(path, "p2p");
        match unpack(&msg.to_bytes()).expect("decode routed message") {
            BinaryMessage::ConnectResponse {
                conn_id,
                success,
                error,
            } => {
                assert_eq!(conn_id, "p2p-udp-0001");
                assert!(success, "unexpected ConnectResponse error: {error}");
            }
            other => panic!("expected ConnectResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p2p_inbound_udp_connect_response_is_pinned_to_ingress_session() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p_a, mut p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        let p2p_b_handle = p2p_b.clone();
        let multi = make_multi(relay);
        install_relation_p2p(
            &multi,
            SessionId::from_bytes([11u8; 16]),
            "mesh-RemoteA1-0",
            "mesh-RemoteA1-1",
            p2p_a,
        );
        install_relation_p2p(
            &multi,
            SessionId::from_bytes([12u8; 16]),
            "mesh-RemoteB1-0",
            "mesh-RemoteB1-1",
            p2p_b,
        );
        let engine = engine_with_multi(multi);
        configure_overlay_export(
            &engine,
            LocalServiceProtocolConfig::Udp,
            "127.0.0.1:9".parse().expect("UDP export target"),
        );
        let conn_id = "p2p-udp-pin";

        engine
            .handle_msg_from_p2p_session_for_test(
                BinaryMessage::Connect {
                    conn_id: conn_id.into(),
                    network: "udp".into(),
                    address: "127.0.0.1:9".to_string(),
                },
                Some(p2p_b_handle),
            )
            .await;

        let msg = recv_msg(&mut p2p_b_rx).await;
        match msg {
            BinaryMessage::ConnectResponse {
                conn_id,
                success,
                error,
            } => {
                assert_eq!(conn_id, "p2p-udp-pin");
                assert!(success, "unexpected ConnectResponse error: {error}");
            }
            other => panic!("expected ConnectResponse on ingress P2P session, got {other:?}"),
        }
        assert_no_queued_relay_msg(&mut p2p_a_rx, "p2p A");
        assert_no_queued_relay_msg(&mut relay_rx, "relay");

        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::Close {
                conn_id: conn_id.into(),
            })
            .await;
    }

    #[tokio::test]
    async fn relay_inbound_connect_stays_on_relay_even_when_p2p_is_healthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind target listener");
        let target = listener.local_addr().expect("target local addr");
        let response = Bytes::from_static(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
        let expected_response = response.clone();
        let accept = tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept target connection");
            stream.write_all(&response).await.expect("write response");
            stream.shutdown().await.expect("shutdown response half");
        });

        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        multi.set_p2p(Some(p2p));
        let engine = engine_with_multi(multi);
        let conn_id = "relay-in-001";

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::Connect {
                conn_id: conn_id.into(),
                network: "tcp".into(),
                address: target.to_string(),
            })
            .await;

        let (path, msg) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                msg = p2p_rx.recv() => ("p2p", msg.expect("p2p routed message channel closed")),
                msg = relay_rx.recv() => ("relay", msg.expect("relay routed message channel closed")),
            }
        })
        .await
        .expect("timed out waiting for ConnectResponse");

        assert_eq!(
            path, "relay",
            "relay-originated TCP flows must keep replies on relay so P2P fallback does not re-enter a congested direct path"
        );
        match unpack(&msg.to_bytes()).expect("decode routed message") {
            BinaryMessage::ConnectResponse {
                conn_id,
                success,
                error,
            } => {
                assert_eq!(conn_id, "relay-in-001");
                assert!(success, "unexpected ConnectResponse error: {error}");
            }
            other => panic!("expected ConnectResponse, got {other:?}"),
        }

        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Data { conn_id, payload } => {
                assert_eq!(conn_id, "relay-in-001");
                assert_eq!(payload, expected_response);
            }
            other => panic!("expected relay Data, got {other:?}"),
        }
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Close { conn_id } => assert_eq!(conn_id, "relay-in-001"),
            other => panic!("expected relay Close, got {other:?}"),
        }
        assert_no_queued_relay_msg(&mut p2p_rx, "p2p");

        accept.await.expect("accept task");
    }

    #[tokio::test]
    async fn inbound_tcp_connect_installs_slot_before_success_response_send() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind target listener");
        let target = listener.local_addr().expect("target local addr");
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let accept = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept target connection");
            let _ = accepted_tx.send(());
            std::future::pending::<()>().await;
            drop(stream);
        });

        let (out_tx, mut relay_rx) = mpsc::channel::<PackedMessage>(1);
        out_tx
            .try_send(pack(&BinaryMessage::HeartbeatAck { timestamp: 1 }))
            .expect("prefill relay queue");
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let relay = Arc::new(Session::new_channeled(
            out_tx, in_rx, peer, closer, writer, reader,
        ));
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let conn_id = "relay-inbound-slot";

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::Connect {
                conn_id: conn_id.into(),
                network: "tcp".into(),
                address: target.to_string(),
            })
            .await;

        accepted_rx.await.expect("target accepted connection");
        tokio::task::yield_now().await;
        let slot_installed_before_unblock = multi.inbound().contains_key(conn_id);

        let _ = relay_rx.recv().await.expect("drain queue prefill");
        let _ = tokio::time::timeout(Duration::from_secs(1), relay_rx.recv())
            .await
            .expect("timed out waiting for ConnectResponse")
            .expect("relay queue closed before ConnectResponse");
        accept.abort();

        assert!(
            slot_installed_before_unblock,
            "target-side inbound slot must exist before ConnectResponse send can unblock"
        );
    }

    #[tokio::test]
    async fn inbound_udp_connect_installs_slot_before_success_response_send() {
        let (out_tx, mut relay_rx) = mpsc::channel::<PackedMessage>(1);
        out_tx
            .try_send(pack(&BinaryMessage::HeartbeatAck { timestamp: 1 }))
            .expect("prefill relay queue");
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let relay = Arc::new(Session::new_channeled(
            out_tx, in_rx, peer, closer, writer, reader,
        ));
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let conn_id = "relay-inbound-udp-slot";

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::Connect {
                conn_id: conn_id.into(),
                network: "udp".into(),
                address: "127.0.0.1:9".into(),
            })
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;
        let slot_installed_before_unblock = multi.udp_inbound().contains_key(conn_id);

        let _ = relay_rx.recv().await.expect("drain queue prefill");
        let _ = tokio::time::timeout(Duration::from_secs(1), relay_rx.recv())
            .await
            .expect("timed out waiting for ConnectResponse")
            .expect("relay queue closed before ConnectResponse");

        assert!(
            slot_installed_before_unblock,
            "target-side UDP slot must exist before ConnectResponse send can unblock"
        );
    }

    #[tokio::test]
    async fn open_tcp_returns_refused_ack_error_and_cleans_pending() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:1").await });
        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: false,
                error: "refused".into(),
            })
            .await;

        let err = match open.await.expect("join") {
            Ok(_) => panic!("open must fail"),
            Err(err) => err,
        };
        assert!(format!("{err:#}").contains("refused"));
        assert!(!engine.proxy_pending_contains_for_test(&conn_id));
        assert!(!multi.inbound().contains_key(&conn_id));
    }

    #[tokio::test]
    async fn open_tcp_timeout_cleans_pending() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:2").await });
        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected Connect, got {other:?}"),
        };
        assert!(engine.proxy_pending_contains_for_test(&conn_id));

        let err = match open.await.expect("join") {
            Ok(_) => panic!("open must time out"),
            Err(err) => err,
        };
        assert!(format!("{err:#}").contains("timed out"));
        assert!(!engine.proxy_pending_contains_for_test(&conn_id));
        assert!(!multi.inbound().contains_key(&conn_id));
    }

    #[tokio::test]
    async fn open_tcp_p2p_ack_timeout_uses_relay_only_when_no_live_p2p_candidate() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        multi.set_p2p(Some(p2p));
        let engine = engine_with_multi(multi.clone());
        let metrics = MetricsManager::new();
        engine.set_metrics(Some(metrics.clone()));
        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));
        let p2p_count_before = multi.p2p_session_count();

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9001").await });

        let stale_conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9001");
                conn_id
            }
            other => panic!("expected first Connect on stale P2P, got {other:?}"),
        };

        let relay_conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9001");
                conn_id
            }
            other => panic!("expected retry Connect on relay, got {other:?}"),
        };
        assert_ne!(
            stale_conn_id, relay_conn_id,
            "relay retry must use a fresh conn_id so a late stale P2P ack cannot satisfy it"
        );
        assert_p2p_session_count_unchanged(&multi, p2p_count_before, "TCP P2P connect timeout");
        assert_no_p2p_to_relay_migration_metric(&metrics);
        assert!(!engine.proxy_pending_contains_for_test(&stale_conn_id));
        assert!(!multi.inbound().contains_key(&stale_conn_id));

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: stale_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        assert!(
            engine.proxy_pending_contains_for_test(&relay_conn_id),
            "a late ack for the timed-out P2P conn_id must not complete the relay retry"
        );
        assert!(
            !multi.inbound().contains_key(&stale_conn_id),
            "late stale ack must not install the timed-out inbound slot"
        );

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: relay_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _conn = open.await.expect("join").expect("open tcp via relay");
        assert!(multi.inbound().contains_key(&relay_conn_id));
        assert!(!engine.proxy_pending_contains_for_test(&relay_conn_id));
        assert_p2p_session_count_unchanged(&multi, p2p_count_before, "TCP relay retry completion");
        assert_no_p2p_to_relay_migration_metric(&metrics);
    }

    #[tokio::test]
    async fn open_tcp_p2p_ack_timeout_falls_back_to_relay_without_trying_other_p2p() {
        let (relay_a, mut relay_a_rx) = channel_session();
        let (relay_b, mut relay_b_rx) = channel_session();
        let (p2p_a, mut p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        let multi_a = make_multi_with_p2p_first_scheduler(relay_a);
        let multi_b = make_multi_with_p2p_first_scheduler(relay_b);
        multi_a.set_p2p(Some(p2p_a));
        multi_b.set_p2p(Some(p2p_b));

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("client-a", multi_a.clone());
        engine.install_proxy_replica_session_for_test("client-b", multi_b.clone());
        let metrics = MetricsManager::new();
        engine.set_metrics(Some(metrics.clone()));

        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));
        let p2p_a_count_before = multi_a.p2p_session_count();
        let p2p_b_count_before = multi_b.p2p_session_count();
        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9013").await });

        let stale_conn_id = match recv_msg(&mut p2p_a_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9013");
                conn_id
            }
            other => panic!("expected first TCP Connect on stale P2P A, got {other:?}"),
        };

        let (retry_conn_id, retry_multi) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                packed = p2p_b_rx.recv() => {
                    let other = unpack(
                        &packed.expect("p2p B routed message channel closed").to_bytes()
                    ).expect("decode routed message");
                    panic!("TCP retry must not probe another P2P after a P2P timeout, got {other:?}")
                },
                packed = relay_a_rx.recv() => {
                    match unpack(&packed.expect("relay A routed message channel closed").to_bytes())
                        .expect("decode routed message")
                    {
                        BinaryMessage::Connect { conn_id, network, address } => {
                            assert_eq!(network, "tcp");
                            assert_eq!(address, "127.0.0.1:9013");
                            (conn_id, multi_a.clone())
                        }
                        other => panic!("expected retry TCP Connect on relay A, got {other:?}"),
                    }
                }
                packed = relay_b_rx.recv() => {
                    match unpack(&packed.expect("relay B routed message channel closed").to_bytes())
                        .expect("decode routed message")
                    {
                        BinaryMessage::Connect { conn_id, network, address } => {
                            assert_eq!(network, "tcp");
                            assert_eq!(address, "127.0.0.1:9013");
                            (conn_id, multi_b.clone())
                        }
                        other => panic!("expected retry TCP Connect on relay B, got {other:?}"),
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for TCP retry Connect on relay");

        assert_ne!(
            stale_conn_id, retry_conn_id,
            "retry must use a fresh conn_id so a late stale P2P ack cannot satisfy it"
        );
        assert_p2p_session_count_unchanged(&multi_a, p2p_a_count_before, "TCP timeout on P2P A");
        assert_p2p_session_count_unchanged(
            &multi_b,
            p2p_b_count_before,
            "TCP retry candidate P2P B",
        );
        assert_no_p2p_to_relay_migration_metric(&metrics);
        assert!(!engine.proxy_pending_contains_for_test(&stale_conn_id));
        assert!(!multi_a.inbound().contains_key(&stale_conn_id));

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: retry_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _conn = open.await.expect("join").expect("open tcp via relay");
        assert!(retry_multi.inbound().contains_key(&retry_conn_id));
        assert!(!engine.proxy_pending_contains_for_test(&retry_conn_id));
        assert_p2p_session_count_unchanged(&multi_a, p2p_a_count_before, "TCP completed P2P A");
        assert_p2p_session_count_unchanged(&multi_b, p2p_b_count_before, "TCP did not retry P2P B");
        assert_no_p2p_to_relay_migration_metric(&metrics);
    }

    #[tokio::test]
    async fn late_p2p_connect_response_after_timeout_is_ignored() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        multi.set_p2p(Some(p2p));
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9020").await });

        let stale_conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9020");
                conn_id
            }
            other => panic!("expected first TCP Connect on stale P2P, got {other:?}"),
        };

        let retry_conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9020");
                conn_id
            }
            other => panic!("expected retry TCP Connect on relay, got {other:?}"),
        };
        assert_ne!(stale_conn_id, retry_conn_id);

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: stale_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        assert!(
            engine.proxy_pending_contains_for_test(&retry_conn_id),
            "late stale TCP ack must leave retry conn_id pending"
        );
        assert!(
            !engine.proxy_pending_contains_for_test(&stale_conn_id),
            "stale TCP conn_id must stay removed from pending map"
        );
        assert!(
            !multi.inbound().contains_key(&stale_conn_id),
            "late stale TCP ack must not install timed-out inbound slot"
        );

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: retry_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _conn = open.await.expect("join").expect("open tcp via relay");
        assert!(multi.inbound().contains_key(&retry_conn_id));
        assert!(!engine.proxy_pending_contains_for_test(&retry_conn_id));
    }

    #[tokio::test]
    async fn p2p_ack_timeout_does_not_close_or_migrate_p2p_session() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        multi.set_p2p(Some(p2p));
        let p2p_count_before = multi.p2p_session_count();
        let engine = engine_with_multi(multi.clone());
        let metrics = MetricsManager::new();
        engine.set_metrics(Some(metrics.clone()));
        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9021").await });

        let stale_conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected first TCP Connect on stale P2P, got {other:?}"),
        };

        let relay_conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected retry TCP Connect on relay, got {other:?}"),
        };

        assert_ne!(stale_conn_id, relay_conn_id);
        assert_p2p_session_count_unchanged(&multi, p2p_count_before, "TCP ack timeout retry");
        assert_no_p2p_to_relay_migration_metric(&metrics);
        assert!(!engine.proxy_pending_contains_for_test(&stale_conn_id));
        assert!(!multi.inbound().contains_key(&stale_conn_id));

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: relay_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _conn = open.await.expect("join").expect("open tcp via relay");
        assert_p2p_session_count_unchanged(&multi, p2p_count_before, "TCP relay retry completion");
        assert_no_p2p_to_relay_migration_metric(&metrics);
    }

    #[tokio::test]
    async fn relay_tcp_uses_legacy_connect_even_when_tcp_flow_capability_advertised() {
        let mut relay_channels = channel_session_with_capabilities(TransportCapabilities {
            tcp_flow_stream_v1: true,
            ..TransportCapabilities::default()
        });
        let multi = make_multi(relay_channels.session.clone());
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_tcp("127.0.0.1:9030").await });

        let conn_id = match recv_msg(&mut relay_channels.data_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9030");
                conn_id
            }
            other => panic!("expected legacy Connect on relay lane, got {other:?}"),
        };
        assert!(
            multi.inbound().contains_key(&conn_id),
            "legacy relay path must install inbound map before connect ack"
        );

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _conn = open.await.expect("join").expect("open tcp via relay");
        assert!(!engine.proxy_pending_contains_for_test(&conn_id));
    }

    #[tokio::test]
    async fn exact_relay_tcp_flow_waits_for_source_attestation_ack_before_opening_stream() {
        let (certs, key) = tls::self_signed(&["localhost"]).expect("self-signed cert");
        let server = QuicServer::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            tls::server_config(certs, key).expect("server tls"),
            QuicTuning::default(),
        )
        .expect("bind quic server");
        let addr = server.local_addr().expect("server local addr");
        let (session_tx, session_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let incoming = server.accept_incoming().await.expect("incoming quic");
            let (_params, session) = QuicServer::complete_handshake(incoming, &AllowAuth)
                .await
                .expect("server handshake");
            let _ = session_tx.send(session);
            std::future::pending::<()>().await;
        });

        let client = QuicClient::new(
            tls::client_config(None, true).expect("client tls"),
            QuicTuning::default(),
        )
        .expect("client quic");
        let relay = Arc::new(
            client
                .connect(addr, "localhost", test_auth("mesh-Local001-0"))
                .await
                .expect("relay client connect"),
        );
        let server_session = session_rx.await.expect("server session");
        let (_server_sender, mut server_receiver, _server_dg) = server_session.split();
        let mut control_rx = server_receiver
            .take_control_receiver()
            .expect("server control receiver");
        let mut flow_rx = server_receiver
            .take_tcp_flow_receiver()
            .expect("server tcp flow receiver");

        let multi = make_multi(relay);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("mesh-Local001-0", multi);
        configure_mesh_identity(&engine, "198.18.1.10");
        let overlay = engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .expect("install exact Overlay route");
        let target = format!("{overlay}:27015");
        let opener = ProxyTunnelOpener::new(engine.clone());
        let open = tokio::spawn({
            let target = target.clone();
            async move { opener.open_tcp(&target).await }
        });

        let conn_id = match tokio::time::timeout(Duration::from_secs(1), control_rx.recv())
            .await
            .expect("route bind timeout")
            .expect("control channel closed")
        {
            BinaryMessage::RelayRouteBind {
                conn_id,
                peer_client_id,
            } => {
                assert_eq!(peer_client_id, "mesh-RemoteB1-0");
                conn_id
            }
            other => panic!("expected route bind before relay TCP flow, got {other:?}"),
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(50), flow_rx.recv())
                .await
                .is_err(),
            "the TCP flow stream must not open before RelayRouteBindAck"
        );
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBindAck {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let mut incoming = tokio::time::timeout(Duration::from_secs(1), flow_rx.recv())
            .await
            .expect("tcp flow timeout after bind ack")
            .expect("tcp flow receiver closed");
        assert_eq!(incoming.preface.conn_id, conn_id);
        assert_eq!(incoming.preface.address, target);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), server_receiver.recv_data())
                .await
                .is_err(),
            "exact relay TCP flow must not also enqueue a framed Connect"
        );
        incoming
            .stream
            .send_connect_response(true, String::new())
            .await
            .expect("send flow connect response");
        let _conn = open
            .await
            .expect("join")
            .expect("open exact relay tcp flow");
        server_task.abort();
    }

    #[tokio::test]
    async fn open_udp_sends_connect_and_installs_udp_slot_after_success_ack() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:5353").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:5353");
                conn_id
            }
            other => panic!("expected Connect, got {other:?}"),
        };
        assert!(engine.proxy_pending_contains_for_test(&conn_id));
        assert!(
            multi.udp_inbound().contains_key(&conn_id),
            "UDP inbound slot must exist before ConnectResponse"
        );
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _datagram = open.await.expect("join").expect("open udp");
        assert!(multi.udp_inbound().contains_key(&conn_id));
        assert!(!engine.proxy_pending_contains_for_test(&conn_id));
    }

    #[tokio::test]
    async fn open_udp_installs_slot_before_ack_and_close_removes_it() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:5354").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:5354");
                conn_id
            }
            other => panic!("expected UDP Connect, got {other:?}"),
        };
        assert!(multi.udp_inbound().contains_key(&conn_id));

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let mut datagram = open.await.expect("join").expect("open udp");
        assert!(multi.udp_inbound().contains_key(&conn_id));

        datagram.close().await;
        assert!(
            !multi.udp_inbound().contains_key(&conn_id),
            "closing local UDP datagram must remove endpoint slot"
        );
    }

    #[tokio::test]
    async fn inbound_close_removes_udp_slot() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:5355").await });
        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected UDP Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let mut datagram = open.await.expect("join").expect("open udp");
        assert!(multi.udp_inbound().contains_key(&conn_id));

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::Close {
                conn_id: conn_id.clone(),
            })
            .await;

        assert!(
            multi.udp_inbound().contains_key(&conn_id),
            "remote Close must keep endpoint UDP slot briefly for late datagrams"
        );

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::UdpData {
                conn_id: conn_id.clone(),
                payload: Bytes::from_static(b"late"),
            })
            .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), datagram.recv())
                .await
                .expect("late UDP payload should be delivered before close drain expires"),
            Some(Bytes::from_static(b"late"))
        );

        tokio::time::sleep(crate::engine::UDP_CLOSE_DRAIN_GRACE + Duration::from_millis(100)).await;
        assert!(
            !multi.udp_inbound().contains_key(&conn_id),
            "remote Close must remove endpoint UDP slot after the close drain"
        );
    }

    #[tokio::test]
    async fn unknown_udp_data_does_not_create_endpoint_flow() {
        let (relay, _relay_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi);

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::UdpData {
                conn_id: "missing-udp".into(),
                payload: Bytes::from_static(b"late"),
            })
            .await;

        assert!(!engine
            .multi_session()
            .expect("multi")
            .udp_inbound()
            .contains_key("missing-udp"));
    }

    #[tokio::test]
    async fn open_udp_round_robins_across_relay_replicas() {
        let (relay_a, mut relay_a_rx) = channel_session();
        let (relay_b, mut relay_b_rx) = channel_session();
        let multi_a = make_multi(relay_a);
        let multi_b = make_multi(relay_b);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("client-a", multi_a.clone());
        engine.install_proxy_replica_session_for_test("client-b", multi_b.clone());

        let opener = ProxyTunnelOpener::new(engine.clone());
        let first_open = tokio::spawn(async move { opener.open_udp("127.0.0.1:47998").await });
        let first_conn_id = match recv_msg(&mut relay_a_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:47998");
                conn_id
            }
            other => panic!("expected first UDP Connect on replica A, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: first_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let _first = first_open.await.expect("join").expect("first udp open");

        let opener = ProxyTunnelOpener::new(engine.clone());
        let second_open = tokio::spawn(async move { opener.open_udp("127.0.0.1:48000").await });
        let second_conn_id = match recv_msg(&mut relay_b_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:48000");
                conn_id
            }
            other => panic!("expected second UDP Connect on replica B, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: second_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let _second = second_open.await.expect("join").expect("second udp open");

        assert_no_queued_relay_msg(&mut relay_a_rx, "relay A");
        assert_no_queued_relay_msg(&mut relay_b_rx, "relay B");
        assert!(multi_a.udp_inbound().contains_key(&first_conn_id));
        assert!(multi_b.udp_inbound().contains_key(&second_conn_id));
    }

    #[tokio::test]
    async fn open_udp_retries_next_relay_replica_after_selected_relay_closed() {
        let (relay_a, relay_a_rx) = channel_session();
        drop(relay_a_rx);
        let (relay_b, mut relay_b_rx) = channel_session();
        let multi_a = make_multi(relay_a);
        let multi_b = make_multi(relay_b);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_anchor_client_id_for_test("client-b");
        engine.install_proxy_replica_session_for_test("client-a", multi_a);
        engine.install_proxy_replica_session_for_test("client-b", multi_b);
        let opener = ProxyTunnelOpener::new(engine.clone());
        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:47999").await });

        let conn_id = match recv_msg(&mut relay_b_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:47999");
                conn_id
            }
            other => panic!("expected UDP Connect on retry replica, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id,
                success: true,
                error: String::new(),
            })
            .await;
        let _datagram = open.await.expect("join").expect("udp retry open");

        assert_eq!(
            engine
                .pick_proxy_relay_lane()
                .expect("remaining relay lane")
                .local_client_id,
            "client-b"
        );
    }

    #[tokio::test]
    async fn open_udp_accepts_inbound_p2p_replies_for_existing_conn() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        multi.set_p2p(Some(p2p));
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:5354").await });

        let (path, packed) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                packed = p2p_rx.recv() => ("p2p", packed.expect("p2p routed message channel closed")),
                packed = relay_rx.recv() => ("relay", packed.expect("relay routed message channel closed")),
            }
        })
        .await
        .expect("timed out waiting for routed UDP Connect");
        assert_eq!(path, "p2p");

        let conn_id = match unpack(&packed.to_bytes()).expect("decode routed message") {
            BinaryMessage::Connect {
                conn_id, network, ..
            } => {
                assert_eq!(network, "udp");
                conn_id
            }
            other => panic!("expected UDP Connect, got {other:?}"),
        };
        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let mut datagram = open.await.expect("join").expect("open udp via p2p");

        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::UdpData {
                conn_id,
                payload: Bytes::from_static(b"dns!"),
            })
            .await;

        let payload = tokio::time::timeout(Duration::from_secs(1), datagram.recv())
            .await
            .expect("timed out waiting for inbound UDP P2P data")
            .expect("inbound UDP P2P data");
        assert_eq!(payload, Bytes::from_static(b"dns!"));
    }

    #[tokio::test]
    async fn stable_tcp_p2p_send_failure_falls_back_to_its_relay_without_alternate_p2p() {
        let (relay_a, mut relay_a_rx) = channel_session();
        let (relay_b, mut relay_b_rx) = channel_session();
        let (p2p_a, p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        drop(p2p_a_rx);
        let multi_a = make_multi(relay_a);
        let multi_b = make_multi(relay_b);
        install_p2p(
            &multi_a,
            SessionId::from_bytes([0xA1; 16]),
            "pc-1-AbC12345-0",
            p2p_a,
        );
        install_p2p(
            &multi_b,
            SessionId::from_bytes([0xB2; 16]),
            "pc-1-AbC12345-1",
            p2p_b,
        );
        let router =
            crate::p2p::multi_sender::MultiSenderRouter::new_p2p_preferred(multi_a.clone());

        router
            .send(BinaryMessage::Data {
                conn_id: "tcp-stable-1".into(),
                payload: Bytes::from_static(b"x"),
            })
            .await
            .expect("relay fallback after failed P2P");

        match recv_msg(&mut relay_a_rx).await {
            BinaryMessage::Data { conn_id, payload } => {
                assert_eq!(conn_id, "tcp-stable-1");
                assert_eq!(payload, Bytes::from_static(b"x"));
            }
            other => panic!("expected TCP relay fallback on same replica, got {other:?}"),
        }
        assert_no_queued_relay_msg(&mut relay_b_rx, "relay B");
        assert!(
            p2p_b_rx.try_recv().is_err(),
            "stable TCP must not try alternate P2P"
        );
    }

    #[tokio::test]
    async fn stable_udp_p2p_full_stays_on_p2p_and_returns_full() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        let sid = SessionId::from_bytes([0x96; 16]);
        install_p2p(&multi, sid, "pc-AbC12345-0", p2p);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("app-AbC12345-0", multi.clone());
        let opener = ProxyTunnelOpener::new(engine.clone());

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9109").await });
        let conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect { conn_id, .. } => conn_id,
            other => panic!("expected UDP Connect on primary P2P, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        let datagram = open.await.expect("join").expect("open udp via primary p2p");
        assert_no_queued_relay_msg(&mut relay_rx, "legacy UDP relay route bind");

        for _ in 0..16 {
            datagram
                .try_send(Bytes::from_static(b"fill"))
                .expect("p2p queue should accept packet until full");
        }
        match datagram.try_send(Bytes::from_static(b"full")) {
            Err(tp_transport::TrySendKind::Full) => {}
            other => {
                panic!("expected P2P Full to stay on P2P without relay switch, got {other:?}")
            }
        }
        assert_no_queued_relay_msg(&mut relay_rx, "same-replica relay after P2P Full");
    }

    #[tokio::test]
    async fn open_udp_p2p_ack_timeout_falls_back_to_relay_without_trying_other_p2p() {
        let (relay_a, mut relay_a_rx) = channel_session();
        let (relay_b, mut relay_b_rx) = channel_session();
        let (p2p_a, mut p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        let multi_a = make_multi_with_p2p_first_scheduler(relay_a);
        let multi_b = make_multi(relay_b);
        multi_a.set_p2p(Some(p2p_a));
        multi_b.set_p2p(Some(p2p_b));

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("client-a", multi_a.clone());
        engine.install_proxy_replica_session_for_test("client-b", multi_b.clone());
        let metrics = MetricsManager::new();
        engine.set_metrics(Some(metrics.clone()));

        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));
        let p2p_a_count_before = multi_a.p2p_session_count();
        let p2p_b_count_before = multi_b.p2p_session_count();
        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9012").await });

        let stale_conn_id = match recv_msg(&mut p2p_a_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9012");
                conn_id
            }
            other => panic!("expected first UDP Connect on stale P2P A, got {other:?}"),
        };

        let (retry_conn_id, retry_multi) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                packed = relay_a_rx.recv() => match unpack(
                    &packed.expect("relay A routed message channel closed").to_bytes()
                ).expect("decode routed message") {
                    BinaryMessage::Connect { conn_id, network, address } => {
                        assert_eq!(network, "udp");
                        assert_eq!(address, "127.0.0.1:9012");
                        (conn_id, multi_a.clone())
                    }
                    other => panic!("expected retry UDP Connect on relay A, got {other:?}"),
                },
                packed = p2p_b_rx.recv() => {
                    let other = unpack(
                        &packed.expect("p2p B routed message channel closed").to_bytes()
                    ).expect("decode routed message");
                    panic!("UDP retry must not probe another P2P after a P2P timeout, got {other:?}")
                }
                packed = relay_b_rx.recv() => {
                    match unpack(&packed.expect("relay B routed message channel closed").to_bytes())
                        .expect("decode routed message")
                    {
                        BinaryMessage::Connect { conn_id, network, address } => {
                            assert_eq!(network, "udp");
                            assert_eq!(address, "127.0.0.1:9012");
                            (conn_id, multi_b.clone())
                        }
                        other => panic!("expected retry UDP Connect on relay B, got {other:?}"),
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for UDP retry Connect on relay");

        assert_ne!(
            stale_conn_id, retry_conn_id,
            "UDP retry must use a fresh conn_id so a late stale P2P ack cannot satisfy it"
        );
        assert_p2p_session_count_unchanged(&multi_a, p2p_a_count_before, "UDP timeout on P2P A");
        assert_p2p_session_count_unchanged(
            &multi_b,
            p2p_b_count_before,
            "UDP retry candidate P2P B",
        );
        assert_no_p2p_to_relay_migration_metric(&metrics);
        assert!(!engine.proxy_pending_contains_for_test(&stale_conn_id));
        assert!(!multi_a.udp_inbound().contains_key(&stale_conn_id));

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: retry_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _datagram = open.await.expect("join").expect("open udp via relay");
        assert!(retry_multi.udp_inbound().contains_key(&retry_conn_id));
        assert!(!engine.proxy_pending_contains_for_test(&retry_conn_id));
        assert_p2p_session_count_unchanged(&multi_a, p2p_a_count_before, "UDP completed P2P A");
        assert_p2p_session_count_unchanged(&multi_b, p2p_b_count_before, "UDP did not retry P2P B");
        assert_no_p2p_to_relay_migration_metric(&metrics);
    }

    #[tokio::test]
    async fn open_udp_p2p_ack_timeout_uses_relay_only_when_no_live_p2p_candidate() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        multi.set_p2p(Some(p2p));
        let engine = engine_with_multi(multi.clone());
        let metrics = MetricsManager::new();
        engine.set_metrics(Some(metrics.clone()));
        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));
        let p2p_count_before = multi.p2p_session_count();

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9014").await });

        let stale_conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9014");
                conn_id
            }
            other => panic!("expected first UDP Connect on stale P2P, got {other:?}"),
        };

        let relay_conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9014");
                conn_id
            }
            other => panic!("expected retry UDP Connect on relay, got {other:?}"),
        };

        assert_ne!(
            stale_conn_id, relay_conn_id,
            "UDP retry must use a fresh conn_id so a late stale P2P ack cannot satisfy it"
        );
        assert_p2p_session_count_unchanged(&multi, p2p_count_before, "UDP P2P associate timeout");
        assert_no_p2p_to_relay_migration_metric(&metrics);
        assert!(!engine.proxy_pending_contains_for_test(&stale_conn_id));
        assert!(!multi.udp_inbound().contains_key(&stale_conn_id));

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: stale_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;
        assert!(
            engine.proxy_pending_contains_for_test(&relay_conn_id),
            "a late ack for the timed-out P2P UDP conn_id must not complete the relay retry"
        );
        assert!(
            !multi.udp_inbound().contains_key(&stale_conn_id),
            "late stale UDP ack must not install the timed-out association"
        );

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: relay_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _datagram = open.await.expect("join").expect("open udp via relay");
        assert!(multi.udp_inbound().contains_key(&relay_conn_id));
        assert!(!engine.proxy_pending_contains_for_test(&relay_conn_id));
        assert_p2p_session_count_unchanged(&multi, p2p_count_before, "UDP relay retry completion");
        assert_no_p2p_to_relay_migration_metric(&metrics);
    }

    #[tokio::test]
    async fn late_p2p_udp_associate_response_after_timeout_is_ignored() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_p2p_first_scheduler(relay);
        multi.set_p2p(Some(p2p));
        let engine = engine_with_multi(multi.clone());
        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9022").await });

        let stale_conn_id = match recv_msg(&mut p2p_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9022");
                conn_id
            }
            other => panic!("expected first UDP Connect on stale P2P, got {other:?}"),
        };

        let retry_conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9022");
                conn_id
            }
            other => panic!("expected retry UDP Connect on relay, got {other:?}"),
        };
        assert_ne!(stale_conn_id, retry_conn_id);

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: stale_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        assert!(
            engine.proxy_pending_contains_for_test(&retry_conn_id),
            "late stale UDP ack must leave retry conn_id pending"
        );
        assert!(
            !engine.proxy_pending_contains_for_test(&stale_conn_id),
            "stale UDP conn_id must stay removed from pending map"
        );
        assert!(
            !multi.udp_inbound().contains_key(&stale_conn_id),
            "late stale UDP ack must not install timed-out association"
        );

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: retry_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _datagram = open.await.expect("join").expect("open udp via relay");
        assert!(multi.udp_inbound().contains_key(&retry_conn_id));
        assert!(!engine.proxy_pending_contains_for_test(&retry_conn_id));
    }

    #[tokio::test]
    async fn udp_ack_timeout_keeps_all_p2p_sessions_installed() {
        let (relay_a, mut relay_a_rx) = channel_session();
        let (relay_b, mut relay_b_rx) = channel_session();
        let (p2p_a, mut p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        let multi_a = make_multi_with_p2p_first_scheduler(relay_a);
        let multi_b = make_multi_with_p2p_first_scheduler(relay_b);
        multi_a.set_p2p(Some(p2p_a));
        multi_b.set_p2p(Some(p2p_b));

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("client-a", multi_a.clone());
        engine.install_proxy_replica_session_for_test("client-b", multi_b.clone());
        let metrics = MetricsManager::new();
        engine.set_metrics(Some(metrics.clone()));
        let opener = ProxyTunnelOpener::new_with_timeout(engine.clone(), Duration::from_millis(10));
        let p2p_a_count_before = multi_a.p2p_session_count();
        let p2p_b_count_before = multi_b.p2p_session_count();
        assert_eq!(p2p_a_count_before + p2p_b_count_before, 2);

        let open = tokio::spawn(async move { opener.open_udp("127.0.0.1:9023").await });

        let stale_conn_id = match recv_msg(&mut p2p_a_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:9023");
                conn_id
            }
            other => panic!("expected first UDP Connect on stale P2P A, got {other:?}"),
        };

        let (retry_conn_id, retry_multi) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                packed = p2p_b_rx.recv() => {
                    let other = unpack(
                        &packed.expect("p2p B routed message channel closed").to_bytes()
                    ).expect("decode routed message");
                    panic!("UDP retry must not probe another P2P after a P2P timeout, got {other:?}")
                },
                packed = relay_a_rx.recv() => {
                    match unpack(&packed.expect("relay A routed message channel closed").to_bytes())
                        .expect("decode routed message")
                    {
                        BinaryMessage::Connect { conn_id, network, address } => {
                            assert_eq!(network, "udp");
                            assert_eq!(address, "127.0.0.1:9023");
                            (conn_id, multi_a.clone())
                        }
                        other => panic!("expected retry UDP Connect on relay A, got {other:?}"),
                    }
                }
                packed = relay_b_rx.recv() => {
                    match unpack(&packed.expect("relay B routed message channel closed").to_bytes())
                        .expect("decode routed message")
                    {
                        BinaryMessage::Connect { conn_id, network, address } => {
                            assert_eq!(network, "udp");
                            assert_eq!(address, "127.0.0.1:9023");
                            (conn_id, multi_b.clone())
                        }
                        other => panic!("expected retry UDP Connect on relay B, got {other:?}"),
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for UDP retry Connect on relay");

        assert_ne!(stale_conn_id, retry_conn_id);
        assert_p2p_session_count_unchanged(&multi_a, p2p_a_count_before, "UDP timed-out P2P A");
        assert_p2p_session_count_unchanged(&multi_b, p2p_b_count_before, "UDP did not retry P2P B");
        assert_no_p2p_to_relay_migration_metric(&metrics);
        assert!(!engine.proxy_pending_contains_for_test(&stale_conn_id));
        assert!(!multi_a.udp_inbound().contains_key(&stale_conn_id));

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: retry_conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let _datagram = open.await.expect("join").expect("open udp via relay");
        assert!(retry_multi.udp_inbound().contains_key(&retry_conn_id));
        assert_p2p_session_count_unchanged(&multi_a, p2p_a_count_before, "UDP completed P2P A");
        assert_p2p_session_count_unchanged(&multi_b, p2p_b_count_before, "UDP did not retry P2P B");
        assert_no_p2p_to_relay_migration_metric(&metrics);
    }

    #[tokio::test]
    async fn local_socks5_backend_open_tcp_uses_proxy_opener() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi);
        let backend = LocalEngineSocks5Backend::new(engine.clone());

        let open =
            tokio::spawn(async move { backend.open_tcp("ignored-group", "127.0.0.1:9000").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "tcp");
                assert_eq!(address, "127.0.0.1:9000");
                conn_id
            }
            other => panic!("expected Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id,
                success: true,
                error: String::new(),
            })
            .await;

        let _tcp = open.await.expect("join").expect("open tcp");
    }

    #[tokio::test]
    async fn local_socks5_backend_open_udp_uses_proxy_opener() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let engine = engine_with_multi(multi);
        let backend = LocalEngineSocks5Backend::new(engine.clone());

        let open =
            tokio::spawn(async move { backend.open_udp("ignored-group", "127.0.0.1:5353").await });

        let conn_id = match recv_msg(&mut relay_rx).await {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(network, "udp");
                assert_eq!(address, "127.0.0.1:5353");
                conn_id
            }
            other => panic!("expected Connect, got {other:?}"),
        };
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::ConnectResponse {
                conn_id: conn_id.clone(),
                success: true,
                error: String::new(),
            })
            .await;

        let tunnel = open.await.expect("join").expect("open udp");
        let (sender, receiver) = tunnel.split();
        assert_eq!(receiver.conn_id(), conn_id);
        sender.try_send(Bytes::from_static(b"ping")).expect("send");

        match recv_msg(&mut relay_rx).await {
            BinaryMessage::UdpData {
                conn_id: sent_id,
                payload,
            } => {
                assert_eq!(sent_id, conn_id);
                assert_eq!(payload, Bytes::from_static(b"ping"));
            }
            other => panic!("expected UdpData, got {other:?}"),
        }
    }
}
