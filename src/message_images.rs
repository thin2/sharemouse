//! Temporary previews for received image messages. Never reads or records clipboard history.
use crate::clipboard_native::NativeClipboard;
use lan_mouse_data::clipboard_image::Image;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary = path.with_extension(format!("part-{}-{nonce}", std::process::id()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&temporary)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(path.parent().ok_or_else(invalid)?)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
fn partial_stem(name: &str) -> Option<&str> {
    let (stem, suffix) = name.split_once(".part-")?;
    let (pid, nonce) = suffix.split_once('-')?;
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (numeric(pid) && numeric(nonce)).then_some(stem)
}
fn clean_messages(run: &Path) -> io::Result<()> {
    if !run.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(run)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid());
    }
    for e in fs::read_dir(run)? {
        let p = e?.path();
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let stem = name
            .strip_suffix("-thumb.png")
            .or_else(|| name.strip_suffix(".png"))
            .or_else(|| name.strip_suffix(".img"))
            .or_else(|| partial_stem(name).map(|s| s.strip_suffix("-thumb").unwrap_or(s)));
        if stem.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) {
            let m = fs::symlink_metadata(&p)?;
            if m.is_file() || m.file_type().is_symlink() {
                fs::remove_file(p)?;
            }
        }
    }
    Ok(())
}

pub(crate) enum Command {
    Message {
        device: String,
        generation: u64,
        id: String,
        image: Image,
        epoch: u64,
    },
    ClearMessages,
}
pub(crate) enum Event {
    Error,
    Message {
        device: String,
        generation: u64,
        id: String,
        filename: String,
        preview: PathBuf,
        thumbnail: PathBuf,
        epoch: u64,
    },
}
pub(crate) struct Worker {
    pub tx: mpsc::Sender<Command>,
    pub events: mpsc::Receiver<Event>,
    job: Option<tokio::task::JoinHandle<()>>,
}
impl Worker {
    pub fn start(run: PathBuf, assets: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::channel(16);
        let (events_tx, events) = mpsc::channel(32);
        let job = tokio::task::spawn_blocking(move || {
            let _ = clean_messages(&run);
            let mut message_bytes = 0_usize;
            let mut message_count = 0_u64;
            let _ = NativeClipboard::collect_assets(&assets);
            while let Some(command) = rx.blocking_recv() {
                let result = (|| -> io::Result<Option<Event>> {
                    match command {
                        Command::ClearMessages => {
                            clean_messages(&run)?;
                            message_bytes = 0;
                            Ok(None)
                        }
                        Command::Message {
                            device,
                            generation,
                            id,
                            image,
                            epoch,
                        } => {
                            private_dir(&run)?;
                            let (preview, thumb) = NativeClipboard::thumbnail(&image)?;
                            let encoded = image.encode();
                            if message_bytes + preview.len() + thumb.len() + encoded.len()
                                > 128 * 1024 * 1024
                            {
                                return Err(invalid());
                            }
                            message_count += 1;
                            if message_count > 500 {
                                for suffix in [".png", "-thumb.png", ".img"] {
                                    let old = run.join(format!("{}{suffix}", message_count - 500));
                                    if let Ok(m) = fs::symlink_metadata(&old) {
                                        if m.is_file() {
                                            message_bytes =
                                                message_bytes.saturating_sub(m.len() as usize);
                                            fs::remove_file(old)?;
                                        }
                                    }
                                }
                            }
                            let path = run.join(format!("{message_count}.png"));
                            let thumbnail = run.join(format!("{message_count}-thumb.png"));
                            let original = run.join(format!("{message_count}.img"));
                            let saved = (|| {
                                atomic(&path, &preview)?;
                                atomic(&thumbnail, &thumb)?;
                                atomic(&original, &encoded)
                            })();
                            if let Err(error) = saved {
                                for file in [&path, &thumbnail, &original] {
                                    let _ = fs::remove_file(file);
                                }
                                return Err(error);
                            }
                            message_bytes += preview.len() + thumb.len() + encoded.len();
                            Ok(Some(Event::Message {
                                device,
                                generation,
                                id,
                                filename: image.filename,
                                preview: path,
                                thumbnail,
                                epoch,
                            }))
                        }
                    }
                })();
                let event = match result {
                    Ok(None) => continue,
                    Ok(Some(event)) => event,
                    Err(_) => Event::Error,
                };
                if events_tx.blocking_send(event).is_err() {
                    break;
                }
            }
            let _ = clean_messages(&run);
        });
        Self {
            tx,
            events,
            job: Some(job),
        }
    }
    pub async fn stop(&mut self) {
        let (dummy, _) = mpsc::channel(1);
        drop(std::mem::replace(&mut self.tx, dummy));
        if let Some(mut job) = self.job.take() {
            loop {
                tokio::select! {
                    _=&mut job=>break,
                    Some(_)=self.events.recv()=>{},
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_and_clear_do_not_touch_legacy_history() {
        let root = std::env::temp_dir().join(format!(
            "sm-message-images-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let legacy = root.join("clipboard-history");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("index.json"), b"legacy history").unwrap();
        let run = root.join("message-images");
        private_dir(&run).unwrap();
        fs::write(run.join("1.png"), b"old preview").unwrap();
        fs::write(run.join("keep.txt"), b"unrelated").unwrap();
        let mut worker = Worker::start(run.clone(), root.join("assets"));
        worker.tx.send(Command::ClearMessages).await.unwrap();
        worker.stop().await;
        assert_eq!(fs::read(legacy.join("index.json")).unwrap(), b"legacy history");
        assert!(!run.join("1.png").exists());
        assert!(run.join("keep.txt").exists());
        assert!(!run.join("index.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
