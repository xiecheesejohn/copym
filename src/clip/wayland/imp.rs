// Shared data-control implementation. `include!`d by `ext.rs` and `wlr.rs`,
// which alias the protocol types to `Manager`, `Device`, `Offer`, `Source`
// (and modules `dev`, `offer`, `src`) and define `MANAGER_IFACE` / `BACKEND`.

use crate::clip::{ClipEvent, ClipWriter, Monitor, MonitorOptions, KDE_PASSWORD_HINT};
use crate::imgutil;
use crate::model::Content;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

/// Marker offered on every selection we create so we can recognise (and never
/// try to read) our own selections.
const MARKER: &str = "application/x-copym-owner";
const TEXT_MIMES: &[&str] = &["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"];

type Offers = Mutex<Vec<String>>;

struct State {
    seat: Option<WlSeat>,
    manager: Option<Manager>,
    device: Option<Device>,
    tx: Sender<ClipEvent>,
    opts: MonitorOptions,
    /// Latest content seen or set, used to restore an emptied clipboard.
    last: Option<Arc<Content>>,
}

pub fn start(tx: Sender<ClipEvent>, opts: MonitorOptions) -> Result<Monitor, String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State { seat: None, manager: None, device: None, tx, opts, last: None };
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    if state.manager.is_none() {
        return Err(format!("compositor does not support {MANAGER_IFACE}"));
    }
    if state.seat.is_none() {
        return Err("no wl_seat".into());
    }
    state.ensure_device(&qh);

    let (cmd_tx, cmd_rx) = mpsc::channel::<Arc<Content>>();
    let (wake_r, wake_w) = pipe(libc::O_NONBLOCK).map_err(|e| e.to_string())?;
    let writer = WaylandWriter { tx: cmd_tx, wake: Mutex::new(std::fs::File::from(wake_w)) };

    let run = move || -> Result<(), String> { event_loop(conn, queue, state, cmd_rx, wake_r) };
    Ok(Monitor { backend: BACKEND, supports_hotkey: false, writer: Box::new(writer), run: Box::new(run) })
}

fn event_loop(
    conn: Connection,
    mut queue: wayland_client::EventQueue<State>,
    mut state: State,
    cmd_rx: Receiver<Arc<Content>>,
    wake_r: OwnedFd,
) -> Result<(), String> {
    let qh = queue.handle();
    let mut wake_file = std::fs::File::from(wake_r);
    loop {
        queue.dispatch_pending(&mut state).map_err(|e| e.to_string())?;
        while let Ok(c) = cmd_rx.try_recv() {
            state.set_selection(c, &qh);
        }
        state.ensure_device(&qh);
        if let Err(e) = conn.flush() {
            if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == io::ErrorKind::WouldBlock) {
                return Err(e.to_string());
            }
        }
        let Some(guard) = queue.prepare_read() else { continue };
        let mut fds = [
            libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_file.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.to_string());
        }
        if fds[0].revents != 0 {
            if let Err(e) = guard.read() {
                if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == io::ErrorKind::WouldBlock) {
                    return Err(e.to_string());
                }
            }
        } else {
            drop(guard);
        }
        if fds[1].revents != 0 {
            let mut buf = [0u8; 64];
            while matches!(wake_file.read(&mut buf), Ok(n) if n > 0) {}
        }
    }
}

impl State {
    fn ensure_device(&mut self, qh: &QueueHandle<Self>) {
        if self.device.is_none() {
            if let (Some(m), Some(s)) = (&self.manager, &self.seat) {
                self.device = Some(m.get_data_device(s, qh, ()));
            }
        }
    }

    fn set_selection(&mut self, content: Arc<Content>, qh: &QueueHandle<Self>) {
        let (Some(manager), Some(device)) = (&self.manager, &self.device) else { return };
        let source = manager.create_data_source(qh, content.clone());
        match &*content {
            Content::Text(_) => {
                for m in TEXT_MIMES {
                    source.offer((*m).to_string());
                }
            }
            Content::Image { .. } => source.offer("image/png".into()),
        }
        source.offer(MARKER.into());
        device.set_selection(Some(&source));
        self.last = Some(content);
    }

