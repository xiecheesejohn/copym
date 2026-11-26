//! "Paste into the previous window": send the paste shortcut (Ctrl+V,
//! Shift+Insert, ⌘V…) as synthetic key presses after the popup closed.
//!
//! * Windows: `SendInput`.
//! * macOS: `CGEventPost` (needs the Accessibility permission).
//! * Linux X11: XTEST.
//! * Linux Wayland: a virtual keyboard through `/dev/uinput` when it is
//!   writable, otherwise `wtype` or `ydotool` when installed.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mod {
    Ctrl,
    Shift,
    Alt,
    /// ⌘ on macOS, the Windows / Super key elsewhere.
    Meta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyName {
    Letter(char),
    Insert,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcut {
    pub mods: Vec<Mod>,
    pub key: KeyName,
}

pub fn default_shortcut() -> &'static str {
    if cfg!(target_os = "macos") { "cmd+v" } else { "ctrl+v" }
}

/// Parse "ctrl+v", "Shift+Insert", "cmd+shift+v" …
pub fn parse(spec: &str) -> Result<Shortcut, String> {
    let spec = if spec.trim().is_empty() { default_shortcut() } else { spec.trim() };
    let mut mods = Vec::new();
    let mut key = None;
    for part in spec.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods.push(Mod::Ctrl),
            "shift" => mods.push(Mod::Shift),
            "alt" | "option" | "opt" => mods.push(Mod::Alt),
            "cmd" | "command" | "super" | "meta" | "win" => mods.push(Mod::Meta),
            "insert" | "ins" => key = Some(KeyName::Insert),
            p if p.len() == 1 && p.as_bytes()[0].is_ascii_lowercase() => key = Some(KeyName::Letter(p.as_bytes()[0] as char)),
            _ => return Err(format!("paste_shortcut: unknown key {part:?} in {spec:?}")),
        }
    }
    Ok(Shortcut { mods, key: key.ok_or_else(|| format!("paste_shortcut: no key in {spec:?}"))? })
}

/// Press and release the shortcut.
pub fn send(spec: &str) -> Result<(), String> {
    let sc = parse(spec)?;
    imp::send(&sc)
}

// ----------------------------------------------------------------- linux --

#[cfg(target_os = "linux")]
mod imp {
    use super::{KeyName, Mod, Shortcut};
    use std::sync::Mutex;

    pub fn send(sc: &Shortcut) -> Result<(), String> {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        if !wayland && std::env::var_os("DISPLAY").is_some() {
            return xtest(sc);
        }
        let mut errors = Vec::new();
        match uinput(sc) {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(e),
        }
        match external(sc) {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(e),
        }
        Err(tf!(
            "无法模拟按键（{}）。可以给当前用户 /dev/uinput 的写权限，或安装 wtype / ydotool",
            "cannot simulate keys ({}). Give your user write access to /dev/uinput, or install wtype / ydotool",
            errors.join("; ")
        ))
    }

    // Linux input event codes (linux/input-event-codes.h).
    const KEY_LEFTCTRL: u16 = 29;
    const KEY_LEFTSHIFT: u16 = 42;
    const KEY_LEFTALT: u16 = 56;
    const KEY_LEFTMETA: u16 = 125;
    const KEY_INSERT: u16 = 110;

