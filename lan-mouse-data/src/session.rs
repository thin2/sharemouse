//! Per-device data sessions.
//!
//! The keypad/mouse channel connects lazily when the pointer enters a
//! screen edge. Files, messages and clipboard content must work while
//! sharing is stopped, so this module keeps one outbound and any number
//! of inbound connections per authorized device.
//!
//! Outbound connections are created on demand and dropped when the
//! peer closes them. Inbound connections are accepted by the service
//! and registered here. A device that reconnects replaces its old
//! session so stale writers cannot keep sending.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::{
    crypto::{AuthorizedFingerprints, Identity},
    protocol::{DataMessage, Frame, ProtocolError},
    transport::{self, DataConnection, TransportError},
};

/// Identifier used by the UI and the IPC layer.
pub type DeviceId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    ImageMessage {
        device: DeviceId,
        generation: u64,
        part: crate::image_message::Part,
    },
    Clipboard {
        device: DeviceId,
        generation: u64,
        message: crate::clipboard_sync::Message,
    },
    /// Session controls carry their connection generation so queued events
    /// from a replaced TLS connection cannot reopen an old input session.
    Sharing {
        device: DeviceId,
        generation: u64,
        message: crate::sharing::Message,
    },
    /// A device completed the authenticated handshake.
    Connected { device: DeviceId, addr: SocketAddr },
    /// A device closed its connection or failed a later read/write.
    Disconnected { device: DeviceId, reason: String },
    /// A control message arrived from a device.
    Message {
        device: DeviceId,
        generation: u64,
        message: DataMessage,
    },
    /// A file chunk arrived for an in-progress transfer.
    Chunk {
        device: DeviceId,
        generation: u64,
        chunk: crate::protocol::FileChunk,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("no address known for device `{0}`")]
    UnknownDevice(DeviceId),
    #[error("peer closed the connection")]
    Closed,
    #[error("device `{0}` is not authorized")]
    Unauthorized(DeviceId),
    #[error("peer certificate does not match device `{0}`")]
    IdentityMismatch(DeviceId),
}

struct Outgoing {
    frame: Frame,
    completion: oneshot::Sender<Result<(), TransportError>>,
}

struct Session {
    sender: mpsc::Sender<Outgoing>,
    transfer_replies: broadcast::Sender<DataMessage>,
    addr: SocketAddr,
    task: JoinHandle<()>,
    generation: u64,
    inbound: bool,
}

/// Shared registry of data-channel sessions.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Mutex<HashMap<DeviceId, Session>>>,
    identity: Identity,
    authorized: AuthorizedFingerprints,
    events: mpsc::Sender<SessionEvent>,
    next_id: Arc<AtomicU64>,
    next_generation: Arc<AtomicU64>,
    connecting: Arc<tokio::sync::Mutex<()>>,
}
impl SessionManager {
    pub fn new(
        identity: Identity,
        authorized: AuthorizedFingerprints,
        events: mpsc::Sender<SessionEvent>,
    ) -> Self {
        Self {
            inner: Default::default(),
            identity,
            authorized,
            events,
            next_id: Arc::new(AtomicU64::new(1)),
            next_generation: Arc::new(AtomicU64::new(1)),
            connecting: Default::default(),
        }
    }

    /// Create a unique message id for the message record.
    pub fn next_message_id(&self) -> String {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("m-{id}")
    }

    pub fn connected_devices(&self) -> Vec<DeviceId> {
        self.inner.lock().expect("lock").keys().cloned().collect()
    }

    pub fn is_connected(&self, device: &str) -> bool {
        self.inner.lock().expect("lock").contains_key(device)
    }

    pub fn generation(&self, device: &str) -> Option<u64> {
        self.inner
            .lock()
            .expect("lock")
            .get(device)
            .map(|s| s.generation)
    }

    /// Register an accepted inbound connection. Replaces any previous
    /// session for the same device.
    pub fn register_inbound(
        &self,
        device: DeviceId,
        connection: DataConnection,
        addr: SocketAddr,
    ) -> Result<(), SessionError> {
        self.register(device, connection, addr, true)
    }

    /// Connect to a device's data port, reusing an existing session.
    pub async fn connect(&self, device: DeviceId, addr: SocketAddr) -> Result<(), SessionError> {
        let _connecting = self.connecting.lock().await;
        self.check_authorized(&device)?;
        if self.is_connected(&device) {
            return Ok(());
        }
        let connection = transport::connect(addr, &self.identity, &self.authorized)
            .await
            .map_err(SessionError::Transport)?;
        self.register(device, connection, addr, false)
    }

