//! Global shortcut on KDE Plasma (Wayland *and* X11) through KGlobalAccel's
//! D-Bus API - the same mechanism native KDE apps use. Wayland gives regular
//! clients no way to grab keys, so this is how copym gets a real hotkey there.

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

const SERVICE: &str = "org.kde.kglobalaccel";
const COMPONENT: &str = "copym";

/// Register `spec` (e.g. "Alt+C") as `action` and call `on_press` whenever it is pressed.
pub fn register(action: &str, label: &str, spec: &str, on_press: impl Fn() + Send + 'static) -> Result<(), String> {
    let key = qt_key(spec).ok_or_else(|| format!("cannot parse hotkey {spec:?}"))?;
    let e = |err: zbus::Error| err.to_string();
    let conn = Connection::session().map_err(e)?;
    let accel = Proxy::new(&conn, SERVICE, "/kglobalaccel", "org.kde.KGlobalAccel").map_err(e)?;
    let id = vec![COMPONENT, action, "copym", label];
    accel.call::<_, _, ()>("doRegister", &(id.clone(),)).map_err(e)?;
    // flags: SetPresent (2) | NoAutoloading (4) -> config.toml is the source of truth.
    let active: Vec<i32> = accel.call("setShortcut", &(id, vec![key], 6u32)).map_err(e)?;
    if !active.contains(&key) {
        return Err(format!("{spec} is already used by another global shortcut"));
    }
    let path: OwnedObjectPath = accel.call("getComponent", &(COMPONENT,)).map_err(e)?;
    let component = Proxy::new(&conn, SERVICE, path, "org.kde.kglobalaccel.Component").map_err(e)?;
    let signals = component.receive_signal("globalShortcutPressed").map_err(e)?;
    let action = action.to_string();
    std::thread::Builder::new()
        .name("kde-hotkey".into())
        .spawn(move || {
            let _keep = (conn, component);
            for msg in signals {
                if let Ok((comp, a, _ts)) = msg.body().deserialize::<(String, String, i64)>() {
                    if comp == COMPONENT && action == a {
                        on_press();
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Convert "Alt+C" / "CmdOrCtrl+Shift+V" / "Super+F2" into a Qt key code.
fn qt_key(spec: &str) -> Option<i32> {
    const SHIFT: i32 = 0x0200_0000;
    const CTRL: i32 = 0x0400_0000;
    const ALT: i32 = 0x0800_0000;
    const META: i32 = 0x1000_0000;
    let mut mods = 0;
    let mut key = None;
    for part in spec.split('+').map(|p| p.trim().to_ascii_uppercase()) {
        match part.as_str() {
            "SHIFT" => mods |= SHIFT,
            "CTRL" | "CONTROL" | "CMDORCTRL" | "COMMANDORCONTROL" | "CMDORCONTROL" | "COMMANDORCTRL" => mods |= CTRL,
            "ALT" | "OPTION" => mods |= ALT,
            "SUPER" | "META" | "WIN" | "CMD" | "COMMAND" => mods |= META,
            k => {
                let k = k.strip_prefix("KEY").filter(|r| r.len() == 1).unwrap_or(k);
                let k = k.strip_prefix("DIGIT").unwrap_or(k);
                key = Some(match k {
                    "SPACE" => 0x20,
                    "ENTER" | "RETURN" => 0x0100_0004,
                    "TAB" => 0x0100_0001,
                    "INSERT" => 0x0100_0006,
                    s if s.len() == 1 && s.chars().all(|c| c.is_ascii_alphanumeric()) => s.as_bytes()[0] as i32,
                    s if s.starts_with('F') => {
                        let n: i32 = s[1..].parse().ok()?;
                        if !(1..=35).contains(&n) {
                            return None;
                        }
                        0x0100_0030 + n - 1
                    }
                    _ => return None,
                });
            }
        }
    }
    key.map(|k| k | mods)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse() {
        assert_eq!(super::qt_key("Alt+C"), Some(0x0800_0043));
        assert_eq!(super::qt_key("alt+KeyC"), Some(0x0800_0043));
        assert_eq!(super::qt_key("CmdOrCtrl+Shift+V"), Some(0x0600_0056));
        assert_eq!(super::qt_key("Super+F2"), Some(0x1100_0031));
        assert_eq!(super::qt_key("Alt+Foo"), None);
    }
}
