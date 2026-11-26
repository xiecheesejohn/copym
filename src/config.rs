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
    /// "auto" (system language), "zh" or "en".
    pub language: String,
    /// UI font file (.ttf / .otf / .ttc; "~/" allowed). Empty = the bundled
    /// Maple Mono NF CN (it stays as fallback for a custom font).
    pub font: String,
    /// Recognize text in images so they can be searched.
    pub ocr_enabled: bool,
    /// "auto" (native engine on Windows/macOS, built-in PP-OCR on Linux, else Tesseract),
    /// "native", "ppocr" (Linux), "tesseract" or "off".
    pub ocr_engine: String,
    /// Tesseract languages, e.g. "chi_sim+chi_tra+eng" (missing packs are skipped).
    pub ocr_languages: String,
    /// Look for a newer release once a day (GitHub Releases; nothing is installed automatically).
    pub check_updates: bool,
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
    /// Format of the file; older files are upgraded once on load (missing = 0).
    #[serde(default)]
    pub config_version: u32,
}

/// Current `config_version`.
const CONFIG_VERSION: u32 = 2;
/// Default `max_items` up to config version 1 (written into every file then).
const OLD_DEFAULT_MAX_ITEMS: usize = 500;

impl Default for Config {
    fn default() -> Self {
        Self {
            max_items: 100_000,
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
            language: "auto".into(),
            font: String::new(),
            ocr_enabled: true,
            ocr_engine: "auto".into(),
            ocr_languages: "chi_sim+chi_tra+eng".into(),
            check_updates: true,
            sync_enabled: true,
            device_name: String::new(),
            sync_port: 47800,
            sync_images: true,
            sync_max_image_bytes: 8 * 1024 * 1024,
            sync_history_items: 200,
            sync_peers: Vec::new(),
            config_version: CONFIG_VERSION,
        }
    }
}

const TEMPLATE_HEADER: &str = "# copym configuration (also editable in the popup: Ctrl+, or :settings).\n\
# The commented lines show the defaults: remove the # and change the value to\n\
# override one. Lines left commented follow the defaults of newer versions.\n\
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
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(mut cfg) => {
                    let changes = cfg.upgrade();
                    if !changes.is_empty() {
                        if let Err(e) = save_values(&changes.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>()) {
                            eprintln!("copym: {e}");
                        }
                    }
                    cfg
                }
                Err(e) => {
                    eprintln!("copym: invalid {}: {e}; using defaults", path.display());
                    Config::default()
                }
            },
            Err(_) => {
                let cfg = Config::default();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                // only the version is set; the defaults are listed as comments,
                // so later versions can change them
                if let Ok(body) = toml::to_string_pretty(&cfg) {
                    let commented: String = body
                        .lines()
                        .filter(|l| !l.starts_with("config_version"))
                        .map(|l| if l.trim().is_empty() { "\n".to_string() } else { format!("# {l}\n") })
                        .collect();
                    let _ = std::fs::write(&path, format!("{TEMPLATE_HEADER}config_version = {CONFIG_VERSION}\n\n{commented}"));
                }
                cfg
            }
        }
    }
}

/// Write `ignore_apps` into config.toml, keeping the rest of the file (comments
/// and formatting) as it is.
pub fn save_ignore_apps(apps: &[String]) -> Result<(), String> {
    save_values(&[("ignore_apps", list_value(apps))])
}

/// A TOML array of strings.
pub fn list_value(items: &[String]) -> toml_edit::Value {
    toml_edit::Value::Array(items.iter().map(String::as_str).collect())
}

/// Write these keys into config.toml, keeping the rest of the file (comments
/// and formatting) as it is.
pub fn save_values(values: &[(&str, toml_edit::Value)]) -> Result<(), String> {
    let path = config_path();
    let text = with_values(&std::fs::read_to_string(&path).unwrap_or_default(), values)?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, text).map_err(|e| tf!("保存 {} 失败：{e}", "Could not save {}: {e}", path.display()))
}

fn with_values(text: &str, values: &[(&str, toml_edit::Value)]) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| tf!("config.toml 格式有误，未保存：{e}", "config.toml is invalid, nothing saved: {e}"))?;
    for (key, value) in values {
        doc[key] = toml_edit::Item::Value(value.clone());
    }
    Ok(doc.to_string())
}

