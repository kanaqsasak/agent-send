//! Versioned, encrypted peer control and transfer framing.
//!
//! Pairing must establish a distinct 32-byte secret per trusted peer over the
//! human-confirmed pairing flow. This module deliberately does not derive trust
//! from mDNS records or accept a secret from the network. It encrypts every
//! control message and file chunk with ChaCha20-Poly1305 and rejects version,
//! peer, and sequence mismatches before handing a message to transfer policy.

use crate::{
    transfer::{IncomingStart, TransferEngine},
    Cancellation, TransferError, TransferOutcome, TransferProgress, CHUNK_SIZE,
};
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::VecDeque,
    fmt,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::Arc,
};
use thiserror::Error;

pub const PEER_PROTOCOL_VERSION: u32 = 1;
const KEY_DOMAIN: &[u8] = b"agent-send peer channel key v1";
const AAD_DOMAIN: &[u8] = b"agent-send peer frame v1";
const MAX_FRAME_BYTES: usize = CHUNK_SIZE + 16 * 1024;

/// Secret established during an out-of-band, human-confirmed pairing.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingSecret([u8; 32]);

impl PairingSecret {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for PairingSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairingSecret([REDACTED])")
    }
}

/// Credentials for one peer that has completed the local pairing flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedPeer {
    id: String,
    secret: PairingSecret,
}

impl PairedPeer {
    pub fn new(id: impl Into<String>, secret: PairingSecret) -> Result<Self, PeerChannelError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(PeerChannelError::EmptyPeerId);
        }
        Ok(Self { id, secret })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedPeerFrame {
    pub version: u32,
    pub sender_id: String,
    pub recipient_id: String,
    pub sequence: u64,
    pub ciphertext: Vec<u8>,
}

/// Control and streamed-transfer messages transported in encrypted frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerMessage {
    Hello {
        protocol_version: u32,
    },
    TransferManifest {
        transfer_id: String,
        destination_folder_id: String,
        idempotency_key: String,
        total_bytes: u64,
        sha256: String,
    },
    TransferReady {
        transfer_id: String,
    },
    Directory {
        transfer_id: String,
        relative_path: String,
    },
    FileStart {
        transfer_id: String,
        relative_path: String,
        size: u64,
    },
    FileChunk {
        transfer_id: String,
        offset: u64,
        bytes: Vec<u8>,
    },
    FileComplete {
        transfer_id: String,
    },
    TransferComplete {
        transfer_id: String,
    },
    TransferResult {
        transfer_id: String,
        bytes: u64,
        files: u64,
        sha256: String,
    },
    TransferRejected {
        transfer_id: String,
    },
    Cancel {
        transfer_id: String,
    },
}

