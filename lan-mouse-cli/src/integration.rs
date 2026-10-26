//! Install and remove the file-manager right-click entry.
//!
//! The two platforms need different mechanisms, and the difference is
//! not cosmetic:
//!
//! - **macOS** uses a Quick Action: a `.workflow` bundle in
//!   `~/Library/Services` that runs our binary as a shell action. It
//!   receives every selected path as one argument list.
//! - **Windows** uses a static shell verb under `*\\shell`. The shell
//!   starts it once per selected item, which is why
//!   [`crate::send::batch`] exists.
//!
//! Neither mechanism needs a privileged installer: both register in the
//! user's own profile, so installing and removing stay reversible
//! without elevation.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::CliError;

/// Identifier used for the service and its files.
pub const TARGET_ID: &str = "de.feschber.LanMouse";

/// What the menu item is called.
pub const LABEL: &str = "Send with Sharemouse";

/// Registry path for the Windows verb.
pub const WINDOWS_VERB_KEY: &str = r"Software\Classes\*\shell\SharemouseSend";

/// Registry path for the folder verb.
pub const WINDOWS_FOLDER_VERB_KEY: &str = r"Software\Classes\Directory\shell\SharemouseSend";

/// A file the integration needs on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    /// Path relative to the install root.
    pub path: PathBuf,
    pub contents: String,
}

/// Quote a path for a shell command line.
///
/// Both the Quick Action and the registry command are shell strings, so
/// an install path with a space would otherwise split the argument. The
/// argv form below is equivalent but unambiguous on both platforms.
pub fn quote_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("\"{}\"", text.replace('"', "\\\""))
}

/// The shell command the Quick Action runs.
///
/// `"$@"` forwards the selected items as separate arguments, which is
/// what keeps names with spaces or non-ASCII characters intact. The
/// subcommand is spelled out in full (`cli send`) because the
/// integration must not depend on how the binary is launched.
pub fn macos_shell_command(exe: &Path) -> String {
    format!("exec {} cli send -- \"$@\"", quote_path(exe))
}

/// Build the Quick Action bundle contents.
///
/// Hand-written plists rather than a serialised model: the Quick Action
/// format is a fixed Automator document, and reproducing only the parts
/// we use keeps the bundle small and auditable.
pub fn macos_workflow_files(exe: &Path) -> Vec<GeneratedFile> {
    let command = macos_shell_command(exe)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    vec![
        GeneratedFile {
            path: PathBuf::from("Info.plist"),
            contents: macos_info_plist(LABEL),
        },
        GeneratedFile {
            path: PathBuf::from("document.wflow"),
            contents: macos_document(&command),
        },
    ]
}

fn macos_info_plist(label: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSMenuItem</key>
			<dict>
				<key>default</key>
				<string>{label}</string>
			</dict>
			<key>NSMessage</key>
			<string>runWorkflowAsService</string>
			<key>NSRequiredContext</key>
			<dict>
				<key>NSApplicationIdentifier</key>
				<string>com.apple.finder</string>
			</dict>
		</dict>
	</array>
	<key>CFBundleName</key>
	<string>{label}</string>
	<key>CFBundleIdentifier</key>
	<string>{id}.send</string>
</dict>
</plist>
"#,
        id = TARGET_ID
    )
}

