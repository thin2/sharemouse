mod conversation;
mod enrollment;
mod image_messages;
mod imp;

use std::collections::HashMap;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::{Object, clone};
use gtk::{
    NoSelection, gdk, gio,
    glib::{self, closure_local},
};

use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, DEFAULT_PORT, FrontendRequest, FrontendRequestWriter,
    Position,
};

use crate::{
    authorization_window::AuthorizationWindow,
    fingerprint_window::FingerprintWindow,
    key_object::KeyObject,
    key_row::KeyRow,
    layout_view::{LayoutDevice, LayoutView},
    message_log::{Direction, Message},
    preferences::{Language, Theme, UiPreferences, tr},
};

/// Accepted message size, mirroring the service's limit so the UI can
/// refuse an oversized message before a round trip that would fail.
///
/// Kept as a local constant rather than a dependency on
/// `lan-mouse-data`: the frontend speaks IPC only, and this number is
/// part of the request contract it already validates against.
const MAX_MESSAGE_BYTES: usize = 128 * 1024;

use super::{client_object::ClientObject, client_row::ClientRow};

#[cfg(target_os = "macos")]
fn set_button_content_label(button: &gtk::Button, label: &str) {
    // The Reenable/Grant/Relaunch button wraps its icon+label in an
    // AdwButtonContent (see window.ui). Walk into it and swap the label
    // rather than GtkButton::set_label, which would replace the content
    // widget and drop the icon.
    if let Some(content) = button.child().and_downcast::<adw::ButtonContent>() {
        content.set_label(label);
    }
}

/// Replace the entries of a combo row model in place. Building a new
/// `GtkStringList` would reset the selection and emit `notify::selected`,
/// which would write the preferences file during a plain language switch.
fn update_choice_labels(row: &adw::ComboRow, labels: &[&str]) {
    let Some(model) = row.model().and_downcast::<gtk::StringList>() else {
        return;
    };
    model.splice(0, model.n_items(), labels);
}

/// Summarise conflicting paths for a dialog body.
///
/// Long transfers can conflict on hundreds of names; showing the first
/// few plus a count keeps the dialog readable while still telling the
/// user what is affected.
fn preview_paths(paths: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut out = String::new();
    for path in paths.iter().take(SHOWN) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("• ");
        out.push_str(&glib::markup_escape_text(path));
    }
    if paths.len() > SHOWN {
        out.push_str(&format!("\n… +{}", paths.len() - SHOWN));
    }
    out
}

/// Describe a selection the user is about to send.
///
/// A right-click on several files arrives as one dialog, so the dialog
/// has to say what it is about; otherwise the user sees only device
/// names and cannot tell whether the whole selection made it.
fn drop_summary(paths: &[std::path::PathBuf], language: crate::preferences::Language) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let names: Vec<String> = paths
        .iter()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned())
        })
        .collect();
    let mut summary = preview_paths(&names);
    if paths.len() > 1 {
        summary.push_str(&format!(
            "\n\n{}",
            tr(language, "transfer.item_count").replace("{n}", &paths.len().to_string()),
        ));
    }
    summary
}

/// Where one transfer stands, for the transfer list.
pub enum TransferStatus {
    Sending {
        sent: u64,
        total: u64,
    },
    Received(usize),
    Sent(usize),
    Failed {
        handle: ClientHandle,
        reason: String,
        paths: Vec<std::path::PathBuf>,
    },
}

/// Title of one message row.
///
/// Both the text and the device name are user-supplied and the title is
/// Pango markup, so a message containing `&` or `<` must be escaped or
/// it renders as an empty row.
fn message_title(message: &Message) -> String {
    let text = if message.is_link() {
        message.text.trim()
    } else {
        message.text.as_str()
    };
    glib::markup_escape_text(text).into()
}

/// Subtitle of one message row: direction plus the other device.
fn message_subtitle(message: &Message, language: Language) -> String {
    format!(
        "{} · {}",
        tr(
            language,
            match message.direction {
                Direction::Sent => "message.sent",
                Direction::Received => "message.received",
            }
        ),
        glib::markup_escape_text(&message.peer)
    )
}

impl TransferStatus {
    fn progress(&self) -> Option<f64> {
        match self {
            // An unknown total (still preparing) shows no bar rather
            // than a bogus 100%.
            Self::Sending { sent, total } if *total > 0 => {
                Some((*sent as f64 / *total as f64).clamp(0.0, 1.0))
            }
            Self::Received(_) | Self::Sent(_) => Some(1.0),
            _ => None,
        }
    }

    fn subtitle(&self, language: Language) -> String {
        match self {
            Self::Sending { sent, total } if *total > 0 => format!(
                "{} · {} / {}",
                tr(language, "transfer.sending"),
                human_bytes(*sent),
                human_bytes(*total),
            ),
            Self::Sending { .. } => tr(language, "transfer.sending").to_owned(),
            Self::Received(count) => format!(
                "{} · {count} {}",
                tr(language, "transfer.received"),
                tr(language, "transfer.files")
            ),
            Self::Sent(count) => format!(
                "{} · {count} {}",
                tr(language, "message.sent"),
                tr(language, "transfer.files")
            ),
            Self::Failed { reason, .. } => {
                format!("{} · {reason}", tr(language, "transfer.failed"))
            }
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::Failed { reason, .. } => Some(reason.clone()),
            _ => None,
        }
    }
}

