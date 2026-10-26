//! Right-click "send to device" entry point.
//!
//! Both file managers hand the selection to this binary, but they do it
//! differently: a macOS Quick Action starts one process with every
//! selected path as an argument, while a Windows static verb registered
//! under `*\\shell` is started once per selected item. `MultiSelectModel`
//! does not merge those launches - the shell only uses it to decide
//! whether the verb is offered at all - so a batch of five files means
//! five processes with one path each.
//!
//! [`batch`] turns either shape into one transfer: every process drops
//! its paths into a spool directory, and exactly one of them (the one
//! that wrote the earliest entry) picks them all up and sends.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use clap::Args;
use futures::StreamExt;
use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, FrontendEvent, FrontendRequest, connect_async,
    is_service_running,
};

use crate::CliError;

pub mod batch;

/// How long to wait for paths that belong to the same right-click.
///
/// The Windows shell starts one process per selected file, and those
/// starts are not synchronised. Waiting for a quiet period after the
/// last arrival is what turns them back into one transfer; the cap
/// bounds the wait for a pathological burst.
const BATCH_QUIET: Duration = Duration::from_millis(150);
const BATCH_MAX_WAIT: Duration = Duration::from_secs(2);

/// Drop entries this old: a crashed invocation must not block later
/// right-clicks indefinitely.
const BATCH_MAX_AGE: Duration = Duration::from_secs(60);

/// How long to wait for a freshly started app to open its IPC socket.
const WAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for the first sign that a transfer started.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct SendArgs {
    /// device to send to, by hostname as shown in the app or by `#handle`;
    /// the app asks which device when this is omitted
    #[arg(long, short)]
    pub device: Option<String>,
    /// files or folders to send
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<PathBuf>,
}

/// Run the right-click entry point.
///
/// Order matters: the selection is spooled and batched first (cheap and
/// local), then the app is started if needed, and only then is the
/// transfer requested. Starting the app first would multiply the wait
/// by the number of invocations the shell made.
pub async fn run(args: SendArgs) -> Result<(), CliError> {
    let paths = selection_paths(&args)?;
    if paths.is_empty() {
        // Another invocation owns this batch and will send our paths
        // along with its own; nothing left to do here.
        return Ok(());
    }

    let already_running = is_service_running();
    ensure_app_running().await?;
    if args.device.is_none() && already_running {
        // A daemon alone cannot show a picker; application activation is idempotent.
        spawn_app(&std::env::current_exe()?)?;
    }

    match args.device {
        Some(spec) => send_to_named_device(&spec, paths).await,
        None => ask_app_to_choose_device(paths).await,
    }
}

// Explicit destinations must never enter the shared shell-selection spool.
fn selection_paths(args: &SendArgs) -> Result<Vec<PathBuf>, CliError> {
    let paths = args
        .paths
        .iter()
        .map(|path| std::fs::canonicalize(path).or_else(|_| std::path::absolute(path)))
        .collect::<Result<Vec<_>, _>>()?;
    if args.device.is_some() {
        Ok(paths)
    } else {
        batch_paths(&paths)
    }
}

/// Fold this process' selection into the shared batch.
///
/// Returns the full selection when this process is the one that should
/// send it, and an empty vector when another process owns the batch.
fn batch_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, CliError> {
    let dir = batch::spool_dir();
    let canonical: Vec<PathBuf> = paths
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let all = batch::submit(&dir, &canonical, BATCH_QUIET, BATCH_MAX_WAIT, BATCH_MAX_AGE)?;
    Ok(all.unwrap_or_default())
}

/// Make sure something is listening on the IPC socket.
///
/// Starting the app is a last resort: when it (or its daemon) is
/// already up we must not start a second copy, which would fail to bind
/// the socket and then fail the send.
async fn ensure_app_running() -> Result<(), CliError> {
    if is_service_running() {
        return Ok(());
    }

    spawn_app(&std::env::current_exe()?)?;

    let deadline = std::time::Instant::now() + WAKE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if is_service_running() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(CliError::AppNotStarting(WAKE_TIMEOUT))
}

