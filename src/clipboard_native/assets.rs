//! Clipboard file references outlive a process and are never truncated in place.
use super::*;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
static PREPARING: Mutex<()> = Mutex::new(());
static PINNED: std::sync::LazyLock<Mutex<std::collections::HashMap<PathBuf, usize>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
struct Pin(PathBuf);
impl Pin {
    fn new(path: &Path) -> Self {
        *PINNED
            .lock()
            .expect("asset pins")
            .entry(path.to_owned())
            .or_default() += 1;
        Self(path.to_owned())
    }
}
impl Drop for Pin {
    fn drop(&mut self) {
        let mut pins = PINNED.lock().expect("asset pins");
        if let Some(count) = pins.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&self.0);
            }
        }
    }
}
const QUOTA: u64 = 512 * 1024 * 1024;
const MAX_ASSETS: usize = 512;
pub(super) fn collect(
    root: &Path,
    keep: Option<[u8; 32]>,
    now: SystemTime,
    mut unchanged: impl FnMut() -> bool,
) -> io::Result<()> {
    let _lock = PREPARING
        .lock()
        .map_err(|_| io::Error::other("image cache lock poisoned"))?;
    if !root.exists() {
        return Ok(());
    }
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let keep = keep.map(|d| d.iter().map(|b| format!("{b:02x}")).collect::<String>());
    for e in fs::read_dir(root)? {
        let e = e?;
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !lan_mouse_data::protocol::is_sha256_hex(name) || keep.as_deref() == Some(name) {
            continue;
        }
        let m = fs::symlink_metadata(e.path())?;
        if !m.is_dir() || m.file_type().is_symlink() {
            continue;
        }
        for file in fs::read_dir(e.path())? {
            let file = file?;
            let m = fs::symlink_metadata(file.path())?;
            if !m.is_file() || m.file_type().is_symlink() {
                continue;
            }
            if PINNED
                .lock()
                .expect("asset pins")
                .contains_key(&file.path())
            {
                continue;
            }
            if now.duration_since(m.modified()?).unwrap_or_default()
                < std::time::Duration::from_secs(86400)
            {
                continue;
            }
            if !unchanged() {
                return Ok(());
            }
            fs::remove_file(file.path())?;
        }
        let _ = fs::remove_dir(e.path()); // only succeeds for an empty asset directory
    }
    Ok(())
}
pub(crate) struct PreparedImage {
    _pin: Pin,
    pub image: Image,
    pub preview: Vec<u8>,
    pub path: PathBuf,
    #[cfg(windows)]
    pub dib: Vec<u8>,
}
pub(super) fn prepare(image: Image, preview: Vec<u8>, root: &Path) -> io::Result<PreparedImage> {
    #[cfg(windows)]
    let dib = super::raster::to_dib(&preview)?;
    let _lock = PREPARING
        .lock()
        .map_err(|_| io::Error::other("image cache lock poisoned"))?;
    fs::create_dir_all(root)?;
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    // Each asset has its own immutable directory, keeping the original basename.
    let digest: String = image.digest().iter().map(|b| format!("{b:02x}")).collect();
    let directory = root.join(&digest);
    // Native file references outlive a process. Cleanup protects the current
    // clipboard, prepared writes and a one-day grace period for other apps.
    let mut total = 0_u64;
    let mut assets = 0_usize;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        for asset in fs::read_dir(entry.path())? {
            let asset = asset?;
            let meta = fs::symlink_metadata(asset.path())?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            total = total.saturating_add(meta.len());
            assets = assets.saturating_add(1);
        }
    }
    let path = directory.join(&image.filename);
    if !path.exists()
        && (assets >= MAX_ASSETS || total.saturating_add(image.bytes.len() as u64) > QUOTA)
    {
        return Err(io::Error::other("clipboard image cache quota exceeded"));
    }
    fs::create_dir_all(&directory)?;
    if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    if path.exists() {
        if Image::from_file(&path)?.digest() != image.digest() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        touch_asset(&path)?;
        return Ok(PreparedImage {
            _pin: Pin::new(&path),
            image,
            preview,
            path,
            #[cfg(windows)]
            dib,
        });
    }
    // Publish only complete files, without replacing an existing immutable
    // asset. A failed write leaves no partial file at the advertised path.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary = directory.join(format!(".pending-{}-{nonce}", std::process::id()));
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let _cleanup = Cleanup(temporary.clone());
    file.write_all(&image.bytes)?;
    file.sync_all()?;
    drop(file);
    match fs::hard_link(&temporary, &path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if Image::from_file(&path)?.digest() != image.digest() {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        Err(error) => return Err(error),
    }
    Ok(PreparedImage {
        _pin: Pin::new(&path),
        image,
        preview,
        path,
        #[cfg(windows)]
        dib,
    })
}

// Windows requires FILE_WRITE_ATTRIBUTES; read-only handles cannot set file times.
fn touch_asset(path: &Path) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.access_mode(0x0100); // FILE_WRITE_ATTRIBUTES, without content write access
    }
    options.open(path)?.set_modified(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_protects_current_prepared_recent_and_changed_clipboard() {
        let root = std::env::temp_dir().join(format!(
            "sm-gc-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let png = super::super::test_png();
        let current = prepare(
            Image::new(png.clone(), Some("current.png")).unwrap(),
            png.clone(),
            &root,
        )
        .unwrap();
        let kept = current.image.digest();
        let current_path = current.path.clone();
        drop(current);
        let pinned = prepare(
            Image::new(png.clone(), Some("prepared.png")).unwrap(),
            png.clone(),
            &root,
        )
        .unwrap();
        let old = prepare(
            Image::new(png.clone(), Some("old.png")).unwrap(),
            png,
            &root,
        )
        .unwrap();
        let old_path = old.path.clone();
        drop(old);
        collect(&root, None, SystemTime::now(), || true).unwrap();
        assert!(old_path.exists(), "recent file has a grace period");
        let future = SystemTime::now() + std::time::Duration::from_secs(86401);
        collect(&root, Some(kept), future, || false).unwrap();
        assert!(old_path.exists(), "native serial changed");
        collect(&root, Some(kept), future, || true).unwrap();
        assert!(current_path.exists() && pinned.path.exists());
        assert!(!old_path.exists());
        let pinned_path = pinned.path.clone();
        drop(pinned);
        collect(&root, Some(kept), future, || true).unwrap();
        assert!(!pinned_path.exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn assets_are_immutable_reusable_and_survive_adapter_drop() {
        let root = std::env::temp_dir().join(format!(
            "sharemouse-image-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let png = super::super::raster::encode(1, 1, &[10, 20, 30, 127]).unwrap();
        let image = Image::new(png.clone(), Some("测试.png")).unwrap();
        let first = prepare(image.clone(), png.clone(), &root).unwrap();
        let path = first.path.clone();
        drop(first);
        let second = prepare(image.clone(), png.clone(), &root).unwrap();
        assert_eq!(path, second.path);
        assert_eq!(Image::from_file(&path).unwrap(), image);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        // A sparse fixture exercises the quota without writing 512 MiB.
        let other = root.join("a".repeat(64));
        fs::create_dir(&other).unwrap();
        let quota_file = fs::File::create(other.join("quota")).unwrap();
        quota_file.set_len(QUOTA).unwrap();
        assert!(prepare(image.clone(), png.clone(), &root).is_ok());
        let new_image = Image::new(png.clone(), Some("other.png")).unwrap();
        assert!(prepare(new_image, png.clone(), &root).is_err());
        assert_eq!(fs::read(&path).unwrap(), png);
        fs::write(&path, b"broken").unwrap();
        assert!(prepare(image, png, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
