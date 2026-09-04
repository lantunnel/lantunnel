use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, SanType, PKCS_ECDSA_P256_SHA256,
};
use sha2::{Digest as _, Sha256};
use x509_parser::extensions::GeneralName;
use x509_parser::parse_x509_certificate;

pub(crate) struct LoadedServerIdentity {
    pub(crate) certificate_pem: String,
    pub(crate) private_key_pem: Vec<u8>,
    pub(crate) certificates: Vec<rustls::pki_types::CertificateDer<'static>>,
    pub(crate) private_key: rustls::pki_types::PrivateKeyDer<'static>,
}

pub(crate) struct SelfSignedIpIdentityOutcome {
    pub(crate) certificate_path: PathBuf,
    pub(crate) key_path: PathBuf,
    pub(crate) leaf_sha256: String,
    pub(crate) spki_sha256: String,
    pub(crate) directory_durability_confirmed: bool,
}

/// Ensure a Gateway has one persistent P-256 identity whose self-signed leaf
/// names exactly its public IP. Independent initialization and Managed
/// onboarding share this machine-local lifecycle. Existing material is
/// validated and reused byte-for-byte; it is never silently replaced.
pub(crate) fn ensure_self_signed_ip_identity(
    config_path: &Path,
    public_ip: IpAddr,
) -> anyhow::Result<SelfSignedIpIdentityOutcome> {
    let (certificate_path, key_path) = configured_tls_paths(config_path)?;
    require_distinct_tls_targets(&certificate_path, &key_path)?;

    let certificate_exists = existing_regular_target(&certificate_path, "Gateway certificate")?;
    let (key, key_directory_synced) = if certificate_exists {
        // A certificate without its original private key cannot be repaired
        // safely. Loading instead of creating preserves that failure mode.
        (load_existing_key(&key_path)?, true)
    } else {
        load_or_create_key(&key_path)?
    };

    let (certificate_pem, certificate_directory_synced) = if certificate_exists {
        let certificate_pem = read_required_owner_only_artifact_bounded(
            &certificate_path,
            "Gateway self-signed certificate",
            128 * 1024,
        )?;
        (
            String::from_utf8(certificate_pem)
                .context("Gateway self-signed certificate is not UTF-8 PEM")?,
            true,
        )
    } else {
        let mut params = CertificateParams::new(Vec::<String>::new())
            .context("create self-signed Gateway certificate parameters")?;
        params.subject_alt_names = vec![SanType::IpAddress(public_ip)];
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, public_ip.to_string());
        params.distinguished_name = distinguished_name;
        let certificate_pem = params
            .self_signed(&key)
            .context("self-sign Gateway public-IP certificate")?
            .pem();
        validate_self_signed_ip_certificate(&certificate_pem, &key, public_ip)?;
        let directory_synced =
            create_owner_only_noclobber(&certificate_path, certificate_pem.as_bytes())
                .with_context(|| {
                    format!(
                        "create Gateway certificate at {}",
                        certificate_path.display()
                    )
                })?;
        (certificate_pem, directory_synced)
    };

    let (leaf_sha256, spki_sha256) =
        validate_self_signed_ip_certificate(&certificate_pem, &key, public_ip)?;
    Ok(SelfSignedIpIdentityOutcome {
        certificate_path,
        key_path,
        leaf_sha256,
        spki_sha256,
        directory_durability_confirmed: key_directory_synced && certificate_directory_synced,
    })
}

/// Validate any existing Independent Gateway identity before its runtime
/// config is persisted. This keeps deterministic identity failures from
/// leaving behind a new config that points at incompatible machine state.
pub(crate) fn preflight_self_signed_ip_identity(
    certificate_path: &Path,
    key_path: &Path,
    public_ip: IpAddr,
) -> anyhow::Result<()> {
    require_distinct_tls_targets(certificate_path, key_path)?;
    let certificate_exists = existing_regular_target(certificate_path, "Gateway certificate")?;
    let key_exists = existing_regular_target(key_path, "Gateway private key")?;
    if !certificate_exists && !key_exists {
        return Ok(());
    }

    let key = load_existing_key(key_path)?;
    if !certificate_exists {
        return Ok(());
    }
    let certificate_pem = read_required_owner_only_artifact_bounded(
        certificate_path,
        "Gateway self-signed certificate",
        128 * 1024,
    )?;
    let certificate_pem = String::from_utf8(certificate_pem)
        .context("Gateway self-signed certificate is not UTF-8 PEM")?;
    validate_self_signed_ip_certificate(&certificate_pem, &key, public_ip)?;
    Ok(())
}

pub(crate) fn load_self_signed_ip_identity(
    config_path: &Path,
    public_ip: IpAddr,
) -> anyhow::Result<SelfSignedIpIdentityOutcome> {
    let (certificate_path, key_path) = configured_tls_paths(config_path)?;
    require_distinct_tls_targets(&certificate_path, &key_path)?;
    let key = load_existing_key(&key_path)?;
    let certificate_pem = read_required_owner_only_artifact_bounded(
        &certificate_path,
        "Gateway self-signed certificate",
        128 * 1024,
    )?;
    let certificate_pem = String::from_utf8(certificate_pem)
        .context("Gateway self-signed certificate is not UTF-8 PEM")?;
    let (leaf_sha256, spki_sha256) =
        validate_self_signed_ip_certificate(&certificate_pem, &key, public_ip)?;
    Ok(SelfSignedIpIdentityOutcome {
        certificate_path,
        key_path,
        leaf_sha256,
        spki_sha256,
        directory_durability_confirmed: true,
    })
}

