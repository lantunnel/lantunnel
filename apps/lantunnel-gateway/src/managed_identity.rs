use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

use crate::gateway_control::{validate_sha256, GatewayControlKind};

const MANAGED_IDENTITY_VERSION: u8 = 1;
const MAX_MANAGED_IDENTITY_BYTES: usize = 16 * 1024;
const PLATFORM_URL: &str = "https://lantunnel.app";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedManagedIdentity {
    pub(crate) version: u8,
    pub(crate) kind: GatewayControlKind,
    pub(crate) id: String,
    pub(crate) platform_url: String,
    pub(crate) public_ip: IpAddr,
    pub(crate) certificate_leaf_sha256: String,
    pub(crate) certificate_spki_sha256: String,
}

impl PersistedManagedIdentity {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        if self.version != MANAGED_IDENTITY_VERSION {
            bail!("unsupported Managed Gateway identity version");
        }
        self.kind.validate_gateway_id(&self.id)?;
        if self.platform_url != PLATFORM_URL {
            bail!("Managed Gateway Platform URL must be {PLATFORM_URL}");
        }
        validate_public_ip(self.public_ip)?;
        validate_sha256(
            "Managed Gateway certificate leaf",
            &self.certificate_leaf_sha256,
        )?;
        validate_sha256(
            "Managed Gateway certificate SPKI",
            &self.certificate_spki_sha256,
        )
    }
}

pub(crate) struct FileManagedIdentityStore {
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitializeOutcome {
    Created,
    AlreadyPresent,
}

impl FileManagedIdentityStore {
    pub(crate) fn from_config_path(config_path: &Path) -> anyhow::Result<Self> {
        if config_path.file_name().is_none() {
            bail!(
                "Gateway config path must name a file: {}",
                config_path.display()
            );
        }
        let mut path = OsString::from(config_path.as_os_str());
        path.push(".managed-identity.json");
        Ok(Self {
            path: PathBuf::from(path),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> anyhow::Result<Option<PersistedManagedIdentity>> {
        let Some(contents) = crate::certificate_lifecycle::read_optional_owner_only_artifact(
            &self.path,
            "Managed Gateway identity",
        )?
        else {
            return Ok(None);
        };
        if contents.len() > MAX_MANAGED_IDENTITY_BYTES {
            bail!("Managed Gateway identity exceeds {MAX_MANAGED_IDENTITY_BYTES} bytes");
        }
        let identity: PersistedManagedIdentity =
            serde_json::from_slice(&contents).context("parse strict Managed Gateway identity")?;
        identity.validate()?;
        Ok(Some(identity))
    }

    pub(crate) fn initialize(
        &self,
        identity: &PersistedManagedIdentity,
    ) -> anyhow::Result<InitializeOutcome> {
        identity.validate()?;
        if let Some(existing) = self.load()? {
            if existing == *identity {
                return Ok(InitializeOutcome::AlreadyPresent);
            }
            bail!("Managed Gateway identity is already initialized with different facts");
        }
        let mut serialized =
            serde_json::to_vec_pretty(identity).context("serialize Managed Gateway identity")?;
        serialized.push(b'\n');
        crate::certificate_lifecycle::create_owner_only_artifact_noclobber(&self.path, &serialized)
            .with_context(|| {
                format!(
                    "initialize Managed Gateway identity at {}",
                    self.path.display()
                )
            })?;
        Ok(InitializeOutcome::Created)
    }
}

pub(crate) fn validate_canonical_public_ip(value: &str) -> anyhow::Result<IpAddr> {
    let address: IpAddr = value
        .parse()
        .map_err(|_| anyhow::anyhow!("Managed Gateway public address is not an IP"))?;
    if address.to_string() != value {
        bail!("Managed Gateway public address is not a canonical IP");
    }
    validate_public_ip(address)?;
    Ok(address)
}

fn validate_public_ip(address: IpAddr) -> anyhow::Result<()> {
    if !is_public_ip(address) {
        bail!("Managed Gateway public address is not a public IP");
    }
    Ok(())
}

pub(crate) fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, d] = address.octets();
    !(a == 0
        || address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0 && d != 9 && d != 10)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (18..=19).contains(&b))
        || a >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if address.to_ipv4_mapped().is_some() {
        return false;
    }
    if matches!(address.segments(), [0x2001, 0x0db8, ..])
        || ipv6_has_prefix(
            address,
            Ipv6Addr::new(0x2620, 0x004f, 0x8000, 0, 0, 0, 0, 0),
            48,
        )
    {
        return false;
    }
    // Independent and Managed Gateways need an ordinary host address, not a
    // protocol anycast/translation prefix. These are the current IANA
    // allocations to regional registries; unallocated and special-purpose
    // Global Unicast space is rejected until it becomes a host allocation.
    // Source: IANA IPv6 Global Unicast Address Space registry.
    PUBLIC_GATEWAY_IPV6_PREFIXES
        .iter()
        .any(|(network, prefix_len)| ipv6_has_prefix(address, *network, *prefix_len))
}

const PUBLIC_GATEWAY_IPV6_PREFIXES: &[(Ipv6Addr, u32)] = &[
    (Ipv6Addr::new(0x2001, 0x0200, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x0400, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x0600, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x0800, 0, 0, 0, 0, 0, 0), 22),
    (Ipv6Addr::new(0x2001, 0x0c00, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x0e00, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x1200, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x1400, 0, 0, 0, 0, 0, 0), 22),
    (Ipv6Addr::new(0x2001, 0x1800, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x1a00, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x1c00, 0, 0, 0, 0, 0, 0), 22),
    (Ipv6Addr::new(0x2001, 0x2000, 0, 0, 0, 0, 0, 0), 19),
    (Ipv6Addr::new(0x2001, 0x4000, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x4200, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x4400, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x4600, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x4800, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x4a00, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x4c00, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x5000, 0, 0, 0, 0, 0, 0), 20),
    (Ipv6Addr::new(0x2001, 0x8000, 0, 0, 0, 0, 0, 0), 19),
    (Ipv6Addr::new(0x2001, 0xa000, 0, 0, 0, 0, 0, 0), 20),
    (Ipv6Addr::new(0x2001, 0xb000, 0, 0, 0, 0, 0, 0), 20),
    (Ipv6Addr::new(0x2003, 0, 0, 0, 0, 0, 0, 0), 18),
    (Ipv6Addr::new(0x2400, 0, 0, 0, 0, 0, 0, 0), 11),
    (Ipv6Addr::new(0x2600, 0, 0, 0, 0, 0, 0, 0), 12),
    (Ipv6Addr::new(0x2610, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2620, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2630, 0, 0, 0, 0, 0, 0, 0), 12),
    (Ipv6Addr::new(0x2800, 0, 0, 0, 0, 0, 0, 0), 12),
    (Ipv6Addr::new(0x2a00, 0, 0, 0, 0, 0, 0, 0), 11),
    (Ipv6Addr::new(0x2c00, 0, 0, 0, 0, 0, 0, 0), 12),
];

fn ipv6_has_prefix(address: Ipv6Addr, network: Ipv6Addr, prefix_len: u32) -> bool {
    let shift = Ipv6Addr::BITS - prefix_len;
    u128::from_be_bytes(address.octets()) >> shift == u128::from_be_bytes(network.octets()) >> shift
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, Ipv4Addr};

