//! lantunnel-gateway — the public Lantunnel gateway.

mod certificate_lifecycle;
mod gateway_control;
mod managed_identity;
mod onboarding;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use tp_core::config::{
    load_from_str, normalize_listen_addr, Config, GatewayConfig, TRANSPORT_TYPE_GRPC,
    TRANSPORT_TYPE_QUIC, TRANSPORT_TYPE_WEBSOCKET,
};
use tp_gateway::{Gateway, GatewayServer, RelayUsageWal};
use tp_transport::{tls, GrpcServer, QuicServer, QuicTuning, WsServer};

#[derive(Parser)]
#[command(name = "lantunnel-gateway", version)]
struct Cli {
    /// Path to YAML config. Defaults to the path onboarding writes.
    #[arg(short, long)]
    config: Option<String>,
    /// Validate the complete V2-only Gateway configuration without binding
    /// listeners or mutating runtime state.
    #[arg(long)]
    check_config: bool,
    #[command(subcommand)]
    command: Option<GatewayCommand>,
}

#[derive(Subcommand)]
enum GatewayCommand {
    /// Generate this machine's self-signed identity and register the claim in
    /// its pairing artifact. The artifact names the kind; Fleet and BYOG are the
    /// same command.
    Onboard(onboarding::OnboardArgs),
    /// Run machine-scoped Gateway support services.
    Mapping(MappingCommand),
}

#[derive(Args)]
struct MappingCommand {
    #[command(subcommand)]
    command: MappingSubcommand,
}

#[derive(Subcommand)]
enum MappingSubcommand {
    /// Serve a standalone UDP mapping reflector. A Gateway binds its own, so
    /// this exists for isolated test setups that run no Gateway.
    Serve(MappingServeArgs),
}

#[derive(Args)]
struct MappingServeArgs {
    /// UDP endpoint this standalone reflector owns.
    #[arg(long, default_value = "0.0.0.0:8444")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();

    if let Some(command) = cli.command {
        return run_gateway_command(command).await;
    }

    // Onboarding wrote the configuration at this path, so an operator who took
    // the defaults starts the Gateway with no arguments at all.
    let config_path = cli
        .config
        .as_deref()
        .unwrap_or(onboarding::DEFAULT_RUNTIME_CONFIG_PATH);

    let raw_config = std::fs::read_to_string(config_path)
        .with_context(|| format!("read public Gateway config at {config_path}"))?;
    reject_legacy_public_gateway_yaml(&raw_config)?;
    let cfg: Config = load_from_str(&raw_config)?;
    let gw_cfg = cfg
        .gateway
        .clone()
        .ok_or_else(|| anyhow::anyhow!("config missing [gateway] section"))?;
    let managed_identity_store =
        managed_identity::FileManagedIdentityStore::from_config_path(Path::new(config_path))?;
    let persisted_managed_identity = managed_identity_store.load()?;
    let managed_runtime = persisted_managed_identity.is_some();
    cfg.validate()?;
    validate_public_v2_gateway_config(&gw_cfg, managed_runtime)?;
    validate_persistent_tls_and_scope_files(&gw_cfg)?;
    if let Some(managed_identity) = persisted_managed_identity.as_ref() {
        let certificate = certificate_lifecycle::load_self_signed_ip_identity(
            Path::new(config_path),
            managed_identity.public_ip,
        )?;
        if certificate.leaf_sha256 != managed_identity.certificate_leaf_sha256
            || certificate.spki_sha256 != managed_identity.certificate_spki_sha256
        {
            anyhow::bail!("persistent Gateway certificate changed from Managed identity state");
        }
    }
    if cli.check_config {
        println!("lantunnel-gateway V2 config: OK");
        return Ok(());
    }
    let _log_guard = init_logging(&cfg.log);
    announce_log_sink(&cfg.log);

