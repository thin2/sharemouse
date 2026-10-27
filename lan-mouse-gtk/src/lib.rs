mod authorization_window;
mod autostart;
mod client_object;
mod client_row;
mod fingerprint_window;
mod key_object;
mod key_row;
mod layout_view;
#[cfg(target_os = "macos")]
mod macos_privacy;
#[cfg(target_os = "macos")]
mod macos_status_item;
mod message_log;
mod palette;
mod preferences;
mod window;
#[cfg(windows)]
mod windows_tray;

use std::{env, process, str, sync::OnceLock};

use gtk::CssProvider;
use window::Window;

/// Local build's commit hash, set once by [`run`] before the GTK
/// main loop starts. Read by per-row UI to compare against each
/// peer's [`lan_mouse_ipc::ClientState::peer_commit`] for the
/// soft-warn version-mismatch indicator.
pub(crate) static LOCAL_COMMIT: OnceLock<[u8; 8]> = OnceLock::new();

/// Convenience: returns the local commit as an 8-char ASCII string,
/// or a placeholder if unset (which would indicate a programmer
/// error since [`run`] always sets it).
pub(crate) fn local_commit_str() -> String {
    LOCAL_COMMIT
        .get()
        .and_then(|c| std::str::from_utf8(c).ok())
        .unwrap_or("????????")
        .to_string()
}

use lan_mouse_ipc::FrontendEvent;

use adw::Application;
use gtk::{IconTheme, gdk::Display, glib::clone, prelude::*};
use gtk::{gio, glib, prelude::ApplicationExt};

use self::client_object::ClientObject;
use self::key_object::KeyObject;
use self::message_log::Direction;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum GtkError {
    #[error("gtk frontend exited with non zero exit code: {0}")]
    NonZeroExitCode(i32),
}

pub fn run(local_commit: [u8; 8]) -> Result<(), GtkError> {
    log::debug!("running gtk frontend");
    LOCAL_COMMIT
        .set(local_commit)
        .expect("local_commit set once");

    #[cfg(windows)]
    let ret = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // https://gitlab.gnome.org/GNOME/gtk/-/commit/52dbb3f372b2c3ea339e879689c1de535ba2c2c3 -> caused crash on windows
        .name("gtk".into())
        .spawn(gtk_main)
        .unwrap()
        .join()
        .unwrap();
    #[cfg(not(windows))]
    let ret = gtk_main();

    match ret {
        glib::ExitCode::SUCCESS => Ok(()),
        e => Err(GtkError::NonZeroExitCode(e.value())),
    }
}

fn gtk_main() -> glib::ExitCode {
    #[cfg(windows)]
    configure_windows_runtime();
    #[cfg(target_os = "macos")]
    {
        configure_macos_bundle_environment();
        install_macos_gtk_log_filter();
    }

    gio::resources_register_include!("lan-mouse.gresource").expect("Failed to register resources.");

    let app = Application::builder()
        .application_id("de.feschber.LanMouse")
        .build();

    app.connect_startup(|app| {
        load_css();
        load_icons();
        setup_actions(app);
        setup_menu(
            app.upcast_ref(),
            preferences::UiPreferences::load().language,
        );
    });
    app.connect_activate(build_ui);

    let args: Vec<&'static str> = vec![];
    app.run_with_args(&args)
}

