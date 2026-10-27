//! User-interface preferences and runtime translations.
//!
//! These settings belong to the frontend, not the daemon: the daemon
//! owns connection and layout state, while language and color scheme
//! are stored in a small key file next to the daemon config. Keeping
//! them separate means switching the theme does not rewrite
//! `config.toml` or trigger a daemon config reload.

use std::{env, path::PathBuf};

use gtk::glib;
use lan_mouse_ipc::Position;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    #[default]
    En,
    Zh,
}

impl Language {
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Zh => "zh",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        // Match the language subtag only: `zh_CN.UTF-8`, `zh-CN` and
        // `en_US` all reduce to their two-letter language.
        let code = code.split(['.', '@']).next().unwrap_or(code);
        let language = code.split(['_', '-']).next().unwrap_or(code);
        match language {
            "en" => Some(Self::En),
            "zh" => Some(Self::Zh),
            _ => None,
        }
    }

    /// Native label used in the language selector.
    pub fn label(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Zh => "中文",
        }
    }

    /// Index of this language in the selector model, and back. The
    /// template callback reads the combo row's selection, so the two
    /// mappings must stay in one place to avoid drifting apart.
    pub fn index(self) -> u32 {
        match self {
            Self::En => 0,
            Self::Zh => 1,
        }
    }

    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Zh,
            _ => Self::En,
        }
    }

    /// Follow the desktop locale on first launch, defaulting to English
    /// when the locale is missing or unrecognised.
    pub fn system_default() -> Self {
        for key in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(value) = env::var(key) {
                if let Some(language) = Self::from_code(&value) {
                    return language;
                }
            }
        }
        Self::En
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub fn code(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    /// Index of this theme in the selector model, and back.
    pub fn index(self) -> u32 {
        match self {
            Self::System => 0,
            Self::Light => 1,
            Self::Dark => 2,
        }
    }

    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Light,
            2 => Self::Dark,
            _ => Self::System,
        }
    }
}

/// Accent family for the whole window. Each palette tints every
/// neutral toward its hue and keeps one accent for the control target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Palette {
    #[default]
    Ember,
    Pine,
}

impl Palette {
    pub fn code(self) -> &'static str {
        match self {
            Self::Ember => "ember",
            Self::Pine => "pine",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "ember" => Some(Self::Ember),
            "pine" => Some(Self::Pine),
            _ => None,
        }
    }

    /// Index of this palette in the selector model, and back.
    pub fn index(self) -> u32 {
        match self {
            Self::Ember => 0,
            Self::Pine => 1,
        }
    }

    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Pine,
            _ => Self::Ember,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiPreferences {
    pub autostart: bool,
    pub language: Language,
    pub theme: Theme,
    pub palette: Palette,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            autostart: true,
            language: Language::system_default(),
            theme: Theme::System,
            palette: Palette::Ember,
        }
    }
}

impl UiPreferences {
    pub fn load() -> Self {
        let key_file = glib::KeyFile::new();
        if key_file
            .load_from_file(prefs_path(), glib::KeyFileFlags::NONE)
            .is_err()
        {
            return Self::default();
        }
        Self::from_key_file(&key_file)
    }

    pub fn save(&self) -> Result<(), String> {
        let path = prefs_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        self.to_key_file()
            .save_to_file(path)
            .map_err(|e| e.to_string())
    }

    fn from_key_file(key_file: &glib::KeyFile) -> Self {
        let language = key_file
            .string("ui", "language")
            .ok()
            .and_then(|value| Language::from_code(value.as_str()))
            .unwrap_or_else(Language::system_default);
        let theme = key_file
            .string("ui", "theme")
            .ok()
            .and_then(|value| Theme::from_code(value.as_str()))
            .unwrap_or_default();
        let palette = key_file
            .string("ui", "palette")
            .ok()
            .and_then(|value| Palette::from_code(value.as_str()))
            .unwrap_or_default();
        let autostart = key_file.boolean("ui", "autostart").unwrap_or(true);
        Self {
            language,
            theme,
            palette,
            autostart,
        }
    }

    fn to_key_file(self) -> glib::KeyFile {
        let key_file = glib::KeyFile::new();
        key_file.set_boolean("ui", "autostart", self.autostart);
        key_file.set_string("ui", "language", self.language.code());
        key_file.set_string("ui", "theme", self.theme.code());
        key_file.set_string("ui", "palette", self.palette.code());
        key_file
    }
}

pub fn prefs_path() -> PathBuf {
    glib::user_config_dir().join("lan-mouse").join("ui.ini")
}

