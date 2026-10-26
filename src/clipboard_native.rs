//! Worker reads/preparation and short synchronous final clipboard writes.
//! No clipboard contents are logged or read during application construction.
use lan_mouse_data::clipboard_image::Image;
use std::io;
#[path = "clipboard_native/assets.rs"]
mod assets;
#[path = "clipboard_native/raster.rs"]
mod raster;
pub(crate) use assets::PreparedImage;
#[cfg(test)]
pub(crate) fn test_png() -> Vec<u8> {
    raster::encode(1, 1, &[20, 40, 60, 127]).unwrap()
}

#[cfg(target_os = "macos")]
#[path = "clipboard_native/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "clipboard_native/windows.rs"]
mod platform;
#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub(super) fn read(_: Option<u64>) -> io::Result<RawSnapshot> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub(super) fn write(_: u64, _: &str) -> io::Result<Option<RawSnapshot>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub(super) fn counter() -> io::Result<u64> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub(super) fn prepare_image(_: &Image) -> io::Result<Vec<u8>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub(super) fn write_image(_: u64, _: &PreparedImage) -> io::Result<Option<RawSnapshot>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

pub(crate) struct RawSnapshot {
    raw: u64,
    text: Option<String>,
    image: Option<Image>,
}

pub(crate) struct Snapshot {
    pub serial: u64,
    /// Contents are populated for changed native serials and confirmed writes.
    pub text: Option<String>,
    pub image: Option<Image>,
}

#[derive(Default)]
pub(crate) struct NativeClipboard {
    raw: Option<u64>,
    serial: u64,
}

