use super::*;

pub(super) struct Controls {
    group: adw::PreferencesGroup,
    row: adw::ActionRow,
    entry_row: adw::ActionRow,
    address: gtk::Entry,
    host: gtk::Button,
    join: gtk::Button,
    stop: gtk::Button,
    ip_box: gtk::Box,
    ip_hint: gtk::Label,
    ip_label: gtk::Label,
    nearby: gtk::Box,
    nearby_title: gtk::Label,
    nearby_hint: gtk::Label,
    nearby_list: gtk::ListBox,
    hosts: Vec<lan_mouse_ipc::DiscoveredHost>,
    addresses: Vec<String>,
    status: String,
}
impl Window {
    pub(super) fn setup_enrollment(&self) {
        let group = adw::PreferencesGroup::new();
        let row = adw::ActionRow::new();
        let host = gtk::Button::builder().valign(gtk::Align::Center).build();
        let stop = gtk::Button::builder().valign(gtk::Align::Center).build();
        row.add_suffix(&host);
        row.add_suffix(&stop);
        group.add(&row);
        let ip_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        ip_box.set_margin_top(16);
        ip_box.set_margin_bottom(16);
        let ip_hint = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let ip_label = gtk::Label::builder().xalign(0.0).selectable(true).build();
        ip_label.set_widget_name("joining-address");
        ip_label.add_css_class("title-1");
        ip_box.append(&ip_hint);
        ip_box.append(&ip_label);
        group.add(&ip_box);
        let nearby = gtk::Box::new(gtk::Orientation::Vertical, 8);
        nearby.set_margin_top(12);
        nearby.set_margin_bottom(12);
        let nearby_title = gtk::Label::builder().xalign(0.0).build();
        nearby_title.add_css_class("heading");
        let nearby_hint = gtk::Label::builder().xalign(0.0).wrap(true).build();
        nearby_hint.add_css_class("dim-label");
        let nearby_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        nearby_list.set_widget_name("nearby-hosts");
        nearby.append(&nearby_title);
        nearby.append(&nearby_hint);
        nearby.append(&nearby_list);
        group.add(&nearby);
        let entry_row = adw::ActionRow::new();
        let address = gtk::Entry::builder()
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let join = gtk::Button::builder().valign(gtk::Align::Center).build();
        entry_row.add_suffix(&address);
        entry_row.add_suffix(&join);
        group.add(&entry_row);
        let parent = self
            .imp()
            .layout_container
            .parent()
            .and_downcast::<gtk::Box>()
            .expect("main column");
        parent.insert_child_after(&group, self.imp().layout_container.prev_sibling().as_ref());
        let window = self.downgrade();
        host.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.show_connection_ip();
            }
        });
        let window = self.downgrade();
        stop.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.request(FrontendRequest::StopEnrollment);
            }
        });
        let window = self.downgrade();
        let field = address.clone();
        join.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.request(FrontendRequest::JoinGroup {
                    address: field.text().to_string(),
                });
            }
        });
        let button = join.clone();
        address.connect_activate(move |_| button.emit_clicked());
        self.imp().enrollment.replace(Some(Controls {
            group,
            row,
            entry_row,
            address,
            host,
            join,
            stop,
            ip_box,
            ip_hint,
            ip_label,
            nearby,
            nearby_title,
            nearby_hint,
            nearby_list,
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
        c.group.set_title(tr(language, "enrollment.title"));
        c.group
            .set_description(Some(tr(language, "enrollment.help")));
        c.row
            .set_title(tr(language, &format!("enrollment.{}", c.status)));
        c.row.set_subtitle("");
        c.ip_box.set_visible(c.status == "hosting");
        c.entry_row.set_visible(c.status != "hosting");
        c.stop
            .set_sensitive(matches!(c.status.as_str(), "hosting" | "joining"));
        c.ip_hint.set_text(tr(
            language,
            if c.addresses.is_empty() {
                "enrollment.no_ip"
            } else {
                "enrollment.ip_hint"
            },
        ));
        c.ip_label.set_text(&c.addresses.join("\n"));
        c.ip_label.set_visible(!c.addresses.is_empty());
        c.nearby.set_visible(c.status != "hosting");
        c.nearby_title.set_text(tr(language, "enrollment.nearby"));
        c.nearby_hint.set_text(tr(language, "enrollment.searching"));
        c.nearby_hint.set_visible(c.hosts.is_empty());
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
        c.address
            .set_placeholder_text(Some(tr(language, "enrollment.address")));
        c.host.set_label(tr(language, "enrollment.host"));
        c.join.set_label(tr(language, "enrollment.join"));
        c.stop.set_label(tr(language, "enrollment.stop"));
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
        tr(self.language(), "enrollment.host")
    }

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
