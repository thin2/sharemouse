use crate::{
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    listen::{LanMouseListener, ListenerCreationError},
};
use futures::StreamExt;
use lan_mouse_data::sharing::{Effect as SharingEffect, Round, Sharing};
use lan_mouse_data::{DataMessage, SessionEvent, SessionManager};
use lan_mouse_ipc::{
    AsyncFrontendListener, ClientHandle, DataSession, FrontendEvent, FrontendRequest, IpcError,
    IpcListenerCreationError, Position, Status,
};
use log;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};

mod clipboard;
mod discovery;
mod enrollment;
mod image_messages;
mod sharing;
enum SessionControl {
    Message(DataMessage),
    ImageReady,
    Image {
        request: lan_mouse_data::clipboard::FetchRequest,
        revision: Option<u64>,
        bytes: Vec<u8>,
        offset: usize,
        epoch: u64,
    },
}
struct SessionSender {
    control: tokio::sync::mpsc::Sender<SessionControl>,
    image: std::rc::Rc<std::cell::RefCell<Option<SessionControl>>>,
}
enum ClipboardJob {
    Read(io::Result<crate::clipboard_native::RawSnapshot>),
    Prepared {
        revision: u64,
        request: Box<lan_mouse_data::clipboard::FetchRequest>,
        image: lan_mouse_data::clipboard_image::Image,
        serial: u64,
        result: io::Result<crate::clipboard_native::PreparedImage>,
    },
}

enum SharingJob {
    Connected {
        device: String,
        result: Result<(), String>,
    },
    Released {
        round: Option<Round>,
        result: Result<(), String>,
    },
    Sent {
        device: String,
        generation: Option<u64>,
        result: Result<(), String>,
    },
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
    #[error("failed to load data-channel identity: `{0}`")]
    DataIdentity(#[from] lan_mouse_data::IdentityError),
}

pub struct Service {
    enrollment: enrollment::Enrollment,
    discovery: discovery::Discovery,
    shutdown: bool,
    messages: VecDeque<FrontendEvent>,
    message_images: crate::message_images::Worker,
    message_epoch: u64,
    image_messages: HashMap<String, image_messages::IncomingImage>,
    clipboard: lan_mouse_data::clipboard_sync::ClipboardSync,
    native_clipboard: crate::clipboard_native::NativeClipboard,
    clipboard_status: lan_mouse_ipc::ClipboardStatus,
    clipboard_route_status: lan_mouse_data::clipboard_sync::Status,
    clipboard_read_error: bool,
    clipboard_read_pending: bool,
    clipboard_jobs: tokio::task::JoinSet<ClipboardJob>,
    clipboard_wire_epoch: std::rc::Rc<std::cell::Cell<u64>>,
    clipboard_asset_dir: std::path::PathBuf,
    input_gate: crate::sharing::InputGate,
    sharing: Sharing,
    sharing_jobs: tokio::task::JoinSet<SharingJob>,
    sharing_generations: HashMap<String, u64>,
    sharing_outgoing: HashMap<String, SessionSender>,
    sharing_last_state: lan_mouse_ipc::SharingState,
    sharing_addresses: HashMap<String, SocketAddr>,
    sharing_connecting: HashSet<String>,
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// fingerprints accepted by the input DTLS and data TLS listeners
    allowed_fingerprints: lan_mouse_data::AuthorizedFingerprints,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// data channel listener address
    data_addr: SocketAddr,
    /// data channel listener task
    data_task: Option<tokio::task::JoinHandle<()>>,
    /// per-device authenticated data sessions
    data_sessions: SessionManager,
    /// events from data sessions
    data_events: tokio::sync::mpsc::Receiver<SessionEvent>,
    /// Bounded background requests; never block the service on network IO.
    data_jobs: tokio::task::JoinSet<Option<FrontendEvent>>,
    /// Progress events produced by background transfer jobs.
    ///
    /// A job cannot call `notify_frontend` (that needs `&mut Service`),
    /// so it sends here and the main loop forwards the events.
    transfer_progress: tokio::sync::mpsc::Receiver<FrontendEvent>,
    /// Sender handed to background transfer jobs.
    transfer_progress_tx: tokio::sync::mpsc::Sender<FrontendEvent>,
    /// Receive-side state for incoming file and folder transfers.
    transfer_receiver: lan_mouse_data::TransferReceiver,
    /// Where incoming transfers are written.
    receive_dir: std::path::PathBuf,
    /// Offers held until the user decides how to treat name conflicts.
    pending_transfers: HashMap<String, PendingTransfer>,
    incoming_transfers: HashMap<String, TransferPeer>,
    /// A selection handed over by the right-click entry point that the
    /// frontend has not been told about yet.
    ///
    /// A context menu can start this daemon and connect before the
    /// window is attached, and events are only broadcast to frontends
    /// that are already connected. Holding the selection lets the next
    /// frontend synchronisation deliver it instead of losing it.
    pending_selection: VecDeque<Vec<std::path::PathBuf>>,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// Device currently receiving input, if any.
    ///
    /// Distinct from a client's `active` flag: several devices can be
    /// eligible to receive input (each sits at a screen edge), but only
    /// one is being driven at a time. A03 asks the interface to show
    /// this one.
    current_target: Option<ClientHandle>,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    next_trigger_handle: u64,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

/// An announced transfer whose destination names are already taken.
///
/// The offer is kept intact so the transfer can start as soon as the
/// user picks overwrite, skip or rename; the sender waits for acceptance
/// before reading or sending any file chunks.
#[derive(Debug)]
struct PendingTransfer {
    peer: TransferPeer,
    files: Vec<lan_mouse_data::OfferedFile>,
}

#[derive(Debug, Clone)]
struct TransferPeer {
    device: String,
    generation: u64,
    id: String,
    updated: std::time::Instant,
}

fn transfer_key(device: &str, generation: u64, id: &str) -> String {
    format!("{device}:{generation}:{id}")
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);
        let mut epoch = [0; 16];
        rustls::crypto::ring::default_provider()
            .secure_random
            .fill(&mut epoch)
            .map_err(|_| io::Error::other("could not generate input session identity"))?;
        let mut members: std::collections::BTreeSet<_> =
            config.authorized_fingerprints().into_keys().collect();
        members.insert(public_key_fingerprint.clone());
        let sharing = Sharing::new(public_key_fingerprint.clone(), members, epoch);
        let input_gate = crate::sharing::InputGate::default();

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        let allowed_fingerprints: lan_mouse_data::AuthorizedFingerprints = Arc::new(RwLock::new(
            authorized_keys
                .read()
                .expect("lock")
                .keys()
                .cloned()
                .collect(),
        ));
        // listener + connection
        let listener = LanMouseListener::new(
            config.port(),
            cert.clone(),
            authorized_keys.clone(),
            allowed_fingerprints.clone(),
        )
        .await?;
        let conn = LanMouseConnection::new(
            cert.clone(),
            client_manager.clone(),
            allowed_fingerprints.clone(),
        );