fn validate_self_signed_ip_certificate(
    certificate_pem: &str,
    key: &KeyPair,
    public_ip: IpAddr,
) -> anyhow::Result<(String, String)> {
    let certificates = tp_transport::tls::parse_certs(certificate_pem.as_bytes())
        .context("parse Gateway self-signed certificate")?;
    if certificates.len() != 1 {
        bail!("Gateway self-signed identity must contain exactly one leaf certificate");
    }
    let leaf = certificates
        .first()
        .expect("the certificate count was checked above");
    let (remainder, parsed) = parse_x509_certificate(leaf.as_ref())
        .map_err(|_| anyhow::anyhow!("parse Gateway self-signed leaf certificate"))?;
    if !remainder.is_empty() {
        bail!("Gateway self-signed leaf contains trailing DER data");
    }
    if parsed.subject() != parsed.issuer() {
        bail!("Gateway certificate is not self-issued");
    }
    if !parsed.validity().is_valid() {
        bail!("Gateway self-signed certificate is not currently valid");
    }
    parsed
        .verify_signature(None)
        .map_err(|_| anyhow::anyhow!("Gateway certificate self-signature is invalid"))?;
    let subject_alt_name = parsed
        .subject_alternative_name()
        .map_err(|_| anyhow::anyhow!("parse Gateway self-signed leaf subjectAltName"))?
        .ok_or_else(|| anyhow::anyhow!("Gateway self-signed leaf has no subjectAltName"))?;
    let expected_ip = match public_ip {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    };
    if subject_alt_name.value.general_names.len() != 1
        || !matches!(
            subject_alt_name.value.general_names.first(),
            Some(GeneralName::IPAddress(bytes)) if *bytes == expected_ip
        )
    {
        bail!("Gateway self-signed leaf SAN must be exactly {public_ip}");
    }
    if parsed.public_key().raw != key.public_key_der() {
        bail!("Gateway self-signed leaf SPKI does not match the machine-local private key");
    }
    let leaf_sha256 = hex::encode(Sha256::digest(leaf.as_ref()));
    let spki_sha256 = hex::encode(Sha256::digest(key.public_key_der()));
    let rustls_key = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into();
    tp_transport::tls::server_config(certificates, rustls_key)
        .context("rustls rejected the Gateway self-signed certificate and keypair")?;
    Ok((leaf_sha256, spki_sha256))
}

pub(crate) fn load_server_identity(
    certificate_path: &Path,
    key_path: &Path,
) -> anyhow::Result<LoadedServerIdentity> {
    let private_key_pem = load_server_identity_key_pem(key_path)?;
    require_distinct_tls_targets(certificate_path, key_path)?;
    let certificate_metadata = fs::symlink_metadata(certificate_path).with_context(|| {
        format!(
            "inspect Gateway certificate at {}",
            certificate_path.display()
        )
    })?;
    reject_link_or_non_file(certificate_path, &certificate_metadata)?;
    require_owner_only_file(
        certificate_path,
        &certificate_metadata,
        "Gateway certificate",
    )?;
    let certificate_pem = read_regular_file(certificate_path)
        .with_context(|| format!("read Gateway certificate at {}", certificate_path.display()))?;
    let certificate_pem =
        String::from_utf8(certificate_pem).context("Gateway certificate is not UTF-8 PEM")?;
    let certificates = tp_transport::tls::parse_certs(certificate_pem.as_bytes())
        .context("parse persistent Gateway certificate")?;
    let private_key = tp_transport::tls::parse_private_key(&private_key_pem)
        .context("parse persistent Gateway private key")?;
    tp_transport::tls::server_config(certificates.clone(), private_key.clone_key())
        .context("validate persistent Gateway TLS keypair")?;
    Ok(LoadedServerIdentity {
        certificate_pem,
        private_key_pem,
        certificates,
        private_key,
    })
}

/// Read an optional machine-private artifact without following links. This is
/// shared by non-certificate lifecycle state, while certificate behavior stays
/// on its existing stricter owner-only-directory path.
pub(crate) fn read_optional_owner_only_artifact(
    path: &Path,
    description: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let _directory_guard = require_trusted_artifact_directory(parent_dir(path))?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {description}")),
    };
    reject_link_or_non_file(path, &metadata)?;
    require_owner_only_file(path, &metadata, description)?;
    require_single_file_link(path, &metadata, description)?;
    read_regular_file(path).map(Some)
}

pub(crate) fn read_required_owner_only_artifact_bounded(
    path: &Path,
    description: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    let _directory_guard = require_trusted_artifact_directory(parent_dir(path))?;
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {description}"))?;
    reject_link_or_non_file(path, &metadata)?;
    require_owner_only_file(path, &metadata, description)?;
    require_single_file_link(path, &metadata, description)?;
    if metadata.len() > max_bytes as u64 {
        bail!("{description} exceeds {max_bytes} bytes");
    }
    read_regular_file_bounded(path, description, max_bytes)
}

pub(crate) fn create_owner_only_artifact_noclobber(
    path: &Path,
    contents: &[u8],
) -> anyhow::Result<()> {
    let parent = parent_dir(path);
    let _directory_guard = require_trusted_artifact_directory(parent)?;
    require_absent(path)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary artifact in {}", parent.display()))?;
    set_owner_only_file(temporary.as_file(), temporary.path())?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("create {} without overwrite", path.display()))?;
    sync_parent(parent)
        .with_context(|| format!("fsync artifact directory {}", parent.display()))?;
    Ok(())
}

/// Validate that a runtime directory is already owner-only or can be created
/// below an existing trusted ancestor without following a link.
pub(crate) fn validate_owner_only_directory_target(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let _directory_guard = require_safe_artifact_directory(path)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            require_existing_trusted_directory_chain(path)
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect Gateway runtime directory {}", path.display())),
    }
}

/// Create a persistent owner-only runtime directory without modifying an
/// existing directory. Missing ancestors are created one component at a time.
pub(crate) fn create_owner_only_directory(path: &Path) -> anyhow::Result<()> {
    validate_owner_only_directory_target(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => return validate_owner_only_directory_target(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect Gateway runtime directory {}", path.display()))
        }
    }

    let parent = parent_dir(path);
    match fs::symlink_metadata(parent) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_owner_only_directory(parent)?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect Gateway runtime parent {}", parent.display()))
        }
    }
    let _parent_guard = require_trusted_artifact_directory(parent)?;

    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("create Gateway runtime directory {}", path.display()))?;
    #[cfg(unix)]
    {
        let directory_guard = open_directory_guard(path)?;
        set_owner_only_directory(&directory_guard, path)?;
    }
    #[cfg(windows)]
    {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect Gateway runtime directory {}", path.display()))?;
        if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            bail!(
                "Gateway runtime directory must be a real directory: {}",
                path.display()
            );
        }
        windows_security::set_owner_only(path, true)?;
    }
    let _directory_guard = require_safe_artifact_directory(path)?;
    sync_parent(parent_dir(path)).with_context(|| {
        format!(
            "fsync Gateway runtime directory parent for {}",
            path.display()
        )
    })
}

