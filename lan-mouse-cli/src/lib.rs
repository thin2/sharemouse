use clap::{Args, Parser, Subcommand};
use futures::StreamExt;

use std::{net::IpAddr, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    ClientHandle, ConnectionError, FrontendEvent, FrontendRequest, IpcError, Position,
    connect_async,
};

pub mod integration;
pub mod send;

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
    #[error("the app did not open its control socket within {0:?}")]
    AppNotStarting(Duration),
    #[error("the app closed the control connection")]
    ServiceWentAway,
    #[error("the app did not answer within {0:?}")]
    NoResponse(Duration),
    #[error("the app reported an error: {0}")]
    Service(String),
    #[error("sending failed: {0}")]
    TransferFailed(String),
    #[error("no device named `{spec}`; known devices: {known}", known = known.join(", "))]
    UnknownDevice { spec: String, known: Vec<String> },
    #[error("this platform has no right-click integration")]
    IntegrationUnsupported,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "lan-mouse-cli", about = "LanMouse CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    enter_hook: Option<String>,
    #[arg(long)]
    leave_hook: Option<String>,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// Gracefully quit the running app and release native input.
    Quit,
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
    /// send files or folders to a device
    ///
    /// Without `--device` the running app asks which device to use, so
    /// this is the entry point a file manager context menu can call.
    Send(send::SendArgs),
    /// install, remove or inspect the file manager right-click entry
    Integration {
        #[command(subcommand)]
        command: IntegrationCommand,
    },
}

#[derive(Clone, Copy, Subcommand, Debug, PartialEq, Eq)]
pub enum IntegrationCommand {
    /// add the right-click entry
    Install,
    /// remove the right-click entry
    Uninstall,
    /// report whether the entry is installed
    Status,
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    // `send` owns its own connections: it may need to start the app
    // first, and it reports the transfer outcome instead of just
    // acknowledging the request.
    if let CliSubcommand::Send(args) = cmd {
        return send::run(args).await;
    }
    // Integration only touches local files and the registry, so it must
    // work with the app closed.
    if let CliSubcommand::Integration { command } = cmd {
        return run_integration(command);
    }
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    match cmd {
        CliSubcommand::Quit => {
            tx.request(FrontendRequest::Shutdown).await?;
            tokio::time::timeout(Duration::from_secs(8), async {
                while rx.next().await.is_some() {}
            })
            .await
            .map_err(|_| CliError::NoResponse(Duration::from_secs(8)))?;
        }
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            enter_hook,
            leave_hook,
        }) => {
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
                    if let Some(hostname) = hostname {
                        tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                            .await?;
                    }
                    if let Some(port) = port {
                        tx.request(FrontendRequest::UpdatePort(handle, port))
                            .await?;
                    }
                    if let Some(ips) = ips {
                        tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                            .await?;
                    }
                    if let Some(enter_hook) = enter_hook {
                        tx.request(FrontendRequest::UpdateEnterHook(handle, Some(enter_hook)))
                            .await?;
                    }
                    if let Some(leave_hook) = leave_hook {
                        tx.request(FrontendRequest::UpdateLeaveHook(handle, Some(leave_hook)))
                            .await?;
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
                    for (handle, config, state) in clients {
                        let host = config.hostname.unwrap_or("unknown".to_owned());
                        let port = config.port;
                        let pos = config.pos;
                        let active = state.active;
                        let ips = state.ips;
                        println!(
                            "id {handle}: {host}:{port} ({pos}) active: {active}, ips: {ips:?}"
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::SetIps { id, ips } => {
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
        // Handled above, before the shared connection is opened.
        CliSubcommand::Send(_) => unreachable!("send returns early"),
        CliSubcommand::Integration { .. } => unreachable!("integration returns early"),
    }
    Ok(())
}

/// Install, remove or report the right-click entry.
///
/// Points the entry at this executable: when the user later moves the
/// app, re-running `install` rewrites the target.
fn run_integration(command: IntegrationCommand) -> Result<(), CliError> {
    match command {
        IntegrationCommand::Install => {
            let exe = std::env::current_exe()?;
            let location = integration::install(&exe)?;
            println!("installed right-click entry at {}", location.display());
        }
        IntegrationCommand::Uninstall => {
            if integration::uninstall()? {
                println!(
                    "removed right-click entry from {}",
                    integration::install_location().display()
                );
            } else {
                println!("no right-click entry was installed");
            }
        }
        IntegrationCommand::Status => {
            let location = integration::install_location();
            if integration::is_installed() {
                println!("installed at {}", location.display());
            } else {
                println!("not installed (expected at {})", location.display());
            }
        }
    }
    Ok(())
}
