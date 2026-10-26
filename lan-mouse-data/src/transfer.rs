//! Streaming file and folder transfer.
//!
//! Files never sit in memory whole: the sender reads fixed-size chunks
//! from disk and hands them to the session write queue, and the
//! receiver appends each chunk to a temporary file while updating a
//! running SHA-256. A file is only committed after its digest matches
//! the one announced in the offer.
//!
//! Paths are validated by [`crate::protocol::validate_relative_path`]
//! before they touch the filesystem, so a peer cannot escape the
//! receive directory.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

#[path = "transfer_fs.rs"]
mod filesystem;
use filesystem::{ReceiveFs, Temporary};

use crate::protocol::{
    MAX_CHUNK_DATA, MAX_FILES_PER_OFFER, OfferedFile, ProtocolError, is_sha256_hex,
    validate_relative_path,
};

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("path `{0}` is not a regular file or directory")]
    UnsupportedEntry(String),
    #[error("offer contains {0} entries, over the {MAX_FILES_PER_OFFER} limit")]
    TooManyEntries(usize),
    #[error("transfer `{0}` is not in progress")]
    UnknownTransfer(String),
    #[error("transfer `{0}` is already in progress")]
    DuplicateTransfer(String),
    #[error("chunk for unknown file `{0}`")]
    UnknownFile(String),
    #[error("chunk for `{path}` starts at {offset}, expected {expected}")]
    UnexpectedOffset {
        path: String,
        offset: u64,
        expected: u64,
    },
    #[error("chunk for `{path}` would exceed the announced size of {size} bytes")]
    ChunkOverflows { path: String, size: u64 },
    #[error("sha256 mismatch for `{path}`: expected {expected}, got {actual}")]
    DigestMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("`{path}` is {actual} bytes, but the offer announced {expected}")]
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("receive directory escapes the transfer root: `{0}`")]
    PathEscapesRoot(String),
    #[error("chunk sink failed: {0}")]
    Sink(String),
    #[error("{} destination(s) already exist; choose overwrite, skip or rename", .0.len())]
    Conflicts(Vec<Conflict>),
    #[error("transfer needs {needed} bytes but only {available} are free in `{root}`")]
    NotEnoughSpace {
        root: String,
        needed: u64,
        available: u64,
    },
}

/// Free space available for `dir`, in bytes, when the platform can
/// report it. `None` means "unknown" rather than "zero", so a missing
/// answer never blocks a transfer.
pub fn available_space(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: `statvfs` only writes into the buffer we hand it and
        // the path is a valid NUL-terminated C string.
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
            return None;
        }
        let block = stats.f_frsize.max(1) as u64;
        Some(stats.f_bavail as u64 * block)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Compare a transfer's announced size with the free space the
/// platform reported.
///
/// `available` is `None` when the platform cannot tell us, which must
/// never block receiving; only a known, too-small figure refuses.
fn check_space(root: &Path, needed: u64, available: Option<u64>) -> Result<(), TransferError> {
    match available {
        Some(available) if needed > available => Err(TransferError::NotEnoughSpace {
            root: root.display().to_string(),
            needed,
            available,
        }),
        _ => Ok(()),
    }
}

/// How to treat an incoming entry whose target already exists.
///
/// The receiver never decides this itself: the requirement is explicit
/// that a same-named file must not be silently replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// Detect conflicts and refuse to start until a policy is chosen.
    #[default]
    Ask,
    /// Replace same-named files; for directories, merge into the
    /// existing tree and keep entries the sender did not include.
    Overwrite,
    /// Leave existing entries alone and receive only the new ones.
    Skip,
    /// Receive everything under a new name (`report (1).pdf`).
    Rename,
}

/// One entry whose destination already exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub is_dir: bool,
}

impl ConflictPolicy {
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "ask" => Some(Self::Ask),
            "overwrite" => Some(Self::Overwrite),
            "skip" => Some(Self::Skip),
            "rename" => Some(Self::Rename),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Overwrite => "overwrite",
            Self::Skip => "skip",
            Self::Rename => "rename",
        }
    }
}

/// Find existing destinations, reporting a conflicting folder once for its
/// entire subtree. Directory merges require an explicit Overwrite decision.
pub async fn detect_conflicts(
    root: &Path,
    files: &[OfferedFile],
) -> Result<Vec<Conflict>, TransferError> {
    let root = root.to_owned();
    let files = files.to_vec();
    blocking(move || detect_conflicts_sync(&root, &files)).await
}

fn detect_conflicts_sync(
    root: &Path,
    files: &[OfferedFile],
) -> Result<Vec<Conflict>, TransferError> {
    for entry in files {
        entry.validate()?;
    }
    let mut fs = match ReceiveFs::open(root, false) {
        Ok(fs) => fs,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut entries: Vec<_> = files.iter().collect();
    entries.sort_by_key(|entry| entry.path.split('/').count());
    let mut conflicts: Vec<Conflict> = Vec::new();
    for entry in entries {
        if conflicts
            .iter()
            .any(|parent| entry.path.starts_with(&format!("{}/", parent.path)))
        {
            continue;
        }
        if let Some(is_dir) = fs.inspect(&entry.path)? {
            conflicts.push(Conflict {
                path: entry.path.clone(),
                is_dir,
            });
        }
    }
    conflicts.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(conflicts)
}

/// Apply the nearest renamed ancestor to a whole subtree.
fn renamed_path(relative: &str, renamed: &HashMap<String, String>) -> String {
    let mut prefix = relative;
    loop {
        if let Some(replacement) = renamed.get(prefix) {
            return format!("{replacement}{}", &relative[prefix.len()..]);
        }
        match prefix.rsplit_once('/') {
            Some((parent, _)) => prefix = parent,
            None => return relative.to_owned(),
        }
    }
}

fn numbered_path(path: &str, index: usize) -> String {
    let path = Path::new(path);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(ext) => format!("{stem} ({index}).{}", ext.to_string_lossy()),
        None => format!("{stem} ({index})"),
    };
    path.with_file_name(name)
        .to_string_lossy()
        .replace('\\', "/")
}

/// One entry read from disk, ready to be offered.
///
/// `source_path` is kept so the sender does not have to reconstruct
/// where the bytes live from the relative path, which breaks as soon as
/// the selected root is a file rather than a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFile {
    pub relative_path: String,
    pub source_path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub is_dir: bool,
}

impl PreparedFile {
    /// Entries that carry file content; directories are skipped.
    pub fn has_content(&self) -> bool {
        !self.is_dir
    }
}

/// Walk a file or directory and describe what would be sent.
///
/// Directories are reported with `is_dir` set and a zero size, which
/// preserves empty directories and the full hierarchy. Symbolic links
/// are rejected rather than followed: following them could pull in
/// files outside the selected tree, and preserving them would need a
/// platform-specific representation the first release does not define.
pub async fn prepare_transfer(root: &Path) -> Result<Vec<PreparedFile>, TransferError> {
    let metadata = tokio::fs::symlink_metadata(root).await?;
    if metadata.file_type().is_symlink() {
        return Err(TransferError::UnsupportedEntry(path_string(root)));
    }

    let mut files = Vec::new();
    let name = root
        .file_name()
        .ok_or_else(|| TransferError::UnsupportedEntry(path_string(root)))?;

    if metadata.is_dir() {
        collect_dir(root, PathBuf::from(name), &mut files).await?;
    } else if metadata.is_file() {
        files.push(prepare_file(root, to_wire_path(&PathBuf::from(name))?).await?);
    } else {
        return Err(TransferError::UnsupportedEntry(path_string(root)));
    }
    Ok(files)
}

