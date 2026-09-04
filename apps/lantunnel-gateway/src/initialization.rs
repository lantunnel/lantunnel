use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use clap::{Args, ValueEnum};
use serde::Serialize;
use tp_core::config::{
    Config, DEFAULT_GATEWAY_MAPPING_PROBE_PORT, TRANSPORT_TYPE_GRPC, TRANSPORT_TYPE_QUIC,
    TRANSPORT_TYPE_WEBSOCKET,
};

use crate::managed_identity::{is_public_ip, FileManagedIdentityStore};

const DEFAULT_DATA_PORT: u16 = 8443;
const MAX_RUNTIME_CONFIG_BYTES: usize = 128 * 1024;

#[derive(Args)]
pub(crate) struct InitArgs {
    /// Fixed public IPv4 or IPv6 address Clients use to reach this Gateway.
    #[arg(long)]
    pub(crate) public_ip: IpAddr,
    /// Data-plane transport. QUIC uses UDP; WebSocket and gRPC use TCP.
    #[arg(long, value_enum, default_value_t = IndependentTransport::Quic)]
    pub(crate) transport: IndependentTransport,
    /// Public data-plane port.
    #[arg(long, default_value_t = DEFAULT_DATA_PORT)]
    pub(crate) data_port: u16,
    /// Public UDP mapping port Clients use for NAT discovery.
    #[arg(long, default_value_t = DEFAULT_GATEWAY_MAPPING_PROBE_PORT)]
    pub(crate) mapping_port: u16,
    /// Runtime YAML to create or validate on an exact replay.
    #[arg(long, default_value = crate::onboarding::DEFAULT_RUNTIME_CONFIG_PATH)]
    pub(crate) config: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum IndependentTransport {
    Quic,
    Websocket,
    Grpc,
}

impl IndependentTransport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Quic => TRANSPORT_TYPE_QUIC,
            Self::Websocket => TRANSPORT_TYPE_WEBSOCKET,
            Self::Grpc => TRANSPORT_TYPE_GRPC,
        }
    }

    fn data_protocol(self) -> &'static str {
        match self {
            Self::Quic => "UDP",
            Self::Websocket | Self::Grpc => "TCP",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Quic => "QUIC",
            Self::Websocket => "WebSocket",
            Self::Grpc => "gRPC",
        }
    }
}

struct RuntimeLayout {
    config: PathBuf,
    config_directory: PathBuf,
    certificate_directory: PathBuf,
    certificate: PathBuf,
    private_key: PathBuf,
    state_directory: PathBuf,
    scopes_directory: PathBuf,
    relay_usage_wal: PathBuf,
}

impl RuntimeLayout {
    fn from_config_path(path: &Path) -> anyhow::Result<Self> {
        let config = std::path::absolute(path)
            .with_context(|| format!("resolve Gateway config path {}", path.display()))?;
        let config_directory = config
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Gateway config path has no parent"))?
            .to_path_buf();
        let deployment_root =
            if config_directory.file_name() == Some(std::ffi::OsStr::new("configs")) {
                config_directory
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("Gateway config path has no deployment root"))?
                    .to_path_buf()
            } else {
                config_directory.clone()
            };
        if deployment_root.parent().is_none() {
            bail!("Gateway deployment root must not be the filesystem root");
        }
        let certificate_directory = deployment_root.join("certs");
        let state_directory = deployment_root.join("state");
        Ok(Self {
            config,
            config_directory,
            certificate: certificate_directory.join("server.crt"),
            private_key: certificate_directory.join("server.key"),
            certificate_directory,
            scopes_directory: state_directory.join("scopes.d"),
            relay_usage_wal: state_directory.join("relay-usage.wal"),
            state_directory,
        })
    }
}

#[derive(Serialize)]
struct RuntimeConfig<'a> {
    log: RuntimeLog,
    gateway: RuntimeGateway<'a>,
}

#[derive(Serialize)]
struct RuntimeLog {
    level: &'static str,
    format: &'static str,
    output: &'static str,
}

#[derive(Serialize)]
struct RuntimeGateway<'a> {
    listen_addr: String,
    transport_type: &'static str,
    tls_cert: &'a str,
    tls_key: &'a str,
    scopes_dir: &'a str,
    mapping_probe_port: u16,
    p2p: RuntimeP2p,
    usage_ledger: RuntimeUsageLedger<'a>,
}

#[derive(Serialize)]
struct RuntimeP2p {
    enabled: bool,
}

#[derive(Serialize)]
struct RuntimeUsageLedger<'a> {
    wal_path: &'a str,
}

