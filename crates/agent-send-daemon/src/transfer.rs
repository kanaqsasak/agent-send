//! Transport-independent transfer engine.
//!
//! The engine only accepts named [`PathPolicy`] capabilities. Socket framing is
//! kept in `peer_transport`; this module prepares and consumes the same bounded
//! manifest/chunk stream while retaining all filesystem policy decisions here.

use crate::peer_transport::PeerMessage;
use agent_send_core::{
    path_policy::{PathOperation, PathPolicy, PathPolicyError},
    TransferRequest,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    #[error("peer transfer protocol error: {0}")]
    Protocol(&'static str),
    #[error("peer transfer byte count did not match its manifest")]
    SizeMismatch,
    #[error("peer transfer hash did not match its manifest")]
    HashMismatch,
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

    pub(crate) fn prepare_outbound(
        &self,
        request: &TransferRequest,
    ) -> Result<PreparedTransfer, TransferError> {
        self.validate_submission(request)?;
        let source = self.folder(&request.source_folder_id)?;
        let mut items = Vec::new();
        for name in &request.source_paths {
            let relative = PathBuf::from(name);
            let path = source.resolve(&relative, PathOperation::Read)?;
            collect_items(&source, &path, &relative, &mut items)?;
        }
        let total_bytes = items.iter().map(|item| item.size).sum();
        let sha256 = hash_items(&items)?;
        Ok(PreparedTransfer {
            transfer_id: request.idempotency_key.clone(),
            destination_folder_id: request.destination_folder_id.clone(),
            idempotency_key: request.idempotency_key.clone(),
            total_bytes,
            sha256,
            items,
        })
    }

    pub(crate) fn begin_incoming(
        self: &Arc<Self>,
        transfer_id: String,
        destination_folder_id: String,
        idempotency_key: String,
        total_bytes: u64,
        sha256: String,
    ) -> Result<IncomingStart, TransferError> {
        if let Some(outcome) = self
            .completed
            .lock()
            .unwrap()
            .get(&idempotency_key)
            .cloned()
        {
            return Ok(IncomingStart::Completed(outcome));
        }
        let destination = self.folder(&destination_folder_id)?;
        // Check the named capability and its write direction before telling a
        // remote peer that it may send any file content.
        destination.resolve(Path::new("."), PathOperation::Write)?;
        Ok(IncomingStart::Ready(IncomingTransfer {
            engine: self.clone(),
            transfer_id,
            idempotency_key,
            destination,
            total_bytes,
            expected_hash: sha256,
            bytes: 0,
            files: 0,
            hash: Sha256::new(),
            active_file: None,
            targets: BTreeSet::new(),
            committed_targets: Vec::new(),
            completed: false,
        }))
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

pub(crate) struct PreparedTransfer {
    transfer_id: String,
    destination_folder_id: String,
    idempotency_key: String,
    total_bytes: u64,
    sha256: String,
    items: Vec<FileItem>,
}

impl PreparedTransfer {
    pub(crate) fn manifest(&self) -> PeerMessage {
        PeerMessage::TransferManifest {
            transfer_id: self.transfer_id.clone(),
            destination_folder_id: self.destination_folder_id.clone(),
            idempotency_key: self.idempotency_key.clone(),
            total_bytes: self.total_bytes,
            sha256: self.sha256.clone(),
        }
    }

    pub(crate) fn stream<F, E>(
        self,
        cancel: &Cancellation,
        mut progress: F,
        mut send: impl FnMut(PeerMessage) -> Result<(), E>,
    ) -> Result<TransferOutcome, E>
    where
        F: FnMut(TransferProgress),
        E: From<TransferError>,
    {
        let mut bytes = 0;
        let mut files = 0;
        for item in self.items {
            if cancel.is_cancelled() {
                let _ = send(PeerMessage::Cancel {
                    transfer_id: self.transfer_id.clone(),
                });
                return Err(TransferError::Cancelled.into());
            }
            let relative = item.relative.to_string_lossy().into_owned();
            if item.source.is_dir() {
                send(PeerMessage::Directory {
                    transfer_id: self.transfer_id.clone(),
                    relative_path: relative,
                })?;
                continue;
            }
            send(PeerMessage::FileStart {
                transfer_id: self.transfer_id.clone(),
                relative_path: relative,
                size: item.size,
            })?;
            let mut input = File::open(&item.source).map_err(TransferError::from)?;
            let mut offset = 0;
            let mut buffer = [0u8; CHUNK_SIZE];
            loop {
                if cancel.is_cancelled() {
                    let _ = send(PeerMessage::Cancel {
                        transfer_id: self.transfer_id.clone(),
                    });
                    return Err(TransferError::Cancelled.into());
                }
                let count = input.read(&mut buffer).map_err(TransferError::from)?;
                if count == 0 {
                    break;
                }
                send(PeerMessage::FileChunk {
                    transfer_id: self.transfer_id.clone(),
                    offset,
                    bytes: buffer[..count].to_vec(),
                })?;
                offset += count as u64;
                bytes += count as u64;
                progress(TransferProgress {
                    bytes,
                    total_bytes: self.total_bytes,
                });
            }
            send(PeerMessage::FileComplete {
                transfer_id: self.transfer_id.clone(),
            })?;
            files += 1;
        }
        send(PeerMessage::TransferComplete {
            transfer_id: self.transfer_id.clone(),
        })?;
        Ok(TransferOutcome {
            bytes,
            files,
            sha256: self.sha256,
        })
    }
}

pub(crate) enum IncomingStart {
    Ready(IncomingTransfer),
    Completed(TransferOutcome),
}

pub(crate) struct IncomingTransfer {
    engine: Arc<TransferEngine>,
    transfer_id: String,
    idempotency_key: String,
    destination: PathPolicy,
    total_bytes: u64,
    expected_hash: String,
    bytes: u64,
    files: u64,
    hash: Sha256,
    active_file: Option<IncomingFile>,
    targets: BTreeSet<PathBuf>,
    committed_targets: Vec<PathBuf>,
    completed: bool,
}

struct IncomingFile {
    target: PathBuf,
    temp: PathBuf,
    file: File,
    expected_size: u64,
    offset: u64,
}

impl IncomingTransfer {
    pub(crate) fn directory(
        &mut self,
        transfer_id: &str,
        relative: &str,
    ) -> Result<(), TransferError> {
        self.require_id(transfer_id)?;
        if self.active_file.is_some() {
            return Err(TransferError::Protocol(
                "directory arrived while a file is open",
            ));
        }
        let target = self.destination.resolve(relative, PathOperation::Write)?;
        if target.exists() && !target.is_dir() {
            return Err(TransferError::DestinationExists(target));
        }
        fs::create_dir_all(target)?;
        Ok(())
    }

    pub(crate) fn file_start(
        &mut self,
        transfer_id: &str,
        relative: &str,
        size: u64,
    ) -> Result<(), TransferError> {
        self.require_id(transfer_id)?;
        if self.active_file.is_some() {
            return Err(TransferError::Protocol(
                "file started while another file is open",
            ));
        }
        let target = self.destination.resolve(relative, PathOperation::Write)?;
        if target.exists() || !self.targets.insert(target.clone()) {
            return Err(TransferError::DestinationExists(target));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = temp_path(&target);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        self.active_file = Some(IncomingFile {
            target,
            temp,
            file,
            expected_size: size,
            offset: 0,
        });
        Ok(())
    }

    pub(crate) fn chunk(
        &mut self,
        transfer_id: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), TransferError> {
        self.require_id(transfer_id)?;
        if bytes.len() > CHUNK_SIZE {
            return Err(TransferError::Protocol(
                "file chunk exceeds the protocol limit",
            ));
        }
        let file = self
            .active_file
            .as_mut()
            .ok_or(TransferError::Protocol("file chunk arrived without a file"))?;
        if file.offset != offset {
            return Err(TransferError::Protocol(
                "file chunk offset is not monotonic",
            ));
        }
        file.file.write_all(bytes)?;
        file.offset += bytes.len() as u64;
        if file.offset > file.expected_size {
            return Err(TransferError::SizeMismatch);
        }
        self.bytes += bytes.len() as u64;
        if self.bytes > self.total_bytes {
            return Err(TransferError::SizeMismatch);
        }
        self.hash.update(bytes);
        Ok(())
    }

    pub(crate) fn file_complete(&mut self, transfer_id: &str) -> Result<(), TransferError> {
        self.require_id(transfer_id)?;
        let file = self
            .active_file
            .take()
            .ok_or(TransferError::Protocol("file completed without a file"))?;
        if file.offset != file.expected_size {
            let _ = fs::remove_file(&file.temp);
            return Err(TransferError::SizeMismatch);
        }
        file.file.sync_all()?;
        drop(file.file);
        fs::rename(&file.temp, &file.target)?;
        self.committed_targets.push(file.target);
        self.files += 1;
        Ok(())
    }

    pub(crate) fn complete(&mut self, transfer_id: &str) -> Result<TransferOutcome, TransferError> {
        self.require_id(transfer_id)?;
        if self.active_file.is_some() {
            return Err(TransferError::Protocol(
                "transfer completed while a file is open",
            ));
        }
        if self.bytes != self.total_bytes {
            return Err(TransferError::SizeMismatch);
        }
        let outcome = TransferOutcome {
            bytes: self.bytes,
            files: self.files,
            sha256: hex(&self.hash.clone().finalize()),
        };
        if !outcome.sha256.eq_ignore_ascii_case(&self.expected_hash) {
            return Err(TransferError::HashMismatch);
        }
        self.engine
            .completed
            .lock()
            .unwrap()
            .insert(self.idempotency_key.clone(), outcome.clone());
        self.completed = true;
        Ok(outcome)
    }

    fn require_id(&self, transfer_id: &str) -> Result<(), TransferError> {
        if self.transfer_id == transfer_id {
            Ok(())
        } else {
            Err(TransferError::Protocol(
                "message belongs to a different transfer",
            ))
        }
    }
}

impl Drop for IncomingTransfer {
    fn drop(&mut self) {
        if let Some(file) = self.active_file.take() {
            let _ = fs::remove_file(file.temp);
        }
        if !self.completed {
            for target in self.committed_targets.iter().rev() {
                let _ = fs::remove_file(target);
            }
        }
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
    policy.resolve(relative, PathOperation::Read)?;
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
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

fn hash_items(items: &[FileItem]) -> Result<String, TransferError> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; CHUNK_SIZE];
    for item in items.iter().filter(|item| item.source.is_file()) {
        let mut file = File::open(&item.source)?;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
    }
    Ok(hex(&hash.finalize()))
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
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
