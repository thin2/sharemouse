//! Screen-arrangement editor.
//!
//! The daemon owns the positions; this widget only renders them and
//! emits a position when a device screen is dropped onto a side. It is a
//! plain GTK composition (not a GObject subclass) because it has no
//! properties that need bindings.
//!
//! Drawing rules: this computer is a solid, inverted screen; the device
//! that currently receives input gets the accent outline and an accent
//! seam on the shared edge; offline devices are outlines only. Empty
//! sides are only shown while a screen is being dragged.

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
    pub online: bool,
}

pub struct LayoutView {
    root: gtk::Box,
    center_who: gtk::Label,
    zones: [gtk::Box; 4],
    zone_labels: [gtk::Label; 4],
    seams: [gtk::Box; 4],
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

/// Wrap a screen in a monitor stand so it reads as a display.
pub(crate) fn monitor(screen: &impl IsA<gtk::Widget>) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.set_valign(gtk::Align::End);
    column.set_halign(gtk::Align::Center);
    column.append(screen);
    let neck = gtk::Box::new(gtk::Orientation::Vertical, 0);
    neck.set_size_request(2, 14);
    neck.set_halign(gtk::Align::Center);
    neck.add_css_class("monitor-neck");
    let foot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    foot.set_size_request(46, 2);
    foot.set_halign(gtk::Align::Center);
    foot.add_css_class("monitor-foot");
    column.append(&neck);
    column.append(&foot);
    column
}

/// The labels inside a screen: side or role at the top, then name and meta.
pub(crate) fn screen_body(who: &gtk::Label, name: &str, meta: Option<&str>) -> gtk::Box {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    who.set_xalign(0.0);
    who.set_vexpand(true);
    who.set_valign(gtk::Align::Start);
    who.add_css_class("screen-who");
    body.append(who);
    let name = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(16)
        .build();
    name.add_css_class("screen-name");
    body.append(&name);
    if let Some(meta) = meta {
        let meta = gtk::Label::builder().label(meta).xalign(0.0).build();
        meta.add_css_class("screen-meta");
        body.append(&meta);
    }
    // The label fills the fixed-size screen; that must not make the
    // whole page expand around it.
    body.set_vexpand(false);
    body
}