/// Start the app without tying it to this process.
///
/// Inside a `.app` bundle the executable is started through
/// LaunchServices: started directly, the process is not registered as
/// an application and gets no menu bar or dock presence. Detached in
/// both cases so closing the right-click does not take the app down.
fn spawn_app(exe: &Path) -> Result<(), CliError> {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = macos_bundle(exe) {
        let opened = Command::new("/usr/bin/open")
            .arg("-gja")
            .arg(&bundle)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if opened.is_ok_and(|status| status.success()) {
            return Ok(());
        }
        // Fall through: a broken LaunchServices registration must not
        // make the send impossible.
    }
    let _ = exe;
    Command::new(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// The `.app` bundle containing `exe`, if it is inside one.
#[cfg(target_os = "macos")]
fn macos_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

async fn send_to_named_device(spec: &str, paths: Vec<PathBuf>) -> Result<(), CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    tx.request(FrontendRequest::Enumerate()).await?;

    let clients = loop {
        match rx.next().await {
            Some(Ok(FrontendEvent::Enumerate(clients))) => break clients,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => return Err(CliError::ServiceWentAway),
        }
    };
    let handle = resolve_device(&clients, spec).ok_or_else(|| CliError::UnknownDevice {
        spec: spec.to_owned(),
        known: clients
            .iter()
            .map(|(handle, config, _)| device_label(*handle, config))
            .collect(),
    })?;

    tx.request(FrontendRequest::SendFiles { handle, paths })
        .await?;

    // Report the real outcome rather than "request sent": a transfer can
    // fail before the first byte moves (device offline, not authorized).
    let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
    loop {
        let event = match tokio::time::timeout_at(deadline, rx.next()).await {
            Ok(Some(Ok(event))) => event,
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(None) => return Err(CliError::ServiceWentAway),
            Err(_) => return Err(CliError::NoResponse(CONFIRM_TIMEOUT)),
        };
        match event {
            FrontendEvent::TransferProgress { handle: h, .. } if h == handle => return Ok(()),
            FrontendEvent::TransferFailed {
                handle: h, reason, ..
            } if h == handle => return Err(CliError::TransferFailed(reason)),
            FrontendEvent::Error(message) => return Err(CliError::Service(message)),
            _ => continue,
        }
    }
}

/// Hand the selection to the app and let it ask which device.
///
/// The request is only a hint: the app keeps it until a window exists,
/// because a right-click may have just started the app and events are
/// only broadcast to frontends that are already listening.
async fn ask_app_to_choose_device(paths: Vec<PathBuf>) -> Result<(), CliError> {
    let (_rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    tx.request(FrontendRequest::SelectTarget { paths }).await?;
    tx.flush().await?;
    Ok(())
}

/// Find the device the user named.
///
/// Accepts the hostname shown in the app (case-insensitively, because
/// nobody retypes the exact capitalisation into a shell) or the
/// `#handle` form the app displays for unnamed devices.
fn resolve_device(
    clients: &[(ClientHandle, ClientConfig, ClientState)],
    spec: &str,
) -> Option<ClientHandle> {
    if let Some(number) = spec
        .strip_prefix('#')
        .and_then(|n| n.parse::<ClientHandle>().ok())
    {
        if let Some((handle, _, _)) = clients.iter().find(|(handle, _, _)| *handle == number) {
            return Some(*handle);
        }
    }
    // An exact match wins over a case-insensitive one so two devices
    // whose names differ only in case stay distinguishable.
    if let Some((handle, _, _)) = clients
        .iter()
        .find(|(_, config, _)| config.hostname.as_deref() == Some(spec))
    {
        return Some(*handle);
    }
    clients
        .iter()
        .find(|(_, config, _)| {
            config
                .hostname
                .as_deref()
                .is_some_and(|host| host.eq_ignore_ascii_case(spec))
        })
        .map(|(handle, _, _)| *handle)
}

fn device_label(handle: ClientHandle, config: &ClientConfig) -> String {
    match config.hostname.as_deref().filter(|h| !h.is_empty()) {
        Some(host) => format!("{host} (#{handle})"),
        None => format!("#{handle}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(
        handle: ClientHandle,
        hostname: Option<&str>,
    ) -> (ClientHandle, ClientConfig, ClientState) {
        (
            handle,
            ClientConfig {
                hostname: hostname.map(str::to_owned),
                ..Default::default()
            },
            ClientState::default(),
        )
    }

    #[test]
    fn explicit_destinations_resolve_relative_files_without_merging_selections() {
        let cwd = std::env::current_dir().unwrap();
        let relative = PathBuf::from(format!(
            "target/send-selection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = cwd.join(&relative);
        std::fs::create_dir_all(&dir).unwrap();
        for device in ["A", "B"] {
            let filename = format!("{device}-file");
            std::fs::write(dir.join(&filename), device).unwrap();
            let selected = selection_paths(&SendArgs {
                device: Some(device.into()),
                paths: vec![relative.join(&filename)],
            })
            .unwrap();
            assert_eq!(
                selected,
                vec![std::fs::canonicalize(dir.join(&filename)).unwrap()]
            );
            assert!(selected[0].is_absolute());
            assert_eq!(std::fs::read_to_string(&selected[0]).unwrap(), device);
            std::fs::remove_file(dir.join(filename)).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn resolves_device_by_hostname() {
        let clients = vec![client(0, Some("macbook")), client(1, Some("desktop"))];
        assert_eq!(resolve_device(&clients, "desktop"), Some(1));
    }

    #[test]
    fn resolves_device_ignoring_hostname_case() {
        let clients = vec![client(0, Some("MacBook-Pro"))];
        assert_eq!(resolve_device(&clients, "macbook-pro"), Some(0));
    }

    #[test]
    fn exact_hostname_match_wins_over_case_insensitive_one() {
        let clients = vec![client(0, Some("desktop")), client(1, Some("Desktop"))];
        assert_eq!(resolve_device(&clients, "Desktop"), Some(1));
        assert_eq!(resolve_device(&clients, "desktop"), Some(0));
    }

    #[test]
    fn resolves_device_by_handle() {
        let clients = vec![client(0, Some("macbook")), client(1, None)];
        assert_eq!(resolve_device(&clients, "#1"), Some(1));
    }

    #[test]
    fn does_not_confuse_a_missing_device_with_handle_zero() {
        let clients = vec![client(0, Some("macbook"))];
        assert_eq!(resolve_device(&clients, "linux"), None);
        assert_eq!(resolve_device(&clients, "#7"), None);
    }

    #[test]
    fn unknown_device_is_not_resolved_by_prefix() {
        // Hostnames are a user-visible label; a prefix match would send
        // to the wrong machine when two of them share a prefix.
        let clients = vec![client(0, Some("desktop-work"))];
        assert_eq!(resolve_device(&clients, "desktop"), None);
    }

    #[test]
    fn device_labels_prefer_the_hostname() {
        let (_, named, _) = client(3, Some("macbook"));
        let (_, unnamed, _) = client(4, None);
        assert_eq!(device_label(3, &named), "macbook (#3)");
        assert_eq!(device_label(4, &unnamed), "#4");
    }

    #[test]
    fn empty_hostname_is_not_shown_as_a_device_name() {
        let (_, config, _) = client(5, Some(""));
        assert_eq!(device_label(5, &config), "#5");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn app_bundle_is_found_from_the_executable() {
        let exe = Path::new("/Applications/Lan Mouse.app/Contents/MacOS/lan-mouse");
        assert_eq!(
            macos_bundle(exe),
            Some(PathBuf::from("/Applications/Lan Mouse.app"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_plain_binary_has_no_app_bundle() {
        assert_eq!(macos_bundle(Path::new("/usr/local/bin/lan-mouse")), None);
    }
}
