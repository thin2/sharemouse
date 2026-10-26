//! Bounded image originals. Paths never travel over the clipboard channel.
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read},
    path::Path,
    sync::Arc,
};

pub const MAX_IMAGE: usize = 16 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 16 * 1024 * 1024;
pub const MAX_DIMENSION: u32 = 16384;
pub const PART_BYTES: usize = 64 * 1024;
pub const MAX_ENCODED: usize = MAX_IMAGE + 260;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
}
impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
        }
    }
    pub fn uti(self) -> &'static str {
        match self {
            Self::Png => "public.png",
            Self::Jpeg => "public.jpeg",
            Self::Gif => "com.compuserve.gif",
        }
    }
    fn tag(self) -> u8 {
        match self {
            Self::Png => 1,
            Self::Jpeg => 2,
            Self::Gif => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub format: Format,
    pub filename: String,
    pub bytes: Arc<Vec<u8>>,
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unsupported or oversized clipboard image",
    )
}
pub fn bounded_dimensions(width: u32, height: u32) -> io::Result<(u32, u32)> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(invalid());
    }
    Ok((width, height))
}

/// Inspect dimensions before handing compressed bytes to an OS decoder.
pub fn inspect(bytes: &[u8]) -> io::Result<(Format, u32, u32)> {
    if bytes.len() > MAX_IMAGE {
        return Err(invalid());
    }
    let (format, w, h) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && bytes.len() >= 33
        && &bytes[12..16] == b"IHDR"
    {
        (
            Format::Png,
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        )
    } else if (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) && bytes.len() >= 13 {
        (
            Format::Gif,
            u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as u32,
            u16::from_le_bytes(bytes[8..10].try_into().unwrap()) as u32,
        )
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        let mut offset = 2;
        let mut dimensions = None;
        while offset + 4 <= bytes.len() {
            if bytes[offset] != 0xff {
                return Err(invalid());
            }
            while bytes.get(offset) == Some(&0xff) {
                offset += 1;
            }
            let marker = *bytes.get(offset).ok_or_else(invalid)?;
            offset += 1;
            if marker == 0xda || marker == 0xd9 {
                break;
            }
            if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                continue;
            }
            let length = bytes.get(offset..offset + 2).ok_or_else(invalid)?;
            let length = u16::from_be_bytes(length.try_into().unwrap()) as usize;
            if length < 2 || offset + length > bytes.len() {
                return Err(invalid());
            }
            if [0xc0, 0xc1, 0xc2].contains(&marker) && length >= 8 {
                dimensions = Some((
                    u16::from_be_bytes(bytes[offset + 5..offset + 7].try_into().unwrap()) as u32,
                    u16::from_be_bytes(bytes[offset + 3..offset + 5].try_into().unwrap()) as u32,
                ));
                break;
            }
            offset += length;
        }
        let (w, h) = dimensions.ok_or_else(invalid)?;
        (Format::Jpeg, w, h)
    } else {
        return Err(invalid());
    };
    bounded_dimensions(w, h)?;
    Ok((format, w, h))
}

impl Image {
    pub fn new(bytes: Vec<u8>, filename: Option<&str>) -> io::Result<Self> {
        let (format, _, _) = inspect(&bytes)?;
        let filename = filename
            .filter(|s| safe_name(s, format))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("clipboard.{}", format.extension()));
        Ok(Self {
            format,
            filename,
            bytes: Arc::new(bytes),
        })
    }
    pub fn valid(&self) -> bool {
        safe_name(&self.filename, self.format)
            && inspect(&self.bytes).is_ok_and(|(f, _, _)| f == self.format)
    }
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update([0x49, self.format.tag()]);
        hash.update((self.filename.len() as u16).to_be_bytes());
        hash.update(self.filename.as_bytes());
        hash.update(self.bytes.as_slice());
        hash.finalize().into()
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.bytes.len() + self.filename.len() + 3);
        bytes.push(self.format.tag());
        bytes.extend_from_slice(&(self.filename.len() as u16).to_be_bytes());
        bytes.extend_from_slice(self.filename.as_bytes());
        bytes.extend_from_slice(&self.bytes);
        bytes
    }
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 3 || bytes.len() > MAX_ENCODED {
            return Err(invalid());
        }
        let size = u16::from_be_bytes(bytes[1..3].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(bytes.get(3..3 + size).ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
        let image = Self::new(
            bytes.get(3 + size..).ok_or_else(invalid)?.to_vec(),
            Some(name),
        )?;
        if image.filename != name || image.format.tag() != bytes[0] {
            return Err(invalid());
        }
        Ok(image)
    }
    pub fn from_file(path: &Path) -> io::Result<Self> {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(invalid());
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(invalid());
            }
        }
        if !metadata.is_file() || metadata.len() > MAX_IMAGE as u64 {
            return Err(invalid());
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_IMAGE as u64 + 1).read_to_end(&mut bytes)?;
        Self::new(bytes, path.file_name().and_then(|s| s.to_str()))
    }
}

fn safe_name(name: &str, format: Format) -> bool {
    !name.is_empty()
        && name.len() <= 240
        && !name.chars().any(char::is_control)
        && !name.contains('/')
        && crate::protocol::validate_relative_path(name).is_ok()
        && Path::new(name)
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| {
                ext.eq_ignore_ascii_case(format.extension())
                    || format == Format::Jpeg && ext.eq_ignore_ascii_case("jpeg")
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gif() -> Vec<u8> {
        // Header inspection is independent of actual frame decoding.
        b"GIF89a\x01\x00\x01\x00\x80\x00\x00".to_vec()
    }
    #[test]
    fn originals_and_portable_filenames_roundtrip_and_are_hashed_together() {
        let image = Image::new(gif(), Some("动图.gif")).unwrap();
        assert_eq!(Image::decode(&image.encode()).unwrap(), image);
        for name in [
            "../x.gif", "CON.gif", "x\\y.gif", "x.png", "x.gif ", "a:b.gif",
        ] {
            assert_eq!(
                Image::new(gif(), Some(name)).unwrap().filename,
                "clipboard.gif"
            );
        }
        assert_ne!(
            image.digest(),
            Image::new(gif(), Some("other.gif")).unwrap().digest()
        );
        let mut encoded = image.encode();
        encoded[0] = 1;
        assert!(Image::decode(&encoded).is_err());
        assert!(Image::decode(&[1, 255, 255]).is_err());
    }
    #[test]
    fn dimension_and_compressed_byte_limits_precede_native_decoder() {
        assert!(bounded_dimensions(0, 1).is_err());
        assert!(bounded_dimensions(MAX_DIMENSION + 1, 1).is_err());
        assert!(bounded_dimensions(5000, 5000).is_err());
        assert!(inspect(&vec![0; MAX_IMAGE + 1]).is_err());
        let mut bytes = gif();
        bytes[6..8].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(Image::new(bytes, None).is_err());
    }
}