impl NativeClipboard {
    fn observe(&mut self, snapshot: RawSnapshot) -> io::Result<Snapshot> {
        if self.raw != Some(snapshot.raw) {
            self.serial = self
                .serial
                .checked_add(1)
                .ok_or_else(|| io::Error::other("clipboard serial exhausted"))?;
            self.raw = Some(snapshot.raw);
        }
        Ok(Snapshot {
            serial: self.serial,
            text: snapshot.text,
            image: snapshot.image,
        })
    }
    #[cfg(test)]
    pub fn read(&mut self) -> io::Result<Snapshot> {
        self.observe(platform::read(self.raw)?)
    }
    pub fn raw(&self) -> Option<u64> {
        self.raw
    }
    pub fn thumbnail(image: &Image) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let preview = platform::prepare_image(image)?;
        let thumbnail = raster::thumbnail(&preview)?;
        Ok((preview, thumbnail))
    }
    pub fn collect_assets(directory: &std::path::Path) -> io::Result<()> {
        let snapshot = platform::read(None)?;
        // Unknown/file-list representations may still refer to our files.
        // Leave them alone rather than infer that an unsupported copy is empty.
        if snapshot.text.is_none() && snapshot.image.is_none() {
            return Ok(());
        }
        let keep = snapshot.image.as_ref().map(Image::digest);
        assets::collect(directory, keep, std::time::SystemTime::now(), || {
            platform::counter().is_ok_and(|n| n == snapshot.raw)
        })
    }
    pub fn poll_owner(&self) {
        let _ = platform::counter();
    }
    pub fn read_raw(previous: Option<u64>) -> io::Result<RawSnapshot> {
        platform::read(previous)
    }
    pub fn finish_read(&mut self, snapshot: RawSnapshot) -> io::Result<Option<Snapshot>> {
        if platform::counter()? != snapshot.raw {
            return Ok(None);
        }
        self.observe(snapshot).map(Some)
    }
    pub fn is_current(&self, serial: u64) -> io::Result<bool> {
        Ok(self.serial == serial && self.raw == Some(platform::counter()?))
    }
    pub fn prepare_image(image: Image, directory: &std::path::Path) -> io::Result<PreparedImage> {
        if !image.valid() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let preview = platform::prepare_image(&image)?;
        assets::prepare(image, preview, directory)
    }
    pub fn write_image(
        &mut self,
        serial: u64,
        prepared: &PreparedImage,
    ) -> io::Result<Option<Snapshot>> {
        if serial != self.serial {
            return Ok(None);
        }
        let Some(raw) = self.raw else {
            return Ok(None);
        };
        platform::write_image(raw, prepared)?
            .map(|s| self.observe(s))
            .transpose()
    }
    /// Windows checks under OpenClipboard; macOS checks changeCount immediately
    /// before the synchronous write (NSPasteboard has no atomic compare-and-set).
    pub fn write(&mut self, expected_serial: u64, text: &str) -> io::Result<Option<Snapshot>> {
        if text.len() > lan_mouse_data::clipboard_sync::MAX_TEXT {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if expected_serial != self.serial {
            return Ok(None);
        }
        let Some(raw) = self.raw else {
            return Ok(None);
        };
        match platform::write(raw, text)? {
            Some(snapshot) => self.observe(snapshot).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_counter_wrap_still_produces_increasing_serials() {
        let mut clipboard = NativeClipboard::default();
        let first = clipboard
            .observe(RawSnapshot {
                raw: u32::MAX as u64,
                text: None,
                image: None,
            })
            .unwrap();
        let next = clipboard
            .observe(RawSnapshot {
                raw: 0,
                text: None,
                image: None,
            })
            .unwrap();
        let duplicate = clipboard
            .observe(RawSnapshot {
                raw: 0,
                text: None,
                image: None,
            })
            .unwrap();
        assert!(next.serial > first.serial);
        assert_eq!(next.serial, duplicate.serial);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires LAN_MOUSE_TEST_PASTEBOARD=sharemouse-test-<unique name>"]
    fn named_native_clipboard_roundtrip_and_stale_write() {
        assert!(
            std::env::var("LAN_MOUSE_TEST_PASTEBOARD")
                .unwrap()
                .starts_with("sharemouse-test-")
        );
        let mut clipboard = NativeClipboard::default();
        let initial = clipboard.read().unwrap();
        let text = "中文 🦀\nline two\0embedded";
        let written = clipboard.write(initial.serial, text).unwrap().unwrap();
        assert_eq!(written.text.as_deref(), Some(text));
        assert_eq!(clipboard.read().unwrap().serial, written.serial);
        // Another adapter stands for an independent native clipboard writer.
        let mut external = NativeClipboard::default();
        let baseline = external.read().unwrap();
        external
            .write(baseline.serial, "new local copy")
            .unwrap()
            .unwrap();
        assert!(
            clipboard
                .write(written.serial, "stale remote copy")
                .unwrap()
                .is_none()
        );
        let current = clipboard.read().unwrap();
        assert_eq!(current.text.as_deref(), Some("new local copy"));
        assert!(current.serial > written.serial);
        // The same bytes explicitly copied again have a new native serial.
        let duplicate = clipboard
            .write(current.serial, "new local copy")
            .unwrap()
            .unwrap();
        assert!(duplicate.serial > current.serial);
        assert!(
            clipboard
                .write(current.serial, "old serial")
                .unwrap()
                .is_none()
        );
        let empty = clipboard.write(duplicate.serial, "").unwrap().unwrap();
        assert_eq!(empty.text.as_deref(), Some(""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires LAN_MOUSE_TEST_PASTEBOARD=sharemouse-test-<unique name>"]
    fn named_native_image_dual_representation_and_alpha() {
        assert!(
            std::env::var("LAN_MOUSE_TEST_PASTEBOARD")
                .unwrap()
                .starts_with("sharemouse-test-")
        );
        let mut clipboard = NativeClipboard::default();
        let initial = clipboard.read().unwrap();
        let rgba = [0, 0, 0, 0, 20, 100, 200, 127, 200, 30, 50, 255];
        let bytes = raster::encode(3, 1, &rgba).unwrap();
        let image = Image::new(bytes, Some("透明.png")).unwrap();
        let directory =
            std::env::temp_dir().join(format!("sharemouse-native-image-{}", std::process::id()));
        let prepared = NativeClipboard::prepare_image(image.clone(), &directory).unwrap();
        let decoded = raster::decode(&prepared.preview).unwrap();
        assert_eq!((decoded.0, decoded.1), (3, 1));
        // ImageIO premultiplies while decoding, so allow one rounding unit for
        // partially transparent channels; alpha itself must remain exact.
        for (a, b) in decoded.2.iter().zip(rgba) {
            assert!(a.abs_diff(b) <= 1);
        }
        let written = clipboard
            .write_image(initial.serial, &prepared)
            .unwrap()
            .unwrap();
        let raw = NativeClipboard::read_raw(None).unwrap();
        assert_eq!(raw.image, Some(image));
        assert_eq!(
            Image::from_file(&prepared.path).unwrap().bytes,
            prepared.image.bytes
        );
        let mut external = NativeClipboard::default();
        let baseline = external.read().unwrap();
        external
            .write(baseline.serial, "new copy")
            .unwrap()
            .unwrap();
        assert!(
            clipboard
                .write_image(written.serial, &prepared)
                .unwrap()
                .is_none()
        );
        let observed = clipboard.read().unwrap();
        assert_eq!(observed.text.as_deref(), Some("new copy"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
