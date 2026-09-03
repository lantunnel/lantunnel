use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use async_trait::async_trait;
use clap::Args;
use serde::Deserialize;
use uuid::Uuid;

use crate::gateway_control::{
    validate_claim_secret, GatewayControlConnectConfig, GatewayControlKind,
};
use crate::managed_identity::{
    validate_canonical_public_ip, FileManagedIdentityStore, InitializeOutcome,
    PersistedManagedIdentity,
};

const MAX_PAIRING_ARTIFACT_BYTES: usize = 16 * 1024;
const PAIRING_ARTIFACT_VERSION: u8 = 2;

/// Where the Gateway keeps its runtime YAML when nobody names a path.
///
/// Onboarding writes it here and startup reads it from here, so an operator who
/// accepts the defaults never types a path at all.
pub(crate) const DEFAULT_RUNTIME_CONFIG_PATH: &str = "configs/gateway.yaml";

#[derive(Args)]
pub(crate) struct OnboardArgs {
    /// Runtime YAML path. Written from the pairing artifact when it does not
    /// exist yet, and validated but never rewritten when it does.
    #[arg(long, default_value = DEFAULT_RUNTIME_CONFIG_PATH)]
    pub(crate) config: PathBuf,
    /// Owner-only one-time pairing artifact downloaded from the Platform.
    #[arg(long)]
    pub(crate) pairing: PathBuf,
}

/// Version 2 carries the listener facts, so the operator no longer has to
/// hand-write a runtime YAML that agrees with what they registered.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingArtifact {
    version: u8,
    kind: GatewayControlKind,
    id: String,
    platform_url: String,
    public_ip: String,
    transport: String,
    data_port: u16,
    mapping_port: u16,
    claim_secret: String,
}

#[async_trait]
trait GatewayRegistrar: Send + Sync {
    async fn register(&self, config: GatewayControlConnectConfig) -> anyhow::Result<()>;
}

struct PlatformGatewayRegistrar;

#[async_trait]
impl GatewayRegistrar for PlatformGatewayRegistrar {
    async fn register(&self, config: GatewayControlConnectConfig) -> anyhow::Result<()> {
        crate::gateway_control::register_once(config).await
    }
}

#[derive(Debug)]
struct OnboardOutcome {
    identity: PersistedManagedIdentity,
    key_path: PathBuf,
    certificate_path: PathBuf,
    state_path: PathBuf,
    replay: bool,
}

/// Onboard this machine as the kind of Gateway its pairing artifact names.
///
/// Fleet and BYOG used to be separate subcommands that made the operator repeat
/// a fact the artifact already carries. `kind:` is in the artifact, so the
/// command reads it there instead of asking.
pub(crate) async fn run(args: OnboardArgs) -> anyhow::Result<()> {
    // The pairing artifact carries the listener facts, so an operator who has
    // no runtime YAML gets one rather than a homework assignment.
    let generated = if args.config.exists() {
        false
    } else {
        write_runtime_config(&args.config, &args.pairing)?;
        true
    };
    let outcome =
        run_with_registrar(&args.config, &args.pairing, &PlatformGatewayRegistrar).await?;
    let kind = outcome.identity.kind;
    println!(
        "Gateway runtime config: {} ({})",
        args.config.display(),
        if generated { "written" } else { "existing" },
    );
    println!("Gateway kind: {}", kind.as_str());
    println!("Gateway ID: {}", outcome.identity.id);
    println!("Public IP: {}", outcome.identity.public_ip);
    println!("Private key: {}", outcome.key_path.display());
    println!("Certificate: {}", outcome.certificate_path.display());
    println!("Leaf SHA-256: {}", outcome.identity.certificate_leaf_sha256);
    println!("SPKI SHA-256: {}", outcome.identity.certificate_spki_sha256);
    println!("Managed identity: {}", outcome.state_path.display());
    println!(
        "Gateway onboarding: {}",
        if outcome.replay {
            "already registered"
        } else {
            "registered"
        }
    );
    println!(
        "Start the Gateway: lantunnel-gateway --config {}",
        args.config.display()
    );
    Ok(())
}

