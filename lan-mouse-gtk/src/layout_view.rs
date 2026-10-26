//! Screen-arrangement editor.
//!
//! The daemon owns the positions; this widget only renders them and
//! emits a position when a device card is dropped onto a side. It is a
//! plain GTK composition (not a GObject subclass) because it has no
//! properties that need bindings.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::{gdk, glib, prelude::*};

use lan_mouse_ipc::Position;

use crate::preferences::{Language, position_label, tr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutDevice {
    pub handle: u64,
    pub name: String,
    pub position: Position,
    pub current: bool,
}

pub struct LayoutView {
    root: gtk::Box,
    grid: gtk::Grid,
    title: gtk::Label,
    hint: gtk::Label,
    center: gtk::Box,
    zones: [gtk::Box; 4],
    zone_labels: [gtk::Label; 4],
    devices: RefCell<Vec<LayoutDevice>>,
    language: Cell<Language>,
    on_drop: Rc<dyn Fn(u64, Position)>,
}

const ZONE_POSITIONS: [Position; 4] = [
    Position::Left,
    Position::Right,
    Position::Top,
    Position::Bottom,
];

impl LayoutView {
    pub fn new(on_drop: impl Fn(u64, Position) + 'static) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.set_css_classes(&["layout-panel"]);

        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.add_css_class("heading");
        let hint = gtk::Label::new(None);
        hint.set_xalign(0.0);
        hint.set_wrap(true);
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");

        let grid = gtk::Grid::new();
        grid.set_row_spacing(8);
        grid.set_column_spacing(8);
        grid.set_halign(gtk::Align::Center);

        let make_zone = |position: Position| {
            let zone = gtk::Box::new(gtk::Orientation::Vertical, 2);
            zone.set_size_request(150, 82);
            zone.set_valign(gtk::Align::Center);
            zone.add_css_class("layout-zone");
            zone.set_widget_name(position_name(position));
            let label = gtk::Label::new(None);
            label.add_css_class("caption");
            label.add_css_class("dim-label");
            zone.append(&label);
            (zone, label)
        };

        let (left, left_label) = make_zone(Position::Left);
        let (right, right_label) = make_zone(Position::Right);
        let (top, top_label) = make_zone(Position::Top);
        let (bottom, bottom_label) = make_zone(Position::Bottom);

        let center = gtk::Box::new(gtk::Orientation::Vertical, 2);
        center.set_size_request(180, 82);
        center.set_valign(gtk::Align::Center);
        center.add_css_class("layout-center");
        let this_device = gtk::Label::new(None);
        this_device.add_css_class("heading");
        center.append(&this_device);
        let hostname_text = hostname::get().ok().and_then(|h| h.into_string().ok());
        let hostname = gtk::Label::new(hostname_text.as_deref());
        hostname.add_css_class("caption");
        hostname.add_css_class("dim-label");
        center.append(&hostname);

        grid.attach(&top, 1, 0, 1, 1);
        grid.attach(&left, 0, 1, 1, 1);
        grid.attach(&center, 1, 1, 1, 1);
        grid.attach(&right, 2, 1, 1, 1);
        grid.attach(&bottom, 1, 2, 1, 1);

        root.append(&title);
        root.append(&hint);
        root.append(&grid);

        let view = Rc::new(Self {
            root,
            grid,
            title,
            hint,
            center,
            zones: [left, right, top, bottom],
            zone_labels: [left_label, right_label, top_label, bottom_label],
            devices: RefCell::new(Vec::new()),
            language: Cell::new(Language::default()),
            on_drop: Rc::new(on_drop),
        });
        view.install_drop_targets();
        view.update_labels();
        view.rebuild();
        view
    }

    pub fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    pub fn set_language(&self, language: Language) {
        self.language.set(language);
        self.update_labels();
        self.rebuild();
    }

    pub fn set_devices(&self, devices: Vec<LayoutDevice>) {
        *self.devices.borrow_mut() = devices;
        self.rebuild();
    }

    fn install_drop_targets(&self) {
        for (index, zone) in self.zones.iter().enumerate() {
            let target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
            // Only the callback is captured. The local layout is
            // rendered again from the daemon's state event, so the
            // drop never touches widget-owned state after the fact.
            let on_drop = self.on_drop.clone();
            target.connect_drop(move |_, value, _, _| {
                let Ok(text) = value.get::<String>() else {
                    return false;
                };
                let Ok(handle) = text.parse::<u64>() else {
                    return false;
                };
                on_drop(handle, ZONE_POSITIONS[index]);
                true
            });
            zone.add_controller(target);
        }
    }

    fn update_labels(&self) {
        let language = self.language.get();
        self.title.set_text(tr(language, "layout.title"));
        self.hint.set_text(tr(language, "layout.hint"));
        for (index, label) in self.zone_labels.iter().enumerate() {
            label.set_text(position_label(language, ZONE_POSITIONS[index]));
        }
        if let Some(label) = self.center.first_child().and_downcast::<gtk::Label>() {
            label.set_text(tr(language, "layout.this_device"));
        }
    }

    fn rebuild(&self) {
        let devices = self.devices.borrow();
        for (index, zone) in self.zones.iter().enumerate() {
            // Keep the direction label, replace device chips.
            while let Some(child) = zone.last_child() {
                if child.eq(&self.zone_labels[index]) {
                    break;
                }
                zone.remove(&child);
            }
        }
        for device in devices.iter() {
            let Some(index) = ZONE_POSITIONS.iter().position(|p| *p == device.position) else {
                continue;
            };
            let chip = self.device_chip(device.handle, &device.name);
            if device.current {
                chip.add_css_class("suggested-action");
                chip.set_label(&format!(
                    "{} · {}",
                    device.name,
                    tr(self.language.get(), "status.active")
                ));
            }
            self.zones[index].append(&chip);
        }
        self.grid.queue_draw();
    }

    fn device_chip(&self, handle: u64, name: &str) -> gtk::Button {
        let chip = gtk::Button::with_label(name);
        chip.add_css_class("layout-chip");
        chip.set_tooltip_text(Some(name));
        chip.set_can_focus(true);

        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        let handle = handle.to_string();
        source.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&handle.to_value()))
        });
        chip.add_controller(source);
        chip
    }
}

fn position_name(position: Position) -> &'static str {
    match position {
        Position::Left => "left",
        Position::Right => "right",
        Position::Top => "top",
        Position::Bottom => "bottom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_order_matches_position_enum() {
        assert_eq!(ZONE_POSITIONS[0], Position::Left);
        assert_eq!(ZONE_POSITIONS[1], Position::Right);
        assert_eq!(ZONE_POSITIONS[2], Position::Top);
        assert_eq!(ZONE_POSITIONS[3], Position::Bottom);
    }

    #[test]
    fn position_names_are_stable_for_css_selectors() {
        assert_eq!(position_name(Position::Left), "left");
        assert_eq!(position_name(Position::Bottom), "bottom");
    }
}