fn macos_document(command: &str) -> String {
    // The action must receive its input as arguments, not as stdin:
    // a file name may contain a newline, and stdin would split it.
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AMApplicationBuild</key>
	<string>521</string>
	<key>AMApplicationVersion</key>
	<string>2.10</string>
	<key>AMDocumentVersion</key>
	<string>2</string>
	<key>actions</key>
	<array>
		<dict>
			<key>action</key>
			<dict>
				<key>ActionBundlePath</key>
				<string>/System/Library/Automator/Run Shell Script.action</string>
				<key>ActionName</key>
				<string>Run Shell Script</string>
				<key>ActionParameters</key>
				<dict>
					<key>COMMAND_STRING</key>
					<string>{command}</string>
					<key>CheckedForUserDefaultShell</key>
					<true/>
					<key>inputMethod</key>
					<integer>1</integer>
					<key>shell</key>
					<string>/bin/sh</string>
					<key>source</key>
					<string></string>
				</dict>
				<key>BundleIdentifier</key>
				<string>com.apple.RunShellScript</string>
				<key>CFBundleVersion</key>
				<string>2.0.3</string>
				<key>Class Name</key>
				<string>RunShellScriptAction</string>
				<key>InputUUID</key>
				<string>9E27C1A0-5D6E-4C3A-9F41-1A2B3C4D5E6F</string>
				<key>OutputUUID</key>
				<string>9E27C1A0-5D6E-4C3A-9F41-1A2B3C4D5E70</string>
				<key>UUID</key>
				<string>9E27C1A0-5D6E-4C3A-9F41-1A2B3C4D5E71</string>
				<key>arguments</key>
				<dict/>
				<key>isViewVisible</key>
				<integer>1</integer>
				<key>location</key>
				<string>309.000000:253.000000</string>
				<key>nibPath</key>
				<string>/System/Library/Automator/Run Shell Script.action/Contents/Resources/Base.lproj/main.nib</string>
			</dict>
			<key>isViewVisible</key>
			<integer>1</integer>
		</dict>
	</array>
	<key>connectors</key>
	<dict/>
	<key>workflowMetaData</key>
	<dict>
		<key>serviceInputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>serviceOutputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>serviceProcessesInput</key>
		<integer>0</integer>
		<key>workflowTypeIdentifier</key>
		<string>com.apple.Automator.servicesMenu</string>
	</dict>
</dict>
</plist>
"#
    )
}

/// Directory the macOS Quick Action is installed into.
pub fn macos_services_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library")
        .join("Services")
}

/// Where the Quick Action bundle lives.
pub fn macos_workflow_path() -> PathBuf {
    macos_services_dir().join(format!("{LABEL}.workflow"))
}

/// Registry value describing the Windows verb.
///
/// `%1` is what the shell substitutes per selected item. Batching
/// several of those launches back into one transfer happens in our
/// binary, so the command only has to accept a single path.
pub fn windows_verb_command(exe: &Path) -> String {
    format!("{} cli send -- \"%1\"", quote_path(exe))
}

/// Install the right-click entry for the current platform.
#[cfg(target_os = "macos")]
pub fn install(exe: &Path) -> Result<PathBuf, CliError> {
    let bundle = macos_workflow_path();
    write_dir(&bundle, &macos_workflow_files(exe))?;
    refresh_services();
    Ok(bundle)
}

#[cfg(windows)]
pub fn install(exe: &Path) -> Result<PathBuf, CliError> {
    windows_install(exe)?;
    Ok(PathBuf::from(WINDOWS_VERB_KEY))
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn install(_exe: &Path) -> Result<PathBuf, CliError> {
    Err(CliError::IntegrationUnsupported)
}

/// Remove the right-click entry, reporting whether anything was removed.
#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<bool, CliError> {
    let bundle = macos_workflow_path();
    if !bundle.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&bundle)?;
    refresh_services();
    Ok(true)
}

