use super::*;

/// Device page widgets that are built in code: the device cards below
/// the screen layout and the first-run view shown before any device is
/// paired. The status line and the layout itself come from the template.
pub(super) struct Controls {
    full: gtk::Box,
    empty: gtk::Box,
    empty_eyebrow: gtk::Label,
    empty_title: gtk::Label,
    empty_body: gtk::Label,
    empty_add: gtk::Button,
    empty_who: gtk::Label,
    section_title: gtk::Label,
    section_count: gtk::Label,
    section_note: gtk::Label,
    grid: gtk::FlowBox,
    add_tile: gtk::FlowBoxChild,
    add_title: gtk::Label,
    add_note: gtk::Label,
    cards: Vec<gtk::FlowBoxChild>,
}

type MenuAction = Box<dyn Fn(&Window)>;

/// Default release chord, named the way each platform labels its keys.
fn release_keys() -> [&'static str; 4] {
    if cfg!(target_os = "macos") {
        ["Control", "Shift", "Option", "Command"]
    } else {
        ["Ctrl", "Shift", "Alt", "Win"]
    }
}

impl Window {
    pub(super) fn setup_home(&self) {
        let imp = self.imp();
        let parent = imp
            .layout_container
            .parent()
            .and_downcast::<gtk::Box>()
            .expect("main column");
        parent.set_spacing(0);

        // Status, layout and device cards form the "full" view.
        let full = gtk::Box::new(gtk::Orientation::Vertical, 0);
        parent.insert_child_after(&full, imp.status_box.prev_sibling().as_ref());
        parent.remove(&*imp.status_box);
        parent.remove(&*imp.layout_container);
        full.append(&*imp.status_box);
        full.append(&*imp.layout_container);

        let section = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        section.add_css_class("section");
        let section_title = gtk::Label::new(None);
        section_title.add_css_class("section-title");
        let section_count = gtk::Label::new(None);
        section_count.add_css_class("section-count");
        let section_note = gtk::Label::builder()
            .hexpand(true)
            .xalign(1.0)
            .wrap(true)
            .build();
        section_note.add_css_class("section-note");
        section.append(&section_title);
        section.append(&section_count);
        section.append(&section_note);
        full.append(&section);

        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(1)
            .max_children_per_line(3)
            .column_spacing(16)
            .row_spacing(16)
            .build();
        grid.add_css_class("device-grid");
        grid.set_valign(gtk::Align::Start);
        let add_button = gtk::Button::builder().vexpand(false).build();
        add_button.add_css_class("add-tile");
        let add_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let plus = gtk::Label::builder()
            .label("+")
            .xalign(0.0)
            .vexpand(true)
            .valign(gtk::Align::Start)
            .margin_bottom(12)
            .build();
        plus.add_css_class("plus");
        let add_title = gtk::Label::builder().xalign(0.0).build();
        add_title.add_css_class("add-title");
        let add_note = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(18)
            .build();
        add_note.add_css_class("add-note");
        add_box.append(&plus);
        add_box.append(&add_title);
        add_box.append(&add_note);
        add_button.set_child(Some(&add_box));
        let add_tile = gtk::FlowBoxChild::new();
        add_tile.set_child(Some(&add_button));
        grid.append(&add_tile);
        full.append(&grid);

        // First run: an invitation and an empty slot next to this screen.
        let empty = gtk::Box::new(gtk::Orientation::Horizontal, 40);
        empty.set_margin_top(60);
        empty.set_margin_bottom(40);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        let empty_eyebrow = gtk::Label::builder().xalign(0.0).build();
        empty_eyebrow.add_css_class("eyebrow");
        let empty_title = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .margin_top(6)
            .margin_bottom(12)
            .build();
        empty_title.add_css_class("first-title");
        let empty_body = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(36)
            .margin_bottom(24)
            .build();
        empty_body.add_css_class("dim");
        let empty_add = gtk::Button::builder().halign(gtk::Align::Start).build();
        empty_add.add_css_class("pill");
        empty_add.add_css_class("suggested-action");
        text.append(&empty_eyebrow);
        text.append(&empty_title);
        text.append(&empty_body);
        text.append(&empty_add);
        let pair = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        pair.set_valign(gtk::Align::Center);
        let empty_who = gtk::Label::new(None);
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_default();
        let screen = crate::layout_view::screen_body(&empty_who, &hostname, None);
        screen.add_css_class("screen");
        screen.add_css_class("self");
        screen.set_size_request(180, 108);
        pair.append(&crate::layout_view::monitor(&screen));
        let slot = gtk::Label::builder()
            .label("+")
            .valign(gtk::Align::End)
            .build();
        slot.add_css_class("first-slot");
        slot.set_size_request(150, 92);
        pair.append(&slot);
        empty.append(&text);
        empty.append(&pair);
        parent.insert_child_after(&empty, Some(&full));

        for button in [&add_button, &empty_add] {
            button.connect_clicked(clone!(
                #[weak(rename_to = window)]
                self,
                move |_| window.open_add_device()
            ));
        }

        imp.home.replace(Some(Controls {
            full,
            empty,
            empty_eyebrow,
            empty_title,
            empty_body,
            empty_add,
            empty_who,
            section_title,
            section_count,
            section_note,
            grid,
            add_tile,
            add_title,
            add_note,
            cards: vec![],
        }));
        self.apply_home_language(self.language());
    }

