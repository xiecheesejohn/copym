//! Launch the daemon at login.

pub fn set(enabled: bool) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy().to_string();
    imp(enabled, &exe)
}

/// Whether copym is currently set to start at login.
pub fn is_enabled() -> bool {
    #[cfg(target_os = "linux")]
    return dirs::config_dir().is_some_and(|d| d.join("autostart/copym.desktop").exists());
    #[cfg(target_os = "macos")]
    return dirs::home_dir().is_some_and(|d| d.join("Library/LaunchAgents/io.github.copym.plist").exists());
    #[cfg(windows)]
    return std::process::Command::new("reg")
        .args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "copym"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    #[allow(unreachable_code)]
    false
}

#[cfg(target_os = "linux")]
fn imp(enabled: bool, exe: &str) -> Result<String, String> {
    let dir = dirs::config_dir().ok_or("no config dir")?.join("autostart");
    let path = dir.join("copym.desktop");
    if enabled {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let body = format!(
            "[Desktop Entry]\nType=Application\nName=copym\nComment=Clipboard manager\n\
             Exec=\"{exe}\" daemon\nIcon=edit-paste\nTerminal=false\nNoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\nX-KDE-autostart-phase=2\n"
        );
        std::fs::write(&path, body).map_err(|e| e.to_string())?;
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(path.display().to_string())
}

#[cfg(target_os = "macos")]
fn imp(enabled: bool, exe: &str) -> Result<String, String> {
    let dir = dirs::home_dir().ok_or("no home dir")?.join("Library/LaunchAgents");
    let path = dir.join("io.github.copym.plist");
    if enabled {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let exe = exe.replace('&', "&amp;").replace('<', "&lt;");
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>io.github.copym</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
        );
        std::fs::write(&path, body).map_err(|e| e.to_string())?;
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(path.display().to_string())
}

#[cfg(windows)]
fn imp(enabled: bool, exe: &str) -> Result<String, String> {
    use std::process::Command;
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let status = if enabled {
        Command::new("reg")
            .args(["add", KEY, "/v", "copym", "/t", "REG_SZ", "/d", &format!("\"{exe}\" daemon"), "/f"])
            .status()
    } else {
        Command::new("reg").args(["delete", KEY, "/v", "copym", "/f"]).status()
    };
    match status {
        Ok(s) if s.success() || !enabled => Ok(format!("{KEY}\\copym")),
        Ok(s) => Err(format!("reg exited with {s}")),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn imp(_: bool, _: &str) -> Result<String, String> {
    Err("unsupported platform".into())
}

/// Windows: put "copym" in the Start menu (so Windows search finds it), pointing
/// at this exe. Rewritten on every daemon start, which also follows a moved exe.
#[cfg(windows)]
pub fn ensure_start_menu_shortcut() -> Result<std::path::PathBuf, String> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{HSTRING, Interface};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // %APPDATA%\Microsoft\Windows\Start Menu\Programs
    let dir = dirs::data_dir().ok_or("no APPDATA")?.join(r"Microsoft\Windows\Start Menu\Programs");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("copym.lnk");
    let e = |e: windows::core::Error| e.to_string();
    unsafe {
        // Ok if COM is already initialised on this thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(e)?;
        link.SetPath(&HSTRING::from(exe.as_os_str())).map_err(e)?;
        link.SetArguments(&HSTRING::from("show")).map_err(e)?;
        if let Some(parent) = exe.parent() {
            link.SetWorkingDirectory(&HSTRING::from(parent.as_os_str())).map_err(e)?;
        }
        link.SetDescription(&HSTRING::from("copym 剪贴板历史")).map_err(e)?;
        link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0).map_err(e)?;
        let file: IPersistFile = link.cast().map_err(e)?;
        file.Save(&HSTRING::from(path.as_os_str()), true).map_err(e)?;
    }
    Ok(path)
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn start_menu_shortcut() {
        let path = super::ensure_start_menu_shortcut().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 0, "{}", path.display());
    }
}
