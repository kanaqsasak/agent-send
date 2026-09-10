//! Transport-independent transfer engine.
//!
//! The engine only accepts named [`PathPolicy`] capabilities. `LoopbackTransport`
//! is the deterministic transport seam used now; a future HTTP/QUIC adapter can
//! carry the same manifest and chunk stream without moving policy into transport.

use agent_send_core::{
    path_policy::{PathOperation, PathPolicy, PathPolicyError},
    TransferRequest,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    pub bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub bytes: u64,
    pub files: u64,
    pub sha256: String,
}

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("invalid transfer request: {0}")]
    Request(#[from] agent_send_core::PolicyError),
    #[error("shared folder capability not found: {0}")]
    FolderNotFound(String),
    #[error("path policy rejected transfer path: {0}")]
    Policy(#[from] PathPolicyError),
    #[error("filesystem error: {0}")]
    Io(#[from] io::Error),
    #[error("transfer was cancelled")]
    Cancelled,
    #[error("destination already exists: {0}")]
    DestinationExists(PathBuf),
}

#[derive(Debug, Clone)]
struct FileItem {
    source: PathBuf,
    relative: PathBuf,
    size: u64,
}

/// A daemon-owned collection of named folder capabilities.
#[derive(Debug, Default)]
pub struct TransferEngine {
    folders: Mutex<BTreeMap<String, PathPolicy>>,
    completed: Mutex<BTreeMap<String, TransferOutcome>>,
}

impl TransferEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_folder(&self, id: impl Into<String>, policy: PathPolicy) {
        self.folders.lock().unwrap().insert(id.into(), policy);
    }

    pub fn remove_folder(&self, id: &str) -> bool {
        self.folders.lock().unwrap().remove(id).is_some()
    }

    pub fn folder_ids(&self) -> Vec<String> {
        self.folders.lock().unwrap().keys().cloned().collect()
    }

    pub fn folders(&self) -> Vec<(String, agent_send_core::FolderDirection)> {
        self.folders
            .lock()
            .unwrap()
            .iter()
            .map(|(id, policy)| (id.clone(), policy.direction().clone()))
            .collect()
    }

    /// Check an agent submission before it is handed to a peer transport.
    /// Only named source capabilities and relative paths are accepted.
    pub fn validate_submission(&self, request: &TransferRequest) -> Result<(), TransferError> {
        request.validate()?;
        let source = self.folder(&request.source_folder_id)?;
        for path in &request.source_paths {
            source.resolve(path, PathOperation::Read)?;
        }
        Ok(())
    }

    /// Send through the in-process loopback seam. No network discovery is involved.
    pub fn send_loopback<F>(
        &self,
        request: &TransferRequest,
        receiver: &TransferEngine,
        cancel: &Cancellation,
        progress: F,
    ) -> Result<TransferOutcome, TransferError>
    where
        F: FnMut(TransferProgress),
    {
        request.validate()?;
        let source = self.folder(&request.source_folder_id)?;
        receiver.receive(request, &source, cancel, progress)
    }

    fn folder(&self, id: &str) -> Result<PathPolicy, TransferError> {
        self.folders
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| TransferError::FolderNotFound(id.into()))
    }

    fn receive<F>(
        &self,
        request: &TransferRequest,
        source: &PathPolicy,
        cancel: &Cancellation,
        mut progress: F,
    ) -> Result<TransferOutcome, TransferError>
    where
        F: FnMut(TransferProgress),
    {
        request.validate()?;
        if let Some(done) = self
            .completed
            .lock()
            .unwrap()
            .get(&request.idempotency_key)
            .cloned()
        {
            return Ok(done);
        }
        let destination = self.folder(&request.destination_folder_id)?;
        let mut items = Vec::new();
        for name in &request.source_paths {
            let relative = PathBuf::from(name);
            let path = source.resolve(&relative, PathOperation::Read)?;
            collect_items(source, &path, &relative, &mut items)?;
        }
        let total = items.iter().map(|i| i.size).sum();
        let mut bytes = 0;
        let mut files = 0;
        let mut hash = Sha256::new();
        for item in items {
            check_cancel(cancel)?;
            let target = destination.resolve(&item.relative, PathOperation::Write)?;
            if item.source.is_dir() {
                if target.exists() {
                    if !target.is_dir() {
                        return Err(TransferError::DestinationExists(target));
                    }
                } else {
                    fs::create_dir_all(&target)?;
                }
                continue;
            }
            if target.exists() {
                return Err(TransferError::DestinationExists(target));
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let temp = temp_path(&target);
            let result = copy_file(
                &item.source,
                &temp,
                cancel,
                &mut hash,
                &mut bytes,
                total,
                &mut progress,
            );
            if result.is_err() {
                let _ = fs::remove_file(&temp);
            }
            result?;
            fs::rename(&temp, &target)?;
            files += 1;
        }
        let outcome = TransferOutcome {
            bytes,
            files,
            sha256: hex(&hash.finalize()),
        };
        self.completed
            .lock()
            .unwrap()
            .insert(request.idempotency_key.clone(), outcome.clone());
        Ok(outcome)
    }
}

/// A cancellable token suitable for local API handlers and UI clients.
#[derive(Debug, Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Named explicitly to document the protocol boundary. It is intentionally tiny:
/// a future streamed transport implements the same operation over a connection.
pub struct LoopbackTransport;
impl LoopbackTransport {
    pub fn send<F>(
        sender: &TransferEngine,
        receiver: &TransferEngine,
        request: &TransferRequest,
        cancel: &Cancellation,
        progress: F,
    ) -> Result<TransferOutcome, TransferError>
    where
        F: FnMut(TransferProgress),
    {
        sender.send_loopback(request, receiver, cancel, progress)
    }
}

fn collect_items(
    policy: &PathPolicy,
    path: &Path,
    relative: &Path,
    out: &mut Vec<FileItem>,
) -> Result<(), TransferError> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        out.push(FileItem {
            source: path.to_owned(),
            relative: relative.to_owned(),
            size: metadata.len(),
        });
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsupported source").into());
    }
    // Validate every discovered path through the shared capability, including directory names.
    policy.resolve(relative, PathOperation::Read)?;
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let child = relative.join(entry.file_name());
        let child_path = policy.resolve(&child, PathOperation::Read)?;
        collect_items(policy, &child_path, &child, out)?;
    }
    out.push(FileItem {
        source: path.to_owned(),
        relative: relative.to_owned(),
        size: 0,
    });
    Ok(())
}

