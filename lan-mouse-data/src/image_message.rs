//! Message image assembly has no access to clipboard versions or native writes.
use crate::clipboard_image::{Image, MAX_ENCODED, PART_BYTES};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub id: String,
    pub digest: [u8; 32],
    pub offset: u32,
    pub total: u32,
    pub bytes: Vec<u8>,
}
impl Part {
    pub fn valid(&self) -> bool {
        !self.id.is_empty()
            && self.id.len() <= 128
            && self.total > 0
            && self.total as usize <= MAX_ENCODED
            && !self.bytes.is_empty()
            && self.bytes.len() <= PART_BYTES
            && self.offset as u64 + self.bytes.len() as u64 <= self.total as u64
    }
}
#[derive(Default)]
pub struct Receiver {
    pending: Option<(String, [u8; 32], u32, Vec<u8>)>,
}
impl Receiver {
    pub fn push(&mut self, part: Part) -> std::io::Result<Option<(String, Image)>> {
        let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidData);
        if !part.valid() {
            self.pending = None;
            return Err(invalid());
        }
        if part.offset == 0 {
            self.pending = Some((part.id.clone(), part.digest, part.total, Vec::new()));
        }
        let Some((id, digest, total, bytes)) = &mut self.pending else {
            return Err(invalid());
        };
        if *id != part.id
            || *digest != part.digest
            || *total != part.total
            || bytes.len() != part.offset as usize
        {
            self.pending = None;
            return Err(invalid());
        }
        bytes.extend(part.bytes);
        if bytes.len() != *total as usize {
            return Ok(None);
        }
        let (id, digest, _, bytes) = self.pending.take().unwrap();
        let image = Image::decode(&bytes)?;
        if image.digest() != digest {
            return Err(invalid());
        }
        Ok(Some((id, image)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Image {
        Image::new(b"GIF89a\x01\0\x01\0\x80\0\0".to_vec(), Some("original.gif")).unwrap()
    }
    fn parts(image: &Image) -> Vec<Part> {
        let bytes = image.encode();
        bytes
            .chunks(7)
            .enumerate()
            .map(|(n, b)| Part {
                id: "message-1".into(),
                digest: image.digest(),
                offset: (n * 7) as u32,
                total: bytes.len() as u32,
                bytes: b.to_vec(),
            })
            .collect()
    }
    #[test]
    fn ordered_parts_keep_original_bytes_and_name() {
        let image = fixture();
        let mut receiver = Receiver::default();
        let mut complete = None;
        for p in parts(&image) {
            complete = receiver.push(p).unwrap();
        }
        assert_eq!(complete, Some(("message-1".into(), image)));
        assert!(receiver.pending.is_none());
    }
    #[test]
    fn reject_reorder_identity_digest_and_size_mismatch() {
        let image = fixture();
        let p = parts(&image);
        let mut receiver = Receiver::default();
        assert!(receiver.push(p[1].clone()).is_err());
        receiver.push(p[0].clone()).unwrap();
        let mut changed = p[1].clone();
        changed.id = "other".into();
        assert!(receiver.push(changed).is_err());
        assert!(receiver.pending.is_none());
        let mut rejected_digest = false;
        for mut part in parts(&image) {
            part.digest = [0; 32];
            let result = receiver.push(part);
            if result.is_err() {
                rejected_digest = true;
                assert!(receiver.pending.is_none());
            }
        }
        assert!(
            rejected_digest,
            "a complete image with the wrong digest must fail"
        );
        let mut bad = p[0].clone();
        bad.total = MAX_ENCODED as u32 + 1;
        assert!(receiver.push(bad).is_err());
        let mut bad = p[0].clone();
        bad.bytes = vec![0; PART_BYTES + 1];
        assert!(receiver.push(bad).is_err());
    }
    #[test]
    fn maximum_text_and_image_part_fit_control_frames() {
        use crate::protocol::{DataMessage, Frame};
        let message = DataMessage::Message {
            id: "id".into(),
            text: "\0".repeat(crate::protocol::MAX_MESSAGE_BYTES),
        };
        assert!(Frame::Control(message).encode().is_ok());
        let message = DataMessage::ImageMessage {
            part: Part {
                id: "id".into(),
                digest: [255; 32],
                offset: 0,
                total: PART_BYTES as u32,
                bytes: vec![255; PART_BYTES],
            },
        };
        assert!(Frame::Control(message).encode().is_ok());
    }
}
