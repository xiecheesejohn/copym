//! Background daemon: clipboard monitor + history store + IPC server + hotkey.

use crate::clip::{self, ClipEvent, ClipWriter, MonitorOptions};
use crate::config::{self, Config};
use crate::filter::Filters;
use crate::ipc;
use crate::model::{Content, Request, Response};
use crate::ocr::Ocr;
use crate::store::{GroupMatcher, Query, Store};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

struct Shared {
    store: Mutex<Store>,
    writer: Mutex<Box<dyn ClipWriter>>,
    paused: AtomicBool,
    gui: Mutex<Option<Child>>,
    backend: &'static str,
    /// Image ids waiting for text recognition.
    ocr_tx: Option<mpsc::Sender<u64>>,
    ocr_name: Option<String>,
    filters: Filters,
    sync: OnceLock<copym_sync::Engine>,
    sync_images: bool,
    sync_max_image: usize,
}

impl Shared {
    fn handle(&self, req: Request) -> Response {
        let r = || -> Result<Response, String> {
            Ok(match req {
                Request::List { query, filter, regex, limit } => {
                    let q = Query::parse(&query, regex)?;
                    Response::Items(self.store.lock().unwrap().list(&q, filter, limit as usize))
                }
                Request::ListGroup { query, regex, group, limit } => {
                    let q = Query::parse(&query, regex)?;
                    let g = group.as_ref().map(GroupMatcher::new).transpose()?;
                    Response::Items(self.store.lock().unwrap().list_group(&q, g.as_ref(), limit as usize))
                }
                Request::GroupCounts { query, regex, groups } => {
                    let q = Query::parse(&query, regex)?;
                    let gs: Vec<_> = groups.iter().map(|g| GroupMatcher::new(g).ok()).collect();
                    Response::Counts(self.store.lock().unwrap().group_counts(&q, &gs))
                }
                Request::Get { id } => Response::Content(self.store.lock().unwrap().get(id)?),
                Request::Thumb { id } => Response::Thumb(self.store.lock().unwrap().thumb(id)?),
                Request::Activate { id } => {
                    let mut store = self.store.lock().unwrap();
                    let Some(content) = store.get(id)? else { return Err("item not found".into()) };
                    self.writer.lock().unwrap().set(&content)?;
                    let source = store.source(id);
                    let new_id = store.insert(content, source)?; // move to top
                    if store.needs_ocr(new_id) {
                        if let Some(tx) = &self.ocr_tx {
                            let _ = tx.send(new_id);
                        }
                    }
                    Response::Ok
                }
                Request::Delete { id } => {
                    let tombs = self.store.lock().unwrap().delete(id)?;
                    if let Some(s) = self.sync.get() {
                        s.broadcast_delete(tombs);
                    }
                    Response::Ok
                }
                Request::TogglePin { id } => {
                    self.store.lock().unwrap().toggle_pin(id)?;
                    Response::Ok
                }
                Request::Clear => {
                    let tombs = self.store.lock().unwrap().clear()?;
                    if let Some(s) = self.sync.get() {
                        s.broadcast_delete(tombs);
                    }
                    Response::Ok
                }
                Request::SyncStatus => Response::Sync(self.sync_info()),
                Request::SyncPair => {
                    let s = self.sync.get().ok_or("同步未启用（config.toml 中 sync_enabled = false）")?;
                    let p = s.pair_start().map_err(|e| e.to_string())?;
                    Response::Pairing {
                        code: p.code,
                        addrs: p.addrs.iter().map(|a| a.to_string()).collect(),
                        expires_secs: p.expires_in.as_secs(),
                    }
                }
                Request::SyncJoin { code, addr } => {
                    let s = self.sync.get().ok_or("同步未启用（config.toml 中 sync_enabled = false）")?;
                    let addr = match addr {
                        Some(a) => Some(a.parse().map_err(|_| format!("地址无效：{a}（格式 IP:端口）"))?),
                        None => None,
                    };
                    let name = s.join(&code, addr).map_err(|e| e.to_string())?;
                    Response::Joined { name }
                }
                Request::SyncLeave => {
                    if let Some(s) = self.sync.get() {
                        s.leave().map_err(|e| e.to_string())?;
                    }
                    Response::Ok
                }
                Request::SetPaused(p) => {
                    let v = p.unwrap_or(!self.paused.load(Ordering::Relaxed));
                    self.paused.store(v, Ordering::Relaxed);
                    self.status()
                }
                Request::Status => self.status(),
                Request::ToggleGui { activation_token } => {
                    self.toggle_gui(activation_token)?;
                    Response::Ok
                }
                Request::Quit => {
                    if let Some(mut c) = self.gui.lock().unwrap().take() {
                        let _ = c.kill();
                    }
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        std::process::exit(0);
                    });
                    Response::Ok
                }
            })
        };
        r().unwrap_or_else(Response::Err)
    }

    fn sync_info(&self) -> crate::model::SyncInfo {
        match self.sync.get() {
            Some(s) => {
                let st = s.status();
                crate::model::SyncInfo {
                    enabled: true,
                    paired: st.paired,
                    device_name: st.device_name,
                    device_id: st.device_id,
                    port: st.port,
                    peers: st.peers.into_iter().map(|p| (p.name, p.addr.to_string())).collect(),
                    pairing_code: st.pairing_code,
                }
            }
            None => crate::model::SyncInfo {
                enabled: false,
                paired: false,
                device_name: String::new(),
                device_id: String::new(),
                port: 0,
                peers: Vec::new(),
                pairing_code: None,
            },
        }
    }

    /// Convert stored content to the sync wire format (None if not shareable).
    fn to_sync_item(&self, content: &Content, created: i64) -> Option<copym_sync::SyncItem> {
        Some(match content {
            Content::Text(t) => copym_sync::SyncItem {
                hash: content.hash(),
                created,
                kind: copym_sync::ItemKind::Text,
                data: t.as_bytes().to_vec(),
                width: 0,
                height: 0,
            },
            Content::Image { png, width, height } => {
                if !self.sync_images || png.len() > self.sync_max_image {
                    return None;
                }
                copym_sync::SyncItem {
                    hash: content.hash(),
                    created,
                    kind: copym_sync::ItemKind::Image,
                    data: png.clone(),
                    width: *width,
                    height: *height,
                }
            }
        })
    }

    fn queue_ocr(&self, id: u64) {
        if let Some(tx) = &self.ocr_tx {
            let _ = tx.send(id);
        }
    }

    fn status(&self) -> Response {
        Response::Status {
            paused: self.paused.load(Ordering::Relaxed),
            count: self.store.lock().unwrap().len(),
            backend: self.backend.to_string(),
            ocr: self.ocr_name.clone(),
        }
    }

    /// Open the popup, or close it if it is already open.
    fn toggle_gui(&self, token: Option<String>) -> Result<(), String> {
        let mut gui = self.gui.lock().unwrap();
        if let Some(child) = gui.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
                *gui = None;
                return Ok(());
            }
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(exe);
        cmd.arg("gui").stdin(Stdio::null()).stdout(Stdio::null());
        match token {
            Some(t) => {
                cmd.env("XDG_ACTIVATION_TOKEN", t);
            }
            None => {
                cmd.env_remove("XDG_ACTIVATION_TOKEN");
            }
        }
        #[cfg(windows)]
        unsafe {
            // We just received the hotkey, so we may hand foreground rights to the popup.
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
                windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY,
            );
        }
        *gui = Some(cmd.spawn().map_err(|e| e.to_string())?);
        Ok(())
    }
}

