//! The agent-send background daemon foundation.
//!
//! The local API is deliberately small and transport-independent types are kept
//! public so a different local transport can be added without changing daemon
//! state or configuration. Authenticated automation clients use `POST /v1/agent`
//! with an `Authorization: Bearer` header and JSON `{ "operation", "params" }`.
//! Supported operations are `peers.list`, `folders.list`, `transfers.submit`
//! (also `transfers.send`), `transfers.status`, and `transfers.cancel`.

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
pub mod peer_transport;
pub mod transfer;

pub use automation::{
    AgentScope, AgentTokenStore, AgentTransferState, AgentTransferStatus, AuditEntry,
    FileAgentTokenStore, FoldersResponse, IssuedAgentToken, MemoryAgentTokenStore,
};
pub use discovery::{
    DiscoveryError, MdnsDiscovery, MockPeerDiscovery, PeerDiscovery, MDNS_SERVICE_TYPE,
};
pub use peer_transport::{
    EncryptedPeerFrame, MockPeerTransport, PairedPeer, PairingSecret, PeerChannelError,
    PeerConnection, PeerConnectionError, PeerMessage, PeerTransport, PeerTransportError,
    SecurePeerChannel, PEER_PROTOCOL_VERSION,
};
pub use transfer::{
    Cancellation, LoopbackTransport, TransferEngine, TransferError, TransferOutcome,
    TransferProgress, CHUNK_SIZE,
};

