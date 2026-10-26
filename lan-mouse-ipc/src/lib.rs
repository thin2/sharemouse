use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{env, path::Path};

use serde::{Deserialize, Serialize};

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect, try_connect};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind lan-mouse socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4242;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
    /// leave hook
    pub leave_cmd: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
            leave_cmd: None,
        }
    }
}

pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether or not the client is available for emulation
    pub alive: bool,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
    /// Peer's build short commit hash from the [`Hello`] proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
    /// Fingerprint the peer proved during the input handshake. `None`
    /// until the device has connected at least once; this is the id the
    /// data channel needs, which a display hostname cannot substitute.
    pub peer_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    EnrollmentState {
        addresses: Vec<String>,
        status: String,
    },
    EnrollmentApproval {
        id: u64,
        ip: String,
        fingerprint: String,
        local_fingerprint: String,
    },
    /// Devices on the local network currently in joining mode.
    DiscoveredHosts(Vec<DiscoveredHost>),
    PresentWindow,
    Messages(Vec<FrontendEvent>),
    MessagesCleared,
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// authenticated data-channel port for files, messages and clipboard
    DataPortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// an error occured
    Error(String),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// authorized public key fingerprints have been updated
    AuthorizedUpdated(HashMap<String, String>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt {
        fingerprint: String,
    },
    /// authenticated data session with a device was established
    DataSessionConnected(DataSession),
    /// authenticated data session with a device ended
    DataSessionDisconnected {
        device: String,
        reason: String,
    },
    /// a text or link message arrived from a device
    MessageReceived {
        device: String,
        message_id: String,
        text: String,
    },
    /// a text or link message this device sent was written to the wire
    ///
    /// Separate from [`FrontendEvent::MessageReceived`] because the
    /// sender's record must not show its own message as incoming; the
    /// peer's id would also be meaningless here.
    MessageSent {
        /// configured device the message went to
        handle: ClientHandle,
        /// Authenticated destination at send time, retained during replay.
        device: String,
        message_id: String,
        text: String,
    },
    /// current list of devices with an authenticated data session
    DataSessions(Vec<DataSession>),
    /// which device the pointer and keyboard are operating right now
    ///
    /// `None` means the input is local: either nothing is shared, or the
    /// pointer has not crossed an edge. A03 asks the interface to show
    /// the current target, so this is reported separately from the
    /// per-device `active` flag: `active` says a device is *eligible* to
    /// receive input, this says it is the one *currently* receiving it.
    CurrentTarget(Option<ClientHandle>),
    /// Group-wide input session status, independent of eligible screen edges.
    SharingState(SharingState),
    /// Preparation status only, never clipboard contents.
    ClipboardStatus(ClipboardStatus),
    ClipboardHistory(Vec<HistoryEntry>),
    HistoryError,
    HistoryCopied,
    ImageMessage {
        device: String,
        outgoing: Option<ClientHandle>,
        message_id: String,
        filename: String,
        preview: PathBuf,
        thumbnail: PathBuf,
    },
    /// an incoming transfer has destinations that already exist
    TransferConflicts {
        device: String,
        transfer_id: String,
        paths: Vec<String>,
    },
    /// an incoming transfer finished and its files were committed
    TransferComplete {
        device: String,
        files: Vec<String>,
    },
    /// The recipient confirmed successful verification and commit.
    TransferSent {
        handle: ClientHandle,
        transfer_id: String,
        entries: usize,
    },
    /// an outgoing transfer is streaming; `total` is 0 when unknown
    TransferProgress {
        /// configured device the files are going to
        handle: ClientHandle,
        transfer_id: String,
        sent: u64,
        total: u64,
        /// how many top-level entries the user selected
        entries: usize,
    },
    /// a transfer failed; the entries are what the user may resend
    TransferFailed {
        handle: ClientHandle,
        transfer_id: String,
        reason: String,
        /// absolute paths of the original selection, for retry
        paths: Vec<PathBuf>,
    },
    /// the receive directory used for incoming files
    ReceiveDirChanged(PathBuf, Option<String>),
    /// files were handed over by the right-click entry point; the
    /// frontend should ask which device receives them
    SelectTarget {
        paths: Vec<PathBuf>,
    },
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    HostGroup {
        port: Option<u16>,
    },
    JoinGroup {
        address: String,
    },
    ConfirmEnrollment {
        id: u64,
        approve: bool,
    },
    StopEnrollment,
    /// End the daemon run through the same cleanup path as Ctrl+C.
    Shutdown,
    PresentWindow,
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// change the leave hook command
    UpdateLeaveHook(u64, Option<String>),
    /// save config file
    SaveConfiguration,
    /// connect to an authorized fingerprint at an explicit data address
    ConnectDataSession {
        device: String,
        addr: SocketAddr,
    },
    /// send a text or link message, connecting to configured peers if needed
    ///
    /// Addressed by handle for the same reason as [`SendFiles`]: the
    /// frontend knows the configured device, and only the daemon holds
    /// the fingerprint its input handshake proved.
    SendMessage {
        handle: ClientHandle,
        text: String,
    },
    /// send files and folders to a device
    ///
    /// Addressed by handle rather than fingerprint: the frontend knows
    /// the configured device, and only the daemon holds the fingerprint
    /// its input handshake proved.
    SendFiles {
        handle: ClientHandle,
        paths: Vec<PathBuf>,
    },
    /// answer a pending transfer conflict with overwrite/skip/rename
    ResolveTransferConflict {
        transfer_id: String,
        policy: String,
    },
    /// change where incoming files and folders are written
    SetReceiveDir(PathBuf),
    /// enumerate devices with an open authenticated data session
    EnumerateDataSessions,
    /// ask a running app to send files, letting the user pick the device
    ///
    /// Used by the right-click entry point when no `--device` was given:
    /// the frontend owns the device picker, so the choice happens where
    /// the user can see the device names.
    SelectTarget {
        paths: Vec<PathBuf>,
    },
    /// Register this connection as a GUI and request pending selections.
    TakePendingSelection,
    /// The GUI has accepted these paths into its device picker.
    SelectionReceived {
        paths: Vec<PathBuf>,
    },
    /// Stop the group and wait for every device to release native input.
    StopSharing,
    /// Select this device as primary after the group has confirmed stop.
    StartSharing,
    ListClipboardHistory,
    ClearClipboardHistory,
    CopyHistory {
        id: u64,
    },
    SendHistory {
        id: u64,
        handle: ClientHandle,
    },
    ClearImageMessages,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: u64,
    pub preview: String,
    pub thumbnail: Option<PathBuf>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharingState {
    pub phase: SharingPhase,
    pub local_primary: bool,
    pub primary: Option<String>,
    pub primary_name: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardStatus {
    #[default]
    Paused,
    Waiting,
    Syncing,
    Ready,
    Unavailable,
    Unsupported,
    Failed,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharingPhase {
    #[default]
    Waiting,
    Stopping,
    Starting,
    Stopped,
    Running,
}

/// A device reachable over the authenticated data channel.
///
/// `device` is the certificate fingerprint, which stays stable across
/// IP and hostname changes and is the same identifier used for input
/// authorization. `name` is a display name when the device config has
/// a hostname.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSession {
    pub device: String,
    pub name: Option<String>,
    pub addr: SocketAddr,
}

/// A main device announcing joining mode on the local network.
///
/// The announcement is unauthenticated; `address` only pre-fills the
/// join request, and fingerprints are still confirmed on both devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredHost {
    /// Computer name reported by the device, for display only.
    pub name: String,
    /// Value accepted by [`FrontendRequest::JoinGroup`].
    pub address: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const LAN_MOUSE_SOCKET_NAME: &str = "lan-mouse-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(LAN_MOUSE_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(LAN_MOUSE_SOCKET_NAME))
}

/// Check if a lan-mouse service is already running by probing the IPC socket.
#[cfg(unix)]
pub fn is_service_running() -> bool {
    let Ok(socket_path) = default_socket_path() else {
        return false;
    };
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

/// Check if a lan-mouse service is already running by probing the IPC socket.
#[cfg(windows)]
pub fn is_service_running() -> bool {
    std::net::TcpStream::connect("127.0.0.1:5252").is_ok()
}