    fn on_selection(&mut self, offer: Option<Offer>, conn: &Connection, qh: &QueueHandle<Self>) {
        let Some(offer) = offer else {
            if self.opts.restore_empty_clipboard {
                if let Some(last) = self.last.clone() {
                    self.set_selection(last, qh);
                }
            }
            return;
        };
        let mimes: Vec<String> = offer.data::<Offers>().map(|m| m.lock().unwrap().clone()).unwrap_or_default();
        if !mimes.iter().any(|m| m == MARKER) {
            if let Some(content) = self.capture(&offer, &mimes, conn) {
                let content = Arc::new(content);
                self.last = Some(content.clone());
                let _ = self.tx.send(ClipEvent { content: (*content).clone(), source: None });
            }
        }
        offer.destroy();
    }

    fn capture(&self, offer: &Offer, mimes: &[String], conn: &Connection) -> Option<Content> {
        let has = |m: &str| mimes.iter().any(|x| x == m);
        if self.opts.respect_password_hints && has(KDE_PASSWORD_HINT) {
            // KeePassXC, Bitwarden (via arboard), KDE apps... set this to "secret".
            match read_offer(offer, KDE_PASSWORD_HINT, conn, 4096) {
                Ok(v) if String::from_utf8_lossy(&v).trim() != "secret" => {}
                _ => return None,
            }
        }
        if let Some(m) = TEXT_MIMES.iter().find(|m| has(m)) {
            let bytes = read_offer(offer, m, conn, self.opts.max_text_bytes).ok()?;
            let text = String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            return (!text.trim().is_empty()).then_some(Content::Text(text));
        }
        if self.opts.store_images && has("image/png") {
            let png = read_offer(offer, "image/png", conn, self.opts.max_image_bytes).ok()?;
            let (width, height) = imgutil::png_size(&png)?;
            return Some(Content::Image { png, width, height });
        }
        None
    }
}

fn pipe(extra_flags: i32) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | extra_flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Ask the selection owner for `mime` and read it with a timeout, so a
/// misbehaving client can never hang the daemon.
fn read_offer(offer: &Offer, mime: &str, conn: &Connection, max: usize) -> io::Result<Vec<u8>> {
    let (r, w) = pipe(0)?;
    offer.receive(mime.to_string(), w.as_fd());
    conn.flush().map_err(io::Error::other)?;
    drop(w);
    let mut file = std::fs::File::from(r);
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "clipboard read timed out"));
        }
        let mut pfd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(1000) as i32) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            continue;
        }
        match file.read(&mut chunk) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                out.extend_from_slice(&chunk[..n]);
                if out.len() > max {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "too large"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

struct WaylandWriter {
    tx: Sender<Arc<Content>>,
    wake: Mutex<std::fs::File>,
}

impl ClipWriter for WaylandWriter {
    fn set(&mut self, content: &Content) -> Result<(), String> {
        self.tx.send(Arc::new(content.clone())).map_err(|e| e.to_string())?;
        let _ = self.wake.lock().unwrap().write(&[1]);
        Ok(())
    }
}

// ---------------------------------------------------------------- dispatch --

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(1), qh, ()));
                }
                // v1 is enough: we only care about the regular clipboard, not PRIMARY.
                MANAGER_IFACE => {
                    state.manager = Some(registry.bind::<Manager, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<Manager, ()> for State {
    fn event(
        _: &mut Self,
        _: &Manager,
        _: <Manager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<Device, ()> for State {
    fn event(
        state: &mut Self,
        device: &Device,
        event: dev::Event,
        _: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            dev::Event::Selection { id } => state.on_selection(id, conn, qh),
            // We don't track PRIMARY, but must release its offers.
            dev::Event::PrimarySelection { id: Some(o) } => o.destroy(),
            dev::Event::Finished => {
                device.destroy();
                state.device = None;
            }
            _ => {}
        }
    }

    event_created_child!(State, Device, [
        dev::EVT_DATA_OFFER_OPCODE => (Offer, Offers::default()),
    ]);
}

impl Dispatch<Offer, Offers> for State {
    fn event(
        _: &mut Self,
        _: &Offer,
        event: offer::Event,
        data: &Offers,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let offer::Event::Offer { mime_type } = event {
            data.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<Source, Arc<Content>> for State {
    fn event(
        _: &mut Self,
        source: &Source,
        event: src::Event,
        data: &Arc<Content>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            src::Event::Send { mime_type, fd } => {
                let content = data.clone();
                // Write on a thread: the receiver may be slow and must not block our loop.
                std::thread::spawn(move || {
                    let mut f = std::fs::File::from(fd);
                    let bytes: &[u8] = match (&*content, mime_type.as_str()) {
                        (_, MARKER) => &[],
                        (Content::Text(t), _) => t.as_bytes(),
                        (Content::Image { png, .. }, _) => png,
                    };
                    let _ = f.write_all(bytes);
                });
            }
            src::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}