/// Settings the daemon only reads at start.
pub const RESTART_KEYS: &[&str] = &[
    "hotkey",
    "keep_popup_loaded",
    "poll_interval_ms",
    "restore_empty_clipboard",
    "respect_password_hints",
    "store_images",
    "max_items",
    "max_text_bytes",
    "max_image_bytes",
    "ocr_enabled",
    "ocr_engine",
    "ocr_languages",
    "check_updates",
    "sync_enabled",
    "device_name",
    "sync_port",
    "sync_images",
    "sync_max_image_bytes",
    "sync_history_items",
    "sync_peers",
];

impl Config {
    /// Every setting as a TOML value, by key.
    /// Bring a file written by an older version up to date. Returns the keys
    /// to write back (empty when nothing changed).
    fn upgrade(&mut self) -> Vec<(&'static str, toml_edit::Value)> {
        if self.config_version >= CONFIG_VERSION {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Version 1 wrote every default into the file, so the old limit of 500
        // stuck even after the default became 100 000.
        if self.max_items == OLD_DEFAULT_MAX_ITEMS {
            self.max_items = Config::default().max_items;
            out.push(("max_items", toml_edit::Value::from(self.max_items as i64)));
        }
        self.config_version = CONFIG_VERSION;
        out.push(("config_version", toml_edit::Value::from(CONFIG_VERSION as i64)));
        out
    }

    pub fn values(&self) -> Vec<(String, toml_edit::Value)> {
        let text = toml::to_string(self).unwrap_or_default();
        let doc: toml_edit::DocumentMut = text.parse().unwrap_or_default();
        doc.iter().filter_map(|(k, v)| v.as_value().map(|v| (k.to_string(), v.clone()))).collect()
    }

    /// Keys whose values differ from `other`.
    pub fn changed(&self, other: &Config) -> Vec<(String, toml_edit::Value)> {
        let theirs = other.values();
        self.values()
            .into_iter()
            .filter(|(k, v)| theirs.iter().find(|(k2, _)| k2 == k).is_none_or(|(_, v2)| v.to_string().trim() != v2.to_string().trim()))
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_apps_keeps_the_rest_of_the_file() {
        let before = "# my notes\ntheme = \"dark\" # inline\nignore_apps = [\"a\"]\n";
        let after = with_values(before, &[("ignore_apps", list_value(&["微信".into(), "chrome.exe".into()]))]).unwrap();
        assert!(after.contains("# my notes") && after.contains("# inline"), "{after}");
        let cfg: Config = toml::from_str(&after).unwrap();
        assert_eq!(cfg.ignore_apps, ["微信", "chrome.exe"]);
        assert_eq!(cfg.theme, "dark");
        // a new file gets the key too
        let cfg: Config = toml::from_str(&with_values("", &[("ignore_apps", list_value(&[]))]).unwrap()).unwrap();
        assert!(cfg.ignore_apps.is_empty());
        assert!(with_values("not = = toml", &[]).is_err());
    }

    #[test]
    fn upgrade_old_files_once() {
        // a version-1 file: every default written out, no version
        let mut old: Config = toml::from_str("max_items = 500\ntheme = \"dark\"").unwrap();
        assert_eq!(old.config_version, 0);
        let keys: Vec<&str> = old.upgrade().iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["max_items", "config_version"]);
        assert_eq!((old.max_items, old.theme.as_str()), (100_000, "dark"));
        // done once: 500 chosen afterwards stays
        let mut new: Config = toml::from_str("config_version = 2\nmax_items = 500").unwrap();
        assert!(new.upgrade().is_empty());
        assert_eq!(new.max_items, 500);
        // a custom limit survives the upgrade
        let mut custom: Config = toml::from_str("max_items = 2000").unwrap();
        assert_eq!(custom.upgrade().len(), 1);
        assert_eq!(custom.max_items, 2000);
    }

    #[test]
    fn changed_keys() {
        let a = Config::default();
        let mut b = a.clone();
        b.theme = "dark".into();
        b.max_items = 9;
        let keys: Vec<String> = b.changed(&a).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["max_items", "theme"]);
        assert!(a.changed(&a.clone()).is_empty());
    }
}