#[cfg(windows)]
fn configure_windows_runtime() {
    use std::os::windows::process::CommandExt;
    let Ok(executable) = env::current_exe() else {
        return;
    };
    let Some(prefix) = executable.parent() else {
        return;
    };
    let share = prefix.join("share");
    if !share.is_dir() {
        return;
    }
    env::set_var("XDG_DATA_DIRS", &share);
    env::set_var("GSETTINGS_SCHEMA_DIR", share.join("glib-2.0/schemas"));
    let loaders = prefix.join("lib/gdk-pixbuf-2.0/2.10.0/loaders");
    let query = prefix.join("gdk-pixbuf-query-loaders.exe");
    if loaders.is_dir() && query.is_file() {
        let cache = glib::user_cache_dir().join("lan-mouse");
        if std::fs::create_dir_all(&cache).is_ok() {
            env::set_var("GDK_PIXBUF_MODULEDIR", loaders);
            env::set_var("GDK_PIXBUF_MODULE_FILE", cache.join("loaders.cache"));
            if let Err(e) = process::Command::new(query)
                .arg("--update-cache")
                .creation_flags(0x08000000)
                .status()
            {
                log::warn!("could not refresh image loaders: {e}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn install_macos_gtk_log_filter() {
    glib::log_set_writer_func(|level, fields| {
        if level == glib::LogLevel::Warning && is_gtk_theme_parser_warning(fields) {
            return glib::LogWriterOutput::Handled;
        }

        glib::log_writer_default(level, fields)
    });
}

#[cfg(target_os = "macos")]
fn is_gtk_theme_parser_warning(fields: &[glib::LogField<'_>]) -> bool {
    let mut domain = None;
    let mut message = None;

    for field in fields {
        match field.key() {
            "GLIB_DOMAIN" => domain = field.value_str(),
            "MESSAGE" => message = field.value_str(),
            _ => {}
        }
    }

    domain == Some("Gtk")
        && message.is_some_and(|message| message.starts_with("Theme parser warning: gtk.css:"))
}

#[cfg(target_os = "macos")]
fn configure_macos_bundle_environment() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(contents) = exe
        .parent()
        .and_then(|dir| dir.parent())
        .map(std::path::Path::to_owned)
    else {
        return;
    };

    let share = contents.join("Resources").join("share");
    if !share.exists() {
        return;
    }

    let schemas = share.join("glib-2.0").join("schemas");
    if schemas.exists() {
        env::set_var("GSETTINGS_SCHEMA_DIR", schemas);
    }

    env::set_var("XDG_DATA_DIRS", &share);
    env::set_var(
        "GTK_DATA_PREFIX",
        contents.join("Resources").to_string_lossy().as_ref(),
    );
}

fn load_css() {
    let provider = CssProvider::default();
    provider.load_from_resource("de/feschber/LanMouse/style.css");
    gtk::style_context_add_provider_for_display(
        &Display::default().expect("Could not connect to a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

thread_local! {
    static PALETTE_PROVIDER: CssProvider = CssProvider::new();
}

/// Install (or refresh) the palette colors. They sit above the static
/// stylesheet's priority so libadwaita's own surface colors follow too.
pub(crate) fn apply_palette(palette: preferences::Palette) {
    let Some(display) = Display::default() else {
        return;
    };
    let dark = adw::StyleManager::default().is_dark();
    // libadwaita 1.6 reads CSS variables instead of named colors.
    let variables = (adw::major_version(), adw::minor_version()) >= (1, 6);
    PALETTE_PROVIDER.with(|provider| {
        provider.load_from_data(&palette::css(palette, dark, variables));
        gtk::style_context_add_provider_for_display(
            &display,
            provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
    });
}

fn load_icons() {
    let display = &Display::default().expect("Could not connect to a display.");
    let icon_theme = IconTheme::for_display(display);
    icon_theme.add_resource_path("/de/feschber/LanMouse/icons");
}

// Add application actions
fn setup_actions(app: &adw::Application) {
    // Quit action
    // This is important on macOS, where users expect a File->Quit action with a Cmd+Q shortcut.
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            if let Ok((_, mut tx)) = lan_mouse_ipc::try_connect() {
                if let Err(e) = tx.request(lan_mouse_ipc::FrontendRequest::Shutdown) {
                    log::error!("could not request daemon shutdown: {e}");
                }
            }
            app.quit();
        }
    });
    app.add_action(&quit_action);
    app.set_accels_for_action("app.quit", &["<Primary>q"]);
    let stop = gio::SimpleAction::new("stop", None);
    stop.connect_activate(|_, _| {
        if let Ok((_, mut tx)) = lan_mouse_ipc::try_connect() {
            let _ = tx.request(lan_mouse_ipc::FrontendRequest::StopSharing);
        }
    });
    app.add_action(&stop);
}

// Set up a global menu
//
// Currently this is used only on macOS
fn setup_menu(app: &gtk::Application, language: preferences::Language) {
    use preferences::tr;
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(Some(tr(language, "app.stop")), Some("app.stop"));
    file_menu.append(Some(tr(language, "app.quit")), Some("app.quit"));
    menu.append_submenu(Some(tr(language, "app.file")), &file_menu);

    app.set_menubar(Some(&menu))
}

fn build_ui(app: &Application) {
    // If a window already exists (re-activation), just present it
    if let Some(window) = app.windows().first() {
        window.present();
        return;
    }

    log::debug!("connecting to lan-mouse-socket");
    let (mut frontend_rx, frontend_tx) = match lan_mouse_ipc::try_connect() {
        Ok(conn) => conn,
        Err(e) => {
            log::warn!("could not connect to daemon ({e}), spawning a new one");
            if let Err(spawn_err) = process::Command::new(
                env::current_exe().expect("could not determine executable path"),
            )
            .args(env::args().skip(1))
            .arg("daemon")
            .spawn()
            {
                log::error!("failed to spawn daemon: {spawn_err}");
                process::exit(1);
            }
            match lan_mouse_ipc::connect() {
                Ok(conn) => conn,
                Err(e) => {
                    log::error!("{e}");
                    process::exit(1);
                }
            }
        }
    };
    log::debug!("connected to lan-mouse-socket");

    let (sender, receiver) = async_channel::bounded(10);

    gio::spawn_blocking(move || {
        while let Some(e) = frontend_rx.next_event() {
            match e {
                Ok(e) => {
                    if sender.send_blocking(e).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::error!("{e}");
                    break;
                }
            }
        }
    });

    let window = Window::new(app, frontend_tx);
    window.initial_sync();
    window.setup_drop_target();
    #[cfg(target_os = "macos")]
    window.connect_close_request(|window| {
        window.set_visible(false);
        glib::Propagation::Stop
    });
    #[cfg(windows)]
    if let Err(e) = windows_tray::setup(app, &window) {
        // Leave the window accessible when the notification icon cannot be created.
        log::error!("notification icon unavailable: {e}");
        window.present();
    } else {
        window.connect_close_request(|window| {
            window.set_visible(false);
            glib::Propagation::Stop
        });
    }
    #[cfg(target_os = "macos")]
    {
        macos_status_item::setup(app, &window);
        macos_status_item::set_language(preferences::UiPreferences::load().language);
        // First-launch TCC prompts. No-op when already granted.
        macos_privacy::fire_initial_prompts();
        // Watch the Accessibility grant continuously for the lifetime
        // of the process. On a grant, swap the warning row into its
        // "relaunch required" state (the daemon subprocess already
        // bailed and can't recover without a restart). On a REVOKE,
        // quit immediately — an active CGEventTap at
        // HeadInsertEventTap can wedge system input if the process
        // lingers after losing AX, and forcing the process to exit is
        // the only bulletproof way to guarantee the kernel tears the
        // tap down.
        let window_weak = window.downgrade();
        let app_weak = app.downgrade();
        macos_privacy::watch_accessibility_state(move |change| match change {
            macos_privacy::AccessibilityChange::Granted => {
                if let Some(window) = window_weak.upgrade() {
                    window.present();
                    window.refresh_capture_emulation_status();
                }
            }
            macos_privacy::AccessibilityChange::Revoked => {
                log::warn!("Accessibility revoked — quitting to avoid wedging system input");
                if let Some(app) = app_weak.upgrade() {
                    app.activate_action("quit", None);
                }
            }
        });
    }

    glib::spawn_future_local(clone!(
        #[weak]
        window,
        async move {
            let mut replay = std::collections::VecDeque::new();
            loop {
                let notify = if let Some(event) = replay.pop_front() {
                    event
                } else {
                    match receiver.recv().await {
                        Ok(event) => event,
                        Err(_) => {
                            log::info!("daemon connection closed");
                            if let Some(app) = window.application() {
                                app.quit();
                            }
                            break;
                        }
                    }
                };
                match notify {
                    FrontendEvent::EnrollmentState { addresses, status } => {
                        window.update_enrollment(addresses, status)
                    }
                    FrontendEvent::EnrollmentApproval {
                        id,
                        ip,
                        fingerprint,
                        local_fingerprint,
                    } => window.confirm_enrollment(id, &ip, &fingerprint, &local_fingerprint),
                    FrontendEvent::DiscoveredHosts(hosts) => window.update_discovered_hosts(hosts),
                    FrontendEvent::PresentWindow => window.present(),
                    FrontendEvent::Messages(events) => {
                        window.reset_messages();
                        replay.extend(events);
                    }
                    FrontendEvent::MessagesCleared => window.reset_messages(),
                    FrontendEvent::Created(handle, client, state) => {
                        window.new_client(handle, client, state)
                    }
                    FrontendEvent::Deleted(client) => window.delete_client(client),
                    FrontendEvent::State(handle, config, state) => {
                        window.update_client_config(handle, config);
                        window.update_client_state(handle, state);
                    }
                    FrontendEvent::NoSuchClient(_) => {}
                    FrontendEvent::Error(e) => window.show_toast(e.as_str()),
                    FrontendEvent::Enumerate(clients) => window.update_client_list(clients),
                    FrontendEvent::PortChanged(port, msg) => window.update_port(port, msg),
                    FrontendEvent::DataPortChanged(port, msg) => window.update_data_port(port, msg),
                    FrontendEvent::CaptureStatus(s) => window.set_capture(s.into()),
                    FrontendEvent::EmulationStatus(s) => window.set_emulation(s.into()),
                    FrontendEvent::AuthorizedUpdated(keys) => window.set_authorized_keys(keys),
                    FrontendEvent::PublicKeyFingerprint(fp) => window.set_pk_fp(&fp),
                    FrontendEvent::ConnectionAttempt { fingerprint } => {
                        window.request_authorization(&fingerprint);
                    }
                    FrontendEvent::DeviceConnected {
                        fingerprint: _,
                        addr,
                    } => {
                        window.show_toast(format!("device connected: {addr}").as_str());
                    }
                    FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    } => {
                        window.set_controlling_device(Some((fingerprint.clone(), addr)));
                        window.show_toast(format!("device entered: {addr} ({pos})").as_str());
                    }
                    FrontendEvent::IncomingDisconnected(addr) => {
                        window.clear_controlling_device(addr);
                        window.show_toast(format!("{addr} disconnected").as_str());
                    }
                    // Data sessions are shown in the Direct Links group;
                    // messages are filed under the device that sent them.
                    FrontendEvent::DataSessionConnected(_) => window.refresh_data_sessions(),
                    FrontendEvent::DataSessionDisconnected { device, reason } => {
                        log::debug!("data session disconnected: {device} ({reason})");
                        window.refresh_data_sessions();
                    }
                    // A message this device sent is reported as sent, so
                    // the record never files its own message under the
                    // peer's incoming ones.
                    FrontendEvent::MessageSent {
                        device,
                        message_id,
                        text,
                        ..
                    } => {
                        let peer = window.device_name_for_fingerprint(&device);
                        window.record_message(&device, &peer, Direction::Sent, &message_id, &text);
                    }
                    FrontendEvent::MessageReceived {
                        device,
                        message_id,
                        text,
                    } => {
                        let peer = window.device_name_for_fingerprint(&device);
                        // Incoming messages arrive while the window may
                        // be behind another app, so they get a toast as
                        // well as a record entry.
                        window.show_message(&peer, &text);
                        window.record_message(
                            &device,
                            &peer,
                            Direction::Received,
                            &message_id,
                            &text,
                        );
                    }
                    FrontendEvent::DataSessions(sessions) => window.set_data_sessions(sessions),
                    FrontendEvent::TransferConflicts {
                        device,
                        transfer_id,
                        paths,
                    } => {
                        window.ask_transfer_conflict(&device, &transfer_id, &paths);
                    }
                    FrontendEvent::TransferComplete { device, files } => {
                        window.show_transfer_complete(&device, &files);
                    }
                    FrontendEvent::TransferSent {
                        handle,
                        transfer_id,
                        entries,
                    } => {
                        window.show_transfer_status(
                            &transfer_id,
                            &window.device_name(handle),
                            window::TransferStatus::Sent(entries),
                        );
                    }
                    FrontendEvent::TransferProgress {
                        handle,
                        transfer_id,
                        sent,
                        total,
                        entries,
                    } => {
                        window.update_transfer_progress(handle, &transfer_id, sent, total, entries);
                    }
                    FrontendEvent::TransferFailed {
                        handle,
                        transfer_id,
                        reason,
                        paths,
                    } => {
                        window.show_transfer_failed(handle, &transfer_id, &reason, paths);
                    }
                    FrontendEvent::ReceiveDirChanged(dir, error) => {
                        window.set_receive_dir(&dir, error);
                    }
                    FrontendEvent::SelectTarget { paths } => {
                        window.send_paths_from_cli(paths);
                    }
                    FrontendEvent::CurrentTarget(handle) => {
                        window.set_current_target(handle);
                    }
                    FrontendEvent::SharingState(state) => window.set_sharing_state(state),
                    FrontendEvent::ClipboardStatus(status) => window.set_clipboard_status(status),
                    // Ignore history events from an older daemon during upgrades.
                    FrontendEvent::ClipboardHistory(_)
                    | FrontendEvent::HistoryError
                    | FrontendEvent::HistoryCopied => {}
                    FrontendEvent::ImageMessage {
                        device,
                        outgoing,
                        message_id,
                        filename,
                        preview,
                        thumbnail,
                    } => {
                        window.record_image_message(
                            &device,
                            outgoing,
                            &message_id,
                            &filename,
                            preview,
                            thumbnail,
                        );
                    }
                }
            }
        }
    ));

    // On macOS, default to presenting the main window on every launch
    // so the user gets a visible confirmation that the app is running
    // — including the post-grant relaunch and normal Dock/Finder/`open`
    // launches. Opt out by setting `LAN_MOUSE_HIDDEN=1` in the
    // environment (useful for a LaunchAgent / login-item configuration
    // where the user wants the app to come up quietly into the menu
    // bar only, with no window on boot).
    if env::var_os("LAN_MOUSE_HIDDEN").is_none() && !env::args().any(|arg| arg == "--hidden") {
        window.present();
    }
}