pub fn run() -> Result<(), String> {
    if ipc::Client::connect().is_ok() {
        eprintln!("copym: daemon already running");
        return Ok(());
    }
    let cfg = Config::load();
    let store = Store::open(&config::data_dir().join("history.redb"), cfg.max_items)?;

    let (tx, rx) = mpsc::channel::<ClipEvent>();
    let opts = MonitorOptions {
        respect_password_hints: cfg.respect_password_hints,
        store_images: cfg.store_images,
        max_text_bytes: cfg.max_text_bytes,
        max_image_bytes: cfg.max_image_bytes,
        restore_empty_clipboard: cfg.restore_empty_clipboard,
        poll_interval_ms: cfg.poll_interval_ms,
    };
    // COPYM_NO_MONITOR: developer aid for running a second, sync-only instance.
    let monitor = if std::env::var_os("COPYM_NO_MONITOR").is_some() { clip::null_monitor(tx) } else { clip::start(tx, opts)? };
    eprintln!("copym: daemon started, backend = {}", monitor.backend);

    let ocr = Ocr::detect(&cfg);
    match &ocr {
        Some(o) => eprintln!("copym: image OCR engine = {}", o.name()),
        None => eprintln!("copym: image OCR unavailable (install tesseract + language packs, or set ocr_engine)"),
    }
    let (ocr_tx, ocr_rx) = mpsc::channel::<u64>();
    let shared = Arc::new(Shared {
        store: Mutex::new(store),
        writer: Mutex::new(monitor.writer),
        paused: AtomicBool::new(false),
        gui: Mutex::new(None),
        backend: monitor.backend,
        ocr_tx: ocr.as_ref().map(|_| ocr_tx.clone()),
        ocr_name: ocr.as_ref().map(Ocr::name),
        filters: Filters::new(&cfg),
        sync: OnceLock::new(),
        sync_images: cfg.sync_images,
        sync_max_image: cfg.sync_max_image_bytes,
    });

    // LAN sync.
    if cfg.sync_enabled {
        let static_peers = cfg
            .sync_peers
            .iter()
            .filter_map(|p| match p.parse() {
                Ok(a) => Some(a),
                Err(_) => {
                    eprintln!("copym: ignoring invalid sync_peers entry {p:?} (expected IP:port)");
                    None
                }
            })
            .collect();
        let sync_cfg = copym_sync::Config {
            identity_path: config::data_dir().join("sync-identity.bin"),
            device_name: cfg.device_name(),
            port: cfg.sync_port,
            static_peers,
            mdns: true,
        };
        let host = Arc::new(SyncHost { shared: shared.clone(), history_items: cfg.sync_history_items });
        match copym_sync::Engine::start(sync_cfg, host) {
            Ok(engine) => {
                let st = engine.status();
                eprintln!(
                    "copym: sync ready as {:?} on port {} ({})",
                    st.device_name,
                    st.port,
                    if st.paired { "paired" } else { "not paired - run `copym sync pair`" }
                );
                let _ = shared.sync.set(engine);
            }
            Err(e) => eprintln!("copym: sync unavailable: {e}"),
        }
    }

    // OCR worker: one low-priority thread, processes new images and backfills old ones.
    if let Some(ocr) = ocr {
        for id in shared.store.lock().unwrap().pending_ocr() {
            let _ = ocr_tx.send(id);
        }
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("ocr".into())
            .spawn(move || {
                crate::ocr::lower_thread_priority();
                for id in ocr_rx {
                    let job = {
                        let store = shared.store.lock().unwrap();
                        if !store.needs_ocr(id) {
                            continue;
                        }
                        match (store.get(id), store.hash_of(id)) {
                            (Ok(Some(Content::Image { png, .. })), Some(hash)) => Some((png, hash)),
                            _ => None,
                        }
                    };
                    let Some((png, hash)) = job else { continue };
                    // Recognize without holding the store lock.
                    let text = ocr.recognize(&png).unwrap_or_else(|e| {
                        eprintln!("copym: ocr failed: {e}");
                        String::new()
                    });
                    if let Err(e) = shared.store.lock().unwrap().set_ocr(hash, text) {
                        eprintln!("copym: store error: {e}");
                    }
                }
            })
            .map_err(|e| e.to_string())?;
    }

    // Recorder thread.
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || {
                for ev in rx {
                    if shared.paused.load(Ordering::Relaxed) {
                        continue;
                    }
                    if let Some(why) = shared.filters.reject(&ev.content, ev.source.as_deref()) {
                        if cfg!(debug_assertions) {
                            eprintln!("copym: skipped ({why})");
                        }
                        continue;
                    }
                    let is_image = matches!(ev.content, Content::Image { .. });
                    let item = shared.sync.get().and_then(|_| shared.to_sync_item(&ev.content, crate::model::now_secs()));
                    match shared.store.lock().unwrap().insert(ev.content, ev.source) {
                        Ok(id) => {
                            if is_image {
                                shared.queue_ocr(id);
                            }
                        }
                        Err(e) => eprintln!("copym: store error: {e}"),
                    }
                    if let (Some(sync), Some(item)) = (shared.sync.get(), item) {
                        sync.broadcast_item(item);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
    }

    // IPC server.
    let listener = ipc::listen().map_err(|e| format!("cannot open IPC socket: {e}"))?;
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("ipc".into())
            .spawn(move || {
                use interprocess::local_socket::traits::ListenerExt as _;
                for conn in listener.incoming().filter_map(Result::ok) {
                    let shared = shared.clone();
                    let _ = std::thread::Builder::new()
                        .name("ipc-conn".into())
                        .spawn(move || ipc::serve_connection(conn, |req| shared.handle(req)));
                }
            })
            .map_err(|e| e.to_string())?;
    }

    // Global hotkey (must be created on the main thread on Windows/macOS).
    let primary = !config::is_secondary_instance();
    let _hotkey = if monitor.supports_hotkey && primary { register_hotkey(&cfg.hotkey, shared.clone()) } else { None };
    #[cfg(target_os = "linux")]
    if !monitor.supports_hotkey && primary {
        // Wayland: use KDE's global shortcut service when available.
        let s = shared.clone();
        match crate::kde_hotkey::register(&cfg.hotkey, move || {
            if let Err(e) = s.toggle_gui(None) {
                eprintln!("copym: cannot open popup: {e}");
            }
        }) {
            Ok(()) => eprintln!("copym: hotkey {} registered (KDE global shortcuts)", cfg.hotkey),
            Err(e) => eprintln!(
                "copym: no global hotkey ({e}); bind `copym show` to a shortcut in your desktop settings"
            ),
        }
    }

    (monitor.run)()
}

fn register_hotkey(spec: &str, shared: Arc<Shared>) -> Option<global_hotkey::GlobalHotKeyManager> {
    use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    let hotkey: HotKey = match spec.parse() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("copym: invalid hotkey {spec:?}: {e}");
            return None;
        }
    };
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("copym: hotkeys unavailable: {e}");
            return None;
        }
    };
    if let Err(e) = manager.register(hotkey) {
        eprintln!("copym: cannot register hotkey {spec}: {e}");
        return None;
    }
    let id = hotkey.id();
    GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
        if ev.id == id && ev.state == HotKeyState::Pressed {
            if let Err(e) = shared.toggle_gui(None) {
                eprintln!("copym: cannot open popup: {e}");
            }
        }
    }));
    eprintln!("copym: hotkey {spec} registered");
    Some(manager)
}

