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
