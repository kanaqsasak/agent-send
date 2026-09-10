//! Isolated persistence for trusted-peer secrets.
//!
//! The default adapter is a local file, not an OS credential store. Keeping
//! this boundary separate lets a platform-backed store replace it without
//! changing pairing or socket authorization.

use crate::{PairedPeer, PairingSecret, PeerRecord};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PeerTrustStoreError {
    #[error("trusted-peer storage failed: {0}")]
    Io(#[from] io::Error),
    #[error("trusted-peer storage data is invalid: {0}")]
    Invalid(#[source] serde_json::Error),
    #[error("trusted-peer storage contains an invalid pairing secret")]
    InvalidSecret,
}

/// One persisted trusted peer. The secret is deliberately never included in
/// [`PeerRecord`] or peer-list API responses.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPeerTrust {
    pub record: PeerRecord,
    pairing_secret: String,
}

impl std::fmt::Debug for StoredPeerTrust {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredPeerTrust")
            .field("record", &self.record)
            .field("pairing_secret", &"[REDACTED]")
            .finish()
    }
}

impl StoredPeerTrust {
    pub(crate) fn new(record: PeerRecord, secret: &PairingSecret) -> Self {
        Self {
            record,
            pairing_secret: secret.to_hex(),
        }
    }

    pub(crate) fn paired_peer(&self) -> Result<PairedPeer, PeerTrustStoreError> {
        let secret = PairingSecret::from_hex(&self.pairing_secret)
            .map_err(|_| PeerTrustStoreError::InvalidSecret)?;
        PairedPeer::new(self.record.advertisement.id.clone(), secret)
            .map_err(|_| PeerTrustStoreError::InvalidSecret)
    }
}

/// Persistence boundary for trusted peer records and their pairing secrets.
pub trait PeerTrustStore: Send + Sync {
    fn load(&self) -> Result<Vec<StoredPeerTrust>, PeerTrustStoreError>;
    fn save(&self, peers: &[StoredPeerTrust]) -> Result<(), PeerTrustStoreError>;
}

/// Local JSON store for trusted-peer secrets.
///
/// Writes are atomic and new files are owner-readable/writable only on Unix.
/// This is intentionally not represented as an OS credential-store adapter.
pub struct FilePeerTrustStore {
    path: PathBuf,
}

impl FilePeerTrustStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PeerTrustStore for FilePeerTrustStore {
    fn load(&self) -> Result<Vec<StoredPeerTrust>, PeerTrustStoreError> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str(&contents).map_err(PeerTrustStoreError::Invalid),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, peers: &[StoredPeerTrust]) -> Result<(), PeerTrustStoreError> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_vec_pretty(peers).expect("trusted peers serialize");
        let temporary = temporary_path(&self.path);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let write_result = (|| -> io::Result<()> {
            let mut file = options.open(&temporary)?;
            file.write_all(&contents)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result.map_err(Into::into)
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let suffix = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("tmp-{}-{suffix}", std::process::id()))
}

#[derive(Debug, Default)]
pub struct MemoryPeerTrustStore(Mutex<Vec<StoredPeerTrust>>);

impl PeerTrustStore for MemoryPeerTrustStore {
    fn load(&self) -> Result<Vec<StoredPeerTrust>, PeerTrustStoreError> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn save(&self, peers: &[StoredPeerTrust]) -> Result<(), PeerTrustStoreError> {
        *self.0.lock().unwrap() = peers.to_vec();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PeerAdvertisement, API_VERSION};

    #[test]
    fn file_store_round_trips_trust_without_debugging_the_secret() {
        let path = std::env::temp_dir().join(format!(
            "agent-send-test-{}-peer-store.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let store = FilePeerTrustStore::new(&path);
        let trust = StoredPeerTrust::new(
            PeerRecord {
                advertisement: PeerAdvertisement {
                    id: "peer-store".into(),
                    alias: "Peer store".into(),
                    address: "127.0.0.1:8742".into(),
                    api_version: API_VERSION,
                },
                trusted: true,
            },
            &PairingSecret::new([9; 32]),
        );
        store.save(&[trust.clone()]).unwrap();
        assert_eq!(store.load().unwrap(), vec![trust.clone()]);
        assert!(!format!("{trust:?}").contains(&"09".repeat(32)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_file(path);
    }
}