/// Translate a key for the active language. Unknown keys return a
/// visible placeholder so a missing entry fails loudly in the UI
/// rather than silently displaying English in the other language.
pub fn tr(language: Language, key: &str) -> &'static str {
    match (language, key) {
        (Language::Zh, "enrollment.title") => "连接设备",
        (Language::Zh, "enrollment.help") => {
            "主设备点击“显示连接 IP”后，其他设备会在“附近的主设备”中自动看到它，点击“连接”即可。找不到时可手动输入 IP。两端确认后，再启动键鼠共享。"
        }
        (Language::Zh, "enrollment.nearby") => "附近的主设备",
        (Language::Zh, "enrollment.searching") => {
            "正在查找……请在主设备点击“显示连接 IP”。如果一直找不到，请在下方手动输入主设备 IP。"
        }
        (Language::Zh, "enrollment.connect") => "连接",
        (Language::Zh, "enrollment.host") => "显示连接 IP",
        (Language::Zh, "enrollment.ip_hint") => {
            "在其他设备的“主设备 IP”中填写同一网络对应的一个地址。连接 Windows 热点时，请使用热点网卡地址（通常为 192.168.137.1）。"
        }
        (Language::Zh, "enrollment.no_ip") => "未找到本机 IPv4，请先连接局域网，再重试。",
        (Language::Zh, "enrollment.setup_hint") => {
            "先显示本机 IP，让其他设备加入，再开始键鼠共享。"
        }
        (Language::Zh, "enrollment.join") => "连接主设备",
        (Language::Zh, "enrollment.stop") => "停止配对",
        (Language::Zh, "enrollment.address") => "主设备 IP，例如 192.168.1.10",
        (Language::Zh, "enrollment.idle") => "尚未加入连接组",
        (Language::Zh, "enrollment.hosting") => "等待其他设备加入，主设备 IP",
        (Language::Zh, "enrollment.joining") => "正在连接，请在两台设备上确认",
        (Language::Zh, "enrollment.joined") => "已加入连接组，可在下方选择主控并开始共享",
        (Language::Zh, "enrollment.failed") => "连接未完成，请检查 IP、主设备等待状态和防火墙",
        (Language::Zh, "enrollment.stopped") => "已停止新设备配对，现有连接保留",
        (Language::Zh, "enrollment.approve") => "确认加入",
        (Language::Zh, "enrollment.confirm") => "确认设备身份",
        (Language::Zh, "enrollment.verify") => {
            "请核对下面两组指纹与另一台设备屏幕一致。确认后，两台设备会互相授权，并信任主设备批准的组成员（最多三台）。"
        }
        (Language::Zh, "enrollment.local") => "本机指纹",
        (Language::Zh, "enrollment.remote") => "对方指纹",
        (Language::En, "enrollment.title") => "Connect devices",
        (Language::En, "enrollment.help") => {
            "Select Show connection IP on the main device. Other devices list it under Nearby main devices; select Connect. If it does not appear, enter the IP manually. Confirm on both devices, then start sharing."
        }
        (Language::En, "enrollment.nearby") => "Nearby main devices",
        (Language::En, "enrollment.searching") => {
            "Searching… Select Show connection IP on the main device. If nothing appears, enter the main device IP below."
        }
        (Language::En, "enrollment.connect") => "Connect",
        (Language::En, "enrollment.host") => "Show connection IP",
        (Language::En, "enrollment.ip_hint") => {
            "Enter one address on the other device's network in “Main device IP”. For a Windows hotspot, use the hotspot adapter address (usually 192.168.137.1)."
        }
        (Language::En, "enrollment.no_ip") => {
            "No local IPv4 found. Connect to your local network and retry."
        }
        (Language::En, "enrollment.setup_hint") => {
            "Show this device's IP, connect the other devices, then start sharing."
        }
        (Language::En, "enrollment.join") => "Connect to main device",
        (Language::En, "enrollment.stop") => "Stop pairing",
        (Language::En, "enrollment.address") => "Main device IP, e.g. 192.168.1.10",
        (Language::En, "enrollment.idle") => "Not in a connection group",
        (Language::En, "enrollment.hosting") => "Waiting for devices. Main device IP",
        (Language::En, "enrollment.joining") => "Connecting. Confirm on both devices",
        (Language::En, "enrollment.joined") => {
            "Group joined. Choose a primary below to start sharing"
        }
        (Language::En, "enrollment.failed") => {
            "Connection incomplete. Check IP, joining mode and firewall"
        }
        (Language::En, "enrollment.stopped") => {
            "New pairing stopped; existing connections retained"
        }
        (Language::En, "enrollment.approve") => "Confirm joining",
        (Language::En, "enrollment.confirm") => "Confirm device identity",
        (Language::En, "enrollment.verify") => {
            "Check that both fingerprints below match the other device's screen. Confirmation authorizes each device and trusts the members approved by the main device (up to three devices)."
        }
        (Language::En, "enrollment.local") => "This device fingerprint",
        (Language::En, "enrollment.remote") => "Other device fingerprint",
        (Language::Zh, "conversation.devices") => "设备",
        (Language::Zh, "conversation.files") => "文件",
        (Language::Zh, "conversation.settings") => "设置",
        (Language::Zh, "conversation.peers") => "设备对话",
        (Language::Zh, "conversation.add_device") => "连接设备",
        (Language::Zh, "conversation.send_files") => "发送文件",
        (Language::Zh, "conversation.send_folder") => "发送文件夹",
        (Language::Zh, "conversation.files_hint") => {
            "将文件或文件夹拖入窗口，选择接收设备即可发送。"
        }
        (Language::Zh, "conversation.conversation") => "与这台设备对话",
        (Language::Zh, "conversation.send") => "发送",
        (Language::Zh, "conversation.image") => "查看图片",
        (Language::Zh, "conversation.clear") => "清空所有对话",
        (Language::Zh, "conversation.clear_hint") => {
            "清除本次运行的所有设备消息，不改变系统剪贴板。"
        }
        (Language::En, "conversation.devices") => "Devices",
        (Language::En, "conversation.files") => "Files",
        (Language::En, "conversation.settings") => "Settings",
        (Language::En, "conversation.peers") => "Conversations",
        (Language::En, "conversation.add_device") => "Connect a device",
        (Language::En, "conversation.send_files") => "Send files",
        (Language::En, "conversation.send_folder") => "Send folder",
        (Language::En, "conversation.files_hint") => {
            "Drop files or folders into this window, then choose a receiving device."
        }
        (Language::En, "conversation.conversation") => "Conversation with",
        (Language::En, "conversation.send") => "Send",
        (Language::En, "conversation.image") => "View image",
        (Language::En, "conversation.clear") => "Clear all conversations",
        (Language::En, "conversation.clear_hint") => {
            "Remove messages with all devices for this run. The system clipboard is unchanged."
        }
        (Language::En, "app.file") => "File",
        (Language::Zh, "app.file") => "文件",
        (Language::En, "pref.autostart") => "Start at login",
        (Language::Zh, "pref.autostart") => "登录时自动启动",
        (Language::En, "app.open") => "Open Lan Mouse",
        (Language::Zh, "app.open") => "打开 Lan Mouse",
        (Language::En, "app.stop") => "Stop sharing",
        (Language::Zh, "app.stop") => "停止共享",
        (Language::En, "app.quit") => "Quit Lan Mouse",
        (Language::Zh, "app.quit") => "退出 Lan Mouse",
        (Language::En, "app.close") => "Close window",
        (Language::Zh, "app.close") => "关闭窗口",
        (_, "app.title") => "Lan Mouse",
        (Language::En, "clipboard.title") => "Clipboard",
        (Language::Zh, "clipboard.title") => "剪贴板",
        (Language::En, "clipboard.paused") => "Paused with sharing",
        (Language::Zh, "clipboard.paused") => "已随共享暂停",
        (Language::En, "clipboard.waiting") => "Waiting for a new copy",
        (Language::Zh, "clipboard.waiting") => "等待新的复制",
        (Language::En, "clipboard.syncing") => "Syncing to the current target",
        (Language::Zh, "clipboard.syncing") => "正在同步到当前目标",
        (Language::En, "clipboard.ready") => "Ready on the current target",
        (Language::Zh, "clipboard.ready") => "当前目标已准备好",
        (Language::En, "clipboard.unavailable") => "Latest copy is unavailable",
        (Language::Zh, "clipboard.unavailable") => "最新复制内容暂不可用",
        (Language::En, "clipboard.unsupported") => {
            "Unsupported copy (text: 128 KiB; one PNG/JPEG/GIF image: 16 MiB)"
        }
        (Language::Zh, "clipboard.unsupported") => {
            "不支持本次复制（文字 128 KiB；单张 PNG/JPEG/GIF 16 MiB）"
        }
        (Language::En, "clipboard.failed") => "Clipboard access failed; retrying",
        (Language::Zh, "clipboard.failed") => "剪贴板访问失败，正在重试",
        (Language::En, "sharing.current_target") => "Current control target",
        (Language::Zh, "sharing.current_target") => "当前控制目标",
        (Language::En, "sharing.stop") => "Stop sharing",
        (Language::Zh, "sharing.stop") => "停止共享",
        (Language::En, "sharing.primary") => "Primary device",
        (Language::Zh, "sharing.primary") => "主设备",
        (Language::En, "sharing.start") => "Use this device",
        (Language::Zh, "sharing.start") => "使用本机主控",
        (Language::En, "sharing.waiting") => {
            "Waiting for all devices to connect with the same authorized group"
        }
        (Language::Zh, "sharing.waiting") => "等待所有设备连接，并确认授权成员一致",
        (Language::En, "sharing.stopping") => "Waiting for every device to confirm input release",
        (Language::Zh, "sharing.stopping") => "等待所有设备确认已释放输入",
        (Language::En, "sharing.starting") => "Preparing the input session",
        (Language::Zh, "sharing.starting") => "正在准备共享会话",
        (Language::En, "sharing.stopped") => "Sharing stopped; select the primary on that device",
        (Language::Zh, "sharing.stopped") => "共享已停止，请在新的主设备上选择主控",
        (Language::En, "sharing.running") => "Sharing from",
        (Language::Zh, "sharing.running") => "当前主设备",
        (Language::En, "app.subtitle") => {
            "easily use your mouse and keyboard on multiple computers"
        }
        (Language::Zh, "app.subtitle") => "在多台电脑之间共用一套键鼠",
        (Language::En, "group.status") => "Capture / Emulation Status",
        (Language::Zh, "group.status") => "采集与模拟状态",
        (Language::En, "group.general") => "General",
        (Language::Zh, "group.general") => "通用",
        (Language::En, "group.connections") => "Connections",
        (Language::Zh, "group.connections") => "设备连接",
        (Language::En, "group.incoming") => "Incoming Connections",
        (Language::Zh, "group.incoming") => "已授权设备",
        (Language::En, "group.data") => "Direct Links",
        (Language::Zh, "group.data") => "直连会话",
        (Language::En, "group.preferences") => "Preferences",
        (Language::Zh, "group.preferences") => "偏好设置",
        (Language::En, "status.capture_off") => "input capture is disabled",
        (Language::Zh, "status.capture_off") => "输入采集已停用",
        (Language::En, "status.capture_off_subtitle") => {
            "required for outgoing and incoming connections"
        }
        (Language::Zh, "status.capture_off_subtitle") => "连接其他设备时必需",
        (Language::En, "status.emulation_off") => "input emulation is disabled",
        (Language::Zh, "status.emulation_off") => "输入模拟已停用",
        (Language::En, "status.emulation_off_subtitle") => "required for incoming connections",
        (Language::Zh, "status.emulation_off_subtitle") => "接收其他设备输入时必需",
        (Language::En, "status.macos_relaunch") => "relaunch required",
        (Language::Zh, "status.macos_relaunch") => "需要重新启动",
        (Language::En, "status.macos_relaunch_subtitle") => {
            "Accessibility granted — restart to activate capture and emulation"
        }
        (Language::Zh, "status.macos_relaunch_subtitle") => {
            "已授予辅助功能权限，重启后启用采集与模拟"
        }
        (Language::En, "status.macos_grant_subtitle") => "grant Accessibility permission to enable",
        (Language::Zh, "status.macos_grant_subtitle") => "授予辅助功能权限以启用",
        (Language::En, "button.reenable") => "Reenable",
        (Language::Zh, "button.reenable") => "重新启用",
        (Language::En, "button.grant") => "Grant",
        (Language::Zh, "button.grant") => "授予权限",
        (Language::En, "button.relaunch") => "Relaunch",
        (Language::Zh, "button.relaunch") => "重新启动",
        (Language::En, "button.add") => "Manual setup",
        (Language::Zh, "button.add") => "手动配置",
        (Language::En, "button.authorize") => "Authorize",
        (Language::Zh, "button.authorize") => "授权设备",
        (Language::En, "general.hostname_port") => "Hostname / port",
        (Language::Zh, "general.hostname_port") => "主机名 / 端口",
        (Language::En, "general.fingerprint") => "certificate fingerprint",
        (Language::Zh, "general.fingerprint") => "证书指纹",
        (Language::En, "general.data_port") => "data port",
        (Language::Zh, "general.data_port") => "数据端口",
        (Language::En, "placeholder.no_clients") => "No connections!",
        (Language::Zh, "placeholder.no_clients") => "暂无设备连接",
        (Language::En, "placeholder.no_clients_subtitle") => "Select the main device above to join",
        (Language::Zh, "placeholder.no_clients_subtitle") => {
            "在上方选择附近的主设备或输入其 IP 加入，设备会自动出现在这里"
        }
        (Language::En, "placeholder.no_authorized") => "no devices registered!",
        (Language::Zh, "placeholder.no_authorized") => "尚无已授权设备",
        (Language::En, "placeholder.no_authorized_subtitle") => {
            "Confirm on both devices when joining by IP"
        }
        (Language::Zh, "placeholder.no_authorized_subtitle") => {
            "通过 IP 加入时，在两端确认后自动授权"
        }
        (Language::En, "placeholder.no_data") => "no direct links",
        (Language::Zh, "placeholder.no_data") => "暂无直连会话",
        (Language::En, "placeholder.no_data_subtitle") => {
            "links open when a device sends files or messages"
        }
        (Language::Zh, "placeholder.no_data_subtitle") => "设备发送文件或消息时会建立直连",
        (Language::En, "layout.title") => "Screen arrangement",
        (Language::Zh, "layout.title") => "屏幕布局",
        (Language::En, "layout.hint") => {
            "Drag a device card onto a side. The layout is saved automatically."
        }
        (Language::Zh, "layout.hint") => "将设备卡片拖到某一侧，布局会自动保存。",
        (Language::En, "layout.this_device") => "This device",
        (Language::Zh, "layout.this_device") => "本机",
        (Language::En, "layout.left") => "Left",
        (Language::Zh, "layout.left") => "左侧",
        (Language::En, "layout.right") => "Right",
        (Language::Zh, "layout.right") => "右侧",
        (Language::En, "layout.top") => "Top",
        (Language::Zh, "layout.top") => "上方",
        (Language::En, "layout.bottom") => "Bottom",
        (Language::Zh, "layout.bottom") => "下方",
        (Language::En, "layout.none") => "No devices",
        (Language::Zh, "layout.none") => "无设备",
        (Language::En, "layout.drop") => "Drop here",
        (Language::Zh, "layout.drop") => "拖到此处",
        (Language::En, "home.controlling") => "KEYBOARD & MOUSE ON",
        (Language::Zh, "home.controlling") => "键鼠正在控制",
        (Language::En, "home.here") => "KEYBOARD & MOUSE ON",
        (Language::Zh, "home.here") => "键鼠在",
        (Language::En, "home.paused") => "SHARING STOPPED",
        (Language::Zh, "home.paused") => "共享已停止",
        (Language::En, "home.this_device") => "This device",
        (Language::Zh, "home.this_device") => "本机",
        (Language::En, "home.release_before") => "Press",
        (Language::Zh, "home.release_before") => "按",
        (Language::En, "home.release_after") => "to come back to this device",
        (Language::Zh, "home.release_after") => "立即回到本机",
        (Language::En, "home.devices") => "Devices",
        (Language::Zh, "home.devices") => "设备",
        (Language::En, "home.count") => "{} devices",
        (Language::Zh, "home.count") => "{} 台",
        (Language::En, "home.drag_hint") => "Drag a screen above to change its side",
        (Language::Zh, "home.drag_hint") => "拖动上面的屏幕可以调整左右位置",
        (Language::En, "home.add") => "Add device",
        (Language::Zh, "home.add") => "添加设备",
        (Language::En, "home.add_note") => "Nearby computers appear automatically",
        (Language::Zh, "home.add_note") => "附近的电脑会自动出现",
        (Language::En, "home.empty_eyebrow") => "NO OTHER DEVICES YET",
        (Language::Zh, "home.empty_eyebrow") => "还没有其他设备",
        (Language::En, "home.empty_title") => "Bring in a <b>second computer</b>",
        (Language::Zh, "home.empty_title") => "把<b>第二台电脑</b>\n接进来",
        (Language::En, "home.empty_body") => {
            "Open Sharemouse on both computers on the same network, then control both with this keyboard and mouse."
        }
        (Language::Zh, "home.empty_body") => {
            "两台电脑都打开 Sharemouse，连上同一个网络，就能用这一套键盘鼠标控制它们。"
        }
        (Language::En, "home.enable") => "Enable",
        (Language::Zh, "home.enable") => "启用",
        (Language::En, "home.disable") => "Disable",
        (Language::Zh, "home.disable") => "停用",
        (Language::En, "home.more") => "More actions",
        (Language::Zh, "home.more") => "更多操作",
        (Language::En, "add.title") => "Add device",
        (Language::Zh, "add.title") => "添加设备",
        (Language::En, "add.nearby") => "NEARBY COMPUTERS",
        (Language::Zh, "add.nearby") => "附近的电脑",
        (Language::En, "add.searching") => {
            "Still searching. The other computer needs “Let other computers connect to me” turned on."
        }
        (Language::Zh, "add.searching") => "还在查找。对方需要先打开“让其他电脑连接我”。",
        (Language::En, "add.by_ip") => "ENTER THE OTHER COMPUTER'S IP",
        (Language::Zh, "add.by_ip") => "输入对方的 IP",
        (Language::En, "add.ip_placeholder") => "e.g. 192.168.1.10",
        (Language::Zh, "add.ip_placeholder") => "例如 192.168.1.10",
        (Language::En, "add.host") => "LET OTHER COMPUTERS CONNECT TO ME",
        (Language::Zh, "add.host") => "让其他电脑连接我",
        (Language::En, "add.host_note") => {
            "When on, computers on the same network see this one under “Nearby computers”"
        }
        (Language::Zh, "add.host_note") => "打开后，同一网络里的电脑会在“附近的电脑”里看到本机",
        (Language::En, "add.close") => "Close",
        (Language::Zh, "add.close") => "关闭",
        (Language::En, "pref.palette") => "Colors",
        (Language::Zh, "pref.palette") => "配色",
        (Language::En, "palette.ember") => "Ember",
        (Language::Zh, "palette.ember") => "暖灰橙",
        (Language::En, "palette.pine") => "Pine",
        (Language::Zh, "palette.pine") => "松林",
        (Language::En, "settings.general") => "General",
        (Language::Zh, "settings.general") => "常规",
        (Language::En, "settings.advanced") => "Advanced",
        (Language::Zh, "settings.advanced") => "高级",
        (Language::En, "settings.advanced_note") => "For troubleshooting",
        (Language::Zh, "settings.advanced_note") => "排查问题时使用",
        (Language::En, "settings.manual") => "Manual device configuration",
        (Language::Zh, "settings.manual") => "手动配置设备",
        (Language::En, "settings.manual_note") => "Enter host name, port and side directly",
        (Language::Zh, "settings.manual_note") => "直接填写主机名、端口和位置",
        (Language::En, "files.drop_title") => "Drop files here to send to <b>{}</b>",
        (Language::Zh, "files.drop_title") => "把文件拖到这里，发给 <b>{}</b>",
        (Language::En, "files.drop_any") => "Drop files here to send them",
        (Language::Zh, "files.drop_any") => "把文件拖到这里发送",
        (Language::En, "files.drop_note") => {
            "Folders work too. You choose the receiving device after dropping."
        }
        (Language::Zh, "files.drop_note") => "文件夹也可以。放下后选择接收的设备。",
        (Language::En, "files.history") => "Transfers",
        (Language::Zh, "files.history") => "传输记录",
        (Language::En, "messages.to") => "SEND TO",
        (Language::Zh, "messages.to") => "发给",
        (Language::En, "lang.en") => "English",
        (Language::Zh, "lang.en") => "English",
        (Language::En, "lang.zh") => "中文",
        (Language::Zh, "lang.zh") => "中文",
        (Language::En, "pref.language") => "Language",
        (Language::Zh, "pref.language") => "语言",
        (Language::En, "pref.theme") => "Theme",
        (Language::Zh, "pref.theme") => "主题",
        (Language::En, "theme.system") => "System",
        (Language::Zh, "theme.system") => "跟随系统",
        (Language::En, "theme.light") => "Light",
        (Language::Zh, "theme.light") => "浅色",
        (Language::En, "theme.dark") => "Dark",
        (Language::Zh, "theme.dark") => "深色",
        (Language::En, "transfer.conflict_title") => "Files already exist",
        (Language::Zh, "transfer.conflict_title") => "存在同名文件",
        (Language::En, "transfer.conflict_body") => {
            "Some names in this transfer already exist in the receive folder."
        }
        (Language::Zh, "transfer.conflict_body") => "本次传输的部分名称在接收目录中已存在。",
        (Language::En, "transfer.conflict_overwrite") => "Overwrite",
        (Language::Zh, "transfer.conflict_overwrite") => "覆盖",
        (Language::En, "transfer.conflict_skip") => "Skip",
        (Language::Zh, "transfer.conflict_skip") => "跳过",
        (Language::En, "transfer.conflict_rename") => "Keep both",
        (Language::Zh, "transfer.conflict_rename") => "保留两者",
        (Language::En, "transfer.conflict_cancel") => "Cancel",
        (Language::Zh, "transfer.conflict_cancel") => "取消",
        (Language::En, "transfer.complete") => "received from",
        (Language::Zh, "transfer.complete") => "接收自",
        (Language::En, "transfer.complete_files") => "file(s)",
        (Language::Zh, "transfer.complete_files") => "个文件",
        (Language::En, "group.transfers") => "Transfers",
        (Language::Zh, "group.transfers") => "文件传输",
        (Language::En, "transfer.receive_dir") => "Save received files to",
        (Language::Zh, "transfer.receive_dir") => "接收目录",
        (Language::En, "transfer.receive_dir_change") => "Change",
        (Language::Zh, "transfer.receive_dir_change") => "更改",
        (Language::En, "transfer.receive_dir_choose") => "Choose receive folder",
        (Language::Zh, "transfer.receive_dir_choose") => "选择接收目录",
        (Language::En, "transfer.placeholder") => "no transfers yet",
        (Language::Zh, "transfer.placeholder") => "暂无传输",
        (Language::En, "transfer.placeholder_subtitle") => {
            "drag files onto this window or use \"Send files\" on a device"
        }
        (Language::Zh, "transfer.placeholder_subtitle") => {
            "把文件拖到窗口内，或在设备上点“发送文件”"
        }
        (Language::En, "transfer.sending") => "sending",
        (Language::Zh, "transfer.sending") => "正在发送",
        (Language::En, "transfer.sent") => "sent",
        (Language::Zh, "transfer.sent") => "已发送",
        (Language::En, "transfer.failed") => "failed",
        (Language::Zh, "transfer.failed") => "失败",
        (Language::En, "transfer.received") => "received",
        (Language::Zh, "transfer.received") => "已接收",
        (Language::En, "transfer.retry") => "Send again",
        (Language::Zh, "transfer.retry") => "重新发送",
        (Language::En, "transfer.dismiss") => "Dismiss",
        (Language::Zh, "transfer.dismiss") => "关闭",
        (Language::En, "transfer.files") => "file(s)",
        (Language::Zh, "transfer.files") => "个文件",
        (Language::En, "transfer.no_device") => "choose a device to send to first",
        (Language::Zh, "transfer.no_device") => "请先选择要发送到的设备",
        (Language::En, "transfer.dropped") => "Send dropped items to",
        (Language::Zh, "transfer.dropped") => "发送拖入的文件到",
        (Language::En, "transfer.drop_empty") => "no files in that drop",
        (Language::Zh, "transfer.drop_empty") => "拖入的内容里没有文件",
        (Language::En, "transfer.item_count") => "{n} items selected",
        (Language::Zh, "transfer.item_count") => "已选择 {n} 项",
        (Language::En, "transfer.dir_empty") => "receive directory cannot be empty",
        (Language::Zh, "transfer.dir_empty") => "接收目录不能为空",
        (Language::En, "transfer.dir_relative") => "receive directory must be an absolute path",
        (Language::Zh, "transfer.dir_relative") => "接收目录必须是绝对路径",
        (Language::En, "client.send_files") => "send files or folders",
        (Language::Zh, "client.send_files") => "发送文件或文件夹",
        (Language::En, "group.messages") => "Messages",
        (Language::Zh, "group.messages") => "消息",
        (Language::En, "message.send_to") => "Send to",
        (Language::Zh, "message.send_to") => "发送到",
        (Language::En, "message.record_hint") => {
            "kept for this run only; never written to the clipboard"
        }
        (Language::Zh, "message.record_hint") => "仅在本次运行保留，不会写入剪贴板",
        (Language::En, "message.no_device") => "no devices yet; add one to send a message",
        (Language::Zh, "message.no_device") => "还没有设备；添加一台后才能发送消息",
        (Language::En, "message.placeholder") => "type a message or paste a link",
        (Language::Zh, "message.placeholder") => "输入消息或粘贴链接",
        (Language::En, "message.send") => "Send message",
        (Language::Zh, "message.send") => "发送消息",
        (Language::En, "message.clear") => "Clear record",
        (Language::Zh, "message.clear") => "清空消息记录",
        (Language::En, "message.sent") => "sent",
        (Language::Zh, "message.sent") => "已发送",
        (Language::En, "message.received") => "received",
        (Language::Zh, "message.received") => "已收到",
        (Language::En, "message.open_link") => "Open link",
        (Language::Zh, "message.open_link") => "打开链接",
        (Language::En, "message.too_long") => "message is longer than the {} byte limit",
        (Language::Zh, "message.too_long") => "消息超过了 {} 字节的上限",
        (Language::En, "message.placeholder_title") => "no messages yet",
        (Language::Zh, "message.placeholder_title") => "暂无消息",
        (Language::En, "message.placeholder_subtitle") => {
            "messages sent and received in this run appear here"
        }
        (Language::Zh, "message.placeholder_subtitle") => "本次运行中收发的消息会显示在这里",
        (Language::En, "status.active") => "Active",
        (Language::Zh, "status.active") => "控制中",
        (Language::En, "status.online") => "Online",
        (Language::Zh, "status.online") => "在线",
        (Language::En, "status.offline") => "Offline",
        (Language::Zh, "status.offline") => "离线",
        (Language::En, "data.linked") => "Linked",
        (Language::Zh, "data.linked") => "已直连",
        (Language::En, "client.no_hostname") => "no hostname",
        (Language::Zh, "client.no_hostname") => "未命名设备",
        (Language::En, "client.unnamed") => "Unnamed device",
        (Language::Zh, "client.unnamed") => "未命名设备",
        (Language::En, "client.position") => "position",
        (Language::Zh, "client.position") => "位置",
        (Language::En, "client.hostname") => "hostname",
        (Language::Zh, "client.hostname") => "主机名",
        (Language::En, "client.port") => "port",
        (Language::Zh, "client.port") => "端口",
        (Language::En, "client.delete") => "delete this device",
        (Language::Zh, "client.delete") => "删除此设备",
        (Language::En, "client.resolve") => "resolve host",
        (Language::Zh, "client.resolve") => "解析主机",
        (Language::En, "client.enable") => "enable",
        (Language::Zh, "client.enable") => "启用",
        (Language::En, "client.status") => "status",
        (Language::Zh, "client.status") => "状态",
        (Language::En, "version.peer") => "Peer version",
        (Language::Zh, "version.peer") => "对方版本",
        (Language::En, "version.ours") => "Ours",
        (Language::Zh, "version.ours") => "本机",
        (Language::En, "version.matched") => "matched",
        (Language::Zh, "version.matched") => "一致",
        (Language::En, "version.unknown") => "unknown",
        (Language::Zh, "version.unknown") => "未知",
        (Language::En, "auth.title") => "Unauthorized Device",
        (Language::Zh, "auth.title") => "未授权设备",
        (Language::En, "auth.message") => {
            "An unauthorized device is trying to connect. Do you want to authorize it?"
        }
        (Language::Zh, "auth.message") => "有未授权设备正在连接，是否授权？",
        (Language::En, "auth.fingerprint") => "sha256 fingerprint",
        (Language::Zh, "auth.fingerprint") => "SHA-256 指纹",
        (Language::En, "auth.cancel") => "Cancel",
        (Language::Zh, "auth.cancel") => "取消",
        (Language::En, "auth.confirm") => "Authorize",
        (Language::Zh, "auth.confirm") => "授权",
        (Language::En, "fingerprint.title") => "Add Certificate Fingerprint",
        (Language::Zh, "fingerprint.title") => "添加证书指纹",
        (Language::En, "fingerprint.intro") => {
            "The certificate fingerprint uniquely identifies your device."
        }
        (Language::Zh, "fingerprint.intro") => "证书指纹用于唯一标识你的设备。",
        (Language::En, "fingerprint.hint") => {
            "You can find it under the \"General\" section of the device you want to connect."
        }
        (Language::Zh, "fingerprint.hint") => "可在目标设备的“通用”部分找到。",
        (Language::En, "fingerprint.description") => "description",
        (Language::Zh, "fingerprint.description") => "描述",
        (Language::En, "fingerprint.fingerprint") => "sha256 fingerprint",
        (Language::Zh, "fingerprint.fingerprint") => "SHA-256 指纹",
        (Language::En, "fingerprint.confirm") => "Confirm",
        (Language::Zh, "fingerprint.confirm") => "确认",
        _ => "?",
    }
}

