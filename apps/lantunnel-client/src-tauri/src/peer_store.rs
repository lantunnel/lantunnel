//! Verified local storage for imported Lantunnel 2.0 Peer profiles.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tp_core::provisioning::{
    normalize_certificate_pem, PeerBootstrapV2, PeerProfileV2, ProvisioningError,
};

const MAX_PEER_FILE_BYTES: u64 = 1024 * 1024;
const MAX_GATEWAY_FILE_BYTES: u64 = 64 * 1024;

/// The key-free result of a successful import. This is safe to expose to the
/// CLI, desktop UI, and status APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerBootstrapKindV2 {
    StaticGateway,
    ManagedPlatform,
}

/// Public Peer information only. Private keys and membership credentials are
/// intentionally absent.
///
/// The two names are local. A `.peer` file has never carried either one, and
/// it cannot start: `PeerProfileV2` is `deny_unknown_fields`, so a Client
/// already installed would reject a profile that grew a field. See
/// [`PeerLabelsV2`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportedPeerSummaryV2 {
    pub tunnel_id: String,
    pub peer_id: String,
    pub overlay_ip: Ipv4Addr,
    pub bootstrap_kind: PeerBootstrapKindV2,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_name: Option<String>,
}

/// The longest name kept for one Tunnel or Peer.
///
/// The Platform bounds its own names; this bounds what an imported profile can
/// be labelled with locally, so a pasted essay cannot bloat the sidecar or the
/// list it feeds.
pub const MAX_PEER_LABEL_CHARS: usize = 64;

/// Where the local names live, beside the profiles they name.
const PEER_LABELS_FILE: &str = "labels.json";

/// Names the Platform knows and a `.peer` file does not.
///
/// Written when the Platform mints a Peer through the Client, and editable for
/// a profile that arrived as a file. Absent names are not an error: the UI
/// falls back to the Overlay IP, which every profile has.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerLabelsV2 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_name: Option<String>,
}

impl PeerLabelsV2 {
    /// Trims, drops the empty, and caps the long.
    pub fn normalized(self) -> Self {
        Self {
            tunnel_name: normalize_label(self.tunnel_name),
            peer_name: normalize_label(self.peer_name),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tunnel_name.is_none() && self.peer_name.is_none()
    }
}

fn normalize_label(value: Option<String>) -> Option<String> {
    let trimmed = value?.trim().to_owned();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_PEER_LABEL_CHARS).collect())
}

fn peer_labels_path(client_config_root: &Path) -> PathBuf {
    client_config_root.join("peers").join(PEER_LABELS_FILE)
}

/// Reads the local names, treating anything unreadable as "no names".
///
/// A profile list that refuses to render because a cosmetic sidecar is corrupt
/// would lock the owner out of their own Tunnels. The names are the only thing
/// at stake here, so the failure is silent by design.
pub fn read_peer_labels(client_config_root: &Path) -> BTreeMap<String, PeerLabelsV2> {
    let path = peer_labels_path(client_config_root);
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return BTreeMap::new();
    };
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return BTreeMap::new();
    }
    if metadata.len() > MAX_GATEWAY_FILE_BYTES {
        return BTreeMap::new();
    }
    fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, PeerLabelsV2>>(&bytes).ok())
        .unwrap_or_default()
}

/// Records the local names for one Tunnel. Empty names remove the entry.
pub fn set_peer_labels(
    client_config_root: &Path,
    tunnel_id: &str,
    labels: PeerLabelsV2,
) -> Result<(), PeerImportError> {
    if !safe_filename_id(tunnel_id) {
        return Err(PeerImportError::UnsafeTunnelId);
    }
    let mut stored = read_peer_labels(client_config_root);
    let labels = labels.normalized();
    if labels.is_empty() {
        stored.remove(tunnel_id);
    } else {
        stored.insert(tunnel_id.to_owned(), labels);
    }
    write_peer_labels(client_config_root, &stored)
}

fn write_peer_labels(
    client_config_root: &Path,
    labels: &BTreeMap<String, PeerLabelsV2>,
) -> Result<(), PeerImportError> {
    let path = peer_labels_path(client_config_root);
    if labels.is_empty() {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(PeerImportError::WriteDestination { path, source }),
        };
    }
    ensure_private_directory(client_config_root)?;
    replace_private_json_file(&path, labels)
}