    use crate::gateway_control::GatewayControlKind;

    use super::*;

    #[test]
    fn public_ip_classification_accepts_only_ordinary_public_host_addresses() {
        let cases = [
            ("2606:4700:4700::1111", true),
            ("2001:4860:4860::8888", true),
            ("240e::1", true),
            ("2a01::1", true),
            ("::ffff:8.8.8.8", false),
            ("::", false),
            ("::1", false),
            ("::2", false),
            ("fe80::1", false),
            ("fc00::1", false),
            ("fec0::1", false),
            ("ff02::1", false),
            ("2001:db8::1", false),
            ("2001:2::1", false),
            ("2620:4f:8000::1", false),
            ("3fff::1", false),
            ("2420::1", false),
            ("2a20::1", false),
            ("64:ff9b::808:808", false),
            ("::ffff:10.0.0.1", false),
            ("::ffff:192.0.2.1", false),
            ("192.0.0.1", false),
            ("192.0.0.9", true),
            ("192.88.99.1", false),
        ];

        for (address, expected) in cases {
            let address: IpAddr = address.parse().unwrap();
            assert_eq!(is_public_ip(address), expected, "address: {address}");
        }
    }

    #[test]
    fn durable_managed_identity_is_secret_free_and_exact_replay_is_idempotent() {
        let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-managed-identity-")
            .tempdir_in(temporary_root)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config_path = temporary.path().join("gateway.yaml");
        fs::write(&config_path, "gateway: {}\n").unwrap();
        let store = FileManagedIdentityStore::from_config_path(&config_path).unwrap();
        let identity = PersistedManagedIdentity {
            version: 1,
            kind: GatewayControlKind::Byog,
            id: "018f0c20-7b64-7a29-9bd1-6e4a598237d1".into(),
            platform_url: "https://lantunnel.app".into(),
            public_ip: IpAddr::V4(Ipv4Addr::new(47, 107, 181, 88)),
            certificate_leaf_sha256:
                "1111111111111111111111111111111111111111111111111111111111111111".into(),
            certificate_spki_sha256:
                "2222222222222222222222222222222222222222222222222222222222222222".into(),
        };

        assert_eq!(
            store.initialize(&identity).unwrap(),
            InitializeOutcome::Created
        );
        assert_eq!(
            store.initialize(&identity).unwrap(),
            InitializeOutcome::AlreadyPresent
        );
        assert_eq!(store.load().unwrap(), Some(identity));
        let stored = fs::read_to_string(store.path()).unwrap();
        assert!(!stored.contains("claim"));
        assert!(!stored.contains("hostname"));
        assert!(!stored.contains("management"));
        assert!(!stored.contains("csr"));
        assert!(!stored.contains("origin"));
        assert_eq!(
            store.path(),
            temporary.path().join("gateway.yaml.managed-identity.json")
        );
    }
}
