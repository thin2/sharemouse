use futures::{Stream, StreamExt, stream::SelectAll};
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    io::ErrorKind,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::TcpListener;

use crate::{FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError};

pub struct AsyncFrontendListener {
    #[cfg(windows)]
    listener: TcpListener,
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    line_streams: SelectAll<RequestStream>,
    tx_streams: Vec<FrontendWriter>,
    next_id: u64,
}

type RequestStream = Pin<Box<dyn Stream<Item = (u64, Option<std::io::Result<String>>)> + Send>>;
const QUEUE_FRAMES: usize = 1024;
const QUEUE_BYTES: usize = 128 * 1024 * 1024;
struct QueuedFrame {
    bytes: Arc<[u8]>,
    _budget: OwnedSemaphorePermit,
}
struct FrontendWriter {
    id: u64,
    gui: bool,
    sender: mpsc::Sender<QueuedFrame>,
    budget: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FrontendWriter {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl FrontendWriter {
    fn new(
        id: u64,
        mut writer: impl AsyncWrite + Unpin + Send + 'static,
    ) -> (Self, oneshot::Receiver<()>) {
        let (sender, mut receiver) = mpsc::channel::<QueuedFrame>(QUEUE_FRAMES);
        let (closed, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _closed = closed;
            while let Some(frame) = receiver.recv().await {
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(10), writer.write_all(&frame.bytes))
                        .await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
        });
        (
            Self {
                id,
                gui: false,
                sender,
                budget: Arc::new(Semaphore::new(QUEUE_BYTES)),
                task,
            },
            cancelled,
        )
    }

    fn enqueue(&self, bytes: Arc<[u8]>) -> bool {
        let Ok(length) = u32::try_from(bytes.len()) else {
            return false;
        };
        let Ok(permit) = self.budget.clone().try_acquire_many_owned(length) else {
            return false;
        };
        self.sender
            .try_send(QueuedFrame {
                bytes,
                _budget: permit,
            })
            .is_ok()
    }
}

impl AsyncFrontendListener {
    pub async fn new() -> Result<Self, IpcListenerCreationError> {
        #[cfg(unix)]
        let (socket_path, listener) = {
            let socket_path = crate::default_socket_path()?;

            log::debug!("remove socket: {socket_path:?}");
            if socket_path.exists() {
                // try to connect to see if some other instance
                // of lan-mouse is already running
                match UnixStream::connect(&socket_path).await {
                    // connected -> lan-mouse is already running
                    Ok(_) => return Err(IpcListenerCreationError::AlreadyRunning),
                    // lan-mouse is not running but a socket was left behind
                    Err(e) => {
                        log::debug!("{socket_path:?}: {e} - removing left behind socket");
                        let _ = std::fs::remove_file(&socket_path);
                    }
                }
            }
            let listener = match UnixListener::bind(&socket_path) {
                Ok(ls) => ls,
                // some other lan-mouse instance has bound the socket in the meantime
                Err(e) if e.kind() == ErrorKind::AddrInUse => {
                    return Err(IpcListenerCreationError::AlreadyRunning);
                }
                Err(e) => return Err(IpcListenerCreationError::Bind(e)),
            };
            (socket_path, listener)
        };

        #[cfg(windows)]
        let listener = match TcpListener::bind("127.0.0.1:5252").await {
            Ok(ls) => ls,
            // some other lan-mouse instance has bound the socket in the meantime
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                return Err(IpcListenerCreationError::AlreadyRunning);
            }
            Err(e) => return Err(IpcListenerCreationError::Bind(e)),
        };

        let adapter = Self {
            listener,
            #[cfg(unix)]
            socket_path,
            line_streams: SelectAll::new(),
            tx_streams: vec![],
            next_id: 0,
        };

        Ok(adapter)
    }

    /// Whether any frontend is attached.
    ///
    /// `broadcast` silently drops an event when nobody is listening, so a
    /// one-shot notification that must not be lost has to check first.
    pub fn has_frontends(&self) -> bool {
        !self.tx_streams.is_empty()
    }

    /// Only connections that requested GUI selection delivery count as windows.
    pub fn has_gui_frontends(&self) -> bool {
        self.tx_streams
            .iter()
            .any(|tx| tx.gui && !tx.sender.is_closed())
    }

    pub async fn broadcast(&mut self, notify: FrontendEvent) {
        let gui_only = matches!(notify, FrontendEvent::SelectTarget { .. });
        let mut json = serde_json::to_vec(&notify).expect("serialize frontend event");
        json.push(b'\n');
        let bytes: Arc<[u8]> = json.into();
        // Never await socket capacity in the service dispatcher. An overflowing
        // client is disconnected, never allowed to receive a truncated frame.
        self.tx_streams.retain(|tx| {
            if tx.sender.is_closed() {
                return false;
            }
            if gui_only && !tx.gui {
                return true;
            }
            tx.enqueue(bytes.clone())
        });
    }
}

