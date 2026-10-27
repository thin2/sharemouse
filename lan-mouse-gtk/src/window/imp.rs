use std::cell::{Cell, RefCell};

use adw::subclass::prelude::*;
use adw::{ActionRow, ComboRow, PreferencesGroup, ToastOverlay, prelude::*};
use glib::subclass::InitializingObject;
use gtk::glib::clone;
use gtk::{Button, CompositeTemplate, Entry, Image, Label, ListBox, gdk, gio, glib};

use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT, FrontendRequestWriter};

use crate::{
    authorization_window::AuthorizationWindow,
    layout_view::LayoutView,
    preferences::{Language, Palette, Theme, UiPreferences},
};

#[derive(CompositeTemplate, Default)]
#[template(resource = "/de/feschber/LanMouse/window.ui")]
pub struct Window {
    pub(super) conversation: RefCell<Option<super::conversation::Controls>>,
    pub message_targets_updating: Cell<bool>,
    pub message_unread: RefCell<std::collections::HashMap<String, usize>>,
    pub message_drafts: RefCell<std::collections::HashMap<String, String>>,
    pub composer_target: RefCell<Option<String>>,
    pub(super) enrollment: RefCell<Option<super::enrollment::Controls>>,
    pub(super) home: RefCell<Option<super::home::Controls>>,
    #[template_child]
    pub main_menu_button: TemplateChild<gtk::MenuButton>,
    #[template_child]
    pub where_label: TemplateChild<Label>,
    #[template_child]
    pub where_title: TemplateChild<Label>,
    #[template_child]
    pub where_hint: TemplateChild<gtk::Box>,
    #[template_child]
    pub status_box: TemplateChild<gtk::Box>,
    #[template_child]
    pub clipboard_row: TemplateChild<ActionRow>,
    #[template_child]
    pub start_sharing_button: TemplateChild<Button>,
    #[template_child]
    pub stop_sharing_button: TemplateChild<Button>,
    #[template_child]
    pub authorized_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub fingerprint_row: TemplateChild<ActionRow>,
    #[template_child]
    pub port_edit_apply: TemplateChild<Button>,
    #[template_child]
    pub port_edit_cancel: TemplateChild<Button>,
    #[template_child]
    pub client_list: TemplateChild<ListBox>,
    #[template_child]
    pub client_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub port_entry: TemplateChild<Entry>,
    #[template_child]
    pub hostname_copy_icon: TemplateChild<Image>,
    #[template_child]
    pub hostname_label: TemplateChild<Label>,
    #[template_child]
    pub hostname_row: TemplateChild<ActionRow>,
    #[template_child]
    pub toast_overlay: TemplateChild<ToastOverlay>,
    #[template_child]
    pub capture_emulation_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub capture_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub emulation_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub input_emulation_button: TemplateChild<Button>,
    #[template_child]
    pub input_capture_button: TemplateChild<Button>,
    #[template_child]
    pub authorized_list: TemplateChild<ListBox>,
    #[template_child]
    pub data_list: TemplateChild<ListBox>,
    #[template_child]
    pub data_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub data_port_row: TemplateChild<ActionRow>,
    #[template_child]
    pub layout_container: TemplateChild<gtk::Box>,
    #[template_child]
    pub language_row: TemplateChild<ComboRow>,
    #[template_child]
    pub theme_row: TemplateChild<ComboRow>,
    #[template_child]
    pub palette_row: TemplateChild<ComboRow>,
    pub autostart_row: RefCell<Option<adw::ActionRow>>,
    /// Settings card holding the capture/emulation warnings.
    pub settings_status_card: RefCell<Option<gtk::ListBox>>,
    #[template_child]
    pub general_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub connections_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub incoming_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub preferences_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub data_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub transfer_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub transfer_list: TemplateChild<ListBox>,
    #[template_child]
    pub transfer_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub receive_dir_row: TemplateChild<ActionRow>,
    #[template_child]
    pub message_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub message_target_row: TemplateChild<ActionRow>,
    #[template_child]
    pub message_compose_row: TemplateChild<gtk::Box>,
    #[template_child]
    pub message_peers: TemplateChild<ListBox>,
    #[template_child]
    pub message_entry: TemplateChild<Entry>,
    #[template_child]
    pub message_send_button: TemplateChild<Button>,
    #[template_child]
    pub message_clear_button: TemplateChild<Button>,
    #[template_child]
    pub message_list: TemplateChild<ListBox>,
    #[template_child]
    pub message_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub header_bar: TemplateChild<adw::HeaderBar>,
    #[template_child]
    pub add_client_content: TemplateChild<adw::ButtonContent>,
    #[template_child]
    pub authorize_content: TemplateChild<adw::ButtonContent>,
    pub clients: RefCell<Option<gio::ListStore>>,
    pub authorized: RefCell<Option<gio::ListStore>>,
    pub frontend_request_writer: RefCell<Option<FrontendRequestWriter>>,
    pub port: Cell<u16>,
    pub data_port: Cell<u16>,
    pub capture_active: Cell<bool>,
    pub emulation_active: Cell<bool>,
    pub authorization_window: RefCell<Option<AuthorizationWindow>>,
    pub preferences: RefCell<UiPreferences>,
    pub layout_view: RefCell<Option<std::rc::Rc<LayoutView>>>,
    /// Live transfer rows, keyed by the transfer id the daemon uses.
    pub transfers: RefCell<std::collections::HashMap<String, gtk::ListBoxRow>>,
    /// Current receive directory, shown in the settings row.
    pub receive_dir: RefCell<Option<std::path::PathBuf>>,
    /// Only widgets added with ActionRow::add_suffix may be removed.
    pub transfer_suffixes: RefCell<std::collections::HashMap<String, Vec<gtk::Widget>>>,
    /// Monotonic counter for locally-created transfer rows.
    /// Manual message record for this run. Deliberately in memory only:
    /// A18 requires it to be gone after a real restart.
    pub messages: RefCell<crate::message_log::MessageLog>,
    /// Identities behind the conversation list, in the same order.
    pub(super) message_targets: RefCell<Vec<super::conversation::Target>>,
    /// The device currently receiving input, if any.
    pub current_target: RefCell<Option<ClientHandle>>,
    /// When this device is being controlled, stores the controlling device's info.
    pub controlling_device: RefCell<Option<(String, std::net::SocketAddr)>>, // (fingerprint, addr)
    pub sharing_state: RefCell<lan_mouse_ipc::SharingState>,
    pub clipboard_status: Cell<lan_mouse_ipc::ClipboardStatus>,
}