async fn run_with_registrar(
    config_path: &Path,
    pairing_path: &Path,
    registrar: &dyn GatewayRegistrar,
) -> anyhow::Result<OnboardOutcome> {
    validate_runtime_config(config_path)?;
    let store = FileManagedIdentityStore::from_config_path(config_path)?;
    if let Some(identity) = store.load()? {
        let certificate = crate::certificate_lifecycle::load_self_signed_ip_identity(
            config_path,
            identity.public_ip,
        )?;
        if certificate.leaf_sha256 != identity.certificate_leaf_sha256
            || certificate.spki_sha256 != identity.certificate_spki_sha256
        {
            bail!("persisted Managed Gateway certificate identity changed");
        }
        remove_replayed_pairing_if_present(pairing_path, &identity)?;
        return Ok(OnboardOutcome {
            identity,
            key_path: certificate.key_path,
            certificate_path: certificate.certificate_path,
            state_path: store.path().to_path_buf(),
            replay: true,
        });
    }

    let artifact = load_pairing_artifact(pairing_path)?;
    let public_ip = artifact.validate()?;
    let certificate =
        crate::certificate_lifecycle::ensure_self_signed_ip_identity(config_path, public_ip)?;
    if !certificate.directory_durability_confirmed {
        bail!("Gateway identity durability could not be confirmed; WSS registration refused");
    }
    let loaded = crate::certificate_lifecycle::load_server_identity(
        &certificate.certificate_path,
        &certificate.key_path,
    )?;
    registrar
        .register(GatewayControlConnectConfig {
            kind: artifact.kind,
            gateway_id: artifact.id.clone(),
            platform_url: artifact.platform_url.clone(),
            boot_id: Uuid::new_v4().hyphenated().to_string(),
            leaf_sha256: certificate.leaf_sha256.clone(),
            private_key_pem: loaded.private_key_pem,
            certificate_pem: Some(loaded.certificate_pem),
            claim_secret: Some(artifact.claim_secret),
        })
        .await
        .context("register Managed Gateway over outbound WSS")?;

    let identity = PersistedManagedIdentity {
        version: 1,
        kind: artifact.kind,
        id: artifact.id,
        platform_url: artifact.platform_url,
        public_ip,
        certificate_leaf_sha256: certificate.leaf_sha256,
        certificate_spki_sha256: certificate.spki_sha256,
    };
    let replay = store.initialize(&identity)? == InitializeOutcome::AlreadyPresent;
    crate::certificate_lifecycle::remove_owner_only_artifact(
        pairing_path,
        "one-time Managed Gateway pairing artifact",
    )?;
    Ok(OnboardOutcome {
        identity,
        key_path: certificate.key_path,
        certificate_path: certificate.certificate_path,
        state_path: store.path().to_path_buf(),
        replay,
    })
}

/// Write the runtime YAML the pairing artifact describes.
///
/// Never overwrites: a Gateway that already has a configuration also has an
/// identity and a certificate keyed to it, and silently replacing that file is
/// how an operator loses both. The caller only reaches this when the path is
/// free.
fn write_runtime_config(target: &Path, pairing_path: &Path) -> anyhow::Result<()> {
    if target.exists() {
        bail!(
            "Gateway runtime config already exists at {}",
            target.display()
        );
    }
    let artifact = load_pairing_artifact(pairing_path)?;
    artifact.validate()?;
    if let Some(parent) = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create Gateway config directory {}", parent.display()))?;
        // The private key and the leaf are written beside this file, and the
        // certificate lifecycle refuses a directory anyone but the owner can
        // read. Creating it under the ambient umask left it group-readable and
        // failed onboarding after the config had already been written.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).with_context(
                || format!("restrict Gateway config directory {}", parent.display()),
            )?;
        }
    }
    std::fs::write(target, artifact.runtime_yaml(target))
        .with_context(|| format!("write Gateway runtime config at {}", target.display()))
}

fn load_pairing_artifact(path: &Path) -> anyhow::Result<PairingArtifact> {
    let raw = crate::certificate_lifecycle::read_required_owner_only_artifact_bounded(
        path,
        "one-time Managed Gateway pairing artifact",
        MAX_PAIRING_ARTIFACT_BYTES,
    )?;
    serde_yaml::from_slice(&raw).context("parse strict Managed Gateway pairing artifact")
}

impl PairingArtifact {
    fn validate(&self) -> anyhow::Result<std::net::IpAddr> {
        if self.version != PAIRING_ARTIFACT_VERSION {
            bail!("unsupported Managed Gateway pairing artifact version");
        }
        self.kind.validate_gateway_id(&self.id)?;
        if self.platform_url != "https://lantunnel.app" {
            bail!("Managed Gateway pairing Platform URL must be https://lantunnel.app");
        }
        match self.transport.as_str() {
            "quic" | "websocket" | "grpc" => {}
            _ => bail!("Managed Gateway pairing transport must be quic, websocket or grpc"),
        }
        if self.data_port == 0 || self.mapping_port == 0 {
            bail!("Managed Gateway pairing ports must be non-zero");
        }
        // Only a UDP data plane can collide with the UDP mapping socket.
        if self.transport == "quic" && self.data_port == self.mapping_port {
            bail!("Managed Gateway pairing QUIC data port collides with its mapping port");
        }
        validate_claim_secret(&self.claim_secret)?;
        validate_canonical_public_ip(&self.public_ip)
    }