pub(crate) fn remove_owner_only_artifact(path: &Path, description: &str) -> anyhow::Result<()> {
    let parent = parent_dir(path);
    let _directory_guard = require_trusted_artifact_directory(parent)?;
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {description}"))?;
    reject_link_or_non_file(path, &metadata)?;
    require_owner_only_file(path, &metadata, description)?;
    require_single_file_link(path, &metadata, description)?;
    fs::remove_file(path).with_context(|| format!("remove {description} at {}", path.display()))?;
    sync_parent(parent).with_context(|| format!("fsync {} parent directory", description))
}

fn configured_tls_paths(config_path: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let raw = read_regular_file(config_path)
        .with_context(|| format!("read Gateway config at {}", config_path.display()))?;
    let raw = String::from_utf8(raw).context("Gateway config is not UTF-8")?;
    let config: tp_core::config::Config =
        tp_core::config::load_from_str(&raw).context("parse Gateway config")?;
    let gateway = config
        .gateway
        .ok_or_else(|| anyhow::anyhow!("config missing [gateway] section"))?;
    let certificate = gateway
        .tls_cert
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("gateway.tls_cert is required for certificate lifecycle"))?;
    let key = gateway
        .tls_key
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("gateway.tls_key is required for certificate lifecycle"))?;
    Ok((PathBuf::from(certificate), PathBuf::from(key)))
}

fn require_distinct_tls_targets(certificate_path: &Path, key_path: &Path) -> anyhow::Result<()> {
    let certificate_exists = match fs::symlink_metadata(certificate_path) {
        Ok(metadata) => {
            reject_link_or_non_file(certificate_path, &metadata)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspect configured Gateway certificate target"),
    };
    if certificate_exists
        && same_file::is_same_file(certificate_path, key_path).with_context(|| {
            format!(
                "compare TLS certificate {} and private key {}",
                certificate_path.display(),
                key_path.display()
            )
        })?
    {
        bail!("gateway.tls_cert and gateway.tls_key must not resolve to the same file");
    }
    let certificate_target = lexical_absolute_path(certificate_path)?;
    let key_target = lexical_absolute_path(key_path)?;
    if certificate_target == key_target {
        bail!("gateway.tls_cert and gateway.tls_key must not resolve to the same file");
    }
    Ok(())
}

fn existing_regular_target(path: &Path, description: &str) -> anyhow::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            reject_link_or_non_file(path, &metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspect {description} at {}", path.display()))
        }
    }
}

fn lexical_absolute_path(path: &Path) -> anyhow::Result<PathBuf> {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("TLS path escapes its filesystem root: {}", path.display());
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    Ok(normalized)
}

fn load_or_create_key(path: &Path) -> anyhow::Result<(KeyPair, bool)> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            reject_link_or_non_file(path, &metadata)?;
            Ok((load_existing_key(path)?, true))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
                .context("generate Gateway private key")?;
            let directory_synced =
                create_owner_only_noclobber(path, key.serialize_pem().as_bytes())
                    .with_context(|| format!("create Gateway private key at {}", path.display()))?;
            Ok((key, directory_synced))
        }
        Err(error) => {
            Err(error).with_context(|| format!("inspect Gateway private key at {}", path.display()))
        }
    }
}

fn load_existing_key(path: &Path) -> anyhow::Result<KeyPair> {
    let _directory_guard = require_safe_artifact_directory(parent_dir(path))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect Gateway private key at {}", path.display()))?;
    reject_link_or_non_file(path, &metadata)?;
    require_owner_only_file(path, &metadata, "existing Gateway private key")?;
    require_single_file_link(path, &metadata, "existing Gateway private key")?;
    require_exact_private_key_mode(path, &metadata)?;
    let pem = read_regular_file(path)
        .with_context(|| format!("read existing Gateway private key at {}", path.display()))?;
    let pem = String::from_utf8(pem).context("Gateway private key is not UTF-8 PEM")?;
    let key = KeyPair::from_pem(&pem).context("parse existing Gateway private key")?;
    if !key.is_compatible(&PKCS_ECDSA_P256_SHA256) {
        bail!("existing Gateway private key must be ECDSA P-256 PKCS#8");
    }
    Ok(key)
}

