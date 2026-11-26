//! The system's light/dark preference on Linux. winit reports it on macOS and
//! Windows (and egui follows it), but not on Linux, where desktops publish it
//! through the XDG desktop portal (`org.freedesktop.appearance color-scheme`).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Once;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

/// 0 = unknown, 1 = dark, 2 = light.
static SCHEME: AtomicU8 = AtomicU8::new(0);
static WATCH: Once = Once::new();

/// `Some(true)` when the desktop prefers dark, `None` when it does not say.
pub fn dark() -> Option<bool> {
    match SCHEME.load(Ordering::Relaxed) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Read the preference now and follow its changes; `changed` runs after each one.
pub fn watch(changed: impl Fn() + Send + 'static) {
    WATCH.call_once(|| {
        let _ = std::thread::Builder::new().name("system-theme".into()).spawn(move || {
            if let Err(e) = run(changed) {
                eprintln!("copym: system theme: {e}");
            }
        });
    });
}

fn run(changed: impl Fn()) -> zbus::Result<()> {
    let conn = Connection::session()?;
    let settings = Proxy::new(&conn, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings")?;
    let current: zbus::Result<OwnedValue> = settings.call("ReadOne", &("org.freedesktop.appearance", "color-scheme"));
    // older portals only have Read (the value comes wrapped once more)
    let current = current.or_else(|_| settings.call::<_, _, OwnedValue>("Read", &("org.freedesktop.appearance", "color-scheme")))?;
    store(&current);
    changed();
    for msg in settings.receive_signal("SettingChanged")? {
        let Ok((namespace, key, value)) = msg.body().deserialize::<(String, String, OwnedValue)>() else { continue };
        if namespace == "org.freedesktop.appearance" && key == "color-scheme" {
            store(&value);
            changed();
        }
    }
    Ok(())
}

fn store(v: &Value) {
    fn scheme(v: &Value) -> Option<u32> {
        match v {
            Value::U32(n) => Some(*n),
            Value::Value(inner) => scheme(inner),
            _ => None,
        }
    }
    // 1 = prefer dark, 2 = prefer light, 0 = no preference (light, as the spec suggests)
    let s = match scheme(v) {
        Some(1) => 1,
        Some(_) => 2,
        None => 0,
    };
    SCHEME.store(s, Ordering::Relaxed);
}
