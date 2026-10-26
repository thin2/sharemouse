use std::cell::RefCell;

use glib::Properties;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use lan_mouse_ipc::ClientHandle;

use super::ClientData;

#[derive(Properties, Default)]
#[properties(wrapper_type = super::ClientObject)]
pub struct ClientObject {
    #[property(name = "handle", get, set, type = ClientHandle, member = handle)]
    #[property(name = "hostname", get, set, type = Option<String>, member = hostname)]
    #[property(name = "port", get, set, type = u32, member = port, maximum = u16::MAX as u32)]
    #[property(name = "active", get, set, type = bool, member = active)]
    #[property(name = "alive", get, set, type = bool, member = alive)]
    #[property(name = "has-pressed-keys", get, set, type = bool, member = has_pressed_keys)]
    #[property(name = "position", get, set, type = String, member = position)]
    #[property(name = "resolving", get, set, type = bool, member = resolving)]
    #[property(name = "ips", get, set, type = Vec<String>, member = ips)]
    #[property(name = "peer-commit", get, set, type = Option<String>, member = peer_commit)]
    #[property(name = "peer-fingerprint", get, set, type = Option<String>, member = peer_fingerprint)]
    #[property(name = "is-current-target", get, set, type = bool, member = is_current_target)]
    pub data: RefCell<ClientData>,
}

#[glib::object_subclass]
impl ObjectSubclass for ClientObject {
    const NAME: &'static str = "ClientObject";
    type Type = super::ClientObject;
}

#[glib::derived_properties]
impl ObjectImpl for ClientObject {}