#[cfg(windows)]
pub fn uninstall() -> Result<bool, CliError> {
    windows_uninstall()
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn uninstall() -> Result<bool, CliError> {
    Ok(false)
}

/// Whether the entry is currently installed.
#[cfg(target_os = "macos")]
pub fn is_installed() -> bool {
    macos_workflow_path().exists()
}

#[cfg(windows)]
pub fn is_installed() -> bool {
    windows_installed()
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn is_installed() -> bool {
    false
}

/// Path the entry is installed at, for reporting.
#[cfg(target_os = "macos")]
pub fn install_location() -> PathBuf {
    macos_workflow_path()
}

#[cfg(windows)]
pub fn install_location() -> PathBuf {
    PathBuf::from(WINDOWS_VERB_KEY)
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn install_location() -> PathBuf {
    PathBuf::from("(unsupported platform)")
}

/// Write a directory tree, replacing whatever was there.
fn write_dir(root: &Path, files: &[GeneratedFile]) -> io::Result<()> {
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    for file in files {
        let path = root.join("Contents").join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &file.contents)?;
    }
    Ok(())
}

/// Tell the system the services menu changed.
#[cfg(target_os = "macos")]
fn refresh_services() {
    // pbs caches the services list; without this the Finder menu keeps
    // showing the old entry (or none) until the next login.
    let _ = std::process::Command::new("/System/Library/CoreServices/pbs")
        .arg("-flush")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(windows)]
fn windows_install(exe: &Path) -> Result<(), CliError> {
    let command = windows_verb_command(exe);
    let icon = exe.to_string_lossy().into_owned();
    for key in [WINDOWS_VERB_KEY, WINDOWS_FOLDER_VERB_KEY] {
        set_reg_string(key, "", LABEL)?;
        set_reg_string(key, "Icon", &icon)?;
        // Without this the shell only offers the verb for a single
        // selection, and a multi-selection would not show it at all.
        set_reg_string(key, "MultiSelectModel", "Player")?;
        set_reg_string(&format!(r"{key}\command"), "", &command)?;
    }
    Ok(())
}

#[cfg(windows)]
fn windows_uninstall() -> Result<bool, CliError> {
    let mut removed = false;
    for key in [WINDOWS_VERB_KEY, WINDOWS_FOLDER_VERB_KEY] {
        match delete_reg_tree(key) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(removed)
}

#[cfg(windows)]
fn windows_installed() -> bool {
    let Some(key) = open_reg_key(WINDOWS_VERB_KEY) else {
        return false;
    };
    let _ = unsafe { windows::Win32::System::Registry::RegCloseKey(key) };
    true
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Open a key under `HKEY_CURRENT_USER`, creating it if needed.
#[cfg(windows)]
fn create_reg_key(path: &str) -> Option<windows::Win32::System::Registry::HKEY> {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, KEY_WRITE, REG_OPTION_NON_VOLATILE, RegCreateKeyExW,
    };
    let mut key = HKEY::default();
    let mut disposition = Default::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(to_wide(path).as_ptr()),
            None,
            windows::core::PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_SET_VALUE,
            None,
            &mut key,
            Some(&mut disposition),
        )
    };
    status.is_ok().then_some(key)
}

#[cfg(windows)]
fn open_reg_key(path: &str) -> Option<windows::Win32::System::Registry::HKEY> {
    use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_READ, RegOpenKeyExW};
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(to_wide(path).as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    };
    status.is_ok().then_some(key)
}

#[cfg(windows)]
fn set_reg_string(path: &str, name: &str, value: &str) -> Result<(), CliError> {
    use windows::Win32::System::Registry::{REG_SZ, RegCloseKey, RegSetValueExW};
    let key = create_reg_key(path).ok_or(CliError::IntegrationUnsupported)?;
    let wide = to_wide(value);
    let bytes = unsafe {
        std::slice::from_raw_parts(
            wide.as_ptr() as *const u8,
            wide.len() * std::mem::size_of::<u16>(),
        )
    };
    let status = unsafe {
        RegSetValueExW(
            key,
            windows::core::PCWSTR(to_wide(name).as_ptr()),
            None,
            REG_SZ,
            Some(bytes),
        )
    };
    let _ = unsafe { RegCloseKey(key) };
    status.ok().map_err(|_| CliError::IntegrationUnsupported)
}