/// Load the persistent TLS key for server startup. On Unix, a root-owned
/// service may consume a key delegated to one non-root operator without
/// weakening the ownership rules used by onboarding or pairing artifacts.
fn load_server_identity_key_pem(path: &Path) -> anyhow::Result<Vec<u8>> {
    #[cfg(not(unix))]
    {
        let _directory_guard = require_safe_artifact_directory(parent_dir(path))?;
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect Gateway private key at {}", path.display()))?;
        reject_link_or_non_file(path, &metadata)?;
        require_owner_only_file(path, &metadata, "existing Gateway private key")?;
        require_single_file_link(path, &metadata, "existing Gateway private key")?;
        return read_regular_file(path)
            .with_context(|| format!("read existing Gateway private key at {}", path.display()));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let effective_uid = unsafe { libc::geteuid() };
        let (mut file, directory_chain) = open_server_identity_key_without_links(path)?;
        #[cfg(target_os = "macos")]
        {
            macos_acl::require_no_allow_acl_fd(&file, "Gateway private key", path)?;
            for (ancestor_path, ancestor) in &directory_chain {
                macos_acl::require_no_allow_acl_fd(
                    ancestor,
                    "Gateway private key path ancestor",
                    ancestor_path,
                )?;
            }
        }
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("expected safe regular file: {}", path.display());
        }
        let ancestors = directory_chain
            .iter()
            .map(|(_, directory)| {
                let metadata = directory.metadata()?;
                Ok((metadata.uid(), metadata.mode()))
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        require_server_identity_key_policy(
            effective_uid,
            metadata.uid(),
            metadata.mode(),
            metadata.nlink(),
            &ancestors,
        )?;
        let mut pem = Vec::new();
        file.read_to_end(&mut pem)
            .with_context(|| format!("read existing Gateway private key at {}", path.display()))?;
        Ok(pem)
    }
}

#[cfg(unix)]
fn open_server_identity_key_without_links(
    path: &Path,
) -> anyhow::Result<(File, Vec<(PathBuf, File)>)> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(path)
    };
    let key_name = absolute
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Gateway private key path has no file name"))?;
    let parent = absolute
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Gateway private key path has no parent"))?;

    let root_name = CString::new("/").expect("root path has no NUL");
    let root_fd = unsafe {
        libc::openat(
            libc::AT_FDCWD,
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open filesystem root safely");
    }
    let root = unsafe { File::from_raw_fd(root_fd) };
    let mut directory_chain = vec![(PathBuf::from("/"), root)];
    let mut current_path = PathBuf::from("/");
    for component in parent.components() {
        match component {
            Component::Prefix(_) => {
                bail!("unexpected path prefix in Unix private key path")
            }
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                bail!(
                    "unsafe parent-directory component in path: {}",
                    path.display()
                )
            }
            Component::Normal(segment) => {
                let segment = CString::new(segment.as_bytes()).with_context(|| {
                    format!("path ancestor contains NUL: {}", current_path.display())
                })?;
                let parent_fd = directory_chain
                    .last()
                    .expect("filesystem root is retained")
                    .1
                    .as_raw_fd();
                let fd = unsafe {
                    libc::openat(
                        parent_fd,
                        segment.as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
                    )
                };
                current_path.push(component.as_os_str());
                if fd < 0 {
                    return Err(std::io::Error::last_os_error()).with_context(|| {
                        format!("open path ancestor safely at {}", current_path.display())
                    });
                }
                directory_chain.push((current_path.clone(), unsafe { File::from_raw_fd(fd) }));
            }
        }
    }
    let key_name =
        CString::new(key_name.as_bytes()).context("private key file name contains NUL")?;
    let parent_fd = directory_chain
        .last()
        .expect("filesystem root is retained")
        .1
        .as_raw_fd();
    let key_fd = unsafe {
        libc::openat(
            parent_fd,
            key_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if key_fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "open expected safe regular file for Gateway private key without following links at {}",
                path.display()
            )
        });
    }
    Ok((unsafe { File::from_raw_fd(key_fd) }, directory_chain))
}

#[cfg(unix)]
fn require_server_identity_key_policy(
    effective_uid: u32,
    key_uid: u32,
    key_mode: u32,
    key_nlink: u64,
    ancestors: &[(u32, u32)],
) -> anyhow::Result<()> {
    if effective_uid != 0 && key_uid != effective_uid {
        bail!("Gateway private key is owned by another user");
    }
    if key_mode & 0o7777 != 0o600 {
        bail!("Gateway private key is not owner-only: permissions must be exactly 0600");
    }
    if key_nlink != 1 {
        bail!("Gateway private key must not be hard-linked");
    }
    let delegated_uid = if effective_uid == 0 {
        key_uid
    } else {
        effective_uid
    };
    for (owner_uid, mode) in ancestors {
        if (*owner_uid != 0 && *owner_uid != delegated_uid) || mode & 0o022 != 0 {
            bail!("path ancestor is writable or owned by an untrusted user");
        }
    }
    Ok(())
}

fn read_regular_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    require_no_link_directory_chain(parent_dir(path))?;
    let _directory_guard = open_directory_guard(parent_dir(path))?;
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    reject_link_or_non_file(path, &metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options, false);
    let mut file = options
        .open(path)
        .with_context(|| format!("open {} without following links", path.display()))?;
    let opened_metadata = file.metadata()?;
    if metadata_is_link_or_reparse(&opened_metadata) || !opened_metadata.is_file() {
        bail!("expected regular file: {}", path.display());
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(contents)
}

fn read_regular_file_bounded(
    path: &Path,
    description: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    require_no_link_directory_chain(parent_dir(path))?;
    let _directory_guard = open_directory_guard(parent_dir(path))?;
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {description}"))?;
    reject_link_or_non_file(path, &metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options, false);
    let file = options
        .open(path)
        .with_context(|| format!("open {description} without following links"))?;
    let opened_metadata = file.metadata()?;
    if metadata_is_link_or_reparse(&opened_metadata) || !opened_metadata.is_file() {
        bail!("expected regular file for {description}");
    }
    let mut contents = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut contents)?;
    if contents.len() > max_bytes {
        bail!("{description} exceeds {max_bytes} bytes");
    }
    Ok(contents)
}

fn require_absent(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("refusing to overwrite existing path: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn create_owner_only_noclobber(path: &Path, contents: &[u8]) -> anyhow::Result<bool> {
    let parent = parent_dir(path);
    let _directory_guard = require_safe_artifact_directory(parent)?;
    require_absent(path)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary artifact in {}", parent.display()))?;
    set_owner_only_file(temporary.as_file(), temporary.path())?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("create {} without overwrite", path.display()))?;
    Ok(sync_parent(parent).is_ok())
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn require_safe_artifact_directory(path: &Path) -> anyhow::Result<File> {
    let directory_guard = require_trusted_artifact_directory(path)?;
    let metadata = directory_guard.metadata()?;
    require_owner_only_directory(path, &metadata)?;
    Ok(directory_guard)
}

fn require_trusted_artifact_directory(path: &Path) -> anyhow::Result<File> {
    require_no_link_directory_chain(path)?;
    let directory_guard = open_directory_guard(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect artifact directory {}", path.display()))?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        bail!(
            "artifact directory must be a real directory: {}",
            path.display()
        );
    }
    require_current_owner(path, &metadata)?;
    Ok(directory_guard)
}

#[cfg(unix)]
fn require_single_file_link(
    path: &Path,
    metadata: &fs::Metadata,
    description: &str,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.nlink() != 1 {
        bail!("{description} must not be hard-linked: {}", path.display());
    }
    Ok(())
}

