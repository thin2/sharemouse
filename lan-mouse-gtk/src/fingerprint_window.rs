mod imp;

use glib::Object;
use gtk::{gio, glib, prelude::ObjectExt, subclass::prelude::ObjectSubclassIsExt};

use crate::preferences::{Language, tr};

glib::wrapper! {
    pub struct FingerprintWindow(ObjectSubclass<imp::FingerprintWindow>)
    @extends adw::Window, gtk::Window, gtk::Widget,
    @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl FingerprintWindow {
    pub(crate) fn new(fingerprint: Option<String>, language: Language) -> Self {
        let window: Self = Object::builder()
            .property("title", tr(language, "fingerprint.title"))
            .build();
        window.imp().apply_language(language);
        if let Some(fp) = fingerprint {
            window.imp().fingerprint.set_property("text", fp);
            window.imp().fingerprint.set_property("editable", false);
        }
        window
    }
}