        // Data channel: a separate TCP listener so file and message
        // traffic cannot delay input events.
        let data_addr = SocketAddr::new("0.0.0.0".parse().expect("invalid ip"), config.data_port());
        let data_listener = tokio::net::TcpListener::bind(data_addr).await?;
        let data_addr = data_listener.local_addr()?;
        let data_identity = lan_mouse_data::load_identity(config.cert_path())?;
        let enrollment =
            enrollment::Enrollment::new(data_identity.clone(), config.connection_group());
        let (data_events_tx, data_events) = tokio::sync::mpsc::channel(64);
        let data_sessions = SessionManager::new(
            data_identity.clone(),
            allowed_fingerprints.clone(),
            data_events_tx,
        );
        let listener_sessions = data_sessions.clone();
        let listener_allowed = allowed_fingerprints.clone();
        let data_task = tokio::task::spawn_local(async move {
            run_data_listener(
                data_listener,
                data_identity,
                listener_allowed,
                listener_sessions,
            )
            .await;
        });

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(
            capture_backend,
            conn,
            config.release_bind(),
            input_gate.clone(),
        );
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener, input_gate.clone());

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();
        let receive_dir = config.receive_dir();
        let (transfer_progress_tx, transfer_progress) = tokio::sync::mpsc::channel(64);
        let discovery = discovery::Discovery::start().await;
        let service = Self {
            enrollment,
            discovery,
            shutdown: false,
            messages: VecDeque::new(),
            message_images: crate::message_images::Worker::start(
                config
                    .clipboard_asset_dir()
                    .with_file_name("message-images"),
                config.clipboard_asset_dir(),
            ),
            message_epoch: 0,
            image_messages: HashMap::new(),
            clipboard: lan_mouse_data::clipboard_sync::ClipboardSync::new(
                public_key_fingerprint.clone(),
                epoch.iter().map(|b| format!("{b:02x}")).collect(),
            ),
            native_clipboard: Default::default(),
            clipboard_status: Default::default(),
            clipboard_route_status: lan_mouse_data::clipboard_sync::Status::Paused,
            clipboard_read_error: false,
            clipboard_read_pending: false,
            clipboard_jobs: tokio::task::JoinSet::new(),
            clipboard_wire_epoch: Default::default(),
            clipboard_asset_dir: config.clipboard_asset_dir(),
            input_gate,
            sharing,
            sharing_jobs: tokio::task::JoinSet::new(),
            sharing_generations: HashMap::new(),
            sharing_outgoing: HashMap::new(),
            sharing_last_state: Default::default(),
            sharing_addresses: HashMap::new(),
            sharing_connecting: HashSet::new(),
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            allowed_fingerprints,
            public_key_fingerprint,
            client_manager,
            data_addr,
            data_task: Some(data_task),
            data_sessions,
            data_events,
            data_jobs: tokio::task::JoinSet::new(),
            transfer_progress,
            transfer_progress_tx,
            transfer_receiver: lan_mouse_data::TransferReceiver::new(),
            receive_dir,
            pending_transfers: HashMap::new(),
            incoming_transfers: HashMap::new(),
            pending_selection: VecDeque::new(),
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            current_target: None,
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }
        self.restore_connection_group();
        let effects = self.sharing.initialize();
        self.apply_sharing_effects(effects);
        let mut sharing_retry = tokio::time::interval(std::time::Duration::from_secs(3));
        self.poll_clipboard();
        let mut clipboard_poll = tokio::time::interval(std::time::Duration::from_millis(200));
        clipboard_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        while !self.shutdown {
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request).await,
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                event = self.data_events.recv() => self.handle_data_event(event).await,
                Some(event) = self.enrollment.events.recv() => self.handle_enrollment_event(event),
                Some(found) = self.discovery.events.recv() => self.handle_discovered(found),
                _ = sharing_retry.tick() => {
                    self.expire_discovered_hosts();
                    self.expire_transfers(false).await;
                    self.image_messages.retain(|_,v|v.updated.elapsed()<std::time::Duration::from_secs(30));
                    self.retry_sharing_connections();
                    let actions = self.clipboard.retry();
                    self.apply_clipboard_actions(actions);
                },
                _ = clipboard_poll.tick() => self.poll_clipboard(),
                Some(event)=self.message_images.events.recv()=>self.handle_image_message_event(event),
                Some(result) = self.clipboard_jobs.join_next(), if !self.clipboard_jobs.is_empty() => {
                    match result {
                        Ok(job) => self.handle_clipboard_job(job),
                        Err(_) => {
                            self.clipboard_read_pending = false;
                            self.clipboard_failed();
                        }
                    }
                }
                Some(result) = self.sharing_jobs.join_next(), if !self.sharing_jobs.is_empty() => {
                    match result {
                        Ok(job) => self.handle_sharing_job(job),
                        Err(e) => {
                            log::error!("input session task failed: {e}");
                            self.fail_sharing(format!("input session task failed: {e}"));
                        }
                    }
                }
                Some(event) = self.transfer_progress.recv() => self.notify_frontend(event),
                result = self.data_jobs.join_next(), if !self.data_jobs.is_empty() => {
                    match result {
                        Some(Ok(Some(event))) => {
                            // Preserve progress-before-terminal ordering across the two queues.
                            while let Ok(progress) = self.transfer_progress.try_recv() {
                                self.notify_frontend(progress);
                            }
                            if let FrontendEvent::MessageReceived { device, .. } = &event {
                                if !self.allowed_fingerprints.read().expect("lock").contains(device) {
                                    continue;
                                }
                            }
                            self.notify_frontend(event);
                        }
                        Some(Err(e)) => log::warn!("data request task failed: {e}"),
                        _ => {}
                    }
                }
                _ = self.config.changed() => {
                    self.handle_config_change();
                    self.expire_transfers(false).await;
                },
                r = signal::ctrl_c() => { r.expect("failed to wait for CTRL+C"); break; },
            }
        }

        log::info!("terminating service ...");
        self.input_gate.close();
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;
        self.data_jobs.abort_all();
        self.enrollment.stop();
        self.expire_transfers(true).await;
        self.sharing_jobs.abort_all();
        self.clipboard_jobs.abort_all();
        self.message_images.stop().await;
        self.data_sessions.terminate();
        if let Some(task) = self.data_task.take() {
            task.abort();
        }

        Ok(())
    }

    async fn handle_frontend_request(
        &mut self,
        request: Option<Result<FrontendRequest, IpcError>>,
    ) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::HostGroup { port } => self.host_connection_group(port).await,
            FrontendRequest::JoinGroup { address } => self.join_connection_group(address),
            FrontendRequest::ConfirmEnrollment { id, approve } => {
                if let Some(decision) = self.enrollment.pending.remove(&id) {
                    let _ = decision.send(approve);
                }
            }
            FrontendRequest::StopEnrollment => {
                self.enrollment.stop();
                self.publish_enrollment_state("stopped");
            }
            FrontendRequest::Shutdown => {
                self.stop_sharing();
                self.shutdown = true;
            }
            FrontendRequest::PresentWindow => self.notify_frontend(FrontendEvent::PresentWindow),
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                let group_peer = self
                    .client_manager
                    .get_state(handle)
                    .and_then(|(_, state)| state.peer_fingerprint)
                    .filter(|fp| {
                        self.enrollment
                            .group
                            .as_ref()
                            .is_some_and(|g| g.peers.iter().any(|p| &p.fingerprint == fp))
                    });
                if let Some(fp) = group_peer {
                    self.remove_authorized_key(fp);
                }
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::ListClipboardHistory => {
                // Keep old IPC clients compatible without reading saved history.
                self.notify_frontend(FrontendEvent::ClipboardHistory(vec![]));
            }
            FrontendRequest::ClearClipboardHistory
            | FrontendRequest::CopyHistory { .. }
            | FrontendRequest::SendHistory { .. } => {
                self.notify_frontend(FrontendEvent::Error(
                    "Clipboard history is no longer supported".into(),
                ));
            }
            FrontendRequest::ClearImageMessages => {
                self.messages.clear();
                self.notify_frontend(FrontendEvent::MessagesCleared);
                self.message_epoch = self.message_epoch.wrapping_add(1);
                self.image_message_command(crate::message_images::Command::ClearMessages);
            }
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.expire_transfers(false).await;
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook)
            }
            FrontendRequest::UpdateLeaveHook(handle, leave_hook) => {
                self.update_leave_hook(handle, leave_hook)
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::SendMessage { handle, text } => {
                let Some(device) = self.client_manager.fingerprint(handle) else {
                    self.notify_frontend(FrontendEvent::Error(
                        "that device has not connected yet; connect it once to learn its identity"
                            .into(),
                    ));
                    return;
                };
                self.start_data_request(
                    device,
                    DataRequestAction::Message {
                        text,
                        handle: Some(handle),
                    },
                    None,
                );
            }
            FrontendRequest::SendFiles { handle, paths } => {
                let Some(device) = self.client_manager.fingerprint(handle) else {
                    self.notify_frontend(FrontendEvent::Error(
                        "that device has not connected yet; connect it once to learn its identity"
                            .into(),
                    ));
                    return;
                };
                if paths.is_empty() {
                    self.notify_frontend(FrontendEvent::Error("no files selected".into()));
                    return;
                }
                let transfer_id = self.data_sessions.next_message_id();
                self.start_data_request(
                    device,
                    DataRequestAction::Files {
                        paths,
                        transfer_id,
                        handle: Some(handle),
                    },
                    None,
                );
            }
            FrontendRequest::SetReceiveDir(dir) => self.set_receive_dir(dir).await,
            FrontendRequest::ResolveTransferConflict {
                transfer_id,
                policy,
            } => {
                if policy == "cancel" {
                    if let Some(pending) = self.pending_transfers.remove(&transfer_id) {
                        self.transfer_reply(
                            &pending.peer,
                            DataMessage::FileRejected {
                                transfer_id: pending.peer.id.clone(),
                                reason: "cancelled by recipient".into(),
                            },
                        );
                    }
                    return;
                }
                let Some(policy) = lan_mouse_data::ConflictPolicy::from_code(&policy) else {
                    self.notify_frontend(FrontendEvent::Error(format!(
                        "unknown conflict policy `{policy}`"
                    )));
                    return;
                };
                self.resolve_transfer_conflict(transfer_id, policy).await;
            }
            FrontendRequest::ConnectDataSession { device, addr } => {
                self.sharing_addresses.insert(device.clone(), addr);
                self.start_data_request(device, DataRequestAction::ConnectOnly, Some(addr));
            }
            FrontendRequest::EnumerateDataSessions => self.enumerate_data_sessions(),
            FrontendRequest::SelectTarget { paths } => {
                if self.pending_selection.len() >= 32 {
                    self.notify_frontend(FrontendEvent::Error(
                        "too many pending file selections".into(),
                    ));
                    return;
                }
                self.pending_selection.push_back(paths.clone());
                if self.frontend_listener.has_gui_frontends() {
                    self.notify_frontend(FrontendEvent::SelectTarget { paths });
                }
            }
            FrontendRequest::TakePendingSelection => {
                for paths in self.pending_selection.clone() {
                    self.notify_frontend(FrontendEvent::SelectTarget { paths });
                }
            }
            FrontendRequest::SelectionReceived { paths } => {
                if let Some(index) = self.pending_selection.iter().position(|p| *p == paths) {
                    self.pending_selection.remove(index);
                }
            }
            FrontendRequest::StopSharing => self.stop_sharing(),
            FrontendRequest::StartSharing => {
                self.request_sharing(Some(self.public_key_fingerprint.clone()))
            }
        }
    }

    /// Close local admission immediately and request a group stop barrier.
    fn stop_sharing(&mut self) {
        let effects = self
            .sharing
            .disconnected(&self.public_key_fingerprint.clone());
        self.apply_sharing_effects(effects);
        self.request_sharing(None);
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                leave_hook: c.leave_cmd,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        self.allowed_fingerprints
            .write()
            .unwrap()
            .clone_from(&authorized_keys.into_keys().collect());
        self.close_revoked_data_sessions();
        self.refresh_sharing_members();
        // A hand-edited `receive_dir` must take effect without a restart.
        self.receive_dir = self.config.receive_dir();
        self.restore_connection_group();
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
                token,
            } => {
                if !self.input_gate.accepts(&fingerprint, token) {
                    return;
                }
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.config.set_input_port(port);
                    self.update_enrollment_port();
                    self.save_config();
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationFailed(reason) => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
                self.notify_frontend(FrontendEvent::Error(format!(
                    "Input emulation unavailable: {reason}"
                )));
                if self.sharing.phase() == lan_mouse_data::sharing::Phase::Running
                    && self
                        .sharing
                        .primary()
                        .is_some_and(|p| p != self.public_key_fingerprint)
                {
                    self.stop_sharing();
                }
            }
            EmulationEvent::ReleaseNotify(token) => {
                if self.input_gate.token() == Some(token) {
                    self.capture.release();
                }
            }
            EmulationEvent::Connected { addr, fingerprint } => {
                let data_addr =
                    *self
                        .sharing_addresses
                        .entry(fingerprint.clone())
                        .or_insert(SocketAddr::new(
                            addr.ip(),
                            lan_mouse_data::DEFAULT_DATA_PORT,
                        ));
                if !self.data_sessions.is_connected(&fingerprint) {
                    self.start_data_request(
                        fingerprint.clone(),
                        DataRequestAction::ConnectOnly,
                        Some(data_addr),
                    );
                }
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.set_current_target(None);
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureFailed(reason) => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
                self.notify_frontend(FrontendEvent::Error(format!(
                    "Input capture unavailable: {reason}"
                )));
                if self.sharing.phase() == lan_mouse_data::sharing::Phase::Running {
                    self.stop_sharing();
                }
            }
            ICaptureEvent::ClientEntered(handle, token) => {
                log::info!("entering client {handle} ...");
                if self
                    .client_manager
                    .get_state(handle)
                    .is_some_and(|(_, s)| s.active)
                    && self.input_gate.sending() == Some(token)
                {
                    self.set_current_target(Some(handle));
                }
                self.spawn_hook_command(handle, HookKind::Enter);
            }
            ICaptureEvent::ClientLeft(handle, token) => {
                log::info!("leaving client {handle} ...");
                if self.current_target == Some(handle) && self.input_gate.token() == Some(token) {
                    self.set_current_target(None);
                }
                self.spawn_hook_command(handle, HookKind::Leave);
            }
        }
    }

    async fn handle_data_event(&mut self, event: Option<SessionEvent>) {
        let Some(event) = event else {
            return;
        };
        match event {
            SessionEvent::Clipboard {
                device,
                generation,
                message,
            } => {
                if self.data_sessions.generation(&device) != Some(generation)
                    || !self
                        .allowed_fingerprints
                        .read()
                        .expect("lock")
                        .contains(&device)
                {
                    return;
                }
                self.sharing_connected(&device);
                let actions = self.clipboard.receive(&device, generation, message);
                self.apply_clipboard_actions(actions);
            }
            SessionEvent::Sharing {
                device,
                generation,
                message,
            } => {
                if self.data_sessions.generation(&device) != Some(generation)
                    || !self
                        .allowed_fingerprints
                        .read()
                        .expect("lock")
                        .contains(&device)
                {
                    return;
                }
                self.sharing_connected(&device);
                let effects = self.sharing.receive(&device, message);
                self.apply_sharing_effects(effects);
            }
            SessionEvent::Connected { device, addr } => {
                self.expire_transfers(false).await;
                if !self.data_sessions.is_connected(&device)
                    || !self
                        .allowed_fingerprints
                        .read()
                        .expect("lock")
                        .contains(&device)
                {
                    return;
                }
                log::info!("data session connected: {device} @ {addr}");
                self.sharing_connected(&device);
                self.broadcast_connection_group();
                self.update_enrollment_port();
                self.notify_frontend(FrontendEvent::DataSessionConnected(DataSession {
                    name: self.device_name(&device),
                    device,
                    addr,
                }));
            }
            SessionEvent::Disconnected { device, reason } => {
                self.expire_transfers(false).await;
                if self.data_sessions.is_connected(&device) {
                    return;
                }
                log::info!("data session disconnected: {device} ({reason})");
                self.sharing_generations.remove(&device);
                self.sharing_outgoing.remove(&device);
                self.clipboard.disconnected(&device);
                let effects = self.sharing.disconnected(&device);
                self.apply_sharing_effects(effects);
                self.notify_frontend(FrontendEvent::DataSessionDisconnected { device, reason });
            }
            SessionEvent::Message {
                device,
                generation,
                message,
            } => {
                if self.data_sessions.generation(&device) != Some(generation)
                    || !self
                        .allowed_fingerprints
                        .read()
                        .expect("lock")
                        .contains(&device)
                {
                    return;
                }
                match message {
                    DataMessage::Group { group } => self.receive_connection_group(&device, group),
                    DataMessage::Message { id, text } => {
                        self.notify_frontend(FrontendEvent::MessageReceived {
                            device,
                            message_id: id,
                            text,
                        });
                    }
                    DataMessage::FileOffer { transfer_id, files } => {
                        self.begin_incoming_transfer(&device, generation, transfer_id, files)
                            .await;
                    }
                    DataMessage::FileComplete { transfer_id } => {
                        self.finish_incoming_transfer(&device, generation, transfer_id)
                            .await;
                    }
                    DataMessage::FileCancel {
                        transfer_id,
                        reason,
                    } => {
                        self.cancel_incoming_transfer(&device, generation, transfer_id, reason)
                            .await;
                    }
                    _ => log::debug!("unused control message ignored"),
                }
            }
            SessionEvent::ImageMessage {
                device,
                generation,
                part,
            } => self.receive_image_message(device, generation, part),
            SessionEvent::Chunk {
                device,
                generation,
                chunk,
            } => {
                if self.data_sessions.generation(&device) != Some(generation)
                    || !self
                        .allowed_fingerprints
                        .read()
                        .expect("lock")
                        .contains(&device)
                {
                    return;
                }
                self.write_incoming_chunk(&device, generation, chunk).await;
            }
        }
    }

    async fn begin_incoming_transfer(
        &mut self,
        device: &str,
        generation: u64,
        transfer_id: String,
        files: Vec<lan_mouse_data::OfferedFile>,
    ) {
        let peer = TransferPeer {
            device: device.into(),
            generation,
            id: transfer_id,
            updated: std::time::Instant::now(),
        };
        let key = transfer_key(device, generation, &peer.id);
        if self.pending_transfers.contains_key(&key) || self.incoming_transfers.contains_key(&key) {
            self.reject_transfer(&peer, "duplicate transfer".into());
            self.pending_transfers.remove(&key);
            self.incoming_transfers.remove(&key);
            let _ = self.transfer_receiver.cancel(&key).await;
            return;
        }
        if self.pending_transfers.len() + self.incoming_transfers.len() >= 16 {
            self.reject_transfer(&peer, "too many incoming transfers".into());
            return;
        }
        let root = self.receive_dir.clone();
        if let Err(e) = tokio::fs::create_dir_all(&root).await {
            self.reject_transfer(&peer, format!("could not create receive directory: {e}"));
            return;
        }
        // No chunks are sent until FileAccepted follows the user's decision.
        match lan_mouse_data::detect_conflicts(&root, &files).await {
            Ok(conflicts) if !conflicts.is_empty() => {
                let paths: Vec<String> = conflicts.iter().map(|c| c.path.clone()).collect();
                self.pending_transfers
                    .insert(key.clone(), PendingTransfer { peer, files });
                self.notify_frontend(FrontendEvent::TransferConflicts {
                    device: device.to_owned(),
                    transfer_id: key,
                    paths,
                });
                return;
            }
            Ok(_) => {}
            Err(e) => {
                self.reject_transfer(&peer, format!("could not inspect receive directory: {e}"));
                return;
            }
        }
        self.start_receiving(peer, files, lan_mouse_data::ConflictPolicy::Ask)
            .await;
    }

    async fn start_receiving(
        &mut self,
        peer: TransferPeer,
        files: Vec<lan_mouse_data::OfferedFile>,
        policy: lan_mouse_data::ConflictPolicy,
    ) {
        let root = self.receive_dir.clone();
        if self.data_sessions.generation(&peer.device) != Some(peer.generation) {
            return;
        }
        let key = transfer_key(&peer.device, peer.generation, &peer.id);
        match self
            .transfer_receiver
            .begin(&key, &root, &files, policy)
            .await
        {
            Ok(_) => {
                self.incoming_transfers.insert(key, peer.clone());
                self.transfer_reply(
                    &peer,
                    DataMessage::FileAccepted {
                        transfer_id: peer.id.clone(),
                    },
                );
            }
            Err(e) => {
                self.reject_transfer(&peer, format!("could not receive transfer: {e}"));
            }
        }
    }

    /// Apply the user's conflict choice to a held transfer.
    async fn resolve_transfer_conflict(
        &mut self,
        transfer_id: String,
        policy: lan_mouse_data::ConflictPolicy,
    ) {
        let Some(pending) = self.pending_transfers.remove(&transfer_id) else {
            log::warn!("no transfer `{transfer_id}` waiting for a conflict decision");
            return;
        };
        self.start_receiving(pending.peer, pending.files, policy)
            .await;
    }

    async fn write_incoming_chunk(
        &mut self,
        device: &str,
        generation: u64,
        chunk: lan_mouse_data::FileChunk,
    ) {
        let key = transfer_key(device, generation, &chunk.transfer_id);
        let Some(peer) = self.incoming_transfers.get_mut(&key) else {
            return;
        };
        peer.updated = std::time::Instant::now();
        if let Err(e) = self
            .transfer_receiver
            .write_chunk(&key, &chunk.path, chunk.offset, &chunk.data)
            .await
        {
            if let Some(peer) = self.incoming_transfers.remove(&key) {
                self.reject_transfer(&peer, format!("chunk rejected: {e}"));
            }
            let _ = self.transfer_receiver.cancel(&key).await;
        }
    }

    async fn finish_incoming_transfer(
        &mut self,
        device: &str,
        generation: u64,
        transfer_id: String,
    ) {
        let key = transfer_key(device, generation, &transfer_id);
        let Some(peer) = self.incoming_transfers.remove(&key) else {
            return;
        };
        match self.transfer_receiver.finish(&key).await {
            Ok(committed) => {
                self.transfer_reply(
                    &peer,
                    DataMessage::FileCommitted {
                        transfer_id: transfer_id.clone(),
                    },
                );
                log::info!(
                    "transfer `{transfer_id}` from {device} complete: {} file(s)",
                    committed.len()
                );
                self.notify_frontend(FrontendEvent::TransferComplete {
                    device: device.to_owned(),
                    files: committed,
                });
            }
            Err(e) => {
                self.reject_transfer(&peer, format!("receive failed: {e}"));
            }
        }
    }

    async fn cancel_incoming_transfer(
        &mut self,
        device: &str,
        generation: u64,
        transfer_id: String,
        reason: Option<String>,
    ) {
        let key = transfer_key(device, generation, &transfer_id);
        self.pending_transfers.remove(&key);
        self.incoming_transfers.remove(&key);
        let _ = self.transfer_receiver.cancel(&key).await;
        log::info!(
            "transfer `{transfer_id}` from {device} cancelled by peer ({})",
            reason.as_deref().unwrap_or("no reason")
        );
    }

    fn transfer_reply(&mut self, peer: &TransferPeer, message: DataMessage) {
        let sessions = self.data_sessions.clone();
        let peer = peer.clone();
        self.data_jobs.spawn_local(async move {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                sessions.send_at(&peer.device, peer.generation, message),
            )
            .await;
            None
        });
    }

    fn reject_transfer(&mut self, peer: &TransferPeer, reason: String) {
        self.transfer_reply(
            peer,
            DataMessage::FileRejected {
                transfer_id: peer.id.clone(),
                reason: reason.clone(),
            },
        );
        self.notify_frontend(FrontendEvent::Error(reason));
    }

    async fn expire_transfers(&mut self, all: bool) {
        let expired = |peer: &TransferPeer| {
            all || self.data_sessions.generation(&peer.device) != Some(peer.generation)
                || !self
                    .allowed_fingerprints
                    .read()
                    .expect("lock")
                    .contains(&peer.device)
                || peer.updated.elapsed() > std::time::Duration::from_secs(300)
        };
        let pending: Vec<_> = self
            .pending_transfers
            .iter()
            .filter(|(_, p)| expired(&p.peer))
            .map(|(k, _)| k.clone())
            .collect();
        let active: Vec<_> = self
            .incoming_transfers
            .iter()
            .filter(|(_, p)| expired(p))
            .map(|(k, _)| k.clone())
            .collect();
        for key in pending {
            if let Some(pending) = self.pending_transfers.remove(&key) {
                if !all {
                    self.reject_transfer(&pending.peer, "transfer expired or disconnected".into());
                }
            }
        }
        for key in active {
            if let Some(peer) = self.incoming_transfers.remove(&key) {
                let _ = self.transfer_receiver.cancel(&key).await;
                if !all {
                    self.reject_transfer(&peer, "transfer expired or disconnected".into());
                }
            }
        }
    }

    /// Display name for a device fingerprint. `authorized_keys` stores
    /// the description entered when the device was approved, which is
    /// the only name that is keyed by the fingerprint itself.
    fn device_name(&self, device: &str) -> Option<String> {
        self.authorized_keys
            .read()
            .expect("lock")
            .get(device)
            .filter(|name| !name.is_empty())
            .cloned()
    }

    fn start_data_request(
        &mut self,
        device: String,
        action: DataRequestAction,
        addr: Option<SocketAddr>,
    ) {
        if let DataRequestAction::Message { text, .. } = &action {
            if text.len() > lan_mouse_data::MAX_MESSAGE_BYTES {
                self.notify_frontend(FrontendEvent::Error(format!(
                    "message exceeds the limit of {} bytes",
                    lan_mouse_data::MAX_MESSAGE_BYTES
                )));
                return;
            }
        }
        if self.data_jobs.len() >= 32 {
            self.notify_frontend(FrontendEvent::Error(
                "too many pending data requests; retry later".into(),
            ));
            return;
        }
        if !self
            .allowed_fingerprints
            .read()
            .expect("lock")
            .contains(&device)
        {
            self.notify_frontend(FrontendEvent::Error(format!(
                "device {device} is not authorized"
            )));
            return;
        }
        let sessions = self.data_sessions.clone();
        let addr = addr.or_else(|| self.sharing_addresses.get(&device).copied());
        let clients = self.client_manager.clients();
        // Progress is only meaningful for file transfers; messages and
        // connection probes finish in one step. The transfer id is
        // allocated here so progress events and the wire offer agree.
        let (paths, transfer_id, request_handle, progress) = match &action {
            DataRequestAction::Files {
                paths,
                transfer_id,
                handle,
            } => (
                paths.clone(),
                transfer_id.clone(),
                Some(*handle),
                Some(self.transfer_progress_tx.clone()),
            ),
            _ => (Vec::new(), String::new(), None, None),
        };
        self.data_jobs.spawn_local(async move {
            let entries = paths.len();
            let mut report = |sent: u64, total: u64| {
                let (Some(tx), Some(Some(handle))) = (&progress, request_handle) else {
                    return;
                };
                // A full channel means the UI is behind; dropping a
                // progress tick is harmless, blocking the transfer is
                // not.
                let _ = tx.try_send(FrontendEvent::TransferProgress {
                    handle,
                    transfer_id: transfer_id.clone(),
                    sent,
                    total,
                    entries,
                });
            };
            let paths_for_retry = paths.clone();
            let id_for_retry = transfer_id.clone();
            // The connect phase is bounded separately inside
            // `connect_data_session`; a wall-clock timeout around the
            // whole request would abort large transfers that are
            // legitimately slow.
            let request = perform_data_request(
                &sessions,
                device.clone(),
                action,
                addr,
                clients,
                &mut report,
            );
            // Only a frontend file request carries a handle, and only
            // such a request can be retried from the UI.
            let retry_handle = request_handle.flatten();
            match request.await {
                Ok(event) => event,
                Err(e) => Some(match retry_handle {
                    Some(handle) if !paths_for_retry.is_empty() => {
                        // Files cannot be retried from a toast: keep the
                        // original selection so the user can press resend.
                        FrontendEvent::TransferFailed {
                            handle,
                            transfer_id: id_for_retry,
                            reason: e,
                            paths: paths_for_retry,
                        }
                    }
                    _ => FrontendEvent::Error(format!("data request failed: {e}")),
                }),
            }
        });
    }

    /// Point incoming transfers at a new folder.
    ///
    /// The directory is created eagerly so the user learns about an
    /// unwritable location while they are still in the settings dialog
    /// instead of losing a later transfer.
    async fn set_receive_dir(&mut self, dir: std::path::PathBuf) {
        if let Some(reason) = receive_dir_problem(&dir) {
            let current = self.receive_dir.clone();
            self.notify_frontend(FrontendEvent::ReceiveDirChanged(current, Some(reason)));
            return;
        }
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            let event = FrontendEvent::ReceiveDirChanged(
                self.receive_dir.clone(),
                Some(format!("could not create {}: {e}", dir.display())),
            );
            self.notify_frontend(event);
            return;
        }
        self.receive_dir = dir.clone();
        self.config.set_receive_dir(dir.clone());
        self.save_config();
        log::info!("receive directory is now {}", dir.display());
        self.notify_frontend(FrontendEvent::ReceiveDirChanged(dir, None));
    }

    fn enumerate_data_sessions(&mut self) {
        let sessions = self
            .data_sessions
            .connected_devices()
            .into_iter()
            .filter_map(|device| {
                let addr = self.data_sessions.addr(&device)?;
                Some(DataSession {
                    name: self.device_name(&device),
                    device,
                    addr,
                })
            })
            .collect();
        self.notify_frontend(FrontendEvent::DataSessions(sessions));
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if self
            .client_manager
            .get_state(handle)
            .is_some_and(|(config, _)| !config.fix_ips.is_empty())
        {
            return;
        }
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        let status = self.enrollment.status.clone();
        self.publish_enrollment_state(&status);
        self.publish_discovered_hosts();
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::DataPortChanged(self.data_addr.port(), None));
        self.notify_frontend(FrontendEvent::ReceiveDirChanged(
            self.receive_dir.clone(),
            None,
        ));
        self.enumerate_data_sessions();
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        self.notify_frontend(FrontendEvent::CurrentTarget(self.current_target));
        self.notify_frontend(FrontendEvent::SharingState(self.sharing_last_state.clone()));
        self.notify_frontend(FrontendEvent::ClipboardStatus(self.clipboard_status));
        self.notify_frontend(FrontendEvent::Messages(
            self.messages.iter().cloned().collect(),
        ));
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture.create(handle, pos, CaptureType::EnterOnly);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        if matches!(
            event,
            FrontendEvent::MessageSent { .. }
                | FrontendEvent::MessageReceived { .. }
                | FrontendEvent::ImageMessage { .. }
        ) {
            if self.messages.len() == 500 {
                self.messages.pop_front();
            }
            self.messages.push_back(event.clone());
        }
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    /// Update the device currently receiving input.
    ///
    /// The current target is the one device being controlled right now; it
    /// differs from a client's active flag, which marks every device that
    /// *could* receive input (each sits at a screen edge). A03 asks the
    /// interface to show the current target.
    fn set_current_target(&mut self, handle: Option<ClientHandle>) {
        if self.current_target != handle {
            self.current_target = handle;
            self.notify_frontend(FrontendEvent::CurrentTarget(handle));
            self.update_clipboard_target();
        }
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys
            .write()
            .expect("lock")
            .insert(fp.clone(), desc);
        self.allowed_fingerprints.write().expect("lock").insert(fp);
        self.refresh_sharing_members();
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.revoke_group_member(&fp);
        self.authorized_keys.write().expect("lock").remove(&fp);
        self.allowed_fingerprints.write().expect("lock").remove(&fp);
        self.close_revoked_data_sessions();
        self.refresh_sharing_members();
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn close_revoked_data_sessions(&mut self) {
        for device in self.data_sessions.remove_unauthorized() {
            self.notify_frontend(FrontendEvent::DataSessionDisconnected {
                device,
                reason: "authorization revoked".into(),
            });
        }
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.current_target == Some(handle) {
            self.set_current_target(None);
        }
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        if self.current_target == Some(handle) {
            self.set_current_target(None);
        }
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn update_leave_hook(&mut self, handle: ClientHandle, leave_hook: Option<String>) {
        self.client_manager.set_leave_hook(handle, leave_hook);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn spawn_hook_command(&self, handle: ClientHandle, kind: HookKind) {
        let cmd = match kind {
            HookKind::Enter => self.client_manager.get_enter_cmd(handle),
            HookKind::Leave => self.client_manager.get_leave_cmd(handle),
        };
        let Some(cmd) = cmd else { return };
        tokio::task::spawn_local(async move {
            log::info!("spawning {kind} hook for client {handle}");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute {kind} hook for client {handle}: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{kind} hook for client {handle} ({cmd}) exited successfully");
                    } else {
                        log::warn!("{kind} hook for client {handle} ({cmd}) exited with {s}");
                    }
                }
                Err(e) => log::warn!("{kind} hook for client {handle} ({cmd}): {e}"),
            }
        });
    }
}