    pub(super) fn apply_home_language(&self, language: Language) {
        {
            let home = self.imp().home.borrow();
            let Some(h) = home.as_ref() else { return };
            h.section_title.set_text(tr(language, "home.devices"));
            h.section_note.set_text(tr(language, "home.drag_hint"));
            h.add_title.set_text(tr(language, "home.add"));
            h.add_note.set_text(tr(language, "home.add_note"));
            h.empty_eyebrow.set_text(tr(language, "home.empty_eyebrow"));
            h.empty_title.set_markup(tr(language, "home.empty_title"));
            h.empty_body.set_text(tr(language, "home.empty_body"));
            h.empty_add.set_label(tr(language, "home.add"));
            h.empty_who.set_text(tr(language, "layout.this_device"));
        }
        let hint = &self.imp().where_hint;
        while let Some(child) = hint.first_child() {
            hint.remove(&child);
        }
        hint.append(&gtk::Label::new(Some(tr(language, "home.release_before"))));
        for key in release_keys() {
            let key = gtk::Label::new(Some(key));
            key.add_css_class("kbd");
            hint.append(&key);
        }
        hint.append(&gtk::Label::new(Some(tr(language, "home.release_after"))));
        self.refresh_home_devices();
    }

    /// Rebuild the device cards from the client list.
    pub(super) fn refresh_home_devices(&self) {
        let language = self.language();
        let clients: Vec<ClientObject> = self
            .clients()
            .iter::<ClientObject>()
            .filter_map(|object| object.ok())
            .collect();
        let empty = self.needs_connection_setup();
        let mut home = self.imp().home.borrow_mut();
        let Some(h) = home.as_mut() else { return };
        h.full.set_visible(!empty);
        h.empty.set_visible(empty);
        h.section_count
            .set_text(&tr(language, "home.count").replace("{}", &clients.len().to_string()));
        h.section_note.set_visible(!clients.is_empty());
        for card in h.cards.drain(..) {
            h.grid.remove(&card);
        }
        for (index, client) in clients.iter().enumerate() {
            let card = gtk::FlowBoxChild::new();
            card.set_child(Some(&self.device_card(client, language)));
            h.grid.insert(&card, index as i32);
            h.cards.push(card);
        }
        // Keep the add tile last.
        h.grid.remove(&h.add_tile);
        h.grid.append(&h.add_tile);
    }

    fn device_card(&self, client: &ClientObject, language: Language) -> gtk::Box {
        let data = client.get_data();
        let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
        card.add_css_class("sm-card");
        card.add_css_class("device-card");
        card.set_width_request(200);

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = gtk::Box::new(gtk::Orientation::Vertical, 3);
        title.set_hexpand(true);
        let name = gtk::Label::builder()
            .label(client.display_name())
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        name.add_css_class("device-name");
        let (class, key) = if data.is_current_target {
            ("live", "status.active")
        } else if data.alive {
            ("on", "status.online")
        } else {
            ("off", "status.offline")
        };
        let state = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        state.add_css_class("state");
        state.add_css_class(class);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("state-dot");
        dot.set_valign(gtk::Align::Center);
        state.append(&dot);
        state.append(&gtk::Label::new(Some(tr(language, key))));
        title.append(&name);
        title.append(&state);
        top.append(&title);
        top.append(&self.device_menu(client, language));
        card.append(&top);

        let meta = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        meta.add_css_class("device-meta");
        let address = data
            .ips
            .first()
            .cloned()
            .or(data.hostname.clone())
            .unwrap_or_default();
        let address = gtk::Label::builder()
            .label(address)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .selectable(true)
            .build();
        address.add_css_class("mono");
        let side = gtk::Label::new(Some(crate::preferences::position_label(
            language,
            data.position.parse().unwrap_or_default(),
        )));
        side.add_css_class("device-side");
        meta.append(&address);
        meta.append(&side);
        card.append(&meta);
        card
    }

    /// Secondary device actions, kept out of the card itself.
    fn device_menu(&self, client: &ClientObject, language: Language) -> gtk::MenuButton {
        let handle = client.handle();
        let active = client.get_data().active;
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let popover = gtk::Popover::builder().child(&menu).build();
        let button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .popover(&popover)
            .valign(gtk::Align::Start)
            .tooltip_text(format!(
                "{} · {}",
                client.display_name(),
                tr(language, "home.more")
            ))
            .build();
        button.add_css_class("more");
        let entries: [(&str, MenuAction); 4] = [
            (
                "client.send_files",
                Box::new(move |w: &Window| w.choose_files_to_send(handle)),
            ),
            (
                if active {
                    "home.disable"
                } else {
                    "home.enable"
                },
                Box::new(move |w: &Window| w.request(FrontendRequest::Activate(handle, !active))),
            ),
            (
                "settings.manual",
                Box::new(|w: &Window| w.show_settings_page()),
            ),
            (
                "client.delete",
                Box::new(move |w: &Window| w.request(FrontendRequest::Delete(handle))),
            ),
        ];
        for (key, action) in entries {
            let item = gtk::Button::builder()
                .label(tr(language, key))
                .halign(gtk::Align::Fill)
                .build();
            item.add_css_class("flat");
            if key == "client.delete" {
                item.add_css_class("destructive-action");
            }
            if let Some(label) = item.child().and_downcast::<gtk::Label>() {
                label.set_xalign(0.0);
            }
            let window = self.downgrade();
            let popover = popover.clone();
            item.connect_clicked(move |_| {
                popover.popdown();
                if let Some(window) = window.upgrade() {
                    action(&window);
                }
            });
            menu.append(&item);
        }
        button
    }
}
