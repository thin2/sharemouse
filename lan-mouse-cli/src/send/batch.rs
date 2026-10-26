//! Fold several right-click invocations into one transfer.
//!
//! A Windows static shell verb is started once per selected item, so
//! "send these five files" arrives as five short-lived processes. This
//! module spools each invocation's paths to disk and elects exactly one
//! of them to send the whole set.
//!
//! Election is by file name: every process writes
//! `{unix_nanos}-{pid}-{seq}`, and the lowest name owns the batch. That
//! is a total order all processes agree on without a lock, and pid plus
//! a per-process counter keeps two starts within the same nanosecond
//! apart.
//!
//! Paths are written NUL-separated so a file name may contain any byte
//! a path may contain, including newlines.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// One spooled invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub paths: Vec<PathBuf>,
}

/// Directory holding batch entries.
///
/// Inside the user's cache directory rather than a world-writable
/// temp dir: the paths are the user's file names, and on a shared
/// machine another account has no business reading them.
pub fn spool_dir() -> PathBuf {
    #[cfg(unix)]
    let base =
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library").join("Caches"));
    #[cfg(not(unix))]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);

    let base = base.unwrap_or_else(std::env::temp_dir);
    base.join("sharemouse").join("send-batch")
}

/// Spool `paths`, then wait for the rest of the batch to settle.
///
/// Returns the full selection when this invocation owns the batch, and
/// `None` when another one does. The owner deletes the entries after
/// reading them; non-owners leave theirs behind for the owner.
pub fn submit(
    dir: &Path,
    paths: &[PathBuf],
    quiet: Duration,
    max_wait: Duration,
    max_age: Duration,
) -> io::Result<Option<Vec<PathBuf>>> {
    fs::create_dir_all(dir)?;
    let ours = write_entry(dir, paths)?;
    let started = std::time::Instant::now();

    // Wait for a quiet period so the remaining invocations of the same
    // right-click have spooled their paths too.
    let mut last_seen = read_entries(dir)?.len();
    loop {
        std::thread::sleep(quiet);
        let seen = read_entries(dir)?.len();
        if seen == last_seen {
            break;
        }
        last_seen = seen;
        if started.elapsed() >= max_wait {
            break;
        }
    }

    let mut entries = read_entries(dir)?;
    prune_stale(dir, &mut entries, max_age);
    let owner = entries
        .iter()
        .map(|entry| entry.name.as_str())
        .min()
        .map(str::to_owned);
    if owner.as_deref() != Some(ours.name.as_str()) {
        return Ok(None);
    }

    // Collect every path before deleting anything: a delete that fails
    // must not lose a path that has not been read yet.
    let mut all = Vec::new();
    for entry in &entries {
        for path in &entry.paths {
            if !all.contains(path) {
                all.push(path.clone());
            }
        }
    }
    for entry in &entries {
        let _ = fs::remove_file(dir.join(&entry.name));
    }
    Ok(Some(all))
}

/// Write one entry and return what was actually stored.
fn write_entry(dir: &Path, paths: &[PathBuf]) -> io::Result<Entry> {
    let name = entry_name();
    let mut buf = Vec::new();
    for path in paths {
        // A path is an arbitrary byte string; NUL is the one byte it
        // cannot contain, which makes it an unambiguous separator.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            buf.extend_from_slice(path.as_os_str().as_bytes());
        }
        #[cfg(not(unix))]
        buf.extend_from_slice(path.to_string_lossy().as_bytes());
        buf.push(0);
    }
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&buf)?;
        // The owner may read this entry immediately; a half-written
        // file would silently drop a path.
        file.sync_all()?;
    }
    fs::rename(&tmp, dir.join(&name))?;

    let stored = read_entry(&dir.join(&name))?.map(|entry| entry.paths);
    Ok(Entry {
        name,
        paths: stored.unwrap_or_else(|| paths.to_vec()),
    })
}

fn entry_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    // Two starts inside the same nanosecond (possible on a coarse
    // clock) would otherwise write the same name and each read back the
    // other's paths.
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{nanos:020}-{pid:05}-{seq:03}")
}

static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// All readable entries, sorted by name.
pub fn read_entries(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(entries),
        Err(e) => return Err(e),
    };
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".tmp") {
            continue;
        }
        if let Some(entry) = read_entry(&entry.path())? {
            entries.push(Entry { name, ..entry });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn read_entry(path: &Path) -> io::Result<Option<Entry>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        // Another invocation deleted it between listing and reading.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut paths = Vec::new();
    for chunk in bytes.split(|b| *b == 0) {
        if chunk.is_empty() {
            continue;
        }
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStrExt;
            PathBuf::from(std::ffi::OsStr::from_bytes(chunk))
        };
        #[cfg(not(unix))]
        let path = PathBuf::from(String::from_utf8_lossy(chunk).into_owned());
        paths.push(path);
    }
    Ok(Some(Entry {
        name: String::new(),
        paths,
    }))
}

/// Drop entries abandoned by a crashed invocation.
fn prune_stale(dir: &Path, entries: &mut Vec<Entry>, max_age: Duration) {
    let keep: Vec<Entry> = entries
        .iter()
        .filter(|entry| entry_name_age(&entry.name).is_some_and(|age| age <= max_age))
        .cloned()
        .collect();
    let stale: Vec<&Entry> = entries
        .iter()
        .filter(|entry| !keep.iter().any(|k| k.name == entry.name))
        .collect();
    for entry in stale {
        let _ = fs::remove_file(dir.join(&entry.name));
    }
    *entries = keep;
}