async fn collect_dir(
    dir: &Path,
    relative: PathBuf,
    files: &mut Vec<PreparedFile>,
) -> Result<(), TransferError> {
    if files.len() >= MAX_FILES_PER_OFFER {
        return Err(TransferError::TooManyEntries(files.len() + 1));
    }
    files.push(PreparedFile {
        relative_path: to_wire_path(&relative)?,
        source_path: dir.to_owned(),
        size: 0,
        sha256: String::new(),
        is_dir: true,
    });

    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut children = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        children.push(entry);
    }
    // Stable order keeps offers reproducible across platforms and
    // makes failures easy to reproduce from a log.
    children.sort_by_key(|entry| entry.file_name());

    for entry in children {
        let file_type = entry.file_type().await?;
        let child_relative = relative.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(TransferError::UnsupportedEntry(path_string(&entry.path())));
        } else if file_type.is_dir() {
            Box::pin(collect_dir(&entry.path(), child_relative, files)).await?;
        } else if file_type.is_file() {
            files.push(prepare_file(&entry.path(), to_wire_path(&child_relative)?).await?);
        }
    }
    Ok(())
}

async fn prepare_file(path: &Path, relative_path: String) -> Result<PreparedFile, TransferError> {
    let mut file = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; MAX_CHUNK_DATA];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok(PreparedFile {
        relative_path,
        source_path: path.to_owned(),
        size,
        sha256: hex(&hasher.finalize()),
        is_dir: false,
    })
}

/// Convert the announced offer into the wire representation.
pub fn offered_files(files: &[PreparedFile]) -> Result<Vec<OfferedFile>, ProtocolError> {
    files
        .iter()
        .map(|file| {
            let entry = OfferedFile {
                path: file.relative_path.clone(),
                size: file.size,
                is_dir: file.is_dir,
                sha256: (!file.is_dir).then(|| file.sha256.clone()),
            };
            entry.validate()?;
            Ok(entry)
        })
        .collect()
}

/// Sink for the chunks a sender produces.
///
/// The session manager is one implementation; tests use an in-memory
/// collector, so the send orchestration is exercised without TLS.
pub trait ChunkSink {
    type Error;
    fn send_chunk(
        &mut self,
        chunk: crate::protocol::FileChunk,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>>;
}

/// Announce and stream an already prepared transfer through `sink`.
///
/// Chunks are produced one at a time from disk, so memory use stays at
/// one chunk regardless of the file size. On any error the caller
/// should send [`crate::protocol::DataMessage::FileCancel`]; this
/// function does not, because the sink may be the failing part.
pub async fn send_transfer<S: ChunkSink>(
    sink: &mut S,
    transfer_id: &str,
    prepared: &[PreparedFile],
) -> Result<u64, TransferError>
where
    S::Error: std::fmt::Display,
{
    send_transfer_with_progress(sink, transfer_id, prepared, |_| {}).await
}

/// Total payload bytes a prepared transfer will stream.
///
/// Directories carry no data, so they must not inflate the total used
/// for progress; otherwise an empty-folder-heavy transfer would appear
/// stuck below 100%.
pub fn total_bytes(prepared: &[PreparedFile]) -> u64 {
    prepared
        .iter()
        .filter(|file| file.has_content())
        .fold(0u64, |total, file| total.saturating_add(file.size))
}

/// Like [`send_transfer`], but calls `on_progress` with the running
/// byte count after each chunk is written to the sink.
///
/// The callback runs after the chunk is accepted, so a reported count
/// never claims bytes the peer has not been handed.
pub async fn send_transfer_with_progress<S, F>(
    sink: &mut S,
    transfer_id: &str,
    prepared: &[PreparedFile],
    mut on_progress: F,
) -> Result<u64, TransferError>
where
    S: ChunkSink,
    S::Error: std::fmt::Display,
    F: FnMut(u64),
{
    let mut sent = 0u64;
    for file in prepared.iter().filter(|file| file.has_content()) {
        let mut reader = FileReader::open(&file.source_path, file.size).await?;
        while let Some(data) = reader.next_chunk().await? {
            let offset = reader.offset() - data.len() as u64;
            sent += data.len() as u64;
            sink.send_chunk(crate::protocol::FileChunk {
                transfer_id: transfer_id.to_owned(),
                path: file.relative_path.clone(),
                offset,
                data,
            })
            .await
            .map_err(|e| TransferError::Sink(e.to_string()))?;
            on_progress(sent);
        }
    }
    Ok(sent)
}

/// Read one file in chunk-sized pieces.
pub struct FileReader {
    file: File,
    offset: u64,
    remaining: u64,
}

impl FileReader {
    pub async fn open(path: &Path, size: u64) -> Result<Self, TransferError> {
        Ok(Self {
            file: File::open(path).await?,
            offset: 0,
            remaining: size,
        })
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Next chunk of at most [`MAX_CHUNK_DATA`] bytes; `None` when the
    /// announced size has been read.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TransferError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let want = self.remaining.min(MAX_CHUNK_DATA as u64) as usize;
        let mut buffer = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            let read = self.file.read(&mut buffer[filled..]).await?;
            if read == 0 {
                // The file shrank after the offer was built.
                return Err(TransferError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file ended before its announced size",
                )));
            }
            filled += read;
        }
        self.offset += want as u64;
        self.remaining -= want as u64;
        Ok(Some(buffer))
    }
}

struct IncomingFile {
    relative_path: String,
    expected_size: u64,
    expected_digest: String,
    written: u64,
    hasher: Sha256,
    file: Option<File>,
    temporary: Temporary,
    final_name: String,
    overwrite: bool,
}

struct IncomingTransfer {
    /// Files still waiting for chunks, keyed by relative path.
    files: HashMap<String, IncomingFile>,
    /// Entries deliberately not received (`Skip` policy). Chunks for
    /// these still arrive and must be discarded quietly, otherwise the
    /// sender's transfer would fail on the receiver's user choice.
    skipped: std::collections::HashSet<String>,
    filesystem: ReceiveFs,
}

/// Receive side of a transfer: creates temp files, checks sizes and
/// digests, then commits them under the receive root.
pub struct TransferReceiver {
    transfers: HashMap<String, IncomingTransfer>,
}

impl Default for TransferReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferReceiver {
    pub fn new() -> Self {
        Self {
            transfers: HashMap::new(),
        }
    }

    /// Create directories and temp files for an announced offer.
    ///
    /// `policy` decides what happens to entries whose destination
    /// already exists. [`ConflictPolicy::Ask`] refuses to start when a
    /// conflict is found, so the caller can present the choice instead
    /// of writing anything the user did not agree to.
    ///
    /// Returns the list of relative paths that will need chunks, in
    /// offer order and under their original wire names. Rename maps
    /// destinations internally; the sender's chunk names do not change.
    pub async fn begin(
        &mut self,
        transfer_id: &str,
        root: &Path,
        files: &[OfferedFile],
        policy: ConflictPolicy,
    ) -> Result<Vec<String>, TransferError> {
        if files.len() > MAX_FILES_PER_OFFER {
            return Err(TransferError::TooManyEntries(files.len()));
        }
        // A second offer with the same id would silently replace the
        // first transfer and leak its open temp files.
        if self.transfers.contains_key(transfer_id) {
            return Err(TransferError::DuplicateTransfer(transfer_id.to_owned()));
        }
        let root = root.to_owned();
        let files = files.to_vec();
        let (transfer, pending) = blocking(move || prepare_incoming(&root, &files, policy)).await?;
        self.transfers.insert(transfer_id.to_owned(), transfer);
        Ok(pending)
    }