    /// The smallest runtime configuration that serves this registration.
    ///
    /// Certificate and key live beside the config, which is where
    /// `ensure_self_signed_ip_identity` creates them.
    fn runtime_yaml(&self, config_path: &Path) -> String {
        let directory = config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let certificate = directory.join("gateway.crt");
        let key = directory.join("gateway.key");
        [
            "log:".to_string(),
            "  level: info".to_string(),
            "  format: text".to_string(),
            "  output: stdout".to_string(),
            String::new(),
            "gateway:".to_string(),
            format!("  listen_addr: \"0.0.0.0:{}\"", self.data_port),
            format!("  transport_type: {}", self.transport),
            format!("  tls_cert: \"{}\"", certificate.display()),
            format!("  tls_key: \"{}\"", key.display()),
            format!("  mapping_probe_port: {}", self.mapping_port),
            "  p2p:".to_string(),
            "    enabled: true".to_string(),
            String::new(),
        ]
        .join("\n")
    }
}

fn validate_runtime_config(path: &Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read Gateway runtime config at {}", path.display()))?;
    crate::reject_legacy_public_gateway_yaml(&raw)?;
    let config: tp_core::config::Config =
        tp_core::config::load_from_str(&raw).context("parse Gateway runtime config")?;
    config.validate()?;
    let gateway = config
        .gateway
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("config missing [gateway] section"))?;
    if gateway.tls_cert.as_deref().is_none_or(str::is_empty)
        || gateway.tls_key.as_deref().is_none_or(str::is_empty)
    {
        bail!("Managed Gateway onboarding requires gateway.tls_cert and gateway.tls_key");
    }
    crate::validate_public_v2_gateway_config(gateway, true)
}

fn remove_replayed_pairing_if_present(
    path: &Path,
    identity: &PersistedManagedIdentity,
) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let artifact = load_pairing_artifact(path)?;
            let public_ip = artifact.validate()?;
            if artifact.kind != identity.kind
                || artifact.id != identity.id
                || artifact.platform_url != identity.platform_url
                || public_ip != identity.public_ip
            {
                bail!("replayed pairing artifact does not match the durable Gateway identity");
            }
            crate::certificate_lifecycle::remove_owner_only_artifact(
                path,
                "one-time Managed Gateway pairing artifact",
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("inspect replayed Managed Gateway pairing artifact"),
    }
}