#[glib::object_subclass]
impl ObjectSubclass for Window {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "LanMouseWindow";
    const ABSTRACT: bool = false;

    type Type = super::Window;
    type ParentType = adw::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[gtk::template_callbacks]
impl Window {
    #[template_callback]
    fn handle_add_client_pressed(&self, _button: &Button) {
        self.obj().request_client_create();
    }

    #[template_callback]
    fn handle_choose_receive_dir(&self, _button: &Button) {
        self.obj().choose_receive_dir();
    }
    #[template_callback]
    fn handle_send_message(&self) {
        self.obj().send_typed_message();
    }
    #[template_callback]
    fn handle_clear_messages(&self, _button: &Button) {
        self.obj().clear_messages();
    }

    #[template_callback]
    fn handle_copy_hostname(&self, _: &Button) {
        if let Ok(hostname) = hostname::get() {
            let display = gdk::Display::default().unwrap();
            let clipboard = display.clipboard();
            clipboard.set_text(hostname.to_str().expect("hostname: invalid utf8"));
            let icon = self.hostname_copy_icon.clone();
            icon.set_icon_name(Some("emblem-ok-symbolic"));
            icon.set_css_classes(&["success"]);
            glib::spawn_future_local(clone!(
                #[weak]
                icon,
                async move {
                    glib::timeout_future_seconds(1).await;
                    icon.set_icon_name(Some("edit-copy-symbolic"));
                    icon.set_css_classes(&[]);
                }
            ));
        }
    }

