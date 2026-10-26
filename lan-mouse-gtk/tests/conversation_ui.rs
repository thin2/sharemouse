// GTK on macOS must run on the process main thread, outside libtest workers.
// Compile the production modules in this test binary to exercise private UI.
// Included libtest helpers are not run by this main-thread harness.
#![allow(dead_code, unused_imports)]
include!("../src/lib.rs");
use adw::prelude::MessageDialogExt;

fn main() {
    if std::env::var_os("LAN_MOUSE_RUN_UI_TEST").is_none() {
        println!(
            "SKIP: set LAN_MOUSE_RUN_UI_TEST=1 with an isolated HOME for native GTK assertions"
        );
        return;
    }
    assert_eq!(
        std::env::var("LAN_MOUSE_DISABLE_AUTOSTART").as_deref(),
        Ok("1")
    );
    adw::init().unwrap();
    gio::resources_register_include!("lan-mouse.gresource").unwrap();
    load_css();
    load_icons();
    let app = adw::Application::builder()
        .application_id("de.feschber.LanMouse.ConversationTest")
        .build();
    app.connect_activate(run_ui);
    app.run_with_args::<&str>(&[]);
}

fn run_ui(app: &adw::Application) {
    if std::env::var_os("LAN_MOUSE_UI_BASELINE").is_some() {
        let plain = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(720)
            .default_height(960)
            .build();
        plain.present();
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
            plain.set_visible(false);
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                plain.present();
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    plain.destroy();
                    println!(
                        "PASS: plain libadwaita baseline, no Sharemouse window or message widgets"
                    );
                    app.quit();
                });
            });
        });
        return;
    }
    use gtk::subclass::prelude::ObjectSubclassIsExt;
    let dark = std::env::var("LAN_MOUSE_UI_THEME").as_deref() == Ok("dark");
    preferences::UiPreferences {
        autostart: false,
        language: if dark {
            preferences::Language::Zh
        } else {
            preferences::Language::En
        },
        theme: if dark {
            preferences::Theme::Dark
        } else {
            preferences::Theme::Light
        },
    }
    .save()
    .unwrap();
    let (_reader, writer, _sink) = fixture_connection();
    let window = window::Window::new(app, writer);
    window.present();
    let app = app.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
        assert!(window.is_active());
        let imp = window.imp();
        assert!(imp.start_sharing_button.is_sensitive());
        imp.start_sharing_button.emit_clicked();
        #[cfg(unix)]
        assert!(fixture_request(&_sink).contains("HostGroup"));
        window.update_enrollment(vec![], "idle".into());
        window.update_discovered_hosts(vec![lan_mouse_ipc::DiscoveredHost {
            name: "Desk & <Lab>".into(),
            address: "192.0.2.20".into(),
        }]);
        let nearby = named_widget(window.upcast_ref(), "nearby-hosts")
            .unwrap()
            .downcast::<gtk::ListBox>()
            .unwrap();
        assert!(nearby.is_visible() && nearby.parent().unwrap().is_visible());
        let row = nearby
            .first_child()
            .unwrap()
            .downcast::<adw::ActionRow>()
            .unwrap();
        assert_eq!(row.subtitle().as_deref(), Some("192.0.2.20"));
        row.activatable_widget()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        #[cfg(unix)]
        {
            let request = fixture_request(&_sink);
            assert!(request.contains("JoinGroup") && request.contains("192.0.2.20"));
        }
        window.update_discovered_hosts(vec![]);
        assert!(nearby.first_child().is_none() && !nearby.is_visible());
        window.update_enrollment(vec!["192.0.2.10".into()], "hosting".into());
        assert!(!nearby.parent().unwrap().is_visible());
        let address = named_widget(window.upcast_ref(), "joining-address")
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(address.text(), "192.0.2.10");
        assert!(address.is_visible() && address.parent().unwrap().is_visible());
        let stack = imp
            .toast_overlay
            .child()
            .unwrap()
            .last_child()
            .unwrap()
            .downcast::<gtk::Stack>()
            .unwrap();
        assert_eq!(stack.visible_child_name().as_deref(), Some("devices"));
        stack.set_visible_child_name("messages");
        for (handle, fingerprint) in [(0, "peer-a"), (1, "peer-b")] {
            window.new_client(
                handle,
                lan_mouse_ipc::ClientConfig {
                    hostname: Some("Desktop".into()),
                    ..Default::default()
                },
                lan_mouse_ipc::ClientState {
                    peer_fingerprint: (handle == 0).then(|| fingerprint.into()),
                    ..Default::default()
                },
            );
        }
        window.set_sharing_state(lan_mouse_ipc::SharingState {
            phase: lan_mouse_ipc::SharingPhase::Stopped,
            ..Default::default()
        });
        imp.start_sharing_button.emit_clicked();
        #[cfg(unix)]
        assert!(fixture_request(&_sink).contains("StartSharing"));
        imp.message_peers
            .select_row(imp.message_peers.row_at_index(1).as_ref());
        imp.message_entry.set_text("Draft for second device");
        window.update_client_state(
            1,
            lan_mouse_ipc::ClientState {
                peer_fingerprint: Some("peer-b".into()),
                ..Default::default()
            },
        );
        assert_eq!(imp.message_peers.selected_row().unwrap().index(), 1);
        assert_eq!(imp.message_entry.text(), "Draft for second device");
        imp.message_peers
            .select_row(imp.message_peers.row_at_index(0).as_ref());
        assert!(imp.message_entry.text().is_empty());
        window.record_message(
            "peer-a",
            "Desktop",
            Direction::Received,
            "1",
            "你好，这是从另一台电脑收到的消息。",
        );
        window.record_message(
            "peer-a",
            "Desktop",
            Direction::Sent,
            "2",
            "收到。链接和文字都在这个对话里。",
        );
        window.record_message(
            "peer-b",
            "Desktop",
            Direction::Received,
            "3",
            "另一台同名设备的消息",
        );
        assert_eq!(imp.message_unread.borrow().get("peer-b"), Some(&1));
        assert!(imp.message_list.row_at_index(1).is_some());
        assert!(imp.message_list.row_at_index(2).is_none());
        let incoming = imp.message_list.row_at_index(0).unwrap().child().unwrap();
        let outgoing = imp.message_list.row_at_index(1).unwrap().child().unwrap();
        assert_eq!(incoming.halign(), gtk::Align::Start);
        assert_eq!(outgoing.halign(), gtk::Align::End);
        imp.message_peers
            .select_row(imp.message_peers.row_at_index(1).as_ref());
        assert_eq!(imp.message_entry.text(), "Draft for second device");
        assert!(imp.message_list.row_at_index(0).is_some());
        assert!(imp.message_list.row_at_index(1).is_none());
        assert!(!imp.message_unread.borrow().contains_key("peer-b"));
        imp.message_peers
            .select_row(imp.message_peers.row_at_index(0).as_ref());
        imp.message_entry.set_text("UI send fixture");
        assert!(imp.message_send_button.is_sensitive());
        imp.message_send_button.emit_clicked();
        assert!(imp.message_entry.text().is_empty());
        #[cfg(unix)]
        {
            use std::io::BufRead;
            _sink
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut reader = std::io::BufReader::new(_sink.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(
                line.contains("SendMessage") && line.contains("UI send fixture"),
                "{line}"
            );
        }
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
            window.set_visible(false);
            window.record_message(
                "peer-a",
                "Desktop",
                Direction::Received,
                "4",
                "窗口隐藏时仍保留未读提示。",
            );
            assert_eq!(window.imp().message_unread.borrow().get("peer-a"), Some(&1));
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                window.present();
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    assert!(!window.imp().message_unread.borrow().contains_key("peer-a"));
                    let _fixture = (_reader, _sink);
                    if let Some(path) = std::env::var_os("LAN_MOUSE_UI_SCREENSHOT") {
                        let paintable = gtk::WidgetPaintable::new(Some(&window));
                        let snapshot = gtk::Snapshot::new();
                        paintable.snapshot(
                            &snapshot,
                            window.width() as f64,
                            window.height() as f64,
                        );
                        let node = snapshot.to_node().expect("window render node");
                        let texture = window.renderer().unwrap().render_texture(&node, None);
                        texture.save_to_png(path).unwrap();
                    }
                    glib::spawn_future_local(async move {
                        let _fixture = _fixture;
                        let root = window.imp().toast_overlay.child().unwrap();
                        let stack = root.last_child().unwrap().downcast::<gtk::Stack>().unwrap();
                        assert!(stack.child_by_name("history").is_none());
                        for page in ["files", "devices", "settings", "messages"] {
                            stack.set_visible_child_name(page);
                            glib::timeout_future(std::time::Duration::from_millis(180)).await;
                            assert_eq!(stack.visible_child_name().as_deref(), Some(page));
                            if let Some(path) = std::env::var_os("LAN_MOUSE_UI_SCREENSHOT") {
                                let path = std::path::PathBuf::from(path);
                                let output = path.with_file_name(format!(
                                    "{}-{page}.png",
                                    path.file_stem().unwrap().to_string_lossy()
                                ));
                                let snapshot = gtk::Snapshot::new();
                                gtk::WidgetPaintable::new(Some(&window)).snapshot(
                                    &snapshot,
                                    window.width() as f64,
                                    window.height() as f64,
                                );
                                window
                                    .renderer()
                                    .unwrap()
                                    .render_texture(snapshot.to_node().unwrap(), None)
                                    .save_to_png(output)
                                    .unwrap();
                            }
                        }
                        // A visible window covered by another window must retain unread.
                        let cover = adw::ApplicationWindow::builder()
                            .application(&app)
                            .default_width(300)
                            .default_height(200)
                            .build();
                        cover.present();
                        glib::timeout_future(std::time::Duration::from_millis(300)).await;
                        assert!(!window.is_active());
                        window.record_message(
                            "peer-a",
                            "Desktop",
                            Direction::Received,
                            "5",
                            "While inactive",
                        );
                        assert_eq!(window.imp().message_unread.borrow().get("peer-a"), Some(&1));
                        window.present();
                        glib::timeout_future(std::time::Duration::from_millis(300)).await;
                        assert!(window.is_active());
                        assert!(!window.imp().message_unread.borrow().contains_key("peer-a"));
                        cover.destroy();
                        window.reset_messages();
                        assert!(window.imp().message_list.row_at_index(0).is_none());
                        window.new_client(2, Default::default(), Default::default());
                        window
                            .imp()
                            .message_peers
                            .select_row(window.imp().message_peers.row_at_index(2).as_ref());
                        window
                            .imp()
                            .message_entry
                            .set_text("Draft for removed unknown peer");
                        window.delete_client(2);
                        window.new_client(2, Default::default(), Default::default());
                        window
                            .imp()
                            .message_peers
                            .select_row(window.imp().message_peers.row_at_index(2).as_ref());
                        assert!(window.imp().message_entry.text().is_empty());
                        window.delete_client(2);
                        // Retain unread for configured peers after eviction, but remove
                        // a deleted peer once its last message leaves the global cap.
                        stack.set_visible_child_name("files");
                        window.record_message(
                            "peer-b",
                            "Desktop",
                            Direction::Received,
                            "retained",
                            "old",
                        );
                        window.new_client(
                            2,
                            Default::default(),
                            lan_mouse_ipc::ClientState {
                                peer_fingerprint: Some("removed-peer".into()),
                                ..Default::default()
                            },
                        );
                        window.record_message(
                            "removed-peer",
                            "Removed",
                            Direction::Received,
                            "removed",
                            "old",
                        );
                        window.delete_client(2);
                        assert_eq!(
                            window.imp().message_unread.borrow().get("removed-peer"),
                            Some(&1)
                        );
                        for index in 0..message_log::MAX_MESSAGES - 1 {
                            window.record_message(
                                "peer-a",
                                "Desktop",
                                Direction::Sent,
                                &format!("cap-{index}"),
                                "new",
                            );
                        }
                        assert_eq!(window.imp().message_unread.borrow().get("peer-b"), Some(&1));
                        assert_eq!(
                            window.imp().message_unread.borrow().get("removed-peer"),
                            Some(&1)
                        );
                        window.record_message(
                            "peer-a",
                            "Desktop",
                            Direction::Sent,
                            "cap-last",
                            "new",
                        );
                        assert_eq!(window.imp().message_unread.borrow().get("peer-b"), Some(&1));
                        assert!(
                            !window
                                .imp()
                                .message_unread
                                .borrow()
                                .contains_key("removed-peer")
                        );
                        window.reset_messages();
                        stack.set_visible_child_name("messages");
                        window.set_default_size(780, 560);
                        glib::timeout_future(std::time::Duration::from_millis(250)).await;
                        if let Some(path) = std::env::var_os("LAN_MOUSE_UI_SCREENSHOT") {
                            let path = std::path::PathBuf::from(path);
                            let output = path.with_file_name(format!(
                                "{}-compact.png",
                                path.file_stem().unwrap().to_string_lossy()
                            ));
                            let snapshot = gtk::Snapshot::new();
                            gtk::WidgetPaintable::new(Some(&window)).snapshot(
                                &snapshot,
                                window.width() as f64,
                                window.height() as f64,
                            );
                            window
                                .renderer()
                                .unwrap()
                                .render_texture(snapshot.to_node().unwrap(), None)
                                .save_to_png(output)
                                .unwrap();
                        }
                        window.destroy();
                        println!(
                            "PASS: four pages without clipboard history, recipient identity transition, independent drafts, directional bubbles, inactive-window unread, cap pruning and message clearing"
                        );
                        app.quit();
                    });
                });
            });
        });
    });
}

fn named_widget(widget: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
    if widget.widget_name() == name {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = named_widget(&current, name) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

#[cfg(unix)]
fn fixture_request(sink: &std::os::unix::net::UnixStream) -> String {
    use std::io::BufRead;
    sink.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(sink.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    line
}

#[cfg(unix)]
fn fixture_connection() -> (
    lan_mouse_ipc::FrontendEventReader,
    lan_mouse_ipc::FrontendRequestWriter,
    std::os::unix::net::UnixStream,
) {
    let path = lan_mouse_ipc::default_socket_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    let (reader, writer) = lan_mouse_ipc::try_connect().unwrap();
    let (sink, _) = listener.accept().unwrap();
    (reader, writer, sink)
}

#[cfg(windows)]
fn fixture_connection() -> (
    lan_mouse_ipc::FrontendEventReader,
    lan_mouse_ipc::FrontendRequestWriter,
    std::net::TcpStream,
) {
    panic!("This isolated native harness currently supports Unix sockets only");
}
