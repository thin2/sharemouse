//! Per-user login registration. No KeepAlive: an explicit quit stays quit.
use std::{io, path::Path};

pub fn apply(enabled: bool) -> io::Result<()> {
    // Integration fixtures must not register their temporary executable.
    if std::env::var_os("LAN_MOUSE_DISABLE_AUTOSTART").is_some() {
        return Ok(());
    }
    register(enabled, &std::env::current_exe()?)
}

#[cfg(target_os = "macos")]
fn register(enabled: bool, executable: &Path) -> io::Result<()> {
    let path = gtk::glib::home_dir().join("Library/LaunchAgents/de.feschber.LanMouse.plist");
    if enabled {
        std::fs::create_dir_all(path.parent().expect("LaunchAgents parent"))?;
        // Atomic rename avoids a partial plist if login overlaps a settings write.
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temp, launch_agent(executable)?)?;
        std::fs::rename(temp, path)
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

#[cfg(target_os = "macos")]
fn launch_agent(executable: &Path) -> io::Result<String> {
    let executable = executable
        .to_str()
        .ok_or_else(|| io::Error::other("executable path is not UTF-8"))?;
    let executable = gtk::glib::markup_escape_text(executable);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>de.feschber.LanMouse</string>
<key>ProgramArguments</key><array><string>{executable}</string><string>--hidden</string></array>
<key>RunAtLoad</key><true/>
<key>ProcessType</key><string>Interactive</string>
</dict></plist>
"#
    ))
}

#[cfg(windows)]
fn register(enabled: bool, executable: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let mut cmd = std::process::Command::new("reg.exe");
    cmd.creation_flags(0x08000000);
    if enabled {
        cmd.args(["add", key, "/v", "Sharemouse", "/t", "REG_SZ", "/d"])
            .arg(format!("\"{}\" --hidden", executable.display()))
            .arg("/f");
    } else {
        // A missing value already means disabled; only delete after a successful query.
        let status = std::process::Command::new("reg.exe")
            .creation_flags(0x08000000)
            .args(["query", key, "/v", "Sharemouse"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Ok(());
        }
        cmd.args(["delete", key, "/v", "Sharemouse", "/f"]);
    }
    if cmd.output()?.status.success() {
        Ok(())
    } else {
        Err(io::Error::other("could not update login registration"))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn register(_: bool, _: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn agent_escapes_path_and_does_not_respawn_after_quit() {
        let xml = launch_agent(Path::new(
            "/Applications/A & B.app/Contents/MacOS/lan-mouse",
        ))
        .unwrap();
        assert!(xml.contains("A &amp; B.app"));
        assert!(xml.contains("<string>--hidden</string>"));
        assert!(!xml.contains("KeepAlive"));
    }
}
