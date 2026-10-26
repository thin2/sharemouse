//! Wire format for the authenticated data channel.
//!
//! A frame is a 32-bit big-endian length followed by a one-byte tag and
//! the payload. Control messages are JSON; file chunks carry raw bytes
//! so large transfers do not pay a base64 expansion.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current wire version. Peers with another version are rejected
/// during the `Hello` exchange instead of failing mid-transfer.
pub const PROTOCOL_VERSION: u16 = 7;

/// Largest accepted control frame. Messages and clipboard text are far
/// below this; the cap stops a peer from exhausting memory.
pub const MAX_CONTROL_FRAME: usize = 1024 * 1024;

/// Largest accepted chunk payload (excluding header).
pub const MAX_CHUNK_DATA: usize = 256 * 1024;

/// Largest frame accepted by the reader, including the tag byte.
pub const MAX_FRAME_SIZE: usize = MAX_CONTROL_FRAME + 1;

/// Maximum UTF-8 length of a text message.
pub const MAX_MESSAGE_BYTES: usize = 128 * 1024;

/// Maximum UTF-8 length of a relative path.
pub const MAX_PATH_BYTES: usize = 4096;

/// Maximum number of entries in one file offer.
pub const MAX_FILES_PER_OFFER: usize = 4096;

const TAG_CONTROL: u8 = 0;
const TAG_CHUNK: u8 = 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    FrameTooLarge { len: usize, max: usize },
    #[error("unknown frame tag {0}")]
    UnknownFrameTag(u8),
    #[error("truncated frame")]
    Truncated,
    #[error("invalid json payload: {0}")]
    Json(String),
    #[error("unsupported protocol version {got}, expected {expected}")]
    UnsupportedVersion { got: u16, expected: u16 },
    #[error("invalid relative path `{path}`: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("text message is {0} bytes, over the {MAX_MESSAGE_BYTES} byte limit")]
    MessageTooLong(usize),
    #[error("file offer contains {0} entries, over the {MAX_FILES_PER_OFFER} limit")]
    TooManyFiles(usize),
    #[error("file `{0}` has a missing or malformed sha256 digest")]
    InvalidDigest(String),
}

/// A file or directory entry in a transfer offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePath {
    /// Slash-separated path relative to the transfer root.
    pub path: String,
    /// Size in bytes; zero for directories.
    pub size: u64,
    pub is_dir: bool,
}

impl FilePath {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_relative_path(&self.path)
    }
}

/// One entry in a transfer offer: a relative path plus the digest the
/// receiver must reproduce before it may commit the file.
///
/// The digest travels in the offer rather than in the chunk stream so a
/// receiver can reject a corrupted or truncated file without keeping a
/// second copy of the payload around.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedFile {
    /// Slash-separated path relative to the transfer root.
    pub path: String,
    /// Size in bytes; zero for directories.
    pub size: u64,
    pub is_dir: bool,
    /// Lowercase hex SHA-256 of the file content; `None` for directories.
    pub sha256: Option<String>,
}

