//! Shared P2P bootstrap helper: builds the listener, attaches signaling
//! channels, and spawns the [`P2pManager`] on top of an already-connected
//! [`Engine`].
//!
//! Every public V2 Peer runs the same symmetric bootstrap. Signed membership
//! and [`PeerLinkManager`] determine which side opens each relationship.

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::p2p::{cert, listener, manager, session};
use crate::peer_link_manager::{PeerConnectivity, PeerDescriptor, PeerLinkManager};
use crate::Engine;
use tp_core::config::ClientP2pConfig;
use tp_core::p2p_types::CertFingerprint;
use tp_core::protocol::BinaryMessage;

const RELAY_ANCHOR_READY_WARN_SECS: u64 = 90;
const RELAY_ANCHOR_POLL_INTERVAL: Duration = Duration::from_millis(200);
const P2P_UNDERLAY_INVENTORY_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
/// Let a moving NIC finish settling before rebuilding the pinned generation,
/// so a flapping adapter cannot drive a rebuild loop.
const P2P_UNDERLAY_RESTART_SETTLE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
struct UnderlayInterfaceCandidate {
    name: String,
    ip: IpAddr,
    index: Option<u32>,
    loopback: bool,
}

/// A kernel-selected underlay interface and the host addresses that belong to
/// it. The interface index pins P2P UDP sockets; the address set prevents a
/// pinned socket from advertising Host Link Candidates from another NIC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2pUnderlay {
    interface_name: String,
    interface_index: NonZeroU32,
    ipv4_interface_index: Option<NonZeroU32>,
    ipv6_interface_index: Option<NonZeroU32>,
    ipv4_source_ip: Option<std::net::Ipv4Addr>,
    host_ips: std::collections::BTreeSet<IpAddr>,
}

#[derive(Clone, Debug)]
struct P2pUnderlayInventory {
    gateway_endpoint: std::net::SocketAddr,
    underlay: P2pUnderlay,
    native_route_exclusions: std::collections::BTreeSet<std::net::Ipv4Addr>,
    connected_lans: Vec<crate::peer_runtime::LanExportPrefixV2>,
}

impl PartialEq for P2pUnderlayInventory {
    fn eq(&self, other: &Self) -> bool {
        self.gateway_endpoint.ip() == other.gateway_endpoint.ip()
            && self.underlay == other.underlay
            && self.native_route_exclusions == other.native_route_exclusions
            && self.connected_lans == other.connected_lans
    }
}

impl Eq for P2pUnderlayInventory {}

impl P2pUnderlay {
    pub fn interface_name(&self) -> &str {
        &self.interface_name
    }

    pub fn interface_index(&self) -> NonZeroU32 {
        self.interface_index
    }

    pub fn ipv4_interface_index(&self) -> Option<NonZeroU32> {
        self.ipv4_interface_index
    }

    pub fn ipv6_interface_index(&self) -> Option<NonZeroU32> {
        self.ipv6_interface_index
    }

    pub fn ipv4_source_ip(&self) -> Option<std::net::Ipv4Addr> {
        self.ipv4_source_ip
    }

    pub fn interface_index_for_addr(&self, addr: std::net::SocketAddr) -> Option<NonZeroU32> {
        match addr {
            std::net::SocketAddr::V4(_) => self.ipv4_interface_index,
            std::net::SocketAddr::V6(_) => self.ipv6_interface_index,
        }
    }

    pub fn host_ips(&self) -> &std::collections::BTreeSet<IpAddr> {
        &self.host_ips
    }
}

/// What one watchdog observation says about the generation that armed it.
/// Only [`UnderlayInventoryOutcome::Changed`] is a durable re-bind reason: the
/// pinned Listener, probe, and punch sockets belong to host addresses that no
/// longer exist, so the generation must be rebuilt rather than merely demoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnderlayInventoryOutcome {
    Unchanged,
    Changed,
    Lost,
    Superseded,
}

fn apply_underlay_inventory_observation(
    engine: &Engine,
    token: crate::engine::P2pUnderlayGeneration,
    expected: &P2pUnderlayInventory,
    observed: anyhow::Result<Option<P2pUnderlayInventory>>,
) -> UnderlayInventoryOutcome {
    if !engine.p2p_underlay_generation_is_ready(token) {
        return UnderlayInventoryOutcome::Superseded;
    }
    let outcome = match observed {
        Ok(Some(actual)) if actual == *expected => return UnderlayInventoryOutcome::Unchanged,
        Ok(Some(_)) => UnderlayInventoryOutcome::Changed,
        Ok(None) | Err(_) => UnderlayInventoryOutcome::Lost,
    };
    if engine.set_p2p_underlay_generation_ready(token, false) {
        tracing::warn!(
            reason = "underlay_inventory_changed",
            action = "native_lan_routes_withdrawn",
            "P2P underlay inventory proof lost"
        );
    }
    outcome
}

type UnderlayInventoryObserver =
    Arc<dyn Fn() -> anyhow::Result<Option<P2pUnderlayInventory>> + Send + Sync + 'static>;

async fn run_underlay_inventory_watchdog_with_observer(
    engine: Arc<Engine>,
    token: crate::engine::P2pUnderlayGeneration,
    expected: P2pUnderlayInventory,
    cancel: CancellationToken,
    interval: Duration,
    observer: UnderlayInventoryObserver,
    restart: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        if !engine.p2p_underlay_generation_is_ready(token) {
            return;
        }
        let observer = Arc::clone(&observer);
        let observed = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = tokio::task::spawn_blocking(move || observer()) => {
                result.unwrap_or_else(|error| Err(anyhow::Error::new(error)))
            }
        };
        match apply_underlay_inventory_observation(&engine, token, &expected, observed) {
            UnderlayInventoryOutcome::Unchanged => {}
            UnderlayInventoryOutcome::Changed => {
                tracing::warn!(
                    reason = "underlay_inventory_changed",
                    action = "p2p_generation_restart_requested",
                    "P2P underlay moved; rebuilding pinned Listener and Manager sockets"
                );
                restart.cancel();
                return;
            }
            UnderlayInventoryOutcome::Lost | UnderlayInventoryOutcome::Superseded => return,
        }
    }
}

fn underlay_revalidation_gateway(
    expected_gateway: std::net::SocketAddr,
    live_gateway: Option<std::net::SocketAddr>,
) -> std::net::SocketAddr {
    live_gateway.unwrap_or(expected_gateway)
}

fn discover_underlay_inventory(
    gateway: std::net::SocketAddr,
) -> anyhow::Result<P2pUnderlayInventory> {
    let underlay = discover_p2p_underlay(gateway)?;
    let native_route_exclusions = discover_native_route_exclusions(gateway, &underlay)?;
    let connected_lans = crate::native_route_guard::discover_connected_lan_prefixes()?;
    Ok(P2pUnderlayInventory {
        gateway_endpoint: gateway,
        underlay,
        native_route_exclusions,
        connected_lans,
    })
}

fn discover_native_route_exclusions(
    gateway: std::net::SocketAddr,
    underlay: &P2pUnderlay,
) -> anyhow::Result<std::collections::BTreeSet<std::net::Ipv4Addr>> {
    let generation_endpoints = underlay
        .host_ips()
        .iter()
        .copied()
        .chain(std::iter::once(gateway.ip()));
    Ok(crate::native_route_guard::discover_native_route_exclusions(
        generation_endpoints,
    )?)
}

fn live_underlay_inventory_observer(
    engine: Arc<Engine>,
    expected_gateway: std::net::SocketAddr,
) -> UnderlayInventoryObserver {
    Arc::new(move || {
        let live_gateway = engine
            .p2p_relay_context()
            .map(|(_, _, multi)| multi.relay().peer_addr());
        let gateway = underlay_revalidation_gateway(expected_gateway, live_gateway);
        discover_underlay_inventory(gateway).map(Some)
    })
}

struct P2pUnderlayReadinessGuard {
    engine: Arc<Engine>,
    token: crate::engine::P2pUnderlayGeneration,
}

impl P2pUnderlayReadinessGuard {
    fn arm(
        engine: Arc<Engine>,
        token: crate::engine::P2pUnderlayGeneration,
    ) -> anyhow::Result<Self> {
        if !engine.set_p2p_underlay_generation_ready(token, true) {
            anyhow::bail!("P2P underlay generation was superseded before Manager startup");
        }
        Ok(Self { engine, token })
    }
}

