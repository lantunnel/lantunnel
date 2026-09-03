use std::sync::Arc;

use anyhow::{bail, Context as _};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures_util::{future::BoxFuture, SinkExt as _, StreamExt as _};
use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tp_core::provisioning::{GatewayScopeFileV2, PROVISIONING_VERSION_V2};
use tp_gateway::scope::ScopeStore;
use tp_gateway::{Gateway, RelayUsageWal};
use uuid::Uuid;
use x509_parser::parse_x509_certificate;

const CONTROL_VERSION: u8 = 2;
const MAX_CONTROL_FRAME_BYTES: usize = 1024 * 1024;
const MAX_AUTHENTICATE_FRAME_BYTES: usize = 24 * 1024;
const MAX_CERTIFICATE_PEM_BYTES: usize = 16 * 1024;
const MAX_GATEWAY_STATE_FRAME_BYTES: usize = 1024;
const MAX_USAGE_FRAME_BYTES: usize = 1024 * 1024;
const MAX_USAGE_ITEMS: usize = 256;
const SCOPE_SNAPSHOT_DIGEST_DOMAIN: &str = "lantunnel.gateway.scope-snapshot.v2";
const REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) type GatewayReadinessProbe =
    Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GatewayControlKind {
    Byog,
    Fleet,
}

impl GatewayControlKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Byog => "byog",
            Self::Fleet => "fleet",
        }
    }

    pub(crate) fn validate_gateway_id(self, gateway_id: &str) -> anyhow::Result<()> {
        match self {
            Self::Byog => validate_canonical_uuid("BYOG Gateway ID", gateway_id),
            Self::Fleet => {
                if gateway_id.len() != 21
                    || !gateway_id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                {
                    bail!("Fleet Gateway ID must be a 21-character Platform nanoid");
                }
                Ok(())
            }
        }
    }
}

pub(crate) struct GatewayControlIdentity {
    kind: GatewayControlKind,
    gateway_id: String,
    boot_id: String,
    leaf_sha256: String,
    signing_key: EcdsaKeyPair,
    certificate_pem: Option<String>,
    claim_secret: Option<String>,
}

impl GatewayControlIdentity {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: GatewayControlKind,
        gateway_id: &str,
        boot_id: &str,
        leaf_sha256: &str,
        private_key_pem: &[u8],
        certificate_pem: Option<String>,
        claim_secret: Option<String>,
    ) -> anyhow::Result<Self> {
        kind.validate_gateway_id(gateway_id)?;
        validate_canonical_uuid("Gateway boot ID", boot_id)?;
        validate_sha256("Gateway leaf", leaf_sha256)?;
        if claim_secret.is_some() != certificate_pem.is_some() {
            bail!("Gateway claim and certificate PEM must either both be present or both absent");
        }
        if let Some(claim) = claim_secret.as_deref() {
            validate_claim_secret(claim)?;
        }
        if certificate_pem
            .as_ref()
            .is_some_and(|pem| pem.len() > MAX_CERTIFICATE_PEM_BYTES)
        {
            bail!("Gateway certificate PEM exceeds {MAX_CERTIFICATE_PEM_BYTES} bytes");
        }

        let private_key_pem = std::str::from_utf8(private_key_pem)
            .context("Gateway control private key is not UTF-8 PEM")?;
        let key =
            KeyPair::from_pem(private_key_pem).context("parse Gateway control private key")?;
        if !key.is_compatible(&PKCS_ECDSA_P256_SHA256) {
            bail!("Gateway control private key must be ECDSA P-256 PKCS#8");
        }
        if let Some(certificate_pem) = certificate_pem.as_deref() {
            validate_certificate_identity(certificate_pem, leaf_sha256, &key)?;
        }
        let signing_key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &key.serialize_der(),
            &SystemRandom::new(),
        )
        .map_err(|error| anyhow::anyhow!("load Gateway control P-256 key: {error}"))?;

        Ok(Self {
            kind,
            gateway_id: gateway_id.to_owned(),
            boot_id: boot_id.to_owned(),
            leaf_sha256: leaf_sha256.to_owned(),
            signing_key,
            certificate_pem,
            claim_secret,
        })
    }

    fn gateway_ref(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.gateway_id)
    }

    fn authenticate(&self, nonce: &str) -> anyhow::Result<GatewayFrame> {
        validate_nonce(nonce)?;
        let preimage = encode_authenticate_preimage(
            self.kind,
            &self.gateway_id,
            &self.boot_id,
            nonce,
            &self.leaf_sha256,
        );
        let signature = self
            .signing_key
            .sign(&SystemRandom::new(), &preimage)
            .map_err(|_| anyhow::anyhow!("sign Gateway control challenge"))?;
        if signature.as_ref().len() != 64 {
            bail!("Gateway control P-256 signature is not fixed-width IEEE-P1363");
        }
        Ok(GatewayFrame::Authenticate {
            version: CONTROL_VERSION,
            boot_id: self.boot_id.clone(),
            signature: URL_SAFE_NO_PAD.encode(signature.as_ref()),
            claim_secret: self.claim_secret.clone(),
            certificate_pem: self.certificate_pem.clone(),
        })
    }
}

