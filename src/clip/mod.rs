//! Platform clipboard backends.
//!
//! Every backend:
//! * watches the system clipboard with the cheapest native mechanism available,
//! * drops content flagged as secret by password managers before it is even read,
//! * sends accepted content to the daemon through a channel,
//! * provides a [`ClipWriter`] to put history items back on the clipboard.

use crate::model::Content;
use std::sync::mpsc::Sender;

#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
mod arboard_io;

pub struct ClipEvent {
    pub content: Content,
    /// Name of the application that owns the clipboard, when the platform can tell.
    pub source: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MonitorOptions {
    pub respect_password_hints: bool,
    pub store_images: bool,
    pub max_text_bytes: usize,
    pub max_image_bytes: usize,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub restore_empty_clipboard: bool,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub poll_interval_ms: u64,
}

pub trait ClipWriter: Send {
    fn set(&mut self, content: &Content) -> Result<(), String>;
}

pub struct Monitor {
    pub backend: &'static str,
    /// Whether an in-process global hotkey can work with this backend.
    pub supports_hotkey: bool,
    pub writer: Box<dyn ClipWriter>,
    /// Blocking event loop; must be run on the main thread.
    pub run: Box<dyn FnOnce() -> Result<(), String>>,
}

/// Mime types / formats password managers use to mark secrets on Linux.
#[cfg(target_os = "linux")]
pub const KDE_PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

struct NullWriter;

impl ClipWriter for NullWriter {
    fn set(&mut self, _: &Content) -> Result<(), String> {
        Ok(())
    }
}

/// A monitor that watches nothing (sync-only test instances).
pub fn null_monitor(tx: Sender<ClipEvent>) -> Monitor {
    Monitor {
        backend: "none",
        supports_hotkey: false,
        writer: Box::new(NullWriter),
        run: Box::new(move || {
            let _keep = tx;
            loop {
                std::thread::park();
            }
        }),
    }
}

pub fn start(tx: Sender<ClipEvent>, opts: MonitorOptions) -> Result<Monitor, String> {
    #[cfg(target_os = "linux")]
    {
        let mut errors = Vec::new();
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match wayland::start(tx.clone(), opts.clone()) {
                Ok(m) => return Ok(m),
                Err(e) => errors.push(format!("wayland: {e}")),
            }
        }
        if std::env::var_os("DISPLAY").is_some() {
            match x11::start(tx, opts) {
                Ok(m) => return Ok(m),
                Err(e) => errors.push(format!("x11: {e}")),
            }
        }
        if errors.is_empty() {
            errors.push("neither WAYLAND_DISPLAY nor DISPLAY is set".into());
        }
        Err(errors.join("; "))
    }
    #[cfg(target_os = "windows")]
    {
        windows::start(tx, opts)
    }
    #[cfg(target_os = "macos")]
    {
        macos::start(tx, opts)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (tx, opts);
        Err("unsupported platform".into())
    }
}
