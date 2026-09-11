//! Owner-only storage for one signed-in Platform account.
//!
//! The token here is a bearer credential for the whole account: it can list
//! every Tunnel and add a Peer to any of them. It therefore lives under the
//! same owner-only file rules as a `.peer` profile, in the same config tree,
//! and is written through the same atomic no-follow replace.
//!
//! Nothing here refreshes. A Platform session has a fixed lifetime, the Client
//! records when it ends, and an expired session is signed out rather than
//! silently renewed — an owner who has not opened the app in a month should be
//! asked again, not kept logged in forever by a background request.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tp_client::platform_account::PlatformToken;

use crate::peer_store::{replace_private_json_file, PeerImportError};

const SESSION_FILE: &str = "platform-session.json";

/// What the Client remembers about a signed-in owner between launches.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformSession {
    /// Which Platform this token is for. A token minted by one Platform must
    /// never be sent to another, so the URL travels with it rather than being
    /// re-derived from settings that may have changed.
    pub platform_url: String,
    pub token: PlatformToken,
    /// Shown in the UI so the owner can tell which account they are on.
    /// Absent when the Platform did not answer the identity call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

fn session_path(client_config_root: &Path) -> PathBuf {
    client_config_root.join(SESSION_FILE)
}

/// Reads the stored session, treating anything unreadable as signed out.
///
/// A corrupt file must not wedge the app on its own launch path; the worst
/// case is that the owner signs in again.
pub fn load_platform_session(client_config_root: &Path) -> Option<PlatformSession> {
    let path = session_path(client_config_root);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.is_symlink() || !metadata.is_file() {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn store_platform_session(
    client_config_root: &Path,
    session: &PlatformSession,
) -> Result<(), PeerImportError> {
    replace_private_json_file(&session_path(client_config_root), session)
}

/// Signs out. Already signed out is the state the caller asked for.
pub fn clear_platform_session(client_config_root: &Path) -> Result<(), PeerImportError> {
    let path = session_path(client_config_root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PeerImportError::WriteDestination { path, source }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    fn session(expires_at_unix: u64) -> PlatformSession {
        PlatformSession {
            platform_url: "https://lantunnel.app".into(),
            token: PlatformToken {
                access_token: Zeroizing::new("session-token".into()),
                expires_at_unix,
            },
            email: Some("owner@example.com".into()),
        }
    }

    #[test]
    fn a_stored_session_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(load_platform_session(dir.path()).is_none());

        store_platform_session(dir.path(), &session(1_800_000_000)).expect("store");
        let loaded = load_platform_session(dir.path()).expect("loaded");
        assert_eq!(loaded.platform_url, "https://lantunnel.app");
        assert_eq!(loaded.token.access_token.as_str(), "session-token");
        assert_eq!(loaded.email.as_deref(), Some("owner@example.com"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(SESSION_FILE))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "an account token must not be group readable");
        }
    }

    #[test]
    fn signing_out_removes_the_token_and_is_repeatable() {
        let dir = tempfile::tempdir().expect("temp dir");
        store_platform_session(dir.path(), &session(1_800_000_000)).expect("store");
        clear_platform_session(dir.path()).expect("clear");
        assert!(load_platform_session(dir.path()).is_none());
        clear_platform_session(dir.path()).expect("clearing twice is not an error");
    }

    #[test]
    fn an_unreadable_session_reads_as_signed_out_rather_than_wedging_launch() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(SESSION_FILE), b"{ not json").expect("write");
        assert!(load_platform_session(dir.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_session_file_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let elsewhere = dir.path().join("elsewhere.json");
        fs::write(&elsewhere, br#"{"platform_url":"https://lantunnel.app","token":{"access_token":"t","expires_at_unix":1}}"#)
            .expect("write");
        std::os::unix::fs::symlink(&elsewhere, dir.path().join(SESSION_FILE)).expect("symlink");
        assert!(load_platform_session(dir.path()).is_none());
    }
}