impl Drop for P2pUnderlayReadinessGuard {
    fn drop(&mut self) {
        let _ = self
            .engine
            .set_p2p_underlay_generation_ready(self.token, false);
    }
}

struct P2pUnderlayBootstrapCleanup {
    engine: Arc<Engine>,
    token: crate::engine::P2pUnderlayGeneration,
    transferred_to_manager: bool,
}

impl P2pUnderlayBootstrapCleanup {
    fn begin(engine: Arc<Engine>) -> Self {
        let token = engine.begin_p2p_underlay_generation();
        Self {
            engine,
            token,
            transferred_to_manager: false,
        }
    }

    fn token(&self) -> crate::engine::P2pUnderlayGeneration {
        self.token
    }

    fn transfer_to_manager(&mut self) {
        self.transferred_to_manager = true;
    }
}

impl Drop for P2pUnderlayBootstrapCleanup {
    fn drop(&mut self) {
        if !self.transferred_to_manager {
            let _ = self
                .engine
                .set_p2p_underlay_generation_ready(self.token, false);
        }
    }
}

fn spawn_manager_with_underlay_readiness(
    engine: &Arc<Engine>,
    manager: manager::P2pManager,
    token: crate::engine::P2pUnderlayGeneration,
    underlay_bypass_ready: bool,
    underlay_inventory: Option<P2pUnderlayInventory>,
    cancel: CancellationToken,
    restart: CancellationToken,
) -> anyhow::Result<()> {
    let readiness_guard = if underlay_bypass_ready {
        Some(P2pUnderlayReadinessGuard::arm(Arc::clone(engine), token)?)
    } else {
        None
    };
    if let Some(expected) = underlay_inventory {
        let observer =
            live_underlay_inventory_observer(Arc::clone(engine), expected.gateway_endpoint);
        // The watchdog runs for every pinned generation, not just LAN Route
        // Alias ones: an underlay move invalidates the exact source address
        // every P2P socket is bound to, whatever the alias policy says.
        engine
            .tasks()
            .spawn(run_underlay_inventory_watchdog_with_observer(
                Arc::clone(engine),
                token,
                expected,
                cancel,
                P2P_UNDERLAY_INVENTORY_WATCHDOG_INTERVAL,
                observer,
                restart,
            ));
    }
    engine.tasks().spawn(async move {
        let _readiness_guard = readiness_guard;
        manager.run().await;
    });
    Ok(())
}

fn is_tunnel_interface_name(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    name == "lantunnel"
        || name.starts_with("lantun")
        || name.starts_with("utun")
        || name.starts_with("tun")
        || name.starts_with("tap")
        || name.contains("wintun")
}

fn is_virtual_non_underlay_interface_name(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    is_tunnel_interface_name(&name)
        || name.starts_with("bridge")
        || name.starts_with("docker")
        || name.starts_with("virbr")
        || name.starts_with("veth")
        || name.starts_with("vmnet")
        || name.starts_with("vmenet")
        || name.starts_with("awdl")
        || name.starts_with("llw")
        || name.starts_with("anpi")
        || name.starts_with("ap")
        || name.contains("vethernet")
        || name.contains("hyper-v")
        || name.contains("wsl")
}

fn is_usable_underlay_host_ip(ip: IpAddr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && match ip {
            IpAddr::V4(ip) => !ip.is_link_local(),
            IpAddr::V6(ip) => !ip.is_unicast_link_local(),
        }
}

#[cfg(test)]
fn select_underlay_interface_index(
    source_ip: IpAddr,
    candidates: &[UnderlayInterfaceCandidate],
) -> std::io::Result<NonZeroU32> {
    select_underlay_interface(source_ip, candidates).map(|(_, index)| index)
}

fn select_underlay_interface(
    source_ip: IpAddr,
    candidates: &[UnderlayInterfaceCandidate],
) -> std::io::Result<(String, NonZeroU32)> {
    let adapters = candidates
        .iter()
        .filter(|candidate| {
            candidate.ip == source_ip
                && !candidate.loopback
                && !is_virtual_non_underlay_interface_name(&candidate.name)
        })
        .filter_map(|candidate| {
            candidate
                .index
                .and_then(NonZeroU32::new)
                .map(|index| (candidate.name.clone(), index))
        })
        .collect::<std::collections::BTreeSet<_>>();

    if adapters.len() == 1 {
        return Ok(adapters
            .into_iter()
            .next()
            .expect("one checked underlay adapter"));
    }
    if !adapters.is_empty() || source_ip.is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "gateway route did not resolve to exactly one safe indexed underlay interface",
        ));
    }

    // A system VPN can own the normal Gateway route, so the kernel-selected
    // source is a utun/tun address. Pinning to that same interface would not
    // bypass learned LAN `/32`s. Fall back only when exactly one physical-ish
    // interface remains; dual-NIC ambiguity fails closed instead of guessing.
    let source_is_tunnel = candidates
        .iter()
        .any(|candidate| candidate.ip == source_ip && is_tunnel_interface_name(&candidate.name));
    if !source_is_tunnel {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "gateway route source is not owned by a safe underlay interface",
        ));
    }
    let fallback_adapters = candidates
        .iter()
        .filter(|candidate| {
            !candidate.loopback
                && !is_virtual_non_underlay_interface_name(&candidate.name)
                && is_usable_underlay_host_ip(candidate.ip)
        })
        .filter_map(|candidate| {
            candidate
                .index
                .and_then(NonZeroU32::new)
                .map(|_| candidate.name.clone())
        })
        .collect::<std::collections::BTreeSet<_>>();
    if fallback_adapters.len() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "tunneled Gateway route did not leave exactly one safe physical underlay interface",
        ));
    }
    let adapter_name = fallback_adapters
        .into_iter()
        .next()
        .expect("one checked fallback interface");
    let route_family_indexes = candidates
        .iter()
        .filter(|candidate| {
            candidate.name == adapter_name
                && candidate.ip.is_ipv4() == source_ip.is_ipv4()
                && !candidate.loopback
                && is_usable_underlay_host_ip(candidate.ip)
        })
        .filter_map(|candidate| candidate.index.and_then(NonZeroU32::new))
        .collect::<std::collections::BTreeSet<_>>();
    if route_family_indexes.len() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "physical underlay interface did not expose exactly one index for the Gateway address family",
        ));
    }
    Ok((
        adapter_name,
        *route_family_indexes
            .iter()
            .next()
            .expect("one checked route-family interface index"),
    ))
}

fn gateway_route_source_ip(gateway: std::net::SocketAddr) -> std::io::Result<IpAddr> {
    if gateway.ip().is_unspecified() || gateway.ip().is_multicast() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "gateway address cannot select a usable underlay route",
        ));
    }
    let bind_addr = if gateway.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    socket.connect(gateway)?;
    let source_ip = socket.local_addr()?.ip();
    if source_ip.is_unspecified() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "kernel route lookup returned an unspecified source address",
        ));
    }
    Ok(source_ip)
}

/// Resolve the route to the already-connected Gateway into one safe indexed
/// underlay interface. Callers can use success as the readiness gate before
/// installing learned peer-LAN `/32` routes into a TUN.
pub fn discover_p2p_underlay(gateway: std::net::SocketAddr) -> std::io::Result<P2pUnderlay> {
    let source_ip = gateway_route_source_ip(gateway)?;
    let candidates = if_addrs::get_if_addrs()?
        .into_iter()
        .map(|interface| {
            let ip = interface.ip();
            let loopback = interface.is_loopback();
            UnderlayInterfaceCandidate {
                name: interface.name,
                ip,
                index: interface.index,
                loopback,
            }
        })
        .collect::<Vec<_>>();
    p2p_underlay_from_candidates(source_ip, &candidates)
}

