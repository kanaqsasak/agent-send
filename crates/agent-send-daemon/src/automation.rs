//! Authenticated local automation state.
//!
//! Agent credentials are generated locally, persisted only as hashes, and
//! carry explicit peer and folder capability allow-lists. This module contains
//! no socket code so the same policy applies to every loopback transport.

use crate::{PeerRecord, TransferEngine, TransferError};
use agent_send_core::{FolderDirection, TransferRequest};
use getrandom::getrandom;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentScope {
    #[serde(default)]
    pub peer_ids: BTreeSet<String>,
    #[serde(default)]
    pub folder_ids: BTreeSet<String>,
}

impl AgentScope {
    pub fn allows_peer(&self, peer_id: &str) -> bool {
        self.peer_ids.contains(peer_id)
    }

    pub fn allows_folder(&self, folder_id: &str) -> bool {
        self.folder_ids.contains(folder_id)
    }

    fn validate(&self) -> Result<(), AutomationError> {
        if self.peer_ids.is_empty() && self.folder_ids.is_empty() {
            return Err(AutomationError::EmptyScope);
        }
        if self.peer_ids.iter().any(|id| id.trim().is_empty())
            || self.folder_ids.iter().any(|id| id.trim().is_empty())
        {
            return Err(AutomationError::InvalidScope);
        }
        Ok(())
    }
}

/// Returned exactly once when a local client creates an agent credential.
/// The raw token is deliberately never stored or included in audit entries.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedAgentToken {
    pub id: String,
    pub token: String,
    pub scope: AgentScope,
}

impl std::fmt::Debug for IssuedAgentToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedAgentToken")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .field("scope", &self.scope)
            .finish()
    }
}

/// A persisted credential has a one-way token hash, never the bearer token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAgentToken {
    pub id: String,
    pub token_hash: String,
    pub scope: AgentScope,
}