    let listen: SocketAddr = normalize_listen_addr(&gw_cfg.listen_addr).parse()?;
    // This Gateway owns its mapping reflector. It used to be a separate process
    // the Gateway could only probe for, which bought nothing — the probe was
    // fatal at startup and advisory afterwards — and cost the operator an
    // ordering constraint between two processes. Binding it here makes the only
    // failure an honest one: the port is already taken.
    let mapping_probe_addr = SocketAddr::new(listen.ip(), gw_cfg.mapping_probe_port);
    let mapping_service = tp_gateway::mapping_probe::MappingProbeServer::bind(mapping_probe_addr)
        .await
        .with_context(|| {
            format!(
                "bind this Gateway's UDP mapping service on {mapping_probe_addr}; another process already holds that port"
            )
        })?;
    tracing::info!(endpoint = %mapping_probe_addr, "UDP mapping service bound");
    let _mapping_service_task = tokio::spawn(async move {
        if let Err(error) = mapping_service.run().await {
            tracing::error!(%error, "UDP mapping service stopped");
        }
    });
    let configured_tls_paths = match (&gw_cfg.tls_cert, &gw_cfg.tls_key) {
        (Some(c), Some(k)) if !c.is_empty() && !k.is_empty() => Some((c.as_str(), k.as_str())),
        _ => None,
    };
    let (cert_path, key_path) = configured_tls_paths.ok_or_else(|| {
        anyhow::anyhow!("lantunnel-gateway requires persistent gateway.tls_cert/tls_key")
    })?;
    let identity = certificate_lifecycle::load_server_identity(
        std::path::Path::new(cert_path),
        std::path::Path::new(key_path),
    )?;
    let gateway_control_config = persisted_managed_identity.as_ref().map(|managed| {
        gateway_control::GatewayControlConnectConfig {
            kind: managed.kind,
            gateway_id: managed.id.clone(),
            platform_url: managed.platform_url.clone(),
            boot_id: uuid::Uuid::new_v4().hyphenated().to_string(),
            leaf_sha256: managed.certificate_leaf_sha256.clone(),
            private_key_pem: identity.private_key_pem.clone(),
            certificate_pem: None,
            claim_secret: None,
        }
    });
    let managed_tls_server_name = persisted_managed_identity
        .as_ref()
        .map(|managed| managed.public_ip.to_string());
    let data_plane_certificate_pem = identity.certificate_pem.clone();
    let grpc_certificate_pem = identity.certificate_pem.into_bytes();
    let grpc_private_key_pem = identity.private_key_pem;
    let tls_cfg = tls::server_config(identity.certificates, identity.private_key)?;

    let tunnel_transport_type = if gw_cfg.transport_type.is_empty() {
        TRANSPORT_TYPE_QUIC
    } else {
        gw_cfg.transport_type.as_str()
    };
    let (server, endpoint) = match tunnel_transport_type {
        TRANSPORT_TYPE_QUIC => {
            let tuning = QuicTuning {
                congestion: gw_cfg.transport.congestion.clone(),
                keep_alive_secs: gw_cfg.transport.keep_alive_secs,
                max_idle_secs: gw_cfg.transport.max_idle_secs,
                ..QuicTuning::game_streaming()
            };
            tracing::info!(
                congestion = %tuning.congestion,
                keep_alive_secs = tuning.keep_alive_secs,
                max_idle_secs = tuning.max_idle_secs,
                initial_mtu = tuning.initial_mtu,
                min_mtu = tuning.min_mtu,
                mtu_upper_bound = tuning.mtu_upper_bound,
                black_hole_cooldown_secs = tuning.black_hole_cooldown_secs,
                "QUIC transport tuning applied"
            );
            let server = QuicServer::bind(listen, tls_cfg.clone(), tuning)?;
            let endpoint = server.endpoint_handle();
            (GatewayServer::Quic(server), Some(endpoint))
        }
        TRANSPORT_TYPE_WEBSOCKET => {
            let server = WsServer::bind_tls(listen, tls_cfg.clone()).await?;
            (GatewayServer::WebSocket(server), None)
        }
        TRANSPORT_TYPE_GRPC => {
            let server =
                GrpcServer::new(listen).with_tls(grpc_certificate_pem, grpc_private_key_pem);
            (GatewayServer::Grpc(server), None)
        }
        other => anyhow::bail!("unsupported gateway.transport_type {other:?}"),
    };
    let relay_usage_wal = Arc::new(
        RelayUsageWal::open(&gw_cfg.usage_ledger.wal_path)
            .with_context(|| format!("open relay usage WAL at {}", gw_cfg.usage_ledger.wal_path))?,
    );
    tracing::info!(
        path = %gw_cfg.usage_ledger.wal_path,
        "gateway passive relay usage ledger enabled"
    );
    let gateway = Gateway::new(gw_cfg.p2p.clone(), Some(relay_usage_wal.clone()));
    if let Some(scopes_dir) = nonempty_scopes_dir(&gw_cfg) {
        let outcome = gateway
            .scopes()
            .reload_static(std::path::Path::new(scopes_dir))
            .with_context(|| format!("load V2 scopes from {scopes_dir}"))?;
        if !gw_cfg.p2p.enabled && outcome.count != 1 {
            anyhow::bail!("isolated Static Relay acceptance clone requires exactly one V2 Scope");
        }
        tracing::info!(
            scopes_dir,
            count = outcome.count,
            "loaded static V2 Gateway scopes"
        );
        if gw_cfg.p2p.enabled {
            spawn_scope_reload_on_sighup(gateway.clone(), scopes_dir.into());
        }
    }
    tracing::info!(
        p2p_enabled = gw_cfg.p2p.enabled,
        peer_idle_secs = gw_cfg.p2p.peer_idle_secs,
        session_idle_secs = gw_cfg.p2p.session_idle_secs,
        punch_sync_offset_ms = gw_cfg.p2p.punch_sync_offset_ms,
        "gateway P2P config loaded"
    );

