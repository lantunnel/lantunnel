//! SOCKS5 (RFC 1928 + RFC 1929) CONNECT + UDP ASSOCIATE frontend.
//!
//! Used by the desktop client and the mobile FFI as the local loopback SOCKS5
//! listener. The Gateway-side public listener is not wired up.
//!
//! ## Architecture — shared UDP listener
//!
//! One UDP listener is bound at the same port as the TCP SOCKS5 port during
//! startup. That single address is advertised to every UDP ASSOCIATE session,
//! and incoming packets are demultiplexed back to the right tunnel by source
//! address: one shared reader task plus a `DashMap<SocketAddr, Session>`.
//!
//! A fixed port is required rather than a fresh ephemeral port per ASSOCIATE,
//! because firewall rules typically open only the configured SOCKS5 port and
//! would drop everything bound elsewhere.
//!
//! ## Fragment reassembly
//!
//! RFC 1928 §7 fragmentation is supported via a per-session
//! [`Socks5FragAssembler`]: standalone packets (`FRAG=0x00`) pass through
//! untouched; fragmented sequences (`0x01..=0x7F` / `0x81..=0xFF`) are
//! buffered until the final-fragment marker (`0x80` bit) arrives.

pub mod backend;

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::{BufMut, Bytes, BytesMut};
use dashmap::{mapref::entry::Entry, DashMap};
use parking_lot::{Mutex, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::oneshot;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tp_transport::{
    bind_tuned_udp, drop_oldest_channel, DropOldestReceiver, DropOldestSender, TrySendKind,
};

const TARGET_FORWARDER_CAP: usize = 2048;
const UDP_READER_DRAIN_BATCH: usize = 256;

const VER: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_USER_PASS: u8 = 0x02;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;
const USER_PASS_VER: u8 = 0x01;

const CMD_CONNECT: u8 = 0x01;
const CMD_UDP: u8 = 0x03;

const ATYP_V4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_V6: u8 = 0x04;

const REP_OK: u8 = 0x00;
const REP_GENERAL_FAIL: u8 = 0x01;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;
const REP_ATYPE_NOT_SUPPORTED: u8 = 0x08;
const MAX_SOCKS5_SESSIONS: usize = 4096;

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Validator for `(username, password)`. Both are opaque to this crate, and
/// the return value is the route key handed to the backend. Unused by the
/// shipped binaries, which all run `AuthMode::NoAuth` on a loopback listener.
pub type AuthValidator = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub enum AuthMode {
    UserPass(AuthValidator),
    NoAuth { group_id: String },
}

impl From<AuthValidator> for AuthMode {
    fn from(auth: AuthValidator) -> Self {
        Self::UserPass(auth)
    }
}

impl AuthMode {
    fn label(&self) -> &'static str {
        match self {
            Self::UserPass(_) => "USER/PASS",
            Self::NoAuth { .. } => "NO AUTH",
        }
    }
}

/// Per-session state tracked by the shared UDP demultiplexer. One of these
/// is created per TCP ASSOCIATE control connection. The outer session map
/// keys them first by `tcp_peer_addr` (pending, unique per ASSOC) and then
/// by the observed clash UDP source address once the first packet arrives.
///
/// S1 refactor: a Session no longer owns a single tunnel; instead each
/// (session, target) lazily opens its own UDP tunnel through the configured
/// backend. The gateway backend preserves the previous round-robin replica
/// behavior; local clients can provide an engine-backed implementation.
struct Session {
    /// Tunnel route key used to round-robin a replica at lazy tunnel open.
    group_id: String,
    /// Backend handle for lazy per-target tunnel opens.
    backend: Arc<dyn backend::Socks5Backend>,
    udp_sock: Arc<UdpSocket>,
    clash_addr: RwLock<Option<SocketAddr>>,
    target_clients: DashMap<String, SocketAddr>,
    frag_asm: Mutex<Socks5FragAssembler>,
    closed: AtomicBool,
    diag_declared_source: AtomicBool,
    diag_source_mismatch: AtomicU64,
    /// Per-target bounded drop-oldest ring, keyed on "host:port". While the
    /// async tunnel is opening or the tunnel writer is busy, newer UDP
    /// replaces older UDP instead of building latency.
    target_forwarders: DashMap<String, DropOldestSender<Bytes>>,
}

/// Shared proxy state — held by every task for the lifetime of `serve()`.
///
/// `pending_by_tcp` is keyed by the full TCP peer `SocketAddr` (IP:port) so
/// N concurrent UDP ASSOCs from the same client IP don't overwrite each
/// other — every TCP connection has a unique ephemeral source port, so the
/// index is keyed by the TCP client address. The earlier
/// `DashMap<IpAddr, _>` design silently dropped all but the last-registered
/// session, causing ~11/12 loss on the multi-stream stress test.
struct ProxyState {
    udp_sock: Arc<UdpSocket>,
    bind_addr: SocketAddr,
    sessions: DashMap<SocketAddr, Arc<Session>>,
    pending_by_tcp: DashMap<SocketAddr, Arc<Session>>,
}

pub async fn serve_with_backend(
    listen_addr: SocketAddr,
    backend: Arc<dyn backend::Socks5Backend>,
    auth: AuthValidator,
) -> anyhow::Result<()> {
    serve_with_backend_ready(listen_addr, backend, auth, None).await
}

pub async fn serve_with_backend_ready(
    listen_addr: SocketAddr,
    backend: Arc<dyn backend::Socks5Backend>,
    auth: AuthValidator,
    ready: Option<oneshot::Sender<SocketAddr>>,
) -> anyhow::Result<()> {
    serve_with_backend_auth_mode_ready(listen_addr, backend, auth.into(), ready).await
}

pub async fn serve_with_backend_auth_mode_ready(
    listen_addr: SocketAddr,
    backend: Arc<dyn backend::Socks5Backend>,
    auth: AuthMode,
    ready: Option<oneshot::Sender<SocketAddr>>,
) -> anyhow::Result<()> {
    let tcp_listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("socks5 tcp bind {listen_addr}"))?;
    let tcp_bind_addr = tcp_listener.local_addr().context("socks5 tcp local_addr")?;

    // Shared UDP listener at the SAME port as TCP, bound on the socks5 listen
    // addr. SO_RCVBUF/SO_SNDBUF tuned
    // to 8 MiB via `bind_tuned_udp` so bursty 4K video doesn't overflow the
    // kernel receive buffer during scheduler hiccups.
    let udp_listen_addr = if listen_addr.port() == 0 {
        tcp_bind_addr
    } else {
        listen_addr
    };
    let std_udp = bind_tuned_udp(udp_listen_addr)
        .with_context(|| format!("socks5 udp bind {udp_listen_addr}"))?;
    let udp_sock = Arc::new(UdpSocket::from_std(std_udp).context("socks5 udp: tokio::from_std")?);
    let bind_addr = udp_sock.local_addr().context("socks5 udp local_addr")?;

    let state = Arc::new(ProxyState {
        udp_sock: udp_sock.clone(),
        bind_addr,
        sessions: DashMap::new(),
        pending_by_tcp: DashMap::new(),
    });

    // Shared UDP reader task — single `recv_from` loop that demultiplexes
    // to the right session by clash source address.
    //
    // REVERTED: a fan-out of N parallel readers was tried here and caused a
    // multi-stream regression (~8.33% = 1/12 loss at 12-stream stress, exact
    // match for first-packet races in the `pending_by_tcp` adoption scan)
    // plus intermittent SOCKS5 UDP test failures. Single-reader is the
    // proven baseline and matches the memory-file invariant.
    let _udp_reader = {
        let state = state.clone();
        AbortOnDrop(tokio::spawn(async move {
            udp_reader_loop(state).await;
        }))
    };

    tracing::info!(tcp = %listen_addr, udp = %bind_addr, auth = auth.label(),
        "socks5 listening (shared UDP listener)");
    if let Some(ready) = ready {
        let _ = ready.send(bind_addr);
    }
    let permits = Arc::new(Semaphore::new(MAX_SOCKS5_SESSIONS));

    loop {
        let (socket, peer) = match tcp_listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "socks5 tcp accept error");
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            backend.increment_listener_rejects();
            tracing::warn!(
                peer = %peer,
                max_sessions = MAX_SOCKS5_SESSIONS,
                "socks5 tcp session limit reached; rejecting"
            );
            drop(socket);
            continue;
        };
        let backend = backend.clone();
        let auth = auth.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle(socket, backend, auth, state).await {
                tracing::debug!(peer = %peer, error = %e, "socks5 session ended");
            }
        });
    }
}

