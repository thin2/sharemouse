use super::*;

pub(super) struct Target {
    pub key: String,
    pub name: String,
    pub handle: Option<ClientHandle>,
}

pub(super) struct Controls {
    stack: gtk::Stack,
    tabs: Vec<(&'static str, &'static str, gtk::ToggleButton)>,
    scroll: gtk::ScrolledWindow,
    peers_title: gtk::Label,
    send_files: gtk::Button,
    drop_title: gtk::Label,
    drop_note: gtk::Label,
    history_title: gtk::Label,
    pick_files: gtk::Button,
    pick_folder: gtk::Button,
    general_title: gtk::Label,
    advanced_title: gtk::Label,
    advanced_note: gtk::Label,
}

fn page_scroll(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::builder()
        .maximum_size(920)
        .tightening_threshold(0)
        .child(child)
        .build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build()
}

fn column() -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.set_margin_top(10);
    column.set_margin_bottom(40);
    column.set_margin_start(28);
    column.set_margin_end(28);
    column
}

/// A section heading: title, an optional faint note, then the content.
fn section_header(column: &gtk::Box) -> (gtk::Label, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("section");
    let title = gtk::Label::new(None);
    title.add_css_class("section-title");
    let note = gtk::Label::new(None);
    note.add_css_class("section-count");
    row.append(&title);
    row.append(&note);
    column.append(&row);
    (title, note)
}

/// Move rows out of their template group into a settings card. The
/// rows keep their handlers; only their parent list changes.
fn move_rows(rows: &[&gtk::Widget], card: &gtk::ListBox) {
    for row in rows {
        if let Some(list) = row.parent().and_downcast::<gtk::ListBox>() {
            list.remove(*row);
        }
        card.append(*row);
    }
}

fn card_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