impl OfferedFile {
    pub fn from_path(path: FilePath, sha256: Option<String>) -> Self {
        Self {
            path: path.path,
            size: path.size,
            is_dir: path.is_dir,
            sha256,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_relative_path(&self.path)?;
        if self.is_dir {
            return Ok(());
        }
        match &self.sha256 {
            Some(digest) if is_sha256_hex(digest) => Ok(()),
            Some(_) => Err(ProtocolError::InvalidDigest(self.path.clone())),
            None => Err(ProtocolError::InvalidDigest(self.path.clone())),
        }
    }
}

/// Length of a hex encoded SHA-256 digest.
pub const SHA256_HEX_LEN: usize = 64;

/// Accept only lowercase hex of the exact digest length, so a peer
/// cannot smuggle a different encoding past the comparison.
pub fn is_sha256_hex(digest: &str) -> bool {
    digest.len() == SHA256_HEX_LEN
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A control-plane message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataMessage {
    Group {
        group: crate::enrollment::Group,
    },
    /// First message on every connection; rejects incompatible peers.
    Hello {
        version: u16,
    },
    /// Input session coordination, separate from file and clipboard bodies.
    Sharing {
        message: crate::sharing::Message,
    },
    Clipboard {
        message: crate::clipboard_sync::Message,
    },
    /// Text or link shown in the recipient's message record.
    Message {
        id: String,
        text: String,
    },
    ImageMessage {
        part: crate::image_message::Part,
    },
    /// Announce a file or folder transfer before sending chunks.
    FileOffer {
        transfer_id: String,
        files: Vec<OfferedFile>,
    },
    /// All chunks were sent and the receiver may commit the transfer.
    FileComplete {
        transfer_id: String,
    },
    /// The receiver created its state and is ready for chunks.
    FileAccepted {
        transfer_id: String,
    },
    /// The receiver verified and committed every received file.
    FileCommitted {
        transfer_id: String,
    },
    /// Receiver rejected this outgoing transfer (distinct from sender cancellation).
    FileRejected {
        transfer_id: String,
        reason: String,
    },
    /// Sender abandoned the transfer; the receiver removes temp files.
    FileCancel {
        transfer_id: String,
        reason: Option<String>,
    },
    /// Apply this clipboard version on the receiving device.
    ///
    /// `source` is the device that originally copied the content, so
    /// the receiver can ignore its own echo. `version` orders copies
    /// without relying on wall-clock time.
    ClipboardOffer {
        version: u64,
        source: String,
        item: ClipboardItem,
    },
    /// Sender has no content for this version (for example the source
    /// device disconnected); the receiver should show unavailable.
    ClipboardUnavailable {
        version: u64,
        reason: String,
    },
}

/// Clipboard payload kinds supported by the first release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardItem {
    Text {
        text: String,
    },
    Image {
        /// Original file bytes (PNG, JPEG, GIF, ...) and a suggested name.
        filename: String,
        bytes: Vec<u8>,
    },
}

impl DataMessage {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Group { group } => {
                if !group.valid() {
                    return Err(ProtocolError::Json("invalid connection group".into()));
                }
            }
            Self::ImageMessage { part } => {
                if !part.valid() {
                    return Err(ProtocolError::Json("invalid image message".into()));
                }
            }
            Self::Clipboard { message } if !message.validate() => {
                return Err(ProtocolError::Json("invalid clipboard message".into()));
            }
            Self::Hello { version } => {
                if *version != PROTOCOL_VERSION {
                    return Err(ProtocolError::UnsupportedVersion {
                        got: *version,
                        expected: PROTOCOL_VERSION,
                    });
                }
            }
            Self::Message { text, .. } => {
                if text.len() > MAX_MESSAGE_BYTES {
                    return Err(ProtocolError::MessageTooLong(text.len()));
                }
            }
            Self::FileOffer { files, .. } => {
                if files.len() > MAX_FILES_PER_OFFER {
                    return Err(ProtocolError::TooManyFiles(files.len()));
                }
                for file in files {
                    file.validate()?;
                }
            }
            Self::ClipboardOffer { item, .. } => match item {
                ClipboardItem::Text { text } => {
                    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                        return Err(ProtocolError::MessageTooLong(text.len()));
                    }
                }
                ClipboardItem::Image { filename, bytes } => {
                    validate_relative_path(filename)?;
                    if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
                        return Err(ProtocolError::FrameTooLarge {
                            len: bytes.len(),
                            max: MAX_CLIPBOARD_IMAGE_BYTES,
                        });
                    }
                }
            },
            Self::FileComplete { .. }
            | Self::FileAccepted { .. }
            | Self::FileCommitted { .. }
            | Self::FileRejected { .. }
            | Self::Sharing { .. }
            | Self::Clipboard { .. }
            | Self::FileCancel { .. }
            | Self::ClipboardUnavailable { .. } => {}
        }
        Ok(())
    }
}

/// Maximum text size carried by one clipboard version.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 256 * 1024;

/// Maximum encoded image size carried by one clipboard version.
///
/// Larger images are sent as files instead.
pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// A slice of one file's content at an absolute offset.
///
/// `path` identifies which file of the transfer the bytes belong to, so
/// a receiver can interleave chunks from several files without keeping
/// a per-file cursor on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunk {
    pub transfer_id: String,
    pub path: String,
    pub offset: u64,
    pub data: Vec<u8>,
}

/// Decoded frame from the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Control(DataMessage),
    Chunk(FileChunk),
}

