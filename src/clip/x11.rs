//! X11 backend: XFixes selection-owner notifications (no polling).
//! TARGETS are inspected for `x-kde-passwordManagerHint` before anything is read.

use super::arboard_io::{self, ArboardWriter};
use super::{ClipEvent, Monitor, MonitorOptions, KDE_PASSWORD_HINT};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, WindowClass};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    hint: Atom,
    prop: Atom,
}

pub fn start(tx: Sender<ClipEvent>, opts: MonitorOptions) -> Result<Monitor, String> {
    let (conn, screen_num) = RustConnection::connect(None).map_err(|e| e.to_string())?;
    conn.xfixes_query_version(5, 0).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
    let screen = &conn.setup().roots[screen_num];
    let win = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(|e| e.to_string())?;
    let intern = |name: &str| -> Result<Atom, String> {
        Ok(conn.intern_atom(false, name.as_bytes()).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
    };
    let atoms = Atoms {
        clipboard: intern("CLIPBOARD")?,
        targets: intern("TARGETS")?,
        hint: intern(KDE_PASSWORD_HINT)?,
        prop: intern("COPYM_SELECTION")?,
    };
    conn.xfixes_select_selection_input(win, atoms.clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER)
        .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;

    let writer = ArboardWriter::spawn()?;
    let run = move || -> Result<(), String> {
        let mut clip = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        loop {
            let ev = conn.wait_for_event().map_err(|e| e.to_string())?;
            let Event::XfixesSelectionNotify(ev) = ev else { continue };
            if ev.owner == x11rb::NONE {
                continue;
            }
            if opts.respect_password_hints {
                let targets = convert(&conn, win, &atoms, atoms.targets, ev.selection_timestamp)
                    .map(|v| {
                        v.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if targets.contains(&atoms.hint) {
                    let secret = convert(&conn, win, &atoms, atoms.hint, ev.selection_timestamp)
                        .map(|v| String::from_utf8_lossy(&v).trim() == "secret")
                        .unwrap_or(true);
                    if secret {
                        continue;
                    }
                }
            }
            if let Some(content) = arboard_io::read(&mut clip, &opts) {
                let _ = tx.send(ClipEvent { content, source: None });
            }
        }
    };
    Ok(Monitor { backend: "x11 (xfixes)", supports_hotkey: true, writer: Box::new(writer), run: Box::new(run) })
}

/// Convert the CLIPBOARD selection to `target` and return the property bytes.
fn convert(conn: &RustConnection, win: u32, atoms: &Atoms, target: Atom, time: u32) -> Option<Vec<u8>> {
    conn.convert_selection(win, atoms.clipboard, target, atoms.prop, time).ok()?;
    conn.flush().ok()?;
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        match conn.poll_for_event().ok()? {
            Some(Event::SelectionNotify(n)) if n.requestor == win => {
                if n.property == x11rb::NONE {
                    return None;
                }
                let reply = conn
                    .get_property(true, win, atoms.prop, AtomEnum::ANY, 0, 1 << 20)
                    .ok()?
                    .reply()
                    .ok()?;
                return Some(reply.value);
            }
            Some(_) => {}
            None => {
                if Instant::now() > deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