// Test fixtures in this file use 1.1.1.1 rather than an RFC 5737
// documentation address on purpose: the code under test validates that a
// Gateway address is globally routable, and `Ipv4Addr::is_documentation`
// rejects 192.0.2.0/24, 198.51.100.0/24, and 203.0.113.0/24. Elsewhere in the
// workspace the documentation ranges are the right choice.
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FakeRegistrar {
        calls: AtomicUsize,
        expected_kind: GatewayControlKind,
        expected_id: &'static str,
    }

    #[async_trait]
    impl GatewayRegistrar for FakeRegistrar {
        async fn register(&self, config: GatewayControlConnectConfig) -> anyhow::Result<()> {
            assert_eq!(config.kind, self.expected_kind);
            assert_eq!(config.gateway_id, self.expected_id);
            assert_eq!(config.platform_url, "https://lantunnel.app");
            assert_eq!(
                config.claim_secret.as_deref(),
                Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            );
            assert!(config
                .certificate_pem
                .as_deref()
                .is_some_and(|pem| { pem.starts_with("-----BEGIN CERTIFICATE-----") }));
            assert!(!config.private_key_pem.is_empty());
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn assert_pairing_claim_is_sent_once(kind: GatewayControlKind, id: &'static str) {
        let temporary_root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-byog-onboard-")
            .tempdir_in(temporary_root)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let config = temporary.path().join("gateway.yaml");
        let certificate = temporary.path().join("gateway.crt");
        let key = temporary.path().join("gateway.key");
        std::fs::write(
            &config,
            format!(
                "gateway:\n  listen_addr: 0.0.0.0:10200\n  tls_cert: {}\n  tls_key: {}\n",
                certificate.display(),
                key.display()
            ),
        )
        .unwrap();
        let pairing = temporary.path().join("pairing.yaml");
        std::fs::write(
            &pairing,
            format!(
                "version: 2\nkind: {}\nid: {id}\nplatform_url: https://lantunnel.app\npublic_ip: 1.1.1.1\ntransport: quic\ndata_port: 10200\nmapping_port: 10444\nclaim_secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
                kind.as_str()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&pairing, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let registrar = FakeRegistrar {
            calls: AtomicUsize::new(0),
            expected_kind: kind,
            expected_id: id,
        };

        let first = run_with_registrar(&config, &pairing, &registrar)
            .await
            .unwrap();
        assert!(!first.replay);
        assert!(!pairing.exists());
        assert_eq!(registrar.calls.load(Ordering::SeqCst), 1);
        let state = std::fs::read_to_string(first.state_path).unwrap();
        assert!(!state.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(!state.contains("claim"));

        let second = run_with_registrar(&config, &pairing, &registrar)
            .await
            .unwrap();
        assert!(second.replay);
        assert_eq!(registrar.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn byog_pairing_claim_is_one_time_only() {
        assert_pairing_claim_is_sent_once(
            GatewayControlKind::Byog,
            "018f0c20-7b64-7a29-9bd1-6e4a598237d1",
        )
        .await;
    }

    #[tokio::test]
    async fn fleet_pairing_claim_uses_the_same_one_time_contract() {
        assert_pairing_claim_is_sent_once(GatewayControlKind::Fleet, "0123456789abcdefghijk").await;
    }

    #[test]
    fn onboarding_writes_a_runtime_the_onboarder_accepts() {
        let temporary_root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-onboard-config-")
            .tempdir_in(temporary_root)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let pairing = temporary.path().join("pairing.yaml");
        std::fs::write(
            &pairing,
            "version: 2\nkind: byog\nid: 018f6e84-e11b-7f3a-8cad-9f68f4481001\n\
             platform_url: https://lantunnel.app\npublic_ip: 1.1.1.1\n\
             transport: quic\ndata_port: 10200\nmapping_port: 10444\n\
             claim_secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&pairing, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let config = temporary.path().join("nested").join("gateway.yaml");
        write_runtime_config(&config, &pairing).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // The leaf and its private key land here, so the directory has to be
            // owner-only before the certificate lifecycle will write into it.
            let mode = std::fs::metadata(config.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "config directory must be owner-only");
        }

        let raw = std::fs::read_to_string(&config).unwrap();
        let parsed: tp_core::config::Config = tp_core::config::load_from_str(&raw).unwrap();
        let gateway = parsed.gateway.expect("gateway section");
        assert_eq!(gateway.listen_addr, "0.0.0.0:10200");
        assert_eq!(gateway.transport_type, "quic");
        assert_eq!(gateway.mapping_probe_port, 10_444);
        assert!(gateway.p2p.enabled);
        // The certificate lifecycle creates these beside the config.
        assert!(gateway.tls_cert.unwrap().ends_with("gateway.crt"));
        assert!(gateway.tls_key.unwrap().ends_with("gateway.key"));

        // A second run must not silently replace a live Gateway's identity.
        let error = write_runtime_config(&config, &pairing).unwrap_err();
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn a_version_1_pairing_artifact_is_refused() {
        let temporary_root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-onboard-legacy-")
            .tempdir_in(temporary_root)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let pairing = temporary.path().join("pairing.yaml");
        std::fs::write(
            &pairing,
            "version: 1\nkind: byog\nid: 018f6e84-e11b-7f3a-8cad-9f68f4481001\n\
             platform_url: https://lantunnel.app\npublic_ip: 1.1.1.1\n\
             claim_secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&pairing, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let error = format!("{:#}", load_pairing_artifact(&pairing).unwrap_err());
        assert!(
            error.contains("missing field") && error.contains("transport"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_quic_data_port_may_not_be_the_mapping_port() {
        let artifact = PairingArtifact {
            version: 2,
            kind: GatewayControlKind::Byog,
            id: "018f6e84-e11b-7f3a-8cad-9f68f4481001".into(),
            platform_url: "https://lantunnel.app".into(),
            public_ip: "1.1.1.1".into(),
            transport: "quic".into(),
            data_port: 10_444,
            mapping_port: 10_444,
            claim_secret: "A".repeat(43),
        };
        let error = artifact.validate().unwrap_err();
        assert!(error.to_string().contains("collides with its mapping port"));

        // A TCP data plane is a different socket, so the number may repeat.
        let websocket = PairingArtifact {
            transport: "websocket".into(),
            ..artifact
        };
        websocket.validate().unwrap();
    }
}
