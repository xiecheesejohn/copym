//! Manual test: copy text the way Bitwarden's desktop app does (arboard +
//! exclude_from_history). copym must NOT record it.
//! Usage: cargo run --example secret_copy -- "text" [--normal]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let text = args.first().cloned().unwrap_or_else(|| "s3cr3t-password".into());
    let normal = args.iter().any(|a| a == "--normal");
    let mut cb = arboard::Clipboard::new().expect("clipboard");
    let set = cb.set();
    #[cfg(target_os = "linux")]
    let set = {
        use arboard::SetExtLinux;
        if normal { set } else { set.exclude_from_history() }
    };
    #[cfg(any(windows, target_os = "macos"))]
    let set = {
        #[cfg(windows)]
        use arboard::SetExtWindows;
        #[cfg(target_os = "macos")]
        use arboard::SetExtApple;
        if normal { set } else { set.exclude_from_history() }
    };
    set.text(text.clone()).expect("set");
    println!("copied {:?} ({})", text, if normal { "normal" } else { "secret" });
    // keep ownership a moment so the clipboard manager can see it
    std::thread::sleep(std::time::Duration::from_secs(2));
}