fn p2p_underlay_from_candidates(
    source_ip: IpAddr,
    candidates: &[UnderlayInterfaceCandidate],
) -> std::io::Result<P2pUnderlay> {
    let (interface_name, interface_index) = select_underlay_interface(source_ip, candidates)?;
    let family_interface_index = |ipv4: bool| -> std::io::Result<Option<NonZeroU32>> {
        let indexes = candidates
            .iter()
            .filter(|candidate| {
                candidate.name == interface_name
                    && candidate.ip.is_ipv4() == ipv4
                    && !candidate.loopback
                    && !is_virtual_non_underlay_interface_name(&candidate.name)
                    && is_usable_underlay_host_ip(candidate.ip)
            })
            .filter_map(|candidate| candidate.index.and_then(NonZeroU32::new))
            .collect::<std::collections::BTreeSet<_>>();
        if indexes.len() > 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "one physical underlay interface exposed multiple indexes for one address family",
            ));
        }
        Ok(indexes.into_iter().next())
    };
    let ipv4_interface_index = family_interface_index(true)?;
    let ipv6_interface_index = family_interface_index(false)?;
    let host_ips = candidates
        .iter()
        .filter(|candidate| {
            candidate.name == interface_name
                && !candidate.loopback
                && !is_virtual_non_underlay_interface_name(&candidate.name)
                && is_usable_underlay_host_ip(candidate.ip)
                && match candidate.ip {
                    IpAddr::V4(_) => candidate.index == ipv4_interface_index.map(NonZeroU32::get),
                    IpAddr::V6(_) => candidate.index == ipv6_interface_index.map(NonZeroU32::get),
                }
        })
        .map(|candidate| candidate.ip)
        .collect::<std::collections::BTreeSet<_>>();
    let source_is_tunnel = candidates
        .iter()
        .any(|candidate| candidate.ip == source_ip && is_tunnel_interface_name(&candidate.name));
    if !host_ips.contains(&source_ip) && !source_is_tunnel {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "selected underlay interface no longer owns the gateway route source",
        ));
    }
    let ipv4_source_ip = match source_ip {
        IpAddr::V4(source) if host_ips.contains(&IpAddr::V4(source)) => Some(source),
        _ => {
            let mut ipv4_hosts = host_ips.iter().filter_map(|ip| match ip {
                IpAddr::V4(ip) => Some(*ip),
                IpAddr::V6(_) => None,
            });
            let first = ipv4_hosts.next();
            first.filter(|_| ipv4_hosts.next().is_none())
        }
    };
    Ok(P2pUnderlay {
        interface_name,
        interface_index,
        ipv4_interface_index,
        ipv6_interface_index,
        ipv4_source_ip,
        host_ips,
    })
}

fn mesh_membership_from_latest_tunnel_config(
    engine: &Engine,
) -> anyhow::Result<(PeerLinkManager, String)> {
    let tunnel_config = engine
        .latest_tunnel_config()
        .ok_or_else(|| anyhow::anyhow!("latest TunnelConfig is unavailable for mesh bootstrap"))?;
    let tunnel_id = tunnel_config.tunnel_id.clone();
    let runtime_replica_count = tunnel_config.replicas as usize;
    if tunnel_config.client_ids.len() != runtime_replica_count {
        anyhow::bail!(
            "invalid local Peer Replica set: replicas={} but client_ids={}",
            tunnel_config.replicas,
            tunnel_config.client_ids.len()
        );
    }
    let v2_profile = engine.active_v2_peer_profile();
    let local_peer = match &v2_profile {
        Some(profile) => PeerDescriptor::from_stable_peer_and_replica_ids(
            profile.peer.peer_id.clone(),
            tunnel_config.client_ids,
        ),
        None => PeerDescriptor::from_replica_ids(tunnel_config.client_ids),
    }
    .map_err(|error| anyhow::anyhow!("invalid local Peer Replica set: {error}"))?;
    // V2 has one logical PeerLink between stable Peers. Runtime Gateway
    // replica fan-out remains a transport detail and must not multiply the
    // identity relationship or its Relay keys.
    let configured_replica_count = if v2_profile.is_some() {
        1
    } else {
        runtime_replica_count
    };
    let manager = PeerLinkManager::new(local_peer, configured_replica_count)
        .map_err(|error| anyhow::anyhow!("invalid mesh Replica count: {error}"))?;
    Ok((manager, tunnel_id))
}

fn wire_mesh_membership(
    engine: &Arc<Engine>,
    manager: &mut manager::P2pManager,
    peer_link_manager: PeerLinkManager,
    tunnel_id: String,
) {
    if let Some(profile) = engine.active_v2_peer_profile() {
        manager.set_v2_profile(profile);
        let membership_engine = Arc::clone(engine);
        manager.set_v2_membership_sink(move |membership| {
            if let Err(error) = membership_engine.install_v2_peer_membership(membership) {
                tracing::warn!(%error, "verified V2 Peer membership could not install Overlay route");
            }
        });
        let membership_cycle_engine = Arc::clone(engine);
        manager.set_v2_membership_cycle_sink(move |peer_ids| {
            membership_cycle_engine.commit_delivered_v2_membership_cycle(peer_ids)
        });
        let current_peer_engine = Arc::clone(engine);
        manager.set_v2_current_peer_authority_source(move |peer_id| {
            current_peer_engine.is_v2_current_member(peer_id)
        });
        let peer_link_engine = Arc::clone(engine);
        manager.set_v2_peer_link_sink(move |peer_id, session_id, keys| {
            if let Err(error) = peer_link_engine.install_v2_peer_link(peer_id, session_id, keys) {
                tracing::warn!(?session_id, %error, "verified V2 PeerLink keys could not install");
            }
        });
    }
    manager.set_peer_link_manager(peer_link_manager);
    let health_engine = Arc::clone(engine);
    manager.set_peer_connectivity_source(move |peer_id| PeerConnectivity {
        healthy_direct: health_engine.has_healthy_direct_path_for_peer(peer_id.as_str()),
        // A Peer absent from the authenticated full membership cycle has no
        // live Replica that can be named by a new exact relay bind. Do not
        // infer relay availability from the local Gateway connection alone.
        usable_exact_relay: health_engine.has_usable_exact_relay_for_peer(peer_id.as_str()),
    });
    let retirement_engine = Arc::clone(engine);
    manager.set_retired_peer_sink(move |peer_id| {
        let committed = retirement_engine.retire_overlay_peer(peer_id.as_str());
        if committed {
            tracing::debug!(peer_id = %peer_id.as_str(), "retired absent Peer exact routes");
        }
        committed
    });
    let membership_engine = Arc::clone(engine);
    manager.set_membership_commit_sink(move |replica_ids| {
        for replica_id in replica_ids {
            if let Err(error) = membership_engine.install_overlay_replica(&tunnel_id, replica_id) {
                tracing::warn!(
                    %tunnel_id,
                    %replica_id,
                    %error,
                    "committed mesh Replica rejected by Overlay route matcher"
                );
            }
        }
    });
}

/// Own the P2P subsystem for the whole connection: bootstrap one pinned
/// generation and return once it is up, then rebuild it in the background
/// whenever the underlay moves.
///
/// Pinned P2P sockets are bound to one exact host source address, so a Wi-Fi
/// change, DHCP renewal, or sleep/wake leaves the Listener, mapping probe, and
/// every future punch socket attached to an address the kernel no longer owns.
/// Demoting readiness is not enough — the generation has to be torn down and
/// re-bound against the current underlay.
pub async fn run(
    engine: Arc<Engine>,
    p2p_cfg: ClientP2pConfig,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    // Each generation owns a child token so a rebuild closes exactly its own
    // Listener, installer, and Manager without ending the connection.
    let generation_cancel = cancel.child_token();
    let restart = CancellationToken::new();
    if let Err(error) = run_generation(
        Arc::clone(&engine),
        p2p_cfg.clone(),
        generation_cancel.clone(),
        restart.clone(),
    )
    .await
    {
        generation_cancel.cancel();
        return Err(error);
    }
    // Returning here keeps the caller's contract: the first generation is up.
    // Later rebuilds are supervised on the engine-lifetime tracker.
    engine.tasks().spawn(run_underlay_rebuild_loop(
        Arc::clone(&engine),
        p2p_cfg,
        cancel,
        generation_cancel,
        restart,
    ));
    Ok(())
}

/// Replace the pinned P2P generation each time its underlay moves.
///
/// A rebuild is never fatal: a generation that cannot bind is retried on the
/// next settle tick rather than dropping the client to permanent Relay-only.
async fn run_underlay_rebuild_loop(
    engine: Arc<Engine>,
    p2p_cfg: ClientP2pConfig,
    cancel: CancellationToken,
    mut generation_cancel: CancellationToken,
    mut restart: CancellationToken,
) {
    let mut generation: u64 = 0;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = restart.cancelled() => {}
        }
        generation_cancel.cancel();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(P2P_UNDERLAY_RESTART_SETTLE) => {}
        }
        generation += 1;
        generation_cancel = cancel.child_token();
        restart = CancellationToken::new();
        if let Err(error) = run_generation(
            Arc::clone(&engine),
            p2p_cfg.clone(),
            generation_cancel.clone(),
            restart.clone(),
        )
        .await
        {
            tracing::warn!(generation, %error, "P2P generation rebuild failed; retrying");
            // Fall straight through to the next settle instead of parking on a
            // watchdog this failed generation never started.
            restart.cancel();
        }
    }
}

