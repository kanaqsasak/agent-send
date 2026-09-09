//! Shared, transport-independent types for agent-send.
//!
//! Networking and OS integration belong in the daemon. Keeping these types
//! small makes the desktop, CLI, and agent adapters use the same contract.

pub mod path_policy;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderDirection {
    Read,
    Write,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedFolder {
    pub id: String,
    pub name: String,
    pub direction: FolderDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub alias: String,
    pub address: String,
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRequest {
    pub peer_id: String,
    pub source_paths: Vec<String>,
    pub destination_folder_id: String,
    pub idempotency_key: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("at least one source path is required")]
    EmptySources,
    #[error("destination folder is required")]
    MissingDestination,
    #[error("idempotency key is required")]
    MissingIdempotencyKey,
}

impl TransferRequest {
    /// Validate request shape before any filesystem or network work occurs.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.source_paths.is_empty() {
            return Err(PolicyError::EmptySources);
        }
        if self.destination_folder_id.trim().is_empty() {
            return Err(PolicyError::MissingDestination);
        }
        if self.idempotency_key.trim().is_empty() {
            return Err(PolicyError::MissingIdempotencyKey);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> TransferRequest {
        TransferRequest {
            peer_id: "peer-1".into(),
            source_paths: vec!["report.pdf".into()],
            destination_folder_id: "shared".into(),
            idempotency_key: "job-1".into(),
        }
    }

    #[test]
    fn accepts_a_well_formed_request() {
        assert_eq!(request().validate(), Ok(()));
    }

    #[test]
    fn rejects_empty_sources() {
        let mut value = request();
        value.source_paths.clear();
        assert_eq!(value.validate(), Err(PolicyError::EmptySources));
    }

    #[test]
    fn rejects_missing_destination_and_key() {
        let mut value = request();
        value.destination_folder_id = " ".into();
        assert_eq!(value.validate(), Err(PolicyError::MissingDestination));

        let mut value = request();
        value.idempotency_key = "".into();
        assert_eq!(value.validate(), Err(PolicyError::MissingIdempotencyKey));
    }
}
