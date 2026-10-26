use super::*;

pub(super) struct Target {
    pub key: String,
    pub name: String,
    pub handle: Option<ClientHandle>,
}

pub(super) struct Controls {
    stack: gtk::Stack,
    messages: gtk::StackPage,
    pages: Vec<(&'static str, gtk::StackPage)>,
    scroll: gtk::ScrolledWindow,
    peers_title: gtk::Label,
    add_device: gtk::Button,
    send_files: gtk::Button,
    files_hint: gtk::Label,
    pick_files: gtk::Button,
    pick_folder: gtk::Button,
}

fn section_page(parent: &gtk::Box, groups: &[&adw::PreferencesGroup]) -> gtk::ScrolledWindow {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 24);
    column.set_margin_top(28);
    column.set_margin_bottom(28);
    column.set_margin_start(28);
    column.set_margin_end(28);
    for group in groups {
        parent.remove(*group);
        column.append(*group);
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(820)
        .child(&column)
        .build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
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
        imp.message_target_row.remove(&*imp.message_peers);
        imp.message_peers.add_css_class("conversation-peers");
        let send_files = gtk::Button::builder().valign(gtk::Align::Center).build();
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

        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.set_hexpand(true);
        page.add_css_class("conversation-page");
        let header = gtk::ListBox::new();
        header.set_selection_mode(gtk::SelectionMode::None);
        header.append(&*imp.message_target_row);
        page.append(&header);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&*imp.message_list)
            .build();
        page.append(&scroll);
        imp.message_compose_row
            .add_css_class("conversation-compose");
        page.append(&*imp.message_compose_row);
        let peers = gtk::Box::new(gtk::Orientation::Vertical, 16);
        peers.add_css_class("conversation-sidebar");
        peers.set_width_request(200);
        let peers_title = gtk::Label::builder().xalign(0.0).build();
        peers_title.add_css_class("heading");
        peers.append(&peers_title);
        peers.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&*imp.message_peers)
                .build(),
        );
        let add_device = gtk::Button::new();
        peers.append(&add_device);
        imp.message_clear_button.set_halign(gtk::Align::Start);
        peers.append(&*imp.message_clear_button);
        let conversations = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        conversations.append(&peers);
        conversations.append(&page);

        let files = section_page(&parent, &[&imp.transfer_group]);
        let files_hint = gtk::Label::builder().xalign(0.0).wrap(true).build();
        // Keep the instruction above the existing transfer actions and records.
        let files_column = imp
            .transfer_group
            .parent()
            .and_downcast::<gtk::Box>()
            .unwrap();
        let file_actions = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let pick_files = gtk::Button::new();
        pick_files.add_css_class("suggested-action");
        let pick_folder = gtk::Button::new();
        file_actions.append(&pick_files);
        file_actions.append(&pick_folder);
        files_column.prepend(&file_actions);
        files_column.prepend(&files_hint);
        for (button, folder) in [(&pick_files, false), (&pick_folder, true)] {
            button.connect_clicked(clone!(
                #[weak(rename_to = window)]
                self,
                move |_| window.choose_transfer_paths(None, folder)
            ));
        }
        // Sharing controls and connected devices precede the large layout editor.
        let sharing = imp
            .primary_row
            .ancestor(adw::PreferencesGroup::static_type())
            .unwrap();
        parent.reorder_child_after(&*imp.connections_group, Some(&sharing));
        parent.reorder_child_after(&*imp.layout_container, Some(&*imp.connections_group));
        let settings = section_page(
            &parent,
            &[
                &imp.preferences_group,
                &imp.general_group,
                &imp.incoming_group,
                &imp.data_group,
            ],
        );
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(false);
        let messages = stack.add_titled(
            &conversations,
            Some("messages"),
            tr(self.language(), "group.messages"),
        );
        let pages = [
            ("files", "conversation.files", files.upcast::<gtk::Widget>()),
            ("devices", "conversation.devices", original),
            ("settings", "conversation.settings", settings.upcast()),
        ]
        .into_iter()
        .map(|(name, key, widget)| {
            (
                key,
                stack.add_titled(&widget, Some(name), tr(self.language(), key)),
            )
        })
        .collect();
        add_device.connect_clicked(clone!(
            #[weak]
            stack,
            move |_| stack.set_visible_child_name("devices")
        ));
        let switcher = gtk::StackSwitcher::builder()
            .stack(&stack)
            .halign(gtk::Align::Center)
            .build();
        switcher.set_margin_top(6);
        switcher.set_margin_bottom(14);
        switcher.add_css_class("workspace-navigation");
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("conversation-shell");
        root.append(&switcher);
        root.append(&stack);
        imp.toast_overlay.set_child(Some(&root));
        imp.conversation.replace(Some(Controls {
            stack: stack.clone(),
            messages,
            pages,
            scroll,
            peers_title,
            add_device,
            send_files,
            files_hint,
            pick_files,
            pick_folder,
        }));
        stack.connect_visible_child_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| {
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

    pub(super) fn reveal_connection_setup(&self) {
        if let Some(controls) = self.imp().conversation.borrow().as_ref() {
            controls.stack.set_visible_child_name("devices");
            if let Some(scroll) = controls
                .stack
                .child_by_name("devices")
                .and_downcast::<gtk::ScrolledWindow>()
            {
                scroll.vadjustment().set_value(0.0);
            }
        }
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
        let title = tr(language, "group.messages");
        controls.messages.set_title(&if unread == 0 {
            title.into()
        } else {
            format!("{title} ({unread})")
        });
        for (key, page) in &controls.pages {
            page.set_title(tr(language, key));
        }
        controls
            .peers_title
            .set_text(tr(language, "conversation.peers"));
        controls
            .add_device
            .set_label(tr(language, "conversation.add_device"));
        controls
            .send_files
            .set_label(tr(language, "conversation.send_files"));
        controls
            .send_files
            .set_sensitive(self.selected_message_target().is_some());
        controls
            .files_hint
            .set_text(tr(language, "conversation.files_hint"));
        controls
            .pick_files
            .set_label(tr(language, "conversation.send_files"));
        controls
            .pick_folder
            .set_label(tr(language, "conversation.send_folder"));
        let name = self.selected_conversation().and_then(|key| {
            imp.message_targets
                .borrow()
                .iter()
                .find(|target| target.key == key)
                .map(|target| target.name.clone())
        });
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