impl LayoutView {
    pub fn new(on_drop: impl Fn(u64, Position) + 'static) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("layout-stage");

        let grid = gtk::Grid::new();
        grid.set_halign(gtk::Align::Center);
        grid.set_valign(gtk::Align::End);

        let make_zone = |position: Position| {
            // Screens on the same side line up side by side, like monitors on a desk.
            let orientation = match position {
                Position::Left | Position::Right => gtk::Orientation::Horizontal,
                Position::Top | Position::Bottom => gtk::Orientation::Vertical,
            };
            let zone = gtk::Box::new(orientation, 16);
            zone.set_valign(gtk::Align::End);
            zone.set_halign(gtk::Align::Center);
            zone.add_css_class("layout-zone");
            zone.set_widget_name(position_name(position));
            let label = gtk::Label::new(None);
            label.add_css_class("layout-slot");
            label.set_size_request(128, 80);
            zone.append(&label);
            (zone, label)
        };
        let (left, left_label) = make_zone(Position::Left);
        let (right, right_label) = make_zone(Position::Right);
        let (top, top_label) = make_zone(Position::Top);
        let (bottom, bottom_label) = make_zone(Position::Bottom);

        let make_seam = |vertical: bool| {
            let seam = gtk::Box::new(gtk::Orientation::Vertical, 0);
            seam.add_css_class("seam");
            if vertical {
                seam.set_size_request(2, -1);
                seam.set_halign(gtk::Align::Center);
                seam.set_margin_start(12);
                seam.set_margin_end(12);
                // Skip the rounded corners above and the stand below.
                seam.set_margin_top(14);
                seam.set_margin_bottom(30);
            } else {
                seam.set_size_request(-1, 2);
                seam.set_valign(gtk::Align::Center);
                seam.set_margin_start(24);
                seam.set_margin_end(24);
                seam.set_margin_top(10);
                seam.set_margin_bottom(10);
            }
            seam
        };
        let seams = [
            make_seam(true),
            make_seam(true),
            make_seam(false),
            make_seam(false),
        ];

        let center_who = gtk::Label::new(None);
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_default();
        let screen = screen_body(&center_who, &hostname, None);
        screen.add_css_class("screen");
        screen.add_css_class("self");
        screen.set_size_request(230, 132);
        let center = monitor(&screen);

        grid.attach(&top, 2, 0, 1, 1);
        grid.attach(&seams[2], 2, 1, 1, 1);
        grid.attach(&left, 0, 2, 1, 1);
        grid.attach(&seams[0], 1, 2, 1, 1);
        grid.attach(&center, 2, 2, 1, 1);
        grid.attach(&seams[1], 3, 2, 1, 1);
        grid.attach(&right, 4, 2, 1, 1);
        grid.attach(&seams[3], 2, 3, 1, 1);
        grid.attach(&bottom, 2, 4, 1, 1);

        root.append(&grid);
        let floor = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        floor.add_css_class("layout-floor");
        floor.set_size_request(-1, 1);
        root.append(&floor);

        let view = Rc::new(Self {
            root,
            center_who,
            zones: [left, right, top, bottom],
            zone_labels: [left_label, right_label, top_label, bottom_label],
            seams,
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
        if *self.devices.borrow() == devices {
            return;
        }
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
        for (index, label) in self.zone_labels.iter().enumerate() {
            label.set_text(position_label(language, ZONE_POSITIONS[index]));
        }
        self.center_who.set_text(tr(language, "layout.this_device"));
    }

    fn rebuild(&self) {
        let devices = self.devices.borrow();
        for (index, zone) in self.zones.iter().enumerate() {
            // Keep the empty-slot label, replace device screens.
            while let Some(child) = zone.last_child() {
                if child.eq(&self.zone_labels[index]) {
                    break;
                }
                zone.remove(&child);
            }
            self.seams[index].remove_css_class("lit");
        }
        for device in devices.iter() {
            let Some(index) = ZONE_POSITIONS.iter().position(|p| *p == device.position) else {
                continue;
            };
            self.zones[index].append(&self.device_screen(device, index));
            if device.current {
                self.seams[index].add_css_class("lit");
            }
        }
        drop(devices);
        show_zones(&self.zones, &self.zone_labels, false);
    }

    fn device_screen(&self, device: &LayoutDevice, index: usize) -> gtk::Box {
        let language = self.language.get();
        let meta = if device.current {
            tr(language, "status.active")
        } else if device.online {
            tr(language, "status.online")
        } else {
            tr(language, "status.offline")
        };
        let who = gtk::Label::new(Some(position_label(language, ZONE_POSITIONS[index])));
        let body = screen_body(&who, &device.name, Some(meta));
        let screen = gtk::Button::builder()
            .child(&body)
            .vexpand(false)
            .tooltip_text(tr(language, "layout.hint"))
            .build();
        screen.add_css_class("screen");
        screen.set_size_request(176, 110);
        if device.current {
            screen.add_css_class("target");
        } else if !device.online {
            screen.add_css_class("off");
        }

        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        let handle = device.handle.to_string();
        source.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&handle.to_value()))
        });
        let zones = self.zones.clone();
        let labels = self.zone_labels.clone();
        source.connect_drag_begin(move |_, _| show_zones(&zones, &labels, true));
        // The daemon's position event rebuilds the layout after a drop;
        // until then, or after a cancelled drag, collapse the empty sides.
        let zones = self.zones.clone();
        let labels = self.zone_labels.clone();
        source.connect_drag_end(move |_, _, _| show_zones(&zones, &labels, false));
        screen.add_controller(source);
        monitor(&screen)
    }
}

/// Empty sides are drop slots, shown only while a screen is dragged.
fn show_zones(zones: &[gtk::Box; 4], labels: &[gtk::Label; 4], dragging: bool) {
    for (zone, label) in zones.iter().zip(labels) {
        let occupied = zone.first_child() != zone.last_child();
        label.set_visible(dragging && !occupied);
        zone.set_visible(dragging || occupied);
        if dragging {
            zone.add_css_class("dragging");
        } else {
            zone.remove_css_class("dragging");
        }
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