    fn evdev_code(k: KeyName) -> u16 {
        match k {
            KeyName::Insert => KEY_INSERT,
            KeyName::Letter(c) => {
                const ROW: [(&str, u16); 3] = [("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)];
                ROW.iter()
                    .find_map(|(row, base)| row.find(c).map(|i| base + i as u16))
                    .unwrap_or(47) // 'v'
            }
        }
    }

    fn evdev_mod(m: Mod) -> u16 {
        match m {
            Mod::Ctrl => KEY_LEFTCTRL,
            Mod::Shift => KEY_LEFTSHIFT,
            Mod::Alt => KEY_LEFTALT,
            Mod::Meta => KEY_LEFTMETA,
        }
    }

    /// Key presses in order: modifiers down, key down, key up, modifiers up.
    fn sequence(sc: &Shortcut) -> Vec<(u16, bool)> {
        let mods: Vec<u16> = sc.mods.iter().map(|m| evdev_mod(*m)).collect();
        let key = evdev_code(sc.key);
        let mut v: Vec<(u16, bool)> = mods.iter().map(|m| (*m, true)).collect();
        v.push((key, true));
        v.push((key, false));
        v.extend(mods.iter().rev().map(|m| (*m, false)));
        v
    }

    // --- uinput ----------------------------------------------------------

    /// The virtual keyboard is created once and kept: the compositor needs a
    /// moment to pick up a new input device.
    static UINPUT: Mutex<Option<std::fs::File>> = Mutex::new(None);

    const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
    const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
    const UI_DEV_SETUP: libc::c_ulong = 0x405C_5503;
    const UI_DEV_CREATE: libc::c_ulong = 0x5501;
    const EV_SYN: u16 = 0;
    const EV_KEY: u16 = 1;

    #[repr(C)]
    struct UinputSetup {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
        name: [u8; 80],
        ff_effects_max: u32,
    }

    fn create_device() -> Result<std::fs::File, String> {
        use std::os::fd::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .map_err(|e| format!("/dev/uinput: {e}"))?;
        let fd = f.as_raw_fd();
        unsafe {
            if libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int) < 0 {
                return Err("uinput: UI_SET_EVBIT failed".into());
            }
            // Every key we may send (modifiers, Insert and the letters).
            for code in [KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_LEFTALT, KEY_LEFTMETA, KEY_INSERT].into_iter().chain(16..26).chain(30..39).chain(44..51) {
                libc::ioctl(fd, UI_SET_KEYBIT, code as libc::c_int);
            }
            let mut setup = UinputSetup { bustype: 0x06 /* BUS_VIRTUAL */, vendor: 0x1209, product: 0xc0b7, version: 1, name: [0; 80], ff_effects_max: 0 };
            let name = b"copym virtual keyboard";
            setup.name[..name.len()].copy_from_slice(name);
            if libc::ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) < 0 {
                return Err("uinput: UI_DEV_SETUP failed".into());
            }
            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                return Err("uinput: UI_DEV_CREATE failed".into());
            }
        }
        // Let the compositor (libinput) discover the device.
        std::thread::sleep(std::time::Duration::from_millis(250));
        Ok(f)
    }

    fn emit(f: &mut std::fs::File, kind: u16, code: u16, value: i32) -> std::io::Result<()> {
        use std::io::Write;
        let ev = libc::input_event { time: libc::timeval { tv_sec: 0, tv_usec: 0 }, type_: kind, code, value };
        let bytes = unsafe { std::slice::from_raw_parts(&ev as *const _ as *const u8, std::mem::size_of::<libc::input_event>()) };
        f.write_all(bytes)
    }

    fn uinput(sc: &Shortcut) -> Result<(), String> {
        let mut guard = UINPUT.lock().unwrap();
        if guard.is_none() {
            *guard = Some(create_device()?);
        }
        let f = guard.as_mut().unwrap();
        let r = (|| -> std::io::Result<()> {
            for (code, down) in sequence(sc) {
                emit(f, EV_KEY, code, down as i32)?;
                emit(f, EV_SYN, 0, 0)?;
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
            Ok(())
        })();
        if let Err(e) = r {
            *guard = None;
            return Err(format!("uinput: {e}"));
        }
        Ok(())
    }

    // --- external tools ----------------------------------------------------

    fn external(sc: &Shortcut) -> Result<(), String> {
        use std::process::{Command, Stdio};
        let run = |cmd: &mut Command| cmd.stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success());
        // wtype (wlroots compositors)
        let mut args: Vec<String> = Vec::new();
        let wname = |m: &Mod| match m {
            Mod::Ctrl => "ctrl",
            Mod::Shift => "shift",
            Mod::Alt => "alt",
            Mod::Meta => "logo",
        };
        for m in &sc.mods {
            args.extend(["-M".into(), wname(m).into()]);
        }
        match sc.key {
            KeyName::Letter(c) => args.extend(["-k".into(), c.to_string()]),
            KeyName::Insert => args.extend(["-k".into(), "Insert".into()]),
        }
        for m in sc.mods.iter().rev() {
            args.extend(["-m".into(), wname(m).into()]);
        }
        if let Ok(true) = run(Command::new("wtype").args(&args)) {
            return Ok(());
        }
        // ydotool (needs ydotoold)
        let keys: Vec<String> = sequence(sc).into_iter().map(|(c, d)| format!("{c}:{}", d as u8)).collect();
        if let Ok(true) = run(Command::new("ydotool").arg("key").args(&keys)) {
            return Ok(());
        }
        Err(t!("没有可用的 wtype / ydotool", "neither wtype nor ydotool is available").into())
    }

    // --- X11 ---------------------------------------------------------------

    fn xtest(sc: &Shortcut) -> Result<(), String> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt as _;
        use x11rb::protocol::xtest::ConnectionExt as _;
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("X11: {e}"))?;
        let root = conn.setup().roots[screen].root;
        let min = conn.setup().min_keycode;
        let max = conn.setup().max_keycode;
        let map = conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let per = map.keysyms_per_keycode as usize;
        let keycode = |sym: u32| -> Option<u8> {
            map.keysyms.chunks(per.max(1)).position(|c| c.contains(&sym)).map(|i| min + i as u8)
        };
        let sym_of_mod = |m: &Mod| match m {
            Mod::Ctrl => 0xffe3,
            Mod::Shift => 0xffe1,
            Mod::Alt => 0xffe9,
            Mod::Meta => 0xffeb,
        };
        let key_sym = match sc.key {
            KeyName::Letter(c) => c as u32,
            KeyName::Insert => 0xff63,
        };
        let mods: Vec<u8> = sc.mods.iter().filter_map(|m| keycode(sym_of_mod(m))).collect();
        let key = keycode(key_sym).ok_or("X11: key not in keymap")?;
        const PRESS: u8 = 2;
        const RELEASE: u8 = 3;
        let fake = |t: u8, k: u8| conn.xtest_fake_input(t, k, 0, root, 0, 0, 0).map(|_| ()).map_err(|e| e.to_string());
        for m in &mods {
            fake(PRESS, *m)?;
        }
        fake(PRESS, key)?;
        fake(RELEASE, key)?;
        for m in mods.iter().rev() {
            fake(RELEASE, *m)?;
        }
        conn.flush().map_err(|e| e.to_string())?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn codes() {
            assert_eq!(evdev_code(KeyName::Letter('v')), 47);
            assert_eq!(evdev_code(KeyName::Letter('q')), 16);
            assert_eq!(evdev_code(KeyName::Letter('a')), 30);
            assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
            let sc = super::super::parse("ctrl+shift+v").unwrap();
            assert_eq!(sequence(&sc), vec![(29, true), (42, true), (47, true), (47, false), (42, false), (29, false)]);
        }
    }
}

