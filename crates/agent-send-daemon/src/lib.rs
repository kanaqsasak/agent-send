//! The agent-send background daemon foundation.
//!
//! The local API is deliberately small and transport-independent types are kept
//! public so a different local transport can be added without changing daemon
//! state or configuration. Authenticated automation clients use `POST /v1/agent`
//! with an `Authorization: Bearer` header and JSON `{ "operation", "params" }`.
//! Supported agent operations are `peers.list`, `folders.list`,
//! `transfers.submit` (also `transfers.send`), `transfers.status`, and
//! `transfers.cancel`. Loopback pairing uses `POST /v1/pairings`,
//! `POST /v1/pairings/confirm`, and `DELETE /v1/peers/{peer_id}`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub mod automation;
pub mod discovery;
pub mod peer_storage;
pub mod peer_transport;
pub mod transfer;

pub use automation::{
    AgentScope, AgentTokenStore, AgentTransferState, AgentTransferStatus, AuditEntry,
    FileAgentTokenStore, FoldersResponse, IssuedAgentToken, MemoryAgentTokenStore,
};
pub use discovery::{
    DiscoveryError, MdnsDiscovery, MockPeerDiscovery, PeerDiscovery, MDNS_SERVICE_TYPE,
};
pub use peer_storage::{
    FilePeerTrustStore, MemoryPeerTrustStore, PeerTrustStore, PeerTrustStoreError, StoredPeerTrust,
};
pub use peer_transport::{
    send_socket_transfer, EncryptedPeerFrame, MockPeerTransport, PairedPeer, PairingSecret,
    PeerChannelError, PeerConnection, PeerConnectionError, PeerMessage, PeerTransport,
    PeerTransportError, SecurePeerChannel, SocketPeerTransport, SocketTransferError,
    PEER_CONNECT_TIMEOUT, PEER_IO_TIMEOUT, PEER_PROTOCOL_VERSION,
};
pub use transfer::{
    Cancellation, LoopbackTransport, TransferEngine, TransferError, TransferOutcome,
    TransferProgress, CHUNK_SIZE, MAX_TRANSFER_BYTES, MAX_TRANSFER_FILES,
};

pub const API_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// The local API must remain loopback-only.
    #[serde(default = "default_bind_addr")]
    pub bind_addr: SocketAddr,
    /// File containing the daemon's local identity placeholder.
    #[serde(default = "default_identity_path")]
    pub identity_path: PathBuf,
    /// LAN listener for encrypted paired-peer traffic. This is separate from
    /// the local automation API and may intentionally bind beyond loopback.
    #[serde(default = "default_peer_bind_addr")]
    pub peer_bind_addr: SocketAddr,
    /// Disable mDNS publication and browsing while retaining the paired-peer
    /// listener for explicit manual-address connections.
    #[serde(default = "default_discovery_enabled")]
    pub discovery_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            identity_path: default_identity_path(),
            peer_bind_addr: default_peer_bind_addr(),
            discovery_enabled: default_discovery_enabled(),
        }
    }
}

impl Config {
    /// The per-user configuration file used by the daemon entrypoint.
    pub fn default_path() -> PathBuf {
        user_data_dir().join("config.json")
    }

    /// Load the per-user config, treating a missing file as the default.
    pub fn load_user() -> Result<Self, DaemonError> {
        let path = Self::default_path();
        match fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).map_err(|error| {
                DaemonError::Config(io::Error::new(io::ErrorKind::InvalidData, error))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(DaemonError::Config(error)),
        }
    }

    pub fn is_loopback(&self) -> bool {
        self.bind_addr.ip().is_loopback()
    }
}