/// A verified identity secret and the connection bootstrap selected for this
/// machine. It intentionally implements neither `Debug` nor `Serialize`.
pub struct LoadedPeerProfileV2 {
    profile: PeerProfileV2,
    effective_bootstrap: PeerBootstrapV2,
}

impl LoadedPeerProfileV2 {
    pub fn profile(&self) -> &PeerProfileV2 {
        &self.profile
    }

    pub fn effective_bootstrap(&self) -> &PeerBootstrapV2 {
        &self.effective_bootstrap
    }
}

/// Loads one verified identity and its bootstrap.
///
/// The Gateway is whatever the `.peer` file says, for both bootstrap kinds. A
/// Static profile used to be overridable from a file beside it — address, TLS
/// server name and trusted certificate together — and the Peer membership
/// signature covers tunnel_id, peer_id, overlay_ip and peer_public_key, not
/// the Gateway facts. Honouring that file pointed the Client at a host of
/// someone's choosing and told it to trust that host's certificate. Importing
/// a different `.peer` file is how the Gateway changes.
/// Removes one stored profile.
///
/// Without this a Tunnel could be joined but never left: the file stayed until
/// someone found it on disk.
pub fn forget_peer_profile(
    client_config_root: &Path,
    tunnel_id: &str,
) -> Result<(), PeerImportError> {
    if !safe_filename_id(tunnel_id) {
        return Err(PeerImportError::UnsafeTunnelId);
    }
    let path = client_config_root
        .join("peers")
        .join(format!("{tunnel_id}.peer"));
    let removed = match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        // Already gone is the state the caller asked for.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PeerImportError::WriteDestination { path, source }),
    };
    // Leaving the names behind would relabel whatever Peer is imported for
    // this Tunnel next with the forgotten one's identity.
    set_peer_labels(client_config_root, tunnel_id, PeerLabelsV2::default())?;
    removed
}

pub fn load_peer_profile(
    client_config_root: &Path,
    tunnel_id: &str,
) -> Result<LoadedPeerProfileV2, PeerImportError> {
    if !safe_filename_id(tunnel_id) {
        return Err(PeerImportError::UnsafeTunnelId);
    }
    let path = client_config_root
        .join("peers")
        .join(format!("{tunnel_id}.peer"));
    let profile = read_peer_profile(&path, StoredFile::Yes)?;
    if profile.tunnel_id != tunnel_id {
        return Err(PeerImportError::StoredTunnelMismatch(path));
    }

    let effective_bootstrap = profile.bootstrap.clone();

    Ok(LoadedPeerProfileV2 {
        profile,
        effective_bootstrap,
    })
}