#[derive(Debug, Error)]
pub enum TokenStoreError {
    #[error("token storage failed: {0}")]
    Io(#[from] io::Error),
    #[error("token storage data is invalid: {0}")]
    Invalid(#[source] serde_json::Error),
}

/// Persistence boundary for local agent credentials. OS credential-store
/// adapters can implement this without changing authorization behavior.
pub trait AgentTokenStore: Send + Sync {
    fn load(&self) -> Result<Vec<StoredAgentToken>, TokenStoreError>;
    fn save(&self, tokens: &[StoredAgentToken]) -> Result<(), TokenStoreError>;
}

pub struct FileAgentTokenStore {
    path: PathBuf,
}

impl FileAgentTokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl AgentTokenStore for FileAgentTokenStore {
    fn load(&self) -> Result<Vec<StoredAgentToken>, TokenStoreError> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str(&contents).map_err(TokenStoreError::Invalid),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, tokens: &[StoredAgentToken]) -> Result<(), TokenStoreError> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &self.path,
            serde_json::to_vec_pretty(tokens).expect("tokens serialize"),
        )?;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct MemoryAgentTokenStore(Mutex<Vec<StoredAgentToken>>);

impl AgentTokenStore for MemoryAgentTokenStore {
    fn load(&self) -> Result<Vec<StoredAgentToken>, TokenStoreError> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn save(&self, tokens: &[StoredAgentToken]) -> Result<(), TokenStoreError> {
        *self.0.lock().unwrap() = tokens.to_vec();
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum AutomationError {
    #[error("agent scopes must permit at least one peer or folder")]
    EmptyScope,
    #[error("agent scope entries must not be empty")]
    InvalidScope,
    #[error(transparent)]
    TokenStore(#[from] TokenStoreError),
    #[error("secure token generation failed")]
    Random,
    #[error("agent token is not authorized")]
    Unauthorized,
    #[error("agent token is not authorized for {0}")]
    ScopeDenied(String),
    #[error("peer is not trusted: {0}")]
    UntrustedPeer(String),
    #[error("transfer was not found")]
    TransferNotFound,
    #[error(transparent)]
    Transfer(#[from] TransferError),
}

pub struct AgentTokens {
    store: Arc<dyn AgentTokenStore>,
    tokens: Mutex<BTreeMap<String, StoredAgentToken>>,
}

impl AgentTokens {
    pub fn load(store: Arc<dyn AgentTokenStore>) -> Result<Self, AutomationError> {
        let tokens = store
            .load()?
            .into_iter()
            .map(|token| (token.id.clone(), token))
            .collect();
        Ok(Self {
            store,
            tokens: Mutex::new(tokens),
        })
    }

    pub fn issue(&self, scope: AgentScope) -> Result<IssuedAgentToken, AutomationError> {
        scope.validate()?;
        let id = random_hex(16)?;
        let token = format!("agent_{}", random_hex(32)?);
        let stored = StoredAgentToken {
            id: id.clone(),
            token_hash: hash(&token),
            scope: scope.clone(),
        };
        let mut tokens = self.tokens.lock().unwrap();
        tokens.insert(id.clone(), stored);
        if let Err(error) = self.persist(&tokens) {
            tokens.remove(&id);
            return Err(error);
        }
        Ok(IssuedAgentToken { id, token, scope })
    }

    pub fn authorize(&self, token: &str) -> Option<AuthorizedAgent> {
        let token_hash = hash(token);
        self.tokens.lock().unwrap().values().find_map(|stored| {
            constant_time_eq(stored.token_hash.as_bytes(), token_hash.as_bytes()).then(|| {
                AuthorizedAgent {
                    id: stored.id.clone(),
                    scope: stored.scope.clone(),
                }
            })
        })
    }

    pub fn revoke(&self, id: &str) -> Result<bool, AutomationError> {
        let mut tokens = self.tokens.lock().unwrap();
        let removed = tokens.remove(id);
        if removed.is_none() {
            return Ok(false);
        }
        if let Err(error) = self.persist(&tokens) {
            tokens.insert(id.to_owned(), removed.expect("checked above"));
            return Err(error);
        }
        Ok(true)
    }

    fn persist(&self, tokens: &BTreeMap<String, StoredAgentToken>) -> Result<(), AutomationError> {
        self.store
            .save(&tokens.values().cloned().collect::<Vec<_>>())?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AuthorizedAgent {
    pub id: String,
    pub scope: AgentScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderResponse {
    pub id: String,
    pub direction: FolderDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldersResponse {
    pub version: u32,
    pub folders: Vec<FolderResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTransferState {
    Submitted,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTransferStatus {
    pub transfer_id: String,
    pub state: AgentTransferState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentTransfer {
    status: AgentTransferStatus,
    actor_id: String,
    idempotency_key: String,
}

/// Tracks validated agent submissions and the result of socket delivery.
#[derive(Debug, Default)]
pub struct AgentTransfers {
    state: Mutex<AgentTransferStateStore>,
}

#[derive(Debug, Default)]
struct AgentTransferStateStore {
    next_id: u64,
    transfers: BTreeMap<String, AgentTransfer>,
}

impl AgentTransfers {
    pub fn submit(
        &self,
        actor: &AuthorizedAgent,
        request: &TransferRequest,
        peers: &[PeerRecord],
        engine: &TransferEngine,
    ) -> Result<AgentTransferStatus, AutomationError> {
        authorize_submission(actor, request, peers, engine)?;
        let mut state = self.state.lock().unwrap();
        if let Some(existing) = state.transfers.values().find(|transfer| {
            transfer.actor_id == actor.id && transfer.idempotency_key == request.idempotency_key
        }) {
            return Ok(existing.status.clone());
        }
        state.next_id += 1;
        let status = AgentTransferStatus {
            transfer_id: format!("local-{}", state.next_id),
            state: AgentTransferState::Submitted,
            error: None,
        };
        state.transfers.insert(
            status.transfer_id.clone(),
            AgentTransfer {
                status: status.clone(),
                actor_id: actor.id.clone(),
                idempotency_key: request.idempotency_key.clone(),
            },
        );
        Ok(status)
    }

    pub fn complete(
        &self,
        actor: &AuthorizedAgent,
        transfer_id: &str,
    ) -> Result<AgentTransferStatus, AutomationError> {
        self.update_result(actor, transfer_id, AgentTransferState::Completed, None)
    }

    pub fn fail(
        &self,
        actor: &AuthorizedAgent,
        transfer_id: &str,
        error: impl Into<String>,
    ) -> Result<AgentTransferStatus, AutomationError> {
        self.update_result(
            actor,
            transfer_id,
            AgentTransferState::Failed,
            Some(error.into()),
        )
    }

    pub fn status(
        &self,
        actor: &AuthorizedAgent,
        transfer_id: &str,
    ) -> Result<AgentTransferStatus, AutomationError> {
        self.for_actor(actor, transfer_id)
            .map(|transfer| transfer.status.clone())
    }

    pub fn cancel(
        &self,
        actor: &AuthorizedAgent,
        transfer_id: &str,
    ) -> Result<AgentTransferStatus, AutomationError> {
        let mut state = self.state.lock().unwrap();
        let transfer = state
            .transfers
            .get_mut(transfer_id)
            .filter(|transfer| transfer.actor_id == actor.id)
            .ok_or(AutomationError::TransferNotFound)?;
        if matches!(transfer.status.state, AgentTransferState::Submitted) {
            transfer.status.state = AgentTransferState::Cancelled;
        }
        Ok(transfer.status.clone())
    }

    fn update_result(
        &self,
        actor: &AuthorizedAgent,
        transfer_id: &str,
        state_value: AgentTransferState,
        error: Option<String>,
    ) -> Result<AgentTransferStatus, AutomationError> {
        let mut state = self.state.lock().unwrap();
        let transfer = state
            .transfers
            .get_mut(transfer_id)
            .filter(|transfer| transfer.actor_id == actor.id)
            .ok_or(AutomationError::TransferNotFound)?;
        if matches!(transfer.status.state, AgentTransferState::Submitted) {
            transfer.status.state = state_value;
            transfer.status.error = error;
        }
        Ok(transfer.status.clone())
    }

    fn for_actor(
        &self,
        actor: &AuthorizedAgent,
        transfer_id: &str,
    ) -> Result<AgentTransfer, AutomationError> {
        self.state
            .lock()
            .unwrap()
            .transfers
            .get(transfer_id)
            .filter(|transfer| transfer.actor_id == actor.id)
            .cloned()
            .ok_or(AutomationError::TransferNotFound)
    }
}

fn authorize_submission(
    actor: &AuthorizedAgent,
    request: &TransferRequest,
    peers: &[PeerRecord],
    engine: &TransferEngine,
) -> Result<(), AutomationError> {
    if !actor.scope.allows_peer(&request.peer_id) {
        return Err(AutomationError::ScopeDenied("peer".into()));
    }
    if !actor.scope.allows_folder(&request.source_folder_id)
        || !actor.scope.allows_folder(&request.destination_folder_id)
    {
        return Err(AutomationError::ScopeDenied("folder".into()));
    }
    if !peers
        .iter()
        .any(|peer| peer.trusted && peer.advertisement.id == request.peer_id)
    {
        return Err(AutomationError::UntrustedPeer(request.peer_id.clone()));
    }
    engine.validate_submission(request)?;
    Ok(())
}

/// Audit metadata intentionally stores stable redacted identifiers rather
/// than request-provided capability, peer, transfer, or actor values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp_ms: u128,
    pub actor_id: String,
    pub operation: String,
    pub peer_id: Option<String>,
    pub folder_ids: Vec<String>,
    pub transfer_id: Option<String>,
    pub result: String,
}

#[derive(Debug, Default)]
pub struct AuditLog(Mutex<Vec<AuditEntry>>);

impl AuditLog {
    pub fn record(
        &self,
        actor_id: impl Into<String>,
        operation: impl Into<String>,
        peer_id: Option<String>,
        folder_ids: Vec<String>,
        transfer_id: Option<String>,
        result: impl Into<String>,
    ) {
        self.0.lock().unwrap().push(AuditEntry {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            actor_id: redact_audit_identifier(actor_id.into()),
            operation: audit_operation(operation.into()),
            peer_id: peer_id.map(redact_audit_identifier),
            folder_ids: folder_ids
                .into_iter()
                .map(redact_audit_identifier)
                .collect(),
            transfer_id: transfer_id.map(redact_audit_identifier),
            result: audit_result(result.into()),
        });
    }

    pub fn entries(&self) -> Vec<AuditEntry> {
        self.0.lock().unwrap().clone()
    }
}

fn redact_audit_identifier(value: String) -> String {
    // A deterministic digest supports correlating related audit records without
    // retaining caller-controlled identifiers that may contain a token, path,
    // pairing secret, or other sensitive text.
    format!("redacted:{}", hash(&value))
}

fn audit_operation(operation: String) -> String {
    match operation.as_str() {
        "peers.list" | "folders.list" | "transfers.submit" | "transfers.send"
        | "transfers.status" | "transfers.cancel" | "invalid_request" => operation,
        _ => "unknown".into(),
    }
}

fn audit_result(result: String) -> String {
    match result.as_str() {
        "ok" | "denied" => result,
        _ => "unknown".into(),
    }
}

pub fn token_store_path(identity_path: &Path) -> PathBuf {
    identity_path.with_extension("agent-tokens.json")
}

fn random_hex(bytes: usize) -> Result<String, AutomationError> {
    let mut value = vec![0; bytes];
    getrandom(&mut value).map_err(|_| AutomationError::Random)?;
    Ok(hex(&value))
}

fn hash(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PeerAdvertisement, PeerRegistry};
    use agent_send_core::path_policy::PathPolicy;

    fn scope() -> AgentScope {
        AgentScope {
            peer_ids: ["peer".to_owned()].into_iter().collect(),
            folder_ids: ["source".to_owned(), "destination".to_owned()]
                .into_iter()
                .collect(),
        }
    }

    fn request(path: &str) -> TransferRequest {
        TransferRequest {
            peer_id: "peer".into(),
            source_folder_id: "source".into(),
            source_paths: vec![path.into()],
            destination_folder_id: "destination".into(),
            idempotency_key: "request-1".into(),
        }
    }

    #[test]
    fn tokens_are_scoped_persisted_as_hashes_and_revocable() {
        let store = Arc::new(MemoryAgentTokenStore::default());
        let tokens = AgentTokens::load(store.clone()).unwrap();
        let issued = tokens.issue(scope()).unwrap();
        assert_eq!(tokens.authorize(&issued.token).unwrap().id, issued.id);
        assert!(tokens.authorize("wrong").is_none());
        let persisted = store.load().unwrap();
        assert_eq!(persisted.len(), 1);
        assert_ne!(persisted[0].token_hash, issued.token);
        assert!(!serde_json::to_string(&persisted)
            .unwrap()
            .contains(&issued.token));
        assert!(tokens.revoke(&issued.id).unwrap());
        assert!(tokens.authorize(&issued.token).is_none());
    }

    #[test]
    fn submission_enforces_capabilities_and_tracks_status_and_cancel() {
        let root =
            std::env::temp_dir().join(format!("agent-send-automation-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("safe.txt"), b"safe").unwrap();
        let engine = TransferEngine::new();
        engine.add_folder("source", PathPolicy::new(&root, FolderDirection::Read));
        let mut registry = PeerRegistry::default();
        let peer = PeerAdvertisement {
            id: "peer".into(),
            alias: "peer".into(),
            address: "127.0.0.1:1".into(),
            api_version: crate::API_VERSION,
        };
        let code = registry.request_pairing(peer, 0).unwrap().code;
        assert!(registry.confirm_pairing("peer", &code, 0));
        let actor = AuthorizedAgent {
            id: "agent".into(),
            scope: scope(),
        };
        let transfers = AgentTransfers::default();
        assert!(matches!(
            transfers.submit(&actor, &request("../secret"), &registry.list(), &engine),
            Err(AutomationError::Transfer(TransferError::Policy(_)))
        ));
        let status = transfers
            .submit(&actor, &request("safe.txt"), &registry.list(), &engine)
            .unwrap();
        assert_eq!(status.state, AgentTransferState::Submitted);
        assert_eq!(
            transfers.status(&actor, &status.transfer_id).unwrap(),
            status
        );
        assert_eq!(
            transfers.cancel(&actor, &status.transfer_id).unwrap().state,
            AgentTransferState::Cancelled
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_entries_never_receive_bearer_secrets() {
        let audit = AuditLog::default();
        audit.record(
            "agent-id",
            "transfers.submit",
            Some("peer".into()),
            vec!["source".into()],
            None,
            "denied",
        );
        let entry = audit.entries().pop().unwrap();
        assert!(entry.actor_id.starts_with("redacted:"));
        assert!(!serde_json::to_string(&entry)
            .unwrap()
            .contains("agent_secret"));
    }

    #[test]
    fn audit_redacts_request_controlled_identifiers_and_unknown_operations() {
        let audit = AuditLog::default();
        let secret = format!("agent_{}", "ab".repeat(32));
        audit.record(
            secret.clone(),
            secret.clone(),
            Some("peer-secret".into()),
            vec!["folder-secret".into()],
            Some("transfer-secret".into()),
            secret.clone(),
        );
        let entry = audit.entries().pop().unwrap();
        let serialized = serde_json::to_string(&entry).unwrap();
        for value in [
            secret.as_str(),
            "peer-secret",
            "folder-secret",
            "transfer-secret",
        ] {
            assert!(!serialized.contains(value), "audit leaked {value}");
        }
        assert_eq!(entry.operation, "unknown");
        assert_eq!(entry.result, "unknown");
    }
}
