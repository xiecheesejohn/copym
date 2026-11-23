//! Windows backend: `AddClipboardFormatListener` on a message-only window
//! (event driven, zero polling).
//!
//! Content is skipped when the owner marks it as private, as Bitwarden,
//! KeePass(XC), 1Password etc. do:
//! * `ExcludeClipboardContentFromMonitorProcessing` present
//! * `CanIncludeInClipboardHistory` == 0
//! * `Clipboard Viewer Ignore` present

use super::arboard_io::{self, ArboardWriter};
use super::{ClipEvent, Monitor, MonitorOptions};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, GetClipboardOwner, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowThreadProcessId, RegisterClassW,
    TranslateMessage, HWND_MESSAGE, MSG, WM_CLIPBOARDUPDATE, WNDCLASSW,
};

static NOTIFY: OnceLock<Sender<()>> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        if let Some(tx) = NOTIFY.get() {
            let _ = tx.send(());
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

struct Privacy {
    excluded: bool,
    owner_pid: u32,
}

/// Inspect the private-content markers and the owner process.
fn inspect() -> Privacy {
    unsafe {
        let exclude = RegisterClipboardFormatW(wide("ExcludeClipboardContentFromMonitorProcessing").as_ptr());
        let viewer_ignore = RegisterClipboardFormatW(wide("Clipboard Viewer Ignore").as_ptr());
        let can_include = RegisterClipboardFormatW(wide("CanIncludeInClipboardHistory").as_ptr());
        let mut excluded = IsClipboardFormatAvailable(exclude) != 0 || IsClipboardFormatAvailable(viewer_ignore) != 0;

        let mut owner_pid = 0u32;
        let owner = GetClipboardOwner();
        if !owner.is_null() {
            GetWindowThreadProcessId(owner, &mut owner_pid);
        }

        if !excluded && IsClipboardFormatAvailable(can_include) != 0 {
            // Need the clipboard open to read the DWORD value.
            let mut opened = false;
            for _ in 0..10 {
                if OpenClipboard(null_mut()) != 0 {
                    opened = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            if opened {
                let h = GetClipboardData(can_include);
                if !h.is_null() && GlobalSize(h) >= 4 {
                    let p = GlobalLock(h) as *const u32;
                    if !p.is_null() {
                        if p.read_unaligned() == 0 {
                            excluded = true;
                        }
                        GlobalUnlock(h);
                    }
                }
                CloseClipboard();
            } else {
                excluded = true; // be safe: can't verify
            }
        }
        Privacy { excluded, owner_pid }
    }
}

fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
        Some(name.trim_end_matches(".exe").trim_end_matches(".EXE").to_string())
    }
}

pub fn start(tx: Sender<ClipEvent>, opts: MonitorOptions) -> Result<Monitor, String> {
    let (ntx, nrx) = mpsc::channel::<()>();
    NOTIFY.set(ntx).map_err(|_| "clipboard listener already started".to_string())?;

    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = wide("copym-clipboard-listener");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), hinst, null());
        if hwnd.is_null() {
            return Err("CreateWindowExW failed".into());
        }
        if AddClipboardFormatListener(hwnd) == 0 {
            return Err("AddClipboardFormatListener failed".into());
        }
    }

    // Worker: does the actual reading so the message loop (and hotkeys) stay responsive.
    let own_pid = unsafe { GetCurrentProcessId() };
    std::thread::Builder::new()
        .name("clip-reader".into())
        .spawn(move || {
            let mut clip = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("copym: clipboard unavailable: {e}");
                    return;
                }
            };
            while nrx.recv().is_ok() {
                // Coalesce bursts (apps often set several formats in a row).
                std::thread::sleep(Duration::from_millis(40));
                while nrx.try_recv().is_ok() {}
                let p = inspect();
                if opts.respect_password_hints && p.excluded {
                    continue;
                }
                if p.owner_pid == own_pid {
                    continue;
                }
                let source = process_name(p.owner_pid);
                if let Some(content) = arboard_io::read(&mut clip, &opts) {
                    let _ = tx.send(ClipEvent { content, source });
                }
            }
        })
        .map_err(|e| e.to_string())?;

    let writer = ArboardWriter::spawn()?;
    let run = || -> Result<(), String> {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    };
    Ok(Monitor { backend: "windows (clipboard format listener)", supports_hotkey: true, writer: Box::new(writer), run: Box::new(run) })
}