/// Reads and normalizes a certificate-only PEM without following symlinks.
pub fn read_gateway_certificate_file(path: &Path) -> Result<String, PeerImportError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| PeerImportError::ReadSource {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata_is_link_or_reparse(&metadata)
        || !metadata.is_file()
        || metadata.len() > MAX_GATEWAY_FILE_BYTES
    {
        return Err(PeerImportError::UnsafeGatewayCertificate(
            path.to_path_buf(),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options
        .open(path)
        .map_err(|source| PeerImportError::ReadSource {
            path: path.to_path_buf(),
            source,
        })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_GATEWAY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| PeerImportError::ReadSource {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_GATEWAY_FILE_BYTES {
        return Err(PeerImportError::UnsafeGatewayCertificate(
            path.to_path_buf(),
        ));
    }
    let pem = String::from_utf8(bytes)
        .map_err(|_| PeerImportError::InvalidGatewayCertificate(path.to_path_buf()))?;
    normalize_certificate_pem(&pem)
        .map_err(|_| PeerImportError::InvalidGatewayCertificate(path.to_path_buf()))
}

/// Atomically replaces one JSON file without following an existing symlink.
/// The containing directory and resulting file are owner-only.
pub fn replace_private_json_file<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), PeerImportError> {
    let parent = path
        .parent()
        .ok_or_else(|| PeerImportError::UnsafeDirectory(path.to_path_buf()))?;
    ensure_private_directory(parent)?;
    let contents = serde_json::to_vec_pretty(value).map_err(|source| {
        PeerImportError::SerializePrivateJson {
            path: path.to_path_buf(),
            source,
        }
    })?;
    replace_private_file(path, &contents)
}

/// Lists verified imported profiles without exposing identity keys or
/// membership credentials. A missing `peers` directory is an empty store.
pub fn list_peer_profiles(
    client_config_root: &Path,
) -> Result<Vec<ImportedPeerSummaryV2>, PeerImportError> {
    let peers_dir = client_config_root.join("peers");
    let directory = match fs::symlink_metadata(&peers_dir) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
            return Err(PeerImportError::UnsafeDirectory(peers_dir));
        }
        Ok(_) => fs::read_dir(&peers_dir).map_err(|source| PeerImportError::ReadStorage {
            path: peers_dir.clone(),
            source,
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(PeerImportError::ReadStorage {
                path: peers_dir,
                source,
            });
        }
    };

    let labels = read_peer_labels(client_config_root);
    let mut summaries = Vec::new();
    for entry in directory {
        let entry = entry.map_err(|source| PeerImportError::ReadStorage {
            path: client_config_root.join("peers"),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("peer") {
            continue;
        }
        let profile = read_peer_profile(&path, StoredFile::Yes)?;
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(profile.tunnel_id.as_str()) {
            return Err(PeerImportError::StoredTunnelMismatch(path));
        }
        let named = labels.get(&profile.tunnel_id);
        summaries.push(public_summary(&profile, named));
    }
    summaries.sort_by(|left, right| left.tunnel_id.cmp(&right.tunnel_id));
    Ok(summaries)
}

/// Verifies and stores one `.peer` file under `peers/<tunnel_id>.peer`.
/// Re-importing the same Tunnel atomically replaces its stored profile.
pub fn import_peer_profile(
    source: &Path,
    client_config_root: &Path,
) -> Result<ImportedPeerSummaryV2, PeerImportError> {
    let profile = read_peer_profile(source, StoredFile::No)?;
    if !safe_filename_id(&profile.tunnel_id) {
        return Err(PeerImportError::UnsafeTunnelId);
    }

    let canonical = serde_yaml::to_string(&profile).map_err(|_| PeerImportError::InvalidProfile)?;
    let peers_dir = client_config_root.join("peers");
    ensure_private_directory(client_config_root)?;
    ensure_private_directory(&peers_dir)?;

    // Replaces rather than refuses. A profile could be imported exactly once
    // per Tunnel and nothing here could remove one, so a reinstall, a second
    // device, or a profile the Platform issued again all hit the same wall.
    // The file names the Tunnel, so importing it again is the owner saying
    // "this is the profile for that Tunnel now".
    let destination = peers_dir.join(format!("{}.peer", profile.tunnel_id));
    replace_private_file(&destination, canonical.as_bytes())?;

    let labels = read_peer_labels(client_config_root);
    let named = labels.get(&profile.tunnel_id);
    Ok(public_summary(&profile, named))
}

/// Stores a `.peer` the Platform just minted, without it touching the disk
/// unprotected first.
///
/// The Platform returns the profile in the response body. Writing that body to
/// a temporary file so [`import_peer_profile`] could read it back would put a
/// Peer private key on disk outside the owner-only tree for as long as the
/// import took, which is exactly the window this store exists to close.
pub fn import_peer_profile_bytes(
    bytes: &[u8],
    client_config_root: &Path,
    labels: PeerLabelsV2,
) -> Result<ImportedPeerSummaryV2, PeerImportError> {
    if bytes.len() as u64 > MAX_PEER_FILE_BYTES {
        return Err(PeerImportError::InvalidProfile);
    }
    let profile: PeerProfileV2 =
        serde_yaml::from_slice(bytes).map_err(|_| PeerImportError::InvalidProfile)?;
    profile.verify()?;
    if !safe_filename_id(&profile.tunnel_id) {
        return Err(PeerImportError::UnsafeTunnelId);
    }

    let canonical = serde_yaml::to_string(&profile).map_err(|_| PeerImportError::InvalidProfile)?;
    let peers_dir = client_config_root.join("peers");
    ensure_private_directory(client_config_root)?;
    ensure_private_directory(&peers_dir)?;
    let destination = peers_dir.join(format!("{}.peer", profile.tunnel_id));
    replace_private_file(&destination, canonical.as_bytes())?;

    let labels = labels.normalized();
    set_peer_labels(client_config_root, &profile.tunnel_id, labels.clone())?;
    Ok(public_summary(&profile, Some(&labels)))
}

fn public_summary(profile: &PeerProfileV2, labels: Option<&PeerLabelsV2>) -> ImportedPeerSummaryV2 {
    ImportedPeerSummaryV2 {
        tunnel_name: labels.and_then(|labels| labels.tunnel_name.clone()),
        peer_name: labels.and_then(|labels| labels.peer_name.clone()),
        tunnel_id: profile.tunnel_id.clone(),
        peer_id: profile.peer.peer_id.clone(),
        overlay_ip: profile.peer.overlay_ip,
        bootstrap_kind: match &profile.bootstrap {
            PeerBootstrapV2::StaticGateway(_) => PeerBootstrapKindV2::StaticGateway,
            PeerBootstrapV2::ManagedPlatform { .. } => PeerBootstrapKindV2::ManagedPlatform,
        },
    }
}

#[derive(Clone, Copy)]
enum StoredFile {
    No,
    Yes,
}

fn read_peer_profile(path: &Path, stored: StoredFile) -> Result<PeerProfileV2, PeerImportError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| match stored {
        StoredFile::No => PeerImportError::ReadSource {
            path: path.to_path_buf(),
            source,
        },
        StoredFile::Yes => PeerImportError::ReadStorage {
            path: path.to_path_buf(),
            source,
        },
    })?;
    if metadata_is_link_or_reparse(&metadata)
        || !metadata.is_file()
        || metadata.len() > MAX_PEER_FILE_BYTES
    {
        return Err(match stored {
            StoredFile::No => PeerImportError::UnsafeSource(path.to_path_buf()),
            StoredFile::Yes => PeerImportError::UnsafeStoredProfile(path.to_path_buf()),
        });
    }

    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options.open(path).map_err(|source| match stored {
        StoredFile::No => PeerImportError::ReadSource {
            path: path.to_path_buf(),
            source,
        },
        StoredFile::Yes => PeerImportError::ReadStorage {
            path: path.to_path_buf(),
            source,
        },
    })?;
    let opened_metadata = file
        .metadata()
        .map_err(|source| PeerImportError::ReadStorage {
            path: path.to_path_buf(),
            source,
        })?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_PEER_FILE_BYTES {
        return Err(match stored {
            StoredFile::No => PeerImportError::UnsafeSource(path.to_path_buf()),
            StoredFile::Yes => PeerImportError::UnsafeStoredProfile(path.to_path_buf()),
        });
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(MAX_PEER_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| PeerImportError::ReadStorage {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_PEER_FILE_BYTES {
        return Err(match stored {
            StoredFile::No => PeerImportError::UnsafeSource(path.to_path_buf()),
            StoredFile::Yes => PeerImportError::UnsafeStoredProfile(path.to_path_buf()),
        });
    }
    let profile: PeerProfileV2 =
        serde_yaml::from_slice(&bytes).map_err(|_| PeerImportError::InvalidProfile)?;
    profile.verify()?;
    Ok(profile)
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

fn replace_private_file(path: &Path, contents: &[u8]) -> Result<(), PeerImportError> {
    let parent = path
        .parent()
        .ok_or_else(|| PeerImportError::UnsafeDirectory(path.to_path_buf()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|source| {
        PeerImportError::WriteDestination {
            path: path.to_path_buf(),
            source,
        }
    })?;
    set_owner_only_open_file_permissions(temporary.as_file(), temporary.path()).map_err(
        |source| PeerImportError::WriteDestination {
            path: path.to_path_buf(),
            source,
        },
    )?;
    temporary
        .write_all(contents)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| PeerImportError::WriteDestination {
            path: path.to_path_buf(),
            source,
        })?;
    reject_unsafe_replace_target(path)?;
    tp_core::atomic_file::persist_atomically(temporary, path).map_err(|source| {
        PeerImportError::WriteDestination {
            path: path.to_path_buf(),
            source,
        }
    })?;
    sync_parent_directory(parent).map_err(|source| PeerImportError::WriteDestination {
        path: path.to_path_buf(),
        source,
    })
}

fn reject_unsafe_replace_target(path: &Path) -> Result<(), PeerImportError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            Err(PeerImportError::UnsafeGatewayOverride(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PeerImportError::ReadStorage {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
fn set_owner_only_open_file_permissions(file: &fs::File, _path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn set_owner_only_open_file_permissions(_file: &fs::File, path: &Path) -> std::io::Result<()> {
    windows_security::set_owner_only(path, false)
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> std::io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

fn safe_filename_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn ensure_private_directory(path: &Path) -> Result<(), PeerImportError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
            return Err(PeerImportError::UnsafeDirectory(path.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| PeerImportError::CreateDirectory {
                path: path.to_path_buf(),
                source,
            })?;
        }
        Err(source) => {
            return Err(PeerImportError::CreateDirectory {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    set_owner_only_dir_permissions(path).map_err(|source| PeerImportError::CreateDirectory {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn set_owner_only_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
fn set_owner_only_dir_permissions(path: &Path) -> std::io::Result<()> {
    windows_security::set_owner_only(path, true)
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    windows_security::metadata_is_reparse(metadata)
}

#[derive(Debug, Error)]
pub enum PeerImportError {
    #[error("cannot read Peer profile {path}: {source}")]
    ReadSource {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Peer profile path is not a safe regular file: {0}")]
    UnsafeSource(PathBuf),
    #[error("Peer profile is invalid")]
    InvalidProfile,
    #[error(transparent)]
    InvalidProvisioning(#[from] ProvisioningError),
    #[error("Peer profile has a Tunnel ID that is unsafe for local storage")]
    UnsafeTunnelId,
    #[error("Peer storage path is not a safe directory: {0}")]
    UnsafeDirectory(PathBuf),
    #[error("cannot read Peer storage path {path}: {source}")]
    ReadStorage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("stored Peer profile path is not a safe regular file: {0}")]
    UnsafeStoredProfile(PathBuf),
    #[error("stored Peer filename does not match its Tunnel ID: {0}")]
    StoredTunnelMismatch(PathBuf),
    #[error("local Gateway override path is not a safe regular file: {0}")]
    UnsafeGatewayOverride(PathBuf),
    #[error("local Gateway override is invalid: {0}")]
    InvalidGatewayOverride(PathBuf),
    #[error("Gateway connection facts are invalid")]
    InvalidGatewayFacts,
    #[error("Managed Platform profiles do not allow a local Gateway override")]
    ManagedGatewayOverrideForbidden,
    #[error("Gateway certificate path is not a safe regular file: {0}")]
    UnsafeGatewayCertificate(PathBuf),
    #[error("Gateway certificate must be a certificate-only PEM: {0}")]
    InvalidGatewayCertificate(PathBuf),
    #[error("cannot create private Peer storage directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Peer profile already exists and was not overwritten: {0}")]
    DestinationExists(PathBuf),
    #[error("cannot store Peer profile {path}: {source}")]
    WriteDestination {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot serialize private JSON file {path}: {source}")]
    SerializePrivateJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[cfg(windows)]
mod windows_security {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle as _;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, SET_ACCESS,
        SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
    };

    pub(super) fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
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

        // SAFETY: all pointers are either null or returned by the documented
        // Win32 security APIs, and allocated ACL/descriptor buffers are freed
        // after SetSecurityInfo has consumed them.
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

#[cfg(test)]
mod label_tests {
    use super::*;
    use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};

    fn owner() -> TunnelOwnerFileV2 {
        TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        })
        .expect("Tunnel")
    }

    fn profile_bytes(owner: &mut TunnelOwnerFileV2) -> (String, Vec<u8>) {
        let profile = owner.add_peer(None, 1, None).expect("Peer");
        let tunnel_id = profile.tunnel_id.clone();
        let bytes = serde_yaml::to_string(&profile)
            .expect("profile yaml")
            .into_bytes();
        (tunnel_id, bytes)
    }

    #[test]
    fn a_platform_minted_peer_keeps_the_names_the_file_cannot_carry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut owner = owner();
        let (tunnel_id, bytes) = profile_bytes(&mut owner);

        let summary = import_peer_profile_bytes(
            &bytes,
            dir.path(),
            PeerLabelsV2 {
                tunnel_name: Some("  Home Lab  ".into()),
                peer_name: Some("laptop".into()),
            },
        )
        .expect("import");
        assert_eq!(summary.tunnel_name.as_deref(), Some("Home Lab"));
        assert_eq!(summary.peer_name.as_deref(), Some("laptop"));

        // The names survive a restart, because the list is what the UI reads.
        let listed = list_peer_profiles(dir.path()).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].tunnel_id, tunnel_id);
        assert_eq!(listed[0].tunnel_name.as_deref(), Some("Home Lab"));
        assert_eq!(listed[0].peer_name.as_deref(), Some("laptop"));
    }

    #[test]
    fn forgetting_a_tunnel_does_not_leave_its_names_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut owner = owner();
        let (tunnel_id, bytes) = profile_bytes(&mut owner);
        import_peer_profile_bytes(
            &bytes,
            dir.path(),
            PeerLabelsV2 {
                tunnel_name: Some("Home Lab".into()),
                peer_name: Some("laptop".into()),
            },
        )
        .expect("import");

        forget_peer_profile(dir.path(), &tunnel_id).expect("forget");
        assert!(!read_peer_labels(dir.path()).contains_key(&tunnel_id));

        // Re-importing the same Tunnel by hand must not inherit the old names:
        // it is a different Peer with a different address.
        let (_, second) = profile_bytes(&mut owner);
        let path = dir.path().join("second.peer");
        fs::write(&path, &second).expect("write");
        let reimported = import_peer_profile(&path, dir.path()).expect("import file");
        assert_eq!(reimported.tunnel_name, None);
        assert_eq!(reimported.peer_name, None);
    }

    #[test]
    fn an_over_long_or_blank_name_is_bounded_before_it_is_stored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut owner = owner();
        let (tunnel_id, bytes) = profile_bytes(&mut owner);
        import_peer_profile_bytes(&bytes, dir.path(), PeerLabelsV2::default()).expect("import");

        set_peer_labels(
            dir.path(),
            &tunnel_id,
            PeerLabelsV2 {
                tunnel_name: Some("x".repeat(500)),
                peer_name: Some("   ".into()),
            },
        )
        .expect("set");
        let stored = read_peer_labels(dir.path());
        let entry = stored.get(&tunnel_id).expect("entry");
        assert_eq!(
            entry.tunnel_name.as_deref().map(str::len),
            Some(MAX_PEER_LABEL_CHARS)
        );
        // A name that is only whitespace is no name at all.
        assert_eq!(entry.peer_name, None);
    }

    #[test]
    fn clearing_both_names_removes_the_entry_rather_than_storing_an_empty_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut owner = owner();
        let (tunnel_id, bytes) = profile_bytes(&mut owner);
        import_peer_profile_bytes(
            &bytes,
            dir.path(),
            PeerLabelsV2 {
                tunnel_name: Some("Home Lab".into()),
                peer_name: Some("laptop".into()),
            },
        )
        .expect("import");

        set_peer_labels(dir.path(), &tunnel_id, PeerLabelsV2::default()).expect("clear");
        assert!(read_peer_labels(dir.path()).is_empty());
        assert!(!dir.path().join("peers").join(PEER_LABELS_FILE).exists());
    }

    #[test]
    fn a_corrupt_label_file_costs_the_names_and_nothing_else() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut owner = owner();
        let (_, bytes) = profile_bytes(&mut owner);
        import_peer_profile_bytes(
            &bytes,
            dir.path(),
            PeerLabelsV2 {
                tunnel_name: Some("Home Lab".into()),
                peer_name: Some("laptop".into()),
            },
        )
        .expect("import");
        fs::write(
            dir.path().join("peers").join(PEER_LABELS_FILE),
            b"{ not json",
        )
        .expect("corrupt");

        // Refusing to list would lock the owner out of their own Tunnels.
        let listed = list_peer_profiles(dir.path()).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].tunnel_name, None);
    }

    #[test]
    fn a_tunnel_id_that_is_unsafe_for_a_filename_cannot_name_a_label_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(matches!(
            set_peer_labels(dir.path(), "../escape", PeerLabelsV2::default()),
            Err(PeerImportError::UnsafeTunnelId)
        ));
    }

    #[test]
    fn a_profile_that_is_not_a_valid_peer_is_refused_before_it_is_stored() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(matches!(
            import_peer_profile_bytes(b"not a profile", dir.path(), PeerLabelsV2::default()),
            Err(PeerImportError::InvalidProfile)
        ));
        assert!(list_peer_profiles(dir.path()).expect("list").is_empty());
    }
}
