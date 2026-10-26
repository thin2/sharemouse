mod imp;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use lan_mouse_ipc::{DEFAULT_PORT, Position};

use super::ClientObject;
use crate::preferences::{Language, tr};

glib::wrapper! {
    pub struct ClientRow(ObjectSubclass<imp::ClientRow>)
    @extends gtk::ListBoxRow, gtk::Widget, adw::PreferencesRow, adw::ExpanderRow,
    @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl ClientRow {
    pub fn new(client_object: &ClientObject) -> Self {
        let client_row: Self = Object::builder().build();
        client_row
            .imp()
            .client_object
            .borrow_mut()
            .replace(client_object.clone());
        client_row
    }

    pub fn bind(&self, client_object: &ClientObject) {
        let mut bindings = self.imp().bindings.borrow_mut();

        // bind client active to switch state
        let active_binding = client_object
            .bind_property("active", &self.imp().enable_switch.get(), "state")
            .sync_create()
            .build();

        // bind client active to switch position
        let switch_position_binding = client_object
            .bind_property("active", &self.imp().enable_switch.get(), "active")
            .sync_create()
            .build();

        // bind hostname to hostname edit field
        let hostname_binding = client_object
            .bind_property("hostname", &self.imp().hostname.get(), "text")
            .transform_to(|_, v: Option<String>| {
                if let Some(hostname) = v {
                    Some(hostname)
                } else {
                    Some("".to_string())
                }
            })
            .sync_create()
            .build();

        // bind hostname to title
        // `title` is Pango markup: escape user-supplied hostnames so a
        // name containing `&` or `<` still renders instead of showing
        // an empty title.
        let title_binding = client_object
            .bind_property("hostname", self, "title")
            .transform_to(|_, v: Option<String>| {
                let markup: glib::GString = v
                    .map(|hostname| glib::markup_escape_text(&hostname))
                    .unwrap_or_else(|| {
                        "<span font_style=\"italic\" font_weight=\"light\" foreground=\"darkgrey\">no hostname!</span>".into()
                    });
                Some(markup)
            })
            .sync_create()
            .build();

        // bind port to port edit field
        let port_binding = client_object
            .bind_property("port", &self.imp().port.get(), "text")
            .transform_to(|_, v: u32| {
                if v == DEFAULT_PORT as u32 {
                    Some("".to_string())
                } else {
                    Some(v.to_string())
                }
            })
            .sync_create()
            .build();

        // bind port to subtitle
        let subtitle_binding = client_object
            .bind_property("port", self, "subtitle")
            .sync_create()
            .build();

        // bind position to selected position
        let position_binding = client_object
            .bind_property("position", &self.imp().position.get(), "selected")
            .transform_to(|_, v: String| match v.as_str() {
                "right" => Some(1u32),
                "top" => Some(2u32),
                "bottom" => Some(3u32),
                _ => Some(0u32),
            })
            .sync_create()
            .build();

        // bind resolving status to spinner visibility
        let resolve_binding = client_object
            .bind_property(
                "resolving",
                &self.imp().dns_loading_indicator.get(),
                "spinning",
            )
            .sync_create()
            .build();

        // bind ips to tooltip-text
        let ip_binding = client_object
            .bind_property("ips", &self.imp().dns_button.get(), "tooltip-text")
            .transform_to(|_, ips: Vec<String>| {
                if ips.is_empty() {
                    Some("no ip addresses associated with this client".into())
                } else {
                    Some(ips.join("\n"))
                }
            })
            .sync_create()
            .build();

        bindings.push(active_binding);
        bindings.push(switch_position_binding);
        bindings.push(hostname_binding);
        bindings.push(title_binding);
        bindings.push(port_binding);
        bindings.push(subtitle_binding);
        bindings.push(position_binding);
        bindings.push(resolve_binding);
        bindings.push(ip_binding);

        // Render the initial collapsed subtitle from whatever
        // peer_commit the ClientObject was created with. Subsequent
        // changes are pushed by `Window::update_client_state` calling
        // `refresh_version_status` after writing the new property.
        self.refresh_version_status();
    }

    pub fn unbind(&self) {
        for binding in self.imp().bindings.borrow_mut().drain(..) {
            binding.unbind();
        }
    }

    pub fn set_active(&self, active: bool) {
        self.imp().set_active(active);
    }

    pub fn set_hostname(&self, hostname: Option<String>) {
        self.imp().set_hostname(hostname);
    }

    pub fn set_port(&self, port: u16) {
        self.imp().set_port(port);
    }

    pub fn set_position(&self, pos: Position) {
        self.imp().set_pos(pos);
    }

    pub fn set_dns_state(&self, resolved: bool) {
        self.imp().set_dns_state(resolved);
    }

    /// Render the connection status chip from the current client state.
    /// `active` means "this device is the current control target";
    /// `alive` means the peer currently has a live input connection.
    pub fn set_status(&self, active: bool, alive: bool, language: Language) {
        let label = self.imp().status_label.get();
        label.remove_css_class("status-active");
        label.remove_css_class("status-online");
        label.remove_css_class("status-offline");
        let (key, class) = if active {
            ("status.active", "status-active")
        } else if alive {
            ("status.online", "status-online")
        } else {
            ("status.offline", "status-offline")
        };
        label.set_text(tr(language, key));
        label.add_css_class(class);
    }

    pub fn set_peer_language(&self, language: Language) {
        let local = crate::local_commit_str();
        let peer: Option<String> = self
            .imp()
            .client_object
            .borrow()
            .as_ref()
            .and_then(|co| co.property::<Option<String>>("peer-commit"));
        let peer_label = tr(language, "version.peer");
        let ours_label = tr(language, "version.ours");
        let unknown_label = tr(language, "version.unknown");
        let matched_label = tr(language, "version.matched");
        // `AdwPreferencesRow::subtitle` is Pango markup. Commit hashes
        // are hex today, but escaping keeps this safe if the wire
        // format ever carries anything else.
        let local = glib::markup_escape_text(&local);
        let markup = match peer.as_deref() {
            None => format!("{peer_label}: {unknown_label} · {ours_label}: {local}"),
            Some(p) if p == crate::local_commit_str() => {
                let p = glib::markup_escape_text(p);
                format!("{peer_label}: {p} · {matched_label}")
            }
            Some(p) => {
                let p = glib::markup_escape_text(p);
                format!("{peer_label}: {p} · {ours_label}: {local}")
            }
        };
        self.set_subtitle(&markup);
    }

    /// Bind language-dependent labels and the status chip to the
    /// current language. Called on construction and whenever the
    /// language selector changes.
    pub fn apply_language(&self, language: Language) {
        self.imp().language.set(language);
        self.imp()
            .enable_switch
            .set_tooltip_text(Some(tr(language, "client.enable")));
        self.imp()
            .position
            .set_title(tr(language, "client.position"));
        self.imp()
            .hostname
            .set_placeholder_text(Some(tr(language, "client.hostname")));
        self.imp()
            .port
            .set_placeholder_text(Some(tr(language, "client.port")));
        self.imp()
            .delete_row
            .set_title(tr(language, "client.delete"));
        self.imp()
            .send_files_row
            .set_title(tr(language, "client.send_files"));
        self.imp()
            .send_files_button
            .set_tooltip_text(Some(tr(language, "client.send_files")));
        self.imp()
            .dns_button
            .set_tooltip_text(Some(tr(language, "client.resolve")));
        let data = self
            .imp()
            .client_object
            .borrow()
            .as_ref()
            .map(|object| (object.is_current_target(), object.property::<bool>("alive")));
        if let Some((active, alive)) = data {
            self.set_status(active, alive, language);
        }
        self.set_peer_language(language);
    }

    /// Recompute the collapsed subtitle (Pango markup) based on the
    /// current `peer-commit` property and the local build's commit.
    /// Soft-warn semantics: a missing or mismatched peer commit
    /// surfaces as orange text but never blocks traffic. Called by
    /// the window after `update_client_state` writes the new
    /// `peer-commit`. The dns-status icon is left to its existing
    /// `set_dns_state` handler so the two indicators don't fight
    /// for the same CSS class.
    pub fn refresh_version_status(&self) {
        let language = self.imp().language.get();
        let peer: Option<String> = self
            .imp()
            .client_object
            .borrow()
            .as_ref()
            .and_then(|co| co.property::<Option<String>>("peer-commit"));
        let local = crate::local_commit_str();
        self.remove_css_class("peer-mismatch");
        self.remove_css_class("peer-match");
        self.remove_css_class("peer-unknown");
        match peer.as_deref() {
            Some(p) if p == local.as_str() => self.add_css_class("peer-match"),
            Some(_) => self.add_css_class("peer-mismatch"),
            None => self.add_css_class("peer-unknown"),
        };
        self.set_peer_language(language);
    }
}