#[cfg(unix)]
fn require_exact_private_key_mode(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.mode() & 0o7777 != 0o600 {
        bail!(
            "Gateway private key is not owner-only: permissions must be exactly 0600: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_exact_private_key_mode(_path: &Path, _metadata: &fs::Metadata) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn require_single_file_link(
    _path: &Path,
    _metadata: &fs::Metadata,
    _description: &str,
) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn open_directory_guard(path: &Path) -> anyhow::Result<File> {
    let directory =
        File::open(path).with_context(|| format!("open artifact directory {}", path.display()))?;
    if !directory.metadata()?.is_dir() {
        bail!(
            "artifact directory changed while opening: {}",
            path.display()
        );
    }
    Ok(directory)
}

#[cfg(windows)]
fn open_directory_guard(path: &Path) -> anyhow::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    let directory = options.open(path).with_context(|| {
        format!(
            "lock artifact directory {} against replacement",
            path.display()
        )
    })?;
    let metadata = directory.metadata()?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        bail!(
            "artifact directory changed while opening: {}",
            path.display()
        );
    }
    Ok(directory)
}

fn require_no_link_directory_chain(path: &Path) -> anyhow::Result<()> {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                bail!(
                    "unsafe parent-directory component in path: {}",
                    path.display()
                )
            }
            Component::Normal(segment) => {
                current.push(segment);
                let metadata = fs::symlink_metadata(&current)
                    .with_context(|| format!("inspect path ancestor {}", current.display()))?;
                if metadata_is_link_or_reparse(&metadata) {
                    bail!(
                        "refusing link or reparse-point ancestor: {}",
                        current.display()
                    );
                }
                if !metadata.is_dir() {
                    bail!("path ancestor is not a directory: {}", current.display());
                }
                require_trusted_ancestor(&current, &metadata)?;
            }
        }
    }
    Ok(())
}

fn require_existing_trusted_directory_chain(path: &Path) -> anyhow::Result<()> {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                bail!(
                    "unsafe parent-directory component in path: {}",
                    path.display()
                )
            }
            Component::Normal(segment) => {
                current.push(segment);
                let metadata = match fs::symlink_metadata(&current) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("inspect path ancestor {}", current.display())
                        })
                    }
                };
                if metadata_is_link_or_reparse(&metadata) {
                    bail!(
                        "refusing link or reparse-point ancestor: {}",
                        current.display()
                    );
                }
                if !metadata.is_dir() {
                    bail!("path ancestor is not a directory: {}", current.display());
                }
                require_trusted_ancestor(&current, &metadata)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn require_trusted_ancestor(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let current_uid = unsafe { libc::geteuid() };
    if (metadata.uid() != current_uid && metadata.uid() != 0) || metadata.mode() & 0o022 != 0 {
        bail!(
            "path ancestor is writable or owned by an untrusted user: {}",
            path.display()
        );
    }
    #[cfg(target_os = "macos")]
    macos_acl::require_no_allow_acl_path(path, "path ancestor")?;
    Ok(())
}

#[cfg(windows)]
fn require_trusted_ancestor(_path: &Path, _metadata: &fs::Metadata) -> anyhow::Result<()> {
    Ok(())
}

fn reject_link_or_non_file(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    if metadata_is_link_or_reparse(metadata) || !metadata.is_file() {
        bail!("expected safe regular file: {}", path.display());
    }
    Ok(())
}

#[cfg(unix)]
fn require_owner_only_file(
    path: &Path,
    metadata: &fs::Metadata,
    description: &str,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        bail!("{description} is not owner-only: {}", path.display());
    }
    #[cfg(target_os = "macos")]
    macos_acl::require_no_allow_acl_path(path, description)?;
    Ok(())
}

#[cfg(unix)]
fn require_current_owner(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "artifact directory is not owned by this process: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn require_owner_only_directory(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.mode() & 0o077 != 0 {
        bail!("artifact directory is not owner-only: {}", path.display());
    }
    #[cfg(target_os = "macos")]
    macos_acl::require_no_allow_acl_path(path, "artifact directory")?;
    Ok(())
}

#[cfg(windows)]
fn require_current_owner(path: &Path, _metadata: &fs::Metadata) -> anyhow::Result<()> {
    windows_security::require_owner_only(path, true)
        .with_context(|| format!("verify owner-only artifact directory at {}", path.display()))
}