    fn register(
        &self,
        device: DeviceId,
        connection: DataConnection,
        addr: SocketAddr,
        inbound: bool,
    ) -> Result<(), SessionError> {
        self.check_authorized(&device)?;
        if connection.peer_fingerprint().as_deref() != Some(device.as_str()) {
            return Err(SessionError::IdentityMismatch(device));
        }
        let mut sessions = self.inner.lock().expect("lock");
        if let Some(old) = sessions.get(&device) {
            // Both peers may dial at once. Keep the same physical
            // connection on both ends, rather than each closing the
            // connection the other peer chose.
            let prefer_inbound = self.identity.fingerprint() > device;
            if old.inbound != inbound && inbound != prefer_inbound {
                return Ok(());
            }
        }
        let (sender, receiver) = mpsc::channel(32);
        let (transfer_replies, _) = broadcast::channel(64);
        let replies = transfer_replies.clone();
        let events = self.events.clone();
        let name = device.clone();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let registry = Arc::downgrade(&self.inner);
        let task = tokio::task::spawn_local(async move {
            if events
                .send(SessionEvent::Connected {
                    device: name.clone(),
                    addr,
                })
                .await
                .is_err()
            {
                remove_generation(&registry, &name, generation);
                return;
            }
            let reason = run_session(
                &name,
                generation,
                connection.into_stream(),
                receiver,
                &events,
                &replies,
            )
            .await;
            if remove_generation(&registry, &name, generation) {
                let _ = events
                    .send(SessionEvent::Disconnected {
                        device: name,
                        reason,
                    })
                    .await;
            }
        });

        if let Some(old) = sessions.insert(
            device.clone(),
            Session {
                sender,
                transfer_replies,
                addr,
                task,
                generation,
                inbound,
            },
        ) {
            old.task.abort();
            log::info!("replaced existing data session for {device}");
        }
        drop(sessions);

        log::info!(
            "data session with {device} @ {addr} established ({})",
            if inbound { "inbound" } else { "outbound" }
        );
        Ok(())
    }

    fn check_authorized(&self, device: &str) -> Result<(), SessionError> {
        if self.authorized.read().expect("lock").contains(device) {
            Ok(())
        } else {
            Err(SessionError::Unauthorized(device.to_owned()))
        }
    }

    /// Send a control message to a connected device.
    pub async fn send(&self, device: &str, message: DataMessage) -> Result<(), SessionError> {
        message.validate()?;
        self.send_frame(device, Frame::Control(message)).await
    }

    async fn send_frame(&self, device: &str, frame: Frame) -> Result<(), SessionError> {
        self.send_frame_at(device, None, frame).await
    }

    /// Subscribe before offering a transfer. Replies belong to this exact TLS session.
    pub fn transfer_replies(
        &self,
        device: &str,
    ) -> Result<(u64, broadcast::Receiver<DataMessage>), SessionError> {
        self.check_authorized(device)?;
        let sessions = self.inner.lock().expect("lock");
        let session = sessions.get(device).ok_or(SessionError::Closed)?;
        Ok((session.generation, session.transfer_replies.subscribe()))
    }

    pub async fn send_at(
        &self,
        device: &str,
        generation: u64,
        message: DataMessage,
    ) -> Result<(), SessionError> {
        message.validate()?;
        self.send_frame_at(device, Some(generation), Frame::Control(message))
            .await
    }

    pub async fn send_chunk_at(
        &self,
        device: &str,
        generation: u64,
        chunk: crate::protocol::FileChunk,
    ) -> Result<(), SessionError> {
        self.send_frame_at(device, Some(generation), Frame::Chunk(chunk))
            .await
    }

    async fn send_frame_at(
        &self,
        device: &str,
        generation: Option<u64>,
        frame: Frame,
    ) -> Result<(), SessionError> {
        self.send_frame_with_timeout(
            device,
            generation,
            frame,
            std::time::Duration::from_secs(30),
        )
        .await
    }

