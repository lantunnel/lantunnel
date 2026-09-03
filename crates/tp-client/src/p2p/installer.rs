use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use tp_core::p2p_types::SessionId;
use tp_core::protocol::BinaryMessage;
use tp_transport::session::Session;

use crate::p2p::session::MultiSession;
use crate::peer_link_manager::PeerRelationKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum P2pInstallExpiration {
    Expired,
    Installed,
    Missing,
}

#[derive(Clone)]
pub struct P2pInstalledSession {
    multi: Arc<MultiSession>,
    session: Arc<Session>,
}

impl P2pInstalledSession {
    pub(crate) fn new(multi: Arc<MultiSession>, session: Arc<Session>) -> Self {
        Self { multi, session }
    }

    pub(crate) fn close_and_clear_if_current(&self) {
        close_p2p_if_current(&self.multi, &self.session);
    }
}

pub(crate) fn close_p2p_if_current(multi: &Arc<MultiSession>, session: &Arc<Session>) {
    session.close();
    multi.close_p2p_session_for_handle(session);
}

pub(crate) fn close_current_p2p(multi: &Arc<MultiSession>) {
    multi.close_all_p2p();
}

#[derive(Clone)]
pub struct P2pSessionInstaller {
    engine: Arc<crate::Engine>,
    cancel: CancellationToken,
}

impl P2pSessionInstaller {
    pub(crate) fn new(engine: Arc<crate::Engine>, cancel: CancellationToken) -> Self {
        Self { engine, cancel }
    }

    /// Compatibility path for bootstrap/legacy sessions that may arrive
    /// without a manager-owned reservation.
    pub async fn install(
        &self,
        session_id: SessionId,
        session: Session,
    ) -> anyhow::Result<P2pInstalledSession> {
        if self.cancel.is_cancelled() {
            session.close();
            self.engine.unreserve_p2p_session_install(session_id);
            anyhow::bail!("P2P installer cancelled");
        }
        self.engine
            .install_p2p_session(session_id, session, self.cancel.clone())
            .await
    }

    /// Install one manager-negotiated generation, failing closed if its
    /// reservation was expired or consumed by another generation.
    pub(crate) async fn install_reserved(
        &self,
        session_id: SessionId,
        session: Session,
    ) -> anyhow::Result<P2pInstalledSession> {
        if self.cancel.is_cancelled() {
            session.close();
            self.engine.unreserve_p2p_session_install(session_id);
            anyhow::bail!("P2P installer cancelled");
        }
        self.engine
            .install_reserved_p2p_session(session_id, session, self.cancel.clone())
            .await
    }

    #[doc(hidden)]
    pub fn reserve_for_session(
        &self,
        session_id: SessionId,
        preferred_client_id: Option<&str>,
        peer_client_id: Option<&str>,
    ) -> bool {
        self.reserve_for_relation(session_id, preferred_client_id, peer_client_id, None)
    }

    pub(crate) fn reserve_for_relation(
        &self,
        session_id: SessionId,
        preferred_client_id: Option<&str>,
        peer_client_id: Option<&str>,
        relation_key: Option<PeerRelationKey>,
    ) -> bool {
        self.engine.reserve_p2p_session_install_for_relation(
            session_id,
            preferred_client_id,
            peer_client_id,
            relation_key,
        )
    }

    pub(crate) fn unreserve_for_session(&self, session_id: SessionId) {
        self.engine.unreserve_p2p_session_install(session_id);
    }

    /// Linearize a signaling timeout against installation under the Engine's
    /// P2P registry lock.
    pub(crate) fn expire_for_session(&self, session_id: SessionId) -> P2pInstallExpiration {
        self.engine.expire_p2p_session_install(session_id)
    }

    pub(crate) fn update_peer_client_id(&self, session_id: SessionId, peer_client_id: &str) {
        self.engine
            .update_p2p_pending_peer_client_id(session_id, peer_client_id);
    }