#[cfg(windows)]
fn require_owner_only_directory(_path: &Path, _metadata: &fs::Metadata) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn require_owner_only_file(
    path: &Path,
    _metadata: &fs::Metadata,
    description: &str,
) -> anyhow::Result<()> {
    windows_security::require_owner_only(path, false)
        .with_context(|| format!("verify {description} ACL at {}", path.display()))
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions, create: bool) {
    use std::os::unix::fs::OpenOptionsExt as _;

    options.custom_flags(libc::O_NOFOLLOW);
    if create {
        options.mode(0o600);
    }
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions, _create: bool) {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(unix)]
fn set_owner_only_file(file: &File, _path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    #[cfg(target_os = "macos")]
    macos_acl::clear_extended_acl_fd(file, "Gateway artifact", _path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    #[cfg(target_os = "macos")]
    macos_acl::require_no_allow_acl_fd(file, "Gateway artifact", _path)?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_directory(directory: &File, _path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    #[cfg(target_os = "macos")]
    macos_acl::clear_extended_acl_fd(directory, "Gateway runtime directory", _path)?;
    directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    #[cfg(target_os = "macos")]
    macos_acl::require_no_allow_acl_fd(directory, "Gateway runtime directory", _path)?;
    Ok(())
}

#[cfg(windows)]
fn set_owner_only_file(_file: &File, path: &Path) -> anyhow::Result<()> {
    windows_security::set_owner_only(path, false)?;
    Ok(())
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    windows_security::metadata_is_reparse(metadata)
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> std::io::Result<()> {
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos_acl {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::fs::File;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;
    use std::ptr;

    use anyhow::{bail, Context as _};

    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_int = 1;

    type Acl = *mut c_void;
    type AclEntry = *mut c_void;

    unsafe extern "C" {
        fn acl_free(object: *mut c_void) -> c_int;
        fn acl_get_entry(acl: Acl, entry_id: c_int, entry: *mut AclEntry) -> c_int;
        fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> Acl;
        fn acl_get_link_np(path: *const c_char, acl_type: c_int) -> Acl;
        fn acl_get_tag_type(entry: AclEntry, tag_type: *mut c_int) -> c_int;
        fn acl_init(count: c_int) -> Acl;
        fn acl_set_fd_np(fd: c_int, acl: Acl, acl_type: c_int) -> c_int;
        fn acl_valid(acl: Acl) -> c_int;
    }

    struct OwnedAcl(Acl);

    impl OwnedAcl {
        fn from_read(acl: Acl, description: &str, path: &Path) -> anyhow::Result<Option<Self>> {
            if acl.is_null() {
                let error = std::io::Error::last_os_error();
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::EOPNOTSUPP)
                ) {
                    return Ok(None);
                }
                return Err(error)
                    .with_context(|| format!("read {description} ACL at {}", path.display()));
            }
            Ok(Some(Self(acl)))
        }

        fn from_allocation(acl: Acl, description: &str, path: &Path) -> anyhow::Result<Self> {
            if acl.is_null() {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("allocate empty {description} ACL at {}", path.display())
                });
            }
            Ok(Self(acl))
        }
    }

    impl Drop for OwnedAcl {
        fn drop(&mut self) {
            unsafe {
                acl_free(self.0);
            }
        }
    }

    pub(super) fn require_no_allow_acl_path(path: &Path, description: &str) -> anyhow::Result<()> {
        let path_bytes = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("{description} path contains NUL"))?;
        let Some(acl) = OwnedAcl::from_read(
            unsafe { acl_get_link_np(path_bytes.as_ptr(), ACL_TYPE_EXTENDED) },
            description,
            path,
        )?
        else {
            return Ok(());
        };
        require_no_allow_entries(&acl, description, path)
    }

    pub(super) fn require_no_allow_acl_fd(
        file: &File,
        description: &str,
        path: &Path,
    ) -> anyhow::Result<()> {
        let Some(acl) = OwnedAcl::from_read(
            unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) },
            description,
            path,
        )?
        else {
            return Ok(());
        };
        require_no_allow_entries(&acl, description, path)
    }

    pub(super) fn clear_extended_acl_fd(
        file: &File,
        description: &str,
        path: &Path,
    ) -> anyhow::Result<()> {
        let empty = OwnedAcl::from_allocation(unsafe { acl_init(0) }, description, path)?;
        if unsafe { acl_set_fd_np(file.as_raw_fd(), empty.0, ACL_TYPE_EXTENDED) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EOPNOTSUPP) {
                return Err(error)
                    .with_context(|| format!("clear {description} ACL at {}", path.display()));
            }
        }
        Ok(())
    }

    fn require_no_allow_entries(
        acl: &OwnedAcl,
        description: &str,
        path: &Path,
    ) -> anyhow::Result<()> {
        if unsafe { acl_valid(acl.0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("validate {description} ACL at {}", path.display()));
        }
        let mut entry = ptr::null_mut();
        let mut entry_id = ACL_FIRST_ENTRY;
        loop {
            if unsafe { acl_get_entry(acl.0, entry_id, &mut entry) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINVAL) {
                    return Ok(());
                }
                return Err(error)
                    .with_context(|| format!("inspect {description} ACL at {}", path.display()));
            }
            let mut tag_type = 0;
            if unsafe { acl_get_tag_type(entry, &mut tag_type) } != 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("inspect {description} ACL entry at {}", path.display())
                });
            }
            if tag_type == ACL_EXTENDED_ALLOW {
                bail!(
                    "{description} has an extended ACL allow entry: {}",
                    path.display()
                );
            }
            entry_id = ACL_NEXT_ENTRY;
        }
    }
}

