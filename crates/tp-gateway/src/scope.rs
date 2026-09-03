//! Gateway admission scopes for Lantunnel 2.0.
//!
//! Static files and Platform-managed in-memory entries are separate inputs to
//! one O(1) lookup. This module intentionally has no lease, watcher, database,
//! Peer list, or runtime route state.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use thiserror::Error;
use tp_core::provisioning::{GatewayScopeFileV2, ProvisioningError};

const MAX_SCOPE_FILE_BYTES: u64 = 64 * 1024;

#[derive(Default)]
struct ScopePartitions {
    static_scopes: HashMap<String, GatewayScopeFileV2>,
    managed_scopes: HashMap<String, GatewayScopeFileV2>,
    seen_issuers: HashMap<String, String>,
}

/// The single Gateway lookup for V2 Tunnel admission.
#[derive(Default)]
pub struct ScopeStore {
    partitions: RwLock<ScopePartitions>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticScopeReload {
    pub count: usize,
    pub removed_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedScopeReplace {
    pub count: usize,
    pub removed_ids: Vec<String>,
}

impl ScopeStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, tunnel_id: &str) -> Option<GatewayScopeFileV2> {
        let partitions = self.partitions.read();
        partitions
            .static_scopes
            .get(tunnel_id)
            .or_else(|| partitions.managed_scopes.get(tunnel_id))
            .cloned()
    }

    pub fn contains(&self, tunnel_id: &str) -> bool {
        self.get(tunnel_id).is_some()
    }

    pub fn static_len(&self) -> usize {
        self.partitions.read().static_scopes.len()
    }

    pub fn managed_len(&self) -> usize {
        self.partitions.read().managed_scopes.len()
    }

    /// Validates a complete Platform-managed snapshot before atomically
    /// replacing only the managed partition. Static scopes remain untouched.
    pub fn replace_managed_snapshot(
        &self,
        scopes: Vec<GatewayScopeFileV2>,
    ) -> Result<ManagedScopeReplace, ScopeStoreError> {
        let mut candidate = HashMap::with_capacity(scopes.len());
        for scope in scopes {
            scope.verify()?;
            let tunnel_id = scope.tunnel_id.clone();
            if candidate.insert(tunnel_id.clone(), scope).is_some() {
                return Err(ScopeStoreError::DuplicateManagedTunnelId { tunnel_id });
            }
        }

        let mut partitions = self.partitions.write();
        for (tunnel_id, scope) in &candidate {
            if partitions.static_scopes.contains_key(tunnel_id) {
                return Err(ScopeStoreError::SourceConflict {
                    tunnel_id: tunnel_id.clone(),
                });
            }
            if partitions
                .seen_issuers
                .get(tunnel_id)
                .is_some_and(|issuer| issuer != &scope.tunnel_signing_public_key)
            {
                return Err(ScopeStoreError::IssuerReplacement {
                    tunnel_id: tunnel_id.clone(),
                });
            }
        }

        let mut removed_ids = partitions
            .managed_scopes
            .keys()
            .filter(|tunnel_id| !candidate.contains_key(*tunnel_id))
            .cloned()
            .collect::<Vec<_>>();
        removed_ids.sort();
        for (tunnel_id, scope) in &candidate {
            partitions
                .seen_issuers
                .entry(tunnel_id.clone())
                .or_insert_with(|| scope.tunnel_signing_public_key.clone());
        }
        let count = candidate.len();
        partitions.managed_scopes = candidate;
        Ok(ManagedScopeReplace { count, removed_ids })
    }

    /// Parses and validates the complete static directory before replacing
    /// only the static partition. Any failure leaves the last-known-good
    /// static and managed partitions untouched.
    pub fn reload_static(&self, directory: &Path) -> Result<StaticScopeReload, ScopeStoreError> {
        let candidate = load_static_directory(directory)?;
        let mut partitions = self.partitions.write();
        for (tunnel_id, scope) in &candidate {
            if partitions.managed_scopes.contains_key(tunnel_id) {
                return Err(ScopeStoreError::SourceConflict {
                    tunnel_id: tunnel_id.clone(),
                });
            }
            if partitions
                .seen_issuers
                .get(tunnel_id)
                .is_some_and(|issuer| issuer != &scope.tunnel_signing_public_key)
            {
                return Err(ScopeStoreError::IssuerReplacement {
                    tunnel_id: tunnel_id.clone(),
                });
            }
        }
        let count = candidate.len();
        let mut removed_ids = partitions
            .static_scopes
            .keys()
            .filter(|tunnel_id| !candidate.contains_key(*tunnel_id))
            .cloned()
            .collect::<Vec<_>>();
        removed_ids.sort();
        for (tunnel_id, scope) in &candidate {
            partitions
                .seen_issuers
                .entry(tunnel_id.clone())
                .or_insert_with(|| scope.tunnel_signing_public_key.clone());
        }
        partitions.static_scopes = candidate;
        Ok(StaticScopeReload { count, removed_ids })
    }
}

fn load_static_directory(
    directory: &Path,
) -> Result<HashMap<String, GatewayScopeFileV2>, ScopeStoreError> {
    let directory_metadata =
        fs::symlink_metadata(directory).map_err(|source| ScopeStoreError::ReadDirectory {
            path: directory.to_path_buf(),
            source,
        })?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(ScopeStoreError::UnsafePath {
            path: directory.to_path_buf(),
        });
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).map_err(|source| ScopeStoreError::ReadDirectory {
        path: directory.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| ScopeStoreError::ReadDirectory {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("scope") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut scopes = HashMap::with_capacity(paths.len());
    for path in paths {
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| ScopeStoreError::ReadScope {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_SCOPE_FILE_BYTES
        {
            return Err(ScopeStoreError::UnsafePath { path });
        }
        let bytes = fs::read(&path).map_err(|source| ScopeStoreError::ReadScope {
            path: path.clone(),
            source,
        })?;
        let scope: GatewayScopeFileV2 =
            serde_yaml::from_slice(&bytes).map_err(|source| ScopeStoreError::InvalidScope {
                path: path.clone(),
                message: source.to_string(),
            })?;
        scope
            .verify()
            .map_err(|source| ScopeStoreError::InvalidScope {
                path: path.clone(),
                message: source.to_string(),
            })?;
        let tunnel_id = scope.tunnel_id.clone();
        if scopes.insert(tunnel_id.clone(), scope).is_some() {
            return Err(ScopeStoreError::DuplicateTunnelId { tunnel_id });
        }
    }
    Ok(scopes)
}

#[derive(Debug, Error)]
pub enum ScopeStoreError {
    #[error("cannot read Scope directory {path}: {source}")]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read Scope file {path}: {source}")]
    ReadScope {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsafe Scope path {path}")]
    UnsafePath { path: PathBuf },
    #[error("invalid Scope file {path}: {message}")]
    InvalidScope { path: PathBuf, message: String },
    #[error("duplicate static Scope for Tunnel {tunnel_id}")]
    DuplicateTunnelId { tunnel_id: String },
    #[error("duplicate managed Scope for Tunnel {tunnel_id}")]
    DuplicateManagedTunnelId { tunnel_id: String },
    #[error("Tunnel {tunnel_id} cannot be present in static and managed Scope sources")]
    SourceConflict { tunnel_id: String },
    #[error("Tunnel {tunnel_id} issuer replacement requires a new Tunnel ID")]
    IssuerReplacement { tunnel_id: String },
    #[error(transparent)]
    InvalidProvisioning(#[from] ProvisioningError),
}