async fn handle(
    mut sock: TcpStream,
    backend: Arc<dyn backend::Socks5Backend>,
    auth: AuthMode,
    state: Arc<ProxyState>,
) -> anyhow::Result<()> {
    // Bound the handshake prefix so a slowloris peer that completes TCP
    // handshake and then drips / stalls cannot pin a handler task (+ fd +
    // stack) indefinitely. 10 s is comfortable for any real client and
    // bounds the worst case an attacker can cost the gateway per
    // connection. The pipe / UDP-associate phases below the `match cmd`
    // are intentionally NOT covered — those are long-lived proxy flows.
    let handshake = async {
        let mut hdr = [0u8; 2];
        sock.read_exact(&mut hdr).await?;
        if hdr[0] != VER {
            anyhow::bail!("bad socks5 version");
        }
        let mut methods = vec![0u8; hdr[1] as usize];
        sock.read_exact(&mut methods).await?;

        let group = match &auth {
            AuthMode::UserPass(validator) => {
                if !methods.contains(&METHOD_USER_PASS) {
                    sock.write_all(&[VER, METHOD_NO_ACCEPTABLE]).await.ok();
                    anyhow::bail!("client did not offer USER/PASS auth");
                }
                sock.write_all(&[VER, METHOD_USER_PASS]).await?;
                match run_user_pass(&mut sock, validator).await? {
                    Some(g) => g,
                    None => anyhow::bail!("user/pass auth failed"),
                }
            }
            AuthMode::NoAuth { group_id } => {
                if !methods.contains(&METHOD_NO_AUTH) {
                    sock.write_all(&[VER, METHOD_NO_ACCEPTABLE]).await.ok();
                    anyhow::bail!("client did not offer NO AUTH");
                }
                sock.write_all(&[VER, METHOD_NO_AUTH]).await?;
                group_id.clone()
            }
        };

        let mut rhdr = [0u8; 4];
        sock.read_exact(&mut rhdr).await?;
        if rhdr[0] != VER {
            anyhow::bail!("bad version on request");
        }
        let cmd = rhdr[1];
        let atyp = rhdr[3];

        let host = match atyp {
            ATYP_V4 => {
                let mut b = [0u8; 4];
                sock.read_exact(&mut b).await?;
                IpAddr::V4(Ipv4Addr::from(b)).to_string()
            }
            ATYP_V6 => {
                let mut b = [0u8; 16];
                sock.read_exact(&mut b).await?;
                IpAddr::V6(Ipv6Addr::from(b)).to_string()
            }
            ATYP_DOMAIN => {
                let mut len_b = [0u8; 1];
                sock.read_exact(&mut len_b).await?;
                let mut name = vec![0u8; len_b[0] as usize];
                sock.read_exact(&mut name).await?;
                String::from_utf8(name)?
            }
            _ => {
                write_reply(&mut sock, REP_ATYPE_NOT_SUPPORTED).await.ok();
                anyhow::bail!("unsupported ATYP");
            }
        };
        let mut port_b = [0u8; 2];
        sock.read_exact(&mut port_b).await?;
        let port = u16::from_be_bytes(port_b);
        Ok::<_, anyhow::Error>((group, cmd, host, port))
    };
    let (group, cmd, host, port) =
        match tokio::time::timeout(Duration::from_secs(10), handshake).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!("socks5 handshake timed out"),
        };

    match cmd {
        CMD_CONNECT => {
            let target = format!("{host}:{port}");
            tracing::debug!(%group, %target, "socks5 CONNECT");
            match backend.open_tcp(&group, &target).await {
                Ok(tunnel) => {
                    write_reply(&mut sock, REP_OK).await?;
                    pipe(sock, tunnel).await
                }
                Err(e) => {
                    tracing::warn!(%group, %target, error = %e, "socks5 CONNECT failed");
                    write_reply(&mut sock, REP_GENERAL_FAIL).await.ok();
                    Err(e)
                }
            }
        }
        CMD_UDP => {
            tracing::debug!(%group, "socks5 UDP ASSOCIATE");
            handle_udp_associate(sock, backend, &group, &host, port, state).await
        }
        _ => {
            write_reply(&mut sock, REP_CMD_NOT_SUPPORTED).await?;
            anyhow::bail!("unsupported SOCKS5 command");
        }
    }
}

async fn run_user_pass(
    sock: &mut TcpStream,
    validator: &AuthValidator,
) -> anyhow::Result<Option<String>> {
    let mut ver = [0u8; 1];
    sock.read_exact(&mut ver).await?;
    if ver[0] != USER_PASS_VER {
        sock.write_all(&[USER_PASS_VER, 0x01]).await.ok();
        return Ok(None);
    }
    let mut ulen = [0u8; 1];
    sock.read_exact(&mut ulen).await?;
    let mut uname = vec![0u8; ulen[0] as usize];
    sock.read_exact(&mut uname).await?;
    let mut plen = [0u8; 1];
    sock.read_exact(&mut plen).await?;
    let mut pwd = vec![0u8; plen[0] as usize];
    sock.read_exact(&mut pwd).await?;
    let u = String::from_utf8(uname).unwrap_or_default();
    let p = String::from_utf8(pwd).unwrap_or_default();
    match (validator)(&u, &p) {
        Some(tunnel_id) => {
            tracing::debug!(user = %u, "socks5 USER/PASS auth accepted");
            sock.write_all(&[USER_PASS_VER, 0x00]).await?;
            Ok(Some(tunnel_id))
        }
        None => {
            tracing::debug!(user = %u, "socks5 USER/PASS auth rejected");
            sock.write_all(&[USER_PASS_VER, 0x01]).await?;
            Ok(None)
        }
    }
}

async fn write_reply(sock: &mut TcpStream, rep: u8) -> anyhow::Result<()> {
    let buf = [VER, rep, 0x00, ATYP_V4, 0, 0, 0, 0, 0, 0];
    sock.write_all(&buf).await?;
    Ok(())
}

async fn write_udp_reply(sock: &mut TcpStream, bind: SocketAddr) -> anyhow::Result<()> {
    let mut buf: Vec<u8> = vec![VER, REP_OK, 0x00];
    match bind.ip() {
        IpAddr::V4(v4) => {
            buf.push(ATYP_V4);
            buf.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            buf.push(ATYP_V6);
            buf.extend_from_slice(&v6.octets());
        }
    }
    buf.extend_from_slice(&bind.port().to_be_bytes());
    sock.write_all(&buf).await?;
    Ok(())
}

/// Handle a UDP ASSOCIATE control channel. Creates the session, registers
/// it in shared state, and keeps the TCP stream alive until clash closes
/// it (per RFC 1928 §6, this signals teardown of the UDP mapping).
///
/// S1: no tunnel is opened here — tunnels are opened lazily in
/// [`target_forwarder_task`] on first-packet-per-target so that each
/// target gets its own round-robin replica selection.
async fn handle_udp_associate(
    mut tcp: TcpStream,
    backend: Arc<dyn backend::Socks5Backend>,
    group_id: &str,
    requested_host: &str,
    requested_port: u16,
    state: Arc<ProxyState>,
) -> anyhow::Result<()> {
    let session = Arc::new(Session {
        group_id: group_id.to_string(),
        backend,
        udp_sock: state.udp_sock.clone(),
        clash_addr: RwLock::new(None),
        target_clients: DashMap::new(),
        frag_asm: Mutex::new(Socks5FragAssembler::new()),
        closed: AtomicBool::new(false),
        diag_declared_source: AtomicBool::new(false),
        diag_source_mismatch: AtomicU64::new(0),
        target_forwarders: DashMap::new(),
    });

    // 1. When the client declares the UDP source endpoint from RFC 1928 §6,
    //    bind this control channel directly to that source before ACK. Clients
    //    that send 0.0.0.0:0 retain the pending first-datagram adoption path.
    let tcp_peer_addr = tcp.peer_addr()?;
    if let Some(source) = declared_udp_source(requested_host, requested_port, tcp_peer_addr)? {
        session.diag_declared_source.store(true, Ordering::Relaxed);
        match state.sessions.entry(source) {
            Entry::Vacant(entry) => {
                *session.clash_addr.write() = Some(source);
                entry.insert(session.clone());
            }
            Entry::Occupied(_) => anyhow::bail!("SOCKS5 UDP source is already associated"),
        }
    } else {
        // The TCP peer's full address is unique per connection. The first UDP
        // datagram from the same IP adopts one pending entry and migrates it
        // into `sessions` keyed by the observed UDP source (IP:port).
        state.pending_by_tcp.insert(tcp_peer_addr, session.clone());
    }

    // 2. Only advertise the shared UDP bind address after the pending
    //    session is visible. A client may send its first UDP datagram as
    //    soon as it reads this reply.
    if let Err(error) = write_udp_reply(&mut tcp, state.bind_addr).await {
        session.closed.store(true, Ordering::SeqCst);
        remove_udp_session_mappings(&state, tcp_peer_addr, &session);
        return Err(error);
    }

    // 3. TCP watchdog — block until clash closes the control channel.
    let mut b = [0u8; 1];
    let _ = tcp.read(&mut b).await;

    // Close sequence: mark closed, then drop all target forwarder senders
    // so their tasks see `rx.recv() == None` and exit (which aborts their
    // per-target reply pump).
    session.closed.store(true, Ordering::SeqCst);
    session.target_forwarders.clear();
    session.target_clients.clear();

    remove_udp_session_mappings(&state, tcp_peer_addr, &session);
    Ok(())
}