    #[template_callback]
    fn handle_copy_fingerprint(&self, button: &Button) {
        let fingerprint: String = self.fingerprint_row.property("subtitle");
        let display = gdk::Display::default().unwrap();
        let clipboard = display.clipboard();
        clipboard.set_text(&fingerprint);
        button.set_icon_name("emblem-ok-symbolic");
        button.set_css_classes(&["success"]);
        glib::spawn_future_local(clone!(
            #[weak]
            button,
            async move {
                glib::timeout_future_seconds(1).await;
                button.set_icon_name("edit-copy-symbolic");
                button.set_css_classes(&[]);
            }
        ));
    }

    #[template_callback]
    fn handle_port_changed(&self, _entry: &Entry) {
        self.port_edit_apply.set_visible(true);
        self.port_edit_cancel.set_visible(true);
    }

    #[template_callback]
    fn handle_port_edit_apply(&self) {
        self.obj().request_port_change();
    }

    #[template_callback]
    fn handle_port_edit_cancel(&self) {
        log::debug!("cancel port edit");
        self.port_entry
            .set_text(self.port.get().to_string().as_str());
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }

    #[template_callback]
    fn handle_emulation(&self) {
        // On macOS the emulation_status_row is hidden — capture_status_row
        // acts as the shared warning (see update_capture_emulation_status).
        // This handler still fires for the non-macOS platforms where the
        // emulation row is distinct.
        self.obj().request_emulation();
    }

    #[template_callback]
    fn handle_capture(&self) {
        #[cfg(target_os = "macos")]
        {
            use crate::macos_privacy;
            if macos_privacy::accessibility_granted() {
                // AX granted but the row is still visible => the daemon
                // subprocess bailed before AX was in place and needs a
                // fresh process. Quit + relaunch via Launch Services.
                log::info!("capture row clicked in relaunch-required state");
                macos_privacy::relaunch_bundle();
                if let Some(app) = self.obj().application() {
                    app.quit();
                }
                return;
            }
            log::info!("capture row clicked in AX-missing state, opening pane");
            macos_privacy::open_accessibility_settings();
        }
        self.obj().request_capture();
    }

    #[template_callback]
    fn handle_add_cert_fingerprint(&self, _button: &Button) {
        self.obj().open_fingerprint_dialog(None);
    }

    pub fn set_port(&self, port: u16) {
        self.port.set(port);
        if port == DEFAULT_PORT {
            self.port_entry.set_text("");
        } else {
            self.port_entry.set_text(format!("{port}").as_str());
        }
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }

    pub fn set_data_port(&self, port: u16) {
        self.data_port.set(port);
        self.data_port_row.set_subtitle(&port.to_string());
    }

    #[template_callback]
    fn handle_language_changed(&self, _pspec: &glib::ParamSpec) {
        // `notify::selected` passes the GParamSpec, not the row, so read
        // the selection from the template child. Skip no-op updates to
        // avoid rewriting the preferences file during construction.
        let language = Language::from_index(self.language_row.selected());
        if self.preferences.borrow().language != language {
            self.obj().set_language(language);
        }
    }

    #[template_callback]
    fn handle_theme_changed(&self, _pspec: &glib::ParamSpec) {
        let theme = Theme::from_index(self.theme_row.selected());
        if self.preferences.borrow().theme != theme {
            self.obj().set_theme_preference(theme);
        }
    }

    #[template_callback]
    fn handle_palette_changed(&self, _pspec: &glib::ParamSpec) {
        let palette = Palette::from_index(self.palette_row.selected());
        if self.preferences.borrow().palette != palette {
            self.obj().set_palette_preference(palette);
        }
    }
}

impl ObjectImpl for Window {
    fn constructed(&self) {
        if let Ok(hostname) = hostname::get() {
            self.hostname_label
                .set_text(hostname.to_str().expect("hostname: invalid utf8"));
        }
        self.parent_constructed();
        self.set_port(DEFAULT_PORT);
        let obj = self.obj();
        obj.setup_icon();
        obj.setup_clients();
        obj.setup_authorized();
        obj.setup_layout();
        obj.setup_preferences();
        obj.setup_home();
        obj.setup_enrollment();
        obj.setup_data_list();
        obj.setup_messages();
    }
}

impl WidgetImpl for Window {}
impl WindowImpl for Window {}
impl ApplicationWindowImpl for Window {}
impl AdwApplicationWindowImpl for Window {}
