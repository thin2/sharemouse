use super::*;

/// Pairing lives in the "Add device" dialog: nearby hosts, a manual IP,
/// and a switch that lets other computers join this one. The backend
/// requests are the same as before; only where the controls live changed.
pub(super) struct Controls {
    dialog: adw::Window,
    title: gtk::Label,
    close: gtk::Button,
    nearby: gtk::Box,
    nearby_title: gtk::Label,
    nearby_hint: gtk::Label,
    nearby_spinner: gtk::Spinner,
    nearby_list: gtk::ListBox,
    by_ip_title: gtk::Label,
    address: gtk::Entry,
    join: gtk::Button,
    host_title: gtk::Label,
    host_switch: gtk::Switch,
    host_handler: glib::SignalHandlerId,
    ip_hint: gtk::Label,
    ip_label: gtk::Label,
    status_row: gtk::Box,
    status_label: gtk::Label,
    stop: gtk::Button,
    hosts: Vec<lan_mouse_ipc::DiscoveredHost>,
    addresses: Vec<String>,
    status: String,
}

fn eyebrow() -> gtk::Label {
    let label = gtk::Label::builder().xalign(0.0).build();
    label.add_css_class("eyebrow");
    label
}

fn section(title: &gtk::Label) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 10);
    section.append(title);
    section
}