impl Window {
    pub(super) fn setup_conversation_page(&self) {
        let imp = self.imp();
        let original = imp.toast_overlay.child().expect("device page");
        imp.toast_overlay.set_child(None::<&gtk::Widget>);
        let parent = imp
            .message_group
            .parent()
            .and_downcast::<gtk::Box>()
            .expect("main column");

        // ---- messages ----
        parent.remove(&*imp.message_group);
        imp.message_group.remove(&*imp.message_target_row);
        imp.message_group.remove(&*imp.message_compose_row);
        imp.message_group.remove(&*imp.message_list);
        let compose = imp
            .message_clear_button
            .parent()
            .and_downcast::<gtk::Box>()
            .expect("composer");
        compose.remove(&*imp.message_clear_button);
        imp.message_send_button.add_css_class("pill");
        imp.message_peers.add_css_class("conversation-peers");
        imp.message_target_row.remove(&*imp.message_peers);
        let send_files = gtk::Button::builder().valign(gtk::Align::Center).build();
        send_files.add_css_class("pill");
        imp.message_target_row.add_suffix(&send_files);
        send_files.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| {
                if let Some(handle) = window.selected_message_target() {
                    window.choose_files_to_send(handle);
                }
            }
        ));
        imp.message_list.remove_css_class("boxed-list");
        imp.message_list.add_css_class("conversation-list");

        let thread = gtk::Box::new(gtk::Orientation::Vertical, 0);
        thread.set_hexpand(true);
        let header = gtk::ListBox::new();
        header.set_selection_mode(gtk::SelectionMode::None);
        header.add_css_class("thread-head");
        header.append(&*imp.message_target_row);
        thread.append(&header);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&*imp.message_list)
            .build();
        thread.append(&scroll);
        imp.message_compose_row
            .add_css_class("conversation-compose");
        thread.append(&*imp.message_compose_row);
        let peers = gtk::Box::new(gtk::Orientation::Vertical, 8);
        peers.add_css_class("conversation-sidebar");
        peers.set_width_request(210);
        let peers_title = gtk::Label::builder().xalign(0.0).build();
        peers_title.add_css_class("eyebrow");
        peers.append(&peers_title);
        peers.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&*imp.message_peers)
                .build(),
        );
        imp.message_clear_button.set_halign(gtk::Align::Start);
        imp.message_clear_button.add_css_class("flat");
        peers.append(&*imp.message_clear_button);
        let conversations = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        conversations.add_css_class("sm-card");
        conversations.add_css_class("conversation-card");
        conversations.set_overflow(gtk::Overflow::Hidden);
        conversations.append(&peers);
        conversations.append(&thread);

        // ---- files ----
        parent.remove(&*imp.transfer_group);
        imp.transfer_group.remove(&*imp.transfer_list);
        let files = column();
        let drop = gtk::Box::new(gtk::Orientation::Horizontal, 24);
        drop.add_css_class("drop-zone");
        let glyph = gtk::Image::builder()
            .icon_name("document-send-symbolic")
            .pixel_size(22)
            .valign(gtk::Align::Center)
            .build();
        glyph.add_css_class("drop-glyph");
        let drop_text = gtk::Box::new(gtk::Orientation::Vertical, 6);
        drop_text.set_hexpand(true);
        drop_text.set_valign(gtk::Align::Center);
        let drop_title = gtk::Label::builder().xalign(0.0).wrap(true).build();
        drop_title.add_css_class("drop-title");
        let drop_note = gtk::Label::builder().xalign(0.0).wrap(true).build();
        drop_note.add_css_class("drop-note");
        drop_text.append(&drop_title);
        drop_text.append(&drop_note);
        let file_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        file_actions.set_valign(gtk::Align::Center);
        let pick_files = gtk::Button::new();
        pick_files.add_css_class("pill");
        pick_files.add_css_class("suggested-action");
        let pick_folder = gtk::Button::new();
        pick_folder.add_css_class("pill");
        file_actions.append(&pick_files);
        file_actions.append(&pick_folder);
        drop.append(&glyph);
        drop.append(&drop_text);
        drop.append(&file_actions);
        files.append(&drop);
        let (history_title, _) = section_header(&files);
        imp.transfer_list.remove_css_class("boxed-list");
        imp.transfer_list.add_css_class("sm-card");
        imp.transfer_list.add_css_class("sm-list");
        files.append(&*imp.transfer_list);
        for (button, folder) in [(&pick_files, false), (&pick_folder, true)] {
            button.connect_clicked(clone!(
                #[weak(rename_to = window)]
                self,
                move |_| window.choose_transfer_paths(None, folder)
            ));
        }

        // ---- settings: general, then advanced ----
        let settings = column();
        settings.add_css_class("settings-page");
        let (general_title, _) = section_header(&settings);
        let general = card_list();
        let autostart = imp.autostart_row.borrow().clone();
        let mut rows: Vec<gtk::Widget> = vec![
            imp.theme_row.get().upcast(),
            imp.palette_row.get().upcast(),
            imp.language_row.get().upcast(),
        ];
        rows.extend(autostart.map(|row| row.upcast()));
        rows.push(imp.clipboard_row.get().upcast());
        rows.push(imp.receive_dir_row.get().upcast());
        move_rows(&rows.iter().collect::<Vec<_>>(), &general);
        parent.remove(&*imp.preferences_group);
        settings.append(&general);
        let (advanced_title, advanced_note) = section_header(&settings);
        let advanced = gtk::Box::new(gtk::Orientation::Vertical, 24);
        // Capture warnings hide themselves while both backends work, so
        // they get their own card that disappears with them.
        let status = card_list();
        move_rows(
            &[
                imp.capture_status_row.upcast_ref(),
                imp.emulation_status_row.upcast_ref(),
            ],
            &status,
        );
        parent.remove(&*imp.capture_emulation_group);
        imp.capture_emulation_group.set_visible(false);
        let identity = card_list();
        move_rows(
            &[
                imp.fingerprint_row.upcast_ref(),
                imp.hostname_row.upcast_ref(),
                imp.data_port_row.upcast_ref(),
            ],
            &identity,
        );
        parent.remove(&*imp.general_group);
        advanced.append(&status);
        advanced.append(&identity);
        imp.settings_status_card.replace(Some(status));
        for group in [
            &*imp.connections_group,
            &*imp.incoming_group,
            &*imp.data_group,
        ] {
            parent.remove(group);
            advanced.append(group);
        }
        settings.append(&advanced);

        // ---- stack and title-bar tabs ----
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(false);
        stack.add_named(&original, Some("devices"));
        stack.add_named(&conversations, Some("messages"));
        stack.add_named(&page_scroll(&files), Some("files"));
        stack.add_named(&page_scroll(&settings), Some("settings"));
        let tab_bar = gtk::Box::new(gtk::Orientation::Horizontal, 26);
        tab_bar.add_css_class("tabs");
        let mut tabs: Vec<(&'static str, &'static str, gtk::ToggleButton)> = vec![];
        for (name, key) in [
            ("devices", "conversation.devices"),
            ("messages", "group.messages"),
            ("files", "conversation.files"),
            ("settings", "conversation.settings"),
        ] {
            let tab = gtk::ToggleButton::new();
            if let Some((_, _, first)) = tabs.first() {
                tab.set_group(Some(first));
            }
            tab.connect_toggled(clone!(
                #[weak]
                stack,
                move |tab| {
                    if tab.is_active() {
                        stack.set_visible_child_name(name);
                    }
                }
            ));
            tab_bar.append(&tab);
            tabs.push((name, key, tab));
        }
        tabs[0].2.set_active(true);
        imp.header_bar.set_title_widget(Some(&tab_bar));
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("conversation-shell");
        root.append(&stack);
        imp.toast_overlay.set_child(Some(&root));
        imp.conversation.replace(Some(Controls {
            stack: stack.clone(),
            tabs,
            scroll,
            peers_title,
            send_files,
            drop_title,
            drop_note,
            history_title,
            pick_files,
            pick_folder,
            general_title,
            advanced_title,
            advanced_note,
        }));
        stack.connect_visible_child_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |stack| {
                // Programmatic page changes must move the tab marker too.
                if let Some(controls) = window.imp().conversation.borrow().as_ref() {
                    let name = stack.visible_child_name();
                    for (tab_name, _, tab) in &controls.tabs {
                        if name.as_deref() == Some(*tab_name) && !tab.is_active() {
                            tab.set_active(true);
                        }
                    }
                }
                window.mark_conversation_read();
                window.update_conversation_navigation();
            }
        ));
        self.connect_visible_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.mark_conversation_read()
        ));
        self.connect_is_active_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.mark_conversation_read()
        ));
        self.update_conversation_navigation();
    }

    pub(super) fn selected_conversation(&self) -> Option<String> {
        self.imp()
            .message_targets
            .borrow()
            .get(self.imp().message_peers.selected_row()?.index() as usize)
            .map(|target| target.key.clone())
    }

    pub(super) fn show_files_page(&self) {
        if let Some(controls) = self.imp().conversation.borrow().as_ref() {
            controls.stack.set_visible_child_name("files");
        }
    }

    pub(super) fn show_settings_page(&self) {
        if let Some(controls) = self.imp().conversation.borrow().as_ref() {
            controls.stack.set_visible_child_name("settings");
        }
    }

    /// Bring the pairing controls into view.
    pub(super) fn reveal_connection_setup(&self) {
        if let Some(controls) = self.imp().conversation.borrow().as_ref() {
            controls.stack.set_visible_child_name("devices");
        }
        self.open_add_device();
    }

    pub(super) fn switch_conversation_draft(&self) {
        let imp = self.imp();
        let selected = self.selected_conversation();
        if *imp.composer_target.borrow() == selected {
            return;
        }
        if let Some(previous) = imp.composer_target.replace(selected.clone()) {
            let text = imp.message_entry.text().to_string();
            if text.is_empty() {
                imp.message_drafts.borrow_mut().remove(&previous);
            } else {
                imp.message_drafts.borrow_mut().insert(previous, text);
            }
        }
        let draft = selected
            .and_then(|key| imp.message_drafts.borrow().get(&key).cloned())
            .unwrap_or_default();
        imp.message_entry.set_text(&draft);
    }

    pub(crate) fn message_peer_id(&self, handle: ClientHandle) -> String {
        self.client_object_for_handle(handle)
            .and_then(|object| object.get_data().peer_fingerprint)
            .unwrap_or_else(|| format!("handle:{handle}"))
    }

    fn conversation_visible(&self) -> bool {
        self.is_visible()
            && self.is_active()
            && self
                .imp()
                .conversation
                .borrow()
                .as_ref()
                .is_some_and(|controls| {
                    controls.stack.visible_child_name().as_deref() == Some("messages")
                })
    }

    pub(super) fn mark_conversation_read(&self) {
        if self.conversation_visible() {
            if let Some(key) = self.selected_conversation() {
                let removed = self
                    .imp()
                    .message_unread
                    .borrow_mut()
                    .remove(&key)
                    .is_some();
                if removed {
                    // Do not replace device rows while GTK is notifying selection.
                    glib::idle_add_local_once(clone!(
                        #[weak(rename_to = window)]
                        self,
                        move || window.refresh_message_targets()
                    ));
                }
            }
        }
    }

    pub(super) fn conversation_message_arrived(&self, key: &str, direction: Direction) {
        if direction == Direction::Received
            && !(self.conversation_visible()
                && self.selected_conversation().as_deref() == Some(key))
        {
            *self
                .imp()
                .message_unread
                .borrow_mut()
                .entry(key.into())
                .or_default() += 1;
        }
        self.refresh_message_targets();
        self.mark_conversation_read();
    }

    pub(super) fn update_conversation_navigation(&self) {
        let imp = self.imp();
        let controls = imp.conversation.borrow();
        let Some(controls) = controls.as_ref() else {
            return;
        };
        let language = self.language();
        let unread: usize = imp.message_unread.borrow().values().sum();
        for (name, key, tab) in &controls.tabs {
            let title = tr(language, key);
            tab.set_label(&if *name == "messages" && unread > 0 {
                format!("{title} ({unread})")
            } else {
                title.into()
            });
        }
        controls.peers_title.set_text(tr(language, "messages.to"));
        controls
            .send_files
            .set_label(tr(language, "conversation.send_files"));
        controls
            .send_files
            .set_sensitive(self.selected_message_target().is_some());
        let name = self.selected_conversation().and_then(|key| {
            imp.message_targets
                .borrow()
                .iter()
                .find(|target| target.key == key)
                .map(|target| target.name.clone())
        });
        // Files go to whoever the keyboard and mouse are on, if anyone.
        let target = (*imp.current_target.borrow()).map(|handle| self.device_name(handle));
        controls.drop_title.set_markup(&match target {
            Some(target) => {
                tr(language, "files.drop_title").replace("{}", &glib::markup_escape_text(&target))
            }
            None => glib::markup_escape_text(tr(language, "files.drop_any")).into(),
        });
        controls.drop_note.set_text(tr(language, "files.drop_note"));
        controls
            .history_title
            .set_text(tr(language, "files.history"));
        controls
            .pick_files
            .set_label(tr(language, "conversation.send_files"));
        controls
            .pick_folder
            .set_label(tr(language, "conversation.send_folder"));
        controls
            .general_title
            .set_text(tr(language, "settings.general"));
        controls
            .advanced_title
            .set_text(tr(language, "settings.advanced"));
        controls
            .advanced_note
            .set_text(tr(language, "settings.advanced_note"));
        imp.message_target_row.set_title(&glib::markup_escape_text(
            name.as_deref()
                .unwrap_or(tr(language, "conversation.conversation")),
        ));
        imp.message_send_button
            .set_label(tr(language, "conversation.send"));
        imp.message_clear_button
            .set_label(tr(language, "conversation.clear"));
        imp.message_clear_button
            .set_tooltip_text(Some(tr(language, "conversation.clear")));
    }

    pub(super) fn conversation_at_bottom(&self) -> bool {
        self.imp()
            .conversation
            .borrow()
            .as_ref()
            .is_none_or(|controls| {
                let adjustment = controls.scroll.vadjustment();
                adjustment.upper() - adjustment.page_size() - adjustment.value() < 48.0
            })
    }

    pub(super) fn scroll_conversation_to_bottom(&self) {
        let controls = self.imp().conversation.borrow();
        let Some(controls) = controls.as_ref() else {
            return;
        };
        let adjustment = controls.scroll.vadjustment();
        glib::idle_add_local_once(move || {
            adjustment.set_value((adjustment.upper() - adjustment.page_size()).max(0.0));
        });
    }
}