impl Config {
    pub fn with_identity_path(path: impl Into<PathBuf>) -> Self {
        Self {
            identity_path: path.into(),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), DaemonError> {
        if !self.bind_addr.ip().is_loopback() {
            return Err(DaemonError::NonLoopbackBind(self.bind_addr));
        }
        if self.identity_path.as_os_str().is_empty() {
            return Err(DaemonError::MissingIdentityPath);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalIdentity {
    pub id: String,
    /// Placeholder for the future device key. It is persisted now so identity
    /// remains stable across restarts, without pretending to be a key yet.
    pub key_placeholder: String,
}

/// Metadata advertised by a peer. Discovery is deliberately transport-neutral;
/// an mDNS adapter can feed these records into the registry later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerAdvertisement {
    pub id: String,
    pub alias: String,
    pub address: String,
    pub api_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerRecord {
    pub advertisement: PeerAdvertisement,
    pub trusted: bool,
}

/// Pairing material returned only to the loopback client that creates an
/// invitation. Convey `pairing_secret` to the other device out of band, then
/// have both local clients confirm the same short code.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingResponse {
    pub peer_id: String,
    pub code: String,
    pub pairing_secret: String,
    pub expires_in_seconds: u32,
}

impl std::fmt::Debug for PairingResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairingResponse")
            .field("peer_id", &self.peer_id)
            .field("code", &self.code)
            .field("pairing_secret", &"[REDACTED]")
            .field("expires_in_seconds", &self.expires_in_seconds)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeersResponse {
    pub version: u32,
    pub peers: Vec<PeerRecord>,
}

#[derive(Debug, Deserialize)]
struct ConfirmPairing {
    peer_id: String,
    code: String,
}

#[derive(Debug, Deserialize)]
struct PairingRequest {
    #[serde(flatten)]
    advertisement: PeerAdvertisement,
    /// Supplying both fields accepts material generated by the other daemon.
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    pairing_secret: Option<String>,
}

const PAIRING_TTL_SECONDS: u64 = 300;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PairingError {
    #[error("peer ID must not be empty")]
    InvalidPeerId,
    #[error("pairing code must be six decimal digits")]
    InvalidCode,
    #[error("pairing code and secret must be supplied together")]
    IncompleteMaterial,
    #[error(transparent)]
    Secret(#[from] PeerChannelError),
}

#[derive(Debug, Clone)]
struct PendingPairing {
    code: String,
    secret: PairingSecret,
    expires_at: u64,
}

/// In-memory pairing state. Pending codes are deliberately not persisted;
/// restart invalidates them. Confirmed secrets are persisted through
/// `PeerTrustStore` by the daemon.
#[derive(Debug, Clone, Default)]
pub struct PeerRegistry {
    peers: BTreeMap<String, PeerRecord>,
    trusted: BTreeMap<String, PairedPeer>,
    pending: BTreeMap<String, PendingPairing>,
}

impl PeerRegistry {
    pub fn list(&self) -> Vec<PeerRecord> {
        self.peers.values().cloned().collect()
    }

    pub fn advertise(&mut self, advertisement: PeerAdvertisement) {
        let id = advertisement.id.clone();
        let trusted = self.trusted.contains_key(&id);
        self.peers.insert(
            id,
            PeerRecord {
                advertisement,
                trusted,
            },
        );
    }

    pub fn request_pairing(
        &mut self,
        advertisement: PeerAdvertisement,
        now: u64,
    ) -> Result<PairingResponse, PairingError> {
        self.request_pairing_with_material(advertisement, now, None)
    }

    pub fn request_pairing_with_material(
        &mut self,
        advertisement: PeerAdvertisement,
        now: u64,
        material: Option<(String, PairingSecret)>,
    ) -> Result<PairingResponse, PairingError> {
        if advertisement.id.trim().is_empty() {
            return Err(PairingError::InvalidPeerId);
        }
        let id = advertisement.id.clone();
        self.advertise(advertisement);
        let (code, secret) = match material {
            Some((code, secret)) => {
                validate_pairing_code(&code)?;
                (code, secret)
            }
            None => (new_pairing_code()?, PairingSecret::generate()?),
        };
        self.pending.insert(
            id.clone(),
            PendingPairing {
                code: code.clone(),
                secret: secret.clone(),
                expires_at: now.saturating_add(PAIRING_TTL_SECONDS),
            },
        );
        Ok(PairingResponse {
            peer_id: id,
            code,
            pairing_secret: secret.to_hex(),
            expires_in_seconds: PAIRING_TTL_SECONDS as u32,
        })
    }

    pub fn confirm_pairing(&mut self, peer_id: &str, code: &str, now: u64) -> bool {
        let Some(pending) = self.pending.get(peer_id) else {
            return false;
        };
        if now >= pending.expires_at || pending.code != code {
            if now >= pending.expires_at {
                self.pending.remove(peer_id);
            }
            return false;
        }
        let pending = self
            .pending
            .remove(peer_id)
            .expect("pending pairing checked");
        let Some(record) = self.peers.get_mut(peer_id) else {
            return false;
        };
        record.trusted = true;
        self.trusted.insert(
            peer_id.to_owned(),
            PairedPeer::new(peer_id, pending.secret).expect("pending peer ID is nonempty"),
        );
        true
    }

    pub fn revoke(&mut self, peer_id: &str) -> bool {
        self.pending.remove(peer_id);
        let removed = self.trusted.remove(peer_id).is_some();
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.trusted = false;
        }
        removed
    }

    fn paired_peer(&self, peer_id: &str) -> Option<PairedPeer> {
        self.trusted.get(peer_id).cloned()
    }

    fn stored_trust(&self) -> Vec<StoredPeerTrust> {
        self.trusted
            .iter()
            .filter_map(|(id, secret)| {
                self.peers
                    .get(id)
                    .cloned()
                    .map(|record| StoredPeerTrust::new(record, secret.secret()))
            })
            .collect()
    }

    fn restore_trusted(
        &mut self,
        records: Vec<StoredPeerTrust>,
    ) -> Result<(), PeerTrustStoreError> {
        for stored in records {
            if !stored.record.trusted {
                return Err(PeerTrustStoreError::InvalidSecret);
            }
            let peer = stored.paired_peer()?;
            let id = stored.record.advertisement.id.clone();
            if self.trusted.insert(id.clone(), peer).is_some() {
                return Err(PeerTrustStoreError::InvalidSecret);
            }
            self.peers.insert(id, stored.record);
        }
        Ok(())
    }
}

fn validate_pairing_code(code: &str) -> Result<(), PairingError> {
    if code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(PairingError::InvalidCode)
    }
}

fn new_pairing_code() -> Result<String, PairingError> {
    const RANGE: u32 = 1_000_000;
    const LIMIT: u32 = u32::MAX - (u32::MAX % RANGE);
    loop {
        let mut bytes = [0; 4];
        getrandom::getrandom(&mut bytes).map_err(|_| PeerChannelError::Random)?;
        let value = u32::from_be_bytes(bytes);
        if value < LIMIT {
            return Ok(format!("{:06}", value % RANGE));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub version: u32,
    pub status: HealthStatus,
    pub identity_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon API must bind to loopback, not {0}")]
    NonLoopbackBind(SocketAddr),
    #[error("identity path is required")]
    MissingIdentityPath,
    #[error("configuration storage failed: {0}")]
    Config(#[source] io::Error),
    #[error("identity storage failed: {0}")]
    Identity(#[source] io::Error),
    #[error(transparent)]
    PeerStorage(#[from] PeerTrustStoreError),
    #[error(transparent)]
    Pairing(#[from] PairingError),
    #[error("failed to start local API: {0}")]
    Bind(#[source] io::Error),
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error("peer is not trusted: {0}")]
    UntrustedPeer(String),
    #[error(transparent)]
    PeerChannel(#[from] PeerChannelError),
    #[error(transparent)]
    SocketTransfer(#[from] SocketTransferError),
    #[error(transparent)]
    Automation(#[from] automation::AutomationError),
    #[error("daemon thread failed to stop")]
    Shutdown,
}

pub struct Daemon {
    config: Config,
    identity: LocalIdentity,
    registry: Arc<Mutex<PeerRegistry>>,
    transfer: Arc<transfer::TransferEngine>,
    agent_tokens: Arc<automation::AgentTokens>,
    agent_transfers: Arc<automation::AgentTransfers>,
    audit: Arc<automation::AuditLog>,
    peer_store: Arc<dyn PeerTrustStore>,
}

impl Daemon {
    pub fn new(config: Config) -> Result<Self, DaemonError> {
        let token_store = Arc::new(automation::FileAgentTokenStore::new(
            automation::token_store_path(&config.identity_path),
        ));
        let peer_store = Arc::new(FilePeerTrustStore::new(trusted_peers_path(
            &config.identity_path,
        )));
        Self::with_stores(config, token_store, peer_store)
    }

    /// Construct a daemon with an explicit token persistence adapter. This
    /// lets platform credential stores replace the default local file without
    /// altering token scopes or local API authorization.
    pub fn with_token_store(
        config: Config,
        token_store: Arc<dyn automation::AgentTokenStore>,
    ) -> Result<Self, DaemonError> {
        let peer_store = Arc::new(FilePeerTrustStore::new(trusted_peers_path(
            &config.identity_path,
        )));
        Self::with_stores(config, token_store, peer_store)
    }

    /// Construct a daemon with an explicit trusted-peer persistence adapter.
    /// The default is a permission-restricted local file, not an OS credential
    /// store; this seam exists so a platform store can be added later.
    pub fn with_peer_trust_store(
        config: Config,
        peer_store: Arc<dyn PeerTrustStore>,
    ) -> Result<Self, DaemonError> {
        let token_store = Arc::new(automation::FileAgentTokenStore::new(
            automation::token_store_path(&config.identity_path),
        ));
        Self::with_stores(config, token_store, peer_store)
    }

    pub fn with_stores(
        config: Config,
        token_store: Arc<dyn automation::AgentTokenStore>,
        peer_store: Arc<dyn PeerTrustStore>,
    ) -> Result<Self, DaemonError> {
        config.validate()?;
        let identity = load_or_create_identity(&config.identity_path)?;
        let registry = load_trusted_peers(peer_store.as_ref())?;
        Ok(Self {
            config,
            identity,
            registry: Arc::new(Mutex::new(registry)),
            transfer: Arc::new(transfer::TransferEngine::new()),
            agent_tokens: Arc::new(automation::AgentTokens::load(token_store)?),
            agent_transfers: Arc::new(automation::AgentTransfers::default()),
            audit: Arc::new(automation::AuditLog::default()),
            peer_store,
        })
    }

    /// Issue a local bearer token with explicit peer and folder capabilities.
    /// Callers must retain the raw token; only a one-way hash is persisted.
    pub fn issue_agent_token(&self, scope: AgentScope) -> Result<IssuedAgentToken, DaemonError> {
        Ok(self.agent_tokens.issue(scope)?)
    }

    pub fn revoke_agent_token(&self, id: &str) -> Result<bool, DaemonError> {
        Ok(self.agent_tokens.revoke(id)?)
    }

    pub fn audit_entries(&self) -> Vec<AuditEntry> {
        self.audit.entries()
    }

    pub fn peers(&self) -> Vec<PeerRecord> {
        self.registry.lock().unwrap().list()
    }

    pub fn advertise_peer(&mut self, advertisement: PeerAdvertisement) {
        self.registry.lock().unwrap().advertise(advertisement);
    }

    /// Publish local LAN presence through a discovery adapter. Advertisements
    /// contain no trust decision or pairing secret.
    pub fn publish_presence<D: PeerDiscovery>(
        &self,
        discovery: &mut D,
        advertisement: &PeerAdvertisement,
    ) -> Result<(), DaemonError> {
        discovery.publish(advertisement)?;
        Ok(())
    }

    /// Pull untrusted LAN advertisements into the ephemeral registry.
    pub fn discover_peers<D: PeerDiscovery>(
        &mut self,
        discovery: &mut D,
    ) -> Result<usize, DaemonError> {
        let advertisements = discovery.discover()?;
        let count = advertisements.len();
        for advertisement in advertisements {
            self.registry.lock().unwrap().advertise(advertisement);
        }
        Ok(count)
    }

    /// Construct a cryptographic channel from the confirmed, persisted trust
    /// record. Callers cannot inject test-only secrets into socket transport.
    pub fn secure_peer_channel(&self, peer_id: &str) -> Result<SecurePeerChannel, DaemonError> {
        let peer = self
            .registry
            .lock()
            .unwrap()
            .paired_peer(peer_id)
            .ok_or_else(|| DaemonError::UntrustedPeer(peer_id.to_owned()))?;
        Ok(SecurePeerChannel::new(self.identity.id.clone(), peer)?)
    }

    /// Stream one transfer through the authenticated paired-peer TCP listener.
    /// Source and destination folder checks remain in `TransferEngine`; this
    /// method only selects a trusted discovered or manually entered endpoint.
    pub fn send_to_peer_socket<F>(
        &self,
        peer_id: &str,
        request: &agent_send_core::TransferRequest,
        cancel: &Cancellation,
        progress: F,
    ) -> Result<TransferOutcome, DaemonError>
    where
        F: FnMut(TransferProgress),
    {
        if request.peer_id != peer_id {
            return Err(DaemonError::UntrustedPeer(peer_id.to_owned()));
        }
        let address = self
            .registry
            .lock()
            .unwrap()
            .peers
            .get(peer_id)
            .filter(|record| record.trusted)
            .ok_or_else(|| DaemonError::UntrustedPeer(peer_id.to_owned()))?
            .advertisement
            .address
            .parse::<SocketAddr>()
            .map_err(|_| DaemonError::UntrustedPeer(peer_id.to_owned()))?;
        let channel = self.secure_peer_channel(peer_id)?;
        Ok(send_socket_transfer(
            &self.transfer,
            channel,
            address,
            request,
            cancel,
            progress,
        )?)
    }

    pub fn request_pairing(
        &mut self,
        advertisement: PeerAdvertisement,
    ) -> Result<PairingResponse, DaemonError> {
        Ok(self
            .registry
            .lock()
            .unwrap()
            .request_pairing(advertisement, now_seconds())?)
    }

    /// Accept invitation material generated by the other device. Both values
    /// must be conveyed out of band and confirmed before any trust is saved.
    pub fn request_pairing_with_material(
        &mut self,
        advertisement: PeerAdvertisement,
        code: String,
        pairing_secret: String,
    ) -> Result<PairingResponse, DaemonError> {
        let secret = PairingSecret::from_hex(&pairing_secret)?;
        Ok(self
            .registry
            .lock()
            .unwrap()
            .request_pairing_with_material(advertisement, now_seconds(), Some((code, secret)))?)
    }

    pub fn confirm_pairing(&mut self, peer_id: &str, code: &str) -> Result<bool, DaemonError> {
        let mut registry = self.registry.lock().unwrap();
        let before = registry.clone();
        let confirmed = registry.confirm_pairing(peer_id, code, now_seconds());
        if confirmed {
            if let Err(error) = self.peer_store.save(&registry.stored_trust()) {
                *registry = before;
                return Err(error.into());
            }
        }
        Ok(confirmed)
    }

    pub fn revoke_peer(&mut self, peer_id: &str) -> Result<bool, DaemonError> {
        let mut registry = self.registry.lock().unwrap();
        let before = registry.clone();
        let revoked = registry.revoke(peer_id);
        if revoked {
            if let Err(error) = self.peer_store.save(&registry.stored_trust()) {
                *registry = before;
                return Err(error.into());
            }
        }
        Ok(revoked)
    }

    pub fn identity(&self) -> &LocalIdentity {
        &self.identity
    }

    /// Register a named capability used by the transfer API. The engine never
    /// accepts an unscoped filesystem path.
    pub fn add_shared_folder(
        &self,
        id: impl Into<String>,
        root: impl Into<PathBuf>,
        direction: agent_send_core::FolderDirection,
    ) {
        self.transfer.add_folder(
            id,
            agent_send_core::path_policy::PathPolicy::new(root, direction),
        );
    }

    pub fn transfer_engine(&self) -> &transfer::TransferEngine {
        self.transfer.as_ref()
    }

    /// Version-independent local client entry point for the current loopback
    /// transport. Network transports can implement the same manifest/chunk seam.
    pub fn send_to<F>(
        &self,
        receiver: &Daemon,
        request: &agent_send_core::TransferRequest,
        cancel: &Cancellation,
        progress: F,
    ) -> Result<TransferOutcome, TransferError>
    where
        F: FnMut(TransferProgress),
    {
        LoopbackTransport::send(
            &self.transfer,
            &receiver.transfer,
            request,
            cancel,
            progress,
        )
    }

    /// Start the loopback automation API, paired-peer socket listener, and
    /// best-effort mDNS lifecycle. Discovery failure never broadens the local
    /// API bind or disables explicit manual-address peer connections.
    pub fn start(&self) -> Result<RunningDaemon, DaemonError> {
        let discovery = if self.config.discovery_enabled {
            MdnsDiscovery::new()
                .ok()
                .map(|discovery| Box::new(discovery) as Box<dyn PeerDiscovery>)
        } else {
            None
        };
        self.start_inner(discovery)
    }

    /// Start with a caller-provided discovery adapter. This is primarily the
    /// deterministic integration seam used by socket lifecycle tests.
    pub fn start_with_discovery<D: PeerDiscovery + 'static>(
        &self,
        discovery: D,
    ) -> Result<RunningDaemon, DaemonError> {
        self.start_inner(Some(Box::new(discovery)))
    }

    fn start_inner(
        &self,
        discovery: Option<Box<dyn PeerDiscovery>>,
    ) -> Result<RunningDaemon, DaemonError> {
        let local_listener = TcpListener::bind(self.config.bind_addr).map_err(DaemonError::Bind)?;
        local_listener
            .set_nonblocking(true)
            .map_err(DaemonError::Bind)?;
        let local_addr = local_listener.local_addr().map_err(DaemonError::Bind)?;
        let peer_listener =
            TcpListener::bind(self.config.peer_bind_addr).map_err(DaemonError::Bind)?;
        peer_listener
            .set_nonblocking(true)
            .map_err(DaemonError::Bind)?;
        let peer_addr = peer_listener.local_addr().map_err(DaemonError::Bind)?;
        let advertisement = PeerAdvertisement {
            id: self.identity.id.clone(),
            alias: format!("agent-send-{}", self.identity.id),
            address: peer_addr.to_string(),
            api_version: API_VERSION,
        };
        let audit = self.audit.clone();
        let mut shutdowns = Vec::new();
        let mut threads = Vec::new();

        let (local_shutdown_tx, local_shutdown_rx) = mpsc::channel();
        shutdowns.push(local_shutdown_tx);
        let local_thread = thread::Builder::new()
            .name("agent-send-local-api".into())
            .spawn({
                let identity = self.identity.clone();
                let registry = self.registry.clone();
                let transfer = self.transfer.clone();
                let agent_tokens = self.agent_tokens.clone();
                let agent_transfers = self.agent_transfers.clone();
                let server_audit = self.audit.clone();
                let peer_store = self.peer_store.clone();
                move || {
                    run_server(
                        local_listener,
                        identity,
                        registry,
                        transfer,
                        agent_tokens,
                        agent_transfers,
                        server_audit,
                        peer_store,
                        local_shutdown_rx,
                    )
                }
            })
            .map_err(DaemonError::Bind)?;
        threads.push(local_thread);

        let (peer_shutdown_tx, peer_shutdown_rx) = mpsc::channel();
        shutdowns.push(peer_shutdown_tx);
        let peer_thread = thread::Builder::new()
            .name("agent-send-peer-listener".into())
            .spawn({
                let identity = self.identity.clone();
                let registry = self.registry.clone();
                let transfer = self.transfer.clone();
                move || {
                    run_peer_server(
                        peer_listener,
                        identity,
                        registry,
                        transfer,
                        peer_shutdown_rx,
                    )
                }
            })
            .map_err(DaemonError::Bind)?;
        threads.push(peer_thread);

        if let Some(discovery) = discovery {
            let (discovery_shutdown_tx, discovery_shutdown_rx) = mpsc::channel();
            shutdowns.push(discovery_shutdown_tx);
            let discovery_thread = thread::Builder::new()
                .name("agent-send-discovery".into())
                .spawn({
                    let registry = self.registry.clone();
                    move || run_discovery(discovery, advertisement, registry, discovery_shutdown_rx)
                })
                .map_err(DaemonError::Bind)?;
            threads.push(discovery_thread);
        }

        Ok(RunningDaemon {
            local_addr,
            peer_addr,
            audit,
            shutdowns,
            threads,
        })
    }
}

pub struct RunningDaemon {
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
    audit: Arc<automation::AuditLog>,
    shutdowns: Vec<Sender<()>>,
    threads: Vec<JoinHandle<()>>,
}

impl RunningDaemon {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The LAN listener address. This is never used for the loopback-only
    /// automation API.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    pub fn audit_entries(&self) -> Vec<AuditEntry> {
        self.audit.entries()
    }

    /// Signals every owned service and waits for their workers to exit.
    pub fn shutdown(mut self) -> Result<(), DaemonError> {
        for sender in self.shutdowns.drain(..) {
            let _ = sender.send(());
        }
        for thread in self.threads.drain(..) {
            thread.join().map_err(|_| DaemonError::Shutdown)?;
        }
        Ok(())
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        for sender in self.shutdowns.drain(..) {
            let _ = sender.send(());
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn run_discovery(
    mut discovery: Box<dyn PeerDiscovery>,
    advertisement: PeerAdvertisement,
    registry: Arc<Mutex<PeerRegistry>>,
    shutdown: mpsc::Receiver<()>,
) {
    if discovery.publish(&advertisement).is_err() {
        return;
    }
    loop {
        if shutdown.try_recv().is_ok() {
            return;
        }
        if let Ok(advertisements) = discovery.discover() {
            let mut peers = registry.lock().unwrap();
            for advertisement in advertisements {
                peers.advertise(advertisement);
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn run_peer_server(
    listener: TcpListener,
    identity: LocalIdentity,
    registry: Arc<Mutex<PeerRegistry>>,
    transfer: Arc<TransferEngine>,
    shutdown: mpsc::Receiver<()>,
) {
    loop {
        if shutdown.try_recv().is_ok() {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                handle_peer_connection(stream, &identity, &registry, transfer.clone())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

fn handle_peer_connection(
    stream: TcpStream,
    identity: &LocalIdentity,
    registry: &Arc<Mutex<PeerRegistry>>,
    transfer: Arc<TransferEngine>,
) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(peer_transport::PEER_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(peer_transport::PEER_IO_TIMEOUT));
    let mut transport = SocketPeerTransport::from_stream(stream);
    let Ok(Some(first_frame)) = transport.receive() else {
        return;
    };
    let peer_id = first_frame.sender_id.clone();
    let Some(peer) = registry.lock().unwrap().paired_peer(&peer_id) else {
        return;
    };
    let Ok(mut channel) = SecurePeerChannel::new(identity.id.clone(), peer) else {
        return;
    };
    if !matches!(channel.open(first_frame), Ok(PeerMessage::Hello { protocol_version }) if protocol_version == PEER_PROTOCOL_VERSION)
    {
        return;
    }
    let mut connection = PeerConnection::new(channel, transport);
    let _ = peer_transport::receive_socket_transfer(&mut connection, transfer);
}

fn run_server(
    listener: TcpListener,
    identity: LocalIdentity,
    registry: Arc<Mutex<PeerRegistry>>,
    transfer: Arc<TransferEngine>,
    agent_tokens: Arc<automation::AgentTokens>,
    agent_transfers: Arc<automation::AgentTransfers>,
    audit: Arc<automation::AuditLog>,
    peer_store: Arc<dyn PeerTrustStore>,
    shutdown: mpsc::Receiver<()>,
) {
    loop {
        if shutdown.try_recv().is_ok() {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => handle_connection(
                stream,
                &identity,
                &registry,
                &transfer,
                &agent_tokens,
                &agent_transfers,
                &audit,
                peer_store.as_ref(),
            ),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    identity: &LocalIdentity,
    registry: &Arc<Mutex<PeerRegistry>>,
    transfer: &TransferEngine,
    agent_tokens: &automation::AgentTokens,
    agent_transfers: &automation::AgentTransfers,
    audit: &automation::AuditLog,
    peer_store: &dyn PeerTrustStore,
) {
    // TCP does not preserve HTTP request boundaries. Read complete headers and
    // any declared body before responding so a segmented local request is not
    // misparsed or closed with unread client data.
    const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    // Accepted sockets inherit the nonblocking setting of the listener on
    // supported platforms; local request reads need a bounded blocking mode.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let mut request = Vec::new();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut buffer = [0; 8192];
    while request.len() < MAX_REQUEST_BYTES {
        let Ok(size) = stream.read(&mut buffer) else {
            return;
        };
        if size == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..size]);
        if request_is_complete(&request) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&request);
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default();
    let first_line = head.lines().next().unwrap_or_default();
    let method_path = first_line.split_whitespace().take(2).collect::<Vec<_>>();
    let (status, body) = match (method_path.first().copied(), method_path.get(1).copied()) {
        (Some("OPTIONS"), _) => ("204 No Content", String::new()),
        (Some("GET"), Some("/v1/health")) => (
            "200 OK",
            serde_json::to_string(&HealthResponse {
                version: API_VERSION,
                status: HealthStatus::Ok,
                identity_id: identity.id.clone(),
            })
            .unwrap(),
        ),
        (Some("GET"), Some("/v1/peers")) => {
            let peers = registry.lock().unwrap().list();
            (
                "200 OK",
                serde_json::to_string(&PeersResponse {
                    version: API_VERSION,
                    peers,
                })
                .unwrap(),
            )
        }
        (Some("POST"), Some("/v1/agent")) => handle_agent_request(
            head,
            body,
            registry,
            transfer,
            agent_tokens,
            agent_transfers,
            audit,
        ),
        (Some("POST"), Some("/v1/pairings")) | (Some("POST"), Some("/v1/pairing")) => {
            match serde_json::from_str::<PairingRequest>(body) {
                Ok(request) => {
                    let material = match (request.code, request.pairing_secret) {
                        (None, None) => Ok(None),
                        (Some(code), Some(secret)) => PairingSecret::from_hex(&secret)
                            .map(|secret| Some((code, secret)))
                            .map_err(|_| {
                                PairingError::Secret(PeerChannelError::InvalidPairingSecret)
                            }),
                        _ => Err(PairingError::IncompleteMaterial),
                    };
                    match material.and_then(|material| {
                        registry.lock().unwrap().request_pairing_with_material(
                            request.advertisement,
                            now_seconds(),
                            material,
                        )
                    }) {
                        Ok(response) => ("200 OK", serde_json::to_string(&response).unwrap()),
                        Err(_) => ("400 Bad Request", "{\"error\":\"invalid_request\"}".into()),
                    }
                }
                Err(_) => ("400 Bad Request", "{\"error\":\"invalid_request\"}".into()),
            }
        }
        (Some("POST"), Some("/v1/pairings/confirm"))
        | (Some("POST"), Some("/v1/pairing/confirm")) => {
            match serde_json::from_str::<ConfirmPairing>(body) {
                Ok(request) => {
                    let mut peers = registry.lock().unwrap();
                    let before = peers.clone();
                    let confirmed =
                        peers.confirm_pairing(&request.peer_id, &request.code, now_seconds());
                    if confirmed && peer_store.save(&peers.stored_trust()).is_err() {
                        *peers = before;
                        (
                            "500 Internal Server Error",
                            "{\"error\":\"storage_failed\"}".into(),
                        )
                    } else if confirmed {
                        ("200 OK", "{\"confirmed\":true}".into())
                    } else {
                        ("400 Bad Request", "{\"confirmed\":false}".into())
                    }
                }
                Err(_) => ("400 Bad Request", "{\"error\":\"invalid_request\"}".into()),
            }
        }
        (Some("DELETE"), Some(path)) if path.starts_with("/v1/peers/") => {
            let id = &path["/v1/peers/".len()..];
            let mut peers = registry.lock().unwrap();
            let before = peers.clone();
            let revoked = peers.revoke(id);
            if revoked && peer_store.save(&peers.stored_trust()).is_err() {
                *peers = before;
                (
                    "500 Internal Server Error",
                    "{\"error\":\"storage_failed\"}".into(),
                )
            } else if revoked {
                ("200 OK", "{\"revoked\":true}".into())
            } else {
                ("404 Not Found", "{\"error\":\"peer_not_found\"}".into())
            }
        }
        _ => ("404 Not Found", "{\"error\":\"not_found\"}".to_owned()),
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

#[derive(Debug, Deserialize)]
struct AgentOperationRequest {
    operation: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct TransferIdParams {
    transfer_id: String,
}

fn handle_agent_request(
    head: &str,
    body: &str,
    registry: &Arc<Mutex<PeerRegistry>>,
    transfer: &TransferEngine,
    agent_tokens: &automation::AgentTokens,
    agent_transfers: &automation::AgentTransfers,
    audit: &automation::AuditLog,
) -> (&'static str, String) {
    let request = match serde_json::from_str::<AgentOperationRequest>(body) {
        Ok(request) => request,
        Err(_) => {
            audit.record(
                "unauthenticated",
                "invalid_request",
                None,
                Vec::new(),
                None,
                "denied",
            );
            return agent_error("400 Bad Request", "invalid_request");
        }
    };
    let token = header_value(head, "authorization").and_then(|value| value.strip_prefix("Bearer "));
    let Some(actor) = token.and_then(|token| agent_tokens.authorize(token)) else {
        audit.record(
            "unauthenticated",
            request.operation,
            None,
            Vec::new(),
            None,
            "denied",
        );
        return agent_error("401 Unauthorized", "unauthorized");
    };
    let actor_id = actor.id.clone();
    let operation = request.operation.as_str();
    let response = match operation {
        "peers.list" => {
            let peers = registry
                .lock()
                .unwrap()
                .list()
                .into_iter()
                .filter(|peer| peer.trusted && actor.scope.allows_peer(&peer.advertisement.id))
                .collect::<Vec<_>>();
            audit.record(actor_id, operation, None, Vec::new(), None, "ok");
            Ok(serde_json::to_value(PeersResponse {
                version: API_VERSION,
                peers,
            })
            .expect("peer response serializes"))
        }
        "folders.list" => {
            let folders = transfer
                .folders()
                .into_iter()
                .filter(|(id, _)| actor.scope.allows_folder(id))
                .map(|(id, direction)| automation::FolderResponse { id, direction })
                .collect();
            audit.record(actor_id, operation, None, Vec::new(), None, "ok");
            Ok(serde_json::to_value(FoldersResponse {
                version: API_VERSION,
                folders,
            })
            .expect("folder response serializes"))
        }
        "transfers.submit" | "transfers.send" => {
            let submission =
                match serde_json::from_value::<agent_send_core::TransferRequest>(request.params) {
                    Ok(submission) => submission,
                    Err(_) => {
                        audit.record(actor_id, operation, None, Vec::new(), None, "denied");
                        return agent_error("400 Bad Request", "invalid_request");
                    }
                };
            let peer_id = Some(submission.peer_id.clone());
            let folder_ids = vec![
                submission.source_folder_id.clone(),
                submission.destination_folder_id.clone(),
            ];
            let result = agent_transfers.submit(
                &actor,
                &submission,
                &registry.lock().unwrap().list(),
                transfer,
            );
            audit.record(
                actor_id,
                operation,
                peer_id,
                folder_ids,
                result
                    .as_ref()
                    .ok()
                    .map(|status| status.transfer_id.clone()),
                if result.is_ok() { "ok" } else { "denied" },
            );
            result.and_then(|status| {
                serde_json::to_value(status)
                    .map_err(|_| automation::AutomationError::ScopeDenied("serialization".into()))
            })
        }
        "transfers.status" | "transfers.cancel" => {
            let params = match serde_json::from_value::<TransferIdParams>(request.params) {
                Ok(params) => params,
                Err(_) => {
                    audit.record(actor_id, operation, None, Vec::new(), None, "denied");
                    return agent_error("400 Bad Request", "invalid_request");
                }
            };
            let result = if operation == "transfers.status" {
                agent_transfers.status(&actor, &params.transfer_id)
            } else {
                agent_transfers.cancel(&actor, &params.transfer_id)
            };
            audit.record(
                actor_id,
                operation,
                None,
                Vec::new(),
                Some(params.transfer_id),
                if result.is_ok() { "ok" } else { "denied" },
            );
            result.and_then(|status| {
                serde_json::to_value(status)
                    .map_err(|_| automation::AutomationError::ScopeDenied("serialization".into()))
            })
        }
        _ => {
            audit.record(actor_id, operation, None, Vec::new(), None, "denied");
            return agent_error("404 Not Found", "unknown_operation");
        }
    };
    match response {
        Ok(result) => (
            "200 OK",
            serde_json::json!({ "version": API_VERSION, "result": result }).to_string(),
        ),
        Err(error) => {
            let (status, code) = match error {
                automation::AutomationError::UntrustedPeer(_)
                | automation::AutomationError::ScopeDenied(_) => ("403 Forbidden", "forbidden"),
                automation::AutomationError::TransferNotFound => {
                    ("404 Not Found", "transfer_not_found")
                }
                automation::AutomationError::Transfer(_) => ("400 Bad Request", "invalid_transfer"),
                _ => ("400 Bad Request", "invalid_request"),
            };
            agent_error(status, code)
        }
    }
}

fn agent_error(status: &'static str, code: &str) -> (&'static str, String) {
    (status, serde_json::json!({ "error": code }).to_string())
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

fn request_is_complete(request: &[u8]) -> bool {
    let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&request[..header_end]);
    let content_length = head
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
        })
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    request.len() >= header_end + 4 + content_length
}

fn load_or_create_identity(path: &Path) -> Result<LocalIdentity, DaemonError> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(|error| {
            DaemonError::Identity(io::Error::new(io::ErrorKind::InvalidData, error))
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(DaemonError::Identity)?;
            }
            let identity = new_identity();
            let contents = serde_json::to_vec_pretty(&identity).expect("identity is serializable");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(DaemonError::Identity)?;
            file.write_all(&contents).map_err(DaemonError::Identity)?;
            Ok(identity)
        }
        Err(error) => Err(DaemonError::Identity(error)),
    }
}

fn new_identity() -> LocalIdentity {
    let mut bytes = [0u8; 16];
    let value = if getrandom::getrandom(&mut bytes).is_ok() {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    } else {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{nanos:032x}")
    };
    LocalIdentity {
        id: format!("device-{}", &value[..16]),
        key_placeholder: value,
    }
}

fn trusted_peers_path(identity_path: &Path) -> PathBuf {
    // Keep custom identity files isolated from one another. In particular, two
    // daemons in the same directory must not race while persisting their peer
    // registries (the default identity keeps the conventional sibling name).
    if identity_path.file_name().and_then(|name| name.to_str()) == Some("identity.json") {
        identity_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("trusted-peers.json")
    } else {
        identity_path.with_extension("trusted-peers.json")
    }
}

fn load_trusted_peers(store: &dyn PeerTrustStore) -> Result<PeerRegistry, DaemonError> {
    let mut registry = PeerRegistry::default();
    registry.restore_trusted(store.load()?)?;
    Ok(registry)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_bind_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn default_peer_bind_addr() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8742))
}

fn default_discovery_enabled() -> bool {
    true
}

fn user_data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agent-send")
}

fn default_identity_path() -> PathBuf {
    user_data_dir().join("identity.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    fn config(path: &Path) -> Config {
        let mut config = Config::with_identity_path(path);
        config.peer_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.discovery_enabled = false;
        config
    }

    #[test]
    fn config_defaults_are_loopback_and_fill_missing_file_fields() {
        let config = serde_json::from_str::<Config>("{}").unwrap();
        assert_eq!(config.bind_addr, "127.0.0.1:0".parse().unwrap());
        assert!(config.is_loopback());
        assert_eq!(config.identity_path, Config::default().identity_path);
    }

    #[test]
    fn startup_rejects_non_loopback_and_persists_identity() {
        let path =
            std::env::temp_dir().join(format!("agent-send-test-{}-startup", std::process::id()));
        let bad = Config {
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            ..config(&path)
        };
        assert!(matches!(
            Daemon::new(bad),
            Err(DaemonError::NonLoopbackBind(_))
        ));

        let first = Daemon::new(config(&path)).unwrap();
        let id = first.identity().clone();
        let second = Daemon::new(config(&path)).unwrap();
        assert_eq!(second.identity(), &id);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn health_is_versioned_and_shutdown_is_deterministic() {
        let path =
            std::env::temp_dir().join(format!("agent-send-test-{}-health", std::process::id()));
        let running = Daemon::new(config(&path)).unwrap().start().unwrap();
        let mut stream = TcpStream::connect(running.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("\"version\":1"));
        assert!(response.contains("\"status\":\"ok\""));
        running.shutdown().unwrap();
        fs::remove_file(path).unwrap();
    }

    fn advertisement(id: &str) -> PeerAdvertisement {
        PeerAdvertisement {
            id: id.into(),
            alias: format!("Peer {id}"),
            address: "127.0.0.1:9000".into(),
            api_version: API_VERSION,
        }
    }

    fn unused_loopback_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        address
    }

    #[derive(Default)]
    struct RecordingDiscoveryState {
        published: Vec<PeerAdvertisement>,
        discovered: VecDeque<PeerAdvertisement>,
    }

    struct RecordingDiscovery(Arc<Mutex<RecordingDiscoveryState>>);

    impl PeerDiscovery for RecordingDiscovery {
        fn publish(&mut self, advertisement: &PeerAdvertisement) -> Result<(), DiscoveryError> {
            self.0.lock().unwrap().published.push(advertisement.clone());
            Ok(())
        }

        fn discover(&mut self) -> Result<Vec<PeerAdvertisement>, DiscoveryError> {
            Ok(self.0.lock().unwrap().discovered.drain(..).collect())
        }
    }

    #[test]
    fn daemon_lifecycle_publishes_and_consumes_discovery_hints() {
        let identity = std::env::temp_dir().join(format!(
            "agent-send-test-{}-discovery-lifecycle",
            std::process::id()
        ));
        let state = Arc::new(Mutex::new(RecordingDiscoveryState::default()));
        state
            .lock()
            .unwrap()
            .discovered
            .push_back(advertisement("lan-hint"));
        let daemon = Daemon::new(config(&identity)).unwrap();
        let running = daemon
            .start_with_discovery(RecordingDiscovery(state.clone()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while daemon.peers().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let published = state.lock().unwrap().published.clone();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].id, daemon.identity().id);
        assert_eq!(published[0].address, running.peer_addr().to_string());
        assert_eq!(daemon.peers()[0].advertisement.id, "lan-hint");
        assert!(!daemon.peers()[0].trusted);
        running.shutdown().unwrap();
        let _ = fs::remove_file(identity);
    }

    #[test]
    fn paired_daemons_transfer_over_authenticated_local_sockets() {
        let root = std::env::temp_dir().join(format!(
            "agent-send-test-{}-socket-transfer",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let destination_root = root.join("destination");
        fs::create_dir_all(source_root.join("nested")).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        fs::write(
            source_root.join("nested/file.txt"),
            b"authenticated socket transfer",
        )
        .unwrap();

        let sender_identity = root.join("sender-identity.json");
        let receiver_identity = root.join("receiver-identity.json");
        let mut sender_config = config(&sender_identity);
        sender_config.peer_bind_addr = unused_loopback_addr();
        let mut receiver_config = config(&receiver_identity);
        receiver_config.peer_bind_addr = unused_loopback_addr();
        let mut sender = Daemon::new(sender_config.clone()).unwrap();
        let mut receiver = Daemon::new(receiver_config.clone()).unwrap();
        sender.add_shared_folder(
            "source",
            &source_root,
            agent_send_core::FolderDirection::Read,
        );
        receiver.add_shared_folder(
            "destination",
            &destination_root,
            agent_send_core::FolderDirection::Write,
        );
        receiver.add_shared_folder(
            "read-only",
            &destination_root,
            agent_send_core::FolderDirection::Read,
        );

        let sender_advertisement = PeerAdvertisement {
            id: sender.identity().id.clone(),
            alias: "sender".into(),
            address: sender_config.peer_bind_addr.to_string(),
            api_version: API_VERSION,
        };
        let receiver_advertisement = PeerAdvertisement {
            id: receiver.identity().id.clone(),
            alias: "receiver".into(),
            address: receiver_config.peer_bind_addr.to_string(),
            api_version: API_VERSION,
        };
        let sender_pairing = sender
            .request_pairing(receiver_advertisement.clone())
            .unwrap();
        let receiver_pairing = receiver
            .request_pairing_with_material(
                sender_advertisement.clone(),
                sender_pairing.code.clone(),
                sender_pairing.pairing_secret.clone(),
            )
            .unwrap();
        assert!(matches!(
            sender.send_to_peer_socket(
                &receiver_advertisement.id,
                &agent_send_core::TransferRequest {
                    peer_id: receiver_advertisement.id.clone(),
                    source_folder_id: "source".into(),
                    source_paths: vec!["nested/file.txt".into()],
                    destination_folder_id: "destination".into(),
                    idempotency_key: "socket-transfer".into(),
                },
                &Cancellation::new(),
                |_| {},
            ),
            Err(DaemonError::UntrustedPeer(_))
        ));
        assert!(sender
            .confirm_pairing(&receiver_advertisement.id, &sender_pairing.code)
            .unwrap());
        assert!(receiver
            .confirm_pairing(&sender_advertisement.id, &receiver_pairing.code)
            .unwrap());

        // A fresh process instance must obtain its socket key exclusively from
        // persisted trust, not from test-only registration state.
        drop(sender);
        drop(receiver);
        let sender = Daemon::new(sender_config).unwrap();
        let receiver = Daemon::new(receiver_config).unwrap();
        sender.add_shared_folder(
            "source",
            &source_root,
            agent_send_core::FolderDirection::Read,
        );
        receiver.add_shared_folder(
            "destination",
            &destination_root,
            agent_send_core::FolderDirection::Write,
        );
        receiver.add_shared_folder(
            "read-only",
            &destination_root,
            agent_send_core::FolderDirection::Read,
        );

        let receiver_running = receiver.start().unwrap();
        let sender_running = sender.start().unwrap();
        let request = agent_send_core::TransferRequest {
            peer_id: receiver_advertisement.id.clone(),
            source_folder_id: "source".into(),
            source_paths: vec!["nested/file.txt".into()],
            destination_folder_id: "destination".into(),
            idempotency_key: "socket-transfer".into(),
        };
        let mut denied_request = request.clone();
        denied_request.destination_folder_id = "read-only".into();
        denied_request.idempotency_key = "socket-transfer-denied".into();
        assert!(matches!(
            sender.send_to_peer_socket(
                &receiver_advertisement.id,
                &denied_request,
                &Cancellation::new(),
                |_| {},
            ),
            Err(DaemonError::SocketTransfer(SocketTransferError::Rejected))
        ));
        assert!(!destination_root.join("nested/file.txt").exists());
        let outcome = sender
            .send_to_peer_socket(
                &receiver_advertisement.id,
                &request,
                &Cancellation::new(),
                |_| {},
            )
            .unwrap();
        assert_eq!(outcome.bytes, b"authenticated socket transfer".len() as u64);
        assert_eq!(
            fs::read(destination_root.join("nested/file.txt")).unwrap(),
            b"authenticated socket transfer"
        );
        sender_running.shutdown().unwrap();
        receiver_running.shutdown().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_is_untrusted_until_pairing_authorizes_a_secure_peer_channel() {
        let path = std::env::temp_dir().join(format!(
            "agent-send-test-{}-network-trust",
            std::process::id()
        ));
        let mut daemon = Daemon::new(config(&path)).unwrap();
        let remote = advertisement("peer-network");
        let mut discovery = MockPeerDiscovery::default();
        daemon.publish_presence(&mut discovery, &remote).unwrap();
        discovery.inject(remote.clone());
        assert_eq!(daemon.discover_peers(&mut discovery).unwrap(), 1);
        assert!(!daemon.peers()[0].trusted);

        assert!(matches!(
            daemon.secure_peer_channel("peer-network"),
            Err(DaemonError::UntrustedPeer(_))
        ));
        let pairing = daemon.request_pairing(remote).unwrap();
        assert!(daemon
            .confirm_pairing("peer-network", &pairing.code)
            .unwrap());
        assert!(daemon.secure_peer_channel("peer-network").is_ok());

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(trusted_peers_path(&path));
    }

    #[test]
    fn pairing_codes_are_wrong_expired_and_replayed_only_once() {
        let mut registry = PeerRegistry::default();
        let first = PairingSecret::new([1; 32]);
        let response = registry
            .request_pairing_with_material(
                advertisement("peer-code"),
                100,
                Some(("123456".into(), first)),
            )
            .unwrap();
        assert!(!registry.confirm_pairing("peer-code", "654321", 100));
        assert!(registry.confirm_pairing("peer-code", &response.code, 100));
        assert!(!registry.confirm_pairing("peer-code", &response.code, 100));

        let expired = registry
            .request_pairing_with_material(
                advertisement("peer-expired"),
                200,
                Some(("222222".into(), PairingSecret::new([2; 32]))),
            )
            .unwrap();
        assert!(!registry.confirm_pairing("peer-expired", &expired.code, 500));
        assert!(!registry.confirm_pairing("peer-expired", &expired.code, 500));
    }

    #[test]
    fn replacing_a_pairing_rotates_and_rejects_the_old_secret() {
        let mut registry = PeerRegistry::default();
        let peer = advertisement("peer-rotate");
        let old = PairingSecret::new([3; 32]);
        registry
            .request_pairing_with_material(peer.clone(), 0, Some(("111111".into(), old.clone())))
            .unwrap();
        assert!(registry.confirm_pairing("peer-rotate", "111111", 0));
        let mut old_sender =
            SecurePeerChannel::new("peer-rotate", PairedPeer::new("local", old).unwrap()).unwrap();

        registry
            .request_pairing_with_material(
                peer,
                1,
                Some(("222222".into(), PairingSecret::new([4; 32]))),
            )
            .unwrap();
        assert!(!registry.confirm_pairing("peer-rotate", "111111", 1));
        assert!(registry.confirm_pairing("peer-rotate", "222222", 1));
        let mut rotated =
            SecurePeerChannel::new("local", registry.paired_peer("peer-rotate").unwrap()).unwrap();
        let stale = old_sender
            .seal(&PeerMessage::Hello {
                protocol_version: PEER_PROTOCOL_VERSION,
            })
            .unwrap();
        assert_eq!(
            rotated.open(stale),
            Err(PeerChannelError::AuthenticationFailed)
        );
    }

    #[test]
    fn pairing_rejects_wrong_code_and_persists_then_revokes_trust() {
        let path =
            std::env::temp_dir().join(format!("agent-send-test-{}-pairing", std::process::id()));
        let mut daemon = Daemon::new(config(&path)).unwrap();
        let pairing = daemon.request_pairing(advertisement("peer-a")).unwrap();
        assert!(!daemon.confirm_pairing("peer-a", "000000").unwrap());
        assert!(daemon.confirm_pairing("peer-a", &pairing.code).unwrap());
        assert_eq!(daemon.peers()[0].advertisement.id, "peer-a");
        let daemon = Daemon::new(config(&path)).unwrap();
        assert!(daemon.peers()[0].trusted);
        assert!(daemon.secure_peer_channel("peer-a").is_ok());
        let mut daemon = daemon;
        assert!(daemon.revoke_peer("peer-a").unwrap());
        assert!(Daemon::new(config(&path)).unwrap().peers().is_empty());
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(trusted_peers_path(
            &std::env::temp_dir().join(format!("agent-send-test-{}-pairing", std::process::id())),
        ));
    }

    fn agent_request(
        addr: SocketAddr,
        token: Option<&str>,
        operation: &str,
        params: serde_json::Value,
    ) -> String {
        let body = serde_json::json!({ "operation": operation, "params": params }).to_string();
        let authorization = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "POST /v1/agent HTTP/1.1\r\nHost: localhost\r\n{authorization}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn authenticated_agent_api_enforces_scopes_and_tracks_submission_lifecycle() {
        let root = std::env::temp_dir().join(format!(
            "agent-send-test-{}-agent-api-root",
            std::process::id()
        ));
        let identity = std::env::temp_dir().join(format!(
            "agent-send-test-{}-agent-api-identity",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(&identity);
        let _ = fs::remove_file(trusted_peers_path(&identity));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("safe.txt"), b"safe").unwrap();
        let store = Arc::new(MemoryAgentTokenStore::default());
        let mut daemon = Daemon::with_token_store(config(&identity), store).unwrap();
        daemon.add_shared_folder("source", &root, agent_send_core::FolderDirection::Read);
        let paired = daemon.request_pairing(advertisement("trusted")).unwrap();
        assert!(daemon.confirm_pairing("trusted", &paired.code).unwrap());
        daemon.advertise_peer(advertisement("untrusted"));
        let token = daemon
            .issue_agent_token(AgentScope {
                peer_ids: ["trusted".to_owned(), "untrusted".to_owned()]
                    .into_iter()
                    .collect(),
                folder_ids: ["source".to_owned(), "destination".to_owned()]
                    .into_iter()
                    .collect(),
            })
            .unwrap();
        let running = daemon.start().unwrap();
        let addr = running.local_addr();

        assert!(
            agent_request(addr, None, "peers.list", serde_json::json!({}))
                .starts_with("HTTP/1.1 401 Unauthorized")
        );
        let folders = agent_request(
            addr,
            Some(&token.token),
            "folders.list",
            serde_json::json!({}),
        );
        assert!(folders.starts_with("HTTP/1.1 200 OK") && folders.contains("source"));
        assert!(!folders.contains("destination"));

        let request = |peer: &str, source: &str| {
            serde_json::json!({
                "peer_id": peer,
                "source_folder_id": "source",
                "source_paths": [source],
                "destination_folder_id": "destination",
                "idempotency_key": format!("{peer}-{source}"),
            })
        };
        assert!(agent_request(
            addr,
            Some(&token.token),
            "transfers.submit",
            request("trusted", "../outside")
        )
        .starts_with("HTTP/1.1 400 Bad Request"));
        assert!(agent_request(
            addr,
            Some(&token.token),
            "transfers.submit",
            request("untrusted", "safe.txt")
        )
        .starts_with("HTTP/1.1 403 Forbidden"));
        let submitted = agent_request(
            addr,
            Some(&token.token),
            "transfers.submit",
            request("trusted", "safe.txt"),
        );
        assert!(
            submitted.contains("\"transfer_id\":\"local-1\"") && submitted.contains("submitted")
        );
        let status = agent_request(
            addr,
            Some(&token.token),
            "transfers.status",
            serde_json::json!({ "transfer_id": "local-1" }),
        );
        assert!(status.contains("submitted"));
        let cancelled = agent_request(
            addr,
            Some(&token.token),
            "transfers.cancel",
            serde_json::json!({ "transfer_id": "local-1" }),
        );
        assert!(cancelled.contains("cancelled"));
        let audit = running.audit_entries();
        assert!(audit
            .iter()
            .any(|entry| entry.operation == "transfers.cancel" && entry.result == "ok"));
        assert!(audit
            .iter()
            .all(|entry| !serde_json::to_string(entry).unwrap().contains(&token.token)));
        running.shutdown().unwrap();
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(identity);
        let _ = fs::remove_file(trusted_peers_path(&std::env::temp_dir().join(format!(
            "agent-send-test-{}-agent-api-identity",
            std::process::id()
        ))));
    }

    fn local_request(addr: SocketAddr, method: &str, path: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn local_pairing_api_requests_confirms_and_revokes_once() {
        let path = std::env::temp_dir().join(format!(
            "agent-send-test-{}-pairing-api",
            std::process::id()
        ));
        let daemon = Daemon::new(config(&path)).unwrap();
        let running = daemon.start().unwrap();
        let advertisement = serde_json::to_string(&advertisement("peer-api")).unwrap();
        let response = local_request(running.local_addr(), "POST", "/v1/pairings", &advertisement);
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let pairing: PairingResponse =
            serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(pairing.peer_id, "peer-api");
        assert_eq!(pairing.pairing_secret.len(), 64);

        let wrong_code = if pairing.code == "000000" {
            "000001"
        } else {
            "000000"
        };
        let wrong = local_request(
            running.local_addr(),
            "POST",
            "/v1/pairings/confirm",
            &serde_json::json!({ "peer_id": "peer-api", "code": wrong_code }).to_string(),
        );
        assert!(wrong.starts_with("HTTP/1.1 400 Bad Request"));
        let confirmed = local_request(
            running.local_addr(),
            "POST",
            "/v1/pairings/confirm",
            &serde_json::json!({ "peer_id": "peer-api", "code": pairing.code }).to_string(),
        );
        assert!(confirmed.starts_with("HTTP/1.1 200 OK"));
        let replay = local_request(
            running.local_addr(),
            "POST",
            "/v1/pairings/confirm",
            &serde_json::json!({ "peer_id": "peer-api", "code": pairing.code }).to_string(),
        );
        assert!(replay.starts_with("HTTP/1.1 400 Bad Request"));
        let peers = local_request(running.local_addr(), "GET", "/v1/peers", "");
        assert!(peers.starts_with("HTTP/1.1 200 OK") && peers.contains("\"trusted\":true"));
        let revoked = local_request(running.local_addr(), "DELETE", "/v1/peers/peer-api", "");
        assert!(revoked.starts_with("HTTP/1.1 200 OK"));
        running.shutdown().unwrap();
        assert!(Daemon::new(config(&path)).unwrap().peers().is_empty());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(trusted_peers_path(&path));
    }
}