impl Window {
    pub(super) fn setup_enrollment(&self) {
        let dialog = adw::Window::builder()
            .transient_for(self)
            .modal(true)
            .hide_on_close(true)
            .resizable(false)
            .default_width(480)
            .build();
        let escape = gtk::ShortcutController::new();
        escape.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::CallbackAction::new(|widget, _| {
                if let Some(window) = widget.downcast_ref::<gtk::Window>() {
                    window.close();
                }
                glib::Propagation::Stop
            })),
        ));
        dialog.add_controller(escape);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("add-dialog");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header.set_margin_top(26);
        header.set_margin_start(28);
        header.set_margin_end(20);
        header.set_margin_bottom(4);
        let title = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        title.add_css_class("dialog-title");
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .valign(gtk::Align::Center)
            .build();
        close.add_css_class("more");
        header.append(&title);
        header.append(&close);
        content.append(&header);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 22);
        body.set_margin_top(10);
        body.set_margin_start(28);
        body.set_margin_end(28);
        body.set_margin_bottom(28);

        let nearby_title = eyebrow();
        let nearby = section(&nearby_title);
        nearby.set_spacing(0);
        let nearby_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["nearby-list"])
            .build();
        nearby_list.set_widget_name("nearby-hosts");
        nearby_list.set_margin_top(4);
        let scan = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        scan.set_margin_top(12);
        let nearby_spinner = gtk::Spinner::new();
        let nearby_hint = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .build();
        nearby_hint.add_css_class("enrollment-status");
        scan.append(&nearby_spinner);
        scan.append(&nearby_hint);
        nearby.append(&nearby_list);
        nearby.append(&scan);
        body.append(&nearby);

        let by_ip_title = eyebrow();
        let by_ip = section(&by_ip_title);
        let field = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let address = gtk::Entry::builder().hexpand(true).build();
        address.add_css_class("round-entry");
        let join = gtk::Button::new();
        join.add_css_class("pill");
        field.append(&address);
        field.append(&join);
        by_ip.append(&field);
        body.append(&by_ip);

        let host_title = eyebrow();
        let host = section(&host_title);
        let found = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        found.add_css_class("be-found");
        let found_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        found_text.set_hexpand(true);
        let ip_label = gtk::Label::builder().xalign(0.0).selectable(true).build();
        ip_label.set_widget_name("joining-address");
        ip_label.add_css_class("be-found-ip");
        let ip_hint = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(40)
            .build();
        ip_hint.add_css_class("enrollment-status");
        found_text.append(&ip_label);
        found_text.append(&ip_hint);
        let host_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        found.append(&found_text);
        found.append(&host_switch);
        host.append(&found);
        body.append(&host);

        let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .build();
        status_label.add_css_class("enrollment-status");
        let stop = gtk::Button::builder().valign(gtk::Align::Center).build();
        stop.add_css_class("pill");
        status_row.append(&status_label);
        status_row.append(&stop);
        body.append(&status_row);

        content.append(&body);
        dialog.set_content(Some(&content));

        let window = dialog.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
        let window = self.downgrade();
        let host_handler = host_switch.connect_active_notify(move |switch| {
            if let Some(window) = window.upgrade() {
                window.request(if switch.is_active() {
                    FrontendRequest::HostGroup { port: None }
                } else {
                    FrontendRequest::StopEnrollment
                });
            }
        });
        let window = self.downgrade();
        stop.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.request(FrontendRequest::StopEnrollment);
            }
        });
        let window = self.downgrade();
        let entry = address.clone();
        join.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.request(FrontendRequest::JoinGroup {
                    address: entry.text().trim().to_string(),
                });
            }
        });
        let button = join.clone();
        address.connect_activate(move |_| button.emit_clicked());
        self.imp().enrollment.replace(Some(Controls {
            dialog,
            title,
            close,
            nearby,
            nearby_title,
            nearby_hint,
            nearby_spinner,
            nearby_list,
            by_ip_title,
            address,
            join,
            host_title,
            host_switch,
            host_handler,
            ip_hint,
            ip_label,
            status_row,
            status_label,
            stop,
            hosts: vec![],
            addresses: vec![],
            status: "idle".into(),
        }));
        self.apply_enrollment_language(self.language());
        self.refresh_sharing_status();
    }

    pub(super) fn apply_enrollment_language(&self, language: Language) {
        let controls = self.imp().enrollment.borrow();
        let Some(c) = controls.as_ref() else { return };
        let hosting = c.status == "hosting";
        c.dialog.set_title(Some(tr(language, "add.title")));
        c.title.set_text(tr(language, "add.title"));
        c.close.set_tooltip_text(Some(tr(language, "add.close")));

        // While this computer is the one being joined, it has nothing to
        // look for nearby.
        c.nearby.set_visible(!hosting);
        c.nearby_title.set_text(tr(language, "add.nearby"));
        c.nearby_hint.set_text(tr(language, "add.searching"));
        c.nearby_hint.set_visible(c.hosts.is_empty());
        c.nearby_spinner.set_visible(c.hosts.is_empty());
        c.nearby_spinner
            .set_spinning(c.hosts.is_empty() && !hosting);
        c.nearby_list.set_visible(!c.hosts.is_empty());
        let mut row = c.nearby_list.first_child();
        while let Some(current) = row {
            if let Some(button) = current
                .downcast_ref::<adw::ActionRow>()
                .and_then(|r| r.activatable_widget())
                .and_downcast::<gtk::Button>()
            {
                button.set_label(tr(language, "enrollment.connect"));
            }
            row = current.next_sibling();
        }

        c.by_ip_title.set_text(tr(language, "add.by_ip"));
        c.address
            .set_placeholder_text(Some(tr(language, "add.ip_placeholder")));
        c.join.set_label(tr(language, "enrollment.connect"));

        c.host_title.set_text(tr(language, "add.host"));
        c.host_switch
            .set_tooltip_text(Some(tr(language, "add.host")));
        c.host_switch.block_signal(&c.host_handler);
        c.host_switch.set_active(hosting);
        c.host_switch.unblock_signal(&c.host_handler);
        c.ip_label.set_text(&c.addresses.join("\n"));
        c.ip_label.set_visible(hosting && !c.addresses.is_empty());
        c.ip_hint.set_text(tr(
            language,
            match (hosting, c.addresses.is_empty()) {
                (false, _) => "add.host_note",
                (true, true) => "enrollment.no_ip",
                (true, false) => "enrollment.ip_hint",
            },
        ));

        // Hosting is already shown by the switch and the address.
        c.status_row
            .set_visible(!matches!(c.status.as_str(), "idle" | "hosting"));
        c.status_label
            .set_text(tr(language, &format!("enrollment.{}", c.status)));
        c.stop.set_label(tr(language, "enrollment.stop"));
        c.stop.set_visible(c.status == "joining");
    }

    pub(crate) fn update_discovered_hosts(&self, hosts: Vec<lan_mouse_ipc::DiscoveredHost>) {
        {
            let mut controls = self.imp().enrollment.borrow_mut();
            let Some(c) = controls.as_mut() else { return };
            if c.hosts == hosts {
                return;
            }
            while let Some(row) = c.nearby_list.first_child() {
                c.nearby_list.remove(&row);
            }
            for host in &hosts {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&host.name))
                    .subtitle(glib::markup_escape_text(&host.address))
                    .build();
                let button = gtk::Button::builder().valign(gtk::Align::Center).build();
                button.add_css_class("pill");
                button.add_css_class("suggested-action");
                row.add_suffix(&button);
                row.set_activatable_widget(Some(&button));
                let window = self.downgrade();
                let field = c.address.clone();
                let address = host.address.clone();
                button.connect_clicked(move |_| {
                    if let Some(window) = window.upgrade() {
                        field.set_text(&address);
                        window.request(FrontendRequest::JoinGroup {
                            address: address.clone(),
                        });
                    }
                });
                c.nearby_list.append(&row);
            }
            c.hosts = hosts;
        }
        self.apply_enrollment_language(self.language());
    }

    pub(crate) fn update_enrollment(&self, addresses: Vec<String>, status: String) {
        let newly_hosting = status == "hosting"
            && self
                .imp()
                .enrollment
                .borrow()
                .as_ref()
                .is_some_and(|c| c.status != "hosting");
        if let Some(c) = self.imp().enrollment.borrow_mut().as_mut() {
            c.addresses = addresses;
            c.status = status;
        }
        self.apply_enrollment_language(self.language());
        self.refresh_sharing_status();
        if newly_hosting {
            self.reveal_connection_setup();
        }
    }

    /// The pairing dialog, for callers that need to present or inspect it.
    pub(crate) fn add_device_dialog(&self) -> Option<adw::Window> {
        self.imp()
            .enrollment
            .borrow()
            .as_ref()
            .map(|c| c.dialog.clone())
    }

    pub(super) fn open_add_device(&self) {
        if let Some(dialog) = self.add_device_dialog() {
            dialog.present();
        }
    }

    pub(super) fn needs_connection_setup(&self) -> bool {
        self.clients().n_items() == 0
            && self
                .imp()
                .enrollment
                .borrow()
                .as_ref()
                .is_none_or(|c| c.status != "joined")
    }

    pub(super) fn connection_setup_hint(&self) -> &'static str {
        tr(self.language(), "enrollment.setup_hint")
    }

    pub(super) fn connection_setup_button(&self) -> &'static str {
        tr(self.language(), "home.add")
    }

    /// Open the pairing dialog with this computer accepting new devices.
    pub(super) fn show_connection_ip(&self) {
        let already_hosting = self
            .imp()
            .enrollment
            .borrow()
            .as_ref()
            .is_some_and(|c| c.status == "hosting" && !c.addresses.is_empty());
        self.reveal_connection_setup();
        if !already_hosting {
            self.request(FrontendRequest::HostGroup { port: None });
        }
    }

    pub(super) fn start_primary_or_connect(&self) {
        if self.needs_connection_setup() {
            self.show_connection_ip();
        } else {
            self.request(FrontendRequest::StartSharing);
        }
    }

    pub(crate) fn confirm_enrollment(&self, id: u64, ip: &str, fingerprint: &str, local: &str) {
        let language = self.language();
        let dialog =
            adw::MessageDialog::new(Some(self), Some(tr(language, "enrollment.confirm")), None);
        dialog.set_body(&format!(
            "{}\n\nIP: {ip}\n\n{}\n{local}\n\n{}\n{fingerprint}",
            tr(language, "enrollment.verify"),
            tr(language, "enrollment.local"),
            tr(language, "enrollment.remote")
        ));
        dialog.add_response("cancel", tr(language, "transfer.conflict_cancel"));
        dialog.add_response("approve", tr(language, "enrollment.approve"));
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let window = self.downgrade();
        dialog.connect_response(None, move |_, response| {
            if let Some(window) = window.upgrade() {
                window.request(FrontendRequest::ConfirmEnrollment {
                    id,
                    approve: response == "approve",
                });
            }
        });
        self.present();
        dialog.present();
    }
}