#[cfg(unix)]
impl Drop for AsyncFrontendListener {
    fn drop(&mut self) {
        log::debug!("remove socket: {:?}", self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Stream for AsyncFrontendListener {
    type Item = Result<FrontendRequest, IpcError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some((id, line))) = self.line_streams.poll_next_unpin(cx) {
            let Some(line) = line else {
                self.tx_streams.retain(|tx| tx.id != id);
                continue;
            };
            // Ignore input from connections evicted for a full writer queue.
            let Some(writer) = self.tx_streams.iter_mut().find(|tx| tx.id == id) else {
                continue;
            };
            let request = line.map_err(IpcError::from).and_then(|line| {
                serde_json::from_str::<FrontendRequest>(&line).map_err(Into::into)
            });
            if matches!(&request, Ok(FrontendRequest::TakePendingSelection)) {
                writer.gui = true;
            }
            return Poll::Ready(Some(request));
        }
        let mut sync = false;
        while let Poll::Ready(Ok((stream, _))) = self.listener.poll_accept(cx) {
            let (rx, tx) = tokio::io::split(stream);
            let buf_reader = BufReader::new(rx);
            let lines = buf_reader.lines();
            let lines = LinesStream::new(lines);
            let id = self.next_id;
            self.next_id += 1;
            let (writer, cancelled) = FrontendWriter::new(id, tx);
            let lines = lines
                .take_until(cancelled)
                .map(move |line| (id, Some(line)))
                .chain(futures::stream::once(async move { (id, None) }));
            self.line_streams.push(Box::pin(lines));
            self.tx_streams.push(writer);
            sync = true;
        }
        if sync {
            Poll::Ready(Some(Ok(FrontendRequest::Sync)))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn large_frames_are_written_completely_in_order() {
        let (socket, mut peer) = tokio::io::duplex(1024);
        let (writer, _closed) = FrontendWriter::new(0, socket);
        let mut expected = Vec::new();
        for index in 0..4 {
            let mut frame = serde_json::to_vec(&FrontendEvent::MessageReceived {
                device: "test".into(),
                message_id: index.to_string(),
                text: "x".repeat(128 * 1024),
            })
            .unwrap();
            frame.push(b'\n');
            expected.extend_from_slice(&frame);
            assert!(writer.enqueue(frame.into()));
        }
        let mut received = vec![0; expected.len()];
        tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn stalled_reader_does_not_block_another_writer_and_queue_is_bounded() {
        let (slow_socket, _slow_peer) = tokio::io::duplex(1);
        let (slow, closed) = FrontendWriter::new(0, slow_socket);
        let frame: Arc<[u8]> = vec![b'x'; 4096].into();
        for _ in 0..QUEUE_FRAMES {
            assert!(slow.enqueue(frame.clone()));
        }
        assert!(!slow.enqueue(frame));
        let (fast_socket, mut fast_peer) = tokio::io::duplex(1);
        let (fast, _fast_closed) = FrontendWriter::new(1, fast_socket);
        assert!(fast.enqueue(Arc::from(&b"ok\n"[..])));
        let mut received = [0; 3];
        tokio::time::timeout(Duration::from_secs(1), fast_peer.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&received, b"ok\n");
        drop(slow);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), closed)
                .await
                .unwrap()
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_connections_are_not_gui_frontends() {
        let path = std::env::temp_dir().join(format!("sm-ipc-test-{}.sock", std::process::id()));
        let mut listener = AsyncFrontendListener {
            listener: UnixListener::bind(&path).unwrap(),
            socket_path: path.clone(),
            line_streams: SelectAll::new(),
            tx_streams: Vec::new(),
            next_id: 0,
        };
        let mut peer = UnixStream::connect(&path).await.unwrap();
        assert_eq!(
            listener.next().await.unwrap().unwrap(),
            FrontendRequest::Sync
        );
        assert!(listener.has_frontends());
        assert!(!listener.has_gui_frontends());
        peer.write_all(b"\"TakePendingSelection\"\n").await.unwrap();
        assert_eq!(
            listener.next().await.unwrap().unwrap(),
            FrontendRequest::TakePendingSelection
        );
        assert!(listener.has_gui_frontends());
        drop(peer);
        let _ = tokio::time::timeout(Duration::from_millis(20), listener.next()).await;
        assert!(!listener.has_gui_frontends());
    }
}