impl Frame {
    /// Encode without the length prefix; [`crate::transport::write_frame`]
    /// adds that.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        match self {
            Self::Control(message) => {
                message.validate()?;
                let json =
                    serde_json::to_vec(message).map_err(|e| ProtocolError::Json(e.to_string()))?;
                if json.len() > MAX_CONTROL_FRAME {
                    return Err(ProtocolError::FrameTooLarge {
                        len: json.len(),
                        max: MAX_CONTROL_FRAME,
                    });
                }
                let mut out = Vec::with_capacity(json.len() + 1);
                out.push(TAG_CONTROL);
                out.extend_from_slice(&json);
                Ok(out)
            }
            Self::Chunk(chunk) => {
                if chunk.data.len() > MAX_CHUNK_DATA {
                    return Err(ProtocolError::FrameTooLarge {
                        len: chunk.data.len(),
                        max: MAX_CHUNK_DATA,
                    });
                }
                if chunk.transfer_id.len() > u16::MAX as usize {
                    return Err(ProtocolError::InvalidPath {
                        path: chunk.transfer_id.clone(),
                        reason: "transfer id is too long",
                    });
                }
                if chunk.path.len() > u16::MAX as usize {
                    return Err(ProtocolError::InvalidPath {
                        path: chunk.path.clone(),
                        reason: "path is too long",
                    });
                }
                // The path is validated on decode as well; doing it on
                // encode keeps a malformed path from being sent at all.
                validate_relative_path(&chunk.path)?;
                let mut out = Vec::with_capacity(
                    1 + 2 + chunk.transfer_id.len() + 2 + chunk.path.len() + 8 + chunk.data.len(),
                );
                out.push(TAG_CHUNK);
                out.extend_from_slice(&(chunk.transfer_id.len() as u16).to_be_bytes());
                out.extend_from_slice(chunk.transfer_id.as_bytes());
                out.extend_from_slice(&(chunk.path.len() as u16).to_be_bytes());
                out.extend_from_slice(chunk.path.as_bytes());
                out.extend_from_slice(&chunk.offset.to_be_bytes());
                out.extend_from_slice(&chunk.data);
                Ok(out)
            }
        }
    }

    /// Decode a frame payload (the bytes after the length prefix).
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let (tag, body) = payload.split_first().ok_or(ProtocolError::Truncated)?;
        match *tag {
            TAG_CONTROL => {
                if body.len() > MAX_CONTROL_FRAME {
                    return Err(ProtocolError::FrameTooLarge {
                        len: body.len(),
                        max: MAX_CONTROL_FRAME,
                    });
                }
                let message: DataMessage =
                    serde_json::from_slice(body).map_err(|e| ProtocolError::Json(e.to_string()))?;
                message.validate()?;
                Ok(Self::Control(message))
            }
            TAG_CHUNK => {
                if body.len() < 2 {
                    return Err(ProtocolError::Truncated);
                }
                let id_len = u16::from_be_bytes([body[0], body[1]]) as usize;
                let rest = &body[2..];
                if rest.len() < id_len + 2 {
                    return Err(ProtocolError::Truncated);
                }
                let transfer_id = std::str::from_utf8(&rest[..id_len])
                    .map_err(|_| ProtocolError::Truncated)?
                    .to_owned();
                let rest = &rest[id_len..];
                let path_len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                let rest = &rest[2..];
                if rest.len() < path_len + 8 {
                    return Err(ProtocolError::Truncated);
                }
                let path = std::str::from_utf8(&rest[..path_len])
                    .map_err(|_| ProtocolError::Truncated)?
                    .to_owned();
                validate_relative_path(&path)?;
                let offset = u64::from_be_bytes(
                    rest[path_len..path_len + 8]
                        .try_into()
                        .map_err(|_| ProtocolError::Truncated)?,
                );
                let data = rest[path_len + 8..].to_vec();
                if data.len() > MAX_CHUNK_DATA {
                    return Err(ProtocolError::FrameTooLarge {
                        len: data.len(),
                        max: MAX_CHUNK_DATA,
                    });
                }
                Ok(Self::Chunk(FileChunk {
                    transfer_id,
                    path,
                    offset,
                    data,
                }))
            }
            other => Err(ProtocolError::UnknownFrameTag(other)),
        }
    }
}

