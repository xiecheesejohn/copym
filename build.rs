//! Windows: embed the app icon and version info in copym.exe, so Explorer, the
//! taskbar and the Start menu shortcut show the copym icon.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico").set("FileDescription", "copym").set("ProductName", "copym");
        // Needs rc.exe (MSVC) or windres (MinGW); a missing tool only costs the icon.
        if let Err(e) = res.compile() {
            if std::env::var_os("CI").is_some() {
                panic!("cannot embed the icon: {e}");
            }
            println!("cargo:warning=copym.exe built without icon: {e}");
        }
    }
}