fn declared_udp_source(
    requested_host: &str,
    requested_port: u16,
    tcp_peer_addr: SocketAddr,
) -> anyhow::Result<Option<SocketAddr>> {
    let requested_ip = requested_host
        .parse::<IpAddr>()
        .map_err(|_| anyhow::anyhow!("SOCKS5 UDP source must be an IP address"))?;
    if requested_port == 0 {
        if requested_ip.is_unspecified() {
            return Ok(None);
        }
        anyhow::bail!("SOCKS5 UDP zero port requires an unspecified source address");
    }
    let source_ip = if requested_ip.is_unspecified() {
        tcp_peer_addr.ip()
    } else {
        requested_ip
    };
    if source_ip != tcp_peer_addr.ip() {
        anyhow::bail!("SOCKS5 UDP source IP does not match the TCP peer");
    }
    Ok(Some(SocketAddr::new(source_ip, requested_port)))
}

fn remove_udp_session_mappings(
    state: &Arc<ProxyState>,
    tcp_peer_addr: SocketAddr,
    session: &Arc<Session>,
) {
    state
        .pending_by_tcp
        .remove_if(&tcp_peer_addr, |_, current| Arc::ptr_eq(current, session));
    let session_keys: Vec<SocketAddr> = state
        .sessions
        .iter()
        .filter(|entry| Arc::ptr_eq(entry.value(), session))
        .map(|entry| *entry.key())
        .collect();
    for addr in session_keys {
        state
            .sessions
            .remove_if(&addr, |_, current| Arc::ptr_eq(current, session));
    }
}

/// Shared demultiplexer for all UDP datagrams arriving at the proxy's bind
/// address. Parses the RFC 1928 §7 header and forwards via payload-only sends so
/// a stalled tunnel for one session never blocks the reader that services
/// every other session.
async fn udp_reader_loop(state: Arc<ProxyState>) {
    const UDP_ARENA_BYTES: usize = 2 * 1024 * 1024;
    const UDP_ARENA_LOW_WATER: usize = 64 * 1024;
    let mut arena = BytesMut::with_capacity(UDP_ARENA_BYTES);
    loop {
        if arena.capacity() - arena.len() < UDP_ARENA_LOW_WATER {
            arena.reserve(UDP_ARENA_BYTES);
        }
        let (n, src) = match state.udp_sock.recv_buf_from(&mut arena).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "socks5 udp recv_from error");
                break;
            }
        };
        let packet = arena.split_to(n).freeze();

        process_udp_packet(&state, src, packet);

        for _ in 0..UDP_READER_DRAIN_BATCH {
            if arena.capacity() - arena.len() < UDP_ARENA_LOW_WATER {
                arena.reserve(UDP_ARENA_BYTES);
            }
            let (n, src) = match state.udp_sock.try_recv_buf_from(&mut arena) {
                Ok(v) => v,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!(error = %e, "socks5 udp try_recv_from error");
                    break;
                }
            };
            let packet = arena.split_to(n).freeze();
            process_udp_packet(&state, src, packet);
        }
    }
}

fn process_udp_packet(state: &Arc<ProxyState>, src: SocketAddr, packet: Bytes) {
    let Some(session) = resolve_udp_session(state, src) else {
        return;
    };
    if session.closed.load(Ordering::SeqCst) {
        return;
    }
    if session
        .clash_addr
        .read()
        .is_some_and(|declared| declared != src)
    {
        session.diag_source_mismatch.fetch_add(1, Ordering::Relaxed);
    }

    let Some((frag, target, payload)) = parse_udp_request_bytes(packet) else {
        return;
    };

    let forward = if frag == 0 {
        Some((target, payload))
    } else {
        session.frag_asm.lock().accept(src, frag, target, payload)
    };
    let Some((tgt, full)) = forward else { return };
    session.target_clients.insert(tgt.clone(), src);

    // Per-target forwarder: lookup-or-spawn the task owning this
    // target's tunnel, then non-blocking-send the payload. Each target
    // gets its own round-robin-picked replica, so a Moonlight session
    // with 3–4 target ports spreads across up to N replica QUIC
    // connections rather than serialising on one.
    let tx = session
        .target_forwarders
        .entry(tgt.clone())
        .or_insert_with(|| {
            let (tx, rx) = drop_oldest_channel::<Bytes>(TARGET_FORWARDER_CAP);
            let session_for_task = session.clone();
            let target_for_task = tgt.clone();
            tokio::spawn(async move {
                target_forwarder_task(target_for_task, rx, session_for_task).await;
            });
            tx
        })
        .clone();

    if tx.is_closed() {
        // Task exited (tunnel errored or session closed). Remove the stale
        // entry so the next packet re-opens cleanly.
        session.target_forwarders.remove(&tgt);
    } else if tx.send_drop_oldest(full).unwrap_or(false) {
        session.backend.increment_udp_drops();
    }
}

fn resolve_udp_session(state: &Arc<ProxyState>, src: SocketAddr) -> Option<Arc<Session>> {
    if let Some(existing) = state.sessions.get(&src).map(|r| r.clone()) {
        return Some(existing);
    }

    // First packet for a UDP ASSOCIATE: adopt one pending TCP control
    // session from the same client IP and cache this observed UDP source.
    let candidate_key = state
        .pending_by_tcp
        .iter()
        .find(|e| e.key().ip() == src.ip())
        .map(|e| *e.key());
    if let Some((_, session)) = candidate_key.and_then(|k| state.pending_by_tcp.remove(&k)) {
        return register_udp_source(state, &session, src).then_some(session);
    }

    // Some clients/tun stacks keep one UDP ASSOCIATE control channel but emit
    // multiple UDP source ports under it. If there is exactly one active ASSOC
    // for this client IP, attach the new source to that session. If there are
    // multiple active sessions for the same IP, the source is ambiguous; drop
    // rather than risk cross-session routing.
    let mut candidates: Vec<Arc<Session>> = Vec::new();
    for entry in state.sessions.iter().filter(|e| e.key().ip() == src.ip()) {
        let session = entry.value().clone();
        if !candidates
            .iter()
            .any(|existing| Arc::ptr_eq(existing, &session))
        {
            candidates.push(session);
        }
    }
    if candidates.len() == 1 {
        let session = candidates.pop().expect("one candidate");
        return register_udp_source(state, &session, src).then_some(session);
    }

    None
}

fn register_udp_source(state: &Arc<ProxyState>, session: &Arc<Session>, src: SocketAddr) -> bool {
    if session.closed.load(Ordering::SeqCst) {
        return false;
    }
    {
        let mut first = session.clash_addr.write();
        if session.closed.load(Ordering::SeqCst) {
            return false;
        }
        if first.is_none() {
            *first = Some(src);
        }
    }
    state.sessions.insert(src, session.clone());
    if session.closed.load(Ordering::SeqCst) {
        state
            .sessions
            .remove_if(&src, |_, current| Arc::ptr_eq(current, session));
        return false;
    }
    true
}

/// Per-(session, target) task. Lazily opens a UDP tunnel via the configured
/// backend, spawns the reply pump consuming the tunnel's inbound datagrams,
/// then forwards pending packets via `tunnel_sender`.
///
/// Exits and removes its entry from `session.target_forwarders` when:
/// - the drop-oldest sender is dropped (session close clears the map),
/// - the tunnel errors (closed by the gateway or client side), or
/// - the session's `closed` flag is observed true.
async fn target_forwarder_task(
    target: String,
    mut rx: DropOldestReceiver<Bytes>,
    session: Arc<Session>,
) {
    let tunnel = match session.backend.open_udp(&session.group_id, &target).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                %target, group_id = %session.group_id, error = %e,
                "socks5 udp: open_udp_tunnel failed; target dropped"
            );
            session.target_forwarders.remove(&target);
            return;
        }
    };
    let (tunnel_sender, tunnel_recv) = tunnel.split();
    let pump = {
        let session = session.clone();
        let target = target.clone();
        tokio::spawn(async move { target_reply_pump(session, target, tunnel_recv).await })
    };

    let mut dropped: u64 = 0;
    while let Some(payload) = rx.recv().await {
        if session.closed.load(Ordering::SeqCst) {
            break;
        }
        if forward_one(tunnel_sender.as_ref(), &target, payload, &mut dropped).is_err() {
            break;
        }
    }

    pump.abort();
    session.target_forwarders.remove(&target);
}