    let mut data_plane_task = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.serve(server).await }
    });
    if let Some(tls_server_name) = managed_tls_server_name.as_deref() {
        if let Err(error) = await_managed_data_plane_readiness(
            &mut data_plane_task,
            tunnel_transport_type,
            listen,
            tls_server_name,
            &data_plane_certificate_pem,
        )
        .await
        {
            data_plane_task.abort();
            return Err(error);
        }
    }

    let _gateway_control_task = if let Some(config) = gateway_control_config {
        let tls_server_name = managed_tls_server_name
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Managed Gateway TLS server name is missing"))?;
        let readiness_probe = managed_gateway_readiness_probe(
            tunnel_transport_type.to_owned(),
            listen,
            tls_server_name,
            data_plane_certificate_pem.clone(),
            mapping_probe_addr,
        );
        Some(tokio::spawn(gateway_control::run_forever(
            config,
            readiness_probe,
            gateway.clone(),
            relay_usage_wal.clone(),
        )))
    } else {
        None
    };

    // Periodic global-metrics summary in the log stream (60s cadence). Lets
    // operators eyeball throughput/error rate without polling `/api/metrics`.
    let _summary_task = gateway
        .metrics()
        .spawn_summary_logger(std::time::Duration::from_secs(60));

    // Background metrics sweeper: marks clients offline after 2 min idle
    // and removes them (+ their stale connection rows) after `max_offline`
    // of continued silence. Without this, `MetricsManager.clients` /
    // `.connections` grow monotonically — every rotating mobile-NAT
    // client_id stays forever, eventually dominating RSS.
    // 1 h is conservative: long enough that a phone changing Wi-Fi and
    // reconnecting doesn't lose its running totals, short enough that a
    // dead tunnel doesn't live for days in memory.
    let _sweeper_task = gateway
        .metrics()
        .spawn_sweeper(std::time::Duration::from_secs(3600));

    tracing::info!(
        listen = %listen,
        transport = %tunnel_transport_type,
        "lantunnel-gateway started"
    );
    let serve_result = tokio::select! {
        r = &mut data_plane_task => r.context("join Gateway data-plane task")?,
        _ = shutdown_signal() => {
            if let Some(endpoint) = endpoint {
                tracing::info!("draining in-flight QUIC tunnels (up to 10s)…");
                // Emit CONNECTION_CLOSE with `shutdown` reason — peers observe
                // a clean terminate instead of a transport reset, and the
                // client-side reconnect backoff won't log the misleading
                // "session failed: connection lost" line.
                endpoint.close(0u32.into(), b"shutdown");
                // wait_idle resolves when every active connection has finished
                // its close handshake (or been dropped). Cap at 10 s so an
                // unresponsive peer can't block the process indefinitely.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    endpoint.wait_idle(),
                )
                .await;
            }
            data_plane_task.abort();
            let _ = data_plane_task.await;
            tracing::info!("shutdown complete");
            Ok(())
        }
    };
    if let Err(e) = gateway.flush_pending_relay_usage_to_wal() {
        tracing::error!(error = %e, "failed to flush relay usage during shutdown");
    }
    if let Ok(batch) = relay_usage_wal.snapshot(256) {
        tracing::info!(
            through_seq = batch.through_seq,
            sampled_items = batch.items.len(),
            "relay usage WAL retained unacked usage at shutdown"
        );
    }
    serve_result?;
    Ok(())
}

async fn run_gateway_command(command: GatewayCommand) -> anyhow::Result<()> {
    match command {
        GatewayCommand::Onboard(args) => onboarding::run(args).await,
        GatewayCommand::Mapping(MappingCommand {
            command: MappingSubcommand::Serve(args),
        }) => run_mapping_service(args).await,
    }
}

async fn run_mapping_service(args: MappingServeArgs) -> anyhow::Result<()> {
    let listen = args.listen;
    let server = tp_gateway::mapping_probe::MappingProbeServer::bind(listen)
        .await
        .with_context(|| {
            format!(
                "bind the UDP mapping service on {listen}; each Gateway owns its own mapping port, and only one process may hold a given port"
            )
        })?;
    let bound = server.local_addr()?;
    tracing::info!(listen = %bound, "UDP mapping service started");
    tokio::select! {
        result = server.run() => result.context("UDP mapping service stopped"),
        _ = shutdown_signal() => Ok(()),
    }
}

fn local_data_plane_probe_address(listen: SocketAddr) -> SocketAddr {
    if listen.ip().is_unspecified() {
        let loopback = if listen.is_ipv4() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        };
        SocketAddr::new(loopback, listen.port())
    } else {
        listen
    }
}