    async fn send_frame_with_timeout(
        &self,
        device: &str,
        generation: Option<u64>,
        frame: Frame,
        timeout: std::time::Duration,
    ) -> Result<(), SessionError> {
        self.check_authorized(device)?;
        let (sender, selected_generation) = {
            let sessions = self.inner.lock().expect("lock");
            sessions
                .get(device)
                .filter(|session| generation.is_none_or(|g| session.generation == g))
                .map(|session| (session.sender.clone(), session.generation))
                .ok_or_else(|| SessionError::UnknownDevice(device.to_owned()))?
        };
        let (completion, result) = oneshot::channel();
        let write = async {
            sender
                .send(Outgoing { frame, completion })
                .await
                .map_err(|_| SessionError::Closed)?;
            result.await.map_err(|_| SessionError::Closed)??;
            Ok(())
        };
        match tokio::time::timeout(timeout, write).await {
            Ok(result) => result,
            Err(_) => {
                let removed = {
                    let mut sessions = self.inner.lock().expect("lock");
                    if sessions
                        .get(device)
                        .is_some_and(|s| s.generation == selected_generation)
                    {
                        sessions.remove(device)
                    } else {
                        None
                    }
                };
                if let Some(session) = removed {
                    session.task.abort();
                    let events = self.events.clone();
                    let device = device.to_owned();
                    // Removal claims the one disconnect notification. Deliver it
                    // independently of a cancelled sender or a full event queue.
                    tokio::task::spawn_local(async move {
                        let _ = events
                            .send(SessionEvent::Disconnected {
                                device,
                                reason: "session write timed out".into(),
                            })
                            .await;
                    });
                }
                Err(SessionError::Closed)
            }
        }
    }

    /// Send a chunk for a transfer.
    pub async fn send_chunk(
        &self,
        device: &str,
        chunk: crate::protocol::FileChunk,
    ) -> Result<(), SessionError> {
        self.send_frame(device, Frame::Chunk(chunk)).await
    }

    pub fn addr(&self, device: &str) -> Option<SocketAddr> {
        self.inner
            .lock()
            .expect("lock")
            .get(device)
            .map(|session| session.addr)
    }

    /// Drop a session, aborting its reader. Used on revocation and on
    /// shutdown.
    pub fn remove(&self, device: &str) {
        if let Some(session) = self.inner.lock().expect("lock").remove(device) {
            session.task.abort();
        }
    }

    /// Close live sessions whose authorization was removed, including
    /// when the authorization configuration is reloaded.
    pub fn remove_unauthorized(&self) -> Vec<DeviceId> {
        let allowed = self.authorized.read().expect("lock");
        let mut sessions = self.inner.lock().expect("lock");
        let revoked: Vec<_> = sessions
            .keys()
            .filter(|d| !allowed.contains(*d))
            .cloned()
            .collect();
        for device in &revoked {
            if let Some(session) = sessions.remove(device) {
                session.task.abort();
            }
        }
        revoked
    }

    pub fn terminate(&self) {
        let mut sessions = self.inner.lock().expect("lock");
        for (_, session) in sessions.drain() {
            session.task.abort();
        }
    }
}

fn remove_generation(
    registry: &Weak<Mutex<HashMap<DeviceId, Session>>>,
    device: &str,
    generation: u64,
) -> bool {
    let Some(registry) = registry.upgrade() else {
        return false;
    };
    let mut sessions = registry.lock().expect("lock");
    if sessions
        .get(device)
        .is_some_and(|s| s.generation == generation)
    {
        sessions.remove(device);
        true
    } else {
        false
    }
}