/// Non-blocking send to the tunnel. Returns `Err(())` if the tunnel is
/// closed (caller should exit the forwarder loop); `Ok(())` for both
/// success and "queue full — dropped". Mutates the shared drop counter for
/// power-of-two-sampled logging.
fn forward_one(
    tunnel_sender: &dyn backend::UdpTunnelSender,
    target: &str,
    payload: Bytes,
    dropped: &mut u64,
) -> Result<(), ()> {
    match tunnel_sender.try_send(payload) {
        Ok(()) => Ok(()),
        Err(TrySendKind::Full) => {
            *dropped = dropped.wrapping_add(1);
            if dropped.is_power_of_two() {
                tracing::debug!(
                    %target, dropped = *dropped,
                    "socks5 udp: tunnel datagram queue full; dropped"
                );
            }
            Ok(())
        }
        Err(TrySendKind::TooLarge(len)) => {
            tracing::warn!(%target, len, "socks5 udp: oversized tunnel frame rejected");
            Ok(())
        }
        Err(TrySendKind::DatagramUnavailable) => {
            tracing::warn!(%target, "socks5 udp: datagram transport unavailable");
            Err(())
        }
        Err(TrySendKind::Closed) => Err(()),
    }
}

/// Per-target reply pump — same semantics as the pre-S1 session pump, but
/// scoped to a single target's tunnel so each target's replies flow
/// independently.
///
/// Emits `socks5 reply pump stats` every 10 s so we can compare SOCKS5's
/// gateway→clash path with TUIC's `tuic outbound stats (assoc)`:
///   - `recv_count` / `bytes_recv`: what arrived from the tunnel udp_inbound.
///   - `send_ok` / `bytes_sent` / `send_err`: what actually made it out
///     of the gateway's shared UDP socket.
///   - `send_would_block` / wait micros: how often the non-blocking socket
///     send path had to fall back to awaited `send_to`.
///   - `no_clash_addr`: arrived before first-packet adoption happened
///     (shouldn't be more than a handful per session).
async fn target_reply_pump(
    session: Arc<Session>,
    target: String,
    mut recv: backend::BoxUdpTunnelReceiver,
) {
    let mut recv_count: u64 = 0;
    let mut bytes_recv: u64 = 0;
    let mut send_ok: u64 = 0;
    let mut bytes_sent: u64 = 0;
    let mut send_err: u64 = 0;
    let mut send_would_block: u64 = 0;
    let mut send_would_block_wait_us: u64 = 0;
    let mut send_would_block_wait_max_us: u64 = 0;
    let mut no_clash_addr: u64 = 0;
    let mut last_log = std::time::Instant::now();
    let log_every = Duration::from_secs(10);
    let conn_id_owned = recv.conn_id().to_string();
    let group = session.group_id.clone();

    // Reusable output buffer. At ~2500 pps moonlight video this eliminates
    // the per-packet `Vec::with_capacity` allocation that was burning
    // ~200 ns/packet = ~500 µs/s of CPU just on the reply path.
    let mut reply_buf = BytesMut::with_capacity(64 * 1024);
    // Cap on how many queued packets we drain per wake-up. Higher values
    // reduce the number of scheduler round-trips at high pps but extend
    // the latency budget for the oldest packet in a batch. 32 ≈ 12 ms of
    // video frames at 2500 pps — well under moonlight's per-frame deadline.
    const DRAIN_BATCH: usize = 32;

    // Handler closure: processes one payload by writing into
    // `reply_buf` and sending via the shared UDP socket.
    //
    // We can't use a closure capturing &mut on `reply_buf` across an await
    // boundary cleanly, so inline the logic. Side effects on the surrounding
    // counters via the captured `&mut` references.
    loop {
        // Block-wait for the first packet of the next burst.
        let Some(payload) = recv.recv().await else {
            break;
        };
        if session.closed.load(Ordering::SeqCst) {
            break;
        }
        // Process the first packet. NB: we read `session.clash_addr` into
        // a local Copy (SocketAddr is Copy) BEFORE any await — holding the
        // parking_lot guard across await would make the future !Send.
        {
            recv_count = recv_count.saturating_add(1);
            bytes_recv = bytes_recv.saturating_add(payload.len() as u64);
            let dst_opt = reply_destination_for_target(&session, &target);
            if let Some(dst) = dst_opt {
                reply_buf.clear();
                write_udp_reply_into(&mut reply_buf, &target, &payload);
                let send_res = match session.udp_sock.try_send_to(&reply_buf[..], dst) {
                    Ok(n) => Ok(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        send_would_block = send_would_block.saturating_add(1);
                        let wait_start = Instant::now();
                        let send_res = session.udp_sock.send_to(&reply_buf[..], dst).await;
                        let wait_us = wait_start.elapsed().as_micros() as u64;
                        send_would_block_wait_us = send_would_block_wait_us.saturating_add(wait_us);
                        send_would_block_wait_max_us = send_would_block_wait_max_us.max(wait_us);
                        send_res
                    }
                    Err(e) => Err(e),
                };
                match send_res {
                    Ok(n) => {
                        send_ok = send_ok.saturating_add(1);
                        bytes_sent = bytes_sent.saturating_add(n as u64);
                    }
                    Err(e) => {
                        send_err = send_err.saturating_add(1);
                        if send_err.is_power_of_two() {
                            tracing::debug!(
                                error = %e, send_err, conn_id = %conn_id_owned,
                                "socks5 reply pump: udp send_to failed"
                            );
                        }
                    }
                }
            } else {
                no_clash_addr = no_clash_addr.saturating_add(1);
            }
        }

        // Drain up to DRAIN_BATCH already-queued packets without yielding
        // so we amortize one scheduler wake-up across multiple sends.
        let mut drained = 0usize;
        while drained < DRAIN_BATCH {
            match recv.try_recv() {
                Ok(payload) => {
                    drained += 1;
                    recv_count = recv_count.saturating_add(1);
                    bytes_recv = bytes_recv.saturating_add(payload.len() as u64);
                    if session.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    let dst_opt = reply_destination_for_target(&session, &target);
                    if let Some(dst) = dst_opt {
                        reply_buf.clear();
                        write_udp_reply_into(&mut reply_buf, &target, &payload);
                        let send_res = match session.udp_sock.try_send_to(&reply_buf[..], dst) {
                            Ok(n) => Ok(n),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                send_would_block = send_would_block.saturating_add(1);
                                let wait_start = Instant::now();
                                let send_res = session.udp_sock.send_to(&reply_buf[..], dst).await;
                                let wait_us = wait_start.elapsed().as_micros() as u64;
                                send_would_block_wait_us =
                                    send_would_block_wait_us.saturating_add(wait_us);
                                send_would_block_wait_max_us =
                                    send_would_block_wait_max_us.max(wait_us);
                                send_res
                            }
                            Err(e) => Err(e),
                        };
                        match send_res {
                            Ok(n) => {
                                send_ok = send_ok.saturating_add(1);
                                bytes_sent = bytes_sent.saturating_add(n as u64);
                            }
                            Err(e) => {
                                send_err = send_err.saturating_add(1);
                                if send_err.is_power_of_two() {
                                    tracing::debug!(
                                        error = %e, send_err, conn_id = %conn_id_owned,
                                        "socks5 reply pump: udp send_to failed"
                                    );
                                }
                            }
                        }
                    } else {
                        no_clash_addr = no_clash_addr.saturating_add(1);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if last_log.elapsed() >= log_every {
            tracing::info!(
                %group,
                conn_id = %conn_id_owned,
                recv_count,
                bytes_recv,
                send_ok,
                bytes_sent,
                send_err,
                send_would_block,
                send_would_block_wait_us,
                send_would_block_wait_max_us,
                no_clash_addr,
                diag_udp_declared_source = session.diag_declared_source.load(Ordering::Relaxed),
                diag_udp_source_mismatch = session.diag_source_mismatch.load(Ordering::Relaxed),
                "socks5 reply pump stats"
            );
            last_log = std::time::Instant::now();
        }
    }
    tracing::info!(
        %group,
        conn_id = %conn_id_owned,
        recv_count,
        bytes_recv,
        send_ok,
        bytes_sent,
        send_err,
        send_would_block,
        send_would_block_wait_us,
        send_would_block_wait_max_us,
        no_clash_addr,
        diag_udp_declared_source = session.diag_declared_source.load(Ordering::Relaxed),
        diag_udp_source_mismatch = session.diag_source_mismatch.load(Ordering::Relaxed),
        "socks5 reply pump stats (final)"
    );
}

fn reply_destination_for_target(session: &Session, target: &str) -> Option<SocketAddr> {
    session
        .target_clients
        .get(target)
        .map(|entry| *entry.value())
        .or_else(|| *session.clash_addr.read())
}

/// Parse a SOCKS5 UDP request datagram per RFC 1928 §7.
fn parse_udp_request(buf: &[u8]) -> Option<(u8, String, Bytes)> {
    parse_udp_request_bytes(Bytes::copy_from_slice(buf))
}

fn parse_udp_request_bytes(buf: Bytes) -> Option<(u8, String, Bytes)> {
    if buf.len() < 6 {
        return None;
    }
    if buf[0] != 0 || buf[1] != 0 {
        return None;
    }
    let frag = buf[2];
    let atyp = buf[3];
    let mut i = 4usize;
    let host = match atyp {
        ATYP_V4 => {
            if buf.len() < i + 4 {
                return None;
            }
            let v4 = Ipv4Addr::new(buf[i], buf[i + 1], buf[i + 2], buf[i + 3]);
            i += 4;
            v4.to_string()
        }
        ATYP_V6 => {
            if buf.len() < i + 16 {
                return None;
            }
            let mut a = [0u8; 16];
            a.copy_from_slice(&buf[i..i + 16]);
            i += 16;
            Ipv6Addr::from(a).to_string()
        }
        ATYP_DOMAIN => {
            if buf.len() < i + 1 {
                return None;
            }
            let len = buf[i] as usize;
            i += 1;
            if buf.len() < i + len {
                return None;
            }
            let name = String::from_utf8_lossy(&buf[i..i + len]).to_string();
            i += len;
            name
        }
        _ => return None,
    };
    if buf.len() < i + 2 {
        return None;
    }
    let port = u16::from_be_bytes([buf[i], buf[i + 1]]);
    i += 2;
    Some((frag, format!("{host}:{port}"), buf.slice(i..)))
}

#[doc(hidden)]
pub fn parse_udp_request_for_bench(buf: &[u8]) -> Option<(u8, String, Bytes)> {
    parse_udp_request(buf)
}

// ----- RFC 1928 §7 fragment reassembler -----------------------------------

const SOCKS5_FRAG_TTL: Duration = Duration::from_secs(10);
const SOCKS5_FRAG_MAX_BUFFERS: usize = 1024;

struct Socks5FragBuffer {
    parts: Vec<Option<Bytes>>,
    target: Option<String>,
    last_seq: Option<usize>,
    highest_seq: usize,
    first_seen: Instant,
}

impl Socks5FragBuffer {
    fn new() -> Self {
        Self {
            parts: Vec::new(),
            target: None,
            last_seq: None,
            highest_seq: 0,
            first_seen: Instant::now(),
        }
    }
}

struct Socks5FragAssembler {
    buffers: HashMap<SocketAddr, Socks5FragBuffer>,
    last_gc: Instant,
}

impl Socks5FragAssembler {
    fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            last_gc: Instant::now(),
        }
    }

    fn accept(
        &mut self,
        src: SocketAddr,
        frag: u8,
        target: String,
        data: Bytes,
    ) -> Option<(String, Bytes)> {
        let is_last = frag & 0x80 != 0;
        let seq = (frag & 0x7F) as usize;
        if seq == 0 {
            return None;
        }

        self.gc_if_due();

        if self.buffers.len() >= SOCKS5_FRAG_MAX_BUFFERS && !self.buffers.contains_key(&src) {
            self.evict_oldest();
        }

        let buf = self
            .buffers
            .entry(src)
            .or_insert_with(Socks5FragBuffer::new);

        if seq < buf.highest_seq {
            *buf = Socks5FragBuffer::new();
        }
        buf.highest_seq = buf.highest_seq.max(seq);

        if buf.target.is_none() {
            buf.target = Some(target);
        }

        if buf.parts.len() < seq {
            buf.parts.resize(seq, None);
        }
        if buf.parts[seq - 1].is_some() {
            return None;
        }
        buf.parts[seq - 1] = Some(data);

        if is_last {
            buf.last_seq = Some(seq);
        }

        let complete = match buf.last_seq {
            Some(last) => {
                buf.parts.len() >= last && buf.parts.iter().take(last).all(|p| p.is_some())
            }
            None => false,
        };
        if !complete {
            return None;
        }

        let done = self.buffers.remove(&src)?;
        let last = done.last_seq?;
        let total_len: usize = done
            .parts
            .iter()
            .take(last)
            .map(|p| p.as_ref().map(|b| b.len()).unwrap_or(0))
            .sum();
        let mut out = BytesMut::with_capacity(total_len);
        for p in done.parts.into_iter().take(last).flatten() {
            out.put_slice(&p);
        }
        let target = done.target.unwrap_or_default();
        Some((target, out.freeze()))
    }

    fn gc_if_due(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_gc) < Duration::from_secs(1) {
            return;
        }
        self.last_gc = now;
        self.buffers
            .retain(|_, b| now.duration_since(b.first_seen) < SOCKS5_FRAG_TTL);
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest_key) = self
            .buffers
            .iter()
            .min_by_key(|(_, b)| b.first_seen)
            .map(|(k, _)| *k)
        {
            self.buffers.remove(&oldest_key);
        }
    }
}