impl PeerMessage {
    fn validate(&self) -> Result<(), PeerChannelError> {
        match self {
            Self::Hello { protocol_version } if *protocol_version != PEER_PROTOCOL_VERSION => {
                Err(PeerChannelError::UnsupportedVersion(*protocol_version))
            }
            Self::TransferManifest {
                transfer_id,
                destination_folder_id,
                idempotency_key,
                sha256,
                ..
            } => {
                required("transfer_id", transfer_id)?;
                required("destination_folder_id", destination_folder_id)?;
                required("idempotency_key", idempotency_key)?;
                valid_hash(sha256)
            }
            Self::Directory {
                transfer_id,
                relative_path,
            }
            | Self::FileStart {
                transfer_id,
                relative_path,
                ..
            } => {
                required("transfer_id", transfer_id)?;
                required("relative_path", relative_path)
            }
            Self::FileChunk {
                transfer_id, bytes, ..
            } => {
                required("transfer_id", transfer_id)?;
                if bytes.len() > CHUNK_SIZE {
                    return Err(PeerChannelError::ChunkTooLarge(bytes.len()));
                }
                Ok(())
            }
            Self::TransferResult {
                transfer_id,
                sha256,
                ..
            } => {
                required("transfer_id", transfer_id)?;
                valid_hash(sha256)
            }
            Self::TransferReady { transfer_id }
            | Self::FileComplete { transfer_id }
            | Self::TransferComplete { transfer_id }
            | Self::TransferRejected { transfer_id }
            | Self::Cancel { transfer_id } => required("transfer_id", transfer_id),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PeerChannelError {
    #[error("peer IDs must not be empty")]
    EmptyPeerId,
    #[error("a peer channel cannot target its own identity")]
    SamePeer,
    #[error("unsupported peer protocol version: {0}")]
    UnsupportedVersion(u32),
    #[error("encrypted frame was addressed to a different peer")]
    WrongPeer,
    #[error("replayed or out-of-order frame: expected {expected}, got {received}")]
    UnexpectedSequence { expected: u64, received: u64 },
    #[error("encrypted frame authentication failed")]
    AuthenticationFailed,
    #[error("failed to encode or decode a peer message")]
    Serialization,
    #[error("{0} is required")]
    MissingField(&'static str),
    #[error("transfer hash must be a hexadecimal SHA-256 value")]
    InvalidHash,
    #[error("file chunk exceeds the {CHUNK_SIZE}-byte protocol limit: {0}")]
    ChunkTooLarge(usize),
    #[error("peer frame sequence space exhausted")]
    SequenceExhausted,
}

pub struct SecurePeerChannel {
    local_id: String,
    peer_id: String,
    send_cipher: ChaCha20Poly1305,
    receive_cipher: ChaCha20Poly1305,
    next_send_sequence: u64,
    next_receive_sequence: u64,
}

impl SecurePeerChannel {
    pub fn new(local_id: impl Into<String>, peer: PairedPeer) -> Result<Self, PeerChannelError> {
        let local_id = local_id.into();
        if local_id.trim().is_empty() {
            return Err(PeerChannelError::EmptyPeerId);
        }
        if local_id == peer.id {
            return Err(PeerChannelError::SamePeer);
        }
        let send_key = derive_key(&local_id, &peer.id, &peer.secret);
        let receive_key = derive_key(&peer.id, &local_id, &peer.secret);
        Ok(Self {
            local_id,
            peer_id: peer.id,
            send_cipher: ChaCha20Poly1305::new((&send_key).into()),
            receive_cipher: ChaCha20Poly1305::new((&receive_key).into()),
            next_send_sequence: 0,
            next_receive_sequence: 0,
        })
    }

    pub fn seal(&mut self, message: &PeerMessage) -> Result<EncryptedPeerFrame, PeerChannelError> {
        message.validate()?;
        let sequence = self.next_send_sequence;
        self.next_send_sequence = self
            .next_send_sequence
            .checked_add(1)
            .ok_or(PeerChannelError::SequenceExhausted)?;
        let plaintext = serde_json::to_vec(message).map_err(|_| PeerChannelError::Serialization)?;
        let sender_id = self.local_id.clone();
        let recipient_id = self.peer_id.clone();
        let ciphertext = self
            .send_cipher
            .encrypt(
                &nonce(sequence),
                Payload {
                    msg: &plaintext,
                    aad: &associated_data(
                        PEER_PROTOCOL_VERSION,
                        &sender_id,
                        &recipient_id,
                        sequence,
                    ),
                },
            )
            .map_err(|_| PeerChannelError::AuthenticationFailed)?;
        Ok(EncryptedPeerFrame {
            version: PEER_PROTOCOL_VERSION,
            sender_id,
            recipient_id,
            sequence,
            ciphertext,
        })
    }

    pub fn open(&mut self, frame: EncryptedPeerFrame) -> Result<PeerMessage, PeerChannelError> {
        if frame.version != PEER_PROTOCOL_VERSION {
            return Err(PeerChannelError::UnsupportedVersion(frame.version));
        }
        if frame.sender_id != self.peer_id || frame.recipient_id != self.local_id {
            return Err(PeerChannelError::WrongPeer);
        }
        if frame.sequence != self.next_receive_sequence {
            return Err(PeerChannelError::UnexpectedSequence {
                expected: self.next_receive_sequence,
                received: frame.sequence,
            });
        }
        let plaintext = self
            .receive_cipher
            .decrypt(
                &nonce(frame.sequence),
                Payload {
                    msg: &frame.ciphertext,
                    aad: &associated_data(
                        frame.version,
                        &frame.sender_id,
                        &frame.recipient_id,
                        frame.sequence,
                    ),
                },
            )
            .map_err(|_| PeerChannelError::AuthenticationFailed)?;
        let message: PeerMessage =
            serde_json::from_slice(&plaintext).map_err(|_| PeerChannelError::Serialization)?;
        message.validate()?;
        self.next_receive_sequence = self
            .next_receive_sequence
            .checked_add(1)
            .ok_or(PeerChannelError::SequenceExhausted)?;
        Ok(message)
    }
}

pub trait PeerTransport {
    fn send(&mut self, frame: EncryptedPeerFrame) -> Result<(), PeerTransportError>;
    fn receive(&mut self) -> Result<Option<EncryptedPeerFrame>, PeerTransportError>;
}

#[derive(Debug, Error)]
pub enum PeerTransportError {
    #[error("peer transport is unavailable")]
    Unavailable,
    #[error("peer socket I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("peer frame was invalid")]
    Serialization,
    #[error("peer frame exceeds the socket limit")]
    FrameTooLarge,
}

pub struct PeerConnection<T> {
    channel: SecurePeerChannel,
    transport: T,
}

impl<T: PeerTransport> PeerConnection<T> {
    pub fn new(channel: SecurePeerChannel, transport: T) -> Self {
        Self { channel, transport }
    }

    pub fn send(&mut self, message: &PeerMessage) -> Result<(), PeerConnectionError> {
        let frame = self.channel.seal(message)?;
        self.transport.send(frame)?;
        Ok(())
    }

    pub fn receive(&mut self) -> Result<Option<PeerMessage>, PeerConnectionError> {
        self.transport
            .receive()?
            .map(|frame| self.channel.open(frame))
            .transpose()
            .map_err(Into::into)
    }

    pub fn into_transport(self) -> T {
        self.transport
    }
}

#[derive(Debug, Error)]
pub enum PeerConnectionError {
    #[error(transparent)]
    Channel(#[from] PeerChannelError),
    #[error(transparent)]
    Transport(#[from] PeerTransportError),
}

/// Length-delimited encrypted frames over a TCP socket. The plaintext protocol
/// is never serialized directly to this transport.
pub struct SocketPeerTransport {
    stream: TcpStream,
}

impl SocketPeerTransport {
    pub fn connect(address: SocketAddr) -> Result<Self, PeerTransportError> {
        Ok(Self {
            stream: TcpStream::connect(address)?,
        })
    }

    pub fn from_stream(stream: TcpStream) -> Self {
        Self { stream }
    }
}

impl PeerTransport for SocketPeerTransport {
    fn send(&mut self, frame: EncryptedPeerFrame) -> Result<(), PeerTransportError> {
        let bytes = serde_json::to_vec(&frame).map_err(|_| PeerTransportError::Serialization)?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(PeerTransportError::FrameTooLarge);
        }
        self.stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        self.stream.write_all(&bytes)?;
        self.stream.flush()?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<EncryptedPeerFrame>, PeerTransportError> {
        let mut length = [0; 4];
        match self.stream.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(PeerTransportError::FrameTooLarge);
        }
        let mut bytes = vec![0; length];
        self.stream.read_exact(&mut bytes)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| PeerTransportError::Serialization)
    }
}

#[derive(Debug, Error)]
pub enum SocketTransferError {
    #[error(transparent)]
    Transfer(#[from] TransferError),
    #[error(transparent)]
    Connection(#[from] PeerConnectionError),
    #[error("peer closed the connection during transfer")]
    Closed,
    #[error("peer rejected the transfer")]
    Rejected,
    #[error("peer sent an unexpected transfer response")]
    UnexpectedResponse,
}

/// Send a policy-validated transfer through a paired encrypted socket channel.
pub fn send_socket_transfer<F>(
    engine: &TransferEngine,
    channel: SecurePeerChannel,
    address: SocketAddr,
    request: &agent_send_core::TransferRequest,
    cancel: &Cancellation,
    progress: F,
) -> Result<TransferOutcome, SocketTransferError>
where
    F: FnMut(TransferProgress),
{
    let prepared = engine.prepare_outbound(request)?;
    let transport = SocketPeerTransport::connect(address).map_err(PeerConnectionError::from)?;
    let mut connection = PeerConnection::new(channel, transport);
    connection.send(&PeerMessage::Hello {
        protocol_version: PEER_PROTOCOL_VERSION,
    })?;
    connection.send(&prepared.manifest())?;
    match connection.receive()? {
        Some(PeerMessage::TransferReady { transfer_id })
            if transfer_id == request.idempotency_key => {}
        Some(PeerMessage::TransferResult {
            transfer_id,
            bytes,
            files,
            sha256,
        }) if transfer_id == request.idempotency_key => {
            return Ok(TransferOutcome {
                bytes,
                files,
                sha256,
            });
        }
        Some(PeerMessage::TransferRejected { transfer_id })
            if transfer_id == request.idempotency_key =>
        {
            return Err(SocketTransferError::Rejected)
        }
        Some(_) => return Err(SocketTransferError::UnexpectedResponse),
        None => return Err(SocketTransferError::Closed),
    }
    let local_outcome = prepared.stream(cancel, progress, |message| {
        connection.send(&message).map_err(SocketTransferError::from)
    })?;
    match connection.receive()? {
        Some(PeerMessage::TransferResult {
            transfer_id,
            bytes,
            files,
            sha256,
        }) if transfer_id == request.idempotency_key
            && bytes == local_outcome.bytes
            && files == local_outcome.files
            && sha256 == local_outcome.sha256 =>
        {
            Ok(local_outcome)
        }
        Some(PeerMessage::TransferRejected { transfer_id })
            if transfer_id == request.idempotency_key =>
        {
            Err(SocketTransferError::Rejected)
        }
        Some(_) => Err(SocketTransferError::UnexpectedResponse),
        None => Err(SocketTransferError::Closed),
    }
}

/// Receive one encrypted transfer after a verified `Hello`. Errors are exposed
/// to the authenticated sender only as a generic rejection.
pub(crate) fn receive_socket_transfer(
    connection: &mut PeerConnection<SocketPeerTransport>,
    engine: Arc<TransferEngine>,
) -> Result<(), PeerConnectionError> {
    let Some(PeerMessage::TransferManifest {
        transfer_id,
        destination_folder_id,
        idempotency_key,
        total_bytes,
        sha256,
    }) = connection.receive()?
    else {
        return Ok(());
    };
    let mut transfer = match engine.begin_incoming(
        transfer_id.clone(),
        destination_folder_id,
        idempotency_key,
        total_bytes,
        sha256,
    ) {
        Ok(IncomingStart::Ready(transfer)) => {
            connection.send(&PeerMessage::TransferReady {
                transfer_id: transfer_id.clone(),
            })?;
            transfer
        }
        Ok(IncomingStart::Completed(outcome)) => {
            connection.send(&result_message(&transfer_id, outcome))?;
            return Ok(());
        }
        Err(_) => {
            connection.send(&PeerMessage::TransferRejected { transfer_id })?;
            return Ok(());
        }
    };

    loop {
        let message = match connection.receive()? {
            Some(message) => message,
            None => return Ok(()),
        };
        let outcome = match message {
            PeerMessage::Directory {
                transfer_id,
                relative_path,
            } => transfer
                .directory(&transfer_id, &relative_path)
                .map(|_| None),
            PeerMessage::FileStart {
                transfer_id,
                relative_path,
                size,
            } => transfer
                .file_start(&transfer_id, &relative_path, size)
                .map(|_| None),
            PeerMessage::FileChunk {
                transfer_id,
                offset,
                bytes,
            } => transfer.chunk(&transfer_id, offset, &bytes).map(|_| None),
            PeerMessage::FileComplete { transfer_id } => {
                transfer.file_complete(&transfer_id).map(|_| None)
            }
            PeerMessage::TransferComplete { transfer_id } => {
                transfer.complete(&transfer_id).map(Some)
            }
            PeerMessage::Cancel { .. } => return Ok(()),
            _ => Err(TransferError::Protocol("unexpected peer transfer message")),
        };
        match outcome {
            Ok(Some(outcome)) => {
                connection.send(&result_message(&transfer_id, outcome))?;
                return Ok(());
            }
            Ok(None) => {}
            Err(_) => {
                connection.send(&PeerMessage::TransferRejected { transfer_id })?;
                return Ok(());
            }
        }
    }
}

fn result_message(transfer_id: &str, outcome: TransferOutcome) -> PeerMessage {
    PeerMessage::TransferResult {
        transfer_id: transfer_id.into(),
        bytes: outcome.bytes,
        files: outcome.files,
        sha256: outcome.sha256,
    }
}

#[derive(Debug, Default)]
pub struct MockPeerTransport {
    inbound: VecDeque<EncryptedPeerFrame>,
    outbound: VecDeque<EncryptedPeerFrame>,
}

impl MockPeerTransport {
    pub fn pair() -> (Self, Self) {
        (Self::default(), Self::default())
    }

    pub fn deliver_to(&mut self, peer: &mut Self) {
        peer.inbound.append(&mut self.outbound);
    }
}

impl PeerTransport for MockPeerTransport {
    fn send(&mut self, frame: EncryptedPeerFrame) -> Result<(), PeerTransportError> {
        self.outbound.push_back(frame);
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<EncryptedPeerFrame>, PeerTransportError> {
        Ok(self.inbound.pop_front())
    }
}

fn required(name: &'static str, value: &str) -> Result<(), PeerChannelError> {
    if value.trim().is_empty() {
        Err(PeerChannelError::MissingField(name))
    } else {
        Ok(())
    }
}

fn valid_hash(hash: &str) -> Result<(), PeerChannelError> {
    if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(PeerChannelError::InvalidHash)
    }
}

fn derive_key(sender_id: &str, recipient_id: &str, secret: &PairingSecret) -> [u8; 32] {
    let mut info = Vec::with_capacity(sender_id.len() + recipient_id.len() + 16);
    for value in [sender_id, recipient_id] {
        info.extend_from_slice(&(value.len() as u64).to_be_bytes());
        info.extend_from_slice(value.as_bytes());
    }
    let mut key = [0; 32];
    Hkdf::<Sha256>::new(Some(KEY_DOMAIN), &secret.0)
        .expand(&info, &mut key)
        .expect("32-byte output is within SHA-256 HKDF limits");
    key
}

fn nonce(sequence: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&sequence.to_be_bytes());
    *Nonce::from_slice(&bytes)
}

fn associated_data(version: u32, sender_id: &str, recipient_id: &str, sequence: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(
        AAD_DOMAIN.len() + sender_id.len() + recipient_id.len() + std::mem::size_of::<u64>() * 3,
    );
    data.extend_from_slice(AAD_DOMAIN);
    data.extend_from_slice(&version.to_be_bytes());
    for value in [sender_id, recipient_id] {
        data.extend_from_slice(&(value.len() as u64).to_be_bytes());
        data.extend_from_slice(value.as_bytes());
    }
    data.extend_from_slice(&sequence.to_be_bytes());
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> PairedPeer {
        PairedPeer::new(id, PairingSecret::new([7; 32])).unwrap()
    }

    fn manifest() -> PeerMessage {
        PeerMessage::TransferManifest {
            transfer_id: "transfer-1".into(),
            destination_folder_id: "shared".into(),
            idempotency_key: "request-1".into(),
            total_bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        }
    }

    #[test]
    fn paired_peers_exchange_encrypted_versioned_control_deterministically() {
        let (alice_transport, mut bob_transport) = MockPeerTransport::pair();
        let mut alice = PeerConnection::new(
            SecurePeerChannel::new("alice", peer("bob")).unwrap(),
            alice_transport,
        );
        alice.send(&manifest()).unwrap();
        alice.into_transport().deliver_to(&mut bob_transport);

        let mut bob = PeerConnection::new(
            SecurePeerChannel::new("bob", peer("alice")).unwrap(),
            bob_transport,
        );
        assert_eq!(bob.receive().unwrap(), Some(manifest()));
    }

    #[test]
    fn encryption_authenticates_peers_and_rejects_replays() {
        let mut alice = SecurePeerChannel::new("alice", peer("bob")).unwrap();
        let mut bob = SecurePeerChannel::new("bob", peer("alice")).unwrap();
        let frame = alice.seal(&manifest()).unwrap();
        assert!(!String::from_utf8_lossy(&frame.ciphertext).contains("transfer-1"));
        assert_eq!(bob.open(frame.clone()).unwrap(), manifest());
        let hello = PeerMessage::Hello {
            protocol_version: PEER_PROTOCOL_VERSION,
        };
        assert_eq!(alice.open(bob.seal(&hello).unwrap()).unwrap(), hello);
        assert!(matches!(
            bob.open(frame),
            Err(PeerChannelError::UnexpectedSequence { .. })
        ));
    }

    #[test]
    fn rejects_oversized_chunks_before_encryption() {
        let mut channel = SecurePeerChannel::new("alice", peer("bob")).unwrap();
        assert!(matches!(
            channel.seal(&PeerMessage::FileChunk {
                transfer_id: "transfer-1".into(),
                offset: 0,
                bytes: vec![0; CHUNK_SIZE + 1],
            }),
            Err(PeerChannelError::ChunkTooLarge(_))
        ));
    }
}
