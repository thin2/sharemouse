//! Authenticated reliable transport for files, messages and clipboard data.
//!
//! The existing mouse/keyboard channel is a lossy UDP DTLS datagram
//! stream sized for tiny input events. Files, messages and clipboard
//! payloads need ordered delivery, backpressure and framing, so they
//! use a separate TLS 1.3 TCP connection.
//!
//! Both directions reuse the certificate from the lan-mouse device
//! authorization flow. A peer is accepted only when the SHA-256
//! fingerprint of its leaf certificate is listed in the local
//! `authorized_fingerprints` map, and self-signed certificates are
//! validated with the same fingerprint instead of a public CA chain.

pub mod clipboard;
pub mod clipboard_image;
pub mod clipboard_sync;
pub mod crypto;
pub mod enrollment;
pub mod image_message;
pub mod protocol;
pub mod session;
pub mod sharing;
pub mod transfer;
pub mod transport;

pub use crypto::{
    AuthorizedFingerprints, Identity, IdentityError, PinnedVerifier, certificate_fingerprint,
    load_identity,
};
pub use protocol::{
    ClipboardItem, DataMessage, FileChunk, FilePath, Frame, MAX_CLIPBOARD_IMAGE_BYTES,
    MAX_CLIPBOARD_TEXT_BYTES, MAX_FILES_PER_OFFER, MAX_FRAME_SIZE, MAX_MESSAGE_BYTES, OfferedFile,
    PROTOCOL_VERSION, ProtocolError, SHA256_HEX_LEN, is_sha256_hex,
};
pub use session::{DeviceId, SessionError, SessionEvent, SessionManager};
pub use transfer::{
    ChunkSink, Conflict, ConflictPolicy, FileReader, PreparedFile, TransferError, TransferReceiver,
    detect_conflicts, offered_files, prepare_transfer, send_transfer, send_transfer_with_progress,
    total_bytes,
};
pub use transport::{
    DataConnection, TransportError, accept, client_config, connect, read_frame, server_config,
    write_frame,
};

/// Default TCP port for the authenticated data channel.
///
/// Kept next to the mouse/keyboard UDP port (4242) so both can be
/// described by a single firewall rule pair.
pub const DEFAULT_DATA_PORT: u16 = 4243;