async fn run_session(
    device: &str,
    generation: u64,
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    mut outgoing: mpsc::Receiver<Outgoing>,
    events: &mpsc::Sender<SessionEvent>,
    transfer_replies: &broadcast::Sender<DataMessage>,
) -> String {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let read = async {
        loop {
            let payload = transport::read_payload(&mut reader).await?;
            match Frame::decode(&payload)? {
                Frame::Control(DataMessage::ImageMessage { part }) => events
                    .send(SessionEvent::ImageMessage {
                        device: device.into(),
                        generation,
                        part,
                    })
                    .await
                    .map_err(|_| TransportError::Rejected)?,
                Frame::Control(DataMessage::Clipboard { message }) => events
                    .send(SessionEvent::Clipboard {
                        device: device.into(),
                        generation,
                        message,
                    })
                    .await
                    .map_err(|_| TransportError::Rejected)?,
                Frame::Control(DataMessage::Sharing { message }) => events
                    .send(SessionEvent::Sharing {
                        device: device.into(),
                        generation,
                        message,
                    })
                    .await
                    .map_err(|_| TransportError::Rejected)?,
                Frame::Control(message) => {
                    if matches!(
                        &message,
                        DataMessage::FileAccepted { .. }
                            | DataMessage::FileCommitted { .. }
                            | DataMessage::FileRejected { .. }
                    ) {
                        let _ = transfer_replies.send(message.clone());
                    }
                    events
                        .send(SessionEvent::Message {
                            device: device.to_owned(),
                            generation,
                            message,
                        })
                        .await
                        .map_err(|_| TransportError::Rejected)?;
                }
                // Chunks are routed to the transfer receiver by the
                // service; dropping them here would silently truncate
                // any file transfer.
                Frame::Chunk(chunk) => events
                    .send(SessionEvent::Chunk {
                        device: device.to_owned(),
                        generation,
                        chunk,
                    })
                    .await
                    .map_err(|_| TransportError::Rejected)?,
            }
        }
    };
    let write = async {
        while let Some(Outgoing { frame, completion }) = outgoing.recv().await {
            let result = match frame.encode() {
                Ok(payload) => transport::write_payload(&mut writer, &payload).await,
                Err(e) => Err(e.into()),
            };
            let failed = result.is_err();
            let _ = completion.send(result);
            if failed {
                return Err(TransportError::Rejected);
            }
        }
        Ok(())
    };
    // Select entire loops: an outgoing frame never cancels a partial
    // read. Only termination drops the other half of this connection.
    let result: Result<(), TransportError> = tokio::select! {
        result = read => result,
        result = write => result,
    };
    result
        .err()
        .map_or_else(|| "session closed".to_owned(), |e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{AuthorizedFingerprints, Identity};
    use crate::protocol::{DataMessage, PROTOCOL_VERSION};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::collections::HashSet;
    use std::sync::RwLock;

    fn identity() -> Identity {
        let params = rcgen::CertificateParams::new(vec!["session-test".into()]).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let key = PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(
            key.serialize_der(),
        ));
        Identity {
            certificate: CertificateDer::from(cert.der().to_vec()),
            private_key: key.secret_der().to_vec(),
        }
    }

    fn allow(identity: &Identity) -> AuthorizedFingerprints {
        Arc::new(RwLock::new(HashSet::from([
            crate::certificate_fingerprint(&identity.certificate),
        ])))
    }

    #[test]
    fn unique_message_ids() {
        let identity = identity();
        let (tx, _rx) = mpsc::channel(1);
        let manager = SessionManager::new(identity.clone(), allow(&identity), tx);
        let a = manager.next_message_id();
        let b = manager.next_message_id();
        assert_ne!(a, b);
    }

    fn stalled_session() -> (
        SessionManager,
        String,
        mpsc::Receiver<SessionEvent>,
        tokio::task::AbortHandle,
    ) {
        let identity = identity();
        let device = identity.fingerprint();
        let (events, received) = mpsc::channel(1);
        let manager = SessionManager::new(identity.clone(), allow(&identity), events);
        let (sender, receiver) = mpsc::channel(32);
        let (stream, peer) = tokio::io::duplex(1);
        let task_events = manager.events.clone();
        let task_device = device.clone();
        let (transfer_replies, _) = broadcast::channel(64);
        let replies = transfer_replies.clone();
        let task = tokio::task::spawn_local(async move {
            // Keeping the peer alive without reading forces a real partial
            // frame write to stall rather than faking a missing completion.
            let _peer = peer;
            run_session(&task_device, 1, stream, receiver, &task_events, &replies).await;
        });
        let abort = task.abort_handle();
        manager.inner.lock().unwrap().insert(
            device.clone(),
            Session {
                sender,
                transfer_replies,
                addr: "127.0.0.1:1".parse().unwrap(),
                task,
                generation: 1,
                inbound: true,
            },
        );
        (manager, device, received, abort)
    }

    #[test]
    fn stalled_writes_emit_one_disconnect_even_with_a_full_event_queue() {
        run_local(async {
            let (manager, device, mut events, task) = stalled_session();
            manager
                .events
                .send(SessionEvent::Connected {
                    device: device.clone(),
                    addr: "127.0.0.1:1".parse().unwrap(),
                })
                .await
                .unwrap();
            let frame = Frame::Control(test_message("stalled"));
            let timeout = std::time::Duration::from_millis(10);
            let (a, b) = tokio::join!(
                manager.send_frame_with_timeout(&device, Some(1), frame.clone(), timeout),
                manager.send_frame_with_timeout(&device, Some(1), frame, timeout),
            );
            assert!(matches!(a, Err(SessionError::Closed)));
            assert!(matches!(b, Err(SessionError::Closed)));
            assert!(!manager.is_connected(&device));
            assert!(matches!(
                events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));
            assert_eq!(
                events.recv().await,
                Some(SessionEvent::Disconnected {
                    device,
                    reason: "session write timed out".into(),
                })
            );
            tokio::task::yield_now().await;
            assert!(task.is_finished());
            assert!(
                events.try_recv().is_err(),
                "only one sender claims disconnect"
            );
        });
    }

    #[test]
    fn timed_out_old_write_does_not_disconnect_a_replacement_generation() {
        run_local(async {
            let (manager, device, mut events, task) = stalled_session();
            let frame = Frame::Control(test_message("stalled"));
            let send = manager.send_frame_with_timeout(
                &device,
                Some(1),
                frame,
                std::time::Duration::from_millis(10),
            );
            let replace = async {
                tokio::task::yield_now().await;
                manager
                    .inner
                    .lock()
                    .unwrap()
                    .get_mut(&device)
                    .unwrap()
                    .generation = 2;
            };
            let (result, ()) = tokio::join!(send, replace);
            assert!(matches!(result, Err(SessionError::Closed)));
            assert_eq!(manager.generation(&device), Some(2));
            assert!(!task.is_finished());
            assert!(events.try_recv().is_err());
            manager.terminate();
        });
    }

    #[test]
    fn sending_without_session_fails() {
        let identity = identity();
        let (tx, _rx) = mpsc::channel(1);
        let manager = SessionManager::new(identity.clone(), allow(&identity), tx);
        assert!(!manager.is_connected("device-a"));
        assert!(manager.addr("device-a").is_none());
    }

    #[test]
    fn enrollment_handshake_requires_version_seven() {
        // Guard against accidentally bumping the version without a
        // migration path in the hello exchange.
        assert_eq!(PROTOCOL_VERSION, 7);
        assert_eq!(
            DataMessage::Hello { version: 4 }.validate(),
            Err(ProtocolError::UnsupportedVersion {
                got: 4,
                expected: PROTOCOL_VERSION
            })
        );
    }

    async fn pair() -> (
        SessionManager,
        SessionManager,
        mpsc::Receiver<SessionEvent>,
        mpsc::Receiver<SessionEvent>,
        std::net::SocketAddr,
    ) {
        let alice = identity();
        let bob = identity();
        let alice_fp = crate::certificate_fingerprint(&alice.certificate);
        let bob_fp = crate::certificate_fingerprint(&bob.certificate);
        let alice_allowed: AuthorizedFingerprints =
            Arc::new(RwLock::new(HashSet::from([bob_fp.clone()])));
        let bob_allowed: AuthorizedFingerprints =
            Arc::new(RwLock::new(HashSet::from([alice_fp.clone()])));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (alice_tx, alice_rx) = mpsc::channel(16);
        let (bob_tx, bob_rx) = mpsc::channel(16);
        let alice_sessions = SessionManager::new(alice.clone(), alice_allowed.clone(), alice_tx);
        let bob_sessions = SessionManager::new(bob.clone(), bob_allowed.clone(), bob_tx);

        let accept_sessions = bob_sessions.clone();
        let accept_identity = bob.clone();
        let accept_allowed = bob_allowed.clone();
        tokio::task::spawn_local(async move {
            while let Ok((connection, peer)) =
                crate::transport::accept(&listener, &accept_identity, &accept_allowed).await
            {
                let device = connection
                    .peer_fingerprint()
                    .unwrap_or_else(|| peer.to_string());
                let _ = accept_sessions.register_inbound(device, connection, peer);
            }
        });

        (alice_sessions, bob_sessions, alice_rx, bob_rx, addr)
    }

    #[test]
    fn messages_flow_between_authorized_sessions() {
        run_local(async {
            let (alice, bob, _alice_events, mut bob_events, addr) = pair().await;

            alice.connect(bob_device(&bob), addr).await.unwrap();
            let connected = bob_events.recv().await.unwrap();
            assert!(matches!(connected, SessionEvent::Connected { .. }));

            alice
                .send(
                    &bob_device(&bob),
                    DataMessage::Message {
                        id: "m-1".into(),
                        text: "hello".into(),
                    },
                )
                .await
                .unwrap();

            match bob_events.recv().await.unwrap() {
                SessionEvent::Message { message, .. } => {
                    assert_eq!(
                        message,
                        DataMessage::Message {
                            id: "m-1".into(),
                            text: "hello".into()
                        }
                    );
                }
                other => panic!("unexpected event: {other:?}"),
            }
        });
    }

    #[test]
    fn clipboard_events_carry_authenticated_connection_generation() {
        run_local(async {
            let (alice, bob, _alice_events, mut events, addr) = pair().await;
            alice.connect(bob_device(&bob), addr).await.unwrap();
            assert!(matches!(
                events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));
            let message = crate::clipboard_sync::Message::Hello {
                incarnation: "source-process".into(),
            };
            alice
                .send(
                    &bob_device(&bob),
                    DataMessage::Clipboard {
                        message: message.clone(),
                    },
                )
                .await
                .unwrap();
            let SessionEvent::Clipboard {
                device,
                generation,
                message: received,
            } = events.recv().await.unwrap()
            else {
                panic!("expected generation-bound clipboard event");
            };
            assert_eq!(received, message);
            assert_eq!(bob.generation(&device), Some(generation));
            assert_eq!(device, alice.identity.fingerprint());
        });
    }

    #[test]
    fn reconnect_creates_a_fresh_session() {
        run_local(async {
            let (alice, bob, _alice_events, mut bob_events, addr) = pair().await;
            let device = bob_device(&bob);

            alice.connect(device.clone(), addr).await.unwrap();
            assert!(matches!(
                bob_events.recv().await.unwrap(),
                SessionEvent::Connected { .. }
            ));
            alice.remove(&device);
            assert!(!alice.is_connected(&device));

            alice.connect(device, addr).await.unwrap();
            // Removing the old local session may also produce a
            // Disconnected event for the peer; wait for the new connect.
            let mut connected = false;
            while let Some(event) = bob_events.recv().await {
                if matches!(event, SessionEvent::Connected { .. }) {
                    connected = true;
                    break;
                }
            }
            assert!(connected, "reconnect must register a new session");
        });
    }

    #[test]
    fn session_is_not_connected_after_revocation() {
        run_local(async {
            let (alice, bob, _alice_events, mut bob_events, addr) = pair().await;
            let device = bob_device(&bob);
            alice.connect(device.clone(), addr).await.unwrap();
            assert!(matches!(
                bob_events.recv().await.unwrap(),
                SessionEvent::Connected { .. }
            ));

            // Use the same reconciliation as service revocation and
            // config reload, without manually removing a session.
            alice.authorized.write().unwrap().clear();
            assert_eq!(alice.remove_unauthorized(), vec![device.clone()]);
            assert!(!alice.is_connected(&device));
            assert!(alice.connect(device, addr).await.is_err());
        });
    }

    #[test]
    fn transfer_replies_and_writes_are_fenced_by_connection_generation() {
        run_local(async {
            let (alice, bob, mut alice_events, mut bob_events, addr) = pair().await;
            let device = bob_device(&bob);
            alice.connect(device.clone(), addr).await.unwrap();
            alice_events.recv().await.unwrap();
            bob_events.recv().await.unwrap();
            let (old_generation, mut replies) = alice.transfer_replies(&device).unwrap();
            alice.remove(&device);
            assert!(replies.recv().await.is_err());
            assert!(
                alice
                    .send_at(&device, old_generation, test_message("stale"))
                    .await
                    .is_err()
            );
            // The new connection must not receive work queued for the old one.
            alice.connect(device.clone(), addr).await.unwrap();
            assert_ne!(alice.generation(&device), Some(old_generation));
            assert!(
                alice
                    .send_at(&device, old_generation, test_message("stale"))
                    .await
                    .is_err()
            );
            alice.terminate();
            bob.terminate();
        });
    }

    #[test]
    fn remote_disconnect_cleans_registry_and_allows_reconnect() {
        run_local(async {
            let (alice, bob, mut alice_events, mut bob_events, addr) = pair().await;
            let device = bob_device(&bob);
            alice.connect(device.clone(), addr).await.unwrap();
            assert!(matches!(
                alice_events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));
            assert!(matches!(
                bob_events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));
            bob.remove(&alice.identity.fingerprint());
            assert!(matches!(
                alice_events.recv().await,
                Some(SessionEvent::Disconnected { .. })
            ));
            assert!(!alice.is_connected(&device));
            assert!(alice.connected_devices().is_empty());
            assert_eq!(alice.addr(&device), None);
            alice.connect(device.clone(), addr).await.unwrap();
            assert!(alice.is_connected(&device));
            assert!(matches!(
                bob_events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));
        });
    }

    #[test]
    fn rejects_another_authorized_device_at_the_requested_address() {
        run_local(async {
            let (alice, bob, _alice_events, _bob_events, addr) = pair().await;
            let expected = identity().fingerprint();
            alice.authorized.write().unwrap().insert(expected.clone());
            let result = alice.connect(expected.clone(), addr).await;
            assert!(
                matches!(result, Err(SessionError::IdentityMismatch(device)) if device == expected)
            );
            assert!(!alice.is_connected(&expected));
            assert!(!alice.is_connected(&bob_device(&bob)));
        });
    }

    #[test]
    fn concurrent_connects_reuse_one_session() {
        run_local(async {
            let (alice, bob, _alice_events, mut bob_events, addr) = pair().await;
            let device = bob_device(&bob);
            let (a, b) = tokio::join!(
                alice.connect(device.clone(), addr),
                alice.connect(device.clone(), addr)
            );
            a.unwrap();
            b.unwrap();
            assert!(matches!(
                bob_events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));
            alice.send(&device, test_message("hello")).await.unwrap();
            assert!(matches!(
                bob_events.recv().await,
                Some(SessionEvent::Message { .. })
            ));
        });
    }

    fn test_message(text: &str) -> DataMessage {
        DataMessage::Message {
            id: "m-test".into(),
            text: text.into(),
        }
    }

    #[test]
    fn outgoing_frames_preserve_partial_incoming_frames() {
        run_local(async {
            use tokio::io::AsyncWriteExt;
            // Test both an incomplete length prefix and an incomplete
            // payload. The peer waits for the reply before finishing it.
            for split in [2, 7] {
                let (stream, mut peer) = tokio::io::duplex(4096);
                let (tx, rx) = mpsc::channel(32);
                let (events, mut received) = mpsc::channel(16);
                let task = tokio::task::spawn_local(async move {
                    run_session("peer", 1, stream, rx, &events, &broadcast::channel(64).0).await
                });
                let message = test_message("fragmented");
                let payload = Frame::Control(message.clone()).encode().unwrap();
                let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
                bytes.extend(payload);
                peer.write_all(&bytes[..split]).await.unwrap();
                tokio::task::yield_now().await;
                let reply = Frame::Control(test_message("reply"));
                let (completion, result) = oneshot::channel();
                tx.send(Outgoing {
                    frame: reply.clone(),
                    completion,
                })
                .await
                .unwrap();
                assert_eq!(
                    Frame::decode(&transport::read_payload(&mut peer).await.unwrap()).unwrap(),
                    reply
                );
                result.await.unwrap().unwrap();
                peer.write_all(&bytes[split..]).await.unwrap();
                assert_eq!(
                    received.recv().await,
                    Some(SessionEvent::Message {
                        device: "peer".into(),
                        generation: 1,
                        message
                    })
                );
                drop(peer);
                task.await.unwrap();
            }
        });
    }

    #[test]
    fn full_event_queue_does_not_block_outgoing_frames() {
        run_local(async {
            let (stream, mut peer) = tokio::io::duplex(4096);
            let (tx, rx) = mpsc::channel(32);
            let (events, _received) = mpsc::channel(1);
            events
                .send(SessionEvent::Connected {
                    device: "peer".into(),
                    addr: "127.0.0.1:1".parse().unwrap(),
                })
                .await
                .unwrap();
            let task = tokio::task::spawn_local(async move {
                run_session("peer", 1, stream, rx, &events, &broadcast::channel(64).0).await
            });
            let payload = Frame::Control(test_message("incoming")).encode().unwrap();
            transport::write_payload(&mut peer, &payload).await.unwrap();
            tokio::task::yield_now().await;
            // More messages than the outgoing queue capacity, while the
            // event consumer deliberately never drains its full queue.
            for _ in 0..40 {
                let frame = Frame::Control(test_message("outgoing"));
                let (completion, result) = oneshot::channel();
                tx.send(Outgoing {
                    frame: frame.clone(),
                    completion,
                })
                .await
                .unwrap();
                assert_eq!(
                    Frame::decode(&transport::read_payload(&mut peer).await.unwrap()).unwrap(),
                    frame
                );
                result.await.unwrap().unwrap();
            }
            task.abort();
        });
    }

    #[test]
    fn chunks_reach_the_peer_with_intact_bytes() {
        run_local(async {
            let (alice, bob, _alice_events, mut bob_events, addr) = pair().await;
            let bob_id = bob_device(&bob);
            alice.connect(bob_id.clone(), addr).await.unwrap();
            assert!(matches!(
                bob_events.recv().await.unwrap(),
                SessionEvent::Connected { .. }
            ));

            // Three chunks, the last one short, to prove offsets and
            // payload boundaries survive the framing.
            let payloads = vec![
                vec![7u8; crate::protocol::MAX_CHUNK_DATA],
                vec![9u8; 1024],
                vec![1, 2, 3],
            ];
            let mut offset = 0u64;
            let send = async {
                for data in &payloads {
                    alice
                        .send_chunk(
                            &bob_id,
                            crate::protocol::FileChunk {
                                transfer_id: "t-1".into(),
                                path: "payload.bin".into(),
                                offset,
                                data: data.clone(),
                            },
                        )
                        .await
                        .unwrap();
                    offset += data.len() as u64;
                }
            };
            send.await;

            let mut received_offset = 0u64;
            for expected in &payloads {
                match bob_events.recv().await.unwrap() {
                    SessionEvent::Chunk { chunk, .. } => {
                        assert_eq!(&chunk.data, expected);
                        assert_eq!(chunk.offset, received_offset);
                        received_offset += chunk.data.len() as u64;
                    }
                    other => panic!("unexpected event: {other:?}"),
                }
            }
            assert_eq!(received_offset, offset);
        });
    }

    /// Whole folder transfers end to end over a real TLS session: the
    /// sender walks the tree and streams chunks, the receiver rebuilds
    /// it and verifies every digest.
    #[test]
    fn folder_transfer_round_trips_over_tls() {
        run_local(async {
            let (alice, bob, _alice_events, mut bob_events, addr) = pair().await;
            let bob_id = bob_device(&bob);

            let unique = std::process::id();
            let source = std::env::temp_dir().join(format!("lan-mouse-e2e-src-{unique}"));
            let dest = std::env::temp_dir().join(format!("lan-mouse-e2e-dst-{unique}"));
            let _ = std::fs::remove_dir_all(&source);
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::create_dir_all(source.join("album/nested")).unwrap();
            std::fs::create_dir_all(source.join("album/empty")).unwrap();
            std::fs::write(source.join("album/notes.txt"), b"hello folder").unwrap();
            let big: Vec<u8> = (0..(crate::protocol::MAX_CHUNK_DATA + 333))
                .map(|i| (i % 253) as u8)
                .collect();
            std::fs::write(source.join("album/nested/big.bin"), &big).unwrap();

            let prepared = crate::transfer::prepare_transfer(&source.join("album"))
                .await
                .unwrap();
            let offered = crate::transfer::offered_files(&prepared).unwrap();

            alice.connect(bob_id.clone(), addr).await.unwrap();
            assert!(matches!(
                bob_events.recv().await.unwrap(),
                SessionEvent::Connected { .. }
            ));

            let mut receiver = crate::transfer::TransferReceiver::new();
            // Interleave the real send with the real receive by driving
            // both sides from this task.
            let manager = alice.clone();
            let device = bob_id.clone();
            let send = async {
                struct Sink<'a> {
                    manager: &'a SessionManager,
                    device: &'a str,
                }
                impl crate::transfer::ChunkSink for Sink<'_> {
                    type Error = SessionError;
                    async fn send_chunk(
                        &mut self,
                        chunk: crate::protocol::FileChunk,
                    ) -> Result<(), Self::Error> {
                        self.manager.send_chunk(self.device, chunk).await
                    }
                }
                let mut sink = Sink {
                    manager: &manager,
                    device: &device,
                };
                crate::transfer::send_transfer(&mut sink, "e2e-1", &prepared)
                    .await
                    .unwrap()
            };
            let receive = async {
                receiver
                    .begin(
                        "e2e-1",
                        &dest,
                        &offered,
                        crate::transfer::ConflictPolicy::Ask,
                    )
                    .await
                    .unwrap();
                let mut chunks = 0usize;
                loop {
                    match bob_events.recv().await.unwrap() {
                        SessionEvent::Chunk { chunk, .. } => {
                            receiver
                                .write_chunk(
                                    &chunk.transfer_id,
                                    &chunk.path,
                                    chunk.offset,
                                    &chunk.data,
                                )
                                .await
                                .unwrap();
                            chunks += 1;
                        }
                        other => panic!("unexpected event: {other:?}"),
                    }
                    if chunks == expected_chunks(&prepared) {
                        break;
                    }
                }
                receiver.finish("e2e-1").await.unwrap()
            };
            let (sent, committed) = tokio::join!(send, receive);

            let expected_bytes: u64 = prepared
                .iter()
                .filter(|f| f.has_content())
                .map(|f| f.size)
                .sum();
            assert_eq!(sent, expected_bytes);
            assert_eq!(committed.len(), 2);
            assert_eq!(
                std::fs::read(dest.join("album/notes.txt")).unwrap(),
                b"hello folder"
            );
            assert_eq!(
                std::fs::read(dest.join("album/nested/big.bin")).unwrap(),
                big
            );
            assert!(dest.join("album/empty").is_dir());

            let _ = std::fs::remove_dir_all(&source);
            let _ = std::fs::remove_dir_all(&dest);
        });
    }

    /// Number of chunks the sender will emit for a prepared transfer.
    fn expected_chunks(prepared: &[crate::transfer::PreparedFile]) -> usize {
        prepared
            .iter()
            .filter(|f| f.has_content())
            .map(|f| {
                let chunk = crate::protocol::MAX_CHUNK_DATA as u64;
                f.size.div_ceil(chunk) as usize
            })
            .sum()
    }

    /// Session tasks use `spawn_local`, matching the service's
    /// single-threaded runtime.
    fn run_local<F>(future: F)
    where
        F: std::future::Future<Output = ()> + 'static,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            tokio::time::timeout(std::time::Duration::from_secs(5), future)
                .await
                .expect("session test timed out");
        });
    }

    fn bob_device(bob: &SessionManager) -> String {
        bob.identity.fingerprint()
    }
}