/// Start the daemon in the background if it is not running yet.
pub fn ensure_running() -> Result<(), String> {
    if ipc::Client::connect().is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config::log_path())
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map_err(|e| e.to_string())?;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(60));
        if ipc::Client::connect().is_ok() {
            return Ok(());
        }
    }
    Err(format!("daemon did not start, see {}", config::log_path().display()))
}

/// Bridges the sync engine to the local history. Received items only go into
/// history; the system clipboard is never touched.
struct SyncHost {
    shared: Arc<Shared>,
    history_items: usize,
}

impl copym_sync::Host for SyncHost {
    fn inventory(&self) -> Vec<(u64, i64)> {
        let s = &self.shared;
        s.store.lock().unwrap().sync_inventory(self.history_items, s.sync_images, s.sync_max_image as u64)
    }

    fn tombstones(&self) -> Vec<(u64, i64)> {
        self.shared.store.lock().unwrap().tombstones()
    }

    fn missing(&self, items: &[(u64, i64)]) -> Vec<u64> {
        self.shared.store.lock().unwrap().sync_missing(items)
    }

    fn fetch(&self, hash: u64) -> Option<copym_sync::SyncItem> {
        let (content, created) = self.shared.store.lock().unwrap().get_by_hash(hash).ok()??;
        self.shared.to_sync_item(&content, created)
    }

    fn receive(&self, item: copym_sync::SyncItem, from: &str) {
        let content = match item.kind {
            copym_sync::ItemKind::Text => match String::from_utf8(item.data) {
                Ok(t) => Content::Text(t),
                Err(_) => return,
            },
            copym_sync::ItemKind::Image => {
                if !self.shared.sync_images || crate::imgutil::png_size(&item.data) != Some((item.width, item.height)) {
                    return;
                }
                Content::Image { png: item.data, width: item.width, height: item.height }
            }
        };
        // Local rules still apply (ignore patterns, size limits, images off...).
        if self.shared.filters.reject(&content, None).is_some() {
            return;
        }
        let is_image = matches!(content, Content::Image { .. });
        match self.shared.store.lock().unwrap().insert_synced(content, Some(from.to_string()), item.created) {
            Ok(Some(id)) if is_image => self.shared.queue_ocr(id),
            Ok(_) => {}
            Err(e) => eprintln!("copym: store error: {e}"),
        }
    }

    fn receive_delete(&self, deleted: &[(u64, i64)]) {
        if deleted.is_empty() {
            return;
        }
        if let Err(e) = self.shared.store.lock().unwrap().delete_synced(deleted) {
            eprintln!("copym: store error: {e}");
        }
    }
}
