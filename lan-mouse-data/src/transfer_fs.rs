//! Receiver filesystem capabilities. Peer paths are resolved one component at
//! a time; subsequent mutations use the held parent, never a canonicalize/check
//! followed by an unprotected path-based operation.
use std::{
    collections::HashMap,
    fs::{File, Metadata},
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(super) struct Directory(Arc<platform::Directory>);

impl Directory {
    fn child(&self, name: &str, create: bool) -> io::Result<(Self, bool)> {
        let created = if create { self.0.mkdir(name)? } else { false };
        Ok((Self(Arc::new(self.0.open_child(name)?)), created))
    }

    pub(super) fn inspect(&self, name: &str) -> io::Result<Option<bool>> {
        match self.0.metadata(name) {
            Ok(meta) => {
                if platform::is_link(&meta) || !(meta.is_file() || meta.is_dir()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "receiver path is a link or special file",
                    ));
                }
                Ok(Some(meta.is_dir()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(super) fn temporary(&self) -> io::Result<(File, Temporary)> {
        loop {
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let name = format!(
                ".lanm-{}-{time:x}-{serial:x}.lanm-partial",
                std::process::id()
            );
            match self.0.create_file(&name) {
                Ok(file) => {
                    let identity = platform::file_identity(&file)?;
                    return Ok((
                        file,
                        Temporary {
                            parent: self.clone(),
                            name,
                            identity,
                            committed: false,
                        },
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

pub(super) struct Temporary {
    parent: Directory,
    name: String,
    identity: platform::Identity,
    committed: bool,
}

impl Temporary {
    pub(super) fn commit(&mut self, name: &str, overwrite: bool) -> io::Result<()> {
        // Refuse links even for explicit Overwrite. rename itself does not
        // dereference the destination, and no-replace installs are atomic.
        self.parent.inspect(name)?;
        if self.parent.0.entry_identity(&self.name)? != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "temporary file was replaced",
            ));
        }
        self.parent.0.commit(&self.name, name, overwrite)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.committed
            && self
                .parent
                .0
                .entry_identity(&self.name)
                .is_ok_and(|identity| identity == self.identity)
        {
            let _ = self.parent.0.remove(&self.name, false);
        }
    }
}

pub(super) struct ReceiveFs {
    root: Directory,
    directories: HashMap<String, Directory>,
    created: Vec<(Directory, String, platform::Identity)>,
    keep_directories: bool,
}

impl ReceiveFs {
    pub(super) fn open(root: &Path, create: bool) -> io::Result<Self> {
        // The configured root is trusted, unlike every peer-supplied component
        // below it. Resolve its OS aliases (e.g. macOS /var) once at this boundary.
        if create {
            std::fs::create_dir_all(root)?;
        }
        let root = std::fs::canonicalize(root)?;
        let root = Directory(Arc::new(platform::Directory::open_root(&root)?));
        Ok(Self {
            root,
            directories: HashMap::new(),
            created: Vec::new(),
            keep_directories: false,
        })
    }

    fn directory(
        &mut self,
        relative: &str,
        create: bool,
        exclusive: bool,
    ) -> io::Result<Directory> {
        let mut parent = self.root.clone();
        let mut prefix = String::new();
        let parts: Vec<_> = relative.split('/').filter(|p| !p.is_empty()).collect();
        for (index, part) in parts.iter().enumerate() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if let Some(dir) = self.directories.get(&prefix) {
                parent = dir.clone();
                continue;
            }
            let (child, created) = parent.child(part, create)?;
            if created {
                self.created
                    .push((parent.clone(), part.to_string(), child.0.own_identity()?));
            } else if exclusive && index + 1 == parts.len() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination directory appeared",
                ));
            }
            self.directories.insert(prefix.clone(), child.clone());
            parent = child;
        }
        Ok(parent)
    }

    pub(super) fn parent(&mut self, path: &str, create: bool) -> io::Result<(Directory, String)> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        Ok((self.directory(parent, create, false)?, name.to_owned()))
    }

    pub(super) fn inspect(&mut self, path: &str) -> io::Result<Option<bool>> {
        match self.parent(path, false) {
            Ok((parent, name)) => parent.inspect(&name),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(super) fn mkdir(&mut self, path: &str, exclusive: bool) -> io::Result<()> {
        self.directory(path, true, exclusive).map(|_| ())
    }

    pub(super) fn keep_directories(&mut self) {
        self.keep_directories = true;
    }
}

impl Drop for ReceiveFs {
    fn drop(&mut self) {
        // Windows forbids deleting open directories. Release child handles
        // before deleting in reverse order, keeping each parent locked.
        self.directories.clear();
        while let Some((parent, name, identity)) = self.created.pop() {
            if !self.keep_directories
                && parent
                    .0
                    .entry_identity(&name)
                    .is_ok_and(|current| current == identity)
            {
                let _ = parent.0.remove(&name, true);
            }
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };

    pub struct Directory {
        file: File,
    }
    pub type Identity = (u64, u64);
    pub fn identity(meta: &Metadata) -> Identity {
        (meta.dev(), meta.ino())
    }
    pub fn file_identity(file: &File) -> io::Result<Identity> {
        Ok(identity(&file.metadata()?))
    }
    pub fn is_link(meta: &Metadata) -> bool {
        meta.file_type().is_symlink()
    }
    fn name(value: &str) -> io::Result<CString> {
        CString::new(value).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
    }
    fn result(value: i32) -> io::Result<()> {
        if value == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn open(parent: i32, name: &CString, flags: i32) -> io::Result<File> {
        // SAFETY: name is terminated, flags include no-follow, and ownership
        // of a successful descriptor is transferred exactly once to File.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }
    impl Directory {
        pub fn open_root(path: &Path) -> io::Result<Self> {
            let path = CString::new(path.as_os_str().as_bytes())?;
            Ok(Self {
                file: open(libc::AT_FDCWD, &path, libc::O_RDONLY | libc::O_DIRECTORY)?,
            })
        }
        pub fn open_child(&self, child: &str) -> io::Result<Self> {
            Ok(Self {
                file: open(
                    self.file.as_raw_fd(),
                    &name(child)?,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                )?,
            })
        }
        pub fn own_identity(&self) -> io::Result<Identity> {
            file_identity(&self.file)
        }
        pub fn entry_identity(&self, child: &str) -> io::Result<Identity> {
            Ok(identity(&self.metadata(child)?))
        }
        pub fn metadata(&self, child: &str) -> io::Result<Metadata> {
            // Open without following links. O_NONBLOCK also avoids hanging
            // on a hostile FIFO before rejecting special file types.
            open(
                self.file.as_raw_fd(),
                &name(child)?,
                libc::O_RDONLY | libc::O_NONBLOCK,
            )?
            .metadata()
        }
        pub fn mkdir(&self, child: &str) -> io::Result<bool> {
            let child = name(child)?;
            // SAFETY: held directory fd and terminated single component.
            match result(unsafe { libc::mkdirat(self.file.as_raw_fd(), child.as_ptr(), 0o700) }) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e),
            }
        }
        pub fn create_file(&self, child: &str) -> io::Result<File> {
            open(
                self.file.as_raw_fd(),
                &name(child)?,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )
        }
        pub fn remove(&self, child: &str, directory: bool) -> io::Result<()> {
            let child = name(child)?;
            // SAFETY: held directory fd, terminated name, valid unlink flags.
            result(unsafe {
                libc::unlinkat(
                    self.file.as_raw_fd(),
                    child.as_ptr(),
                    if directory { libc::AT_REMOVEDIR } else { 0 },
                )
            })
        }
        pub fn commit(&self, source: &str, target: &str, overwrite: bool) -> io::Result<()> {
            let source = name(source)?;
            let target = name(target)?;
            let fd = self.file.as_raw_fd();
            if overwrite {
                // SAFETY: both names are relative to the same held directory.
                result(unsafe { libc::renameat(fd, source.as_ptr(), fd, target.as_ptr()) })
            } else {
                // Atomic no-replace rename also works on filesystems without
                // hard links (e.g. exFAT). Never fall back to check-then-rename.
                #[cfg(target_os = "macos")]
                return result(unsafe {
                    libc::renameatx_np(fd, source.as_ptr(), fd, target.as_ptr(), libc::RENAME_EXCL)
                });
                #[cfg(target_os = "linux")]
                return result(unsafe {
                    libc::renameat2(
                        fd,
                        source.as_ptr(),
                        fd,
                        target.as_ptr(),
                        libc::RENAME_NOREPLACE,
                    )
                });
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                {
                    result(unsafe { libc::linkat(fd, source.as_ptr(), fd, target.as_ptr(), 0) })?;
                    result(unsafe { libc::unlinkat(fd, source.as_ptr(), 0) })
                }
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        fs::OpenOptions,
        os::windows::fs::{MetadataExt, OpenOptionsExt},
        path::{Component, PathBuf},
    };
    const READ_ATTRIBUTES: u32 = 0x80;
    const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
    const OPEN_REPARSE_BACKUP: u32 = 0x00200000 | 0x02000000;
    const REPARSE_POINT: u32 = 0x400;

    pub struct Directory {
        path: PathBuf,
        file: File,
        // Retain all ancestor locks until the last descendant is released.
        _ancestors: Vec<Arc<File>>,
    }
    pub type Identity = (u64, u64);
    pub fn file_identity(file: &File) -> io::Result<Identity> {
        use std::os::windows::io::AsRawHandle;
        // BY_HANDLE_FILE_INFORMATION's DWORD / FILETIME layout. Use the
        // filesystem's file ID, not a timestamp that another file may share.
        #[repr(C)]
        struct FileInformation {
            attributes: u32,
            creation: [u32; 2],
            access: [u32; 2],
            write: [u32; 2],
            volume: u32,
            size_high: u32,
            size_low: u32,
            links: u32,
            index_high: u32,
            index_low: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetFileInformationByHandle(
                handle: *mut std::ffi::c_void,
                info: *mut FileInformation,
            ) -> i32;
        }
        let mut info = std::mem::MaybeUninit::<FileInformation>::uninit();
        // SAFETY: live file handle and correctly sized/aligned output buffer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let info = unsafe { info.assume_init() };
        Ok((
            info.volume as u64,
            ((info.index_high as u64) << 32) | info.index_low as u64,
        ))
    }
    pub fn is_link(meta: &Metadata) -> bool {
        meta.file_attributes() & REPARSE_POINT != 0
    }
    fn lock(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .access_mode(READ_ATTRIBUTES)
            .share_mode(SHARE_READ_WRITE)
            .custom_flags(OPEN_REPARSE_BACKUP)
            .open(path)?;
        let meta = file.metadata()?;
        if is_link(&meta) || !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receiver directory is a reparse point or not a directory",
            ));
        }
        Ok(file)
    }
    impl Directory {
        pub fn open_root(path: &Path) -> io::Result<Self> {
            let mut current = PathBuf::new();
            let mut ancestors = Vec::new();
            for part in path.components() {
                current.push(part);
                if matches!(part, Component::RootDir | Component::Normal(_)) {
                    ancestors.push(Arc::new(lock(&current)?));
                }
            }
            Ok(Self {
                path: path.to_owned(),
                file: lock(path)?,
                _ancestors: ancestors,
            })
        }
        pub fn open_child(&self, child: &str) -> io::Result<Self> {
            let path = self.path.join(child);
            let mut ancestors = self._ancestors.clone();
            ancestors.push(Arc::new(self.file.try_clone()?));
            Ok(Self {
                file: lock(&path)?,
                path,
                _ancestors: ancestors,
            })
        }
        pub fn own_identity(&self) -> io::Result<Identity> {
            file_identity(&self.file)
        }
        pub fn entry_identity(&self, child: &str) -> io::Result<Identity> {
            let file = OpenOptions::new()
                .access_mode(READ_ATTRIBUTES)
                .share_mode(SHARE_READ_WRITE)
                .custom_flags(OPEN_REPARSE_BACKUP)
                .open(self.path.join(child))?;
            if is_link(&file.metadata()?) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "receiver entry is a reparse point",
                ));
            }
            file_identity(&file)
        }
        pub fn metadata(&self, child: &str) -> io::Result<Metadata> {
            std::fs::symlink_metadata(self.path.join(child))
        }
        pub fn mkdir(&self, child: &str) -> io::Result<bool> {
            match std::fs::create_dir(self.path.join(child)) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e),
            }
        }
        pub fn create_file(&self, child: &str) -> io::Result<File> {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(OPEN_REPARSE_BACKUP)
                .share_mode(SHARE_READ_WRITE)
                .open(self.path.join(child))
        }
        pub fn remove(&self, child: &str, directory: bool) -> io::Result<()> {
            if directory {
                std::fs::remove_dir(self.path.join(child))
            } else {
                std::fs::remove_file(self.path.join(child))
            }
        }
        pub fn commit(&self, source: &str, target: &str, overwrite: bool) -> io::Result<()> {
            if overwrite {
                std::fs::rename(self.path.join(source), self.path.join(target))
            } else {
                // MoveFileEx without REPLACE_EXISTING atomically refuses an
                // existing destination. Parent handles deny delete sharing.
                use std::os::windows::ffi::OsStrExt;
                #[link(name = "kernel32")]
                unsafe extern "system" {
                    fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
                }
                let from: Vec<_> = self
                    .path
                    .join(source)
                    .as_os_str()
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                let to: Vec<_> = self
                    .path
                    .join(target)
                    .as_os_str()
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }
    }
}