fn validate_certificate_identity(
    certificate_pem: &str,
    expected_leaf_sha256: &str,
    key: &KeyPair,
) -> anyhow::Result<()> {
    let certificates = tp_transport::tls::parse_certs(certificate_pem.as_bytes())
        .context("parse Gateway control certificate")?;
    if certificates.len() != 1 {
        bail!("Gateway control identity must contain exactly one leaf certificate");
    }
    let leaf = certificates.first().expect("certificate count checked");
    if hex::encode(Sha256::digest(leaf.as_ref())) != expected_leaf_sha256 {
        bail!("Gateway control certificate does not match its leaf SHA-256");
    }
    let (remainder, parsed) = parse_x509_certificate(leaf.as_ref())
        .map_err(|_| anyhow::anyhow!("parse Gateway control leaf certificate"))?;
    if !remainder.is_empty() || parsed.public_key().raw != key.public_key_der() {
        bail!("Gateway control certificate does not match its P-256 private key");
    }
    Ok(())
}

fn encode_authenticate_preimage(
    kind: GatewayControlKind,
    gateway_id: &str,
    boot_id: &str,
    nonce: &str,
    leaf_sha256: &str,
) -> Vec<u8> {
    format!(
        "lantunnel-gateway-control-auth-v2\nkind={}\nid={}\nboot_id={}\nnonce={}\nleaf_sha256={}\n",
        kind.as_str(),
        gateway_id,
        boot_id,
        nonce,
        leaf_sha256
    )
    .into_bytes()
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum PlatformFrame {
    #[serde(rename = "challenge")]
    Challenge { version: u8, nonce: String },
    #[serde(rename = "scope_snapshot")]
    ScopeSnapshot {
        version: u8,
        digest: String,
        scopes: Vec<ScopeSnapshotItem>,
    },
    #[serde(rename = "usage_ack")]
    UsageAck { version: u8, through_seq: u64 },
}

/// A Platform-authoritative Relay budget for one Tunnel in one period.
///
/// Absent means "no ceiling": a Gateway that has not been told a budget keeps
/// relaying, which is what every Gateway did before budgets existed.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RelayQuotaSnapshot {
    period_yyyymm: String,
    quota_bytes: u64,
    remaining_bytes: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScopeSnapshotItem {
    tunnel_id: String,
    tunnel_signing_public_key: String,
    /// Outside the Scope digest on purpose: the digest covers admission, and a
    /// budget must not make two Gateways disagree about whether they converged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_quota: Option<RelayQuotaSnapshot>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum GatewayFrame {
    Authenticate {
        version: u8,
        boot_id: String,
        signature: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        claim_secret: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        certificate_pem: Option<String>,
    },
    GatewayState {
        version: u8,
        boot_id: String,
        ready: bool,
        applied_scope_digest: String,
    },
    UsageSnapshot {
        version: u8,
        through_seq: u64,
        items: Vec<UsageSnapshotItem>,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct UsageSnapshotItem {
    seq: u64,
    report_id: String,
    tunnel_id: String,
    period_yyyymm: String,
    relay_billable: u64,
}

impl GatewayFrame {
    pub(crate) fn to_text(&self) -> anyhow::Result<String> {
        let text = serde_json::to_string(self).context("serialize Gateway control frame")?;
        let limit = match self {
            Self::Authenticate { .. } => MAX_AUTHENTICATE_FRAME_BYTES,
            Self::GatewayState { .. } => MAX_GATEWAY_STATE_FRAME_BYTES,
            Self::UsageSnapshot { .. } => MAX_USAGE_FRAME_BYTES,
        };
        if text.len() > limit {
            bail!("outbound Gateway control frame exceeds {limit} bytes");
        }
        Ok(text)
    }
}

pub(crate) struct ControlAction {
    pub(crate) outbound: Option<GatewayFrame>,
    applied_snapshot: Option<AppliedSnapshot>,
    pub(crate) usage_ack: Option<u64>,
}

struct AppliedSnapshot {
    state: GatewayFrame,
    removed_tunnel_ids: Vec<String>,
    relay_quotas: Vec<(String, RelayQuotaSnapshot)>,
}

struct PreparedSnapshot {
    state: GatewayFrame,
    usage: Option<GatewayFrame>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SessionPhase {
    AwaitingChallenge,
    AwaitingSnapshot,
    Active,
}

pub(crate) struct GatewayControlSession {
    identity: GatewayControlIdentity,
    scopes: Arc<ScopeStore>,
    phase: SessionPhase,
    applied_scope_digest: Option<String>,
}

impl GatewayControlSession {
    pub(crate) fn new(identity: GatewayControlIdentity, scopes: Arc<ScopeStore>) -> Self {
        Self {
            identity,
            scopes,
            phase: SessionPhase::AwaitingChallenge,
            applied_scope_digest: None,
        }
    }

    pub(crate) fn handle_text(&mut self, text: &str) -> anyhow::Result<ControlAction> {
        if text.len() > MAX_CONTROL_FRAME_BYTES {
            bail!("Gateway control frame exceeds {MAX_CONTROL_FRAME_BYTES} bytes");
        }
        let frame: PlatformFrame =
            serde_json::from_str(text).context("parse strict Gateway control frame")?;
        match frame {
            PlatformFrame::Challenge { version, nonce } => {
                validate_version(version)?;
                if self.phase != SessionPhase::AwaitingChallenge {
                    bail!("Gateway control challenge is out of order");
                }
                let outbound = self.identity.authenticate(&nonce)?;
                self.phase = SessionPhase::AwaitingSnapshot;
                Ok(ControlAction {
                    outbound: Some(outbound),
                    applied_snapshot: None,
                    usage_ack: None,
                })
            }
            PlatformFrame::ScopeSnapshot {
                version,
                digest,
                scopes,
            } => {
                validate_version(version)?;
                if self.phase == SessionPhase::AwaitingChallenge {
                    bail!("Gateway Scope snapshot arrived before authentication");
                }
                validate_sha256("Gateway Scope snapshot", &digest)?;
                let (candidate, relay_quotas) =
                    validate_scope_snapshot(&self.identity.gateway_ref(), &digest, scopes)?;
                let replaced = self.scopes.replace_managed_snapshot(candidate)?;
                self.applied_scope_digest = Some(digest.clone());
                self.phase = SessionPhase::Active;
                Ok(ControlAction {
                    outbound: None,
                    applied_snapshot: Some(AppliedSnapshot {
                        state: self.gateway_state(&digest),
                        removed_tunnel_ids: replaced.removed_ids,
                        relay_quotas,
                    }),
                    usage_ack: None,
                })
            }
            PlatformFrame::UsageAck {
                version,
                through_seq,
            } => {
                validate_version(version)?;
                if self.phase == SessionPhase::AwaitingChallenge {
                    bail!("Gateway usage ACK arrived before authentication");
                }
                Ok(ControlAction {
                    outbound: None,
                    applied_snapshot: None,
                    usage_ack: Some(through_seq),
                })
            }
        }
    }

    pub(crate) fn periodic_gateway_state(&self) -> Option<GatewayFrame> {
        self.applied_scope_digest
            .as_deref()
            .map(|digest| self.gateway_state(digest))
    }

    fn gateway_state(&self, digest: &str) -> GatewayFrame {
        GatewayFrame::GatewayState {
            version: CONTROL_VERSION,
            boot_id: self.identity.boot_id.clone(),
            ready: false,
            applied_scope_digest: digest.to_owned(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct GatewayControlConnectConfig {
    pub(crate) kind: GatewayControlKind,
    pub(crate) gateway_id: String,
    pub(crate) platform_url: String,
    pub(crate) boot_id: String,
    pub(crate) leaf_sha256: String,
    pub(crate) private_key_pem: Vec<u8>,
    pub(crate) certificate_pem: Option<String>,
    pub(crate) claim_secret: Option<String>,
}

impl GatewayControlConnectConfig {
    fn identity(&self) -> anyhow::Result<GatewayControlIdentity> {
        GatewayControlIdentity::new(
            self.kind,
            &self.gateway_id,
            &self.boot_id,
            &self.leaf_sha256,
            &self.private_key_pem,
            self.certificate_pem.clone(),
            self.claim_secret.clone(),
        )
    }

    fn endpoint(&self) -> anyhow::Result<String> {
        self.kind.validate_gateway_id(&self.gateway_id)?;
        let authority = self
            .platform_url
            .strip_prefix("https://")
            .ok_or_else(|| anyhow::anyhow!("Gateway control Platform URL must use https"))?;
        if authority.is_empty()
            || authority.contains('/')
            || authority.contains('?')
            || authority.contains('#')
            || authority.contains('@')
        {
            bail!("Gateway control Platform URL must contain only an HTTPS origin");
        }
        Ok(format!(
            "wss://{authority}/api/gateway-control/v2/{}/{}",
            self.kind.as_str(),
            self.gateway_id
        ))
    }
}

/// Perform the one WSS authentication transaction used by onboarding. Receipt
/// and application of the first authoritative snapshot is the success signal;
/// callers may then remove the one-time BYOG pairing input.
pub(crate) async fn register_once(config: GatewayControlConnectConfig) -> anyhow::Result<()> {
    tokio::time::timeout(REGISTRATION_TIMEOUT, register_once_inner(config))
        .await
        .context("Gateway control registration timed out")?
}

async fn register_once_inner(config: GatewayControlConnectConfig) -> anyhow::Result<()> {
    let mut socket = connect_control_socket(&config).await?;
    let mut session = GatewayControlSession::new(config.identity()?, Arc::new(ScopeStore::new()));

    while let Some(message) = socket.next().await {
        match message.context("read Gateway control WSS frame")? {
            Message::Text(text) => {
                let action = session.handle_text(&text)?;
                if let Some(outbound) = action.outbound {
                    socket
                        .send(Message::Text(outbound.to_text()?))
                        .await
                        .context("send Gateway control WSS frame")?;
                }
                if let Some(applied) = action.applied_snapshot {
                    if !applied.removed_tunnel_ids.is_empty() {
                        bail!("onboarding Scope snapshot unexpectedly removed a Tunnel");
                    }
                    socket
                        .send(Message::Text(applied.state.to_text()?))
                        .await
                        .context("send onboarding Gateway state")?;
                    socket.close(None).await.context("close onboarding WSS")?;
                    return Ok(());
                }
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .context("send Gateway control WSS pong")?,
            Message::Pong(_) => {}
            Message::Close(frame) => {
                bail!("Gateway control WSS closed during registration: {frame:?}")
            }
            Message::Binary(_) | Message::Frame(_) => {
                bail!("Gateway control accepts text protocol frames only")
            }
        }
    }
    bail!("Gateway control WSS ended before registration completed")
}

type ControlSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect_control_socket(
    config: &GatewayControlConnectConfig,
) -> anyhow::Result<ControlSocket> {
    let endpoint = config.endpoint()?;
    let websocket_config = WebSocketConfig {
        max_message_size: Some(MAX_CONTROL_FRAME_BYTES),
        max_frame_size: Some(MAX_CONTROL_FRAME_BYTES),
        ..Default::default()
    };
    let (socket, _) =
        tokio_tungstenite::connect_async_with_config(endpoint, Some(websocket_config), false)
            .await
            .context("connect Gateway control WSS with public Platform TLS validation")?;
    Ok(socket)
}

pub(crate) async fn run_forever(
    config: GatewayControlConnectConfig,
    readiness_probe: GatewayReadinessProbe,
    gateway: Arc<Gateway>,
    relay_usage_wal: Arc<RelayUsageWal>,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        if let Err(error) = run_runtime_connection(
            &config,
            &readiness_probe,
            gateway.clone(),
            relay_usage_wal.clone(),
        )
        .await
        {
            tracing::warn!(
                error = %error,
                retry_secs = backoff.as_secs(),
                "Gateway outbound control WSS disconnected"
            );
        }
        tokio::time::sleep(backoff).await;
        backoff = backoff
            .saturating_mul(2)
            .min(std::time::Duration::from_secs(30));
    }
}

async fn run_runtime_connection(
    config: &GatewayControlConnectConfig,
    readiness_probe: &GatewayReadinessProbe,
    gateway: Arc<Gateway>,
    relay_usage_wal: Arc<RelayUsageWal>,
) -> anyhow::Result<()> {
    let mut socket = connect_control_socket(config).await?;
    let mut session = GatewayControlSession::new(config.identity()?, gateway.scopes().clone());
    let mut state_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
    );
    state_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            message = socket.next() => {
                let message = message
                    .ok_or_else(|| anyhow::anyhow!("Gateway control WSS ended"))?
                    .context("read Gateway control WSS frame")?;
                match message {
                    Message::Text(text) => {
                        let action = session.handle_text(&text)?;
                        if let Some(outbound) = action.outbound {
                            send_frame(&mut socket, outbound).await?;
                        }
                        if let Some(through_seq) = action.usage_ack {
                            apply_usage_ack(&gateway, &relay_usage_wal, through_seq)?;
                        }
                        if let Some(applied) = action.applied_snapshot {
                            let prepared = prepare_applied_snapshot(
                                applied,
                                |tunnel_id| {
                                    let disconnected = gateway.disconnect_tunnel_clients(tunnel_id);
                                    if disconnected > 0 {
                                        tracing::info!(
                                            tunnel_id,
                                            disconnected,
                                            "disconnected attachments removed by Managed Scope snapshot"
                                        );
                                    }
                                },
                                |tunnel_id, period, quota_bytes, remaining_bytes| {
                                    gateway.apply_relay_quota(
                                        tunnel_id,
                                        period,
                                        quota_bytes,
                                        remaining_bytes,
                                    );
                                },
                                || usage_snapshot(&gateway, &relay_usage_wal),
                            )?;
                            let state = refresh_gateway_state_readiness(
                                prepared.state,
                                readiness_probe,
                            )
                            .await;
                            send_frame(&mut socket, state).await?;
                            if let Some(usage) = prepared.usage {
                                send_frame(&mut socket, usage).await?;
                            }
                        }
                    }
                    Message::Ping(payload) => socket
                        .send(Message::Pong(payload))
                        .await
                        .context("send Gateway control WSS pong")?,
                    Message::Pong(_) => {}
                    Message::Close(frame) => {
                        bail!("Gateway control WSS closed: {frame:?}")
                    }
                    Message::Binary(_) | Message::Frame(_) => {
                        bail!("Gateway control accepts text protocol frames only")
                    }
                }
            }
            _ = state_interval.tick() => {
                if let Some(state) = session.periodic_gateway_state() {
                    let state = refresh_gateway_state_readiness(state, readiness_probe).await;
                    send_frame(&mut socket, state).await?;
                    if let Some(usage) = usage_snapshot(&gateway, &relay_usage_wal)? {
                        send_frame(&mut socket, usage).await?;
                    }
                }
            }
        }
    }
}

async fn refresh_gateway_state_readiness(
    mut state: GatewayFrame,
    readiness_probe: &GatewayReadinessProbe,
) -> GatewayFrame {
    if let GatewayFrame::GatewayState { ready, .. } = &mut state {
        *ready = readiness_probe().await;
    }
    state
}

/// Complete the local side effects covered by an applied digest before making
/// that digest externally visible in `gateway_state`.
fn prepare_applied_snapshot<D, Q, F>(
    applied: AppliedSnapshot,
    mut disconnect: D,
    mut apply_relay_quota: Q,
    flush_and_snapshot_usage: F,
) -> anyhow::Result<PreparedSnapshot>
where
    D: FnMut(&str),
    Q: FnMut(&str, &str, u64, u64),
    F: FnOnce() -> anyhow::Result<Option<GatewayFrame>>,
{
    for tunnel_id in &applied.removed_tunnel_ids {
        disconnect(tunnel_id);
    }
    // Budgets are applied before the digest becomes externally visible, so a
    // Gateway never advertises convergence on a Scope whose ceilings it has not
    // installed yet.
    for (tunnel_id, quota) in &applied.relay_quotas {
        apply_relay_quota(
            tunnel_id,
            &quota.period_yyyymm,
            quota.quota_bytes,
            quota.remaining_bytes,
        );
    }
    let usage = flush_and_snapshot_usage()?;
    Ok(PreparedSnapshot {
        state: applied.state,
        usage,
    })
}

async fn send_frame(socket: &mut ControlSocket, frame: GatewayFrame) -> anyhow::Result<()> {
    socket
        .send(Message::Text(frame.to_text()?))
        .await
        .context("send Gateway control WSS frame")
}

fn usage_snapshot(
    gateway: &Gateway,
    relay_usage_wal: &RelayUsageWal,
) -> anyhow::Result<Option<GatewayFrame>> {
    gateway
        .flush_pending_relay_usage_to_wal()
        .context("flush relay usage to existing WAL")?;
    let batch = relay_usage_wal
        .snapshot(MAX_USAGE_ITEMS)
        .context("snapshot existing relay usage WAL")?;
    if batch.items.is_empty() {
        return Ok(None);
    }
    let items = batch
        .items
        .into_iter()
        .map(|item| UsageSnapshotItem {
            seq: item.seq,
            report_id: format!("{}:{}:{}", item.seq, item.period_yyyymm, item.tunnel_id),
            tunnel_id: item.tunnel_id,
            period_yyyymm: item.period_yyyymm,
            relay_billable: item.bytes,
        })
        .collect();
    Ok(Some(GatewayFrame::UsageSnapshot {
        version: CONTROL_VERSION,
        through_seq: batch.through_seq,
        items,
    }))
}

fn apply_usage_ack(
    gateway: &Gateway,
    relay_usage_wal: &RelayUsageWal,
    through_seq: u64,
) -> anyhow::Result<()> {
    let batch = relay_usage_wal
        .snapshot(MAX_USAGE_ITEMS)
        .context("snapshot relay usage WAL before ACK")?;
    if through_seq > batch.through_seq {
        bail!(
            "relay usage ACK {through_seq} exceeds available prefix {}",
            batch.through_seq
        );
    }
    let acked = batch
        .items
        .into_iter()
        .filter(|item| item.seq <= through_seq)
        .collect::<Vec<_>>();
    gateway.mark_relay_usage_reported(&acked);
    relay_usage_wal
        .ack(through_seq)
        .with_context(|| format!("ack relay usage WAL through seq {through_seq}"))
}

type ValidatedScopeSnapshot = (Vec<GatewayScopeFileV2>, Vec<(String, RelayQuotaSnapshot)>);

fn validate_scope_snapshot(
    gateway_ref: &str,
    expected_digest: &str,
    scopes: Vec<ScopeSnapshotItem>,
) -> anyhow::Result<ValidatedScopeSnapshot> {
    if scopes
        .windows(2)
        .any(|pair| pair[0].tunnel_id >= pair[1].tunnel_id)
    {
        bail!("Gateway Scope snapshot must be strictly sorted by Tunnel ID");
    }
    let mut candidate = Vec::with_capacity(scopes.len());
    let mut digest_entries = Vec::with_capacity(scopes.len());
    let mut quotas = Vec::new();
    for scope in scopes {
        validate_canonical_uuid("Tunnel ID", &scope.tunnel_id)?;
        if let Some(quota) = scope.relay_quota.clone() {
            quotas.push((scope.tunnel_id.clone(), quota));
        }
        let scope = GatewayScopeFileV2 {
            version: PROVISIONING_VERSION_V2,
            tunnel_id: scope.tunnel_id,
            tunnel_signing_public_key: scope.tunnel_signing_public_key,
        };
        scope.verify()?;
        let mut canonical_scope =
            serde_json::to_vec(&scope).context("serialize canonical Gateway Scope")?;
        canonical_scope.push(b'\n');
        digest_entries.push(ScopeDigestEntry {
            tunnel_id: scope.tunnel_id.clone(),
            scope_sha256: hex::encode(Sha256::digest(&canonical_scope)),
        });
        candidate.push(scope);
    }
    let digest_entries =
        serde_json::to_string(&digest_entries).context("serialize Scope digest entries")?;
    let computed = hex::encode(Sha256::digest(
        format!("{SCOPE_SNAPSHOT_DIGEST_DOMAIN}\n{gateway_ref}\n{digest_entries}").as_bytes(),
    ));
    if computed != expected_digest {
        bail!("Gateway Scope snapshot digest does not match its canonical contents");
    }
    Ok((candidate, quotas))
}

#[derive(Serialize)]
struct ScopeDigestEntry {
    tunnel_id: String,
    scope_sha256: String,
}

fn validate_version(version: u8) -> anyhow::Result<()> {
    if version != CONTROL_VERSION {
        bail!("unsupported Gateway control protocol version {version}");
    }
    Ok(())
}

fn validate_nonce(nonce: &str) -> anyhow::Result<()> {
    let decoded = URL_SAFE_NO_PAD
        .decode(nonce)
        .context("Gateway control nonce is not base64url without padding")?;
    if decoded.len() != 32 || URL_SAFE_NO_PAD.encode(&decoded) != nonce {
        bail!("Gateway control nonce must be canonical base64url for exactly 32 bytes");
    }
    Ok(())
}

pub(crate) fn validate_claim_secret(claim: &str) -> anyhow::Result<()> {
    let decoded = URL_SAFE_NO_PAD
        .decode(claim)
        .context("Gateway claim is not base64url without padding")?;
    if claim.len() != 43 || decoded.len() != 32 || URL_SAFE_NO_PAD.encode(&decoded) != claim {
        bail!("Gateway claim must be canonical unpadded base64url for exactly 32 bytes");
    }
    Ok(())
}

fn validate_canonical_uuid(description: &str, value: &str) -> anyhow::Result<()> {
    let parsed = Uuid::parse_str(value).with_context(|| format!("{description} is not a UUID"))?;
    if parsed.hyphenated().to_string() != value {
        bail!("{description} must be a lowercase hyphenated UUID");
    }
    Ok(())
}

pub(crate) fn validate_sha256(description: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{description} SHA-256 must be lowercase hexadecimal");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
    use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};
    use sha2::Sha256;
    use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};
    use tp_gateway::scope::ScopeStore;

    use super::*;

    const GATEWAY_ID: &str = "018f0c20-7b64-7a29-9bd1-6e4a598237d1";
    const BOOT_ID: &str = "018f0c20-7b64-7a29-9bd1-6e4a598237d2";
    const NONCE: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const TUNNEL_ID: &str = "11111111-1111-4111-8111-111111111111";
    const TUNNEL_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
    const SNAPSHOT_DIGEST: &str =
        "78ab043348542f62b03b09d8f0b2b972bd3f6f091111e888075ed48e40c4f07f";

    fn snapshot_item(relay_quota: Option<RelayQuotaSnapshot>) -> ScopeSnapshotItem {
        ScopeSnapshotItem {
            tunnel_id: TUNNEL_ID.into(),
            tunnel_signing_public_key: TUNNEL_KEY.into(),
            relay_quota,
        }
    }

    #[test]
    fn scope_without_a_relay_quota_still_validates() {
        let (scopes, quotas) = validate_scope_snapshot(
            &format!("byog:{GATEWAY_ID}"),
            SNAPSHOT_DIGEST,
            vec![snapshot_item(None)],
        )
        .expect("a Platform that sends no budget must still converge");

        assert_eq!(scopes.len(), 1);
        assert!(quotas.is_empty(), "no budget means no ceiling");
    }

    #[test]
    fn a_relay_quota_rides_along_without_changing_the_digest() {
        let quota = RelayQuotaSnapshot {
            period_yyyymm: "202608".into(),
            quota_bytes: 50,
            remaining_bytes: 20,
        };
        // The same digest as the quota-free snapshot above: budgets are outside
        // it on purpose, so an old and a new Gateway agree on convergence.
        let (scopes, quotas) = validate_scope_snapshot(
            &format!("byog:{GATEWAY_ID}"),
            SNAPSHOT_DIGEST,
            vec![snapshot_item(Some(quota))],
        )
        .expect("a budget must not disturb the Scope digest");

        assert_eq!(scopes.len(), 1);
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].0, TUNNEL_ID);
        assert_eq!(quotas[0].1.remaining_bytes, 20);
    }

    #[test]
    fn an_older_platform_snapshot_json_still_parses() {
        let item: ScopeSnapshotItem = serde_json::from_str(
            r#"{"tunnel_id":"11111111-1111-4111-8111-111111111111","tunnel_signing_public_key":"k"}"#,
        )
        .expect("the field is optional so an unupgraded Platform keeps working");

        assert!(item.relay_quota.is_none());
    }

    fn static_scope() -> tp_core::provisioning::GatewayScopeFileV2 {
        TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example.com".into(),
            port: 443,
            mapping_port: None,
            tls_server_name: None,
            trusted_certificate_pem: None,
        })
        .unwrap()
        .scope()
        .unwrap()
    }

    #[tokio::test]
    async fn challenge_then_full_snapshot_emits_authenticate_and_ready_gateway_state() {
        let signing_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let verification_key = signing_key.public_key_raw().to_vec();
        let certificate_pem = CertificateParams::new(vec!["203.0.113.42".into()])
            .unwrap()
            .self_signed(&signing_key)
            .unwrap()
            .pem();
        let leaf = tp_transport::tls::parse_certs(certificate_pem.as_bytes())
            .unwrap()
            .remove(0);
        let leaf_sha256 = hex::encode(Sha256::digest(leaf.as_ref()));
        let private_key_pem = signing_key.serialize_pem();
        let missing_claim = GatewayControlIdentity::new(
            GatewayControlKind::Byog,
            GATEWAY_ID,
            BOOT_ID,
            &leaf_sha256,
            private_key_pem.as_bytes(),
            Some(certificate_pem.clone()),
            None,
        )
        .err()
        .expect("certificate-only authentication must be rejected");
        assert!(missing_claim.to_string().contains("both be present"));
        let identity = GatewayControlIdentity::new(
            GatewayControlKind::Byog,
            GATEWAY_ID,
            BOOT_ID,
            &leaf_sha256,
            private_key_pem.as_bytes(),
            Some(certificate_pem.clone()),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into()),
        )
        .unwrap();

        let static_dir = tempfile::tempdir().unwrap();
        let static_scope = static_scope();
        fs::write(
            static_dir.path().join("static.scope"),
            serde_yaml::to_string(&static_scope).unwrap(),
        )
        .unwrap();
        let scopes = Arc::new(ScopeStore::new());
        scopes.reload_static(static_dir.path()).unwrap();
        let mut session = GatewayControlSession::new(identity, scopes.clone());

        let auth = session
            .handle_text(&format!(
                r#"{{"version":2,"type":"challenge","nonce":"{NONCE}"}}"#
            ))
            .unwrap();
        let outbound = auth.outbound.expect("authenticate response");
        assert!(outbound.to_text().unwrap().len() <= MAX_AUTHENTICATE_FRAME_BYTES);
        let GatewayFrame::Authenticate {
            version,
            boot_id,
            signature,
            claim_secret,
            certificate_pem: sent_certificate,
        } = outbound
        else {
            panic!("challenge must emit authenticate")
        };
        assert_eq!(version, 2);
        assert_eq!(boot_id, BOOT_ID);
        assert_eq!(
            claim_secret.as_deref(),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        );
        assert_eq!(sent_certificate.as_deref(), Some(certificate_pem.as_str()));
        let preimage = format!(
            "lantunnel-gateway-control-auth-v2\nkind=byog\nid={GATEWAY_ID}\nboot_id={BOOT_ID}\nnonce={NONCE}\nleaf_sha256={leaf_sha256}\n"
        );
        let signature = URL_SAFE_NO_PAD.decode(signature).unwrap();
        assert_eq!(signature.len(), 64);
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, verification_key)
            .verify(preimage.as_bytes(), &signature)
            .expect("fixed-width P-256 signature verifies over the frozen preimage");

        let snapshot = format!(
            r#"{{"version":2,"type":"scope_snapshot","digest":"{SNAPSHOT_DIGEST}","scopes":[{{"tunnel_id":"{TUNNEL_ID}","tunnel_signing_public_key":"{TUNNEL_KEY}"}}]}}"#
        );
        let action = session.handle_text(&snapshot).unwrap();
        assert!(action.outbound.is_none());
        let applied = action.applied_snapshot.expect("applied snapshot response");
        assert!(applied.removed_tunnel_ids.is_empty());
        let prepared =
            prepare_applied_snapshot(applied, |_| unreachable!(), |_, _, _, _| {}, || Ok(None))
                .unwrap();
        let readiness_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let readiness: GatewayReadinessProbe = Arc::new({
            let readiness_calls = readiness_calls.clone();
            move || {
                let call = readiness_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move { call == 0 })
            }
        });
        let outbound = refresh_gateway_state_readiness(prepared.state, &readiness).await;
        assert!(outbound.to_text().unwrap().len() <= MAX_GATEWAY_STATE_FRAME_BYTES);
        let GatewayFrame::GatewayState {
            version,
            boot_id,
            ready,
            applied_scope_digest,
        } = outbound
        else {
            panic!("scope snapshot must emit gateway_state")
        };
        assert_eq!(version, 2);
        assert_eq!(boot_id, BOOT_ID);
        assert!(ready);
        assert_eq!(applied_scope_digest, SNAPSHOT_DIGEST);
        assert!(scopes.contains(&static_scope.tunnel_id));
        assert!(scopes.contains(TUNNEL_ID));
        assert_eq!(scopes.static_len(), 1);
        assert_eq!(scopes.managed_len(), 1);

        let periodic = refresh_gateway_state_readiness(
            session.periodic_gateway_state().expect("periodic state"),
            &readiness,
        )
        .await;
        assert!(matches!(
            periodic,
            GatewayFrame::GatewayState { ready: false, .. }
        ));
        assert_eq!(
            readiness_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "immediate and periodic states must each run a fresh readiness probe"
        );

        let empty_digest = hex::encode(Sha256::digest(
            format!("{SCOPE_SNAPSHOT_DIGEST_DOMAIN}\nbyog:{GATEWAY_ID}\n[]").as_bytes(),
        ));
        let removed = session
            .handle_text(&format!(
                r#"{{"version":2,"type":"scope_snapshot","digest":"{empty_digest}","scopes":[]}}"#
            ))
            .unwrap();
        assert!(!scopes.contains(TUNNEL_ID), "replace happens first");
        let events = std::cell::RefCell::new(vec!["replace"]);
        let prepared = prepare_applied_snapshot(
            removed.applied_snapshot.expect("removed snapshot response"),
            |tunnel_id| {
                assert_eq!(tunnel_id, TUNNEL_ID);
                events.borrow_mut().push("disconnect");
            },
            |_, _, _, _| {},
            || {
                events.borrow_mut().push("wal_flush");
                Ok(None)
            },
        )
        .unwrap();
        assert!(prepared.usage.is_none());
        assert!(matches!(
            prepared.state,
            GatewayFrame::GatewayState { ref applied_scope_digest, .. }
                if applied_scope_digest == &empty_digest
        ));
        events.borrow_mut().push("gateway_state");
        assert_eq!(
            events.into_inner(),
            ["replace", "disconnect", "wal_flush", "gateway_state"]
        );
    }
}
