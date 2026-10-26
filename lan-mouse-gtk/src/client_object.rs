mod imp;

use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState};

glib::wrapper! {
    pub struct ClientObject(ObjectSubclass<imp::ClientObject>);
}

impl ClientObject {
    pub fn new(handle: ClientHandle, client: ClientConfig, state: ClientState) -> Self {
        Object::builder()
            .property("handle", handle)
            .property("hostname", client.hostname)
            .property("port", client.port as u32)
            .property("position", client.pos.to_string())
            .property("active", state.active)
            .property("alive", state.alive)
            .property("has-pressed-keys", state.has_pressed_keys)
            .property(
                "ips",
                state
                    .ips
                    .iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<_>>(),
            )
            .property("resolving", state.resolving)
            .property("peer-commit", peer_commit_to_string(state.peer_commit))
            .property("peer-fingerprint", state.peer_fingerprint)
            .property("is-current-target", false)
            .build()
    }

    pub fn get_data(&self) -> ClientData {
        self.imp().data.borrow().clone()
    }

    /// Human-readable device name. Falls back to a stable, shortened
    /// handle when the peer has no hostname yet, so every card remains
    /// distinguishable in the layout editor.
    pub fn display_name(&self) -> String {
        let data = self.get_data();
        data.hostname
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| format!("#{}", data.handle))
    }
}

/// Render the 8-byte ASCII commit hash carried in
/// [`lan_mouse_ipc::ClientState::peer_commit`] as a `String`. `None`
/// in → `None` out (peer hasn't sent a Hello yet, or speaks an older
/// proto).
pub fn peer_commit_to_string(commit: Option<[u8; 8]>) -> Option<String> {
    commit.and_then(|c| std::str::from_utf8(&c).ok().map(str::to_string))
}

#[derive(Default, Clone)]
pub struct ClientData {
    pub handle: ClientHandle,
    pub hostname: Option<String>,
    pub port: u32,
    pub active: bool,
    pub alive: bool,
    pub has_pressed_keys: bool,
    pub position: String,
    pub resolving: bool,
    pub ips: Vec<String>,
    pub peer_commit: Option<String>,
    /// Fingerprint the peer proved during the input handshake.
    ///
    /// The data channel addresses devices by fingerprint while the
    /// device list is keyed by handle, so this is what lets an incoming
    /// message name its sender.
    pub peer_fingerprint: Option<String>,
    /// Whether this device is currently receiving input control.
    pub is_current_target: bool,
}
