//! App-owned Lantunnel 2.0 settings compilation.
//!
//! The persisted values stay small and UI-shaped. Compilation validates the
//! whole V2 settings block before either disk or a live Engine is changed.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tp_client::access_policy::{
    ClientAccessPolicyErrorV2, ClientAccessPolicyV2, CompiledClientAccessPolicyV2,
};
use tp_client::peer_runtime::{
    LanExportPrefixV2, LanExportV2, LocalLanExportConfigV2, PeerRuntimeErrorV2, PeerRuntimeRecordV2,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettingsV2 {
    pub client_access: ClientAccessPolicyV2,
    pub exported_lans: Vec<String>,
    /// Export the private networks this machine is attached to, without the
    /// owner having to name them. Independent of `exported_lans`: turning it
    /// off withdraws only what it added.
    pub auto_export_current_lan: bool,
    pub tunnel_first: bool,
}

impl Default for ClientSettingsV2 {
    fn default() -> Self {
        Self {
            // An empty Allow list means every Peer in the Tunnel may reach this
            // Client. Reaching it already requires an issued Peer profile for
            // the same Tunnel, so refusing on top of that added no boundary —
            // it only made a fresh install silently unreachable until its owner
            // found this setting. A Deny rule, or any Allow rule, closes it.
            client_access: ClientAccessPolicyV2 {
                allow: Vec::new(),
                deny: Vec::new(),
            },
            exported_lans: Vec::new(),
            // The overwhelmingly common reason to install this is to reach the
            // machines beside this one. Making that wait until its owner found
            // this setting and typed their own subnet out by hand is a worse
            // default than sharing the network they are already on, and the
            // Tunnel is still the boundary: only Peers issued a profile for it
            // can use the Export.
            auto_export_current_lan: true,
            tunnel_first: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledClientSettingsV2 {
    pub client_access: ClientAccessPolicyV2,
    /// The owner's answer, which the Engine re-resolves on every interface
    /// scan so a machine that changes network republishes the one it is on.
    pub local_export_config: LocalLanExportConfigV2,
    /// The snapshot this compilation was resolved against, so installing it
    /// does not have to enumerate every interface a second time.
    pub connected_lans: Option<Vec<LanExportPrefixV2>>,
    pub local_runtime_record: PeerRuntimeRecordV2,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClientSettingsErrorV2 {
    #[error(transparent)]
    AccessPolicy(#[from] ClientAccessPolicyErrorV2),
    #[error("invalid Exported LAN at index {index}: {value:?}")]
    InvalidExport { index: usize, value: String },
    #[error(transparent)]
    RuntimeRecord(#[from] PeerRuntimeErrorV2),
}

pub fn compile_client_settings_v2(
    settings: &ClientSettingsV2,
) -> Result<CompiledClientSettingsV2, ClientSettingsErrorV2> {
    compile_client_settings_v2_with_connected_lans(settings, None)
}

/// Compile settings against one authoritative connected-LAN snapshot.
///
/// `None` means interface discovery was unavailable, so configured exports
/// remain valid but are withdrawn (`ready=false`). A configured prefix is
/// published only when the canonical configured prefix is exactly present in
/// the snapshot; containment in either direction is intentionally
/// insufficient. Automatic Exports come from the snapshot itself, so they are
/// exact by construction.
pub fn compile_client_settings_v2_with_connected_lans(
    settings: &ClientSettingsV2,
    connected_lans: Option<&[LanExportPrefixV2]>,
) -> Result<CompiledClientSettingsV2, ClientSettingsErrorV2> {
    CompiledClientAccessPolicyV2::compile(&settings.client_access)?;
    let configured = settings
        .exported_lans
        .iter()
        .enumerate()
        .map(|(index, value)| compile_export(index, value))
        .collect::<Result<Vec<_>, _>>()?;
    // The owner's own list is validated on its own terms — a repeated or
    // over-long list still rejects the whole block — so what this machine
    // happens to be attached to can never turn a valid list into an invalid
    // one, or an invalid one into a valid one.
    PeerRuntimeRecordV2::new(
        configured
            .iter()
            .copied()
            .map(|prefix| LanExportV2 {
                prefix,
                ready: false,
            })
            .collect(),
    )?;
    let local_export_config = LocalLanExportConfigV2 {
        configured,
        auto_current_lan: settings.auto_export_current_lan,
    };
    let local_runtime_record = local_export_config.resolve(connected_lans);
    Ok(CompiledClientSettingsV2 {
        client_access: settings.client_access.clone(),
        local_export_config,
        connected_lans: connected_lans.map(<[LanExportPrefixV2]>::to_vec),
        local_runtime_record,
    })
}

fn compile_export(index: usize, value: &str) -> Result<LanExportPrefixV2, ClientSettingsErrorV2> {
    let invalid = || ClientSettingsErrorV2::InvalidExport {
        index,
        value: value.to_owned(),
    };
    let (network, prefix_len) = value.split_once('/').ok_or_else(invalid)?;
    let network = network.parse().map_err(|_| invalid())?;
    let prefix_len = prefix_len.parse().map_err(|_| invalid())?;
    let prefix = LanExportPrefixV2::new(network, prefix_len).map_err(|_| invalid())?;
    if value != format!("{}/{}", prefix.network, prefix.prefix_len)
        || !prefix_is_entirely_rfc1918(prefix)
    {
        return Err(invalid());
    }
    Ok(prefix)
}

fn prefix_is_entirely_rfc1918(prefix: LanExportPrefixV2) -> bool {
    let mask = u32::MAX << (32 - prefix.prefix_len);
    let first = u32::from(prefix.network);
    let last = first | !mask;
    [
        (u32::from_be_bytes([10, 0, 0, 0]), 8_u8),
        (u32::from_be_bytes([172, 16, 0, 0]), 12_u8),
        (u32::from_be_bytes([192, 168, 0, 0]), 16_u8),
    ]
    .into_iter()
    .any(|(base, bits)| {
        let private_mask = u32::MAX << (32 - bits);
        first >= base && last <= base | !private_mask
    })
}