/// Byte count formatted for humans, e.g. `1.4 MB`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub(super) fn new(app: &adw::Application, conn: FrontendRequestWriter) -> Self {
        let window: Self = Object::builder().property("application", app).build();
        window
            .imp()
            .frontend_request_writer
            .borrow_mut()
            .replace(conn);
        window
    }

    fn clients(&self) -> gio::ListStore {
        self.imp()
            .clients
            .borrow()
            .clone()
            .expect("Could not get clients")
    }

    fn authorized(&self) -> gio::ListStore {
        self.imp()
            .authorized
            .borrow()
            .clone()
            .expect("Could not get authorized")
    }

    fn client_by_idx(&self, idx: u32) -> Option<ClientObject> {
        self.clients().item(idx).map(|o| o.downcast().unwrap())
    }

    fn authorized_by_idx(&self, idx: u32) -> Option<KeyObject> {
        self.authorized().item(idx).map(|o| o.downcast().unwrap())
    }

    fn row_by_idx(&self, idx: i32) -> Option<ClientRow> {
        self.imp()
            .client_list
            .get()
            .row_at_index(idx)
            .map(|o| o.downcast().expect("expected ClientRow"))
    }

    fn setup_authorized(&self) {
        let store = gio::ListStore::new::<KeyObject>();
        self.imp().authorized.replace(Some(store));
        let selection_model = NoSelection::new(Some(self.authorized()));
        self.imp().authorized_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let key_obj = obj.downcast_ref().expect("object of type `KeyObject`");
                    let row = window.create_key_row(key_obj);
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: KeyRow| {
                                if let Some(key_obj) = window.authorized_by_idx(row.index() as u32)
                                {
                                    window.request_fingerprint_remove(key_obj.get_fingerprint());
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        )
    }

    fn setup_clients(&self) {
        let model = gio::ListStore::new::<ClientObject>();
        self.imp().clients.replace(Some(model));

        let selection_model = NoSelection::new(Some(self.clients()));
        self.imp().client_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let client_object = obj
                        .downcast_ref()
                        .expect("Expected object of type `ClientObject`.");
                    let row = window.create_client_row(client_object);
                    row.connect_closure(
                        "request-hostname-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, hostname: String| {
                                log::debug!("request-hostname-change");
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let hostname = Some(hostname).filter(|s| !s.is_empty());
                                    /* changed in response to FrontendEvent
                                     * -> do not request additional update */
                                    window.request(FrontendRequest::UpdateHostname(
                                        client.handle(),
                                        hostname,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-port-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, port: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::UpdatePort(
                                        client.handle(),
                                        port as u16,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-activate",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, active: bool| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    log::debug!(
                                        "request: {} client",
                                        if active { "activating" } else { "deactivating" }
                                    );
                                    window.request(FrontendRequest::Activate(
                                        client.handle(),
                                        active,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::Delete(client.handle()));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-send-files",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.choose_files_to_send(client.handle());
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-dns",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::ResolveDns(
                                        client.get_data().handle,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-position-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, pos_idx: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let position = match pos_idx {
                                        0 => Position::Left,
                                        1 => Position::Right,
                                        2 => Position::Top,
                                        _ => Position::Bottom,
                                    };
                                    window.request(FrontendRequest::UpdatePosition(
                                        client.handle(),
                                        position,
                                    ));
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        );
    }

    fn setup_icon(&self) {
        self.set_icon_name(Some("de.feschber.LanMouse"));
    }

    fn setup_preferences(&self) {
        let preferences = UiPreferences::load();
        self.imp().preferences.replace(preferences);
        // Set the stored values before the template callbacks can fire,
        // then apply them without writing the file again.
        self.imp()
            .language_row
            .set_selected(preferences.language.index());
        self.imp().theme_row.set_selected(preferences.theme.index());
        self.set_theme(preferences.theme);
        let row = adw::ActionRow::new();
        let toggle = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(preferences.autostart)
            .build();
        row.add_suffix(&toggle);
        row.set_activatable_widget(Some(&toggle));
        self.imp().preferences_group.add(&row);
        self.imp().autostart_row.replace(Some(row));
        if let Err(e) = crate::autostart::apply(preferences.autostart) {
            self.show_toast(&e.to_string());
        }
        let weak = self.downgrade();
        toggle.connect_state_set(move |_, enabled| {
            if let Some(window) = weak.upgrade() {
                let mut prefs = *window.imp().preferences.borrow();
                prefs.autostart = enabled;
                match crate::autostart::apply(enabled)
                    .map_err(|e| e.to_string())
                    .and_then(|_| prefs.save())
                {
                    Ok(()) => {
                        window.imp().preferences.replace(prefs);
                    }
                    Err(e) => {
                        let previous = window.imp().preferences.borrow().autostart;
                        if let Err(rollback) = crate::autostart::apply(previous) {
                            log::error!("could not restore login registration: {rollback}");
                        }
                        window.show_toast(&e);
                        return glib::Propagation::Stop;
                    }
                }
            }
            glib::Propagation::Proceed
        });
        self.apply_language(preferences.language);
    }

    pub(super) fn set_language(&self, language: Language) {
        let mut preferences = self.imp().preferences.borrow().to_owned();
        if preferences.language == language {
            return;
        }
        preferences.language = language;
        self.imp().preferences.replace(preferences);
        self.apply_language(language);
        if let Err(e) = preferences.save() {
            log::warn!("failed to save UI preferences: {e}");
        }
    }

    pub(super) fn set_theme_preference(&self, theme: Theme) {
        let mut preferences = self.imp().preferences.borrow().to_owned();
        if preferences.theme == theme {
            return;
        }
        preferences.theme = theme;
        self.imp().preferences.replace(preferences);
        self.set_theme(theme);
        if let Err(e) = preferences.save() {
            log::warn!("failed to save UI preferences: {e}");
        }
    }

    pub(super) fn set_theme(&self, theme: Theme) {
        let style_manager = adw::StyleManager::default();
        match theme {
            Theme::System => {
                style_manager.set_color_scheme(adw::ColorScheme::Default);
            }
            Theme::Light => {
                style_manager.set_color_scheme(adw::ColorScheme::ForceLight);
            }
            Theme::Dark => {
                style_manager.set_color_scheme(adw::ColorScheme::ForceDark);
            }
        }
    }

    fn apply_language(&self, language: Language) {
        self.apply_enrollment_language(language);
        self.update_conversation_navigation();
        let imp = self.imp();
        let menu = gio::Menu::new();
        menu.append(Some(tr(language, "app.close")), Some("window.close"));
        menu.append(Some(tr(language, "app.stop")), Some("app.stop"));
        menu.append(Some(tr(language, "app.quit")), Some("app.quit"));
        imp.main_menu_button.set_menu_model(Some(&menu));
        if let Some(row) = imp.autostart_row.borrow().as_ref() {
            row.set_title(tr(language, "pref.autostart"));
        }
        if let Some(app) = self.application() {
            crate::setup_menu(&app, language);
        }
        #[cfg(target_os = "macos")]
        crate::macos_status_item::set_language(language);
        imp.capture_emulation_group
            .set_title(tr(language, "group.status"));
        imp.capture_status_row
            .set_title(tr(language, "status.capture_off"));
        imp.capture_status_row
            .set_subtitle(tr(language, "status.capture_off_subtitle"));
        imp.emulation_status_row
            .set_title(tr(language, "status.emulation_off"));
        imp.emulation_status_row
            .set_subtitle(tr(language, "status.emulation_off_subtitle"));
        imp.fingerprint_row
            .set_title(tr(language, "general.fingerprint"));
        imp.hostname_row
            .set_title(tr(language, "general.hostname_port"));
        imp.data_port_row
            .set_title(tr(language, "general.data_port"));
        imp.language_row.set_title(tr(language, "pref.language"));
        imp.theme_row.set_title(tr(language, "pref.theme"));
        imp.preferences_group
            .set_title(tr(language, "group.preferences"));
        imp.data_group.set_title(tr(language, "group.data"));
        imp.message_group.set_title(tr(language, "group.messages"));
        imp.message_target_row
            .set_title(tr(language, "message.send_to"));
        imp.message_entry
            .set_placeholder_text(Some(tr(language, "message.placeholder")));
        imp.message_send_button
            .set_tooltip_text(Some(tr(language, "message.send")));
        imp.message_clear_button
            .set_tooltip_text(Some(tr(language, "message.clear")));
        imp.message_placeholder
            .set_title(tr(language, "message.placeholder_title"));
        imp.message_placeholder
            .set_subtitle(tr(language, "message.placeholder_subtitle"));
        imp.connections_group
            .set_title(tr(language, "group.connections"));
        imp.incoming_group.set_title(tr(language, "group.incoming"));
        imp.general_group.set_title(tr(language, "group.general"));
        imp.client_placeholder
            .set_title(tr(language, "placeholder.no_clients"));
        imp.client_placeholder
            .set_subtitle(tr(language, "placeholder.no_clients_subtitle"));
        imp.authorized_placeholder
            .set_title(tr(language, "placeholder.no_authorized"));
        imp.authorized_placeholder
            .set_subtitle(tr(language, "placeholder.no_authorized_subtitle"));
        imp.data_placeholder
            .set_title(tr(language, "placeholder.no_data"));
        imp.data_placeholder
            .set_subtitle(tr(language, "placeholder.no_data_subtitle"));
        imp.hero_subtitle.set_label(tr(language, "app.subtitle"));
        imp.add_client_content.set_label(tr(language, "button.add"));
        imp.authorize_content
            .set_label(tr(language, "button.authorize"));
        update_choice_labels(
            &imp.language_row,
            &[tr(language, "lang.en"), tr(language, "lang.zh")],
        );
        update_choice_labels(
            &imp.theme_row,
            &[
                tr(language, "theme.system"),
                tr(language, "theme.light"),
                tr(language, "theme.dark"),
            ],
        );
        if let Some(layout) = imp.layout_view.borrow().as_ref() {
            layout.set_language(language);
        }
        for row in self.client_rows() {
            row.apply_language(language);
        }
        // Message rows carry "sent"/"received" labels, so the visible
        // list has to be rebuilt in the new language.
        self.rebuild_message_list();
        self.refresh_layout();
    }

    fn client_rows(&self) -> Vec<ClientRow> {
        self.clients()
            .iter::<ClientObject>()
            .filter_map(|object| object.ok())
            .filter_map(|object| self.row_for_handle(object.handle()))
            .collect()
    }

    fn refresh_layout(&self) {
        self.refresh_sharing_status();
        let devices = self
            .clients()
            .iter::<ClientObject>()
            .filter_map(|object| object.ok())
            .map(|object| {
                let data = object.get_data();
                let position = data.position.parse().unwrap_or_default();
                LayoutDevice {
                    handle: data.handle,
                    name: object.display_name(),
                    position,
                    current: data.is_current_target,
                }
            })
            .collect();
        if let Some(layout) = self.imp().layout_view.borrow().as_ref() {
            layout.set_devices(devices);
        }
    }

    fn setup_layout(&self) {
        self.imp().stop_sharing_button.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.request_stop_sharing()
        ));
        self.imp().start_sharing_button.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.start_primary_or_connect()
        ));
        let layout = LayoutView::new(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |handle, position| {
                window.request(FrontendRequest::UpdatePosition(handle, position));
            }
        ));
        self.imp().layout_container.append(&layout.widget());
        self.imp().layout_view.replace(Some(layout));
    }

    /// workaround for a bug in libadwaita that shows an ugly line beneath
    /// the last element if a placeholder is set.
    /// https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6308
    fn update_placeholder_visibility(&self) {
        let visible = self.clients().n_items() == 0;
        let placeholder = self.imp().client_placeholder.get();
        self.imp().client_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn update_auth_placeholder_visibility(&self) {
        let visible = self.authorized().n_items() == 0;
        let placeholder = self.imp().authorized_placeholder.get();
        self.imp().authorized_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn create_client_row(&self, client_object: &ClientObject) -> ClientRow {
        let row = ClientRow::new(client_object);
        row.bind(client_object);
        let language = self.imp().preferences.borrow().language;
        row.apply_language(language);
        row.set_status(
            client_object.is_current_target(),
            client_object.property::<bool>("alive"),
            language,
        );
        row
    }

    fn create_key_row(&self, key_object: &KeyObject) -> KeyRow {
        let row = KeyRow::new();
        row.bind(key_object);
        row
    }

    pub(super) fn new_client(
        &self,
        handle: ClientHandle,
        client: ClientConfig,
        state: ClientState,
    ) {
        let client = ClientObject::new(handle, client, state.clone());
        client.set_is_current_target(*self.imp().current_target.borrow() == Some(handle));
        self.clients().append(&client);
        self.update_placeholder_visibility();
        self.update_dns_state(handle, !state.ips.is_empty());
        self.refresh_layout();
        self.refresh_message_targets();
    }

    pub(super) fn update_client_list(
        &self,
        clients: Vec<(ClientHandle, ClientConfig, ClientState)>,
    ) {
        for (handle, client, state) in clients {
            if self.client_idx(handle).is_some() {
                self.update_client_config(handle, client);
                self.update_client_state(handle, state);
            } else {
                self.new_client(handle, client, state);
            }
        }
        self.refresh_layout();
    }

    pub(super) fn set_data_sessions(&self, sessions: Vec<lan_mouse_ipc::DataSession>) {
        let list = self.imp().data_list.get();
        let empty = sessions.is_empty();
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        let language = self.imp().preferences.borrow().language;
        for session in sessions {
            let row = adw::ActionRow::builder()
                .title(
                    session
                        .name
                        .clone()
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or_else(|| tr(language, "client.unnamed").to_string()),
                )
                .subtitle(format!(
                    "{} · {}",
                    session.addr,
                    tr(language, "data.linked")
                ))
                .icon_name("network-wired-symbolic")
                .build();
            list.append(&row);
        }
        // libadwaita draws a stray separator under the last row when a
        // placeholder is present; only install it for an empty list.
        list.set_placeholder(empty.then_some(&*self.imp().data_placeholder));
    }

    fn setup_data_list(&self) {
        self.imp()
            .data_list
            .set_placeholder(Some(&*self.imp().data_placeholder));
    }

    pub(super) fn show_message(&self, device: &str, text: &str) {
        let preview: String = text.chars().take(120).collect();
        self.show_toast(format!("{device}: {preview}").as_str());
    }

    // --- manual messages (task 008) ---

    /// Prepare the message panel: target list plus the entry that
    /// keeps the send button in step with the text.
    ///
    /// Nothing here touches the system clipboard. A17 requires that
    /// sending a message never overwrites it, so the panel only ever
    /// writes what it was given to the record below.
    fn setup_messages(&self) {
        self.setup_conversation_page();
        let imp = self.imp();
        imp.message_list
            .set_placeholder(Some(&*imp.message_placeholder));
        imp.message_entry.connect_changed(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.update_message_compose_state()
        ));
        imp.message_peers.connect_row_selected(clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| {
                if !window.imp().message_targets_updating.get() {
                    window.switch_conversation_draft();
                    window.mark_conversation_read();
                    window.rebuild_message_list();
                    window.scroll_conversation_to_bottom();
                }
            }
        ));
        self.refresh_message_targets();
    }

    /// Rebuild the "send to" list from the configured devices.
    ///
    /// The selection is preserved by identity: devices reconnect and
    /// change their display name, and the user should not lose the
    /// target they picked because of it.
    fn refresh_message_targets(&self) {
        let imp = self.imp();
        let mut previous = self.selected_conversation();
        let mut targets: Vec<conversation::Target> = self
            .clients()
            .iter::<ClientObject>()
            .filter_map(|object| object.ok())
            .map(|object| conversation::Target {
                key: self.message_peer_id(object.handle()),
                name: object.display_name(),
                handle: Some(object.handle()),
            })
            .collect();
        for message in imp.messages.borrow().iter() {
            if !targets.iter().any(|target| target.key == message.peer_id) {
                targets.push(conversation::Target {
                    key: message.peer_id.clone(),
                    name: message.peer.clone(),
                    handle: None,
                });
            }
        }
        // Unread counts only belong to conversations still reachable in the sidebar.
        imp.message_unread
            .borrow_mut()
            .retain(|key, _| targets.iter().any(|target| &target.key == key));
        // Upgrade temporary keys for every newly identified device, including
        // conversations with a saved draft that are not currently selected.
        for target in &targets {
            let Some(handle) = target.handle else {
                continue;
            };
            let old = format!("handle:{handle}");
            if target.key == old {
                continue;
            }
            if previous.as_ref() == Some(&old) {
                previous = Some(target.key.clone());
            }
            if imp.composer_target.borrow().as_ref() == Some(&old) {
                imp.composer_target.replace(Some(target.key.clone()));
            }
            let draft = imp.message_drafts.borrow_mut().remove(&old);
            if let Some(draft) = draft {
                imp.message_drafts
                    .borrow_mut()
                    .insert(target.key.clone(), draft);
            }
        }
        let labels: Vec<String> = targets
            .iter()
            .map(|target| {
                let name = if targets
                    .iter()
                    .filter(|other| other.name == target.name)
                    .count()
                    > 1
                {
                    let suffix: String = target
                        .key
                        .chars()
                        .rev()
                        .take(8)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
                    format!("{} ({suffix})", target.name)
                } else {
                    target.name.clone()
                };
                let count = imp
                    .message_unread
                    .borrow()
                    .get(&target.key)
                    .copied()
                    .unwrap_or(0);
                if count == 0 {
                    name
                } else {
                    format!("{name} ({count})")
                }
            })
            .collect();
        imp.message_targets_updating.set(true);
        let selected = previous
            .as_ref()
            .and_then(|key| targets.iter().position(|t| &t.key == key));
        imp.message_targets.replace(targets);
        while let Some(child) = imp.message_peers.first_child() {
            imp.message_peers.remove(&child);
        }
        for name in labels {
            let label = gtk::Label::builder()
                .label(&name)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .width_chars(16)
                .max_width_chars(20)
                .tooltip_text(&name)
                .build();
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&label));
            imp.message_peers.append(&row);
        }
        imp.message_peers.select_row(
            imp.message_peers
                .row_at_index(selected.unwrap_or(0) as i32)
                .as_ref(),
        );
        imp.message_targets_updating.set(false);
        self.switch_conversation_draft();
        self.rebuild_message_list();
    }

    /// Handle currently picked in the conversation list.
    fn selected_message_target(&self) -> Option<ClientHandle> {
        let index = self.imp().message_peers.selected_row()?.index();
        self.imp()
            .message_targets
            .borrow()
            .get(index as usize)
            .and_then(|target| target.handle)
    }

    /// Grey out the send and clear buttons when they would do nothing.
    ///
    /// The device list starts empty and the entry starts blank, so the
    /// send button would otherwise look pressable and do nothing.
    fn update_message_compose_state(&self) {
        let imp = self.imp();
        let language = self.language();
        let has_target = self.selected_message_target().is_some();
        let has_text = !imp.message_entry.text().trim().is_empty();
        imp.message_send_button
            .set_sensitive(has_target && has_text);
        // Explain why sending is impossible instead of leaving the user
        // with a dead button and no reason.
        if !has_target {
            imp.message_target_row
                .set_subtitle(tr(language, "message.no_device"));
        } else {
            imp.message_target_row
                .set_subtitle(tr(language, "message.record_hint"));
        }
        let has_messages = !imp.messages.borrow().is_empty();
        imp.message_clear_button.set_sensitive(has_messages);
    }

    /// Send whatever is typed in the compose entry.
    fn send_typed_message(&self) {
        let imp = self.imp();
        let text = imp.message_entry.text().to_string();
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Some(handle) = self.selected_message_target() else {
            self.show_toast(tr(self.language(), "message.no_device"));
            return;
        };
        if text.len() > MAX_MESSAGE_BYTES {
            self.show_toast(
                tr(self.language(), "message.too_long")
                    .replace("{}", &MAX_MESSAGE_BYTES.to_string())
                    .as_str(),
            );
            return;
        }
        // Clear before the round trip so a slow or offline peer cannot
        // lead to a double send; the daemon reports the failure as a
        // toast and the record stays unchanged.
        imp.message_entry.set_text("");
        self.request(FrontendRequest::SendMessage {
            handle,
            text: text.to_owned(),
        });
    }

    /// Forget every message in this run's record.
    fn clear_messages(&self) {
        let language = self.language();
        let dialog = adw::MessageDialog::new(
            Some(self),
            Some(tr(language, "conversation.clear")),
            Some(tr(language, "conversation.clear_hint")),
        );
        dialog.add_response("cancel", tr(language, "transfer.conflict_cancel"));
        dialog.add_response("clear", tr(language, "conversation.clear"));
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to = window)]
                self,
                move |_, response| {
                    if response == "clear" {
                        window.request(FrontendRequest::ClearImageMessages);
                        window.reset_messages();
                    }
                }
            ),
        );
        dialog.present();
    }

    pub(crate) fn reset_messages(&self) {
        self.imp().messages.borrow_mut().clear();
        self.imp().message_unread.borrow_mut().clear();
        self.refresh_message_targets();
    }

    /// File a message in the record and show it.
    ///
    /// Memory only: A18 requires the record to be gone after a real
    /// restart, and keeping it out of any file is the simplest way to
    /// guarantee that.
    pub(super) fn record_message(
        &self,
        peer_id: &str,
        peer: &str,
        direction: Direction,
        id: &str,
        text: &str,
    ) {
        // Wire counters restart with a peer process, and both directions
        // may use the same id. Every received frame is a record.
        self.imp().messages.borrow_mut().push(Message {
            peer_id: peer_id.to_owned(),
            image: None,
            direction,
            peer: peer.to_owned(),
            text: text.to_owned(),
            id: id.to_owned(),
        });
        self.conversation_message_arrived(peer_id, direction);
    }

    /// Rebuild the visible list from the record.
    ///
    /// Rebuilding rather than appending keeps the order and the
    /// "clear" path identical, and the record is capped at
    /// [`MAX_MESSAGES`] so this stays small.
    fn rebuild_message_list(&self) {
        let imp = self.imp();
        let follow_latest = self.conversation_at_bottom();
        let list = imp.message_list.get();
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        let selected = self.selected_conversation();
        let items: Vec<Message> = imp
            .messages
            .borrow()
            .iter()
            .filter(|message| selected.as_deref() == Some(message.peer_id.as_str()))
            .cloned()
            .collect();
        let language = self.language();
        for message in &items {
            list.append(&self.create_message_row(message, language));
        }
        // libadwaita draws a stray separator under the last row when a
        // placeholder is present; only install it for an empty list.
        list.set_placeholder(items.is_empty().then_some(&*imp.message_placeholder));
        self.update_message_compose_state();
        self.update_conversation_navigation();
        if follow_latest {
            self.scroll_conversation_to_bottom();
        }
    }

    fn create_message_row(&self, message: &Message, language: Language) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .build();
        let bubble = gtk::Box::new(gtk::Orientation::Vertical, 8);
        bubble.add_css_class("message-bubble");
        bubble.add_css_class(if message.direction == Direction::Sent {
            "message-outgoing"
        } else {
            "message-incoming"
        });
        bubble.set_halign(if message.direction == Direction::Sent {
            gtk::Align::End
        } else {
            gtk::Align::Start
        });
        let caption = gtk::Label::new(None);
        caption.set_markup(&message_subtitle(message, language));
        caption.set_xalign(0.0);
        caption.set_wrap(true);
        caption.set_max_width_chars(48);
        caption.add_css_class("message-caption");
        bubble.append(&caption);
        let body = gtk::Label::new(None);
        body.set_markup(&message_title(message));
        body.set_selectable(true);
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        body.set_max_width_chars(54);
        bubble.append(&body);
        row.set_child(Some(&bubble));
        if let Some((preview, thumbnail)) = &message.image {
            if let Ok(texture) = gdk::Texture::from_file(&gio::File::for_path(thumbnail)) {
                let picture = gtk::Picture::for_paintable(&texture);
                picture.set_size_request(160, 100);
                picture.set_margin_top(6);
                picture.set_margin_bottom(6);
                bubble.append(&picture);
            }
            let file = gio::File::for_path(preview);
            let uri = file.uri().to_string();
            let window = self.clone();
            let open = gtk::Button::with_label(tr(language, "conversation.image"));
            open.connect_clicked(move |_| gtk::show_uri(Some(&window), &uri, gdk::CURRENT_TIME));
            bubble.append(&open);
        }
        // A message that is nothing but a link opens in the browser.
        // This is the only action that leaves the app, and it uses the
        // system handler rather than writing anything anywhere.
        if message.is_link() {
            let button = gtk::Button::builder()
                .icon_name("folder-open-symbolic")
                .tooltip_text(tr(language, "message.open_link"))
                .valign(gtk::Align::Center)
                .build();
            button.add_css_class("flat");
            let uri = message.text.trim().to_owned();
            let window = self.clone();
            button.connect_clicked(move |_| {
                gtk::show_uri(Some(&window), &uri, gtk::gdk::CURRENT_TIME);
            });
            bubble.append(&button);
        }
        row
    }

    pub(super) fn refresh_data_sessions(&self) {
        self.request(FrontendRequest::EnumerateDataSessions);
    }

    /// Ask how to handle an incoming transfer whose names already exist.
    ///
    /// The daemon holds the transfer until this answers, so dismissing
    /// the dialog sends the cancel policy rather than leaving the peer
    /// waiting.
    pub(super) fn ask_transfer_conflict(&self, device: &str, transfer_id: &str, paths: &[String]) {
        let language = self.imp().preferences.borrow().language;
        let dialog = adw::MessageDialog::new(
            Some(self),
            Some(tr(language, "transfer.conflict_title")),
            None,
        );
        dialog.set_body(&format!(
            "{}\n\n{device}\n\n{}",
            tr(language, "transfer.conflict_body"),
            preview_paths(paths)
        ));
        dialog.add_response("cancel", tr(language, "transfer.conflict_cancel"));
        dialog.add_response("skip", tr(language, "transfer.conflict_skip"));
        dialog.add_response("rename", tr(language, "transfer.conflict_rename"));
        dialog.add_response("overwrite", tr(language, "transfer.conflict_overwrite"));
        dialog.set_response_appearance("overwrite", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");

        let window = self.clone();
        let transfer_id = transfer_id.to_owned();
        dialog.connect_response(None, move |_, response| {
            let policy = match response {
                "overwrite" | "skip" | "rename" => response,
                _ => "cancel",
            };
            window.request(FrontendRequest::ResolveTransferConflict {
                transfer_id: transfer_id.clone(),
                policy: policy.to_owned(),
            });
        });
        dialog.present();
    }

    pub(super) fn show_transfer_complete(&self, device: &str, files: &[String]) {
        let language = self.imp().preferences.borrow().language;
        self.show_toast(
            format!(
                "{device}: {} {} {}",
                files.len(),
                tr(language, "transfer.complete"),
                tr(language, "transfer.complete_files"),
            )
            .as_str(),
        );
        // The row stays as a record of what landed; the toast alone
        // would vanish before the user looks at the window.
        self.show_transfer_status(
            &format!("incoming-{device}"),
            device,
            TransferStatus::Received(files.len()),
        );
    }

    /// Show the current receive directory in the settings row.
    pub(super) fn set_receive_dir(&self, dir: &std::path::Path, error: Option<String>) {
        if let Some(error) = error {
            self.show_toast(&error);
            return;
        }
        self.imp().receive_dir.replace(Some(dir.to_path_buf()));
        let language = self.imp().preferences.borrow().language;
        let escaped = gtk::glib::markup_escape_text(&dir.display().to_string());
        self.imp().receive_dir_row.set_subtitle(&escaped);
        self.imp()
            .receive_dir_row
            .set_tooltip_text(Some(tr(language, "transfer.receive_dir_choose")));
    }

    /// Update or create the row for one transfer.
    ///
    /// `transfer_id` is the daemon's id for outgoing transfers and a
    /// stable per-device key for incoming ones, so repeated progress
    /// events for the same transfer update a single row.
    pub(super) fn show_transfer_status(
        &self,
        transfer_id: &str,
        device: &str,
        status: TransferStatus,
    ) {
        let language = self.imp().preferences.borrow().language;
        let existing = self
            .imp()
            .transfers
            .borrow()
            .get(transfer_id)
            .and_then(|row| row.downcast_ref::<adw::ActionRow>().cloned());
        let row = match existing {
            Some(row) => row,
            None => {
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(device))
                    .build();
                self.imp().transfer_list.append(&row);
                self.imp()
                    .transfers
                    .borrow_mut()
                    .insert(transfer_id.to_owned(), row.clone().upcast());
                row
            }
        };

        // The bar and the action button are rebuilt each update so a
        // finished row cannot keep a stale progress widget around.
        row.set_subtitle(&status.subtitle(language));
        if let Some(children) = self
            .imp()
            .transfer_suffixes
            .borrow_mut()
            .remove(transfer_id)
        {
            for child in children {
                row.remove(&child);
            }
        }
        let mut suffixes: Vec<gtk::Widget> = Vec::new();
        if let Some(progress) = status.progress() {
            let bar = gtk::ProgressBar::builder()
                .show_text(false)
                .valign(gtk::Align::Center)
                .width_request(140)
                .build();
            bar.set_fraction(progress);
            row.add_suffix(&bar);
            suffixes.push(bar.upcast());
        }
        if let TransferStatus::Failed { paths, handle, .. } = &status {
            let button = gtk::Button::builder()
                .icon_name("view-refresh-symbolic")
                .tooltip_text(tr(language, "transfer.retry"))
                .valign(gtk::Align::Center)
                .build();
            button.add_css_class("flat");
            let paths = paths.clone();
            let window = self.clone();
            let handle = *handle;
            button.connect_clicked(move |_| {
                window.request_send_files(handle, paths.clone());
            });
            row.add_suffix(&button);
            suffixes.push(button.upcast());
        }
        self.imp()
            .transfer_suffixes
            .borrow_mut()
            .insert(transfer_id.to_owned(), suffixes);
        row.set_tooltip_text(status.detail().as_deref());
    }

    /// Ask which device should receive files handed over by the
    /// right-click entry point.
    ///
    /// The CLI cannot show a device picker (it may be running before
    /// the app is even up), so it forwards the paths and lets the
    /// window ask. Sharing the drop path keeps one picker and one
    /// transfer list rather than a second, CLI-only flow.
    pub(super) fn send_paths_from_cli(&self, paths: Vec<std::path::PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.present();
        self.ask_drop_target(paths.clone());
        self.request(FrontendRequest::SelectionReceived { paths });
    }
    /// Ask which device should receive the currently dropped paths.
    fn ask_drop_target(&self, paths: Vec<std::path::PathBuf>) {
        let clients: Vec<(ClientHandle, String)> = self
            .clients()
            .iter::<ClientObject>()
            .filter_map(|c| {
                let c = c.ok()?;
                let label = c
                    .property::<Option<String>>("hostname")
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| tr(self.language(), "client.unnamed").to_owned());
                Some((c.handle(), label))
            })
            .collect();
        let language = self.language();
        if clients.is_empty() {
            self.show_toast(tr(language, "transfer.no_device"));
            return;
        }
        // One device means no choice to make.
        if clients.len() == 1 {
            let (handle, _) = clients[0].clone();
            self.request_send_files(handle, paths);
            return;
        }
        let summary = drop_summary(&paths, language);
        let paths = std::cell::RefCell::new(paths);
        let dialog = adw::MessageDialog::new(
            Some(self),
            Some(tr(language, "transfer.dropped")),
            Some(&summary),
        );
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .build();
        for (_, label) in &clients {
            let row = adw::ActionRow::builder().title(label.clone()).build();
            list.append(&row);
        }
        list.select_row(list.row_at_index(0).as_ref());
        dialog.set_extra_child(Some(&list));
        dialog.add_response("cancel", tr(language, "transfer.conflict_cancel"));
        dialog.add_response("send", tr(language, "client.send_files"));
        dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("send"));
        dialog.set_close_response("cancel");

        let window = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "send" {
                paths.borrow_mut().clear();
                return;
            }
            let Some(index) = list.selected_row().map(|r| r.index()) else {
                return;
            };
            let Some((handle, _)) = clients.get(index as usize) else {
                return;
            };
            let paths = paths.take();
            window.request_send_files(*handle, paths);
        });
        dialog.present();
    }

    /// Send already-chosen paths to the device behind `handle`.
    ///
    /// The daemon creates the row through its first progress event.
    fn request_send_files(&self, handle: ClientHandle, paths: Vec<std::path::PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.show_toast(tr(self.language(), "transfer.sending"));
        self.request(FrontendRequest::SendFiles { handle, paths });
        self.show_files_page();
    }

    pub(super) fn update_transfer_progress(
        &self,
        handle: ClientHandle,
        transfer_id: &str,
        sent: u64,
        total: u64,
        entries: usize,
    ) {
        let name = self.device_name(handle);
        self.show_transfer_status(transfer_id, &name, TransferStatus::Sending { sent, total });
        log::trace!("transfer {transfer_id}: {sent}/{total} bytes ({entries} entries)");
    }

    pub(super) fn show_transfer_failed(
        &self,
        handle: ClientHandle,
        transfer_id: &str,
        reason: &str,
        paths: Vec<std::path::PathBuf>,
    ) {
        let name = self.device_name(handle);
        self.show_toast(&format!("{name}: {reason}"));
        // The retry button re-sends exactly this selection, which is
        // what makes a failed transfer recoverable without the user
        // finding the files again.
        self.show_transfer_status(
            transfer_id,
            &name,
            TransferStatus::Failed {
                handle,
                reason: reason.to_owned(),
                paths,
            },
        );
    }

    /// Display name for a configured device, falling back to its handle.
    pub(super) fn device_name(&self, handle: ClientHandle) -> String {
        self.client_object_for_handle(handle)
            .map(|object| object.display_name())
            .unwrap_or_else(|| format!("#{handle}"))
    }

    /// Display name for the device behind a data-channel fingerprint.
    ///
    /// Incoming messages are addressed by fingerprint, which means
    /// nothing to the user; the configured device with the same
    /// fingerprint gives the name shown in the record. Falling back to
    /// the fingerprint keeps the sender identifiable for a device that
    /// is authorized but not configured locally.
    pub(super) fn device_name_for_fingerprint(&self, fingerprint: &str) -> String {
        self.clients()
            .iter::<ClientObject>()
            .filter_map(|object| object.ok())
            .find(|object| {
                object
                    .get_data()
                    .peer_fingerprint
                    .as_deref()
                    .is_some_and(|known| known == fingerprint)
            })
            .map(|object| object.display_name())
            .unwrap_or_else(|| fingerprint.to_owned())
    }

    fn language(&self) -> Language {
        self.imp().preferences.borrow().language
    }

    /// Let the user pick one or more files and send them.
    ///
    /// `GtkFileDialog` is GTK 4.10 while this crate supports 4.2, so
    /// the native chooser is used instead: it is the same macOS sheet
    /// and existing on every supported platform.
    pub(super) fn choose_files_to_send(&self, handle: ClientHandle) {
        self.choose_transfer_paths(Some(handle), false);
    }

    fn choose_transfer_paths(&self, handle: Option<ClientHandle>, folder: bool) {
        let language = self.language();
        if self.clients().n_items() == 0 {
            self.show_toast(tr(language, "transfer.no_device"));
            return;
        }
        let chooser = gtk::FileChooserNative::builder()
            .title(tr(
                language,
                if folder {
                    "conversation.send_folder"
                } else {
                    "conversation.send_files"
                },
            ))
            .action(if folder {
                gtk::FileChooserAction::SelectFolder
            } else {
                gtk::FileChooserAction::Open
            })
            .accept_label(tr(language, "client.send_files"))
            .cancel_label(tr(language, "transfer.conflict_cancel"))
            .select_multiple(true)
            .modal(true)
            .transient_for(self)
            .build();
        let window = self.clone();
        chooser.connect_response(move |chooser, response| {
            if response != gtk::ResponseType::Accept {
                return;
            }
            // `files()` returns a list model; collect it first so the
            // iterator does not outlive the chooser's model.
            let paths: Vec<std::path::PathBuf> = chooser
                .files()
                .snapshot()
                .into_iter()
                .filter_map(|object| object.downcast::<gio::File>().ok())
                .filter_map(|file| file.path())
                .collect();
            if paths.is_empty() {
                return;
            }
            if let Some(handle) = handle {
                window.request_send_files(handle, paths);
            } else {
                window.ask_drop_target(paths);
            }
        });
        chooser.show();
    }

    /// Ask the user for a new receive folder.
    pub(super) fn choose_receive_dir(&self) {
        let language = self.language();
        let chooser = gtk::FileChooserNative::builder()
            .title(tr(language, "transfer.receive_dir_choose"))
            .action(gtk::FileChooserAction::SelectFolder)
            .accept_label(tr(language, "transfer.receive_dir_change"))
            .cancel_label(tr(language, "transfer.conflict_cancel"))
            .modal(true)
            .transient_for(self)
            .build();
        let window = self.clone();
        chooser.connect_response(move |chooser, response| {
            if response != gtk::ResponseType::Accept {
                return;
            }
            let Some(path) = chooser.file().and_then(|file| file.path()) else {
                return;
            };
            window.request(FrontendRequest::SetReceiveDir(path));
        });
        chooser.show();
    }

    /// Accept files dropped anywhere on the window.
    ///
    /// The drop is staged rather than sent immediately: the user still
    /// has to say which device should receive it, which is the choice
    /// A13 asks for.
    pub(super) fn setup_drop_target(&self) {
        let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let window = self.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let paths: Vec<std::path::PathBuf> = list
                .files()
                .into_iter()
                .filter_map(|file| file.path())
                .collect();
            if paths.is_empty() {
                window.show_toast(tr(window.language(), "transfer.drop_empty"));
                return false;
            }
            window.ask_drop_target(paths);
            true
        });
        self.add_controller(drop_target);
    }

    /// Populate data sessions and the requested initial state after the
    /// frontend connects. Called after the window has been constructed.
    pub(super) fn initial_sync(&self) {
        self.request(FrontendRequest::Sync);
        self.request(FrontendRequest::EnumerateDataSessions);
        // A right-click can have launched this app, in which case the
        // selection arrived before there was a window to show it in.
        self.request(FrontendRequest::TakePendingSelection);
    }

    pub(super) fn update_port(&self, port: u16, msg: Option<String>) {
        if let Some(msg) = msg {
            self.show_toast(msg.as_str());
        }
        self.imp().set_port(port);
    }

    pub(super) fn update_data_port(&self, port: u16, msg: Option<String>) {
        if let Some(msg) = msg {
            self.show_toast(msg.as_str());
        }
        self.imp().set_data_port(port);
    }

    fn client_idx(&self, handle: ClientHandle) -> Option<usize> {
        self.clients()
            .iter::<ClientObject>()
            .position(|c| c.ok().map(|c| c.handle() == handle).unwrap_or_default())
    }

    pub(super) fn delete_client(&self, handle: ClientHandle) {
        let Some(idx) = self.client_idx(handle) else {
            log::warn!("could not find client with handle {handle}");
            return;
        };

        // An unidentified device has no stable identity to retain a draft under.
        // Its numeric handle may be reused by the next configured device.
        let temporary_key = format!("handle:{handle}");
        self.imp()
            .message_drafts
            .borrow_mut()
            .remove(&temporary_key);
        if self.imp().composer_target.borrow().as_ref() == Some(&temporary_key) {
            self.imp().composer_target.take();
            self.imp().message_entry.set_text("");
        }
        self.clients().remove(idx as u32);
        if self.clients().n_items() == 0 {
            self.update_placeholder_visibility();
        }
        self.refresh_layout();
        self.refresh_message_targets();
    }

    pub(super) fn update_client_config(&self, handle: ClientHandle, client: ClientConfig) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        row.set_hostname(client.hostname);
        row.set_port(client.port);
        row.set_position(client.pos);
        self.refresh_layout();
        // The device name is the dropdown label, so a rename has to
        // reach the message target list too.
        self.refresh_message_targets();
    }

    pub(super) fn update_client_state(&self, handle: ClientHandle, state: ClientState) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        let Some(client_object) = self.client_object_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };

        /* activation state */
        row.set_active(state.active);
        client_object.set_property("alive", state.alive);
        client_object.set_property("has-pressed-keys", state.has_pressed_keys);
        let language = self.imp().preferences.borrow().language;
        row.set_status(client_object.is_current_target(), state.alive, language);

        /* dns state */
        client_object.set_resolving(state.resolving);

        self.update_dns_state(handle, !state.ips.is_empty());
        let ips = state
            .ips
            .into_iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>();
        client_object.set_ips(ips);

        /* peer build version (drives the version-match indicator) */
        client_object.set_property(
            "peer-commit",
            crate::client_object::peer_commit_to_string(state.peer_commit),
        );
        // Needed to name an incoming message's sender; the data channel
        // addresses devices by fingerprint, not by handle.
        let identity_changed = client_object.get_data().peer_fingerprint != state.peer_fingerprint;
        client_object.set_property("peer-fingerprint", state.peer_fingerprint.clone());
        row.set_peer_language(language);
        row.refresh_version_status();
        self.refresh_layout();
        if identity_changed {
            self.refresh_message_targets();
        }
    }

    fn client_object_for_handle(&self, handle: ClientHandle) -> Option<ClientObject> {
        self.client_idx(handle)
            .and_then(|i| self.client_by_idx(i as u32))
    }

    fn row_for_handle(&self, handle: ClientHandle) -> Option<ClientRow> {
        self.client_idx(handle)
            .and_then(|i| self.row_by_idx(i as i32))
    }

    fn update_dns_state(&self, handle: ClientHandle, resolved: bool) {
        if let Some(client_row) = self.row_for_handle(handle) {
            client_row.set_dns_state(resolved);
        }
    }

    fn request_port_change(&self) {
        let port = self
            .imp()
            .port_entry
            .get()
            .text()
            .as_str()
            .parse::<u16>()
            .unwrap_or(DEFAULT_PORT);
        self.request(FrontendRequest::ChangePort(port));
    }

    fn request_capture(&self) {
        self.request(FrontendRequest::EnableCapture);
    }

    fn request_emulation(&self) {
        self.request(FrontendRequest::EnableEmulation);
    }

    fn request_client_create(&self) {
        self.request(FrontendRequest::Create);
    }

    fn open_fingerprint_dialog(&self, fp: Option<String>) {
        let language = self.imp().preferences.borrow().language;
        let window = FingerprintWindow::new(fp, language);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: FingerprintWindow, desc: String, fp: String| {
                    parent.request_fingerprint_add(desc, fp);
                    w.close();
                }
            ),
        );
        window.present();
    }

    fn request_fingerprint_add(&self, desc: String, fp: String) {
        self.request(FrontendRequest::AuthorizeKey(desc, fp));
    }

    fn request_fingerprint_remove(&self, fp: String) {
        self.request(FrontendRequest::RemoveAuthorizedKey(fp));
    }

    fn request(&self, request: FrontendRequest) {
        let mut requester = self.imp().frontend_request_writer.borrow_mut();
        let requester = requester.as_mut().unwrap();
        if let Err(e) = requester.request(request) {
            log::error!("error sending message: {e}");
        };
    }

    pub(super) fn show_toast(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        self.add_toast(toast);
    }

    pub(super) fn add_toast(&self, toast: adw::Toast) {
        let toast_overlay = &self.imp().toast_overlay;
        toast_overlay.add_toast(toast);
    }

    pub(super) fn set_capture(&self, active: bool) {
        self.imp().capture_active.replace(active);
        self.update_capture_emulation_status();
    }

    pub(super) fn set_emulation(&self, active: bool) {
        self.imp().emulation_active.replace(active);
        self.update_capture_emulation_status();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn refresh_capture_emulation_status(&self) {
        self.update_capture_emulation_status();
    }

    fn update_capture_emulation_status(&self) {
        let capture = self.imp().capture_active.get();
        let emulation = self.imp().emulation_active.get();

        #[cfg(target_os = "macos")]
        {
            // On macOS, capture and emulation share the same TCC gate
            // (Accessibility). Collapse to a single warning row —
            // emulation_status_row stays hidden and capture_status_row
            // doubles as the shared status indicator. Its text and
            // button mutate based on whether we're waiting for AX or
            // waiting for the user to relaunch the app.
            let anything_off = !capture || !emulation;
            self.imp().emulation_status_row.set_visible(false);
            self.imp().capture_status_row.set_visible(anything_off);
            self.imp().capture_emulation_group.set_visible(anything_off);

            if anything_off {
                self.update_macos_warning_row_text();
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            self.imp().capture_status_row.set_visible(!capture);
            self.imp().emulation_status_row.set_visible(!emulation);
            self.imp()
                .capture_emulation_group
                .set_visible(!capture || !emulation);
        }
    }

    #[cfg(target_os = "macos")]
    fn update_macos_warning_row_text(&self) {
        let row = &self.imp().capture_status_row;
        let button = &self.imp().input_capture_button;

        if crate::macos_privacy::accessibility_granted() {
            // AX granted but capture/emulation still off → the daemon
            // subprocess bailed at startup and needs a fresh process to
            // re-initialize with the new grant in place.
            let language = self.imp().preferences.borrow().language;
            row.set_title(tr(language, "status.macos_relaunch"));
            row.set_subtitle(tr(language, "status.macos_relaunch_subtitle"));
            set_button_content_label(button, tr(language, "button.relaunch"));
        } else {
            // AX missing → send the user to System Settings.
            let language = self.imp().preferences.borrow().language;
            row.set_title(tr(language, "status.capture_off"));
            row.set_subtitle(tr(language, "status.macos_grant_subtitle"));
            set_button_content_label(button, tr(language, "button.grant"));
        }
    }

    pub(super) fn set_authorized_keys(&self, fingerprints: HashMap<String, String>) {
        let authorized = self.authorized();
        // clear list
        authorized.remove_all();
        // insert fingerprints
        for (fingerprint, description) in fingerprints {
            let key_obj = KeyObject::new(description, fingerprint);
            authorized.append(&key_obj);
        }
        self.update_auth_placeholder_visibility();
    }

    pub(super) fn set_pk_fp(&self, fingerprint: &str) {
        self.imp().fingerprint_row.set_subtitle(fingerprint);
    }

    pub(super) fn request_authorization(&self, fingerprint: &str) {
        if let Some(w) = self.imp().authorization_window.borrow_mut().take() {
            w.close();
        }
        let window = AuthorizationWindow::new(fingerprint);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: AuthorizationWindow, fp: String| {
                    w.close();
                    parent.open_fingerprint_dialog(Some(fp));
                }
            ),
        );
        window.connect_closure(
            "cancel-clicked",
            false,
            closure_local!(move |w: AuthorizationWindow| {
                w.close();
            }),
        );
        window.present();
        self.imp().authorization_window.replace(Some(window));
    }

    pub(super) fn set_current_target(&self, handle: Option<ClientHandle>) {
        self.imp().current_target.replace(handle);
        let clients = self.clients();
        let language = self.imp().preferences.borrow().language;
        for i in 0..clients.n_items() {
            if let Some(client_obj) = self.client_by_idx(i) {
                let is_current = handle == Some(client_obj.handle());
                client_obj.set_is_current_target(is_current);
                if let Some(row) = self.row_for_handle(client_obj.handle()) {
                    row.set_status(is_current, client_obj.alive(), language);
                }
            }
        }
        self.refresh_layout();
    }

    fn refresh_sharing_status(&self) {
        self.set_clipboard_status(self.imp().clipboard_status.get());
        use lan_mouse_ipc::SharingPhase;
        let language = self.imp().preferences.borrow().language;
        let state = self.imp().sharing_state.borrow();
        let phase = match state.phase {
            SharingPhase::Waiting => "sharing.waiting",
            SharingPhase::Stopping => "sharing.stopping",
            SharingPhase::Starting => "sharing.starting",
            SharingPhase::Stopped => "sharing.stopped",
            SharingPhase::Running => "sharing.running",
        };
        let primary_name = if state.local_primary {
            tr(language, "layout.this_device").to_owned()
        } else {
            state
                .primary_name
                .clone()
                .or_else(|| state.primary.clone())
                .unwrap_or_default()
        };
        let subtitle = if state.phase == SharingPhase::Running {
            format!("{}: {}", tr(language, phase), primary_name)
        } else {
            tr(language, phase).to_owned()
        };
        self.imp()
            .primary_row
            .set_title(tr(language, "sharing.primary"));
        self.imp()
            .primary_row
            .set_subtitle(&glib::markup_escape_text(
                if self.needs_connection_setup() {
                    self.connection_setup_hint()
                } else {
                    &subtitle
                },
            ));
        self.imp()
            .start_sharing_button
            .set_label(if self.needs_connection_setup() {
                self.connection_setup_button()
            } else {
                tr(language, "sharing.start")
            });
        self.imp()
            .start_sharing_button
            .set_sensitive(self.needs_connection_setup() || state.phase == SharingPhase::Stopped);
        let current = *self.imp().current_target.borrow();
        let name = current
            .and_then(|h| self.client_object_for_handle(h))
            .map(|c| c.display_name())
            .unwrap_or_else(|| tr(language, "layout.this_device").to_owned());
        self.imp()
            .current_target_row
            .set_title(tr(language, "sharing.current_target"));
        self.imp()
            .current_target_row
            .set_subtitle(&glib::markup_escape_text(&name));
        self.imp()
            .stop_sharing_button
            .set_label(tr(language, "sharing.stop"));
        self.imp()
            .stop_sharing_button
            .set_sensitive(state.phase != SharingPhase::Stopped);
    }

    pub(super) fn set_sharing_state(&self, state: lan_mouse_ipc::SharingState) {
        self.imp().sharing_state.replace(state);
        self.refresh_sharing_status();
    }

    pub(super) fn set_clipboard_status(&self, status: lan_mouse_ipc::ClipboardStatus) {
        use lan_mouse_ipc::ClipboardStatus::*;
        self.imp().clipboard_status.set(status);
        let key = match status {
            Paused => "clipboard.paused",
            Waiting => "clipboard.waiting",
            Syncing => "clipboard.syncing",
            Ready => "clipboard.ready",
            Unavailable => "clipboard.unavailable",
            Unsupported => "clipboard.unsupported",
            Failed => "clipboard.failed",
        };
        let language = self.imp().preferences.borrow().language;
        self.imp()
            .clipboard_row
            .set_title(tr(language, "clipboard.title"));
        self.imp().clipboard_row.set_subtitle(tr(language, key));
    }

    fn request_stop_sharing(&self) {
        self.request(FrontendRequest::StopSharing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_drop_shows_its_file_name_without_a_count() {
        let paths = vec![std::path::PathBuf::from("/tmp/report.pdf")];
        let summary = drop_summary(&paths, crate::preferences::Language::En);
        assert!(summary.contains("report.pdf"), "{summary}");
        // A count of one is noise; the name already says everything.
        assert!(!summary.contains("items selected"), "{summary}");
    }

    #[test]
    fn a_multi_item_drop_names_the_entries_and_the_total() {
        let paths = vec![
            std::path::PathBuf::from("/tmp/one.txt"),
            std::path::PathBuf::from("/tmp/two.txt"),
        ];
        let summary = drop_summary(&paths, crate::preferences::Language::En);
        assert!(summary.contains("one.txt"), "{summary}");
        assert!(summary.contains("two.txt"), "{summary}");
        assert!(summary.contains("2 items selected"), "{summary}");
    }

    #[test]
    fn a_large_drop_is_summarised_instead_of_listing_every_path() {
        let paths: Vec<std::path::PathBuf> = (0..40)
            .map(|i| std::path::PathBuf::from(format!("/tmp/file{i}.txt")))
            .collect();
        let summary = drop_summary(&paths, crate::preferences::Language::En);
        assert!(summary.contains("file0.txt"));
        // Only the first few names, then a remainder marker.
        assert!(summary.contains("… +35"), "{summary}");
        assert!(!summary.contains("file39.txt"), "{summary}");
        assert!(summary.contains("40 items selected"), "{summary}");
    }

    #[test]
    fn the_drop_summary_is_translated() {
        let paths = vec![
            std::path::PathBuf::from("/tmp/a.txt"),
            std::path::PathBuf::from("/tmp/b.txt"),
        ];
        let zh = drop_summary(&paths, crate::preferences::Language::Zh);
        assert!(zh.contains("已选择 2 项"), "{zh}");
    }

    #[test]
    fn an_empty_drop_has_no_summary() {
        assert_eq!(
            drop_summary(&[], crate::preferences::Language::En),
            String::new()
        );
    }

    #[test]
    fn a_path_without_a_file_name_falls_back_to_the_full_path() {
        let paths = vec![std::path::PathBuf::from("/")];
        let summary = drop_summary(&paths, crate::preferences::Language::En);
        assert!(summary.contains('/'), "{summary}");
    }

    #[test]
    fn a_drop_with_special_characters_is_escaped_for_markup() {
        let paths = vec![std::path::PathBuf::from("/tmp/a & b <c>.txt")];
        let summary = drop_summary(&paths, crate::preferences::Language::En);
        assert!(!summary.contains(" & "), "{summary}");
        assert!(summary.contains("&amp;"), "{summary}");
    }

    #[test]
    fn byte_counts_are_human_readable_at_each_scale() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn progress_is_clamped_and_absent_when_the_total_is_unknown() {
        let halfway = TransferStatus::Sending {
            sent: 50,
            total: 100,
        };
        assert_eq!(halfway.progress(), Some(0.5));

        // A preparing transfer has no total yet; showing 100% would be
        // a lie.
        let preparing = TransferStatus::Sending { sent: 0, total: 0 };
        assert_eq!(preparing.progress(), None);

        // A peer cannot announce a total smaller than what arrived.
        let inconsistent = TransferStatus::Sending {
            sent: 200,
            total: 100,
        };
        assert_eq!(inconsistent.progress(), Some(1.0));

        assert_eq!(TransferStatus::Received(3).progress(), Some(1.0));
        assert_eq!(
            TransferStatus::Failed {
                handle: 1,
                reason: "offline".into(),
                paths: Vec::new(),
            }
            .progress(),
            None
        );
    }

    #[test]
    fn subtitles_distinguish_sending_received_and_failed() {
        let language = Language::En;
        let sending = TransferStatus::Sending {
            sent: 1024,
            total: 2048,
        };
        assert_eq!(sending.subtitle(language), "sending · 1.0 KB / 2.0 KB");

        assert_eq!(
            TransferStatus::Received(2).subtitle(language),
            "received · 2 file(s)"
        );

        let failed = TransferStatus::Failed {
            handle: 7,
            reason: "peer vanished".into(),
            paths: vec!["/tmp/a".into()],
        };
        assert_eq!(failed.subtitle(language), "failed · peer vanished");
        assert_eq!(failed.detail(), Some("peer vanished".to_owned()));
    }

    #[test]
    fn chinese_subtitles_use_the_shipped_translations() {
        let subtitle = TransferStatus::Received(1).subtitle(Language::Zh);
        assert!(subtitle.contains("已接收"), "{subtitle}");
        let subtitle = TransferStatus::Sending { sent: 0, total: 0 }.subtitle(Language::Zh);
        assert_eq!(subtitle, "正在发送");
    }

    #[test]
    fn every_new_transfer_key_exists_in_both_languages() {
        for key in [
            "group.transfers",
            "transfer.receive_dir",
            "transfer.receive_dir_change",
            "transfer.receive_dir_choose",
            "transfer.placeholder",
            "transfer.placeholder_subtitle",
            "transfer.sending",
            "transfer.sent",
            "transfer.failed",
            "transfer.received",
            "transfer.retry",
            "transfer.dismiss",
            "transfer.files",
            "transfer.no_device",
            "transfer.dropped",
            "transfer.drop_empty",
            "transfer.item_count",
            "transfer.dir_empty",
            "transfer.dir_relative",
            "client.send_files",
        ] {
            assert_ne!(tr(Language::En, key), "?", "missing English `{key}`");
            assert_ne!(tr(Language::Zh, key), "?", "missing Chinese `{key}`");
        }
    }

    fn message(direction: Direction, peer: &str, text: &str) -> Message {
        Message {
            peer_id: peer.to_owned(),
            image: None,
            direction,
            peer: peer.to_owned(),
            text: text.to_owned(),
            id: "id".into(),
        }
    }

    #[test]
    fn message_titles_escape_user_text_for_markup() {
        // Titles are Pango markup: an unescaped `&` makes the row
        // render as empty text, which looks like a lost message.
        let row = message(Direction::Sent, "mac", "a & b <tag>");
        let title = message_title(&row);
        assert_eq!(title, "a &amp; b &lt;tag&gt;");
        assert!(!title.contains("<tag>"), "{title}");
    }

    #[test]
    fn message_subtitles_say_which_way_the_message_went() {
        let outgoing = message(Direction::Sent, "win-desktop", "hi");
        assert_eq!(
            message_subtitle(&outgoing, Language::En),
            "sent · win-desktop"
        );
        let incoming = message(Direction::Received, "win-desktop", "hi");
        assert_eq!(
            message_subtitle(&incoming, Language::En),
            "received · win-desktop"
        );
        // The peer name is escaped too: a device name with `&` must not
        // silently swallow the directional label.
        let odd = message(Direction::Received, "R&D", "hi");
        assert_eq!(message_subtitle(&odd, Language::En), "received · R&amp;D");
    }

    #[test]
    fn message_subtitles_are_translated() {
        let incoming = message(Direction::Received, "mac", "链接");
        let subtitle = message_subtitle(&incoming, Language::Zh);
        assert!(subtitle.contains("已收到"), "{subtitle}");
        let outgoing = message(Direction::Sent, "mac", "链接");
        let subtitle = message_subtitle(&outgoing, Language::Zh);
        assert!(subtitle.contains("已发送"), "{subtitle}");
    }

    #[test]
    fn a_bare_link_row_shows_the_link_without_surrounding_space() {
        let link = message(Direction::Sent, "mac", "  https://example.com  ");
        assert!(link.is_link());
        assert_eq!(message_title(&link), "https://example.com");
    }

    #[test]
    fn a_message_containing_a_link_keeps_its_prose() {
        let prose = message(Direction::Sent, "mac", "see https://example.com first");
        assert!(!prose.is_link());
        assert_eq!(
            message_title(&prose),
            "see https://example.com first",
            "a prose message must not be reduced to its link"
        );
    }

    /// The frontend refuses oversized text before a round trip that the
    /// daemon would reject anyway; the two limits must not drift.
    #[test]
    fn the_ui_limit_matches_the_protocol_limit() {
        assert_eq!(MAX_MESSAGE_BYTES, 128 * 1024);
    }
}
