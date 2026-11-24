//! User configuration (`config.toml`) and well-known paths.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Maximum number of non-pinned items kept in history.
    pub max_items: usize,
    /// Texts larger than this (bytes) are not recorded.
    pub max_text_bytes: usize,
    /// Images larger than this (PNG bytes) are not recorded.
    pub max_image_bytes: usize,
    pub store_images: bool,
    /// Global shortcut. Alt = Option on macOS. Works on Windows, macOS, X11 and KDE Wayland;
    /// on other Wayland desktops bind `copym show` in the system settings.
    pub hotkey: String,
    /// Skip content marked as secret by password managers
    /// (x-kde-passwordManagerHint, ExcludeClipboardContentFromMonitorProcessing,
    /// org.nspasteboard.ConcealedType, ...).
    pub respect_password_hints: bool,
    /// Source applications whose copies are ignored (case-insensitive substring of
    /// the process / app name). Works on Windows and macOS.
    pub ignore_apps: Vec<String>,
    /// Regular expressions; matching texts are not recorded.
    pub ignore_patterns: Vec<String>,
    /// Wayland: when the clipboard owner quits and the clipboard becomes empty,
    /// re-offer the latest item.
    pub restore_empty_clipboard: bool,
    /// macOS clipboard polling interval in milliseconds.
    pub poll_interval_ms: u64,
    /// Close the popup when it loses focus.
    pub close_on_focus_lost: bool,
    /// Keep the popup process loaded in the background so the hotkey opens it
    /// instantly (costs some memory while hidden). false = start it on demand.
    pub keep_popup_loaded: bool,
    /// Where the keyboard goes when the popup opens: "list" (navigate right
    /// away; press / or start typing in the search box with a click) or "search".
    pub popup_focus: String,
    /// Enter pastes the chosen item into the window that was active before the
    /// popup (Shift+Enter only copies). false = Enter only copies.
    pub paste_on_enter: bool,
    /// Keys sent to paste: "ctrl+v", "shift+insert", "ctrl+shift+v" ...
    /// (macOS: "cmd+v"). Empty = platform default.
    pub paste_shortcut: String,
    /// "system", "dark" or "light".
    pub theme: String,
    /// Recognize text in images so they can be searched.
    pub ocr_enabled: bool,
    /// "auto" (native engine on Windows/macOS, Tesseract elsewhere), "native", "tesseract" or "off".
    pub ocr_engine: String,
    /// Tesseract languages, e.g. "chi_sim+chi_tra+eng" (missing packs are skipped).
    pub ocr_languages: String,
    /// LAN sync (end-to-end encrypted). Inactive until paired with `copym sync pair`.
    pub sync_enabled: bool,
    /// Name shown on other devices (empty = host name).
    pub device_name: String,
    /// TCP port for sync connections (falls back to a random port if busy).
    pub sync_port: u16,
    pub sync_images: bool,
    /// Images larger than this are not synced.
    pub sync_max_image_bytes: usize,
    /// How many recent items a device offers when (re)connecting.
    pub sync_history_items: usize,
    /// Extra peers to connect to when mDNS is blocked, e.g. ["192.168.1.20:47800"].
    pub sync_peers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_items: 500,
            max_text_bytes: 4 * 1024 * 1024,
            max_image_bytes: 32 * 1024 * 1024,
            store_images: true,
            hotkey: "Alt+C".into(),
            respect_password_hints: true,
            ignore_apps: [
                "bitwarden",
                "keepass",
                "1password",
                "lastpass",
                "dashlane",
                "enpass",
                "keeper",
                "proton pass",
                "keychain access",
                "passwords",
            ]
            .map(String::from)
            .to_vec(),
            ignore_patterns: Vec::new(),
            restore_empty_clipboard: true,
            poll_interval_ms: 500,
            close_on_focus_lost: true,
            keep_popup_loaded: true,
            popup_focus: "list".into(),
            paste_on_enter: true,
            paste_shortcut: String::new(),
            theme: "system".into(),
            ocr_enabled: true,
            ocr_engine: "auto".into(),
            ocr_languages: "chi_sim+chi_tra+eng".into(),
            sync_enabled: true,
            device_name: String::new(),
            sync_port: 47800,
            sync_images: true,
            sync_max_image_bytes: 8 * 1024 * 1024,
            sync_history_items: 200,
            sync_peers: Vec::new(),
        }
    }
}

const TEMPLATE_HEADER: &str = "# copym configuration. Delete a line to fall back to its default.\n\
# ignore_patterns example: ignore_patterns = ['^\\d{6}$', '(?i)password']\n\n";

impl Config {
    pub fn device_name(&self) -> String {
        if !self.device_name.trim().is_empty() {
            return self.device_name.trim().to_string();
        }
        host_name().unwrap_or_else(|| "copym".into())
    }

    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("copym: invalid {}: {e}; using defaults", path.display());
                Config::default()
            }),
            Err(_) => {
                let cfg = Config::default();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Ok(body) = toml::to_string_pretty(&cfg) {
                    let _ = std::fs::write(&path, format!("{TEMPLATE_HEADER}{body}"));
                }
                cfg
            }
        }
    }
}

fn host_name() -> Option<String> {
    #[cfg(target_os = "linux")]
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok();
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(target_os = "macos")]
    let name = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    let name = std::env::var("HOSTNAME").ok();
    name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
}

/// `COPYM_HOME` (developer/testing aid) relocates all files, so a second
/// instance can run side by side.
fn home_override() -> Option<PathBuf> {
    std::env::var_os("COPYM_HOME").map(PathBuf::from)
}

pub fn is_secondary_instance() -> bool {
    home_override().is_some()
}

pub fn config_path() -> PathBuf {
    if let Some(h) = home_override() {
        return h.join("config.toml");
    }
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("copym")
        .join("config.toml")
}

pub fn data_dir() -> PathBuf {
    if let Some(h) = home_override() {
        let _ = std::fs::create_dir_all(&h);
        return h;
    }
    let d = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("copym");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Private per-user runtime directory (holds the IPC socket on Unix).
#[cfg_attr(windows, allow(dead_code))]
pub fn runtime_dir() -> PathBuf {
    if let Some(h) = home_override() {
        let _ = std::fs::create_dir_all(&h);
        return h;
    }
    let d = dirs::runtime_dir()
        .map(|d| d.join("copym"))
        .or_else(|| dirs::cache_dir().map(|d| d.join("copym")))
        .unwrap_or_else(|| std::env::temp_dir().join("copym"));
    let _ = std::fs::create_dir_all(&d);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    d
}

pub fn log_path() -> PathBuf {
    if let Some(h) = home_override() {
        return h.join("daemon.log");
    }
    let d = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("copym");
    let _ = std::fs::create_dir_all(&d);
    d.join("daemon.log")
}