pub fn position_label(language: Language, position: Position) -> &'static str {
    tr(
        language,
        match position {
            Position::Left => "layout.left",
            Position::Right => "layout.right",
            Position::Top => "layout.top",
            Position::Bottom => "layout.bottom",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_round_trip_and_accept_locales() {
        for language in [Language::En, Language::Zh] {
            assert_eq!(Language::from_code(language.code()), Some(language));
        }
        assert_eq!(Language::from_code("zh_CN.UTF-8"), Some(Language::Zh));
        assert_eq!(Language::from_code("en_US"), Some(Language::En));
        assert_eq!(Language::from_code("de_DE"), None);
    }

    #[test]
    fn theme_codes_round_trip() {
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            assert_eq!(Theme::from_code(theme.code()), Some(theme));
        }
        assert_eq!(Theme::from_code("sepia"), None);
    }

    #[test]
    fn preferences_survive_key_file_round_trip() {
        let preferences = UiPreferences {
            autostart: false,
            language: Language::Zh,
            theme: Theme::Dark,
            palette: Palette::Pine,
        };
        let restored = UiPreferences::from_key_file(&preferences.to_key_file());
        assert_eq!(restored, preferences);
    }

    #[test]
    fn invalid_preferences_fall_back_to_theme_system() {
        let key_file = glib::KeyFile::new();
        key_file.set_string("ui", "language", "de");
        key_file.set_string("ui", "theme", "sepia");
        key_file.set_string("ui", "palette", "neon");
        let preferences = UiPreferences::from_key_file(&key_file);
        assert!(preferences.autostart);
        assert_eq!(preferences.theme, Theme::System);
        assert_eq!(preferences.palette, Palette::Ember);
        assert!(matches!(preferences.language, Language::En | Language::Zh));
    }

    #[test]
    fn translations_cover_both_languages() {
        for key in [
            "app.title",
            "conversation.devices",
            "conversation.files",
            "clipboard.title",
            "conversation.settings",
            "conversation.peers",
            "conversation.add_device",
            "conversation.send_files",
            "conversation.send_folder",
            "conversation.files_hint",
            "group.messages",
            "conversation.conversation",
            "conversation.send",
            "conversation.image",
            "conversation.clear",
            "conversation.clear_hint",
            "enrollment.title",
            "enrollment.help",
            "enrollment.nearby",
            "enrollment.searching",
            "enrollment.connect",
            "enrollment.host",
            "enrollment.ip_hint",
            "enrollment.no_ip",
            "enrollment.setup_hint",
            "enrollment.join",
            "enrollment.stop",
            "enrollment.address",
            "enrollment.idle",
            "enrollment.hosting",
            "enrollment.joining",
            "enrollment.joined",
            "enrollment.failed",
            "enrollment.stopped",
            "enrollment.approve",
            "transfer.conflict_cancel",
            "enrollment.confirm",
            "enrollment.verify",
            "enrollment.local",
            "enrollment.remote",
            "group.connections",
            "layout.hint",
            "theme.dark",
            "auth.message",
            "fingerprint.title",
            "group.messages",
            "message.send_to",
            "message.record_hint",
            "message.no_device",
            "message.placeholder",
            "message.send",
            "message.clear",
            "message.sent",
            "message.received",
            "message.open_link",
            "message.too_long",
            "message.placeholder_title",
            "message.placeholder_subtitle",
            "home.controlling",
            "home.here",
            "home.paused",
            "home.this_device",
            "home.release_before",
            "home.release_after",
            "home.devices",
            "home.count",
            "home.drag_hint",
            "home.add",
            "home.add_note",
            "home.empty_eyebrow",
            "home.empty_title",
            "home.empty_body",
            "home.more",
            "home.enable",
            "home.disable",
            "add.title",
            "add.nearby",
            "add.searching",
            "add.by_ip",
            "add.ip_placeholder",
            "add.host",
            "add.host_note",
            "add.close",
            "pref.palette",
            "palette.ember",
            "palette.pine",
            "settings.general",
            "settings.advanced",
            "settings.advanced_note",
            "settings.manual",
            "settings.manual_note",
            "files.drop_title",
            "files.drop_any",
            "files.drop_note",
            "files.history",
            "messages.to",
        ] {
            assert_ne!(tr(Language::En, key), "?", "missing English key {key}");
            assert_ne!(tr(Language::Zh, key), "?", "missing Chinese key {key}");
        }
        assert_eq!(tr(Language::En, "layout.left"), "Left");
        assert_eq!(tr(Language::Zh, "layout.left"), "左侧");
    }

    #[test]
    fn templated_strings_keep_one_placeholder() {
        for key in ["home.count", "files.drop_title"] {
            for language in [Language::En, Language::Zh] {
                assert_eq!(
                    tr(language, key).matches("{}").count(),
                    1,
                    "{language:?} {key}"
                );
            }
        }
    }

    /// A placeholder that lost its `{}` would show a byte limit
    /// nowhere, and a translation that kept the wrong one would show
    /// the number twice.
    #[test]
    fn the_message_length_warning_keeps_its_placeholder() {
        for language in [Language::En, Language::Zh] {
            let text = tr(language, "message.too_long");
            assert_eq!(
                text.matches("{}").count(),
                1,
                "{language:?}: {text} needs exactly one placeholder"
            );
        }
    }

    #[test]
    fn positions_map_to_direction_labels() {
        assert_eq!(position_label(Language::En, Position::Top), "Top");
        assert_eq!(position_label(Language::Zh, Position::Bottom), "下方");
    }

    /// The combo row reports its selection as an index, and the template
    /// callback converts it back. A mismatch silently changes the wrong
    /// setting, so pin both directions here.
    #[test]
    fn language_index_matches_selector_order() {
        assert_eq!(Language::En.index(), 0);
        assert_eq!(Language::Zh.index(), 1);
        for language in [Language::En, Language::Zh] {
            assert_eq!(Language::from_index(language.index()), language);
        }
        // Out-of-range selections fall back to the first entry rather
        // than panicking in the template callback.
        assert_eq!(Language::from_index(7), Language::En);
    }

    #[test]
    fn theme_index_matches_selector_order() {
        assert_eq!(Theme::System.index(), 0);
        assert_eq!(Theme::Light.index(), 1);
        assert_eq!(Theme::Dark.index(), 2);
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            assert_eq!(Theme::from_index(theme.index()), theme);
        }
        assert_eq!(Theme::from_index(9), Theme::System);
    }

    #[test]
    fn palette_codes_and_indices_round_trip() {
        for palette in [Palette::Ember, Palette::Pine] {
            assert_eq!(Palette::from_code(palette.code()), Some(palette));
            assert_eq!(Palette::from_index(palette.index()), palette);
        }
        assert_eq!(Palette::Ember.index(), 0);
        assert_eq!(Palette::Pine.index(), 1);
        assert_eq!(Palette::from_code("brass"), None);
        assert_eq!(Palette::from_index(5), Palette::Ember);
    }
}
