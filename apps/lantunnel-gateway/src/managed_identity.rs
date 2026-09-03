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
    let public = match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    };
    if !public {
        bail!("Managed Gateway public address is not a public IP");
    }
    Ok(())
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    !(a == 0
        || address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
        || (a == 100 && (64..=127).contains(&b))
        || (a == 198 && (18..=19).contains(&b))
        || a >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let first = address.segments()[0];
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address.is_unicast_link_local()
        || first & 0xfe00 == 0xfc00
        || (address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, Ipv4Addr};

    use crate::gateway_control::GatewayControlKind;

    use super::*;

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