    /// Append a chunk; returns the relative path it belonged to.
    ///
    /// Chunks for an entry the user chose to skip are accepted and
    /// discarded: the sender has no way to know the receiver dropped
    /// that file, and failing here would abort an otherwise fine
    /// transfer.
    pub async fn write_chunk(
        &mut self,
        transfer_id: &str,
        file_path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), TransferError> {
        let transfer = self
            .transfers
            .get_mut(transfer_id)
            .ok_or_else(|| TransferError::UnknownTransfer(transfer_id.to_owned()))?;
        if transfer.skipped.contains(file_path) {
            return Ok(());
        }
        let file = transfer
            .files
            .get_mut(file_path)
            .ok_or_else(|| TransferError::UnknownFile(file_path.to_owned()))?;
        if offset != file.written {
            return Err(TransferError::UnexpectedOffset {
                path: file_path.to_owned(),
                offset,
                expected: file.written,
            });
        }
        if data.len() as u64 > file.expected_size.saturating_sub(file.written) {
            return Err(TransferError::ChunkOverflows {
                path: file_path.to_owned(),
                size: file.expected_size,
            });
        }
        file.file
            .as_mut()
            .expect("active file")
            .write_all(data)
            .await?;
        file.hasher.update(data);
        file.written += data.len() as u64;
        Ok(())
    }

    /// Verify every file that received data and move it into place.
    ///
    /// Returns the committed paths. The temp files of a failed
    /// verification are removed so an interrupted transfer cannot leave
    /// hidden data behind, and the previously committed file is left
    /// untouched until the new content is known good.
    pub async fn finish(&mut self, transfer_id: &str) -> Result<Vec<String>, TransferError> {
        let mut transfer = self
            .transfers
            .remove(transfer_id)
            .ok_or_else(|| TransferError::UnknownTransfer(transfer_id.to_owned()))?;
        let (completion, result) = tokio::sync::oneshot::channel();
        // Own cleanup independently of the caller: dropping finish/cancel
        // must not unlink a file while Tokio still has a write in flight.
        tokio::spawn(async move {
            let verified: Result<(), TransferError> = async {
                for file in transfer.files.values() {
                    verify(file)?;
                }
                for file in transfer.files.values_mut() {
                    let handle = file.file.as_mut().expect("active file");
                    handle.flush().await?;
                    handle.sync_all().await?;
                }
                Ok(())
            }
            .await;
            close_files(&mut transfer.files).await;
            tokio::task::spawn_blocking(move || {
                let outcome = (|| {
                    verified?;
                    if completion.is_closed() {
                        return Err(
                            io::Error::new(io::ErrorKind::Interrupted, "finish cancelled").into(),
                        );
                    }
                    let mut committed = Vec::new();
                    for file in transfer.files.values_mut() {
                        if completion.is_closed() {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "finish cancelled",
                            )
                            .into());
                        }
                        file.temporary.commit(&file.final_name, file.overwrite)?;
                        committed.push(file.relative_path.clone());
                    }
                    transfer.filesystem.keep_directories();
                    committed.sort();
                    Ok(committed)
                })();
                // Drop temporary files before their parent capabilities.
                drop(transfer);
                let _ = completion.send(outcome);
            })
            .await
            .expect("transfer commit worker panicked");
        });
        result
            .await
            .map_err(|error| io::Error::other(error.to_string()))?
    }

    /// Abandon a transfer, closing all handles before removing owned files
    /// and empty directories. Pre-existing directories are never removed.
    pub async fn cancel(&mut self, transfer_id: &str) -> Result<(), TransferError> {
        let Some(mut transfer) = self.transfers.remove(transfer_id) else {
            return Err(TransferError::UnknownTransfer(transfer_id.to_owned()));
        };
        let cleanup = tokio::spawn(async move {
            close_files(&mut transfer.files).await;
            blocking(move || {
                drop(transfer);
                Ok(())
            })
            .await
        });
        cleanup
            .await
            .map_err(|error| io::Error::other(error.to_string()))?
    }

    pub fn is_active(&self, transfer_id: &str) -> bool {
        self.transfers.contains_key(transfer_id)
    }
}

// Keep prepared capabilities and temporary files owned by a blocking worker
// even if the caller cancels while waiting for preparation to finish.
struct BlockingValue<T: Send + 'static>(Option<T>);

impl<T: Send + 'static> Drop for BlockingValue<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            tokio::task::spawn_blocking(move || drop(value));
        }
    }
}

async fn blocking<T, F>(operation: F) -> Result<T, TransferError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, TransferError> + Send + 'static,
{
    let mut result = tokio::task::spawn_blocking(move || BlockingValue(Some(operation())))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    result.0.take().expect("blocking result")
}

fn prepare_incoming(
    root: &Path,
    files: &[OfferedFile],
    policy: ConflictPolicy,
) -> Result<(IncomingTransfer, Vec<String>), TransferError> {
    // Validate the complete manifest before any mutation, including
    // duplicates and a file used as another entry's parent.
    let mut manifest = HashMap::new();
    for entry in files {
        entry.validate()?;
        if manifest.insert(entry.path.as_str(), entry.is_dir).is_some() {
            return Err(TransferError::UnsupportedEntry(entry.path.clone()));
        }
    }
    for entry in files {
        let mut parent = entry.path.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('/') {
            if manifest.get(prefix) == Some(&false) {
                return Err(TransferError::UnsupportedEntry(entry.path.clone()));
            }
            parent = prefix;
        }
    }
    if policy == ConflictPolicy::Ask {
        let conflicts = detect_conflicts_sync(root, files)?;
        if !conflicts.is_empty() {
            return Err(TransferError::Conflicts(conflicts));
        }
    }
    let needed = files
        .iter()
        .filter(|entry| !entry.is_dir)
        .fold(0u64, |total, entry| total.saturating_add(entry.size));
    check_space(root, needed, available_space(root))?;
    let mut filesystem = ReceiveFs::open(root, true)?;
    let mut incoming = HashMap::new();
    let mut skipped = std::collections::HashSet::<String>::new();
    let mut renamed = HashMap::new();
    let mut reserved = std::collections::HashSet::new();
    // Parents first even if an untrusted offer lists children first.
    let mut entries: Vec<_> = files.iter().collect();
    entries.sort_by_key(|entry| entry.path.split('/').count());
    let result: Result<(), TransferError> = (|| {
        for entry in entries {
            if skipped
                .iter()
                .any(|path| entry.path.starts_with(&format!("{path}/")))
            {
                skipped.insert(entry.path.clone());
                continue;
            }
            let mut target = renamed_path(&entry.path, &renamed);
            let exists = filesystem.inspect(&target)?.is_some();
            if exists && policy == ConflictPolicy::Skip {
                skipped.insert(entry.path.clone());
                continue;
            }
            if policy == ConflictPolicy::Rename && (exists || reserved.contains(&target)) {
                let original = target.clone();
                for index in 1.. {
                    let candidate = numbered_path(&original, index);
                    validate_relative_path(&candidate)?;
                    if !reserved.contains(&candidate) && filesystem.inspect(&candidate)?.is_none() {
                        target = candidate;
                        break;
                    }
                }
                renamed.insert(entry.path.clone(), target.clone());
            } else if exists && policy == ConflictPolicy::Ask {
                return Err(TransferError::Conflicts(vec![Conflict {
                    path: target,
                    is_dir: entry.is_dir,
                }]));
            }
            reserved.insert(target.clone());
            if entry.is_dir {
                filesystem.mkdir(&target, policy != ConflictPolicy::Overwrite)?;
                continue;
            }
            let (parent, final_name) = filesystem.parent(&target, true)?;
            let (file, temporary) = parent.temporary()?;
            incoming.insert(
                entry.path.clone(),
                IncomingFile {
                    relative_path: target,
                    expected_size: entry.size,
                    expected_digest: entry.sha256.clone().unwrap_or_default(),
                    written: 0,
                    hasher: Sha256::new(),
                    file: Some(File::from_std(file)),
                    temporary,
                    final_name,
                    overwrite: policy == ConflictPolicy::Overwrite,
                },
            );
        }
        Ok(())
    })();
    if let Err(error) = result {
        // Drop temporaries before the directory capabilities.
        drop(incoming);
        return Err(error);
    }
    let pending = files
        .iter()
        .filter(|entry| incoming.contains_key(&entry.path))
        .map(|entry| entry.path.clone())
        .collect();
    Ok((
        IncomingTransfer {
            files: incoming,
            skipped,
            filesystem,
        },
        pending,
    ))
}