#[cfg(windows)]
fn delete_reg_tree(path: &str) -> io::Result<()> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
    let status = unsafe {
        RegDeleteTreeW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(to_wide(path).as_ptr()),
        )
    };
    if status.is_ok() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not delete registry key",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/Applications/Lan Mouse.app/Contents/MacOS/lan-mouse";

    fn exe() -> PathBuf {
        PathBuf::from(EXE)
    }

    #[test]
    fn the_shell_command_forwards_selected_items_as_arguments() {
        let command = macos_shell_command(&exe());
        // `"$@"` is what preserves spaces and Unicode in a file name.
        assert!(command.contains("\"$@\""), "{command}");
        assert!(command.contains("cli send --"), "{command}");
    }

    #[test]
    fn the_shell_command_uses_a_subcommand_the_binary_actually_has() {
        // `send` lives under `cli`; a bare `lan-mouse send` would fail
        // with clap's "unrecognized subcommand" before any work starts.
        assert!(macos_shell_command(&exe()).contains(" cli send "));
        assert!(windows_verb_command(&exe()).contains(" cli send "));
    }

    #[test]
    fn an_install_path_with_a_space_is_quoted() {
        let command = macos_shell_command(&exe());
        assert!(
            command.starts_with("exec \"/Applications/Lan Mouse.app"),
            "{command}"
        );
    }

    #[test]
    fn a_quote_in_the_path_cannot_end_the_quoting_early() {
        let exe = Path::new("/tmp/we\"ird/lan-mouse");
        let command = macos_shell_command(exe);
        assert!(command.contains("we\\\"ird"), "{command}");
    }

    #[test]
    fn the_workflow_bundle_contains_both_required_files() {
        let files = macos_workflow_files(&exe());
        let names: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            names,
            vec![PathBuf::from("Info.plist"), PathBuf::from("document.wflow")]
        );
    }

    #[test]
    fn the_generated_plists_are_well_formed_xml() {
        for file in macos_workflow_files(&exe()) {
            assert!(file.contents.starts_with("<?xml version=\"1.0\""));
            assert!(file.contents.contains("<plist version=\"1.0\">"));
            assert!(file.contents.ends_with("</plist>\n"));
            // Tags must balance, which catches a mangled escape.
            let open = file.contents.matches("<dict>").count();
            let close = file.contents.matches("</dict>").count();
            assert_eq!(open, close, "{}", file.path.display());
        }
    }

    #[test]
    fn the_quick_action_is_offered_for_files_and_folders() {
        let document = macos_workflow_files(&exe())
            .into_iter()
            .find(|f| f.path == Path::new("document.wflow"))
            .unwrap();
        // `fileSystemObject` is what makes Finder offer the item for a
        // selection of files and folders alike.
        assert!(
            document
                .contents
                .contains("com.apple.Automator.fileSystemObject")
        );
        assert!(
            document
                .contents
                .contains("com.apple.Automator.servicesMenu")
        );
    }

    #[test]
    fn the_menu_item_carries_our_label() {
        let info = macos_workflow_files(&exe())
            .into_iter()
            .find(|f| f.path == Path::new("Info.plist"))
            .unwrap();
        assert!(info.contents.contains(LABEL));
    }

    #[test]
    fn the_windows_command_passes_one_item_per_invocation() {
        let command = windows_verb_command(Path::new(r"C:\Program Files\Lan Mouse\lan-mouse.exe"));
        assert!(command.contains("\"%1\""), "{command}");
        assert!(command.contains("cli send --"), "{command}");
        assert!(
            command.starts_with("\"C:\\Program Files\\Lan Mouse\\lan-mouse.exe\""),
            "{command}"
        );
    }

    #[test]
    fn the_windows_verb_is_registered_for_files_and_directories() {
        assert_ne!(WINDOWS_VERB_KEY, WINDOWS_FOLDER_VERB_KEY);
        assert!(WINDOWS_VERB_KEY.contains(r"*\shell"));
        assert!(WINDOWS_FOLDER_VERB_KEY.contains(r"Directory\shell"));
    }

    #[test]
    fn the_install_path_is_inside_the_user_profile() {
        assert!(!Path::new(TARGET_ID).is_absolute());
        #[cfg(target_os = "macos")]
        {
            let home = std::env::var("HOME").unwrap();
            assert!(macos_workflow_path().starts_with(&home));
            assert!(
                macos_workflow_path()
                    .to_string_lossy()
                    .ends_with(".workflow")
            );
        }
    }
}