/// Reject paths that could escape the receive directory or that cannot
/// be represented on Windows and macOS alike.
pub fn validate_relative_path(path: &str) -> Result<(), ProtocolError> {
    let invalid = |reason: &'static str| ProtocolError::InvalidPath {
        path: path.to_owned(),
        reason,
    };

    if path.is_empty() {
        return Err(invalid("path is empty"));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(invalid("path is too long"));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(invalid("absolute paths are not allowed"));
    }
    if path.contains('\0') {
        return Err(invalid("path contains a NUL byte"));
    }
    if path.contains('\\') {
        return Err(invalid("use '/' as the path separator"));
    }

    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(invalid("empty path segment"));
        }
        if segment == "." || segment == ".." {
            return Err(invalid("path traversal is not allowed"));
        }
        if segment.len() > 255 {
            return Err(invalid("path segment is too long"));
        }
        for ch in segment.chars() {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                return Err(invalid("path contains a character unsupported on Windows"));
            }
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(invalid("path segment ends with a dot or space"));
        }
        if is_windows_reserved_name(segment) {
            return Err(invalid("path uses a reserved Windows device name"));
        }
    }
    Ok(())
}

fn is_windows_reserved_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: Frame) {
        let encoded = frame.encode().unwrap();
        let decoded = Frame::decode(&encoded).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn control_frame_roundtrip() {
        roundtrip(Frame::Control(DataMessage::Hello {
            version: PROTOCOL_VERSION,
        }));
        roundtrip(Frame::Control(DataMessage::Message {
            id: "m-1".into(),
            text: "hello".into(),
        }));
        roundtrip(Frame::Control(DataMessage::FileOffer {
            transfer_id: "t-1".into(),
            files: vec![
                OfferedFile {
                    path: "docs".into(),
                    size: 0,
                    is_dir: true,
                    sha256: None,
                },
                OfferedFile {
                    path: "docs/report.pdf".into(),
                    size: 42,
                    is_dir: false,
                    sha256: Some("1".repeat(64)),
                },
            ],
        }));
    }

    #[test]
    fn chunk_frame_roundtrip() {
        roundtrip(Frame::Chunk(FileChunk {
            transfer_id: "t-1".into(),
            path: "docs/report.pdf".into(),
            offset: 1234,
            data: vec![0, 1, 2, 255],
        }));
    }

    #[test]
    fn unknown_version_is_rejected() {
        let frame = Frame::Control(DataMessage::Hello { version: 99 });
        assert!(matches!(
            frame.encode(),
            Err(ProtocolError::UnsupportedVersion { got: 99, .. })
        ));
    }

    #[test]
    fn malformed_json_is_rejected() {
        let mut payload = vec![TAG_CONTROL];
        payload.extend_from_slice(b"{not json");
        assert!(matches!(
            Frame::decode(&payload),
            Err(ProtocolError::Json(_))
        ));
    }

    #[test]
    fn unknown_tag_is_rejected() {
        assert_eq!(
            Frame::decode(&[7, 1, 2, 3]),
            Err(ProtocolError::UnknownFrameTag(7))
        );
    }

    #[test]
    fn truncated_chunk_is_rejected() {
        assert_eq!(
            Frame::decode(&[TAG_CHUNK, 0, 5]),
            Err(ProtocolError::Truncated)
        );
    }

    #[test]
    fn oversized_chunk_is_rejected() {
        let chunk = FileChunk {
            transfer_id: "t".into(),
            path: "a.txt".into(),
            offset: 0,
            data: vec![0; MAX_CHUNK_DATA + 1],
        };
        assert!(matches!(
            Frame::Chunk(chunk).encode(),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        for path in [
            "",
            "/etc/passwd",
            "C:/Windows/system32",
            "../secret",
            "docs/../../secret",
            "docs//file",
            "docs\\file",
            "docs/CON.txt",
            "docs/trailing.",
            "docs/name:stream",
            "docs/question?",
        ] {
            assert!(
                validate_relative_path(path).is_err(),
                "{path} should have been rejected"
            );
        }
    }

    #[test]
    fn ordinary_paths_are_accepted() {
        for path in [
            "file.txt",
            "docs/report.pdf",
            "图片/截图 2026.png",
            ".hidden",
        ] {
            assert!(
                validate_relative_path(path).is_ok(),
                "{path} should be valid"
            );
        }
    }

    #[test]
    fn too_many_files_are_rejected() {
        let message = DataMessage::FileOffer {
            transfer_id: "t".into(),
            files: vec![
                OfferedFile {
                    path: "a".into(),
                    size: 0,
                    is_dir: false,
                    sha256: Some("0".repeat(64)),
                };
                MAX_FILES_PER_OFFER + 1
            ],
        };
        assert!(matches!(
            message.validate(),
            Err(ProtocolError::TooManyFiles(_))
        ));
    }
}