fn managed_gateway_readiness_probe(
    transport: String,
    listen: SocketAddr,
    tls_server_name: String,
    certificate_pem: String,
    mapping_probe_address: SocketAddr,
) -> gateway_control::GatewayReadinessProbe {
    let data_probe_address = local_data_plane_probe_address(listen);
    Arc::new(move || {
        let transport = transport.clone();
        let tls_server_name = tls_server_name.clone();
        let certificate_pem = certificate_pem.clone();
        Box::pin(async move {
            let (data, mapping) = tokio::join!(
                tls::probe_data_plane_tls(
                    &transport,
                    data_probe_address,
                    &tls_server_name,
                    &certificate_pem,
                ),
                tp_gateway::mapping_probe::probe_local_readiness(
                    mapping_probe_address,
                    std::time::Duration::from_secs(1),
                ),
            );
            if let Err(error) = &data {
                tracing::warn!(%error, "Managed Gateway local data TLS readiness probe failed");
            }
            if let Err(error) = &mapping {
                tracing::warn!(%error, "Managed Gateway local mapping readiness probe failed");
            }
            data.is_ok() && mapping.is_ok()
        })
    })
}

async fn await_managed_data_plane_readiness(
    data_plane_task: &mut tokio::task::JoinHandle<anyhow::Result<()>>,
    transport: &str,
    listen: SocketAddr,
    tls_server_name: &str,
    certificate_pem: &str,
) -> anyhow::Result<()> {
    let probe_address = local_data_plane_probe_address(listen);
    tokio::select! {
        biased;
        result = &mut *data_plane_task => {
            return Err(match result {
                Ok(Ok(())) => anyhow::anyhow!("Gateway data-plane server stopped before readiness"),
                Ok(Err(error)) => error.context("Gateway data-plane server stopped before readiness"),
                Err(error) => anyhow::Error::new(error)
                    .context("Gateway data-plane server stopped before readiness"),
            });
        }
        result = tls::probe_data_plane_tls(
            transport,
            probe_address,
            tls_server_name,
            certificate_pem,
        ) => result.context("probe Managed Gateway data-plane TLS readiness")?,
    }

    if data_plane_task.is_finished() {
        return Err(match data_plane_task.await {
            Ok(Ok(())) => anyhow::anyhow!("Gateway data-plane server stopped before readiness"),
            Ok(Err(error)) => error.context("Gateway data-plane server stopped before readiness"),
            Err(error) => anyhow::Error::new(error)
                .context("Gateway data-plane server stopped before readiness"),
        });
    }
    Ok(())
}