pub(crate) fn run(args: InitArgs) -> anyhow::Result<()> {
    validate_args(&args)?;
    let layout = RuntimeLayout::from_config_path(&args.config)?;
    let yaml = runtime_yaml(&args, &layout)?;
    validate_runtime_yaml(&yaml)?;

    let managed_store = FileManagedIdentityStore::from_config_path(&layout.config)?;
    reject_managed_identity(managed_store.path())?;
    let replay = inspect_runtime_config(&layout.config, yaml.as_bytes())?;
    crate::certificate_lifecycle::preflight_self_signed_ip_identity(
        &layout.certificate,
        &layout.private_key,
        args.public_ip,
    )?;

    let directories = [
        layout.config_directory.as_path(),
        layout.certificate_directory.as_path(),
        layout.state_directory.as_path(),
        layout.scopes_directory.as_path(),
    ];
    for directory in directories {
        crate::certificate_lifecycle::validate_owner_only_directory_target(directory)?;
    }
    for directory in directories {
        crate::certificate_lifecycle::create_owner_only_directory(directory)?;
    }

    if !replay {
        crate::certificate_lifecycle::create_owner_only_artifact_noclobber(
            &layout.config,
            yaml.as_bytes(),
        )
        .with_context(|| {
            format!(
                "create Independent Gateway runtime config at {}",
                layout.config.display()
            )
        })?;
    }
    let identity = crate::certificate_lifecycle::ensure_self_signed_ip_identity(
        &layout.config,
        args.public_ip,
    )?;
    if !identity.directory_durability_confirmed {
        bail!("Independent Gateway identity durability could not be confirmed");
    }

    validate_complete_runtime(&layout.config)?;
    reject_managed_identity(managed_store.path())?;

    println!(
        "Independent Gateway: {}",
        if replay {
            "already initialized"
        } else {
            "initialized"
        }
    );
    println!("Gateway runtime config: {}", layout.config.display());
    println!(
        "Private key (keep only on this host): {}",
        layout.private_key.display()
    );
    println!(
        "Public certificate (copy to the Tunnel owner): {}",
        layout.certificate.display()
    );
    println!("Certificate SHA-256: {}", identity.leaf_sha256);
    println!(
        "Static Scope directory: {}",
        layout.scopes_directory.display()
    );
    println!(
        "Open inbound: {} {} ({} data) and UDP {} (mapping)",
        args.transport.data_protocol(),
        args.data_port,
        args.transport.display_name(),
        args.mapping_port,
    );
    println!("Create the Tunnel on the trusted owner machine:");
    println!("  lantunnel-admin init-tunnel \\");
    println!("    --gateway-transport {} \\", args.transport.as_str());
    println!("    --gateway-ip {} \\", args.public_ip);
    println!("    --gateway-port {} \\", args.data_port);
    println!("    --gateway-mapping-port {} \\", args.mapping_port);
    println!("    --gateway-cert ./server.crt \\");
    println!("    --output-dir ./provision");
    println!(
        "Install only the generated .scope file in {}; never copy a .tunnel or .peer file to this host.",
        layout.scopes_directory.display()
    );
    println!(
        "Start the Gateway: lantunnel-gateway --config {}",
        layout.config.display()
    );
    Ok(())
}

fn validate_args(args: &InitArgs) -> anyhow::Result<()> {
    if !is_public_ip(args.public_ip) {
        bail!("Independent Gateway public address is not a public IP");
    }
    if args.data_port == 0 {
        bail!("Independent Gateway data port must be non-zero");
    }
    if args.mapping_port == 0 {
        bail!("Independent Gateway mapping port must be non-zero");
    }
    if args.transport == IndependentTransport::Quic && args.data_port == args.mapping_port {
        bail!(
            "Independent Gateway QUIC data port collides with mapping port {}",
            args.mapping_port
        );
    }
    if args.config.file_name().is_none() {
        bail!(
            "Gateway config path must name a file: {}",
            args.config.display()
        );
    }
    if args
        .config
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        bail!(
            "Gateway config path must not contain '..': {}",
            args.config.display()
        );
    }
    Ok(())
}

fn runtime_yaml(args: &InitArgs, layout: &RuntimeLayout) -> anyhow::Result<String> {
    let listen_ip = match args.public_ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    let listen_addr = std::net::SocketAddr::new(listen_ip, args.data_port);
    let certificate = utf8_path(&layout.certificate)?;
    let private_key = utf8_path(&layout.private_key)?;
    let scopes_directory = utf8_path(&layout.scopes_directory)?;
    let relay_usage_wal = utf8_path(&layout.relay_usage_wal)?;
    serde_yaml::to_string(&RuntimeConfig {
        log: RuntimeLog {
            level: "info",
            format: "text",
            output: "stdout",
        },
        gateway: RuntimeGateway {
            listen_addr: listen_addr.to_string(),
            transport_type: args.transport.as_str(),
            tls_cert: certificate,
            tls_key: private_key,
            scopes_dir: scopes_directory,
            mapping_probe_port: args.mapping_port,
            p2p: RuntimeP2p { enabled: true },
            usage_ledger: RuntimeUsageLedger {
                wal_path: relay_usage_wal,
            },
        },
    })
    .context("serialize Independent Gateway runtime config")
}

fn validate_runtime_yaml(yaml: &str) -> anyhow::Result<()> {
    crate::reject_legacy_public_gateway_yaml(yaml)?;
    let config: Config = tp_core::config::load_from_str(yaml)
        .context("validate generated Independent Gateway config")?;
    config.validate()?;
    let gateway = config
        .gateway
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("generated config missing [gateway] section"))?;
    crate::validate_public_v2_gateway_config(gateway, false)
}

fn validate_complete_runtime(path: &Path) -> anyhow::Result<()> {
    let yaml = fs::read_to_string(path)
        .with_context(|| format!("read Independent Gateway config at {}", path.display()))?;
    validate_runtime_yaml(&yaml)?;
    let config: Config = tp_core::config::load_from_str(&yaml)?;
    let gateway = config
        .gateway
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("config missing [gateway] section"))?;
    crate::validate_persistent_tls_and_scope_files(gateway)
}

fn inspect_runtime_config(path: &Path, expected: &[u8]) -> anyhow::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let existing = crate::certificate_lifecycle::read_required_owner_only_artifact_bounded(
                path,
                "Independent Gateway runtime config",
                MAX_RUNTIME_CONFIG_BYTES,
            )?;
            if existing != expected {
                bail!(
                    "refusing to replace existing Gateway runtime config at {}",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("inspect Gateway runtime config at {}", path.display())),
    }
}

fn utf8_path(path: &Path) -> anyhow::Result<&str> {
    path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "Gateway runtime path is not valid UTF-8: {}",
            path.display()
        )
    })
}

fn reject_managed_identity(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "Independent Gateway initialization refuses Managed identity state at {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("inspect Managed Gateway identity at {}", path.display())),
    }
}