/// Age encoded in an entry name, or `None` if the name is not ours.
fn entry_name_age(name: &str) -> Option<Duration> {
    let nanos: u128 = name.split('-').next()?.parse().ok()?;
    let created = UNIX_EPOCH.checked_add(Duration::from_nanos(nanos.try_into().ok()?))?;
    SystemTime::now().duration_since(created).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "sharemouse-batch-{tag}-{nanos}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const QUIET: Duration = Duration::from_millis(10);
    const MAX_WAIT: Duration = Duration::from_millis(200);
    const BATCH_AGE: Duration = Duration::from_secs(60);

    #[test]
    fn a_single_invocation_sends_its_own_paths() {
        let dir = temp_dir("single");
        let paths = vec![PathBuf::from("/tmp/a.txt")];
        let got = submit(&dir, &paths, QUIET, MAX_WAIT, BATCH_AGE).unwrap();
        assert_eq!(got, Some(paths));
        // The owner clears its entry so a repeat click starts clean.
        assert!(read_entries(&dir).unwrap().is_empty());
    }

    #[test]
    fn paths_with_spaces_and_unicode_survive_the_spool() {
        let dir = temp_dir("unicode");
        let paths = vec![
            PathBuf::from("/tmp/子 目录/テ スト.txt"),
            PathBuf::from("/tmp/two words/file name.txt"),
        ];
        let got = submit(&dir, &paths, QUIET, MAX_WAIT, BATCH_AGE).unwrap();
        assert_eq!(got, Some(paths));
    }

    #[test]
    fn a_path_containing_a_newline_is_not_split() {
        let dir = temp_dir("newline");
        let paths = vec![PathBuf::from("/tmp/weird\nname.txt")];
        let got = submit(&dir, &paths, QUIET, MAX_WAIT, BATCH_AGE).unwrap();
        assert_eq!(got, Some(paths));
    }

    #[test]
    fn only_the_lowest_named_entry_owns_the_batch() {
        let dir = temp_dir("owner");
        // Simulate three shell invocations that all spooled before any
        // of them looked for an owner.
        let batch = [
            ("00000000000000000001-00001-000", b"/tmp/first\0".to_vec()),
            ("00000000000000000002-00002-000", b"/tmp/second\0".to_vec()),
            ("00000000000000000003-00003-000", b"/tmp/third\0".to_vec()),
        ];
        for (name, contents) in &batch {
            fs::write(dir.join(name), contents).unwrap();
        }

        let entries = read_entries(&dir).unwrap();
        assert_eq!(entries.len(), 3);
        let owner = entries.iter().map(|e| e.name.as_str()).min().unwrap();
        assert_eq!(owner, batch[0].0);

        // The owner reads every path, not just its own.
        let all: Vec<PathBuf> = entries.iter().flat_map(|e| e.paths.clone()).collect();
        assert_eq!(
            all,
            vec![
                PathBuf::from("/tmp/first"),
                PathBuf::from("/tmp/second"),
                PathBuf::from("/tmp/third"),
            ]
        );
    }

    #[test]
    fn paths_appearing_twice_are_sent_once() {
        let dir = temp_dir("dupes");
        fs::write(
            dir.join("00000000000000000001-00001-000"),
            b"/tmp/a\0/tmp/a\0",
        )
        .unwrap();
        fs::write(
            dir.join("00000000000000000002-00002-000"),
            b"/tmp/a\0/tmp/b\0",
        )
        .unwrap();
        let entries = read_entries(&dir).unwrap();
        let mut all: Vec<PathBuf> = Vec::new();
        for entry in &entries {
            for path in &entry.paths {
                if !all.contains(path) {
                    all.push(path.clone());
                }
            }
        }
        assert_eq!(all, vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]);
    }

    #[test]
    fn half_written_temp_files_are_ignored() {
        let dir = temp_dir("tmp");
        fs::write(
            dir.join("00000000000000000001-00001-000.tmp"),
            b"/tmp/partial\0",
        )
        .unwrap();
        assert!(read_entries(&dir).unwrap().is_empty());
    }

    #[test]
    fn stale_entries_from_a_crashed_invocation_are_dropped() {
        let dir = temp_dir("stale");
        // Encoded time far in the past: 1970.
        let stale = "00000000000000000001-00001-000";
        fs::write(dir.join(stale), b"/tmp/old\0").unwrap();
        let mut entries = read_entries(&dir).unwrap();
        prune_stale(&dir, &mut entries, Duration::from_secs(1));
        assert!(entries.is_empty());
        assert!(!dir.join(stale).exists());
    }

    #[test]
    fn a_missing_spool_directory_reads_as_empty() {
        let dir = temp_dir("missing").join("nested");
        assert!(read_entries(&dir).unwrap().is_empty());
    }

    #[test]
    fn entry_names_parse_back_to_a_recent_age() {
        let name = entry_name();
        let age = entry_name_age(&name).expect("own name parses");
        assert!(age < Duration::from_secs(5));
    }

    #[test]
    fn foreign_file_names_are_not_treated_as_entries() {
        assert_eq!(entry_name_age("notes.txt"), None);
        assert_eq!(entry_name_age(""), None);
    }

    #[test]
    fn two_starts_in_the_same_nanosecond_get_different_names() {
        let a = entry_name();
        let b = entry_name();
        assert_ne!(a, b);
    }

    #[test]
    fn spool_lives_outside_the_shared_temp_directory() {
        // The spool holds the user's file names, which should not land
        // in a world-readable location.
        let dir = spool_dir();
        assert!(!dir.starts_with(std::env::temp_dir()));
    }
}