/// Bootstrap one P2P listener + [`P2pManager`] generation on top of a
/// connected engine. Self-contained: only calls `pub` methods on [`Engine`].
///
/// Polls [`Engine::tunnel_identity`] until the live replica installs it
/// (with periodic warnings while waiting), then builds the cert bundle,
/// binds the QUIC listener, attaches the signaling channels, applies the
/// runtime knobs from `p2p_cfg`, and spawns the manager.
async fn run_generation(
    engine: Arc<Engine>,
    p2p_cfg: ClientP2pConfig,
    cancel: CancellationToken,
    restart: CancellationToken,
) -> anyhow::Result<()> {
    // A reconnect generation must prove its own pinned Listener + Manager
    // sockets before the desktop TUN may export learned peer-LAN `/32`s.
    // Every early error or cancellation therefore remains fail-closed.
    let mut underlay_cleanup = P2pUnderlayBootstrapCleanup::begin(Arc::clone(&engine));
    let (client_id, group_id, multi) = wait_for_p2p_relay_context(
        &engine,
        &cancel,
        Duration::from_secs(RELAY_ANCHOR_READY_WARN_SECS),
        RELAY_ANCHOR_POLL_INTERVAL,
    )
    .await?;
    let (peer_link_manager, tunnel_id) = mesh_membership_from_latest_tunnel_config(&engine)?;

    ensure_not_cancelled(&cancel)?;
    let bundle = cert::generate_self_signed_cert(&client_id)
        .map_err(|e| anyhow::anyhow!("p2p cert generation failed: {e}"))?;

    let mapping_probe_reflector = Some(crate::p2p::mapping_probe::mapping_probe_addr_for_gateway(
        multi.relay().peer_addr(),
        engine.managed_mapping_port(),
    ));
    if let Some(reflector) = mapping_probe_reflector {
        tracing::info!(
            reflector = %reflector,
            "P2P mapping probe endpoint resolved"
        );
    }

    let native_lan_bypass_required =
        p2p_cfg.allow_lan_route_aliases && p2p_cfg.allow_lan_candidates;
    let native_lan_inventory_enabled =
        p2p_cfg.allow_lan_route_aliases || engine.active_v2_peer_profile().is_some();
    let gateway_endpoint = multi.relay().peer_addr();
    let p2p_underlay = match discover_p2p_underlay(gateway_endpoint) {
        Ok(underlay) => {
            tracing::info!(
                interface_index = underlay.interface_index().get(),
                host_address_count = underlay.host_ips().len(),
                "P2P underlay bypass ready"
            );
            Some(underlay)
        }
        Err(error) if native_lan_bypass_required => {
            return Err(anyhow::anyhow!(
                "P2P underlay bypass is required when LAN Route Aliases and LAN Link Candidates are both enabled: {error}"
            ));
        }
        Err(_) => {
            tracing::debug!(
                reason = "underlay_bypass_unavailable",
                action = "p2p_socket_pinning_disabled",
                "P2P underlay bypass unavailable; continuing without pinned P2P sockets"
            );
            None
        }
    };
    let p2p_underlay_inventory = if native_lan_inventory_enabled {
        match p2p_underlay.as_ref() {
            Some(underlay) => match discover_native_route_exclusions(gateway_endpoint, underlay) {
                Ok(native_route_exclusions) => {
                    let connected_lans =
                        crate::native_route_guard::discover_connected_lan_prefixes()?;
                    engine.configure_native_lan_route_inventory(
                        underlay_cleanup.token(),
                        native_route_exclusions.clone(),
                        connected_lans.clone(),
                    )?;
                    Some(P2pUnderlayInventory {
                        gateway_endpoint,
                        underlay: underlay.clone(),
                        native_route_exclusions,
                        connected_lans,
                    })
                }
                Err(error) if native_lan_bypass_required => {
                    return Err(anyhow::anyhow!(
                        "native LAN route inventory is required when LAN Route Aliases and LAN Link Candidates are both enabled: {error}"
                    ));
                }
                Err(_) => None,
            },
            None => None,
        }
    } else {
        None
    };

    // Bind the QUIC listener on a fresh tuned UDP socket.
    ensure_not_cancelled(&cancel)?;
    let p2p_listener = listener::P2pListener::bind_with_mapping_probe_on_interfaces(
        &bundle,
        mapping_probe_reflector,
        p2p_underlay
            .as_ref()
            .map(|underlay| listener::P2pUnderlayInterfaceIndexes {
                ipv4: underlay.ipv4_interface_index(),
                ipv6: underlay.ipv6_interface_index(),
                ipv4_source_ip: underlay.ipv4_source_ip(),
            }),
    )?;
    let p2p_local_port = p2p_listener.local_addr().port();
    let listener_probe_socket = p2p_listener.probe_socket().ok();
    let listener_observed_public_addr = p2p_listener.mapping_probe_observed();
    let (endpoint, _local_addr) = p2p_listener.into_parts();

    // Diagnostic read-back slot retained for Engine/API compatibility. The
    // listener validates against the keyed `expected_peers` map below.
    let expected_fp: Arc<StdMutex<Option<CertFingerprint>>> = Arc::new(StdMutex::new(None));
    let expected_peers = crate::p2p::expected::ExpectedPeerMap::default();

    // Engine plumbing: signaling channels + fp handle.
    let (out_tx, out_rx) = mpsc::channel::<BinaryMessage>(64);
    let (in_tx, in_rx) = mpsc::channel::<BinaryMessage>(64);
    ensure_not_cancelled(&cancel)?;
    engine.attach_p2p_signaling(in_tx, out_rx);
    ensure_not_cancelled(&cancel)?;
    engine.set_p2p_expected_fp_handle(expected_fp.clone());

    // Telemetry sink (Task 4.12). One MetricsManager per process is
    // enough — incoming `incr_*` calls are lock-free atomic adds and the
    // structure is `Arc<...>` so the same handle plugs into engine,
    // multi-session, manager, and listener. The render endpoint is wired
    // by the GUI / IPC layer separately; that's not in scope here.
    let metrics = tp_metrics::MetricsManager::new();
    engine.set_metrics(Some(metrics.clone()));
    multi.set_metrics(Some(metrics.clone()));

    // Listener accept loop: split incoming Sessions, install the send half,
    // and pump receive halves through the engine dispatcher.
    let installer = engine.attach_p2p_session_installer_with_cancel(cancel.clone());
    let installer_for_listener = installer.clone();
    let install_tasks = engine.tasks();
    let on_session: Arc<
        dyn Fn(tp_core::p2p_types::SessionId, tp_transport::session::Session) + Send + Sync,
    > = Arc::new(
        move |session_id: tp_core::p2p_types::SessionId, sess: tp_transport::session::Session| {
            let installer = installer_for_listener.clone();
            install_tasks.spawn(async move {
                match installer.install(session_id, sess).await {
                    Ok(_) => tracing::debug!(?session_id, "accepted P2P session installed"),
                    Err(e) => {
                        tracing::warn!(?session_id, error = %e, "failed to install accepted P2P session");
                    }
                }
            });
        },
    );
    // Register the listener accept loop under the engine-lifetime
    // tracker so `Engine::disconnect()` drains it before returning.
    ensure_not_cancelled(&cancel)?;
    engine.tasks().spawn(listener::run_listener_loop(
        endpoint,
        expected_peers.clone(),
        on_session,
        cancel.clone(),
        Some(metrics.clone()),
    ));

    // Offer/Answer is a per-relationship role. Public clients never select a
    // manager-wide product role; PeerLinkManager supplies relationship
    // direction from signed V2 membership.
    let mut mgr = manager::P2pManager::new(
        multi,
        client_id.clone(),
        group_id.clone(),
        bundle.fingerprint,
        session::ClientRole::Acceptor,
        in_rx,
        out_tx,
        p2p_local_port,
    );
    wire_mesh_membership(&engine, &mut mgr, peer_link_manager, tunnel_id);
    mgr.set_expected_fp_handle(expected_fp);
    mgr.set_expected_peer_map(expected_peers);
    if let Some(socket) = listener_probe_socket {
        mgr.set_listener_probe_socket(socket);
    } else {
        tracing::debug!(
            p2p_local_port,
            "P2P listener probe socket clone unavailable; mapping probe cannot run for offers"
        );
    }
    mgr.set_mapping_probe_reflector(mapping_probe_reflector);
    mgr.set_listener_observed_public_addr(listener_observed_public_addr);
    mgr.set_session_installer(installer);
    mgr.set_tls_identity(&bundle);
    mgr.set_metrics(Some(metrics));
    // Apply the current Direct-path cooldown knobs.
    mgr.set_cooldown_config(p2p_cfg.cooldown_initial_secs, p2p_cfg.cooldown_max_secs);
    mgr.set_allow_lan_candidates(p2p_cfg.allow_lan_candidates);
    if let Some(underlay) = p2p_underlay.as_ref() {
        mgr.set_underlay_interface_indexes(
            underlay.ipv4_interface_index(),
            underlay.ipv6_interface_index(),
            underlay.ipv4_source_ip(),
            underlay.host_ips().iter().copied(),
        );
    }
    ensure_not_cancelled(&cancel)?;
    engine.set_p2p_refill_handle(mgr.refill_handle());
    // Same engine-lifetime tracker so `Engine::disconnect` joins on
    // the manager's `run()` (it owns the inner per-task TaskTracker that
    // tracker; both drains happen in series).
    ensure_not_cancelled(&cancel)?;
    spawn_manager_with_underlay_readiness(
        &engine,
        mgr,
        underlay_cleanup.token(),
        p2p_underlay.is_some(),
        p2p_underlay_inventory,
        cancel.clone(),
        restart,
    )?;
    underlay_cleanup.transfer_to_manager();

    tracing::info!(
        %client_id,
        %group_id,
        p2p_local_port,
        "symmetric P2P bootstrap complete"
    );
    Ok(())
}