// --------------------------------------------------------------- windows --

#[cfg(windows)]
mod imp {
    use super::{KeyName, Mod, Shortcut};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    };

    fn input(vk: u16, up: bool, extended: bool) -> INPUT {
        let mut flags = if up { KEYEVENTF_KEYUP } else { 0 };
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        }
    }

    pub fn send(sc: &Shortcut) -> Result<(), String> {
        let mods: Vec<u16> = sc
            .mods
            .iter()
            .map(|m| match m {
                Mod::Ctrl => 0x11,
                Mod::Shift => 0x10,
                Mod::Alt => 0x12,
                Mod::Meta => 0x5B,
            })
            .collect();
        let (key, ext) = match sc.key {
            KeyName::Letter(c) => (c.to_ascii_uppercase() as u16, false),
            KeyName::Insert => (0x2D, true),
        };
        let mut v: Vec<INPUT> = mods.iter().map(|m| input(*m, false, false)).collect();
        v.push(input(key, false, ext));
        v.push(input(key, true, ext));
        v.extend(mods.iter().rev().map(|m| input(*m, true, false)));
        let n = unsafe { SendInput(v.len() as u32, v.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        if n as usize == v.len() { Ok(()) } else { Err(t!("SendInput 被系统拒绝（目标窗口权限更高？）", "SendInput was refused (does the target window run with higher privileges?)").into()) }
    }
}

// ----------------------------------------------------------------- macos --

#[cfg(target_os = "macos")]
mod imp {
    use super::{KeyName, Mod, Shortcut};
    use std::ffi::c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceCreate(state: i32) -> *mut c_void;
        fn CGEventCreateKeyboardEvent(source: *mut c_void, key: u16, down: bool) -> *mut c_void;
        fn CGEventSetFlags(event: *mut c_void, flags: u64);
        fn CGEventPost(tap: u32, event: *mut c_void);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
        static kAXTrustedCheckOptionPrompt: *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFBooleanTrue: *const c_void;
        static kCFTypeDictionaryKeyCallBacks: u8;
        static kCFTypeDictionaryValueCallBacks: u8;
        fn CFDictionaryCreate(
            alloc: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            n: isize,
            key_cb: *const u8,
            value_cb: *const u8,
        ) -> *const c_void;
    }

    /// Without the Accessibility permission macOS drops synthetic key
    /// events. Ask once per run with the system's own dialog (it offers
    /// "Open System Settings"), never by jumping into the settings ourselves.
    fn trusted() -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        static ASKED: AtomicBool = AtomicBool::new(false);
        if unsafe { AXIsProcessTrusted() } {
            return true;
        }
        if !ASKED.swap(true, Ordering::SeqCst) {
            unsafe {
                let keys = [kAXTrustedCheckOptionPrompt];
                let values = [kCFBooleanTrue];
                let opts = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                AXIsProcessTrustedWithOptions(opts);
                if !opts.is_null() {
                    CFRelease(opts);
                }
            }
        }
        false
    }

    /// Key codes of the modifier keys themselves (left side).
    fn mod_key(m: Mod) -> (u16, u64) {
        match m {
            Mod::Meta => (55, 0x0010_0000),  // kVK_Command
            Mod::Shift => (56, 0x0002_0000), // kVK_Shift
            Mod::Alt => (58, 0x0008_0000),   // kVK_Option
            Mod::Ctrl => (59, 0x0004_0000),  // kVK_Control
        }
    }

    /// ANSI virtual key codes.
    fn keycode(k: KeyName) -> u16 {
        match k {
            KeyName::Insert => 114, // Help/Insert
            KeyName::Letter(c) => {
                const MAP: &[(char, u16)] = &[
                    ('a', 0), ('s', 1), ('d', 2), ('f', 3), ('h', 4), ('g', 5), ('z', 6), ('x', 7), ('c', 8), ('v', 9),
                    ('b', 11), ('q', 12), ('w', 13), ('e', 14), ('r', 15), ('y', 16), ('t', 17), ('o', 31), ('u', 32),
                    ('i', 34), ('p', 35), ('l', 37), ('j', 38), ('k', 40), ('n', 45), ('m', 46),
                ];
                MAP.iter().find(|(ch, _)| *ch == c).map(|(_, k)| *k).unwrap_or(9)
            }
        }
    }

    /// Like CopyQ: modifier down, key down, key up, modifier up, each a real
    /// event from the combined session state, posted at the HID tap.
    pub fn send(sc: &Shortcut) -> Result<(), String> {
        if !trusted() {
            return Err(t!(
                "已复制。自动粘贴需要「辅助功能」权限：系统设置 › 隐私与安全性 › 辅助功能 里允许 copym（更新后需先删掉旧的 copym 再重新添加），然后重启 copym",
                "Copied. Pasting needs the Accessibility permission: allow copym in System Settings › Privacy & Security › Accessibility (after an update remove the old copym entry and add it again), then restart copym"
            )
            .into());
        }
        let mods: Vec<(u16, u64)> = sc.mods.iter().map(|m| mod_key(*m)).collect();
        let all: u64 = mods.iter().fold(0, |f, (_, m)| f | m);
        let key = keycode(sc.key);
        // (key code, down, flags while it is sent)
        let mut events = Vec::new();
        let mut flags = 0u64;
        for (code, mask) in &mods {
            flags |= mask;
            events.push((*code, true, flags));
        }
        // 0x8 (a device-dependent bit) makes Emacs accept ⌘V (Flycut / CopyQ)
        let extra = if all & 0x0010_0000 != 0 { 0x8 } else { 0 };
        events.push((key, true, all | extra));
        events.push((key, false, all | extra));
        for (code, mask) in mods.iter().rev() {
            flags &= !mask;
            events.push((*code, false, flags));
        }
        unsafe {
            let src = CGEventSourceCreate(0); // kCGEventSourceStateCombinedSessionState
            for (code, down, flags) in events {
                let ev = CGEventCreateKeyboardEvent(src, code, down);
                if ev.is_null() {
                    continue;
                }
                CGEventSetFlags(ev, flags);
                CGEventPost(0, ev); // kCGHIDEventTap
                CFRelease(ev);
            }
            if !src.is_null() {
                CFRelease(src);
            }
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod imp {
    pub fn send(_: &super::Shortcut) -> Result<(), String> {
        Err("unsupported platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shortcuts() {
        assert_eq!(parse("Ctrl+V").unwrap(), Shortcut { mods: vec![Mod::Ctrl], key: KeyName::Letter('v') });
        assert_eq!(parse("shift+insert").unwrap(), Shortcut { mods: vec![Mod::Shift], key: KeyName::Insert });
        assert_eq!(parse("cmd+shift+v").unwrap().mods, vec![Mod::Meta, Mod::Shift]);
        assert!(parse("ctrl+").is_err());
        assert!(parse("hyper+v").is_err());
        assert!(parse("").is_ok());
    }
}