fn copy_file<F>(
    source: &Path,
    temp: &Path,
    cancel: &Cancellation,
    hash: &mut Sha256,
    bytes: &mut u64,
    total: u64,
    progress: &mut F,
) -> Result<(), TransferError>
where
    F: FnMut(TransferProgress),
{
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new().write(true).create_new(true).open(temp)?;
    let mut buffer = [0u8; CHUNK_SIZE];
    loop {
        check_cancel(cancel)?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        *bytes += count as u64;
        progress(TransferProgress {
            bytes: *bytes,
            total_bytes: total,
        });
    }
    output.sync_all()?;
    Ok(())
}
fn check_cancel(cancel: &Cancellation) -> Result<(), TransferError> {
    if cancel.is_cancelled() {
        Err(TransferError::Cancelled)
    } else {
        Ok(())
    }
}
fn temp_path(target: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    target.with_extension(format!("agent-send-{stamp}-{}.part", std::process::id()))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent-send-transfer-{label}-{}",
            std::process::id()
        ))
    }
    fn request(key: &str, sources: Vec<&str>) -> TransferRequest {
        TransferRequest {
            peer_id: "peer".into(),
            source_folder_id: "out".into(),
            source_paths: sources.into_iter().map(str::to_owned).collect(),
            destination_folder_id: "in".into(),
            idempotency_key: key.into(),
        }
    }
    #[test]
    fn file_nested_hash_counts_and_idempotency() {
        let a = root("a");
        let b = root("b");
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
        fs::create_dir_all(a.join("nested")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("nested/x"), b"hello").unwrap();
        let sender = TransferEngine::new();
        let receiver = TransferEngine::new();
        sender.add_folder(
            "out",
            PathPolicy::new(&a, agent_send_core::FolderDirection::Read),
        );
        receiver.add_folder(
            "in",
            PathPolicy::new(&b, agent_send_core::FolderDirection::Write),
        );
        let outcome = LoopbackTransport::send(
            &sender,
            &receiver,
            &request("one", vec!["nested"]),
            &Cancellation::new(),
            |_| {},
        )
        .unwrap();
        assert_eq!(outcome.bytes, 5);
        assert_eq!(outcome.files, 1);
        assert_eq!(
            outcome.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(fs::read(b.join("nested/x")).unwrap(), b"hello");
        assert_eq!(
            LoopbackTransport::send(
                &sender,
                &receiver,
                &request("one", vec!["nested"]),
                &Cancellation::new(),
                |_| {}
            )
            .unwrap(),
            outcome
        );
        let _ = fs::remove_dir_all(a);
        let _ = fs::remove_dir_all(b);
    }
    #[test]
    fn denied_destination_and_cancellation_are_safe() {
        let a = root("c");
        let b = root("d");
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("x"), vec![1u8; CHUNK_SIZE * 2]).unwrap();
        let s = TransferEngine::new();
        let r = TransferEngine::new();
        s.add_folder(
            "out",
            PathPolicy::new(&a, agent_send_core::FolderDirection::Read),
        );
        r.add_folder(
            "in",
            PathPolicy::new(&b, agent_send_core::FolderDirection::Read),
        );
        assert!(matches!(
            LoopbackTransport::send(
                &s,
                &r,
                &request("deny", vec!["x"]),
                &Cancellation::new(),
                |_| {}
            ),
            Err(TransferError::Policy(PathPolicyError::WriteNotAllowed))
        ));
        let r = TransferEngine::new();
        r.add_folder(
            "in",
            PathPolicy::new(&b, agent_send_core::FolderDirection::Write),
        );
        let c = Cancellation::new();
        c.cancel();
        assert!(matches!(
            LoopbackTransport::send(&s, &r, &request("cancel", vec!["x"]), &c, |_| {}),
            Err(TransferError::Cancelled)
        ));
        assert!(!b.join("x").exists());
        let _ = fs::remove_dir_all(a);
        let _ = fs::remove_dir_all(b);
        std::thread::sleep(Duration::from_millis(1));
    }
}