fn nonempty_scopes_dir(config: &GatewayConfig) -> Option<&str> {
    config
        .scopes_dir
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn reject_legacy_public_gateway_yaml(raw: &str) -> anyhow::Result<()> {
    let document: serde_yaml::Value =
        serde_yaml::from_str(raw).context("parse public Gateway config")?;
    let gateway_key = serde_yaml::Value::String("gateway".into());
    let gateway = document
        .as_mapping()
        .and_then(|root| root.get(&gateway_key))
        .and_then(serde_yaml::Value::as_mapping)
        .ok_or_else(|| anyhow::anyhow!("config missing [gateway] section"))?;
    let merge_key = serde_yaml::Value::String("<<".into());
    if gateway.contains_key(&merge_key) {
        anyhow::bail!(
            "lantunnel-gateway is V2-only; YAML merge keys are forbidden in gateway config"
        );
    }
    if gateway
        .keys()
        .any(|key| !matches!(key, serde_yaml::Value::String(_)))
    {
        anyhow::bail!(
            "lantunnel-gateway is V2-only; gateway config keys must be plain YAML strings"
        );
    }
    for forbidden_runtime_key in ["platform_pairing"] {
        if gateway.contains_key(serde_yaml::Value::String(forbidden_runtime_key.into())) {
            anyhow::bail!(
                "lantunnel-gateway Managed V2 identity is state-owned; gateway.{forbidden_runtime_key} is forbidden in runtime YAML"
            );
        }
    }
    for obsolete_inbound_key in ["web", "platform_auth"] {
        if gateway.contains_key(serde_yaml::Value::String(obsolete_inbound_key.into())) {
            anyhow::bail!(
                "lantunnel-gateway uses outbound Managed V2 control; gateway.{obsolete_inbound_key} is obsolete"
            );
        }
    }
    for legacy_key in [
        "auth_username",
        "auth_password",
        "credential",
        "proxy",
        "tunnel_key",
        "group",
        "group_id",
        "group_password",
        "username",
        "password",
    ] {
        if gateway.contains_key(serde_yaml::Value::String(legacy_key.into())) {
            anyhow::bail!(
                "lantunnel-gateway is V2-only; gateway.{legacy_key} is a forbidden Legacy field"
            );
        }
    }
    Ok(())
}

/// Public `lantunnel-gateway` startup policy.
fn validate_public_v2_gateway_config(
    config: &GatewayConfig,
    managed_runtime: bool,
) -> anyhow::Result<()> {
    if !config.auth_username.trim().is_empty() || !config.auth_password.trim().is_empty() {
        anyhow::bail!(
            "lantunnel-gateway is V2-only; gateway.auth_username/auth_password are forbidden"
        );
    }
    if config.credential.is_some() {
        anyhow::bail!("lantunnel-gateway is V2-only; gateway.credential stores are forbidden");
    }
    // Public HTTP/SOCKS5/TUIC listeners are not wired into the V2 runtime. The
    // three frontends still build — `crates/tp-proxy-{http,socks5,tuic}` — and
    // each takes a backend trait rather than reaching into the Gateway, so a
    // future release can opt them in. Two things must be settled first: V2 has
    // no shared proxy secret to authenticate against, and the Gateway no longer
    // exposes a primitive for dialing an arbitrary target on a Peer's behalf.
    // Until then, accepting the config would promise a listener that never
    // binds, so refuse it loudly instead.
    if config.proxy.http.is_some() || config.proxy.socks5.is_some() || config.proxy.tuic.is_some() {
        anyhow::bail!(
            "gateway.proxy.{{http,socks5,tuic}} listeners are not wired into the V2 runtime yet; remove the section to start"
        );
    }
    let static_scope_source = nonempty_scopes_dir(config).is_some();
    if !config.p2p.enabled && (managed_runtime || !static_scope_source) {
        anyhow::bail!(
            "lantunnel-gateway requires gateway.p2p.enabled=true outside an isolated Static Relay acceptance clone"
        );
    }
    // The mapping port used to be pinned to the shared default here. It is now
    // a registration fact: the Platform records the port this host reflects on
    // and hands it to Clients through managed resolve, so an operator whose host
    // already uses that port can move the reflector. Range and collision rules
    // still apply, and `GatewayConfig::validate` enforces them.
    if config.mapping_probe_port == 0 {
        anyhow::bail!("lantunnel-gateway requires a non-zero gateway.mapping_probe_port");
    }
    if !static_scope_source && !managed_runtime {
        anyhow::bail!(
            "lantunnel-gateway requires a V2 Scope source: gateway.scopes_dir or outbound managed control"
        );
    }
    Ok(())
}

fn validate_persistent_tls_and_scope_files(config: &GatewayConfig) -> anyhow::Result<()> {
    let (cert_path, key_path) = match (&config.tls_cert, &config.tls_key) {
        (Some(cert), Some(key)) if !cert.trim().is_empty() && !key.trim().is_empty() => {
            (cert.as_str(), key.as_str())
        }
        _ => anyhow::bail!("lantunnel-gateway requires persistent gateway.tls_cert/tls_key"),
    };
    certificate_lifecycle::load_server_identity(
        std::path::Path::new(cert_path),
        std::path::Path::new(key_path),
    )?;
    if let Some(scopes_dir) = nonempty_scopes_dir(config) {
        let outcome = tp_gateway::scope::ScopeStore::new()
            .reload_static(std::path::Path::new(scopes_dir))
            .with_context(|| format!("validate V2 scopes from {scopes_dir}"))?;
        if !config.p2p.enabled && outcome.count != 1 {
            anyhow::bail!("isolated Static Relay acceptance clone requires exactly one V2 Scope");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn spawn_scope_reload_on_sighup(gateway: Arc<Gateway>, scopes_dir: std::path::PathBuf) {
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!(%error, "cannot install SIGHUP Scope reload handler");
                return;
            }
        };
        while sighup.recv().await.is_some() {
            match gateway.scopes().reload_static(&scopes_dir) {
                Ok(outcome) => {
                    for tunnel_id in &outcome.removed_ids {
                        let disconnected = gateway.disconnect_tunnel_clients(tunnel_id);
                        if disconnected > 0 {
                            tracing::warn!(
                                tunnel_id,
                                disconnected,
                                "disconnected Gateway attachments after static Scope removal"
                            );
                        }
                    }
                    tracing::info!(
                        scopes_dir = %scopes_dir.display(),
                        count = outcome.count,
                        removed = outcome.removed_ids.len(),
                        "reloaded static V2 Gateway scopes"
                    );
                }
                Err(error) => tracing::error!(
                    scopes_dir = %scopes_dir.display(),
                    %error,
                    "rejected static Scope reload and kept last-known-good snapshot"
                ),
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_scope_reload_on_sighup(_gateway: Arc<Gateway>, _scopes_dir: std::path::PathBuf) {}

/// Block until SIGINT (Ctrl-C) on any platform, or SIGTERM on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "cannot install SIGTERM handler, SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("received SIGINT");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received Ctrl-C");
    }
}

/// Log a one-liner announcing where gateway logs are going. Runs AFTER
/// `init_logging`, so the announcement itself lands in the same sink.
/// Operators reading the stdout or file tail can then trace back to find it.
fn announce_log_sink(cfg: &tp_core::config::LogConfig) {
    match cfg.output.as_str() {
        "file" => {
            let path = cfg.file.as_deref().unwrap_or("logs/gateway.log");
            tracing::info!(
                output = "file",
                path,
                rotation = "daily",
                max_backups = cfg.max_backups,
                max_age_days = cfg.max_age,
                level = %cfg.level,
                format = %cfg.format,
                "gateway log sink"
            );
        }
        other => {
            tracing::info!(
                output = %other,
                level = %cfg.level,
                format = %cfg.format,
                "gateway log sink (no file rotation — set log.output=\"file\" + log.file=… to persist)"
            );
        }
    }
}

/// Initialize logging honoring `LogConfig`. Returns a `WorkerGuard` when file
/// rotation is active — keep it alive for the lifetime of the process.
///
/// File rotation is handled by [`tp_core::log::build_rolling_writer`], which
/// wraps `file-rotate` so every knob in `LogConfig` (`max_size` / `max_age` /
/// `max_backups` / `compress`) takes effect.
fn init_logging(
    cfg: &tp_core::config::LogConfig,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(cfg.level.as_str()));
    let json = cfg.format.eq_ignore_ascii_case("json");

    let guard = match cfg.output.as_str() {
        "file" => {
            let path = cfg.file.as_deref().unwrap_or("logs/gateway.log");
            let writer = match tp_core::log::build_rolling_writer(path, cfg) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("cannot open rolling log writer at {path}: {e}");
                    return None;
                }
            };
            // non_blocking keeps tracing hot path off the rename/gzip I/O
            // that file-rotate performs on rotation.
            let (nb, guard) = tracing_appender::non_blocking(writer);
            let builder = fmt().with_env_filter(filter).with_writer(nb);
            if json {
                builder.json().init();
            } else {
                builder.init();
            }
            if cfg.max_age > 0 && cfg.max_backups > 0 {
                tracing::warn!(
                    max_age_days = cfg.max_age,
                    max_backups = cfg.max_backups,
                    "log.max_age and log.max_backups both set — age-based pruning wins (file-rotate FileLimit is exclusive)"
                );
            }
            Some(guard)
        }
        "stderr" => {
            let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);
            if json {
                builder.json().init();
            } else {
                builder.init();
            }
            None
        }
        _ => {
            let builder = fmt().with_env_filter(filter);
            if json {
                builder.json().init();
            } else {
                builder.init();
            }
            None
        }
    };
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_service_cli_uses_the_machine_wide_fixed_udp_port() {
        let cli = Cli::try_parse_from(["lantunnel-gateway", "mapping", "serve"])
            .expect("mapping service command parses");
        let Some(GatewayCommand::Mapping(MappingCommand {
            command: MappingSubcommand::Serve(args),
        })) = cli.command
        else {
            panic!("mapping serve command was not selected");
        };

        assert_eq!(args.listen, "0.0.0.0:8444".parse().unwrap());

        let cli = Cli::try_parse_from([
            "lantunnel-gateway",
            "mapping",
            "serve",
            "--listen",
            "127.0.0.1:18444",
        ])
        .expect("isolated static test endpoint parses");
        let Some(GatewayCommand::Mapping(MappingCommand {
            command: MappingSubcommand::Serve(args),
        })) = cli.command
        else {
            panic!("mapping serve command was not selected");
        };
        assert_eq!(args.listen, "127.0.0.1:18444".parse().unwrap());
    }

    #[tokio::test]
    async fn a_gateway_serves_its_own_mapping_reflector() {
        let server =
            tp_gateway::mapping_probe::MappingProbeServer::bind("127.0.0.1:0".parse().unwrap())
                .await
                .expect("a Gateway binds its own mapping port");
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());

        // What it binds is what a Client probes, with no second process in
        // between and no startup ordering between the two.
        tp_gateway::mapping_probe::probe_local_readiness(
            address,
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("the reflector answers on the port the Gateway registered");

        // A port someone else already holds is the one honest failure left.
        let taken = tp_gateway::mapping_probe::MappingProbeServer::bind(address).await;
        assert!(
            taken.is_err(),
            "two processes must not share one mapping port"
        );

        task.abort();
    }

    #[tokio::test]
    async fn managed_gateway_never_becomes_ready_when_grpc_listener_bind_fails() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve the gRPC data port");
        let address = occupied.local_addr().unwrap();
        let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate_pem = identity.cert.pem();
        let server = GrpcServer::new(address).with_tls(
            certificate_pem.clone().into_bytes(),
            identity.key_pair.serialize_pem().into_bytes(),
        );
        let gateway = Gateway::new(Default::default(), None);
        let mut data_plane_task = tokio::spawn({
            let gateway = gateway.clone();
            async move { gateway.serve(GatewayServer::Grpc(server)).await }
        });

        let error = await_managed_data_plane_readiness(
            &mut data_plane_task,
            TRANSPORT_TYPE_GRPC,
            address,
            "localhost",
            &certificate_pem,
        )
        .await
        .expect_err("a failed gRPC bind must never become Managed-ready");

        assert!(
            error.to_string().contains("stopped before readiness"),
            "unexpected readiness error: {error:#}"
        );
    }

    #[tokio::test]
    async fn managed_gateway_readiness_rechecks_the_mapping_service() {
        let mapping =
            tp_gateway::mapping_probe::MappingProbeServer::bind("127.0.0.1:0".parse().unwrap())
                .await
                .expect("bind mapping service");
        let mapping_address = mapping.local_addr().unwrap();
        let mapping_task = tokio::spawn(mapping.run());

        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve the gRPC data port");
        let data_address = reservation.local_addr().unwrap();
        drop(reservation);
        let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate_pem = identity.cert.pem();
        let server = GrpcServer::new(data_address).with_tls(
            certificate_pem.clone().into_bytes(),
            identity.key_pair.serialize_pem().into_bytes(),
        );
        let gateway = Gateway::new(Default::default(), None);
        let data_plane_task = tokio::spawn({
            let gateway = gateway.clone();
            async move { gateway.serve(GatewayServer::Grpc(server)).await }
        });
        let readiness = managed_gateway_readiness_probe(
            TRANSPORT_TYPE_GRPC.to_string(),
            data_address,
            "localhost".into(),
            certificate_pem,
            mapping_address,
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !readiness().await {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("both local listeners become ready");
        mapping_task.abort();
        let _ = mapping_task.await;
        assert!(
            !readiness().await,
            "a stopped mapping service must clear readiness"
        );

        data_plane_task.abort();
    }

    fn test_tls_gateway() -> (tempfile::TempDir, GatewayConfig) {
        let temporary_root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-gateway-startup-")
            .tempdir_in(temporary_root)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate_path = temporary.path().join("server.crt");
        let key_path = temporary.path().join("server.key");
        std::fs::write(&certificate_path, identity.cert.pem()).unwrap();
        std::fs::write(&key_path, identity.key_pair.serialize_pem()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&certificate_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = GatewayConfig {
            tls_cert: Some(certificate_path.to_string_lossy().into_owned()),
            tls_key: Some(key_path.to_string_lossy().into_owned()),
            scopes_dir: None,
            ..static_v2_gateway()
        };
        (temporary, config)
    }

    fn static_v2_gateway() -> GatewayConfig {
        GatewayConfig {
            listen_addr: "127.0.0.1:8443".into(),
            tls_cert: Some("server.crt".into()),
            tls_key: Some("server.key".into()),
            scopes_dir: Some("scopes.d".into()),
            ..Default::default()
        }
    }

    #[test]
    fn public_gateway_accepts_static_v2_scope_source() {
        validate_public_v2_gateway_config(&static_v2_gateway(), false).expect("Static V2 Gateway");
    }

    #[test]
    fn managed_gateway_reflects_on_whichever_port_its_host_registered() {
        // The port used to be pinned to the shared default, which left an
        // operator no way off a port their host already used. It travels with
        // the registration now; only a nonsense value is refused here.
        let mut gateway = static_v2_gateway();
        gateway.mapping_probe_port = 18_444;
        validate_public_v2_gateway_config(&gateway, true)
            .expect("a Managed V2 Gateway may reflect on the port it registered");

        gateway.mapping_probe_port = 0;
        let error = validate_public_v2_gateway_config(&gateway, true)
            .expect_err("a zero mapping port is not a port");
        assert!(error
            .to_string()
            .contains("non-zero gateway.mapping_probe_port"));
    }

    #[test]
    fn managed_gateway_uses_outbound_control_without_an_inbound_management_origin() {
        let mut gateway = static_v2_gateway();
        validate_public_v2_gateway_config(&gateway, true)
            .expect("Managed V2 needs only its outbound control identity");

        gateway.p2p.enabled = false;
        let disabled_mesh = validate_public_v2_gateway_config(&gateway, true)
            .expect_err("Managed V2 Gateway cannot disable P2P signaling");
        assert!(disabled_mesh.to_string().contains("isolated Static Relay"));
    }

    #[test]
    fn public_gateway_rejects_legacy_fields_even_when_empty() {
        for legacy_field in [
            "auth_username: \"\"",
            "auth_password: \"\"",
            "credential: null",
            "proxy: {}",
            "tunnel_key: \"\"",
            "group: \"\"",
            "group_id: \"\"",
            "group_password: \"\"",
            "username: \"\"",
            "password: \"\"",
        ] {
            let raw = format!(
                "gateway:\n  listen_addr: '127.0.0.1:8443'\n  scopes_dir: scopes.d\n  {legacy_field}\n"
            );
            let error = reject_legacy_public_gateway_yaml(&raw)
                .expect_err("public Gateway must not expose a Legacy switch");
            assert!(error.to_string().contains("forbidden Legacy field"));
        }

        {
            let state_owned_field = "platform_pairing: {}";
            let raw = format!(
                "gateway:\n  listen_addr: '127.0.0.1:8443'\n  scopes_dir: scopes.d\n  {state_owned_field}\n"
            );
            let error = reject_legacy_public_gateway_yaml(&raw)
                .expect_err("managed identity must not be copied into runtime YAML");
            assert!(error.to_string().contains("state-owned"));
        }

        for obsolete_inbound_field in ["web: {}", "platform_auth: {}"] {
            let raw = format!(
                "gateway:\n  listen_addr: '127.0.0.1:8443'\n  scopes_dir: scopes.d\n  {obsolete_inbound_field}\n"
            );
            let error = reject_legacy_public_gateway_yaml(&raw)
                .expect_err("the old inbound management surface must stay removed");
            assert!(error.to_string().contains("outbound Managed V2 control"));
        }
    }

    #[test]
    fn public_gateway_rejects_yaml_merge_or_tagged_keys() {
        for raw in [
            "gateway:\n  <<: {auth_username: legacy, auth_password: secret}\n",
            "gateway:\n  !!str auth_username: legacy\n",
        ] {
            let error = reject_legacy_public_gateway_yaml(raw)
                .expect_err("public Gateway must reject ambiguous YAML key syntax");
            assert!(
                error.to_string().contains("forbidden") || error.to_string().contains("plain YAML")
            );
        }
    }

    #[test]
    fn public_gateway_requires_scope_source_and_allows_relay_only_clone() {
        let mut config = static_v2_gateway();
        config.scopes_dir = None;
        let missing_scope = validate_public_v2_gateway_config(&config, false)
            .expect_err("Gateway without Static or Managed Scope source must fail");
        assert!(missing_scope.to_string().contains("V2 Scope source"));

        config.scopes_dir = Some("scopes.d".into());
        config.p2p.enabled = false;
        validate_public_v2_gateway_config(&config, false)
            .expect("an isolated Static Gateway may disable P2P signaling for Relay acceptance");
    }

    #[test]
    fn relay_only_clone_rejects_an_empty_static_scope_directory() {
        let (temporary, mut config) = test_tls_gateway();
        let scopes = temporary.path().join("scopes.d");
        std::fs::create_dir(&scopes).unwrap();
        config.scopes_dir = Some(scopes.to_string_lossy().into_owned());
        config.p2p.enabled = false;

        let error = validate_persistent_tls_and_scope_files(&config)
            .expect_err("Relay-only clone must contain exactly one Static Scope");
        assert!(error.to_string().contains("exactly one V2 Scope"));
    }

    #[test]
    fn public_gateway_rejects_programmatic_fixed_seed_or_credential_store() {
        let mut config = static_v2_gateway();
        config.auth_username = "legacy-tunnel".into();
        config.auth_password = "legacy-key".into();
        assert!(validate_public_v2_gateway_config(&config, false).is_err());

        let mut config = static_v2_gateway();
        config.credential = Some(tp_core::config::CredentialConfig {
            kind: "memory".into(),
            file_path: None,
            db: None,
        });
        assert!(validate_public_v2_gateway_config(&config, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn persistent_tls_startup_rejects_group_or_world_accessible_identity_files() {
        use std::os::unix::fs::PermissionsExt as _;

        for field in ["certificate", "private key"] {
            let (_temporary, config) = test_tls_gateway();
            let path = if field == "certificate" {
                config.tls_cert.as_ref().unwrap()
            } else {
                config.tls_key.as_ref().unwrap()
            };
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();

            let error = validate_persistent_tls_and_scope_files(&config)
                .expect_err("startup must reject an identity file accessible by other users");

            assert!(
                error.to_string().contains("owner-only"),
                "unexpected {field} error: {error:#}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_tls_startup_rejects_a_private_key_symlink() {
        use std::os::unix::fs::symlink;

        let (temporary, mut config) = test_tls_gateway();
        let key_path = config.tls_key.as_ref().unwrap();
        let linked_key = temporary.path().join("linked-server.key");
        symlink(key_path, &linked_key).unwrap();
        config.tls_key = Some(linked_key.to_string_lossy().into_owned());

        let error = validate_persistent_tls_and_scope_files(&config)
            .expect_err("startup must reject a private-key symlink");

        assert!(error.to_string().contains("safe regular file"));
    }
}