/// Write a SOCKS5 UDP reply (RFC 1928 §7) into `dst`. `dst.clear()` is
/// the caller's responsibility — this lets the same `BytesMut` be reused
/// across thousands of sends without reallocating its backing storage.
/// Replaces the earlier `build_udp_reply` which returned a fresh
/// `Vec<u8>` per call — a ~200 ns allocation on the moonlight video hot
/// path (~2500 pps) that we can eliminate entirely.
fn write_udp_reply_into(dst: &mut BytesMut, src: &str, payload: &[u8]) {
    // Reserve once: RSV(2) + FRAG(1) + ATYP(1) + ADDR(<=256) + PORT(2) + payload.
    dst.reserve(6 + 256 + payload.len());
    dst.put_u8(0);
    dst.put_u8(0);
    dst.put_u8(0);
    let (host, port) = match src.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(0)),
        None => (src, 0u16),
    };
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        dst.put_u8(ATYP_V4);
        dst.put_slice(&v4.octets());
    } else if let Ok(v6) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<Ipv6Addr>()
    {
        dst.put_u8(ATYP_V6);
        dst.put_slice(&v6.octets());
    } else {
        dst.put_u8(ATYP_DOMAIN);
        let hb = host.as_bytes();
        dst.put_u8(hb.len() as u8);
        dst.put_slice(hb);
    }
    dst.put_slice(&port.to_be_bytes());
    dst.put_slice(payload);
}