/// What a background data request should deliver once connected.
#[derive(Debug)]
enum DataRequestAction {
    /// Open the connection and stop there.
    ConnectOnly,
    /// Send a text or link message.
    Message {
        text: String,
        /// Configured device the user picked, when the request came
        /// from the frontend; the sent message is reported against it.
        handle: Option<ClientHandle>,
    },
    /// Announce and stream files and folders.
    Files {
        paths: Vec<std::path::PathBuf>,
        /// Id used on the wire, so progress events can carry it too.
        transfer_id: String,
        /// Configured device the user picked, when the request came
        /// from the frontend; progress is reported against it.
        handle: Option<ClientHandle>,
    },
}

/// Sink that forwards transfer chunks through the session manager.
struct SessionChunkSink<'a> {
    sessions: &'a SessionManager,
    device: &'a str,
    generation: u64,
}

impl lan_mouse_data::ChunkSink for SessionChunkSink<'_> {
    type Error = lan_mouse_data::SessionError;

    async fn send_chunk(&mut self, chunk: lan_mouse_data::FileChunk) -> Result<(), Self::Error> {
        self.sessions
            .send_chunk_at(self.device, self.generation, chunk)
            .await
    }
}

async fn perform_data_request(
    sessions: &SessionManager,
    device: String,
    action: DataRequestAction,
    addr: Option<SocketAddr>,
    clients: Vec<(lan_mouse_ipc::ClientConfig, lan_mouse_ipc::ClientState)>,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Option<FrontendEvent>, String> {
    if !sessions.is_connected(&device) {
        let mut addresses = Vec::new();
        if let Some(addr) = addr {
            addresses.push(addr);
        } else {
            for (config, state) in clients {
                addresses.extend(
                    state
                        .ips
                        .into_iter()
                        .map(|ip| SocketAddr::new(ip, lan_mouse_data::DEFAULT_DATA_PORT)),
                );
                if let Some(host) = config.hostname {
                    if let Ok(resolved) =
                        tokio::net::lookup_host((host.as_str(), lan_mouse_data::DEFAULT_DATA_PORT))
                            .await
                    {
                        addresses.extend(resolved);
                    }
                }
            }
        }
        addresses.sort_unstable();
        addresses.dedup();
        connect_data_session(sessions, &device, addresses).await?;
    }
    match action {
        DataRequestAction::ConnectOnly => Ok(None),
        DataRequestAction::Message { text, handle } => {
            let id = sessions.next_message_id();
            sessions
                .send(
                    &device,
                    DataMessage::Message {
                        id: id.clone(),
                        text: text.clone(),
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            // Report the send as such. Calling it `MessageReceived`
            // would file the user's own message under incoming on the
            // sender's side, which is exactly what A17 must not do.
            Ok(handle.map(|handle| FrontendEvent::MessageSent {
                handle,
                device,
                message_id: id,
                text,
            }))
        }
        DataRequestAction::Files {
            paths,
            transfer_id,
            handle,
        } => {
            let entries = paths.len();
            send_paths(sessions, &device, paths, &transfer_id, progress).await?;
            Ok(handle.map(|handle| FrontendEvent::TransferSent {
                handle,
                transfer_id,
                entries,
            }))
        }
    }
}

/// Walk the selected paths, announce one transfer, then stream it.
///
/// Every path is offered under its own file name, so selecting
/// `/a/report.pdf` and `/b/photos` sends `report.pdf` and `photos/...`
/// rather than flattening them into one directory.
///
/// Progress is reported through `progress` so the frontend can show a
/// moving bar instead of a silent wait. A failure reports the original
/// selection back to the caller, which is what the user must pick
/// again to retry: the prepared tree is gone by then.
async fn send_paths(
    sessions: &SessionManager,
    device: &str,
    paths: Vec<std::path::PathBuf>,
    transfer_id: &str,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Option<FrontendEvent>, String> {
    if paths.is_empty() {
        return Err("no files selected".into());
    }
    let entries = paths.len();
    let mut prepared = Vec::new();
    for path in &paths {
        let files = lan_mouse_data::prepare_transfer(path)
            .await
            .map_err(|e| format!("{}: {e}", path.display()))?;
        for entry in files {
            if prepared.len() >= lan_mouse_data::MAX_FILES_PER_OFFER {
                return Err(format!(
                    "selection exceeds {} entries",
                    lan_mouse_data::MAX_FILES_PER_OFFER
                ));
            }
            prepared.push(entry);
        }
    }
    let total = lan_mouse_data::total_bytes(&prepared);
    progress(0, total);
    let files = lan_mouse_data::offered_files(&prepared).map_err(|e| e.to_string())?;
    let (generation, mut replies) = sessions
        .transfer_replies(device)
        .map_err(|e| e.to_string())?;
    let result = async {
        sessions
            .send_at(
                device,
                generation,
                DataMessage::FileOffer {
                    transfer_id: transfer_id.to_owned(),
                    files,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        wait_transfer_reply(&mut replies, transfer_id, false).await?;

        let mut sink = SessionChunkSink { sessions, device, generation };
        let bytes = tokio::select! {
            sent = lan_mouse_data::send_transfer_with_progress(&mut sink, transfer_id, &prepared, |sent| {
                progress(sent.min(total.saturating_sub(1)), total)
            }) => sent.map_err(|e| e.to_string())?,
            reply = receive_transfer_reply(&mut replies, transfer_id, true) => {
                return Err(reply.err().unwrap_or_else(|| "premature transfer acknowledgement".into()));
            }
        };
        sessions
            .send_at(
                device,
                generation,
                DataMessage::FileComplete {
                    transfer_id: transfer_id.to_owned(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        wait_transfer_reply(&mut replies, transfer_id, true).await?;
        progress(total, total);
        log::info!("sent {bytes} bytes to {device} ({entries} selected entries)");
        Ok(None)
    }.await;
    if let Err(e) = &result {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            sessions.send_at(
                device,
                generation,
                DataMessage::FileCancel {
                    transfer_id: transfer_id.to_owned(),
                    reason: Some(e.to_string()),
                },
            ),
        )
        .await;
    }
    result
}

async fn wait_transfer_reply(
    replies: &mut tokio::sync::broadcast::Receiver<DataMessage>,
    id: &str,
    committed: bool,
) -> Result<(), String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        receive_transfer_reply(replies, id, committed),
    )
    .await
    .map_err(|_| "transfer acknowledgement timed out".to_owned())?
}

async fn receive_transfer_reply(
    replies: &mut tokio::sync::broadcast::Receiver<DataMessage>,
    id: &str,
    committed: bool,
) -> Result<(), String> {
    loop {
        match replies
            .recv()
            .await
            .map_err(|e| format!("transfer connection lost: {e}"))?
        {
            DataMessage::FileRejected {
                transfer_id,
                reason,
            } if transfer_id == id => return Err(reason),
            DataMessage::FileAccepted { transfer_id } if transfer_id == id && !committed => {
                return Ok(());
            }
            DataMessage::FileCommitted { transfer_id } if transfer_id == id && committed => {
                return Ok(());
            }
            _ => {}
        }
    }
}

/// Why a proposed receive directory is unusable, if it is.
///
/// Kept separate from the async create so the rule is testable without
/// touching the filesystem.
fn receive_dir_problem(dir: &std::path::Path) -> Option<String> {
    if dir.as_os_str().is_empty() {
        return Some("receive directory cannot be empty".into());
    }
    if !dir.is_absolute() {
        return Some("receive directory must be an absolute path".into());
    }
    None
}

async fn connect_data_session(
    sessions: &SessionManager,
    device: &str,
    addresses: Vec<SocketAddr>,
) -> Result<(), String> {
    let mut last_error = "no data address known for device".to_owned();
    for addr in addresses {
        match sessions.connect(device.to_owned(), addr).await {
            Ok(()) => return Ok(()),
            Err(e) => last_error = e.to_string(),
        }
    }
    Err(last_error)
}

/// Accept authenticated data-channel connections.
///
/// Each authenticated peer is registered with the session manager,
/// which owns the read loop and keeps the connection open while the
/// pointer is elsewhere. File and clipboard payload handling is added
/// in later tasks; text messages are routed to the frontend already.
async fn run_data_listener(
    listener: tokio::net::TcpListener,
    identity: lan_mouse_data::Identity,
    authorized: lan_mouse_data::AuthorizedFingerprints,
    sessions: SessionManager,
) {
    log::info!(
        "data channel listening on {}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "unknown".into())
    );
    loop {
        match lan_mouse_data::accept(&listener, &identity, &authorized).await {
            Ok((connection, addr)) => {
                // The peer certificate fingerprint is the stable device
                // id; the handshake already verified it against the
                // authorized set.
                let device = connection
                    .peer_fingerprint()
                    .unwrap_or_else(|| addr.to_string());
                if let Err(e) = sessions.register_inbound(device, connection, addr) {
                    log::warn!("failed to register data session from {addr}: {e}");
                }
            }
            Err(e) => log::debug!("data channel handshake failed: {e}"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum HookKind {
    Enter,
    Leave,
}

impl std::fmt::Display for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookKind::Enter => f.write_str("enter"),
            HookKind::Leave => f.write_str("leave"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lan_mouse_data::{AuthorizedFingerprints, Identity};
    use tokio::sync::mpsc;

    fn identity() -> Identity {
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["service-test".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        Identity {
            certificate: rustls::pki_types::CertificateDer::from(certificate.der().to_vec()),
            private_key: key.serialize_der(),
        }
    }

    async fn data_pair() -> (
        SessionManager,
        String,
        mpsc::Receiver<SessionEvent>,
        SocketAddr,
        SessionManager,
    ) {
        let client = identity();
        let server = identity();
        let server_device = server.fingerprint();
        let client_allowed: AuthorizedFingerprints =
            Arc::new(RwLock::new(HashSet::from([server_device.clone()])));
        let server_allowed: AuthorizedFingerprints =
            Arc::new(RwLock::new(HashSet::from([client.fingerprint()])));
        let (client_tx, mut client_rx) = mpsc::channel(1);
        let (server_tx, server_rx) = mpsc::channel(16);
        let client_sessions = SessionManager::new(client, client_allowed, client_tx);
        let server_sessions =
            SessionManager::new(server.clone(), server_allowed.clone(), server_tx);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::task::spawn_local(run_data_listener(
            listener,
            server,
            server_allowed,
            server_sessions.clone(),
        ));
        tokio::task::spawn_local(async move { while client_rx.recv().await.is_some() {} });
        (
            client_sessions,
            server_device,
            server_rx,
            addr,
            server_sessions,
        )
    }

    fn run_local(future: impl std::future::Future<Output = ()>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&runtime, async {
            tokio::time::timeout(std::time::Duration::from_secs(5), future)
                .await
                .expect("service data test timed out");
        });
    }

    #[test]
    fn receive_dir_rejects_empty_and_relative_paths() {
        assert!(receive_dir_problem(std::path::Path::new("")).is_some());
        assert!(receive_dir_problem(std::path::Path::new("relative/dir")).is_some());
        // Absolute paths are accepted on both platforms the app ships
        // on; the platform root differs, so build it from a known
        // absolute location.
        let absolute = std::env::temp_dir();
        assert!(absolute.is_absolute(), "{absolute:?}");
        assert_eq!(receive_dir_problem(&absolute), None);
    }

    #[test]
    fn transfer_reply_requires_matching_id_and_receiver_commit() {
        run_local(async {
            let (tx, mut rx) = tokio::sync::broadcast::channel(8);
            tx.send(DataMessage::FileCommitted {
                transfer_id: "other".into(),
            })
            .unwrap();
            tx.send(DataMessage::FileAccepted {
                transfer_id: "ours".into(),
            })
            .unwrap();
            tx.send(DataMessage::FileRejected {
                transfer_id: "ours".into(),
                reason: "disk full".into(),
            })
            .unwrap();
            assert_eq!(
                wait_transfer_reply(&mut rx, "ours", true).await,
                Err("disk full".into())
            );
            drop(tx);
            assert!(
                wait_transfer_reply(&mut rx, "ours", true)
                    .await
                    .unwrap_err()
                    .contains("connection lost")
            );
        });
    }

    #[test]
    fn transfer_identity_includes_peer_and_connection_generation() {
        assert_ne!(transfer_key("a", 1, "m-1"), transfer_key("b", 1, "m-1"));
        assert_ne!(transfer_key("a", 1, "m-1"), transfer_key("a", 2, "m-1"));
    }

    #[test]
    fn data_request_connects_without_input_sharing_then_sends() {
        run_local(async {
            let (sessions, device, mut events, addr, _peer) = data_pair().await;
            assert!(!sessions.is_connected(&device));
            let connected = perform_data_request(
                &sessions,
                device.clone(),
                DataRequestAction::ConnectOnly,
                Some(addr),
                vec![],
                &mut |_, _| {},
            )
            .await
            .unwrap();
            assert!(connected.is_none());
            assert!(matches!(
                events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));
            let sent = perform_data_request(
                &sessions,
                device.clone(),
                DataRequestAction::Message {
                    text: "hello".into(),
                    handle: Some(7),
                },
                None,
                vec![],
                &mut |_, _| {},
            )
            .await
            .unwrap();
            // A send is reported as a send, never as an incoming
            // message: otherwise the sender's record would file its own
            // message under the peer's incoming ones.
            let Some(FrontendEvent::MessageSent {
                handle,
                device: destination,
                message_id,
                text,
            }) = sent
            else {
                panic!("expected a sent message");
            };
            assert_eq!(handle, 7);
            assert_eq!(destination, device);
            assert_eq!(text, "hello");
            assert!(matches!(
                events.recv().await,
                Some(SessionEvent::Message { message: DataMessage::Message { id, text }, .. })
                    if id == message_id && text == "hello"
            ));
            sessions.terminate();
        });
    }

    #[test]
    fn data_connection_tries_next_candidate_after_refused_address() {
        run_local(async {
            let (sessions, device, mut events, addr, _peer) = data_pair().await;
            let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let unavailable_addr = unavailable.local_addr().unwrap();
            drop(unavailable);
            connect_data_session(&sessions, &device, vec![unavailable_addr, addr])
                .await
                .unwrap();
            assert!(sessions.is_connected(&device));
            assert!(matches!(
                events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));
            sessions.terminate();
        });
    }

    /// Sending a folder end to end through the production send path:
    /// offer, stream, complete, then rebuild on the receiving side.
    #[test]
    fn send_files_delivers_folder_contents() {
        run_local(async {
            let (sessions, device, mut events, addr, peer) = data_pair().await;
            let unique = std::process::id();
            let source = std::env::temp_dir().join(format!("sm-send-src-{unique}"));
            let dest = std::env::temp_dir().join(format!("sm-send-dst-{unique}"));
            let _ = std::fs::remove_dir_all(&source);
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::create_dir_all(source.join("docs/deep")).unwrap();
            std::fs::create_dir_all(source.join("docs/empty")).unwrap();
            std::fs::write(source.join("docs/readme.md"), b"# sharemouse").unwrap();
            let blob: Vec<u8> = (0..(lan_mouse_data::protocol::MAX_CHUNK_DATA * 2 + 7))
                .map(|i| (i % 249) as u8)
                .collect();
            std::fs::write(source.join("docs/deep/blob.bin"), &blob).unwrap();

            connect_data_session(&sessions, &device, vec![addr])
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(SessionEvent::Connected { .. })
            ));

            // Drive the send and the receive side together, matching how
            // the service and the peer would interleave.
            let manager = sessions.clone();
            let target = device.clone();
            let send_root = source.join("docs");
            let progress_samples = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let samples = progress_samples.clone();
            let expected_total = blob.len() as u64 + "# sharemouse".len() as u64;
            let send = async move {
                let mut record = move |sent: u64, total: u64| {
                    samples.borrow_mut().push((sent, total));
                };
                perform_data_request(
                    &manager,
                    target,
                    DataRequestAction::Files {
                        paths: vec![send_root],
                        transfer_id: "test-transfer".into(),
                        handle: None,
                    },
                    None,
                    vec![],
                    &mut record,
                )
                .await
                .unwrap()
            };

            let receive = async {
                let mut receiver = lan_mouse_data::TransferReceiver::new();
                let mut transfer_id = String::new();
                loop {
                    match events.recv().await.unwrap() {
                        SessionEvent::Message {
                            device,
                            message:
                                DataMessage::FileOffer {
                                    transfer_id: id,
                                    files,
                                },
                            ..
                        } => {
                            transfer_id = id.clone();
                            receiver
                                .begin(&id, &dest, &files, lan_mouse_data::ConflictPolicy::Ask)
                                .await
                                .unwrap();
                            // The sender must wait while the recipient decides.
                            assert!(
                                tokio::time::timeout(
                                    std::time::Duration::from_millis(30),
                                    events.recv()
                                )
                                .await
                                .is_err()
                            );
                            peer.send(&device, DataMessage::FileAccepted { transfer_id: id })
                                .await
                                .unwrap();
                        }
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
                        }
                        SessionEvent::Message {
                            device,
                            message: DataMessage::FileComplete { transfer_id: id },
                            ..
                        } => {
                            assert_eq!(id, transfer_id);
                            let committed = receiver.finish(&id).await.unwrap();
                            peer.send(&device, DataMessage::FileCommitted { transfer_id: id })
                                .await
                                .unwrap();
                            break committed;
                        }
                        SessionEvent::Message {
                            message: DataMessage::FileCancel { reason, .. },
                            ..
                        } => panic!("sender cancelled: {reason:?}"),
                        other => panic!("unexpected event: {other:?}"),
                    }
                }
            };
            let (sent, committed) = tokio::join!(send, receive);
            assert!(sent.is_none());
            assert_eq!(committed.len(), 2);
            assert_eq!(
                std::fs::read(dest.join("docs/readme.md")).unwrap(),
                b"# sharemouse"
            );
            assert_eq!(
                std::fs::read(dest.join("docs/deep/blob.bin")).unwrap(),
                blob
            );
            assert!(dest.join("docs/empty").is_dir());

            // Progress must reach the caller: a silent transfer leaves
            // the UI stuck at zero.
            let samples = progress_samples.borrow();
            assert!(!samples.is_empty(), "no progress reported");
            assert_eq!(samples[0], (0, expected_total), "total announced up front");
            assert_eq!(
                samples.last().copied(),
                Some((expected_total, expected_total)),
                "progress ends at the announced total: {samples:?}"
            );
            assert!(
                samples.windows(2).all(|w| w[0].0 <= w[1].0),
                "progress went backwards: {samples:?}"
            );

            let _ = std::fs::remove_dir_all(&source);
            let _ = std::fs::remove_dir_all(&dest);
            sessions.terminate();
        });
    }
}