pub const API_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// The local API must remain loopback-only.
    pub bind_addr: SocketAddr,
    /// File containing the daemon's local identity placeholder.
    pub identity_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            identity_path: default_identity_path(),
        }
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingResponse {
    pub peer_id: String,
    pub code: String,
    pub expires_in_seconds: u32,
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

/// Deterministic in-memory registry. Persistence is performed after trust
/// changes, while advertisements themselves remain ephemeral.
#[derive(Debug, Default)]
pub struct PeerRegistry {
    peers: BTreeMap<String, PeerRecord>,
    pending: BTreeMap<String, String>,
    next_code: u64,
}

impl PeerRegistry {
    pub fn list(&self) -> Vec<PeerRecord> {
        self.peers.values().cloned().collect()
    }

    pub fn advertise(&mut self, advertisement: PeerAdvertisement) {
        let id = advertisement.id.clone();
        let trusted = self.peers.get(&id).is_some_and(|peer| peer.trusted);
        self.peers.insert(
            id,
            PeerRecord {
                advertisement,
                trusted,
            },
        );
    }

    pub fn request_pairing(&mut self, advertisement: PeerAdvertisement) -> PairingResponse {
        let id = advertisement.id.clone();
        self.advertise(advertisement);
        self.next_code = self.next_code.wrapping_add(1);
        // A short, visible code; no network randomness is required for this
        // foundation, and the monotonic seed makes local tests reproducible.
        let code = format!("{:06}", self.next_code % 1_000_000);
        self.pending.insert(id.clone(), code.clone());
        PairingResponse {
            peer_id: id,
            code,
            expires_in_seconds: 300,
        }
    }

    pub fn confirm_pairing(&mut self, peer_id: &str, code: &str) -> bool {
        if self.pending.get(peer_id).map(String::as_str) != Some(code) {
            return false;
        }
        self.pending.remove(peer_id);
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.trusted = true;
            true
        } else {
            false
        }
    }

    pub fn revoke(&mut self, peer_id: &str) -> bool {
        self.pending.remove(peer_id);
        self.peers
            .get_mut(peer_id)
            .map(|peer| {
                peer.trusted = false;
                true
            })
            .unwrap_or(false)
    }

    fn restore_trusted(&mut self, records: Vec<PeerRecord>) {
        for record in records {
            self.peers.insert(record.advertisement.id.clone(), record);
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
    #[error("identity storage failed: {0}")]
    Identity(#[source] io::Error),
    #[error("peer storage failed: {0}")]
    PeerStorage(#[source] io::Error),
    #[error("failed to start local API: {0}")]
    Bind(#[source] io::Error),
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error("peer is not trusted: {0}")]
    UntrustedPeer(String),
    #[error(transparent)]
    PeerChannel(#[from] PeerChannelError),
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
}

impl Daemon {
    pub fn new(config: Config) -> Result<Self, DaemonError> {
        let token_store = Arc::new(automation::FileAgentTokenStore::new(
            automation::token_store_path(&config.identity_path),
        ));
        Self::with_token_store(config, token_store)
    }

    /// Construct a daemon with an explicit token persistence adapter. This
    /// lets platform credential stores replace the default local file without
    /// altering token scopes or local API authorization.
    pub fn with_token_store(
        config: Config,
        token_store: Arc<dyn automation::AgentTokenStore>,
    ) -> Result<Self, DaemonError> {
        config.validate()?;
        let identity = load_or_create_identity(&config.identity_path)?;
        let registry = load_trusted_peers(&trusted_peers_path(&config.identity_path))?;
        Ok(Self {
            config,
            identity,
            registry: Arc::new(Mutex::new(registry)),
            transfer: Arc::new(transfer::TransferEngine::new()),
            agent_tokens: Arc::new(automation::AgentTokens::load(token_store)?),
            agent_transfers: Arc::new(automation::AgentTransfers::default()),
            audit: Arc::new(automation::AuditLog::default()),
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

    /// Construct a cryptographic channel only after the existing pairing/trust
    /// registry authorizes the peer ID. Pairing secrets remain outside DNS-SD
    /// and must come from the human-confirmed pairing protocol.
    pub fn secure_peer_channel(&self, peer: PairedPeer) -> Result<SecurePeerChannel, DaemonError> {
        if !self
            .registry
            .lock()
            .unwrap()
            .peers
            .get(peer.id())
            .is_some_and(|record| record.trusted)
        {
            return Err(DaemonError::UntrustedPeer(peer.id().to_owned()));
        }
        Ok(SecurePeerChannel::new(self.identity.id.clone(), peer)?)
    }

    pub fn request_pairing(&mut self, advertisement: PeerAdvertisement) -> PairingResponse {
        self.registry.lock().unwrap().request_pairing(advertisement)
    }

    pub fn confirm_pairing(&mut self, peer_id: &str, code: &str) -> Result<bool, DaemonError> {
        let mut registry = self.registry.lock().unwrap();
        let confirmed = registry.confirm_pairing(peer_id, code);
        if confirmed {
            save_trusted_peers(&trusted_peers_path(&self.config.identity_path), &registry)?;
        }
        Ok(confirmed)
    }

    pub fn revoke_peer(&mut self, peer_id: &str) -> Result<bool, DaemonError> {
        let mut registry = self.registry.lock().unwrap();
        let revoked = registry.revoke(peer_id);
        if revoked {
            save_trusted_peers(&trusted_peers_path(&self.config.identity_path), &registry)?;
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

    pub fn start(self) -> Result<RunningDaemon, DaemonError> {
        let listener = TcpListener::bind(self.config.bind_addr).map_err(DaemonError::Bind)?;
        listener.set_nonblocking(true).map_err(DaemonError::Bind)?;
        let local_addr = listener.local_addr().map_err(DaemonError::Bind)?;
        let identity = self.identity;
        let registry = self.registry;
        let transfer = self.transfer;
        let agent_tokens = self.agent_tokens;
        let agent_transfers = self.agent_transfers;
        let audit = self.audit;
        let server_audit = audit.clone();
        let persist_path = trusted_peers_path(&self.config.identity_path);
        let (shutdown_tx, shutdown_rx) = mpsc::channel();

        let thread = thread::Builder::new()
            .name("agent-send-local-api".into())
            .spawn(move || {
                run_server(
                    listener,
                    identity,
                    registry,
                    transfer,
                    agent_tokens,
                    agent_transfers,
                    server_audit,
                    persist_path,
                    shutdown_rx,
                )
            })
            .map_err(DaemonError::Bind)?;

        Ok(RunningDaemon {
            local_addr,
            audit,
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        })
    }
}

pub struct RunningDaemon {
    local_addr: SocketAddr,
    audit: Arc<automation::AuditLog>,
    shutdown_tx: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl RunningDaemon {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn audit_entries(&self) -> Vec<AuditEntry> {
        self.audit.entries()
    }

    /// Signals the server and waits for its worker to exit.
    pub fn shutdown(mut self) -> Result<(), DaemonError> {
        self.shutdown_tx
            .take()
            .expect("shutdown sender present")
            .send(())
            .ok();
        self.thread
            .take()
            .expect("daemon thread present")
            .join()
            .map_err(|_| DaemonError::Shutdown)
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown_tx.take() {
            let _ = sender.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_server(
    listener: TcpListener,
    identity: LocalIdentity,
    registry: Arc<Mutex<PeerRegistry>>,
    transfer: Arc<TransferEngine>,
    agent_tokens: Arc<automation::AgentTokens>,
    agent_transfers: Arc<automation::AgentTransfers>,
    audit: Arc<automation::AuditLog>,
    persist_path: PathBuf,
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
                &persist_path,
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
    persist_path: &Path,
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
            match serde_json::from_str::<PeerAdvertisement>(body) {
                Ok(ad) => {
                    let response = registry.lock().unwrap().request_pairing(ad);
                    ("200 OK", serde_json::to_string(&response).unwrap())
                }
                Err(_) => ("400 Bad Request", "{\"error\":\"invalid_request\"}".into()),
            }
        }
        (Some("POST"), Some("/v1/pairings/confirm"))
        | (Some("POST"), Some("/v1/pairing/confirm")) => {
            match serde_json::from_str::<ConfirmPairing>(body) {
                Ok(request) => {
                    let mut peers = registry.lock().unwrap();
                    let confirmed = peers.confirm_pairing(&request.peer_id, &request.code);
                    if confirmed {
                        let _ = save_trusted_peers(persist_path, &peers);
                    }
                    if confirmed {
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
            let revoked = peers.revoke(id);
            if revoked {
                let _ = save_trusted_peers(persist_path, &peers);
            }
            if revoked {
                ("200 OK", "{\"revoked\":true}".into())
            } else {
                ("404 Not Found", "{\"error\":\"peer_not_found\"}".into())
            }
        }
        _ => ("404 Not Found", "{\"error\":\"not_found\"}".to_owned()),
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let value = format!("{nanos:032x}");
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

fn load_trusted_peers(path: &Path) -> Result<PeerRegistry, DaemonError> {
    let mut registry = PeerRegistry::default();
    match fs::read_to_string(path) {
        Ok(contents) => {
            let records = serde_json::from_str::<Vec<PeerRecord>>(&contents).map_err(|error| {
                DaemonError::PeerStorage(io::Error::new(io::ErrorKind::InvalidData, error))
            })?;
            registry.restore_trusted(records);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(DaemonError::PeerStorage(error)),
    }
    Ok(registry)
}

fn save_trusted_peers(path: &Path, registry: &PeerRegistry) -> Result<(), DaemonError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(DaemonError::PeerStorage)?;
    }
    let records: Vec<_> = registry
        .peers
        .values()
        .filter(|peer| peer.trusted)
        .cloned()
        .collect();
    let contents = serde_json::to_vec_pretty(&records).expect("peer records are serializable");
    fs::write(path, contents).map_err(DaemonError::PeerStorage)
}

fn default_identity_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agent-send")
        .join("identity.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::time::Duration;

    fn config(path: &Path) -> Config {
        Config::with_identity_path(path)
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

        let paired = PairedPeer::new("peer-network", PairingSecret::new([3; 32])).unwrap();
        assert!(matches!(
            daemon.secure_peer_channel(paired.clone()),
            Err(DaemonError::UntrustedPeer(_))
        ));
        let pairing = daemon.request_pairing(remote);
        assert!(daemon
            .confirm_pairing("peer-network", &pairing.code)
            .unwrap());
        assert!(daemon.secure_peer_channel(paired).is_ok());

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(trusted_peers_path(&path));
    }

    #[test]
    fn pairing_rejects_wrong_code_and_persists_then_revokes_trust() {
        let path =
            std::env::temp_dir().join(format!("agent-send-test-{}-pairing", std::process::id()));
        let mut daemon = Daemon::new(config(&path)).unwrap();
        let pairing = daemon.request_pairing(advertisement("peer-a"));
        assert!(!daemon.confirm_pairing("peer-a", "000000").unwrap());
        assert!(daemon.confirm_pairing("peer-a", &pairing.code).unwrap());
        assert_eq!(daemon.peers()[0].advertisement.id, "peer-a");
        let daemon = Daemon::new(config(&path)).unwrap();
        assert!(daemon.peers()[0].trusted);
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
        let paired = daemon.request_pairing(advertisement("trusted"));
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

    #[test]
    fn peer_api_lists_and_revokes_with_versioned_json() {
        let path = std::env::temp_dir().join(format!("agent-send-test-{}-api", std::process::id()));
        let mut daemon = Daemon::new(config(&path)).unwrap();
        let pairing = daemon.request_pairing(advertisement("peer-api"));
        daemon.confirm_pairing("peer-api", &pairing.code).unwrap();
        let running = daemon.start().unwrap();
        let mut stream = TcpStream::connect(running.local_addr()).unwrap();
        stream
            .write_all(b"GET /v1/peers HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"version\":1") && response.contains("peer-api"));
        running.shutdown().unwrap();
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(std::env::temp_dir().join("trusted-peers.json"));
    }
}