async fn pipe<T>(a: TcpStream, b: T) -> anyhow::Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut client_read, mut client_write) = a.into_split();
    let (mut tunnel_read, mut tunnel_write) = tokio::io::split(b);

    let client_to_tunnel = async {
        let copied = tokio::io::copy(&mut client_read, &mut tunnel_write).await?;
        tunnel_write.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    let tunnel_to_client = async {
        let copied = tokio::io::copy(&mut tunnel_read, &mut client_write).await?;
        client_write.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    tokio::pin!(client_to_tunnel);
    tokio::pin!(tunnel_to_client);

    tokio::select! {
        result = &mut client_to_tunnel => {
            result?;
            tunnel_to_client.await?;
        }
        result = &mut tunnel_to_client => {
            result?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod pipe_tests {
    use super::*;

    #[tokio::test]
    async fn pipe_exits_when_tunnel_side_closes_before_client_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (mut tunnel_peer, tunnel) = tokio::io::duplex(4096);

        let pipe_task = tokio::spawn(async move {
            let (server_socket, _) = listener.accept().await.unwrap();
            pipe(server_socket, tunnel).await.unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"POST /upload HTTP/1.1\r\n")
            .await
            .unwrap();

        let mut request_prefix = [0u8; 4];
        tunnel_peer.read_exact(&mut request_prefix).await.unwrap();
        assert_eq!(&request_prefix, b"POST");

        tunnel_peer
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        tunnel_peer.shutdown().await.unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .expect("local client should see tunnel close")
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 403 Forbidden"));

        tokio::time::timeout(Duration::from_secs(1), pipe_task)
            .await
            .expect("pipe should exit after tunnel close")
            .unwrap();
    }

    #[tokio::test]
    async fn pipe_keeps_tunnel_to_client_open_after_client_write_half_fin() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (mut tunnel_peer, tunnel) = tokio::io::duplex(4096);

        let pipe_task = tokio::spawn(async move {
            let (server_socket, _) = listener.accept().await.unwrap();
            pipe(server_socket, tunnel).await.unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"POST /upload HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody")
            .await
            .unwrap();
        client.shutdown().await.unwrap();

        let mut request =
            vec![0u8; b"POST /upload HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody".len()];
        tunnel_peer.read_exact(&mut request).await.unwrap();
        assert_eq!(
            &request,
            b"POST /upload HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody"
        );

        tunnel_peer
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
            .await
            .unwrap();
        tunnel_peer.shutdown().await.unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .expect("local client should receive response after half-fin")
            .unwrap();
        assert_eq!(&response, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");

        tokio::time::timeout(Duration::from_secs(1), pipe_task)
            .await
            .expect("pipe should exit after both directions finish")
            .unwrap();
    }
}

#[cfg(test)]
mod socks5_frag_tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:12345".parse().unwrap()
    }

    #[test]
    fn in_order_fragments_assemble() {
        let mut asm = Socks5FragAssembler::new();
        assert!(asm
            .accept(
                addr(),
                0x01,
                "a.example:53".into(),
                Bytes::from_static(b"AAA")
            )
            .is_none());
        assert!(asm
            .accept(addr(), 0x02, "ignored:53".into(), Bytes::from_static(b"BB"))
            .is_none());
        let out = asm
            .accept(addr(), 0x83, "ignored:53".into(), Bytes::from_static(b"C"))
            .expect("last fragment completes");
        assert_eq!(out.0, "a.example:53", "target is from first fragment");
        assert_eq!(&out.1[..], b"AAABBC");
    }

    #[test]
    fn lower_seq_resets_queue() {
        let mut asm = Socks5FragAssembler::new();
        asm.accept(addr(), 0x01, "first:53".into(), Bytes::from_static(b"OLD1"));
        asm.accept(addr(), 0x02, "first:53".into(), Bytes::from_static(b"OLD2"));
        asm.accept(
            addr(),
            0x01,
            "second:53".into(),
            Bytes::from_static(b"NEW1"),
        );
        let out = asm
            .accept(
                addr(),
                0x82,
                "second:53".into(),
                Bytes::from_static(b"NEW2"),
            )
            .expect("new sequence completes cleanly");
        assert_eq!(out.0, "second:53");
        assert_eq!(&out.1[..], b"NEW1NEW2");
    }

    #[test]
    fn standalone_is_caller_handled() {
        let mut asm = Socks5FragAssembler::new();
        assert!(asm
            .accept(addr(), 0x00, "x:1".into(), Bytes::from_static(b"X"))
            .is_none());
    }

    #[test]
    fn duplicate_fragment_ignored() {
        let mut asm = Socks5FragAssembler::new();
        asm.accept(addr(), 0x01, "t:1".into(), Bytes::from_static(b"A"));
        assert!(asm
            .accept(addr(), 0x01, "t:1".into(), Bytes::from_static(b"DUP"))
            .is_none());
        let out = asm
            .accept(addr(), 0x82, "t:1".into(), Bytes::from_static(b"B"))
            .unwrap();
        assert_eq!(&out.1[..], b"AB", "first-won for duplicate seq=1");
    }
}

#[cfg(test)]
mod pending_by_tcp_tests {
    //! Lock in the multi-stream fix: N concurrent UDP ASSOCs from the same
    //! client IP must each be adoptable by exactly one first-UDP-packet.
    //!
    //! Regression guard for the earlier `pending_by_ip: DashMap<IpAddr, Session>`
    //! design that silently dropped 11/12 sessions because DashMap::insert
    //! upserts on the IP key.
    use super::*;

    /// Simulates the post-fix `ProxyState.pending_by_tcp` lookup/adoption loop.
    #[test]
    fn twelve_concurrent_assocs_all_adopt() {
        let pending: DashMap<SocketAddr, u32> = DashMap::new();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        // Register 12 ASSOCs from the same client IP, each with a distinct
        // TCP source port — mirrors kernel-assigned ephemeral ports for
        // back-to-back `TcpStream::connect` from one client.
        for port in 55001..55013 {
            pending.insert(SocketAddr::new(ip, port), u32::from(port));
        }
        assert_eq!(pending.len(), 12, "all 12 registrations survive");

        // 12 distinct UDP source ports arrive; each must adopt exactly one
        // pending entry (matches `udp_reader_loop` logic).
        let mut adoptions: Vec<u32> = Vec::new();
        for udp_src_port in 50001u16..50013 {
            let src = SocketAddr::new(ip, udp_src_port);
            let candidate_key = pending
                .iter()
                .find(|e| e.key().ip() == src.ip())
                .map(|e| *e.key());
            if let Some(k) = candidate_key {
                if let Some((_, v)) = pending.remove(&k) {
                    adoptions.push(v);
                }
            }
        }

        assert_eq!(adoptions.len(), 12, "every ASSOC got adopted");
        assert_eq!(pending.len(), 0, "pending drained");
        let mut unique = adoptions.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 12, "each adoption picks a distinct session");
    }

    /// A 13th UDP source from the same IP finds no pending session — correct
    /// behaviour, since only 12 ASSOCs were established.
    #[test]
    fn thirteenth_packet_finds_nothing() {
        let pending: DashMap<SocketAddr, u32> = DashMap::new();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        for port in 55001..55013 {
            pending.insert(SocketAddr::new(ip, port), u32::from(port));
        }
        for _ in 0..12 {
            let k = pending
                .iter()
                .find(|e| e.key().ip() == ip)
                .map(|e| *e.key())
                .unwrap();
            pending.remove(&k).unwrap();
        }
        let miss = pending
            .iter()
            .find(|e| e.key().ip() == ip)
            .map(|e| *e.key());
        assert!(miss.is_none(), "pool exhausted after 12 adoptions");
    }

    /// Different-IP packets don't accidentally adopt entries from another IP's
    /// pool.
    #[test]
    fn isolation_across_ips() {
        let pending: DashMap<SocketAddr, u32> = DashMap::new();
        let ip_a: IpAddr = "10.0.0.1".parse().unwrap();
        let ip_b: IpAddr = "10.0.0.2".parse().unwrap();
        pending.insert(SocketAddr::new(ip_a, 40000), 1);
        pending.insert(SocketAddr::new(ip_b, 40000), 2);

        let src_c: IpAddr = "10.0.0.3".parse().unwrap();
        let miss = pending
            .iter()
            .find(|e| e.key().ip() == src_c)
            .map(|e| *e.key());
        assert!(miss.is_none(), "unrelated IP cannot adopt");

        // IP_a's packet adopts only IP_a's entry.
        let k = pending
            .iter()
            .find(|e| e.key().ip() == ip_a)
            .map(|e| *e.key())
            .unwrap();
        let (_, v) = pending.remove(&k).unwrap();
        assert_eq!(v, 1);
        assert_eq!(pending.len(), 1, "IP_b's entry untouched");
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct RecordingBackend {
        calls: Mutex<Vec<(String, String)>>,
    }

    struct RecordingUdpBackend {
        opens: Mutex<Vec<(String, String)>>,
    }

    struct BlockingUdpBackend {
        opens: Mutex<Vec<(String, String)>>,
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<Option<oneshot::Receiver<()>>>,
        sent_payloads: Arc<Mutex<Vec<Bytes>>>,
    }

    struct NoopUdpTunnel {
        conn_id: String,
    }

    struct CollectingUdpTunnel {
        conn_id: String,
        sent_payloads: Arc<Mutex<Vec<Bytes>>>,
    }

    struct NoopUdpSender;

    struct CollectingUdpSender {
        sent_payloads: Arc<Mutex<Vec<Bytes>>>,
    }

    struct NoopUdpReceiver {
        conn_id: String,
    }

    impl crate::backend::UdpTunnel for NoopUdpTunnel {
        fn split(
            self: Box<Self>,
        ) -> (
            crate::backend::BoxUdpTunnelSender,
            crate::backend::BoxUdpTunnelReceiver,
        ) {
            (
                Box::new(NoopUdpSender),
                Box::new(NoopUdpReceiver {
                    conn_id: self.conn_id,
                }),
            )
        }
    }

    impl crate::backend::UdpTunnel for CollectingUdpTunnel {
        fn split(
            self: Box<Self>,
        ) -> (
            crate::backend::BoxUdpTunnelSender,
            crate::backend::BoxUdpTunnelReceiver,
        ) {
            (
                Box::new(CollectingUdpSender {
                    sent_payloads: self.sent_payloads,
                }),
                Box::new(NoopUdpReceiver {
                    conn_id: self.conn_id,
                }),
            )
        }
    }

    impl crate::backend::UdpTunnelSender for NoopUdpSender {
        fn try_send(&self, _payload: Bytes) -> Result<(), TrySendKind> {
            Ok(())
        }
    }

    impl crate::backend::UdpTunnelSender for CollectingUdpSender {
        fn try_send(&self, payload: Bytes) -> Result<(), TrySendKind> {
            self.sent_payloads.lock().push(payload);
            Ok(())
        }
    }

    #[async_trait]
    impl crate::backend::UdpTunnelReceiver for NoopUdpReceiver {
        async fn recv(&mut self) -> Option<Bytes> {
            None
        }

        fn try_recv(&mut self) -> Result<Bytes, tokio::sync::mpsc::error::TryRecvError> {
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        }

        fn conn_id(&self) -> &str {
            &self.conn_id
        }

        async fn close(&mut self) {}
    }

    #[async_trait]
    impl crate::backend::Socks5Backend for RecordingBackend {
        async fn open_tcp(
            &self,
            group_id: &str,
            target: &str,
        ) -> anyhow::Result<crate::backend::BoxTcpTunnel> {
            self.calls
                .lock()
                .push((group_id.to_string(), target.to_string()));
            let (client, mut server) = tokio::io::duplex(1024);
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                if let Ok(n) = server.read(&mut buf).await {
                    let _ = server.write_all(&buf[..n]).await;
                }
            });
            Ok(Box::pin(client))
        }

        async fn open_udp(
            &self,
            _group_id: &str,
            _target: &str,
        ) -> anyhow::Result<crate::backend::BoxUdpTunnel> {
            anyhow::bail!("udp unused in CONNECT backend test")
        }
    }

    #[async_trait]
    impl crate::backend::Socks5Backend for RecordingUdpBackend {
        async fn open_tcp(
            &self,
            _group_id: &str,
            _target: &str,
        ) -> anyhow::Result<crate::backend::BoxTcpTunnel> {
            anyhow::bail!("tcp unused in UDP backend test")
        }

        async fn open_udp(
            &self,
            group_id: &str,
            target: &str,
        ) -> anyhow::Result<crate::backend::BoxUdpTunnel> {
            self.opens
                .lock()
                .push((group_id.to_string(), target.to_string()));
            Ok(Box::new(NoopUdpTunnel {
                conn_id: target.to_string(),
            }))
        }
    }

    #[async_trait]
    impl crate::backend::Socks5Backend for BlockingUdpBackend {
        async fn open_tcp(
            &self,
            _group_id: &str,
            _target: &str,
        ) -> anyhow::Result<crate::backend::BoxTcpTunnel> {
            anyhow::bail!("tcp unused in UDP backend test")
        }

        async fn open_udp(
            &self,
            group_id: &str,
            target: &str,
        ) -> anyhow::Result<crate::backend::BoxUdpTunnel> {
            self.opens
                .lock()
                .push((group_id.to_string(), target.to_string()));
            if let Some(started) = self.started.lock().take() {
                let _ = started.send(());
            }
            let release = self.release.lock().take();
            if let Some(release) = release {
                let _ = release.await;
            }
            Ok(Box::new(CollectingUdpTunnel {
                conn_id: target.to_string(),
                sent_payloads: self.sent_payloads.clone(),
            }))
        }
    }

    async fn test_proxy_state() -> Arc<ProxyState> {
        let udp_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let std_udp = bind_tuned_udp(udp_addr).expect("udp bind");
        let udp_sock = Arc::new(UdpSocket::from_std(std_udp).expect("tokio udp"));
        let bind_addr = udp_sock.local_addr().expect("udp local addr");
        Arc::new(ProxyState {
            udp_sock,
            bind_addr,
            sessions: DashMap::new(),
            pending_by_tcp: DashMap::new(),
        })
    }

    async fn unused_loopback_addr() -> SocketAddr {
        for _ in 0..50 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            if bind_tuned_udp(addr).is_ok() {
                drop(listener);
                return addr;
            }
        }
        panic!("could not find free TCP+UDP loopback port");
    }

    async fn wait_for_tcp_listener(
        addr: SocketAddr,
        task: &tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        for _ in 0..250 {
            assert!(
                !task.is_finished(),
                "serve_with_backend exited before TCP listener opened"
            );
            if TcpStream::connect(addr).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("socks5 listener did not open at {addr}");
    }

    fn encode_udp_packet(target: &str, payload: &'static [u8]) -> Bytes {
        encode_udp_packet_bytes(target, Bytes::from_static(payload))
    }

    fn encode_udp_packet_bytes(target: &str, payload: Bytes) -> Bytes {
        let mut out = BytesMut::new();
        write_udp_reply_into(&mut out, target, &payload);
        out.freeze()
    }

    async fn wait_for_udp_opens(
        backend: &RecordingUdpBackend,
        expected_len: usize,
    ) -> Vec<(String, String)> {
        for _ in 0..100 {
            let opens = backend.opens.lock().clone();
            if opens.len() >= expected_len {
                return opens;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        backend.opens.lock().clone()
    }

    async fn wait_for_pending_session_from_ip(state: &Arc<ProxyState>, ip: IpAddr) {
        for _ in 0..100 {
            if state
                .pending_by_tcp
                .iter()
                .any(|entry| entry.key().ip() == ip)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("pending UDP ASSOCIATE session for {ip} was not registered");
    }

    async fn wait_for_sent_payloads(
        sent_payloads: &Arc<Mutex<Vec<Bytes>>>,
        expected_len: usize,
    ) -> Vec<Bytes> {
        for _ in 0..100 {
            let sent = sent_payloads.lock().clone();
            if sent.len() >= expected_len {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sent_payloads.lock().clone()
    }

    #[tokio::test]
    async fn udp_target_forwarder_buffers_open_window_without_dropping_oldest_packets() {
        const OPEN_WINDOW_PACKETS: usize = 512;

        let state = test_proxy_state().await;
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let sent_payloads = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(BlockingUdpBackend {
            opens: Mutex::new(Vec::new()),
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(Some(release_rx)),
            sent_payloads: sent_payloads.clone(),
        });
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let session = Arc::new(Session {
            group_id: "group-1".to_string(),
            backend,
            udp_sock: state.udp_sock.clone(),
            clash_addr: RwLock::new(None),
            target_clients: DashMap::new(),
            frag_asm: Mutex::new(Socks5FragAssembler::new()),
            closed: AtomicBool::new(false),
            diag_declared_source: AtomicBool::new(false),
            diag_source_mismatch: AtomicU64::new(0),
            target_forwarders: DashMap::new(),
        });
        state
            .pending_by_tcp
            .insert(SocketAddr::new(ip, 55000), session.clone());

        let src = SocketAddr::new(ip, 50001);
        let target = "127.0.0.1:47998";
        process_udp_packet(
            &state,
            src,
            encode_udp_packet_bytes(target, Bytes::from_static(&[0, 0])),
        );
        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("open_udp should start")
            .expect("open_udp start signal should be sent");

        for seq in 1..OPEN_WINDOW_PACKETS {
            let mut payload = BytesMut::with_capacity(2);
            payload.put_u16(seq as u16);
            process_udp_packet(
                &state,
                src,
                encode_udp_packet_bytes(target, payload.freeze()),
            );
        }

        release_tx
            .send(())
            .expect("release receiver should be live");
        let sent = wait_for_sent_payloads(&sent_payloads, OPEN_WINDOW_PACKETS).await;

        session.closed.store(true, Ordering::SeqCst);
        session.target_forwarders.clear();

        assert_eq!(
            sent.len(),
            OPEN_WINDOW_PACKETS,
            "SOCKS5 UDP open window should not drop packets before the tunnel is ready"
        );
        assert_eq!(&sent[0][..], &[0, 0]);
        assert_eq!(
            &sent[OPEN_WINDOW_PACKETS - 1][..],
            &(OPEN_WINDOW_PACKETS as u16 - 1).to_be_bytes()
        );
    }

    #[tokio::test]
    async fn udp_assoc_accepts_later_source_ports_from_same_client_ip() {
        let state = test_proxy_state().await;
        let backend = Arc::new(RecordingUdpBackend {
            opens: Mutex::new(Vec::new()),
        });
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let session = Arc::new(Session {
            group_id: "group-1".to_string(),
            backend: backend.clone(),
            udp_sock: state.udp_sock.clone(),
            clash_addr: RwLock::new(None),
            target_clients: DashMap::new(),
            frag_asm: Mutex::new(Socks5FragAssembler::new()),
            closed: AtomicBool::new(false),
            diag_declared_source: AtomicBool::new(false),
            diag_source_mismatch: AtomicU64::new(0),
            target_forwarders: DashMap::new(),
        });
        state
            .pending_by_tcp
            .insert(SocketAddr::new(ip, 55000), session);

        process_udp_packet(
            &state,
            SocketAddr::new(ip, 50001),
            encode_udp_packet("127.0.0.1:47998", b"video"),
        );
        let opens = wait_for_udp_opens(&backend, 1).await;
        assert_eq!(
            opens,
            vec![("group-1".to_string(), "127.0.0.1:47998".to_string())]
        );

        process_udp_packet(
            &state,
            SocketAddr::new(ip, 50002),
            encode_udp_packet("127.0.0.1:48000", b"audio"),
        );
        let opens = wait_for_udp_opens(&backend, 2).await;
        assert_eq!(
            opens,
            vec![
                ("group-1".to_string(), "127.0.0.1:47998".to_string()),
                ("group-1".to_string(), "127.0.0.1:48000".to_string()),
            ],
            "a second UDP source port from the same client IP must stay on the active ASSOC session"
        );
    }

    #[tokio::test]
    async fn aborting_serve_releases_shared_udp_socket() {
        let addr = unused_loopback_addr().await;
        let backend = Arc::new(RecordingBackend {
            calls: Mutex::new(Vec::new()),
        });
        let auth: AuthValidator = Arc::new(|_, _| Some("group-1".to_string()));
        let task = tokio::spawn(serve_with_backend(addr, backend, auth));

        wait_for_tcp_listener(addr, &task).await;
        task.abort();
        let _ = task.await;

        for _ in 0..50 {
            if bind_tuned_udp(addr).is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("aborting serve_with_backend must release UDP bind {addr}");
    }

    #[tokio::test]
    async fn connect_uses_configured_backend() {
        let backend = Arc::new(RecordingBackend {
            calls: Mutex::new(Vec::new()),
        });
        let auth: AuthValidator = Arc::new(|user, pass| {
            (user == "alice" && pass == "secret").then(|| "group-1".to_string())
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let state = test_proxy_state().await;
        let backend_for_handle = backend.clone();
        let handle_task =
            tokio::spawn(
                async move { handle(server, backend_for_handle, auth.into(), state).await },
            );

        client.write_all(&[VER, 1, METHOD_USER_PASS]).await.unwrap();
        let mut method = [0u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [VER, METHOD_USER_PASS]);

        client
            .write_all(&[
                USER_PASS_VER,
                5,
                b'a',
                b'l',
                b'i',
                b'c',
                b'e',
                6,
                b's',
                b'e',
                b'c',
                b'r',
                b'e',
                b't',
            ])
            .await
            .unwrap();
        let mut auth_reply = [0u8; 2];
        client.read_exact(&mut auth_reply).await.unwrap();
        assert_eq!(auth_reply, [USER_PASS_VER, 0x00]);

        client
            .write_all(&[
                VER,
                CMD_CONNECT,
                0,
                ATYP_DOMAIN,
                11,
                b'e',
                b'x',
                b'a',
                b'm',
                b'p',
                b'l',
                b'e',
                b'.',
                b'c',
                b'o',
                b'm',
                0x1f,
                0x90,
            ])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], REP_OK);

        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        drop(client);

        handle_task.await.unwrap().unwrap();
        assert_eq!(
            backend.calls.lock().as_slice(),
            &[("group-1".to_string(), "example.com:8080".to_string())]
        );
    }

    #[tokio::test]
    async fn connect_accepts_no_auth_when_mode_disables_auth() {
        let backend = Arc::new(RecordingBackend {
            calls: Mutex::new(Vec::new()),
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let state = test_proxy_state().await;
        let backend_for_handle = backend.clone();
        let auth = AuthMode::NoAuth {
            group_id: "group-1".to_string(),
        };
        let handle_task =
            tokio::spawn(async move { handle(server, backend_for_handle, auth, state).await });

        client.write_all(&[VER, 1, METHOD_NO_AUTH]).await.unwrap();
        let mut method = [0u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [VER, METHOD_NO_AUTH]);

        client
            .write_all(&[
                VER,
                CMD_CONNECT,
                0,
                ATYP_DOMAIN,
                11,
                b'e',
                b'x',
                b'a',
                b'm',
                b'p',
                b'l',
                b'e',
                b'.',
                b'c',
                b'o',
                b'm',
                0x1f,
                0x90,
            ])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], REP_OK);
        drop(client);

        handle_task.await.unwrap().unwrap();
        assert_eq!(
            backend.calls.lock().as_slice(),
            &[("group-1".to_string(), "example.com:8080".to_string())]
        );
    }

    #[tokio::test]
    async fn serve_auth_mode_ready_accepts_no_auth_connections() {
        let addr = unused_loopback_addr().await;
        let backend = Arc::new(RecordingBackend {
            calls: Mutex::new(Vec::new()),
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(serve_with_backend_auth_mode_ready(
            addr,
            backend.clone(),
            AuthMode::NoAuth {
                group_id: "group-1".to_string(),
            },
            Some(ready_tx),
        ));
        let _udp_addr = ready_rx.await.expect("listener should publish ready addr");

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(&[VER, 1, METHOD_NO_AUTH]).await.unwrap();
        let mut method = [0u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [VER, METHOD_NO_AUTH]);

        client
            .write_all(&[
                VER,
                CMD_CONNECT,
                0,
                ATYP_DOMAIN,
                11,
                b'e',
                b'x',
                b'a',
                b'm',
                b'p',
                b'l',
                b'e',
                b'.',
                b'c',
                b'o',
                b'm',
                0x1f,
                0x90,
            ])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], REP_OK);

        task.abort();
        let _ = task.await;
        assert_eq!(
            backend.calls.lock().as_slice(),
            &[("group-1".to_string(), "example.com:8080".to_string())]
        );
    }

    #[tokio::test]
    async fn serve_ready_with_ephemeral_port_reports_tcp_listener_addr() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let backend = Arc::new(RecordingBackend {
            calls: Mutex::new(Vec::new()),
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(serve_with_backend_auth_mode_ready(
            addr,
            backend,
            AuthMode::NoAuth {
                group_id: "group-1".to_string(),
            },
            Some(ready_tx),
        ));

        let ready_addr = ready_rx.await.expect("listener should publish ready addr");

        assert_ne!(ready_addr.port(), 0);
        TcpStream::connect(ready_addr)
            .await
            .expect("ready addr should be the TCP listener addr");
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn udp_associate_accepts_no_auth_and_routes_with_configured_group() {
        let backend = Arc::new(RecordingUdpBackend {
            opens: Mutex::new(Vec::new()),
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        let client_addr = client.local_addr().unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let state = test_proxy_state().await;
        let backend_for_handle = backend.clone();
        let state_for_handle = state.clone();
        let auth = AuthMode::NoAuth {
            group_id: "group-1".to_string(),
        };
        let handle_task = tokio::spawn(async move {
            handle(server, backend_for_handle, auth, state_for_handle).await
        });

        client.write_all(&[VER, 1, METHOD_NO_AUTH]).await.unwrap();
        let mut method = [0u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [VER, METHOD_NO_AUTH]);

        client
            .write_all(&[VER, CMD_UDP, 0, ATYP_V4, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut udp_reply = [0u8; 10];
        client.read_exact(&mut udp_reply).await.unwrap();
        assert_eq!(udp_reply[1], REP_OK);
        wait_for_pending_session_from_ip(&state, client_addr.ip()).await;

        process_udp_packet(
            &state,
            SocketAddr::new(client_addr.ip(), 50001),
            encode_udp_packet("127.0.0.1:47998", b"video"),
        );

        let opens = wait_for_udp_opens(&backend, 1).await;
        assert_eq!(
            opens,
            vec![("group-1".to_string(), "127.0.0.1:47998".to_string())]
        );

        drop(client);
        handle_task.await.unwrap().unwrap();
    }
}