async fn close_files(files: &mut HashMap<String, IncomingFile>) {
    for file in files.values_mut() {
        if let Some(handle) = file.file.take() {
            drop(handle.into_std().await);
        }
    }
}

fn verify(file: &IncomingFile) -> Result<(), TransferError> {
    if file.written != file.expected_size {
        return Err(TransferError::SizeMismatch {
            path: file.relative_path.clone(),
            expected: file.expected_size,
            actual: file.written,
        });
    }
    let actual = hex(&file.hasher.clone().finalize());
    if !is_sha256_hex(&file.expected_digest) || actual != file.expected_digest {
        return Err(TransferError::DigestMismatch {
            path: file.relative_path.clone(),
            expected: file.expected_digest.clone(),
            actual,
        });
    }
    Ok(())
}

fn to_wire_path(path: &Path) -> Result<String, TransferError> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| TransferError::UnsupportedEntry(path_string(path)))?;
                segments.push(part);
            }
            _ => return Err(TransferError::UnsupportedEntry(path_string(path))),
        }
    }
    Ok(segments.join("/"))
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::protocol::MAX_CHUNK_DATA;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Unique scratch directory per test, removed on drop.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "lan-mouse-transfer-{}-{label}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn run_local<F>(future: F)
    where
        F: std::future::Future<Output = ()> + 'static,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(future);
    }

    #[test]
    fn cancelled_preparation_cleans_up_off_the_runtime_thread() {
        struct PreparedProbe {
            transfer: Option<IncomingTransfer>,
            cleaned: Option<tokio::sync::oneshot::Sender<std::thread::ThreadId>>,
        }
        impl Drop for PreparedProbe {
            fn drop(&mut self) {
                drop(self.transfer.take());
                let _ = self
                    .cleaned
                    .take()
                    .unwrap()
                    .send(std::thread::current().id());
            }
        }
        run_local(async {
            let dir = Scratch::new("cancel-preparation");
            let root = dir.path.clone();
            let runtime_thread = std::thread::current().id();
            let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                blocking(move || {
                    assert_ne!(std::thread::current().id(), runtime_thread);
                    let (transfer, _) = prepare_incoming(
                        &root,
                        &[offer("folder/file.txt", b"payload")],
                        ConflictPolicy::Ask,
                    )?;
                    let probe = PreparedProbe {
                        transfer: Some(transfer),
                        cleaned: Some(cleaned_tx),
                    };
                    prepared_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(probe)
                })
                .await
            });
            // A blocked filesystem worker leaves this single runtime thread
            // free to cancel the caller before preparation returns its files.
            prepared_rx.await.unwrap();
            assert!(dir.join("folder").is_dir());
            task.abort();
            assert!(matches!(task.await, Err(error) if error.is_cancelled()));
            release_tx.send(()).unwrap();
            let cleanup_thread =
                tokio::time::timeout(std::time::Duration::from_secs(5), cleaned_rx)
                    .await
                    .unwrap()
                    .unwrap();
            assert_ne!(cleanup_thread, runtime_thread);
            assert!(!dir.join("folder").exists());
        });
    }

    /// Send a prepared transfer through a receiver, exercising the real
    /// chunk size and offsets.
    async fn transmit(
        prepared: &[PreparedFile],
        receiver: &mut TransferReceiver,
        dest: &Path,
        transfer_id: &str,
    ) -> Result<Vec<String>, TransferError> {
        transmit_policy(prepared, receiver, dest, transfer_id, ConflictPolicy::Ask).await
    }

    /// Same as [`transmit`] but with an explicit conflict policy.
    async fn transmit_policy(
        prepared: &[PreparedFile],
        receiver: &mut TransferReceiver,
        dest: &Path,
        transfer_id: &str,
        policy: ConflictPolicy,
    ) -> Result<Vec<String>, TransferError> {
        let offered = offered_files(prepared)?;
        receiver.begin(transfer_id, dest, &offered, policy).await?;
        for file in prepared.iter().filter(|f| f.has_content()) {
            let mut reader = FileReader::open(&file.source_path, file.size).await?;
            while let Some(chunk) = reader.next_chunk().await? {
                let offset = reader.offset() - chunk.len() as u64;
                receiver
                    .write_chunk(transfer_id, &file.relative_path, offset, &chunk)
                    .await?;
            }
        }
        receiver.finish(transfer_id).await
    }

    /// In-memory sink so the send path can be driven without TLS.
    #[derive(Default)]
    struct VecSink {
        chunks: Vec<crate::protocol::FileChunk>,
    }

    impl ChunkSink for VecSink {
        type Error = std::convert::Infallible;

        async fn send_chunk(
            &mut self,
            chunk: crate::protocol::FileChunk,
        ) -> Result<(), Self::Error> {
            self.chunks.push(chunk);
            Ok(())
        }
    }

    fn write(dir: &Path, relative: &str, content: &[u8]) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Progress must count payload bytes only: folders carry no data, so
    /// including them would leave the bar stuck below 100%.
    #[test]
    fn total_bytes_counts_payload_not_directories() {
        run_local(async {
            let dir = Scratch::new("progress-src");
            write(&dir.path, "a.txt", b"12345");
            std::fs::create_dir_all(dir.join("empty")).unwrap();
            write(&dir.path, "nested/b.bin", &[0u8; 100]);

            let prepared = prepare_transfer(&dir.path).await.unwrap();
            let payload: u64 = prepared
                .iter()
                .filter(|file| !file.is_dir)
                .map(|file| file.size)
                .sum();
            assert_eq!(payload, 105);
            assert_eq!(total_bytes(&prepared), 105);
            // Sanity: the transfer really contains a directory entry.
            assert!(prepared.iter().any(|file| file.is_dir));
        });
    }

    /// The progress callback must fire once per chunk, monotonically,
    /// and end at the announced total.
    #[test]
    fn progress_increases_per_chunk_up_to_the_total() {
        run_local(async {
            let dir = Scratch::new("progress-send");
            let dest = Scratch::new("progress-recv");
            let data: Vec<u8> = (0..(MAX_CHUNK_DATA * 2 + 9))
                .map(|i| (i % 251) as u8)
                .collect();
            write(&dir.path, "big.bin", &data);

            let prepared = prepare_transfer(&dir.join("big.bin")).await.unwrap();
            let total = total_bytes(&prepared);
            assert_eq!(total, data.len() as u64);

            let mut receiver = TransferReceiver::new();
            let offered = offered_files(&prepared).unwrap();
            receiver
                .begin("progress", &dest.path, &offered, ConflictPolicy::Overwrite)
                .await
                .unwrap();

            let mut sink = VecSink::default();
            let mut samples = Vec::new();
            let sent = send_transfer_with_progress(&mut sink, "progress", &prepared, |sent| {
                samples.push(sent)
            })
            .await
            .unwrap();
            assert_eq!(sent, total);

            // One sample per chunk, strictly increasing, ending at the
            // total: that is what makes the bar move smoothly.
            assert_eq!(samples.len(), sink.chunks.len());
            assert!(samples.windows(2).all(|w| w[0] < w[1]), "{samples:?}");
            assert_eq!(samples.last().copied(), Some(total));

            for chunk in sink.chunks {
                receiver
                    .write_chunk("progress", &chunk.path, chunk.offset, &chunk.data)
                    .await
                    .unwrap();
            }
            receiver.finish("progress").await.unwrap();
            assert_eq!(std::fs::read(dest.join("big.bin")).unwrap(), data);
        });
    }

    /// An empty selection has nothing to stream and must report zero.
    #[test]
    fn total_bytes_of_directories_only_is_zero() {
        run_local(async {
            let dir = Scratch::new("progress-empty");
            std::fs::create_dir_all(dir.join("only/empty")).unwrap();
            let prepared = prepare_transfer(&dir.join("only")).await.unwrap();
            assert_eq!(total_bytes(&prepared), 0);
        });
    }

    #[test]
    fn nested_tree_round_trips_with_matching_digests() {
        run_local(async {
            let source = Scratch::new("tree-src");
            let dest = Scratch::new("tree-dst");
            write(&source.join("album"), "a.png", b"first image");
            write(&source.join("album"), "nested/b.txt", b"deeper file");
            write(&source.join("album"), "nested/c.bin", &[0u8, 1, 2, 255]);
            std::fs::create_dir_all(source.join("album/empty")).unwrap();

            let prepared = prepare_transfer(&source.join("album")).await.unwrap();
            let mut receiver = TransferReceiver::new();
            let committed = transmit(&prepared, &mut receiver, &dest.path, "t-1")
                .await
                .unwrap();

            assert_eq!(committed.len(), 3);
            assert_eq!(
                std::fs::read(dest.join("album/a.png")).unwrap(),
                b"first image"
            );
            assert_eq!(
                std::fs::read(dest.join("album/nested/b.txt")).unwrap(),
                b"deeper file"
            );
            assert_eq!(
                std::fs::read(dest.join("album/nested/c.bin")).unwrap(),
                vec![0u8, 1, 2, 255]
            );
            // Empty directories survive because they are offered.
            assert!(dest.join("album/empty").is_dir());
            // No temp files are left behind.
            assert!(std::fs::read_dir(dest.join("album")).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".lanm-partial")
            }));
        });
    }

    #[test]
    fn large_file_streams_in_multiple_chunks() {
        run_local(async {
            let source = Scratch::new("large-src");
            let dest = Scratch::new("large-dst");
            let content: Vec<u8> = (0..(MAX_CHUNK_DATA * 2 + 517))
                .map(|i| (i % 251) as u8)
                .collect();
            std::fs::write(source.join("big.bin"), &content).unwrap();

            let prepared = prepare_transfer(&source.join("big.bin")).await.unwrap();
            assert_eq!(prepared.len(), 1);
            assert!(!prepared[0].is_dir);
            let mut receiver = TransferReceiver::new();
            let committed = transmit(&prepared, &mut receiver, &dest.path, "t-2")
                .await
                .unwrap();

            assert_eq!(committed, vec!["big.bin".to_string()]);
            assert_eq!(std::fs::read(dest.join("big.bin")).unwrap(), content);
        });
    }

    #[test]
    fn corrupted_payload_is_rejected_and_leaves_no_temp_file() {
        run_local(async {
            let source = Scratch::new("bad-src");
            let dest = Scratch::new("bad-dst");
            std::fs::write(source.join("doc.txt"), b"trustworthy").unwrap();
            let prepared = prepare_transfer(&source.join("doc.txt")).await.unwrap();

            let offered = offered_files(&prepared).unwrap();
            let mut receiver = TransferReceiver::new();
            receiver
                .begin("t-3", &dest.path, &offered, ConflictPolicy::Ask)
                .await
                .unwrap();
            // Same length, different bytes: the size check passes and
            // only the digest can catch this.
            receiver
                .write_chunk("t-3", "doc.txt", 0, b"trustworthY")
                .await
                .unwrap();
            let error = receiver.finish("t-3").await.unwrap_err();
            assert!(matches!(error, TransferError::DigestMismatch { .. }));
            assert!(!dest.join("doc.txt").exists());
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 0);
        });
    }

    #[test]
    fn truncated_payload_fails_the_size_check() {
        run_local(async {
            let source = Scratch::new("short-src");
            let dest = Scratch::new("short-dst");
            std::fs::write(source.join("short.txt"), b"0123456789").unwrap();
            let prepared = prepare_transfer(&source.join("short.txt")).await.unwrap();

            let offered = offered_files(&prepared).unwrap();
            let mut receiver = TransferReceiver::new();
            receiver
                .begin("t-4", &dest.path, &offered, ConflictPolicy::Ask)
                .await
                .unwrap();
            receiver
                .write_chunk("t-4", "short.txt", 0, b"01234")
                .await
                .unwrap();
            let error = receiver.finish("t-4").await.unwrap_err();
            assert!(matches!(error, TransferError::SizeMismatch { .. }));
            assert!(!dest.join("short.txt").exists());
        });
    }

    #[test]
    fn out_of_order_and_overflowing_chunks_are_rejected() {
        run_local(async {
            let source = Scratch::new("offset-src");
            let dest = Scratch::new("offset-dst");
            std::fs::write(source.join("f.txt"), b"abcdef").unwrap();
            let prepared = prepare_transfer(&source.join("f.txt")).await.unwrap();
            let offered = offered_files(&prepared).unwrap();

            let mut receiver = TransferReceiver::new();
            receiver
                .begin("t-5", &dest.path, &offered, ConflictPolicy::Ask)
                .await
                .unwrap();
            let error = receiver
                .write_chunk("t-5", "f.txt", 3, b"def")
                .await
                .unwrap_err();
            assert!(matches!(error, TransferError::UnexpectedOffset { .. }));

            let error = receiver
                .write_chunk("t-5", "f.txt", 0, b"far too much data")
                .await
                .unwrap_err();
            assert!(matches!(error, TransferError::ChunkOverflows { .. }));
        });
    }

    #[test]
    fn traversal_paths_never_leave_the_receive_root() {
        run_local(async {
            let dest = Scratch::new("escape-dst");
            let mut receiver = TransferReceiver::new();
            for bad in ["../escape.txt", "/abs.txt", "a/../../b.txt", "a\\b.txt"] {
                let offered = vec![OfferedFile {
                    path: bad.to_owned(),
                    size: 1,
                    is_dir: false,
                    sha256: Some("0".repeat(64)),
                }];
                let result = receiver
                    .begin("t-bad", &dest.path, &offered, ConflictPolicy::Ask)
                    .await;
                assert!(result.is_err(), "path `{bad}` should have been rejected");
                // Nothing may be created outside or inside the root.
                assert!(!dest.join("escape.txt").exists());
                assert!(!dest.join("b.txt").exists());
            }
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 0);
        });
    }

    #[test]
    fn cancel_removes_partial_files() {
        run_local(async {
            let source = Scratch::new("cancel-src");
            let dest = Scratch::new("cancel-dst");
            std::fs::write(source.join("c.txt"), b"0123456789").unwrap();
            let prepared = prepare_transfer(&source.join("c.txt")).await.unwrap();
            let offered = offered_files(&prepared).unwrap();

            let mut receiver = TransferReceiver::new();
            receiver
                .begin("t-6", &dest.path, &offered, ConflictPolicy::Ask)
                .await
                .unwrap();
            receiver
                .write_chunk("t-6", "c.txt", 0, b"01234")
                .await
                .unwrap();
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 1);
            receiver.cancel("t-6").await.unwrap();
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 0);
            assert!(!dest.join("c.txt").exists());
            assert!(!receiver.is_active("t-6"));
        });
    }

    #[test]
    fn symlinks_are_refused_rather_than_followed() {
        run_local(async {
            let source = Scratch::new("link-src");
            let outside = Scratch::new("link-outside");
            write(&outside.path, "secret.txt", b"should not travel");
            let root = source.join("tree");
            std::fs::create_dir_all(root.join("inner")).unwrap();
            write(&root, "inner/ok.txt", b"fine");

            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("inner/leak.txt"))
                    .unwrap();
                let error = prepare_transfer(&root).await.unwrap_err();
                assert!(matches!(error, TransferError::UnsupportedEntry(_)));
            }
            #[cfg(not(unix))]
            {
                // Windows symlinks need privileges; the code path is the
                // same `symlink_metadata` check, so nothing to add here.
                let _ = &outside;
            }
        });
    }

    /// A bad entry halfway through an offer must not leave temp files
    /// or directories behind from the entries that already succeeded.
    #[test]
    fn rejected_offer_cleans_up_what_it_already_created() {
        run_local(async {
            let dest = Scratch::new("partial-offer-dst");
            let mut receiver = TransferReceiver::new();
            let files = vec![
                OfferedFile {
                    path: "good/first.txt".into(),
                    size: 4,
                    is_dir: false,
                    sha256: Some("a".repeat(64)),
                },
                OfferedFile {
                    path: "../escape.txt".into(),
                    size: 4,
                    is_dir: false,
                    sha256: Some("b".repeat(64)),
                },
            ];
            let error = receiver
                .begin("t-bad", &dest.path, &files, ConflictPolicy::Ask)
                .await
                .unwrap_err();
            assert!(matches!(error, TransferError::Protocol(_)));
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 0);
            assert!(!dest.join("good").exists());
            assert!(!receiver.is_active("t-bad"));
        });
    }

    /// Two offers with the same id would otherwise overwrite the first
    /// transfer and leak its open files.
    #[test]
    fn duplicate_transfer_id_is_rejected() {
        run_local(async {
            let source = Scratch::new("dup-src");
            let dest = Scratch::new("dup-dst");
            std::fs::write(source.join("a.txt"), b"aaaa").unwrap();
            let prepared = prepare_transfer(&source.join("a.txt")).await.unwrap();
            let offered = offered_files(&prepared).unwrap();

            let mut receiver = TransferReceiver::new();
            receiver
                .begin("same", &dest.path, &offered, ConflictPolicy::Ask)
                .await
                .unwrap();
            let error = receiver
                .begin("same", &dest.path, &offered, ConflictPolicy::Ask)
                .await
                .unwrap_err();
            assert!(matches!(error, TransferError::DuplicateTransfer(_)));
            // The original transfer is still usable afterwards.
            assert!(receiver.is_active("same"));
        });
    }

    /// Overwriting a folder merges: same-named files are replaced, new
    /// files are added, and entries only the receiver had stay.
    #[test]
    fn overwrite_merges_directory_and_keeps_receiver_only_files() {
        run_local(async {
            let source = Scratch::new("merge-src");
            let dest = Scratch::new("merge-dst");
            // Sender side: docs/{keep.txt is NOT sent, updated.txt, added.txt}
            std::fs::create_dir_all(source.join("docs")).unwrap();
            std::fs::write(source.join("docs/updated.txt"), b"new content").unwrap();
            std::fs::write(source.join("docs/added.txt"), b"added").unwrap();

            // Receiver already has the folder with a same-named file and
            // a file the sender does not know about.
            std::fs::create_dir_all(dest.join("docs")).unwrap();
            std::fs::write(dest.join("docs/updated.txt"), b"old content").unwrap();
            std::fs::write(dest.join("docs/receiver-only.txt"), b"mine").unwrap();

            let prepared = prepare_transfer(&source.join("docs")).await.unwrap();
            let offered = offered_files(&prepared).unwrap();
            let committed = transmit_policy(
                &prepared,
                &mut TransferReceiver::new(),
                &dest.path,
                "merge",
                ConflictPolicy::Overwrite,
            )
            .await
            .unwrap();

            assert_eq!(committed.len(), 2);
            assert_eq!(
                std::fs::read(dest.join("docs/updated.txt")).unwrap(),
                b"new content"
            );
            assert_eq!(
                std::fs::read(dest.join("docs/added.txt")).unwrap(),
                b"added"
            );
            // The whole point of merging rather than replacing.
            assert_eq!(
                std::fs::read(dest.join("docs/receiver-only.txt")).unwrap(),
                b"mine"
            );
            let _ = offered;
        });
    }

    #[test]
    fn skip_existing_folder_skips_its_entire_subtree() {
        run_local(async {
            let source = Scratch::new("skip-src");
            let dest = Scratch::new("skip-dst");
            std::fs::create_dir_all(source.join("batch")).unwrap();
            std::fs::write(source.join("batch/existing.txt"), b"incoming").unwrap();
            std::fs::write(source.join("batch/fresh.txt"), b"fresh").unwrap();

            std::fs::create_dir_all(dest.join("batch")).unwrap();
            std::fs::write(dest.join("batch/existing.txt"), b"original").unwrap();

            let prepared = prepare_transfer(&source.join("batch")).await.unwrap();
            let committed = transmit_policy(
                &prepared,
                &mut TransferReceiver::new(),
                &dest.path,
                "skip",
                ConflictPolicy::Skip,
            )
            .await
            .unwrap();

            assert!(committed.is_empty());
            assert_eq!(
                std::fs::read(dest.join("batch/existing.txt")).unwrap(),
                b"original"
            );
            assert!(!dest.join("batch/fresh.txt").exists());
        });
    }

    #[test]
    fn rename_keeps_both_copies() {
        run_local(async {
            let source = Scratch::new("rename-src");
            let dest = Scratch::new("rename-dst");
            std::fs::write(source.join("report.pdf"), b"v2").unwrap();
            std::fs::write(dest.join("report.pdf"), b"v1").unwrap();

            let prepared = prepare_transfer(&source.join("report.pdf")).await.unwrap();
            let committed = transmit_policy(
                &prepared,
                &mut TransferReceiver::new(),
                &dest.path,
                "rename",
                ConflictPolicy::Rename,
            )
            .await
            .unwrap();

            assert_eq!(committed, vec!["report (1).pdf".to_string()]);
            assert_eq!(std::fs::read(dest.join("report.pdf")).unwrap(), b"v1");
            assert_eq!(std::fs::read(dest.join("report (1).pdf")).unwrap(), b"v2");
        });
    }

    #[test]
    fn ask_policy_refuses_to_write_when_a_name_exists() {
        run_local(async {
            let source = Scratch::new("ask-src");
            let dest = Scratch::new("ask-dst");
            std::fs::write(source.join("a.txt"), b"new").unwrap();
            std::fs::write(dest.join("a.txt"), b"old").unwrap();

            let prepared = prepare_transfer(&source.join("a.txt")).await.unwrap();
            let offered = offered_files(&prepared).unwrap();
            let mut receiver = TransferReceiver::new();
            let error = receiver
                .begin("ask", &dest.path, &offered, ConflictPolicy::Ask)
                .await
                .unwrap_err();
            match error {
                TransferError::Conflicts(list) => {
                    assert_eq!(list.len(), 1);
                    assert_eq!(list[0].path, "a.txt");
                }
                other => panic!("expected conflicts, got {other:?}"),
            }
            // Nothing was touched and no temp file was created.
            assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"old");
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 1);
            assert!(!receiver.is_active("ask"));
        });
    }

    #[test]
    fn conflicts_are_detected_before_any_write() {
        run_local(async {
            let dest = Scratch::new("detect-dst");
            std::fs::create_dir_all(dest.join("album")).unwrap();
            std::fs::write(dest.join("album/taken.txt"), b"x").unwrap();

            let files = vec![
                OfferedFile {
                    path: "album".into(),
                    size: 0,
                    is_dir: true,
                    sha256: None,
                },
                OfferedFile {
                    path: "album/taken.txt".into(),
                    size: 1,
                    is_dir: false,
                    sha256: Some("a".repeat(64)),
                },
                OfferedFile {
                    path: "album/free.txt".into(),
                    size: 1,
                    is_dir: false,
                    sha256: Some("b".repeat(64)),
                },
            ];
            let conflicts = detect_conflicts(&dest.path, &files).await.unwrap();
            // A folder conflict covers the entire offered subtree.
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, "album");
            assert!(conflicts[0].is_dir);
        });
    }

    #[test]
    fn rename_skips_existing_numbered_names() {
        run_local(async {
            let dir = Scratch::new("unique-dst");
            std::fs::write(dir.join("f.txt"), b"old").unwrap();
            std::fs::write(dir.join("f (1).txt"), b"old numbered").unwrap();
            let mut receiver = TransferReceiver::new();
            let files = [offer("f.txt", b"new"), offer("free.txt", b"")];
            receiver
                .begin("rename", &dir.path, &files, ConflictPolicy::Rename)
                .await
                .unwrap();
            receiver
                .write_chunk("rename", "f.txt", 0, b"new")
                .await
                .unwrap();
            assert_eq!(
                receiver.finish("rename").await.unwrap(),
                vec!["f (2).txt", "free.txt"]
            );
            assert_eq!(std::fs::read(dir.join("f.txt")).unwrap(), b"old");
            assert_eq!(
                std::fs::read(dir.join("f (1).txt")).unwrap(),
                b"old numbered"
            );
            assert_eq!(std::fs::read(dir.join("f (2).txt")).unwrap(), b"new");
        });
    }

    fn offer(path: &str, data: &[u8]) -> OfferedFile {
        OfferedFile {
            path: path.into(),
            size: data.len() as u64,
            is_dir: false,
            sha256: Some(hex(&Sha256::digest(data))),
        }
    }

    fn directory_offer(path: &str) -> OfferedFile {
        OfferedFile {
            path: path.into(),
            size: 0,
            is_dir: true,
            sha256: None,
        }
    }

    #[test]
    fn preexisting_partial_is_never_modified_or_removed() {
        run_local(async {
            let dest = Scratch::new("user-partial");
            write(&dest.path, "doc.lanm-partial", b"user data");
            let mut receiver = TransferReceiver::new();
            let files = [offer("doc", b"new")];
            receiver
                .begin("a", &dest.path, &files, ConflictPolicy::Ask)
                .await
                .unwrap();
            receiver.write_chunk("a", "doc", 0, b"new").await.unwrap();
            assert_eq!(
                std::fs::read(dest.join("doc.lanm-partial")).unwrap(),
                b"user data"
            );
            receiver.cancel("a").await.unwrap();
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 1);
            receiver
                .begin("b", &dest.path, &files, ConflictPolicy::Ask)
                .await
                .unwrap();
            receiver.write_chunk("b", "doc", 0, b"new").await.unwrap();
            receiver.finish("b").await.unwrap();
            assert_eq!(std::fs::read(dest.join("doc")).unwrap(), b"new");
            assert_eq!(
                std::fs::read(dest.join("doc.lanm-partial")).unwrap(),
                b"user data"
            );
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 2);
        });
    }

    #[test]
    fn concurrent_receivers_never_overwrite_without_permission() {
        run_local(async {
            for policy in [
                ConflictPolicy::Ask,
                ConflictPolicy::Skip,
                ConflictPolicy::Rename,
            ] {
                let dest = Scratch::new("concurrent");
                let mut first = TransferReceiver::new();
                let mut second = TransferReceiver::new();
                first
                    .begin("same", &dest.path, &[offer("doc", b"one")], policy)
                    .await
                    .unwrap();
                second
                    .begin("same", &dest.path, &[offer("doc", b"two")], policy)
                    .await
                    .unwrap();
                assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 2);
                first.write_chunk("same", "doc", 0, b"one").await.unwrap();
                second.write_chunk("same", "doc", 0, b"two").await.unwrap();
                first.finish("same").await.unwrap();
                assert!(second.finish("same").await.is_err());
                assert_eq!(std::fs::read(dest.join("doc")).unwrap(), b"one");
                assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 1);
            }
        });
    }

    #[test]
    fn cancelling_one_transfer_does_not_remove_anothers_temporary() {
        run_local(async {
            let dest = Scratch::new("cancel-concurrent");
            let mut receiver = TransferReceiver::new();
            let files = [offer("doc", b"data")];
            for id in ["a", "b"] {
                receiver
                    .begin(id, &dest.path, &files, ConflictPolicy::Overwrite)
                    .await
                    .unwrap();
                receiver.write_chunk(id, "doc", 0, b"data").await.unwrap();
            }
            receiver.cancel("a").await.unwrap();
            receiver.finish("b").await.unwrap();
            assert_eq!(std::fs::read(dest.join("doc")).unwrap(), b"data");
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 1);
        });
    }

    #[test]
    fn ask_includes_empty_existing_folders_and_rename_maps_subtree() {
        run_local(async {
            let dest = Scratch::new("folder-policies");
            std::fs::create_dir(dest.join("album")).unwrap();
            // Deliberately child-first to exercise manifest ordering.
            let files = [
                offer("album/nested/doc", b"new"),
                directory_offer("album"),
                directory_offer("album/nested"),
                directory_offer("album/empty"),
            ];
            let mut receiver = TransferReceiver::new();
            assert!(matches!(
                receiver
                    .begin("ask", &dest.path, &files, ConflictPolicy::Ask)
                    .await,
                Err(TransferError::Conflicts(_))
            ));
            assert_eq!(std::fs::read_dir(dest.join("album")).unwrap().count(), 0);
            assert_eq!(
                receiver
                    .begin("rename", &dest.path, &files, ConflictPolicy::Rename)
                    .await
                    .unwrap(),
                vec!["album/nested/doc"]
            );
            receiver
                .write_chunk("rename", "album/nested/doc", 0, b"new")
                .await
                .unwrap();
            assert_eq!(
                receiver.finish("rename").await.unwrap(),
                vec!["album (1)/nested/doc"]
            );
            assert_eq!(
                std::fs::read(dest.join("album (1)/nested/doc")).unwrap(),
                b"new"
            );
            assert!(dest.join("album (1)/empty").is_dir());
            assert_eq!(std::fs::read_dir(dest.join("album")).unwrap().count(), 0);
        });
    }

    #[test]
    fn cancel_only_removes_directories_created_by_this_transfer() {
        run_local(async {
            let dest = Scratch::new("owned-dirs");
            std::fs::create_dir(dest.join("existing")).unwrap();
            let files = [
                directory_offer("existing"),
                directory_offer("existing/new"),
                offer("existing/new/deeper/doc", b"data"),
            ];
            let mut receiver = TransferReceiver::new();
            receiver
                .begin("a", &dest.path, &files, ConflictPolicy::Overwrite)
                .await
                .unwrap();
            receiver
                .write_chunk("a", "existing/new/deeper/doc", 0, b"data")
                .await
                .unwrap();
            receiver.cancel("a").await.unwrap();
            assert!(dest.join("existing").is_dir());
            assert_eq!(std::fs::read_dir(dest.join("existing")).unwrap().count(), 0);
        });
    }

    #[test]
    fn failed_begin_closes_files_and_cleans_only_owned_paths() {
        run_local(async {
            let dest = Scratch::new("failed-begin");
            std::fs::create_dir(dest.join("existing")).unwrap();
            write(&dest.path, "blocked", b"mine");
            let files = [
                directory_offer("existing"),
                offer("existing/good", b"new"),
                offer("blocked/bad", b"new"),
            ];
            let mut receiver = TransferReceiver::new();
            assert!(
                receiver
                    .begin("a", &dest.path, &files, ConflictPolicy::Overwrite)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_dir(dest.join("existing")).unwrap().count(), 0);
            assert_eq!(std::fs::read(dest.join("blocked")).unwrap(), b"mine");
        });
    }

    #[cfg(unix)]
    #[test]
    fn receiver_rejects_symlink_ancestors_and_final_symlinks() {
        run_local(async {
            let dest = Scratch::new("receiver-link");
            let outside = Scratch::new("receiver-outside");
            write(&outside.path, "doc", b"private");
            std::os::unix::fs::symlink(&outside.path, dest.join("link")).unwrap();
            std::os::unix::fs::symlink(outside.join("doc"), dest.join("doc")).unwrap();
            let mut receiver = TransferReceiver::new();
            for path in ["link/new", "link/doc", "doc"] {
                for policy in [
                    ConflictPolicy::Ask,
                    ConflictPolicy::Overwrite,
                    ConflictPolicy::Skip,
                    ConflictPolicy::Rename,
                ] {
                    assert!(
                        receiver
                            .begin("a", &dest.path, &[offer(path, b"new")], policy)
                            .await
                            .is_err()
                    );
                }
            }
            assert_eq!(std::fs::read(outside.join("doc")).unwrap(), b"private");
            assert_eq!(std::fs::read_dir(&outside.path).unwrap().count(), 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn replacing_parent_with_symlink_cannot_redirect_commit_or_cleanup() {
        run_local(async {
            let dest = Scratch::new("swapped-parent");
            let outside = Scratch::new("swapped-outside");
            write(&outside.path, "doc", b"private");
            let mut receiver = TransferReceiver::new();
            for (id, finish) in [("commit", true), ("cancel", false)] {
                let path = format!("{id}/doc");
                receiver
                    .begin(
                        id,
                        &dest.path,
                        &[offer(&path, b"new")],
                        ConflictPolicy::Overwrite,
                    )
                    .await
                    .unwrap();
                receiver.write_chunk(id, &path, 0, b"new").await.unwrap();
                std::fs::rename(dest.join(id), dest.join(&format!("{id}-moved"))).unwrap();
                std::os::unix::fs::symlink(&outside.path, dest.join(id)).unwrap();
                if finish {
                    receiver.finish(id).await.unwrap();
                } else {
                    receiver.cancel(id).await.unwrap();
                }
                assert_eq!(std::fs::read(outside.join("doc")).unwrap(), b"private");
            }
            assert_eq!(
                std::fs::read(dest.join("commit-moved/doc")).unwrap(),
                b"new"
            );
            assert_eq!(
                std::fs::read_dir(dest.join("cancel-moved"))
                    .unwrap()
                    .count(),
                0
            );
        });
    }

    #[cfg(windows)]
    #[test]
    fn receiver_holds_windows_parent_against_rename_until_cancel() {
        run_local(async {
            let dest = Scratch::new("windows-parent-lock");
            std::fs::create_dir(dest.join("parent")).unwrap();
            let mut receiver = TransferReceiver::new();
            receiver
                .begin(
                    "a",
                    &dest.path,
                    &[offer("parent/doc", b"new")],
                    ConflictPolicy::Overwrite,
                )
                .await
                .unwrap();
            assert!(std::fs::rename(dest.join("parent"), dest.join("moved")).is_err());
            receiver.cancel("a").await.unwrap();
            std::fs::rename(dest.join("parent"), dest.join("moved")).unwrap();
        });
    }

    /// The space comparison is pure so it cannot race the live disk.
    #[test]
    fn space_check_refuses_only_when_a_known_limit_is_too_small() {
        let root = Path::new("/receive");
        let refused = check_space(root, 101, Some(100));
        assert!(matches!(
            refused,
            Err(TransferError::NotEnoughSpace {
                needed: 101,
                available: 100,
                ..
            })
        ));
        assert!(check_space(root, 100, Some(100)).is_ok());
        assert!(check_space(root, 100, Some(1_000)).is_ok());
        // An unknown limit must never block receiving.
        assert!(check_space(root, u64::MAX, None).is_ok());
    }

    /// A peer can announce sizes that overflow a `u64` total. The sum
    /// must saturate and still be refused instead of panicking (debug)
    /// or wrapping to a small number that passes the check (release).
    #[cfg(unix)]
    #[test]
    fn absurd_announced_sizes_do_not_overflow_the_space_check() {
        run_local(async {
            let dest = Scratch::new("overflow-dst");
            let files = vec![
                OfferedFile {
                    path: "a.bin".into(),
                    size: u64::MAX,
                    is_dir: false,
                    sha256: Some("a".repeat(64)),
                },
                OfferedFile {
                    path: "b.bin".into(),
                    size: u64::MAX,
                    is_dir: false,
                    sha256: Some("b".repeat(64)),
                },
            ];
            let mut receiver = TransferReceiver::new();
            let error = receiver
                .begin("overflow", &dest.path, &files, ConflictPolicy::Overwrite)
                .await
                .unwrap_err();
            match error {
                TransferError::NotEnoughSpace { needed, .. } => {
                    assert_eq!(needed, u64::MAX, "the total must saturate");
                }
                other => panic!("expected NotEnoughSpace, got {other:?}"),
            }
            assert!(!receiver.is_active("overflow"));
        });
    }

    /// A transfer larger than the free space is refused before any
    /// temp file is created.
    ///
    /// The requested size is far beyond any real disk rather than
    /// `available + 1`: other tests free space concurrently, so a
    /// one-byte margin makes this race against the live filesystem.
    #[cfg(unix)]
    #[test]
    fn transfer_larger_than_free_space_is_refused() {
        run_local(async {
            let dest = Scratch::new("space-dst");
            assert!(
                available_space(&dest.path).is_some(),
                "unix should report free space"
            );
            let files = vec![OfferedFile {
                path: "huge.bin".into(),
                size: u64::MAX / 2,
                is_dir: false,
                sha256: Some("a".repeat(64)),
            }];
            let mut receiver = TransferReceiver::new();
            let error = receiver
                .begin("space", &dest.path, &files, ConflictPolicy::Overwrite)
                .await
                .unwrap_err();
            assert!(matches!(error, TransferError::NotEnoughSpace { .. }));
            assert_eq!(std::fs::read_dir(&dest.path).unwrap().count(), 0);
            assert!(!receiver.is_active("space"));
        });
    }

    /// A transfer that fits must still start; the check must not be a
    /// blanket refusal.
    #[test]
    fn transfer_that_fits_is_accepted() {
        run_local(async {
            let source = Scratch::new("fits-src");
            let dest = Scratch::new("fits-dst");
            std::fs::write(source.join("small.txt"), b"tiny").unwrap();
            let prepared = prepare_transfer(&source.join("small.txt")).await.unwrap();
            let offered = offered_files(&prepared).unwrap();
            let mut receiver = TransferReceiver::new();
            receiver
                .begin("fits", &dest.path, &offered, ConflictPolicy::Overwrite)
                .await
                .unwrap();
            assert!(receiver.is_active("fits"));
        });
    }

    #[test]
    fn offer_requires_a_well_formed_digest_for_files() {
        let missing = OfferedFile {
            path: "a.txt".into(),
            size: 3,
            is_dir: false,
            sha256: None,
        };
        assert!(matches!(
            missing.validate(),
            Err(ProtocolError::InvalidDigest(_))
        ));
        let short = OfferedFile {
            sha256: Some("abc".into()),
            ..missing
        };
        assert!(matches!(
            short.validate(),
            Err(ProtocolError::InvalidDigest(_))
        ));
        let uppercase = OfferedFile {
            path: "a.txt".into(),
            size: 3,
            is_dir: false,
            sha256: Some("A".repeat(64)),
        };
        assert!(matches!(
            uppercase.validate(),
            Err(ProtocolError::InvalidDigest(_))
        ));
        let directory = OfferedFile {
            path: "dir".into(),
            size: 0,
            is_dir: true,
            sha256: None,
        };
        assert!(directory.validate().is_ok());
    }
}