fn ensure_not_cancelled(cancel: &CancellationToken) -> anyhow::Result<()> {
    if cancel.is_cancelled() {
        anyhow::bail!("P2P bootstrap cancelled");
    }
    Ok(())
}

async fn wait_for_p2p_relay_context(
    engine: &Engine,
    cancel: &CancellationToken,
    warn_after: Duration,
    poll_interval: Duration,
) -> anyhow::Result<(String, String, Arc<crate::p2p::session::MultiSession>)> {
    // Wait for the P2P anchor relay replica to install its identity and
    // MultiSession. Platform/gateway recovery can take longer than a fixed
    // startup timeout, so keep waiting for the current connect generation and
    // surface periodic warnings instead of permanently falling back to relay.
    let started_at = std::time::Instant::now();
    let mut next_warn_at = started_at + warn_after;
    loop {
        ensure_not_cancelled(cancel)?;
        if let Some(ctx) = engine.p2p_relay_context() {
            return Ok(ctx);
        }

        let now = std::time::Instant::now();
        if now >= next_warn_at {
            tracing::warn!(
                waited_ms = started_at.elapsed().as_millis(),
                "P2P relay anchor is not ready yet; continuing to wait"
            );
            next_warn_at = now + warn_after;
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                anyhow::bail!("P2P bootstrap cancelled");
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use bytes::Bytes;
    use dashmap::DashMap;
    use tokio::sync::mpsc;
    use tp_core::protocol::{BinaryMessage, PackedMessage};
    use tp_transport::{session::Session, DropOldestSender};

    use crate::p2p::session::MultiSession;
    use crate::platform::TunnelConfig;
    use crate::status::NullListener;
    use crate::{Engine, EngineConfig};

    fn channel_session() -> Arc<Session> {
        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        Arc::new(Session::new_channeled(
            out_tx, in_rx, peer, closer, writer, reader,
        ))
    }

    fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
        let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
    }

    fn underlay_inventory_fixture() -> P2pUnderlayInventory {
        let source = IpAddr::V4(Ipv4Addr::new(192, 168, 240, 44));
        let underlay = p2p_underlay_from_candidates(
            source,
            &[UnderlayInterfaceCandidate {
                name: "en0".into(),
                ip: source,
                index: Some(7),
                loopback: false,
            }],
        )
        .unwrap();
        P2pUnderlayInventory {
            gateway_endpoint: "203.0.113.8:443".parse().unwrap(),
            underlay,
            native_route_exclusions: [
                Ipv4Addr::new(192, 168, 240, 1),
                Ipv4Addr::new(192, 168, 240, 53),
            ]
            .into_iter()
            .collect(),
            connected_lans: vec![crate::peer_runtime::LanExportPrefixV2::new(
                Ipv4Addr::new(192, 168, 240, 0),
                24,
            )
            .unwrap()],
        }
    }

    fn ready_native_alias_generation(
        inventory: &P2pUnderlayInventory,
    ) -> (
        Arc<Engine>,
        crate::engine::P2pUnderlayGeneration,
        P2pUnderlayReadinessGuard,
    ) {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_candidates: true,
            allow_lan_route_aliases: true,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            ..TunnelConfig::default()
        });
        engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .unwrap();
        engine
            .replace_peer_lan_aliases("mesh-RemoteB1-0", &["192.168.241.20".into()])
            .unwrap();
        let token = engine.begin_p2p_underlay_generation();
        engine.set_native_lan_route_exclusions_for_test(
            &inventory
                .native_route_exclusions
                .iter()
                .copied()
                .collect::<Vec<_>>(),
        );
        let guard = P2pUnderlayReadinessGuard::arm(Arc::clone(&engine), token).unwrap();
        assert_eq!(engine.lan_alias_route_cidrs(), vec!["192.168.241.20/32"]);
        (engine, token, guard)
    }

    #[test]
    fn manager_underlay_readiness_is_true_only_for_the_guard_lifetime() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        assert!(!engine.p2p_underlay_bypass_ready_for_test());

        let token = engine.begin_p2p_underlay_generation();
        let guard = P2pUnderlayReadinessGuard::arm(Arc::clone(&engine), token).unwrap();
        assert!(
            engine.p2p_underlay_bypass_ready_for_test(),
            "a configured pinned Manager generation must enable TUN route export"
        );

        drop(guard);
        assert!(
            !engine.p2p_underlay_bypass_ready_for_test(),
            "normal return, cancellation, or future drop must revoke readiness"
        );
    }

    #[test]
    fn stale_manager_drop_does_not_clear_a_new_underlay_generation() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let old_token = engine.begin_p2p_underlay_generation();
        let old_guard = P2pUnderlayReadinessGuard::arm(Arc::clone(&engine), old_token).unwrap();
        let new_token = engine.begin_p2p_underlay_generation();
        let new_guard = P2pUnderlayReadinessGuard::arm(Arc::clone(&engine), new_token).unwrap();

        drop(old_guard);
        assert!(
            engine.p2p_underlay_bypass_ready_for_test(),
            "an old Manager generation must not clear the replacement generation"
        );

        drop(new_guard);
        assert!(!engine.p2p_underlay_bypass_ready_for_test());
    }

    #[test]
    fn unchanged_underlay_inventory_keeps_current_generation_ready() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let token = engine.begin_p2p_underlay_generation();
        let _guard = P2pUnderlayReadinessGuard::arm(Arc::clone(&engine), token).unwrap();
        let expected = underlay_inventory_fixture();

        assert_eq!(
            apply_underlay_inventory_observation(
                &engine,
                token,
                &expected,
                Ok(Some(expected.clone())),
            ),
            UnderlayInventoryOutcome::Unchanged,
        );
        assert!(engine.p2p_underlay_bypass_ready_for_test());
    }

    #[test]
    fn dns_inventory_change_withdraws_native_lan_routes() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);
        let mut observed = expected.clone();
        observed
            .native_route_exclusions
            .remove(&Ipv4Addr::new(192, 168, 240, 53));
        observed
            .native_route_exclusions
            .insert(Ipv4Addr::new(192, 168, 240, 54));

        assert_eq!(
            apply_underlay_inventory_observation(&engine, token, &expected, Ok(Some(observed))),
            UnderlayInventoryOutcome::Changed,
        );
        assert!(engine.lan_alias_route_cidrs().is_empty());
        assert_eq!(
            engine.resolve_overlay_peer("192.168.241.20:27015").unwrap(),
            Some("mesh-RemoteB1-0".into()),
            "SOCKS matching must survive native route withdrawal"
        );
    }

    #[test]
    fn live_gateway_ip_change_withdraws_native_lan_routes() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);
        let mut observed = expected.clone();
        observed
            .gateway_endpoint
            .set_ip(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)));

        assert_eq!(
            apply_underlay_inventory_observation(&engine, token, &expected, Ok(Some(observed))),
            UnderlayInventoryOutcome::Changed,
        );
        assert!(engine.lan_alias_route_cidrs().is_empty());
    }

    #[test]
    fn relay_replacement_on_same_gateway_ip_keeps_native_lan_routes() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);
        let mut observed = expected.clone();
        observed.gateway_endpoint.set_port(8443);

        assert_eq!(
            apply_underlay_inventory_observation(&engine, token, &expected, Ok(Some(observed))),
            UnderlayInventoryOutcome::Unchanged,
        );
        assert_eq!(engine.lan_alias_route_cidrs(), vec!["192.168.241.20/32"]);
    }

    #[test]
    fn transient_missing_live_relay_reuses_expected_gateway_for_revalidation() {
        let expected: SocketAddr = "203.0.113.8:443".parse().unwrap();
        let live: SocketAddr = "203.0.113.9:443".parse().unwrap();

        assert_eq!(underlay_revalidation_gateway(expected, None), expected);
        assert_eq!(underlay_revalidation_gateway(expected, Some(live)), live);
    }

    #[test]
    fn physical_adapter_identity_change_withdraws_native_lan_routes() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);
        let mut observed = expected.clone();
        observed.underlay.interface_name = "en1".into();

        assert_eq!(
            apply_underlay_inventory_observation(&engine, token, &expected, Ok(Some(observed))),
            UnderlayInventoryOutcome::Changed,
        );
        assert!(engine.lan_alias_route_cidrs().is_empty());
    }

    #[test]
    fn family_interface_index_change_withdraws_native_lan_routes() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);
        let mut observed = expected.clone();
        observed.underlay.ipv4_interface_index = NonZeroU32::new(8);

        assert_eq!(
            apply_underlay_inventory_observation(&engine, token, &expected, Ok(Some(observed))),
            UnderlayInventoryOutcome::Changed,
        );
        assert!(engine.lan_alias_route_cidrs().is_empty());
    }

    #[test]
    fn underlay_host_ip_change_withdraws_native_lan_routes() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);
        let mut observed = expected.clone();
        observed
            .underlay
            .host_ips
            .remove(&IpAddr::V4(Ipv4Addr::new(192, 168, 240, 44)));
        observed
            .underlay
            .host_ips
            .insert(IpAddr::V4(Ipv4Addr::new(192, 168, 240, 45)));

        assert_eq!(
            apply_underlay_inventory_observation(&engine, token, &expected, Ok(Some(observed))),
            UnderlayInventoryOutcome::Changed,
        );
        assert!(engine.lan_alias_route_cidrs().is_empty());
    }

    #[test]
    fn unknown_live_underlay_inventory_withdraws_native_lan_routes() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);

        assert_eq!(
            apply_underlay_inventory_observation(&engine, token, &expected, Ok(None)),
            UnderlayInventoryOutcome::Lost,
        );
        assert!(engine.lan_alias_route_cidrs().is_empty());
    }

    #[test]
    fn underlay_inventory_discovery_error_withdraws_native_lan_routes() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);

        assert_eq!(
            apply_underlay_inventory_observation(
                &engine,
                token,
                &expected,
                Err(anyhow::anyhow!("route inventory unavailable")),
            ),
            UnderlayInventoryOutcome::Lost,
        );
        assert!(engine.lan_alias_route_cidrs().is_empty());
    }

    #[test]
    fn stale_inventory_watchdog_stops_without_clearing_new_generation() {
        let expected = underlay_inventory_fixture();
        let (engine, stale_token, _stale_guard) = ready_native_alias_generation(&expected);
        let current_token = engine.begin_p2p_underlay_generation();
        engine.set_native_lan_route_exclusions_for_test(
            &expected
                .native_route_exclusions
                .iter()
                .copied()
                .collect::<Vec<_>>(),
        );
        let _current_guard =
            P2pUnderlayReadinessGuard::arm(Arc::clone(&engine), current_token).unwrap();

        assert_eq!(
            apply_underlay_inventory_observation(
                &engine,
                stale_token,
                &expected,
                Ok(Some(expected.clone())),
            ),
            UnderlayInventoryOutcome::Superseded,
        );
        assert!(engine.p2p_underlay_bypass_ready_for_test());
        assert_eq!(engine.lan_alias_route_cidrs(), vec!["192.168.241.20/32"]);
    }

    #[tokio::test]
    async fn cancelled_inventory_watchdog_exits_without_waiting_for_next_tick() {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_observer = Arc::clone(&calls);
        let observer: UnderlayInventoryObserver = Arc::new(move || {
            calls_for_observer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(None)
        });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let restart = CancellationToken::new();

        tokio::time::timeout(
            Duration::from_millis(100),
            run_underlay_inventory_watchdog_with_observer(
                Arc::clone(&engine),
                token,
                expected,
                cancel,
                Duration::from_secs(3600),
                observer,
                restart.clone(),
            ),
        )
        .await
        .expect("cancelled watchdog must exit immediately");

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(engine.p2p_underlay_bypass_ready_for_test());
        assert!(
            !restart.is_cancelled(),
            "a cancelled connection must not request a P2P generation rebuild"
        );
    }

    async fn watchdog_restart_request_for(
        observed: anyhow::Result<Option<P2pUnderlayInventory>>,
    ) -> bool {
        let expected = underlay_inventory_fixture();
        let (engine, token, _guard) = ready_native_alias_generation(&expected);
        let observed = Arc::new(StdMutex::new(Some(observed)));
        let observer: UnderlayInventoryObserver = Arc::new(move || {
            observed
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Ok(Some(underlay_inventory_fixture())))
        });
        let restart = CancellationToken::new();

        tokio::time::timeout(
            Duration::from_secs(5),
            run_underlay_inventory_watchdog_with_observer(
                Arc::clone(&engine),
                token,
                expected,
                CancellationToken::new(),
                Duration::ZERO,
                observer,
                restart.clone(),
            ),
        )
        .await
        .expect("watchdog must stop on the first non-matching observation");

        restart.is_cancelled()
    }

    #[tokio::test]
    async fn moved_underlay_requests_a_p2p_generation_rebuild() {
        // The pinned Listener, mapping probe, and every future punch socket are
        // bound to the old exact host address, so demoting readiness alone would
        // leave macOS binds failing with EADDRNOTAVAIL until the next reconnect.
        let mut observed = underlay_inventory_fixture();
        observed
            .underlay
            .host_ips
            .remove(&IpAddr::V4(Ipv4Addr::new(192, 168, 240, 44)));
        observed.underlay.ipv4_source_ip = Some(Ipv4Addr::new(10, 23, 188, 149));
        observed
            .underlay
            .host_ips
            .insert(IpAddr::V4(Ipv4Addr::new(10, 23, 188, 149)));

        assert!(watchdog_restart_request_for(Ok(Some(observed))).await);
    }

    #[tokio::test]
    async fn unreadable_underlay_inventory_does_not_request_a_rebuild() {
        assert!(!watchdog_restart_request_for(Ok(None)).await);
        assert!(
            !watchdog_restart_request_for(Err(anyhow::anyhow!("route inventory unavailable")))
                .await
        );
    }

    #[tokio::test]
    async fn superseded_generation_does_not_request_a_rebuild() {
        let expected = underlay_inventory_fixture();
        let (engine, stale_token, _stale_guard) = ready_native_alias_generation(&expected);
        let replacement_token = engine.begin_p2p_underlay_generation();
        let _replacement_guard =
            P2pUnderlayReadinessGuard::arm(Arc::clone(&engine), replacement_token).unwrap();
        let observer: UnderlayInventoryObserver = Arc::new(|| Ok(None));
        let restart = CancellationToken::new();

        tokio::time::timeout(
            Duration::from_secs(5),
            run_underlay_inventory_watchdog_with_observer(
                Arc::clone(&engine),
                stale_token,
                expected,
                CancellationToken::new(),
                Duration::ZERO,
                observer,
                restart.clone(),
            ),
        )
        .await
        .expect("a superseded watchdog must stop");

        assert!(
            !restart.is_cancelled(),
            "an old generation must not rebuild over its replacement"
        );
    }

    #[tokio::test]
    async fn superseded_underlay_generation_cannot_start_its_manager() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let stale_token = engine.begin_p2p_underlay_generation();
        let _replacement_token = engine.begin_p2p_underlay_generation();
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let (_in_tx, in_rx) = mpsc::channel(4);
        let manager = manager::P2pManager::new(
            make_multi(channel_session()),
            "peer-a-AbCd0001-0".into(),
            "group-test".into(),
            CertFingerprint::from_bytes([0x32; 32]),
            session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let error = spawn_manager_with_underlay_readiness(
            &engine,
            manager,
            stale_token,
            true,
            Some(underlay_inventory_fixture()),
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .expect_err("a superseded generation must fail before Manager spawn");

        assert!(error.to_string().contains("superseded"), "{error}");
        assert!(out_rx.try_recv().is_err(), "stale Manager emitted Announce");
        assert!(!engine.p2p_underlay_bypass_ready_for_test());
    }

    #[tokio::test]
    async fn spawned_manager_holds_underlay_readiness_until_it_exits() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let (in_tx, in_rx) = mpsc::channel(4);
        let manager = manager::P2pManager::new(
            make_multi(channel_session()),
            "peer-a-AbCd0001-0".into(),
            "group-test".into(),
            CertFingerprint::from_bytes([0x31; 32]),
            session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let token = engine.begin_p2p_underlay_generation();
        spawn_manager_with_underlay_readiness(
            &engine,
            manager,
            token,
            true,
            None,
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .unwrap();
        assert!(engine.p2p_underlay_bypass_ready_for_test());
        assert!(matches!(
            out_rx.recv().await,
            Some(BinaryMessage::P2pAnnounce { .. })
        ));

        drop(in_tx);
        tokio::time::timeout(Duration::from_millis(500), async {
            while engine.p2p_underlay_bypass_ready_for_test() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Manager exit must revoke underlay readiness");
    }

    #[test]
    fn underlay_interface_selection_uses_the_exact_non_tunnel_gateway_source() {
        let source = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 44));
        let candidates = vec![
            UnderlayInterfaceCandidate {
                name: "lo0".into(),
                ip: source,
                index: Some(1),
                loopback: true,
            },
            UnderlayInterfaceCandidate {
                name: "utun24".into(),
                ip: source,
                index: Some(24),
                loopback: false,
            },
            UnderlayInterfaceCandidate {
                name: "en0".into(),
                ip: source,
                index: Some(7),
                loopback: false,
            },
            UnderlayInterfaceCandidate {
                name: "en1".into(),
                ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8)),
                index: Some(9),
                loopback: false,
            },
        ];

        assert_eq!(
            select_underlay_interface_index(source, &candidates).unwrap(),
            std::num::NonZeroU32::new(7).unwrap()
        );
        assert!(select_underlay_interface_index(source, &candidates[..2]).is_err());
    }

    #[test]
    fn underlay_host_addresses_are_limited_to_the_selected_nic() {
        let source = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 44));
        let selected_v6 = "2001:db8:1::44".parse().unwrap();
        let other_nic = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8));
        let candidates = vec![
            UnderlayInterfaceCandidate {
                name: "en0".into(),
                ip: source,
                index: Some(7),
                loopback: false,
            },
            UnderlayInterfaceCandidate {
                name: "en0".into(),
                ip: selected_v6,
                index: Some(7),
                loopback: false,
            },
            UnderlayInterfaceCandidate {
                name: "en1".into(),
                ip: other_nic,
                index: Some(9),
                loopback: false,
            },
        ];

        let underlay = p2p_underlay_from_candidates(source, &candidates).unwrap();

        assert_eq!(underlay.interface_index().get(), 7);
        assert_eq!(
            underlay.host_ips(),
            &[source, selected_v6].into_iter().collect()
        );
        assert!(!underlay.host_ips().contains(&other_nic));
    }

    #[test]
    fn underlay_keeps_family_specific_indexes_for_one_physical_adapter() {
        let source = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 44));
        let selected_v6 = "2001:db8:1::44".parse().unwrap();
        let other_nic = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8));
        let candidates = vec![
            UnderlayInterfaceCandidate {
                name: "Wi-Fi".into(),
                ip: source,
                index: Some(7),
                loopback: false,
            },
            UnderlayInterfaceCandidate {
                name: "Wi-Fi".into(),
                ip: selected_v6,
                index: Some(70),
                loopback: false,
            },
            UnderlayInterfaceCandidate {
                name: "Ethernet".into(),
                ip: other_nic,
                index: Some(9),
                loopback: false,
            },
        ];

        let underlay = p2p_underlay_from_candidates(source, &candidates).unwrap();

        assert_eq!(underlay.ipv4_interface_index().unwrap().get(), 7);
        assert_eq!(underlay.ipv6_interface_index().unwrap().get(), 70);
        assert_eq!(
            underlay.host_ips(),
            &[source, selected_v6].into_iter().collect()
        );
        assert!(!underlay.host_ips().contains(&other_nic));
    }

    #[test]
    fn gateway_route_source_is_selected_by_the_kernel_route_table() {
        let gateway = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();

        assert_eq!(
            gateway_route_source_ip(gateway.local_addr().unwrap()).unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    #[ignore = "requires a host default route and indexed non-TUN interface"]
    fn current_host_default_route_resolves_to_a_safe_underlay_interface() {
        let underlay = discover_p2p_underlay("1.1.1.1:443".parse().unwrap())
            .expect("current default route should resolve to a safe interface");

        assert!(!underlay.host_ips().is_empty());
        assert_ne!(underlay.interface_index().get(), 0);
    }

    #[test]
    fn relay_anchor_ready_warning_allows_slow_platform_connects() {
        const {
            assert!(
                RELAY_ANCHOR_READY_WARN_SECS >= 60,
                "real platform config + gateway registration can exceed 30s"
            );
        }
    }

    #[test]
    fn underlay_bypass_warning_does_not_embed_discovery_error() {
        let source = include_str!("bootstrap.rs");
        let forbidden = ["error = %", "error"].concat();

        assert!(
            !source.contains(&forbidden),
            "underlay discovery errors can contain private network details"
        );
    }

    #[tokio::test]
    async fn relay_anchor_wait_continues_after_warning_until_context_ready() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let cancel = tokio_util::sync::CancellationToken::new();
        let engine_for_wait = engine.clone();
        let cancel_for_wait = cancel.clone();

        let wait = tokio::spawn(async move {
            wait_for_p2p_relay_context(
                &engine_for_wait,
                &cancel_for_wait,
                Duration::from_millis(10),
                Duration::from_millis(2),
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        engine.install_proxy_replica_session_for_test(
            "client-primary",
            make_multi(channel_session()),
        );

        let (client_id, group_id, _) = tokio::time::timeout(Duration::from_millis(500), wait)
            .await
            .expect("anchor wait should finish after relay appears")
            .expect("anchor wait task should not panic")
            .expect("anchor wait should return relay context");
        assert_eq!(client_id, "client-primary");
        assert_eq!(group_id, "group-test");
    }

    #[tokio::test]
    async fn bootstrap_exits_promptly_when_cancelled_while_waiting_for_relay_anchor() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let cancel = tokio_util::sync::CancellationToken::new();
        let bootstrap = tokio::spawn(run(engine, ClientP2pConfig::default(), cancel.clone()));

        tokio::time::sleep(Duration::from_millis(25)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_millis(500), bootstrap)
            .await
            .expect("bootstrap should exit promptly after cancellation")
            .expect("bootstrap task should not panic");
        assert!(
            result.is_err(),
            "cancelled bootstrap should return a cancellation error"
        );
    }

    #[tokio::test]
    async fn bootstrap_mesh_membership_wiring_keeps_ack_committed_routes_stable() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            replicas: 1,
            client_ids: vec!["mesh-ZLocal01-0".into()],
            ..TunnelConfig::default()
        });
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let (in_tx, in_rx) = mpsc::channel(8);
        let (peer_link_manager, tunnel_id) = mesh_membership_from_latest_tunnel_config(&engine)
            .expect("latest platform config supplies mesh identity");
        let mut manager = manager::P2pManager::new(
            make_multi(channel_session()),
            "mesh-ZLocal01-0".into(),
            "group-test".into(),
            CertFingerprint::from_bytes([1; 32]),
            session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        wire_mesh_membership(&engine, &mut manager, peer_link_manager, tunnel_id);

        let run_handle = tokio::spawn(manager.run());
        assert!(matches!(
            out_rx.recv().await,
            Some(BinaryMessage::P2pAnnounce { .. })
        ));
        for peer_client_id in ["mesh-RemoteB1-1", "mesh-RemoteC1-0", "mesh-RemoteB1-0"] {
            in_tx
                .send(BinaryMessage::P2pPeerHint {
                    peer_client_id: peer_client_id.into(),
                })
                .await
                .expect("send membership hint");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let peer_b_overlay = crate::overlay::overlay_ipv4_for_replica_id("mesh", "mesh-RemoteB1-0")
            .expect("stable Peer B overlay");
        assert_eq!(
            peer_b_overlay,
            crate::overlay::overlay_ipv4_for_replica_id("mesh", "mesh-RemoteB1-1")
                .expect("same-family Replica shares Peer overlay")
        );
        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{peer_b_overlay}:27015"))
                .expect("pre-Ack route lookup"),
            None,
            "Hint alone must not make a Peer routable"
        );
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 4433,
                server_time_ms: 1,
            })
            .await
            .expect("commit membership cycle");

        let peer_c_overlay = crate::overlay::overlay_ipv4_for_replica_id("mesh", "mesh-RemoteC1-0")
            .expect("stable Peer C overlay");
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let peer_b = engine
                    .resolve_overlay_peer(&format!("{peer_b_overlay}:27015"))
                    .expect("Peer B route lookup");
                let peer_c = engine
                    .resolve_overlay_peer(&format!("{peer_c_overlay}:27015"))
                    .expect("Peer C route lookup");
                if peer_b.is_some() && peer_c.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Ack should install the committed membership route");
        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{peer_b_overlay}:27015"))
                .expect("route lookup"),
            Some("mesh-RemoteB1-0".to_string())
        );
        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{peer_c_overlay}:27015"))
                .expect("route lookup"),
            Some("mesh-RemoteC1-0".to_string())
        );

        for peer_client_id in ["mesh-RemoteB1-0", "mesh-RemoteB1-1", "mesh-RemoteC1-0"] {
            in_tx
                .send(BinaryMessage::P2pPeerHint {
                    peer_client_id: peer_client_id.into(),
                })
                .await
                .expect("replay membership hint");
        }
        for server_time_ms in [2, 3] {
            in_tx
                .send(BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms,
                })
                .await
                .expect("commit replay or empty membership cycle");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{peer_b_overlay}:27015"))
                .expect("route retained after replay and soft absence"),
            Some("mesh-RemoteB1-0".to_string())
        );
        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{peer_c_overlay}:27015"))
                .expect("route retained after replay and soft absence"),
            Some("mesh-RemoteC1-0".to_string())
        );

        in_tx
            .send(BinaryMessage::P2pPeerHint {
                peer_client_id: "other-RemoteD1-0".into(),
            })
            .await
            .expect("send cross-Tunnel membership hint");
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 4433,
                server_time_ms: 4,
            })
            .await
            .expect("commit cross-Tunnel membership cycle");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let cross_tunnel_overlay =
            crate::overlay::overlay_ipv4_for_replica_id("other", "other-RemoteD1-0")
                .expect("valid overlay in its own Tunnel");
        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{cross_tunnel_overlay}:27015"))
                .expect("cross-Tunnel route lookup"),
            None,
            "a Replica outside the bootstrap Tunnel must not install a route"
        );

        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("manager shutdown")
            .expect("manager task");
    }

    #[tokio::test]
    async fn bootstrap_attaches_p2p_when_relay_has_multiple_replicas() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_anchor_client_id_for_test("peer-a-AbCd0001-0");
        engine.install_proxy_replica_session_for_test(
            "peer-a-AbCd0001-1",
            make_multi(channel_session()),
        );
        engine.install_proxy_replica_session_for_test(
            "peer-a-AbCd0001-0",
            make_multi(channel_session()),
        );
        engine.set_replicas_for_test(3);
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            replicas: 3,
            client_ids: (0..3)
                .map(|index| format!("peer-a-AbCd0001-{index}"))
                .collect(),
            ..TunnelConfig::default()
        });

        run(
            engine.clone(),
            ClientP2pConfig::default(),
            CancellationToken::new(),
        )
        .await
        .expect("P2P bootstrap should not skip replicas > 1");

        assert!(
            engine.p2p_expected_fp_handle().is_some(),
            "bootstrap must install P2P plumbing instead of returning early when replicas > 1"
        );
    }

    #[tokio::test]
    async fn bootstrap_rejects_invalid_platform_mesh_identity_instead_of_degrading() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test(
            "peer-a-AbCd0001-0",
            make_multi(channel_session()),
        );
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            replicas: 2,
            client_ids: vec!["peer-a-AbCd0001-0".into()],
            ..TunnelConfig::default()
        });

        let error = run(
            engine.clone(),
            ClientP2pConfig::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("invalid platform mesh identity must abort bootstrap");

        assert!(error.to_string().contains("replicas=2"), "{error}");
        assert!(
            engine.p2p_expected_fp_handle().is_none(),
            "failed mesh construction must happen before installing P2P plumbing"
        );
    }

    #[tokio::test]
    async fn lan_route_alias_bootstrap_fails_before_plumbing_without_safe_underlay_bypass() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test(
            "peer-a-AbCd0001-0",
            make_multi(channel_session()),
        );
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            replicas: 1,
            client_ids: vec!["peer-a-AbCd0001-0".into()],
            ..TunnelConfig::default()
        });
        let p2p_cfg = ClientP2pConfig {
            allow_lan_route_aliases: true,
            allow_lan_candidates: true,
            ..ClientP2pConfig::default()
        };

        let error = run(engine.clone(), p2p_cfg, CancellationToken::new())
            .await
            .expect_err("LAN Route Aliases require a non-TUN underlay interface");

        assert!(error.to_string().contains("underlay bypass"), "{error}");
        assert!(
            engine.p2p_expected_fp_handle().is_none(),
            "unsafe bootstrap must fail before installing P2P plumbing"
        );
        assert!(!engine.p2p_underlay_bypass_ready_for_test());
    }

    #[tokio::test]
    async fn alias_only_bootstrap_does_not_require_a_pinned_underlay() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test(
            "peer-a-AbCd0001-0",
            make_multi(channel_session()),
        );
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            replicas: 1,
            client_ids: vec!["peer-a-AbCd0001-0".into()],
            ..TunnelConfig::default()
        });
        let p2p_cfg = ClientP2pConfig {
            allow_lan_route_aliases: true,
            allow_lan_candidates: false,
            ..ClientP2pConfig::default()
        };

        run(engine.clone(), p2p_cfg, CancellationToken::new())
            .await
            .expect("alias-only mode has no recursive LAN Link Candidate to bypass");

        assert!(
            engine.p2p_expected_fp_handle().is_some(),
            "alias-only bootstrap must still install P2P plumbing"
        );
        assert!(!engine.p2p_underlay_bypass_ready_for_test());
    }

    #[test]
    fn mesh_manager_uses_latest_platform_replica_set_and_count() {
        use crate::peer_link_manager::{
            MembershipSnapshot, PeerDescriptor, PeerLinkCommand, RelationRole,
        };

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            replicas: 3,
            client_ids: (0..3)
                .map(|index| format!("peer-a-AbCd0001-{index}"))
                .collect(),
            ..TunnelConfig::default()
        });

        let (mut manager, _tunnel_id) = mesh_membership_from_latest_tunnel_config(&engine)
            .expect("valid platform identity installs mesh manager");
        let remote_peer = PeerDescriptor::from_replica_ids(
            (0..3)
                .map(|index| format!("peer-b-AbCd0002-{index}"))
                .collect(),
        )
        .expect("valid remote Peer");
        let work = manager.apply_snapshot(&MembershipSnapshot::new(vec![remote_peer]));

        let lanes = work
            .into_iter()
            .map(|command| match command {
                PeerLinkCommand::EnsureLane(lane) => (
                    lane.index(),
                    lane.local_replica_id().to_string(),
                    lane.local_role(),
                ),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lanes,
            vec![
                (0, "peer-a-AbCd0001-0".into(), RelationRole::Initiator),
                (1, "peer-a-AbCd0001-1".into(), RelationRole::Initiator),
                (2, "peer-a-AbCd0001-2".into(), RelationRole::Initiator),
            ]
        );
    }

    #[test]
    fn mesh_manager_rejects_platform_replica_count_mismatch() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            replicas: 3,
            client_ids: vec!["peer-a-AbCd0001-0".into(), "peer-a-AbCd0001-1".into()],
            ..TunnelConfig::default()
        });

        let error = mesh_membership_from_latest_tunnel_config(&engine)
            .expect_err("partial local Replica set must fail mesh bootstrap");
        assert!(
            error.to_string().contains("replicas=3") && error.to_string().contains("client_ids=2"),
            "unexpected error: {error}"
        );
    }
}