#[cfg(windows)]
mod windows_security {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle as _;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, SET_ACCESS,
        SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION,
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, INHERITED_ACE, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
        TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(super) fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    pub(super) fn require_owner_only(path: &Path, directory: bool) -> io::Result<()> {
        use std::mem::{size_of, zeroed};

        let mut options = OpenOptions::new();
        options
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT
                    | if directory {
                        FILE_FLAG_BACKUP_SEMANTICS
                    } else {
                        0
                    },
            );
        let file = options.open(path)?;
        if metadata_is_reparse(&file.metadata()?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing Windows reparse point",
            ));
        }

        // SAFETY: buffers are initialized for Win32 and the returned security
        // descriptor owns the owner/DACL pointers until LocalFree below.
        unsafe {
            let handle = file.as_raw_handle();
            let mut owner: PSID = ptr::null_mut();
            let mut dacl: *mut ACL = ptr::null_mut();
            let mut descriptor = ptr::null_mut();
            let status = GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            );
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            let inspected = (|| {
                if descriptor.is_null() || owner.is_null() || dacl.is_null() {
                    return Err(io::Error::other("Windows object has no owner-only DACL"));
                }
                if !owner_is_current_user(owner)? {
                    return Err(io::Error::other(
                        "Windows object is not owned by the current user",
                    ));
                }
                let mut control = 0;
                let mut revision = 0;
                if GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0
                    || control & SE_DACL_PROTECTED == 0
                {
                    return Err(io::Error::other("Windows DACL is not protected"));
                }
                let mut size: ACL_SIZE_INFORMATION = zeroed();
                if GetAclInformation(
                    dacl,
                    (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                ) == 0
                    || size.AceCount != 1
                {
                    return Err(io::Error::other("Windows DACL is not owner-only"));
                }
                let mut raw_ace = ptr::null_mut();
                if GetAce(dacl, 0, &mut raw_ace) == 0 || raw_ace.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let ace = &*(raw_ace as *const ACCESS_ALLOWED_ACE);
                let expected_inheritance = if directory {
                    CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
                } else {
                    0
                };
                if ace.Header.AceType != 0
                    || u32::from(ace.Header.AceFlags) & INHERITED_ACE != 0
                    || u32::from(ace.Header.AceFlags) & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE)
                        != expected_inheritance
                    || ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
                {
                    return Err(io::Error::other("Windows DACL grants unexpected access"));
                }
                let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast();
                if EqualSid(owner, ace_sid) == 0 {
                    return Err(io::Error::other("Windows DACL principal is not the owner"));
                }
                Ok(())
            })();
            LocalFree(descriptor);
            inspected
        }
    }

    unsafe fn owner_is_current_user(owner: PSID) -> io::Result<bool> {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut required = 0;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required);
            if required == 0 {
                return Err(io::Error::last_os_error());
            }
            let word_size = std::mem::size_of::<usize>();
            let mut storage = vec![0usize; (required as usize).div_ceil(word_size)];
            if GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr().cast(),
                required,
                &mut required,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let token_user = &*(storage.as_ptr().cast::<TOKEN_USER>());
            Ok(EqualSid(owner, token_user.User.Sid) != 0)
        })();
        CloseHandle(token);
        result
    }

    pub(super) fn set_owner_only(path: &Path, directory: bool) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options
            .access_mode(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT
                    | if directory {
                        FILE_FLAG_BACKUP_SEMANTICS
                    } else {
                        0
                    },
            );
        let file = options.open(path)?;
        if metadata_is_reparse(&file.metadata()?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing Windows reparse point",
            ));
        }

        // SAFETY: pointers come from documented Win32 security APIs and are
        // released after SetSecurityInfo has consumed the ACL.
        unsafe {
            let handle = file.as_raw_handle();
            let mut owner: PSID = ptr::null_mut();
            let mut descriptor = ptr::null_mut();
            let status = GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            );
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            if descriptor.is_null() || owner.is_null() {
                LocalFree(descriptor);
                return Err(io::Error::other("Windows object has no owner SID"));
            }
            let entry = EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_ALL_ACCESS,
                grfAccessMode: SET_ACCESS,
                grfInheritance: if directory {
                    CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
                } else {
                    0
                },
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: ptr::null_mut(),
                    MultipleTrusteeOperation: 0,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_USER,
                    ptstrName: owner.cast(),
                },
            };
            let mut acl: *mut ACL = ptr::null_mut();
            let acl_status = SetEntriesInAclW(1, &entry, ptr::null(), &mut acl);
            if acl_status != ERROR_SUCCESS {
                LocalFree(descriptor);
                return Err(io::Error::from_raw_os_error(acl_status as i32));
            }
            let set_status = SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            );
            LocalFree(acl.cast());
            LocalFree(descriptor);
            if set_status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(set_status as i32));
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_only_setup_removes_macos_allow_acls() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-gateway-owner-only-acl-")
            .tempdir_in(temporary_root)
            .unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let file_path = temporary.path().join("secret");
        fs::write(&file_path, b"secret").unwrap();
        fs::set_permissions(&file_path, fs::Permissions::from_mode(0o600)).unwrap();

        for path in [temporary.path(), file_path.as_path()] {
            let status = std::process::Command::new("chmod")
                .args(["+a", "everyone allow read"])
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success());
        }
        assert!(require_owner_only_file(
            &file_path,
            &fs::symlink_metadata(&file_path).unwrap(),
            "test artifact",
        )
        .is_err());

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file_path)
            .unwrap();
        set_owner_only_file(&file, &file_path).unwrap();
        let directory = File::open(temporary.path()).unwrap();
        set_owner_only_directory(&directory, temporary.path()).unwrap();

        require_owner_only_file(
            &file_path,
            &fs::symlink_metadata(&file_path).unwrap(),
            "test artifact",
        )
        .unwrap();
        require_owner_only_directory(
            temporary.path(),
            &fs::symlink_metadata(temporary.path()).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn self_signed_ip_identity_is_valid_and_reused_byte_for_byte() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::os::unix::fs::PermissionsExt as _;

        let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-gateway-self-signed-ip-")
            .tempdir_in(temporary_root)
            .unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let certificate_path = temporary.path().join("server.crt");
        let key_path = temporary.path().join("server.key");
        let config_path = temporary.path().join("gateway.yaml");
        fs::write(
            &config_path,
            format!(
                "gateway:\n  listen_addr: 0.0.0.0:443\n  tls_cert: {}\n  tls_key: {}\n",
                certificate_path.display(),
                key_path.display()
            ),
        )
        .unwrap();
        let public_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 42));

        let first = ensure_self_signed_ip_identity(&config_path, public_ip)
            .expect("create the machine-local self-signed identity");
        let first_certificate = fs::read(&certificate_path).unwrap();
        let first_key = fs::read(&key_path).unwrap();
        let loaded = load_server_identity(&certificate_path, &key_path)
            .expect("rustls accepts the persisted identity");
        let leaf = loaded.certificates.first().unwrap();
        let (remainder, parsed) = parse_x509_certificate(leaf.as_ref()).unwrap();
        assert!(remainder.is_empty());
        assert_eq!(parsed.subject(), parsed.issuer());
        let san = parsed
            .subject_alternative_name()
            .unwrap()
            .expect("self-signed leaf has an IP SAN");
        assert_eq!(san.value.general_names.len(), 1);
        assert!(matches!(
            san.value.general_names.first(),
            Some(GeneralName::IPAddress(bytes)) if *bytes == [203, 0, 113, 42]
        ));
        let key = KeyPair::from_pem(std::str::from_utf8(&first_key).unwrap()).unwrap();
        assert_eq!(parsed.public_key().raw, key.public_key_der());
        assert_eq!(first.certificate_path, certificate_path);
        assert_eq!(first.key_path, key_path);
        assert_eq!(
            first.leaf_sha256,
            hex::encode(Sha256::digest(leaf.as_ref()))
        );

        let second = ensure_self_signed_ip_identity(&config_path, public_ip)
            .expect("reuse the machine-local self-signed identity");
        assert_eq!(fs::read(&certificate_path).unwrap(), first_certificate);
        assert_eq!(fs::read(&key_path).unwrap(), first_key);
        assert_eq!(second.leaf_sha256, first.leaf_sha256);
        assert_eq!(second.spki_sha256, first.spki_sha256);
    }

    fn write_rsa_server_identity(
        directory: &Path,
        key_encoding: RsaTestKeyEncoding,
    ) -> (PathBuf, PathBuf) {
        use rand::rngs::OsRng;
        use rsa::pkcs1::EncodeRsaPrivateKey as _;
        use rsa::pkcs8::{EncodePrivateKey as _, LineEnding};
        use std::os::unix::fs::PermissionsExt as _;

        let private_key = rsa::RsaPrivateKey::new(&mut OsRng, 2_048).expect("generate RSA key");
        let pkcs8_pem = private_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("encode RSA PKCS#8 key");
        let signing_key = KeyPair::from_pem(pkcs8_pem.as_str()).expect("load RSA signing key");
        let certificate = CertificateParams::new(vec!["localhost".to_owned()])
            .expect("RSA certificate parameters")
            .self_signed(&signing_key)
            .expect("self-sign RSA certificate")
            .pem();
        let key_pem = match key_encoding {
            RsaTestKeyEncoding::Pkcs8 => pkcs8_pem.to_string(),
            RsaTestKeyEncoding::Pkcs1 => private_key
                .to_pkcs1_pem(LineEnding::LF)
                .expect("encode RSA PKCS#1 key")
                .to_string(),
        };
        let certificate_path = directory.join("server.crt");
        let key_path = directory.join("server.key");
        fs::write(&certificate_path, certificate).expect("write RSA certificate");
        fs::write(&key_path, key_pem).expect("write RSA private key");
        fs::set_permissions(&certificate_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        (certificate_path, key_path)
    }

    enum RsaTestKeyEncoding {
        Pkcs8,
        Pkcs1,
    }

    #[test]
    fn server_startup_accepts_a_matching_rsa_pkcs8_identity() {
        let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-gateway-rsa-identity-")
            .tempdir_in(temporary_root)
            .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let (certificate_path, key_path) =
            write_rsa_server_identity(temporary.path(), RsaTestKeyEncoding::Pkcs8);

        let identity = load_server_identity(&certificate_path, &key_path)
            .expect("server startup accepts a matching RSA PKCS#8 identity");

        assert!(identity
            .private_key_pem
            .starts_with(b"-----BEGIN PRIVATE KEY-----"));
    }

    #[test]
    fn server_startup_accepts_a_matching_rsa_pkcs1_identity() {
        let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-gateway-rsa-identity-")
            .tempdir_in(temporary_root)
            .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let (certificate_path, key_path) =
            write_rsa_server_identity(temporary.path(), RsaTestKeyEncoding::Pkcs1);

        let identity = load_server_identity(&certificate_path, &key_path)
            .expect("server startup accepts a matching RSA PKCS#1 identity");

        assert!(identity
            .private_key_pem
            .starts_with(b"-----BEGIN RSA PRIVATE KEY-----"));
    }

    #[test]
    fn root_server_startup_accepts_an_owner_only_key_from_one_delegated_owner() {
        require_server_identity_key_policy(
            0,
            1_000,
            0o100600,
            1,
            &[(0, 0o040755), (1_000, 0o040700)],
        )
        .expect("root may load one delegated owner's private Gateway key");
    }

    #[test]
    fn root_server_startup_rejects_a_third_owner_in_the_key_ancestor_chain() {
        let error = require_server_identity_key_policy(
            0,
            1_000,
            0o100600,
            1,
            &[(0, 0o040755), (1_001, 0o040700)],
        )
        .expect_err("an unrelated UID must not control a key ancestor");

        assert!(error.to_string().contains("untrusted user"));
    }

    #[test]
    fn root_server_startup_rejects_a_group_writable_delegated_ancestor() {
        let error = require_server_identity_key_policy(
            0,
            1_000,
            0o100600,
            1,
            &[(0, 0o040755), (1_000, 0o040720)],
        )
        .expect_err("a group-writable ancestor must not be trusted");

        assert!(error.to_string().contains("writable"));
    }

    #[test]
    fn nonroot_server_startup_rejects_a_foreign_private_key_owner() {
        let error = require_server_identity_key_policy(
            1_000,
            1_001,
            0o100600,
            1,
            &[(0, 0o040755), (1_000, 0o040700)],
        )
        .expect_err("non-root startup must not consume another user's key");

        assert!(error.to_string().contains("another user"));
    }

    #[test]
    fn server_startup_requires_exactly_0600_private_key_permissions() {
        let error = require_server_identity_key_policy(
            0,
            1_000,
            0o100400,
            1,
            &[(0, 0o040755), (1_000, 0o040700)],
        )
        .expect_err("owner-read-only is not the persistent key contract");

        assert!(error.to_string().contains("exactly 0600"));
    }

    #[test]
    fn server_startup_rejects_special_permission_bits_on_the_private_key() {
        let error = require_server_identity_key_policy(
            0,
            1_000,
            0o104600,
            1,
            &[(0, 0o040755), (1_000, 0o040700)],
        )
        .expect_err("set-user-ID is not part of exact 0600 permissions");

        assert!(error.to_string().contains("exactly 0600"));
    }

    #[test]
    fn server_startup_rejects_a_hard_linked_private_key() {
        let error = require_server_identity_key_policy(
            0,
            1_000,
            0o100600,
            2,
            &[(0, 0o040755), (1_000, 0o040700)],
        )
        .expect_err("a private key with another hard link must not be loaded");

        assert!(error.to_string().contains("hard-linked"));
    }

    #[test]
    fn server_identity_key_loader_refuses_a_symlink_ancestor() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-gateway-server-key-")
            .tempdir_in(temporary_root)
            .unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let real = temporary.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let key_path = real.join("server.key");
        fs::write(&key_path, KeyPair::generate().unwrap().serialize_pem()).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        let linked = temporary.path().join("linked");
        symlink(&real, &linked).unwrap();

        let error = load_server_identity_key_pem(&linked.join("server.key"))
            .expect_err("server startup must not follow a key ancestor symlink");

        assert!(error.to_string().contains("open path ancestor safely"));
    }

    #[test]
    fn opened_server_identity_key_is_stable_after_its_path_is_replaced() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("lantunnel-gateway-server-key-swap-")
            .tempdir_in(temporary_root)
            .unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key_directory = temporary.path().join("identity");
        fs::create_dir(&key_directory).unwrap();
        fs::set_permissions(&key_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let key_path = key_directory.join("server.key");
        fs::write(&key_path, b"opened-generation").unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        let (mut opened_key, directory_chain) =
            open_server_identity_key_without_links(&key_path).unwrap();

        let moved_directory = temporary.path().join("moved-identity");
        fs::rename(&key_directory, &moved_directory).unwrap();
        fs::create_dir(&key_directory).unwrap();
        fs::write(&key_path, b"replacement-generation").unwrap();

        let mut opened_contents = Vec::new();
        opened_key.read_to_end(&mut opened_contents).unwrap();
        assert_eq!(opened_contents, b"opened-generation");
        assert_eq!(
            directory_chain.last().unwrap().1.metadata().unwrap().ino(),
            fs::metadata(&moved_directory).unwrap().ino()
        );
    }
}