    pub(crate) fn close_installed_session(&self, session_id: SessionId) -> bool {
        self.engine.close_p2p_session_by_id(session_id)
    }

    pub(crate) fn has_installed_session(&self, session_id: SessionId) -> bool {
        self.engine.has_p2p_session_by_id(session_id)
    }

    pub(crate) fn has_live_or_pending_relation(&self, relation_key: &PeerRelationKey) -> bool {
        self.engine.has_live_or_pending_p2p_relation(relation_key)
    }

    pub(crate) fn has_reserved_session(&self, session_id: SessionId) -> bool {
        self.engine.has_pending_p2p_session_install(session_id)
    }

    pub(crate) fn active_session_count(&self) -> usize {
        self.engine.p2p_eligible_session_count()
    }

    pub(crate) fn pending_session_count(&self) -> usize {
        self.engine.p2p_pending_session_count()
    }

    pub(crate) fn desired_session_count(&self) -> usize {
        self.engine.p2p_desired_session_count()
    }

    pub(crate) fn available_install_client_ids(&self) -> Vec<String> {
        self.engine.p2p_available_install_client_ids()
    }
}

#[doc(hidden)]
pub struct P2pDataPump {
    multi: Arc<crate::p2p::session::MultiSession>,
    observed: Arc<Mutex<HashSet<String>>>,
    notify: Arc<Notify>,
}

impl P2pDataPump {
    #[doc(hidden)]
    pub fn for_test(multi: Arc<crate::p2p::session::MultiSession>) -> Self {
        Self {
            multi,
            observed: Arc::new(Mutex::new(HashSet::new())),
            notify: Arc::new(Notify::new()),
        }
    }

    #[doc(hidden)]
    pub async fn install_for_test(&mut self, session: Session) -> anyhow::Result<()> {
        let (sender, mut receiver, datagram_receiver) = session.split();
        let send_shell = Arc::new(Session::send_only_from_sender(sender));
        self.multi.set_p2p(Some(send_shell));
        self.multi.set_state(crate::p2p::session::P2pState::Active {
            session_id: SessionId::from_bytes([0u8; 16]),
            since: Instant::now(),
        });

        let observed = self.observed.clone();
        let notify = self.notify.clone();
        tokio::spawn(async move {
            while let Some(msg) = receiver.recv().await {
                record_observed_conn_id(&observed, &notify, &msg).await;
            }
        });

        if let Some(mut dg_rx) = datagram_receiver {
            let observed = self.observed.clone();
            let notify = self.notify.clone();
            tokio::spawn(async move {
                while let Some(msg) = dg_rx.recv().await {
                    record_observed_conn_id(&observed, &notify, &msg).await;
                }
            });
        }

        Ok(())
    }

    #[doc(hidden)]
    pub async fn wait_observed_for_test(
        &self,
        conn_id: &str,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.observed.lock().await.contains(conn_id) {
                return Ok(());
            }

            let now = Instant::now();
            if now >= deadline {
                anyhow::bail!("timed out waiting for P2P frame for conn_id={conn_id}");
            }
            let remaining = deadline.saturating_duration_since(now);
            tokio::time::timeout(remaining, notified)
                .await
                .with_context(|| {
                    format!("timed out waiting for P2P frame for conn_id={conn_id}")
                })?;
        }
    }
}

async fn record_observed_conn_id(
    observed: &Mutex<HashSet<String>>,
    notify: &Notify,
    msg: &BinaryMessage,
) {
    let conn_id = match msg {
        BinaryMessage::Connect { conn_id, .. }
        | BinaryMessage::ConnectResponse { conn_id, .. }
        | BinaryMessage::Data { conn_id, .. }
        | BinaryMessage::UdpData { conn_id, .. }
        | BinaryMessage::Close { conn_id } => conn_id,
        _ => return,
    };
    observed.lock().await.insert(conn_id.clone());
    notify.notify_waiters();
}
