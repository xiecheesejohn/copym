//! The popup window (egui / eframe, OpenGL renderer).
//!
//! A separate process that talks to the daemon over IPC. By default it is the
//! long-lived "popup host" (fonts and event loop stay loaded, each hotkey press
//! only creates the window); with `keep_popup_loaded = false` it is started on
//! demand and exits when the window closes.

use crate::config::Config;
use crate::groups::{self, GroupDef, Groups};
use crate::imgutil;
use crate::ipc::Client;
use crate::model::{now_secs, Content, GroupRule, ItemMeta, Kind, Request, Response, SyncInfo};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Key,
    Layout, Margin, Modifiers, Pos2, Rect, RichText, Sense, Stroke, TextFormat, TextureHandle, TextureOptions,
    ViewportCommand, Vec2,
};
use copym_core::kinds::{self, classify, TextKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const ROW_H: f32 = 40.0;
const SECTION_H: f32 = 30.0;
const SEARCH_H: f32 = 56.0;
const FOOTER_H: f32 = 40.0;
const LIST_LIMIT: u32 = 1000;
const PREVIEW_MAX_SIDE: u32 = 2048;

#[cfg(target_os = "macos")]
const KEY_GROUP: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const KEY_GROUP: &str = "Ctrl+";
#[cfg(target_os = "macos")]
const KEY_ACTIONS: &str = "⌘K";
#[cfg(not(target_os = "macos"))]
const KEY_ACTIONS: &str = "Alt+K";

/// What the popup asks for once its window is gone.
#[derive(Default)]
struct AfterClose {
    /// Send the paste shortcut to the previous window.
    paste: bool,
}

/// One-shot popup process. `open`: "" (normal), "sync" (sync dialog),
/// "groups" (group manager) or "actions" (action panel).
pub fn run(open: &str) -> Result<(), String> {
    trace("gui::run");
    let prev = std::env::var("COPYM_PREV_WINDOW").ok();
    let after = open_window(None, Config::load(), open, None, Arc::new(AtomicBool::new(false)))?;
    if let Some(e) = finish(&after, prev.as_deref()) {
        eprintln!("copym: {e}");
    }
    Ok(())
}

/// Long-lived popup process (`keep_popup_loaded`): fonts and the event loop
/// stay loaded, so the hotkey only has to create the window. The daemon
/// writes `toggle<TAB>activation-token<TAB>previous-window` lines on stdin.
pub fn host() -> Result<(), String> {
    trace("popup host");
    let ctx = egui::Context::default();
    setup_ctx(&ctx, &Config::load());
    let open = Arc::new(AtomicBool::new(false));
    let close = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Option<(Option<String>, Option<String>)>>();
    {
        let (ctx, open, close) = (ctx.clone(), open.clone(), close.clone());
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let mut parts = line.split('\t').map(|p| (p != "-" && !p.is_empty()).then(|| p.to_string()));
                let _cmd = parts.next();
                let token = parts.next().flatten();
                let prev = parts.next().flatten();
                if open.swap(true, Ordering::SeqCst) {
                    // Already open (or opening): the hotkey closes it.
                    close.store(true, Ordering::SeqCst);
                    ctx.request_repaint();
                } else if tx.send(Some((token, prev))).is_err() {
                    break;
                }
            }
            // stdin closed: the daemon is gone.
            close.store(true, Ordering::SeqCst);
            ctx.request_repaint();
            let _ = tx.send(None);
        });
    }
    let mut shown = false;
    let mut notice = None;
    while let Ok(Some((token, prev))) = rx.recv() {
        trace("show");
        // SAFETY: only this thread reads the environment from here on
        // (winit reads the activation token when it creates the window).
        unsafe {
            match &token {
                Some(t) => std::env::set_var("XDG_ACTIVATION_TOKEN", t),
                None => std::env::remove_var("XDG_ACTIVATION_TOKEN"),
            }
        }
        if shown {
            // Every window gets a new GL context: upload the whole font atlas again.
            let atlas = ctx.fonts(|f| f.image());
            ctx.tex_manager().write().set(
                egui::TextureId::default(),
                egui::epaint::ImageDelta::full(atlas, egui::epaint::TextureAtlas::texture_options()),
            );
        }
        // Start every window fresh: no open menus, no widget state (scroll
        // positions…) from the previous one. (Fonts and style stay.)
        egui::Popup::close_all(&ctx);
        ctx.memory_mut(|m| m.data.clear());
        close.store(false, Ordering::SeqCst);
        let after = open_window(Some(ctx.clone()), Config::load(), "", notice.take(), close.clone());
        shown = true;
        open.store(false, Ordering::SeqCst);
        match after {
            Ok(after) => notice = finish(&after, prev.as_deref()).map(|e| format!("自动粘贴失败：{e}（内容已在剪贴板）")),
            Err(e) => {
                eprintln!("copym: popup: {e}");
                return Err(e);
            }
        }
    }
    Ok(())
}

fn open_window(ctx: Option<egui::Context>, cfg: Config, open: &str, notice: Option<String>, close: Arc<AtomicBool>) -> Result<AfterClose, String> {
    let open = open.to_string();
    let fresh = ctx.is_none();
    let after = Arc::new(Mutex::new(AfterClose::default()));
    let result = after.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("copym")
            .with_app_id("copym")
            .with_inner_size([760.0, 480.0])
            .with_min_inner_size([560.0, 340.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_active(true),
        centered: true,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native_ext(
        "copym",
        options,
        ctx,
        Box::new(move |cc| {
            trace("window + GL ready");
            if fresh {
                setup_ctx(&cc.egui_ctx, &cfg);
                trace("fonts");
            }
            let mut app = App::new(cfg, after, close);
            trace("app state");
            if let Some(n) = notice {
                app.notify(n);
            }
            app.sync_open = open == "sync";
            if open == "actions" {
                app.actions = Some(0);
            }
            if open == "groups" {
                app.editor = Some(Editor::new(&app.groups, app.group));
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| e.to_string())?;
    let mut r = result.lock().unwrap();
    Ok(std::mem::take(&mut *r))
}

/// Fonts, theme and widget style (once per egui context).
fn setup_ctx(ctx: &egui::Context, cfg: &Config) {
    install_fonts(ctx);
    match cfg.theme.as_str() {
        "dark" => ctx.set_theme(egui::ThemePreference::Dark),
        "light" => ctx.set_theme(egui::ThemePreference::Light),
        _ => ctx.set_theme(egui::ThemePreference::System),
    }
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.scroll.bar_width = 6.0;
        s.spacing.scroll.floating = true;
        s.interaction.selectable_labels = false;
        s.visuals.window_corner_radius = CornerRadius::same(12);
        s.visuals.menu_corner_radius = CornerRadius::same(10);
        for w in [
            &mut s.visuals.widgets.inactive,
            &mut s.visuals.widgets.hovered,
            &mut s.visuals.widgets.active,
            &mut s.visuals.widgets.noninteractive,
            &mut s.visuals.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(7);
        }
    });
}

/// After the window closed: give focus back to the previous window and paste.
/// Returns the paste error, if any.
fn finish(after: &AfterClose, prev: Option<&str>) -> Option<String> {
    restore_focus(prev);
    if !after.paste {
        return None;
    }
    trace("paste");
    match Client::connect().and_then(|mut c| c.request(&Request::Paste)) {
        Ok(Response::Err(e)) => Some(e),
        Err(e) => Some(e.to_string()),
        Ok(_) => None,
    }
}

#[cfg_attr(not(windows), allow(unused_variables))]
fn restore_focus(prev: Option<&str>) {
    #[cfg(windows)]
    if let Some(h) = prev.and_then(|p| p.parse::<isize>().ok()) {
        // We are the foreground process, so this is allowed.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(h as _);
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        // A windowless app stays active on macOS: hide it so the previous app
        // gets the focus back.
        objc2_app_kit::NSApplication::sharedApplication(mtm).hide(None);
    }
}

// ------------------------------------------------------------------ fonts --

/// Key symbols (⌘ ⇧ ⌥ ⌃ ⏎ ⌫ ⎋ arrows) subset from Adwaita Sans (OFL-1.1, see
/// assets/fonts). macOS system fonts lack ⏎/⇧/⌥ and egui drew them as boxes.
static SYMBOLS_FONT: &[u8] = include_bytes!("../assets/fonts/copym-symbols.ttf");

/// egui's bundled fonts have no CJK glyphs; add a system font as fallback.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    if let Some((bytes, index)) = find_cjk_font() {
        let mut data = FontData::from_owned(bytes);
        data.index = index;
        fonts.font_data.insert("system-cjk".into(), Arc::new(data));
        // Proportional: CJK font first (modern UI look, consistent metrics).
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "system-cjk".into());
        fonts.families.entry(FontFamily::Monospace).or_default().push("system-cjk".into());
    }
    add_symbols(&mut fonts);
    ctx.set_fonts(fonts);
}

/// First in every family, so keycaps look the same on every platform (the
/// font has no other glyphs).
fn add_symbols(fonts: &mut FontDefinitions) {
    fonts.font_data.insert("copym-symbols".into(), Arc::new(FontData::from_static(SYMBOLS_FONT)));
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, "copym-symbols".into());
    }
}

fn find_cjk_font() -> Option<(Vec<u8>, u32)> {
    let mut candidates: Vec<(String, u32)> = Vec::new();
    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("fc-match").args(["-f", "%{file}\n%{index}", "sans-serif:lang=zh-cn"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        let mut it = s.lines();
        if let Some(file) = it.next() {
            // fontconfig encodes variable-font named instances in the high 16 bits.
            let index = it.next().and_then(|i| i.trim().parse::<u32>().ok()).unwrap_or(0) & 0xFFFF;
            let lower = file.to_lowercase();
            if [".ttf", ".ttc", ".otf", ".otc"].iter().any(|e| lower.ends_with(e)) {
                candidates.push((file.to_string(), index));
            }
        }
    }
    #[cfg(target_os = "linux")]
    candidates.extend(
        [
            "/usr/share/fonts/google-noto-sans-cjk-vf-fonts/NotoSansCJK-VF.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        ]
        .map(|p| (p.to_string(), 0)),
    );
    #[cfg(windows)]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        for f in ["msyh.ttc", "msyh.ttf", "msjh.ttc", "YuGothR.ttc", "malgun.ttf", "simsun.ttc"] {
            candidates.push((format!("{windir}\\Fonts\\{f}"), 0));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend(
        [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
            "/Library/Fonts/Arial Unicode.ttf",
        ]
        .map(|p| (p.to_string(), 0)),
    );
    candidates.into_iter().find_map(|(p, i)| std::fs::read(&p).ok().map(|b| (b, i)))
}

// ---------------------------------------------------------------- palette --

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    surface: Color32,
    hover: Color32,
    selected: Color32,
    accent: Color32,
    on_accent: Color32,
    border: Color32,
    text: Color32,
    dim: Color32,
    faint: Color32,
    danger: Color32,
    ok: Color32,
    dark: bool,
}

impl Palette {
    /// Raycast-like: neutral grays, translucent white/black overlays for
    /// hover and selection, accent only for small highlights.
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                bg: Color32::from_rgba_unmultiplied(30, 30, 33, 250),
                surface: Color32::from_rgb(40, 40, 44),
                hover: Color32::from_rgba_unmultiplied(255, 255, 255, 12),
                selected: Color32::from_rgba_unmultiplied(255, 255, 255, 24),
                accent: Color32::from_rgb(255, 99, 99),
                on_accent: Color32::WHITE,
                border: Color32::from_rgba_unmultiplied(255, 255, 255, 22),
                text: Color32::from_rgb(238, 238, 240),
                dim: Color32::from_rgb(160, 160, 166),
                faint: Color32::from_rgb(112, 112, 120),
                danger: Color32::from_rgb(255, 110, 110),
                ok: Color32::from_rgb(90, 205, 135),
                dark,
            }
        } else {
            Self {
                bg: Color32::from_rgba_unmultiplied(251, 251, 252, 252),
                surface: Color32::WHITE,
                hover: Color32::from_rgba_unmultiplied(0, 0, 0, 10),
                selected: Color32::from_rgba_unmultiplied(0, 0, 0, 20),
                accent: Color32::from_rgb(232, 64, 64),
                on_accent: Color32::WHITE,
                border: Color32::from_rgba_unmultiplied(0, 0, 0, 24),
                text: Color32::from_rgb(28, 28, 30),
                dim: Color32::from_rgb(100, 100, 108),
                faint: Color32::from_rgb(150, 150, 158),
                danger: Color32::from_rgb(210, 50, 50),
                ok: Color32::from_rgb(22, 150, 80),
                dark,
            }
        }
    }

    /// Accent color of a content type (icon tiles, chips).
    fn tint(&self, k: TextKind) -> Color32 {
        let (l, d) = match k {
            TextKind::Url => ((37, 99, 235), (96, 165, 250)),
            TextKind::Email => ((14, 116, 144), (34, 211, 238)),
            TextKind::Code => ((5, 150, 105), (52, 211, 153)),
            TextKind::Json => ((217, 119, 6), (251, 191, 36)),
            TextKind::Path => ((124, 58, 237), (167, 139, 250)),
            TextKind::Color => ((219, 39, 119), (244, 114, 182)),
            TextKind::Image => ((13, 148, 136), (45, 212, 191)),
            TextKind::Plain => ((100, 100, 112), (160, 161, 175)),
        };
        let (r, g, b) = if self.dark { d } else { l };
        Color32::from_rgb(r, g, b)
    }
}

// ---------------------------------------------------------- content types --

fn color32(c: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
}

/// Prepared content of the selected item for the preview pane.
enum Detail {
    Text { kind: TextKind, body: String },
    Image { tex: Option<TextureHandle> },
    Missing,
}

// -------------------------------------------------------------------- app --

struct App {
    cfg: Config,
    client: Option<Client>,
    error: Option<String>,
    notice: Option<(String, Instant)>,
    query: String,
    groups: Groups,
    groups_error: Option<String>,
    group: usize,
    counts: Vec<Option<u32>>,
    items: Vec<ItemMeta>,
    selected: usize,
    dirty: bool,
    paused: bool,
    autostart: bool,
    thumbs: HashMap<u64, Option<TextureHandle>>,
    detail: Option<(u64, Detail)>,
    had_focus: bool,
    scroll_to_selected: bool,
    scroll_offset: f32,
    view_height: f32,
    regex: bool,
    search_error: Option<String>,
    ocr_available: bool,
    sync: Option<SyncInfo>,
    sync_open: bool,
    sync_polled: Option<Instant>,
    sync_input: String,
    /// (text, is_error)
    sync_msg: Option<(String, bool)>,
    /// Code + addresses of the pairing window we opened.
    sync_pairing: Option<(String, Vec<String>)>,
    sync_job: Option<mpsc::Receiver<Result<String, String>>>,
    editor: Option<Editor>,
    /// Rows of the list (sections + items) with their y offsets.
    lines: Vec<(f32, Line)>,
    item_y: Vec<f32>,
    list_height: f32,
    /// Action panel (⌘K) open, with the selected action.
    actions: Option<usize>,
    /// Primary button clicked in the action bar (paste?).
    pending_activate: Option<bool>,
    /// Quit button clicked.
    pending_quit: bool,
    /// Local time zone offset in seconds.
    tz: i64,
    frames: u64,
    /// Keyboard goes to the search box (otherwise to the list).
    search_mode: bool,
    /// Set by the popup host when the hotkey asks the window to close.
    close_req: Arc<AtomicBool>,
    after: Arc<Mutex<AfterClose>>,
}

#[derive(Clone, Copy)]
enum Line {
    Section(&'static str),
    Item(usize),
}

#[derive(Clone)]
enum Act {
    Paste,
    Copy,
    Pin,
    OpenUrl(String),
    OpenPath(String),
    Delete,
    Pause,
    Sync,
    Groups,
    Autostart,
    Settings,
    Quit,
}

enum PanelRow {
    Title(String),
    Act(Act, String, String),
}

/// State of the "manage groups" dialog.
struct Editor {
    draft: Groups,
    sel: usize,
    patterns: String,
    apps: String,
    count: Option<Option<u32>>,
    count_dirty: bool,
    error: Option<String>,
}

impl Editor {
    fn new(groups: &Groups, sel: usize) -> Self {
        let mut e = Editor {
            draft: groups.clone(),
            sel,
            patterns: String::new(),
            apps: String::new(),
            count: None,
            count_dirty: true,
            error: None,
        };
        e.load_bufs();
        e
    }

    fn load_bufs(&mut self) {
        if let Some(g) = self.draft.list.get(self.sel) {
            self.patterns = g.patterns.join("\n");
            self.apps = g.apps.join("\n");
        }
        self.count_dirty = true;
    }

    fn store_bufs(&mut self) {
        let lines = |s: &str| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect::<Vec<_>>();
        if let Some(g) = self.draft.list.get_mut(self.sel) {
            g.patterns = lines(&self.patterns);
            g.apps = lines(&self.apps);
        }
    }

    fn select(&mut self, i: usize) {
        self.store_bufs();
        self.sel = i.min(self.draft.list.len().saturating_sub(1));
        self.load_bufs();
    }
}

enum Action {
    /// (index, paste into the previous window)
    Activate(usize, bool),
    Delete(usize),
    Pin(usize),
}

impl App {
    fn new(cfg: Config, after: Arc<Mutex<AfterClose>>, close_req: Arc<AtomicBool>) -> Self {
        let (groups, groups_error) = groups::load();
        let group = groups.default_index();
        let mut app = Self {
            cfg,
            client: None,
            error: None,
            notice: None,
            query: String::new(),
            groups,
            groups_error,
            group,
            counts: Vec::new(),
            items: Vec::new(),
            selected: 0,
            dirty: true,
            paused: false,
            autostart: crate::autostart::is_enabled(),
            thumbs: HashMap::new(),
            detail: None,
            had_focus: false,
            scroll_to_selected: false,
            scroll_offset: 0.0,
            view_height: 0.0,
            regex: false,
            search_error: None,
            ocr_available: false,
            sync: None,
            sync_open: false,
            sync_polled: None,
            sync_input: String::new(),
            sync_msg: None,
            sync_pairing: None,
            sync_job: None,
            editor: None,
            lines: Vec::new(),
            item_y: Vec::new(),
            list_height: 0.0,
            actions: None,
            pending_activate: None,
            pending_quit: false,
            tz: local_offset(),
            frames: 0,
            search_mode: false,
            close_req,
            after,
        };
        app.search_mode = app.cfg.popup_focus == "search";
        app.connect();
        if let Some(Response::Status { paused, ocr, .. }) = app.req(&Request::Status) {
            app.paused = paused;
            app.ocr_available = ocr.is_some();
        }
        app.refresh_sync();
        app
    }

    fn connect(&mut self) {
        match Client::connect() {
            Ok(c) => {
                self.client = Some(c);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("无法连接 copym 守护进程：{e}")),
        }
    }

    fn req(&mut self, r: &Request) -> Option<Response> {
        if self.client.is_none() {
            self.connect();
        }
        let c = self.client.as_mut()?;
        match c.request(r) {
            Ok(Response::Err(e)) => {
                self.error = Some(e);
                None
            }
            Ok(resp) => Some(resp),
            Err(e) => {
                self.client = None;
                self.error = Some(format!("守护进程连接中断：{e}"));
                None
            }
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
    }

    fn refresh(&mut self) {
        self.dirty = false;
        let rule = self.groups.rule(self.group);
        let group = (rule != GroupRule::default()).then_some(rule);
        let req = Request::ListGroup { query: self.query.clone(), regex: self.regex, group, limit: LIST_LIMIT };
        self.search_error = None;
        match self.req(&req) {
            Some(Response::Items(items)) => {
                self.items = items;
                self.selected = self.selected.min(self.items.len().saturating_sub(1));
                self.scroll_to_selected = true;
                self.layout_list();
            }
            // An invalid regex is a search problem, not a connection problem.
            _ if self.client.is_some() => {
                self.search_error = self.error.take();
                self.items.clear();
            }
            _ => {}
        }
        let counts = Request::GroupCounts { query: self.query.clone(), regex: self.regex, groups: self.groups.rules() };
        match self.req(&counts) {
            Some(Response::Counts(c)) => self.counts = c,
            _ => {
                self.counts.clear();
                if self.client.is_some() {
                    self.error = None; // already reported as search_error
                }
            }
        }
    }

    /// Split the list into sections: pinned, today, yesterday, this week, older.
    fn layout_list(&mut self) {
        let today = (now_secs() + self.tz).div_euclid(86_400);
        let mut lines = Vec::with_capacity(self.items.len() + 5);
        let mut item_y = Vec::with_capacity(self.items.len());
        let mut y = 4.0;
        let mut last = "";
        for (i, m) in self.items.iter().enumerate() {
            let sec = if m.pinned {
                "已固定"
            } else {
                match today - (m.created + self.tz).div_euclid(86_400) {
                    ..=0 => "今天",
                    1 => "昨天",
                    2..=6 => "本周",
                    _ => "更早",
                }
            };
            if sec != last {
                lines.push((y, Line::Section(sec)));
                y += SECTION_H;
                last = sec;
            }
            lines.push((y, Line::Item(i)));
            item_y.push(y);
            y += ROW_H;
        }
        self.lines = lines;
        self.item_y = item_y;
        self.list_height = y + 6.0;
    }

    fn set_group(&mut self, i: usize) {
        if i < self.groups.list.len() && i != self.group {
            self.group = i;
            self.selected = 0;
            self.dirty = true;
        }
    }

    fn thumb(&mut self, ctx: &egui::Context, id: u64) -> Option<TextureHandle> {
        if let Some(t) = self.thumbs.get(&id) {
            return t.clone();
        }
        let tex = match self.req(&Request::Thumb { id }) {
            Some(Response::Thumb(Some(png))) => imgutil::decode_png(&png).map(|(w, h, rgba)| {
                let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                ctx.load_texture(format!("thumb-{id}"), img, TextureOptions::LINEAR)
            }),
            _ => None,
        };
        self.thumbs.insert(id, tex.clone());
        tex
    }

    /// Full content of an item, prepared for display (cached for the selection).
    fn detail(&mut self, ctx: &egui::Context, id: u64) -> &Detail {
        if self.detail.as_ref().is_none_or(|(d, _)| *d != id) {
            let d = match self.req(&Request::Get { id }) {
                Some(Response::Content(Some(Content::Text(t)))) => {
                    let kind = classify(&t, true);
                    let body = match kind {
                        TextKind::Json => kinds::pretty_json(&t).unwrap_or(t),
                        _ => t,
                    };
                    Detail::Text { kind, body }
                }
                Some(Response::Content(Some(Content::Image { png, .. }))) => {
                    let tex = imgutil::decode_png(&png).map(|(w, h, rgba)| {
                        let (w, h, rgba) = if w.max(h) > PREVIEW_MAX_SIDE {
                            imgutil::thumbnail(w, h, &rgba, PREVIEW_MAX_SIDE)
                        } else {
                            (w, h, rgba)
                        };
                        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                        ctx.load_texture(format!("full-{id}"), img, TextureOptions::LINEAR)
                    });
                    Detail::Image { tex }
                }
                _ => Detail::Missing,
            };
            self.detail = Some((id, d));
        }
        &self.detail.as_ref().unwrap().1
    }

    fn perform(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Activate(i, paste) => {
                if let Some(id) = self.items.get(i).map(|m| m.id) {
                    if self.req(&Request::Activate { id }).is_some() {
                        self.after.lock().unwrap().paste = paste;
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                }
            }
            Action::Delete(i) => {
                if let Some(id) = self.items.get(i).map(|m| m.id) {
                    self.req(&Request::Delete { id });
                    self.thumbs.remove(&id);
                    self.dirty = true;
                }
            }
            Action::Pin(i) => {
                if let Some(id) = self.items.get(i).map(|m| m.id) {
                    self.req(&Request::TogglePin { id });
                    self.dirty = true;
                }
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let max = self.items.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
        self.scroll_to_selected = true;
    }

    fn handle_keys(&mut self, ctx: &egui::Context) -> Option<Action> {
        if self.sync_open || self.editor.is_some() {
            return None; // dialogs own the keyboard
        }
        if self.actions.is_some() {
            return self.panel_keys(ctx);
        }
        let mut action = None;
        let mut close = false;
        let mut nav = 0isize;
        let mut cycle = 0isize;
        let mut group = None;
        let mut toggle_regex = false;
        let mut open_actions = false;
        let mut search = false;
        let mut quit = false;
        let list_mode = !self.search_mode;
        let page = ((self.view_height / ROW_H).floor() as isize).max(1);
        let last = self.items.len() as isize;
        let paste = self.cfg.paste_on_enter;
        ctx.input_mut(|i| {
            // Action panel: ⌘K on macOS, Alt+K elsewhere (Ctrl+K is Vim "up").
            #[cfg(target_os = "macos")]
            let actions_key = i.consume_key(Modifiers::COMMAND, Key::K);
            #[cfg(not(target_os = "macos"))]
            let actions_key = i.consume_key(Modifiers::ALT, Key::K);
            if actions_key {
                open_actions = true;
                i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
            }
            if i.consume_key(Modifiers::COMMAND, Key::Q) {
                quit = true;
            }
            // Down / up: arrows, Vim (Ctrl+J / Ctrl+K) and Emacs (Ctrl+N / Ctrl+P).
            // Plain Ctrl - on macOS ⌘ stays free for the app's own shortcuts.
            for k in [Key::ArrowDown, Key::J, Key::N] {
                let m = if k == Key::ArrowDown { Modifiers::NONE } else { Modifiers::CTRL };
                if i.consume_key(m, k) {
                    nav = 1;
                }
            }
            for k in [Key::ArrowUp, Key::K, Key::P] {
                let m = if k == Key::ArrowUp { Modifiers::NONE } else { Modifiers::CTRL };
                if i.consume_key(m, k) {
                    nav = -1;
                }
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) {
                nav = page;
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                nav = -page;
            }
            // Shift+Enter first: matching ignores extra Shift.
            if i.consume_key(Modifiers::SHIFT, Key::Enter) {
                action = Some(Action::Activate(self.selected, !paste));
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                action = Some(Action::Activate(self.selected, paste));
            }
            if i.consume_key(Modifiers::COMMAND, Key::S) {
                action = Some(Action::Pin(self.selected));
            }
            if i.consume_key(Modifiers::SHIFT, Key::Delete) || i.consume_key(Modifiers::COMMAND, Key::D) {
                action = Some(Action::Delete(self.selected));
            }
            if i.consume_key(Modifiers::SHIFT, Key::Tab) {
                cycle = -1;
            }
            if i.consume_key(Modifiers::NONE, Key::Tab) {
                cycle = 1;
            }
            if i.consume_key(Modifiers::COMMAND, Key::R) || i.consume_key(Modifiers::ALT, Key::R) {
                toggle_regex = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::F) {
                search = true;
            }
            let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            let mut swallow_text = false;
            for (n, k) in digits.iter().enumerate() {
                if i.consume_key(Modifiers::COMMAND, *k) {
                    group = Some(n);
                }
                if i.consume_key(Modifiers::ALT, *k) || (list_mode && i.consume_key(Modifiers::NONE, *k)) {
                    action = Some(Action::Activate(n, paste));
                    swallow_text = true;
                }
            }
            if list_mode {
                // Vim-style keys while the list has the keyboard.
                if i.consume_key(Modifiers::NONE, Key::J) {
                    nav = 1;
                }
                if i.consume_key(Modifiers::NONE, Key::K) {
                    nav = -1;
                }
                if i.consume_key(Modifiers::NONE, Key::End) || i.consume_key(Modifiers::SHIFT, Key::G) {
                    nav = last;
                }
                if i.consume_key(Modifiers::NONE, Key::Home) || i.consume_key(Modifiers::NONE, Key::G) {
                    nav = -last;
                }
                if i.consume_key(Modifiers::NONE, Key::Slash) || i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "/")) {
                    search = true;
                }
                // Nothing is typed into the search box until it is focused.
                swallow_text = true;
            }
            if swallow_text {
                // ⌥1 also types "¡" on macOS - keep it out of the search box.
                i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
        });
        if quit {
            self.quit(ctx);
            return None;
        }
        if open_actions {
            self.actions = Some(0);
        }
        if nav != 0 {
            self.move_selection(nav);
        }
        if toggle_regex {
            self.regex = !self.regex;
            self.dirty = true;
        }
        if let Some(g) = group {
            self.set_group(g);
        }
        if cycle != 0 && !self.groups.list.is_empty() {
            let n = self.groups.list.len() as isize;
            self.set_group(((self.group as isize + cycle).rem_euclid(n)) as usize);
        }
        if search {
            self.search_mode = true;
        }
        if close {
            if self.search_mode {
                // Esc leaves the search box (and clears it); a second Esc closes.
                if !self.query.is_empty() {
                    self.query.clear();
                    self.dirty = true;
                }
                if self.cfg.popup_focus == "search" && self.query.is_empty() && !self.dirty {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                } else {
                    self.search_mode = false;
                    ctx.memory_mut(|m| m.surrender_focus(egui::Id::unique("search")));
                }
            } else if !self.query.is_empty() {
                self.query.clear();
                self.dirty = true;
            } else {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
        action
    }

    /// Stop copym (daemon and popup).
    fn quit(&mut self, ctx: &egui::Context) {
        let _ = self.req(&Request::Quit);
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    /// Rows of the action panel for the selected item.
    fn panel_rows(&self) -> Vec<PanelRow> {
        let mut rows = Vec::new();
        let cmd = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };
        let enter = if cfg!(target_os = "macos") { "⏎" } else { "Enter" };
        if let Some(m) = self.items.get(self.selected) {
            let title = match m.kind {
                Kind::Text => one_line(&m.preview),
                Kind::Image => format!("图片 {}×{}", m.width, m.height),
            };
            rows.push(PanelRow::Title(title));
            let shift_enter = if cfg!(target_os = "macos") { "⇧⏎" } else { "Shift+Enter" };
            let (paste_key, copy_key) = if self.cfg.paste_on_enter { (enter, shift_enter) } else { (shift_enter, enter) };
            rows.push(PanelRow::Act(Act::Paste, "粘贴到当前窗口".into(), paste_key.into()));
            rows.push(PanelRow::Act(Act::Copy, "复制到剪贴板".into(), copy_key.into()));
            if let Some((id, Detail::Text { kind, body })) = &self.detail {
                if *id == m.id {
                    match kind {
                        TextKind::Url => rows.push(PanelRow::Act(Act::OpenUrl(body.trim().into()), "在浏览器中打开".into(), String::new())),
                        TextKind::Path => rows.push(PanelRow::Act(Act::OpenPath(body.trim().into()), "在文件管理器中打开".into(), String::new())),
                        _ => {}
                    }
                }
            }
            rows.push(PanelRow::Act(Act::Pin, if m.pinned { "取消固定" } else { "固定" }.into(), format!("{cmd}S")));
            rows.push(PanelRow::Act(Act::Delete, "删除".into(), format!("{cmd}D")));
        }
        rows.push(PanelRow::Title("copym".into()));
        rows.push(PanelRow::Act(Act::Pause, if self.paused { "继续记录剪贴板" } else { "暂停记录剪贴板" }.into(), String::new()));
        rows.push(PanelRow::Act(Act::Sync, "局域网同步…".into(), String::new()));
        rows.push(PanelRow::Act(Act::Groups, "管理分组…".into(), String::new()));
        rows.push(PanelRow::Act(Act::Autostart, if self.autostart { "关闭登录时自动启动" } else { "登录时自动启动" }.into(), String::new()));
        rows.push(PanelRow::Act(Act::Settings, "打开设置文件".into(), String::new()));
        rows.push(PanelRow::Act(Act::Quit, "退出 copym".into(), format!("{cmd}Q")));
        rows
    }

    fn panel_acts(&self) -> Vec<Act> {
        self.panel_rows().into_iter().filter_map(|r| if let PanelRow::Act(a, ..) = r { Some(a) } else { None }).collect()
    }

    fn panel_keys(&mut self, ctx: &egui::Context) -> Option<Action> {
        let n = self.panel_acts().len();
        let mut sel = self.actions.unwrap_or(0);
        let mut run = false;
        let mut close = false;
        ctx.input_mut(|i| {
            for (k, m, d) in [
                (Key::ArrowDown, Modifiers::NONE, 1isize),
                (Key::J, Modifiers::CTRL, 1),
                (Key::N, Modifiers::CTRL, 1),
                (Key::ArrowUp, Modifiers::NONE, -1),
                (Key::K, Modifiers::CTRL, -1),
                (Key::P, Modifiers::CTRL, -1),
            ] {
                if i.consume_key(m, k) {
                    sel = (sel as isize + d).rem_euclid(n.max(1) as isize) as usize;
                }
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                run = true;
            }
            #[cfg(target_os = "macos")]
            let toggle = i.consume_key(Modifiers::COMMAND, Key::K);
            #[cfg(not(target_os = "macos"))]
            let toggle = i.consume_key(Modifiers::ALT, Key::K);
            if toggle || i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
            i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
        });
        self.actions = Some(sel);
        if close {
            self.actions = None;
        }
        if run {
            let act = self.panel_acts().get(sel).cloned();
            self.actions = None;
            return act.and_then(|a| self.run_act(ctx, a));
        }
        None
    }

    fn run_act(&mut self, _ctx: &egui::Context, act: Act) -> Option<Action> {
        match act {
            Act::Paste => return Some(Action::Activate(self.selected, true)),
            Act::Copy => return Some(Action::Activate(self.selected, false)),
            Act::Quit => self.pending_quit = true,
            Act::Pin => return Some(Action::Pin(self.selected)),
            Act::Delete => return Some(Action::Delete(self.selected)),
            Act::OpenUrl(u) => open_external(&u),
            Act::OpenPath(p) => {
                let path = std::path::Path::new(p.trim_start_matches("file://"));
                let dir = if path.is_dir() { Some(path) } else { path.parent().filter(|d| d.is_dir()) };
                match dir {
                    Some(d) => open_external(&d.to_string_lossy()),
                    None => self.notify("本机上不存在这个路径"),
                }
            }
            Act::Pause => {
                if let Some(Response::Status { paused, .. }) = self.req(&Request::SetPaused(None)) {
                    self.paused = paused;
                    self.notify(if paused { "已暂停记录" } else { "继续记录" });
                }
            }
            Act::Sync => {
                self.sync_open = true;
                self.sync_msg = None;
                self.refresh_sync();
            }
            Act::Groups => self.editor = Some(Editor::new(&self.groups, self.group)),
            Act::Autostart => match crate::autostart::set(!self.autostart) {
                Ok(_) => {
                    self.autostart = !self.autostart;
                    self.notify(if self.autostart { "已设为登录时自动启动" } else { "已关闭登录时自动启动" });
                }
                Err(e) => self.notify(format!("设置失败：{e}")),
            },
            Act::Settings => open_external(&crate::config::config_path().to_string_lossy()),
        }
        None
    }

    // ------------------------------------------------------------- views --

    /// Big borderless search field, regex toggle and group dropdown, with a
    /// full-width hairline below (Raycast style).
    fn header(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), SEARCH_H), Sense::hover());
        let inner = Rect::from_min_max(Pos2::new(rect.left() + 46.0, rect.top()), Pos2::new(rect.right() - 12.0, rect.bottom()));
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            self.group_dropdown(ui, pal);
            let on = self.regex;
            let b = ui
                .add(
                    egui::Button::new(RichText::new(".*").monospace().size(12.5).color(if on { pal.text } else { pal.faint }))
                        .fill(if on { pal.selected } else { Color32::TRANSPARENT })
                        .stroke(Stroke::NONE)
                        .corner_radius(6)
                        .min_size(Vec2::new(30.0, 26.0)),
                )
                .on_hover_text(format!("正则表达式搜索 ({KEY_GROUP}R)"));
            if b.clicked() {
                self.regex = !self.regex;
                self.dirty = true;
            }
            let hint = if self.regex { "正则，例如 \\d{6} 或 ^https?://" } else { "搜索剪贴板历史…" };
            let edit = egui::TextEdit::singleline(&mut self.query)
                .hint_text(RichText::new(hint).color(pal.faint))
                .desired_width(ui.available_width())
                .font(FontId::proportional(17.0))
                .text_color(pal.text)
                .frame(egui::Frame::NONE)
                .margin(Margin::symmetric(0, 4));
            let r = ui.add(edit.id(egui::Id::unique("search")));
            if r.changed() {
                self.selected = 0;
                self.dirty = true;
            }
            if r.has_focus() || r.clicked() {
                self.search_mode = true;
            }
            if self.search_mode && !self.sync_open && self.editor.is_none() && self.actions.is_none() {
                r.request_focus();
            }
        });
        // magnifier
        let c = Pos2::new(rect.left() + 24.0, rect.center().y - 1.0);
        let p = ui.painter();
        let color = if self.search_error.is_some() { pal.danger } else { pal.faint };
        p.circle_stroke(c, 6.0, Stroke::new(1.7, color));
        p.line_segment([c + Vec2::new(4.4, 4.4), c + Vec2::new(8.5, 8.5)], Stroke::new(1.9, color));
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, pal.border));
    }

    /// "全部 ⌄" button opening the list of groups (like Raycast's type filter).
    fn group_dropdown(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let name = self.groups.list.get(self.group).map(|g| g.name.clone()).unwrap_or_default();
        let resp = ui
            .add(
                egui::Button::new(RichText::new(format!("{name}    ")).size(13.0).color(pal.dim))
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::new(1.0, pal.border))
                    .corner_radius(7)
                    .min_size(Vec2::new(0.0, 28.0)),
            )
            .on_hover_text(format!("分组（{KEY_GROUP}1…9 / Tab 切换）"));
        // chevron
        let r = resp.rect;
        let c = Pos2::new(r.right() - 14.0, r.center().y);
        ui.painter().line(vec![c + Vec2::new(-3.5, -1.5), c + Vec2::new(0.0, 2.0), c + Vec2::new(3.5, -1.5)], Stroke::new(1.4, pal.faint));

        let mut chosen = None;
        let mut manage = false;
        let mut make_default = None;
        // Close only on a click outside: a right click on a row opens its own
        // context menu ("设为默认分组") and must not close the list.
        egui::Popup::menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, g) in self.groups.list.iter().enumerate() {
                let (row, hover) = ui.allocate_exact_size(Vec2::new(230.0, 28.0), Sense::click());
                let fill = if i == self.group { pal.selected } else if hover.hovered() { pal.hover } else { Color32::TRANSPARENT };
                let p = ui.painter();
                p.rect_filled(row, 6, fill);
                let label = if self.groups.default == g.name { format!("{}  · 默认", g.name) } else { g.name.clone() };
                p.text(row.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(13.0), pal.text);
                let mut right = match self.counts.get(i) {
                    Some(Some(n)) => n.to_string(),
                    Some(None) => "规则无效".into(),
                    None => String::new(),
                };
                if i < 9 {
                    right = format!("{right}   {KEY_GROUP}{}", i + 1);
                }
                p.text(row.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, right, FontId::proportional(11.5), pal.faint);
                if hover.clicked() {
                    chosen = Some(i);
                    ui.close();
                }
                // Right click: make it the group shown when the popup opens. (A nested
                // context menu would replace this popup - egui keeps one open.)
                if hover.secondary_clicked() {
                    make_default = Some(i);
                }
            }
            ui.separator();
            let (row, hover) = ui.allocate_exact_size(Vec2::new(230.0, 28.0), Sense::click());
            if hover.hovered() {
                ui.painter().rect_filled(row, 6, pal.hover);
            }
            ui.painter().text(row.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, "管理分组…", FontId::proportional(13.0), pal.text);
            ui.painter().text(row.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, "右键：设为默认", FontId::proportional(11.0), pal.faint);
            if hover.clicked() {
                manage = true;
                ui.close();
            }
        });
        if let Some(i) = chosen {
            self.set_group(i);
        }
        if manage {
            self.editor = Some(Editor::new(&self.groups, self.group));
        }
        if let Some(i) = make_default {
            self.groups.default = self.groups.list[i].name.clone();
            match groups::save(&self.groups) {
                Ok(()) => self.notify(format!("打开时将默认显示「{}」", self.groups.default)),
                Err(e) => self.notify(format!("保存失败：{e}")),
            }
        }
    }

    fn list(&mut self, ui: &mut egui::Ui, pal: &Palette) -> Option<Action> {
        let mut action = None;
        if let Some(err) = &self.search_error {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(RichText::new(err).color(pal.danger).size(12.5));
            });
            return None;
        }
        if self.items.is_empty() {
            ui.centered_and_justified(|ui| {
                let msg = if !self.query.is_empty() {
                    "没有匹配的条目"
                } else if self.group == 0 {
                    "剪贴板历史为空\n复制点什么试试吧"
                } else {
                    "这个分组里还没有内容"
                };
                ui.label(RichText::new(msg).color(pal.faint).size(13.5));
            });
            return None;
        }
        let ctx = ui.ctx().clone();
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("list");
        if self.scroll_to_selected {
            if let Some(&y) = self.item_y.get(self.selected) {
                // keep the section header of the first item visible too
                let top = if self.lines.iter().any(|(ly, l)| matches!(l, Line::Section(_)) && (y - ly - SECTION_H).abs() < 0.5) { y - SECTION_H } else { y };
                let bottom = y + ROW_H;
                if top < self.scroll_offset {
                    area = area.vertical_scroll_offset(top);
                } else if bottom > self.scroll_offset + self.view_height && self.view_height > 0.0 {
                    area = area.vertical_scroll_offset(bottom - self.view_height + 4.0);
                }
            }
            self.scroll_to_selected = false;
        }
        let lines = self.lines.clone();
        let total = self.list_height;
        let out = area.show_viewport(ui, |ui, viewport| {
            let width = ui.available_width();
            let origin = ui.max_rect().min;
            ui.set_height(total);
            for (y, line) in lines {
                let h = if matches!(line, Line::Section(_)) { SECTION_H } else { ROW_H };
                if y + h < viewport.min.y || y > viewport.max.y {
                    continue;
                }
                let rect = Rect::from_min_size(origin + Vec2::new(0.0, y), Vec2::new(width, h));
                match line {
                    Line::Section(name) => {
                        ui.painter().text(
                            Pos2::new(rect.left() + 14.0, rect.bottom() - 9.0),
                            Align2::LEFT_BOTTOM,
                            name,
                            FontId::proportional(11.5),
                            pal.faint,
                        );
                    }
                    Line::Item(i) => {
                        let meta = self.items[i].clone();
                        let row = rect.shrink2(Vec2::new(6.0, 1.0));
                        let resp = ui.interact(row, egui::Id::unique(("row", meta.id)), Sense::click());
                        let selected = i == self.selected;
                        let fill = if selected {
                            pal.selected
                        } else if resp.hovered() {
                            pal.hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        ui.painter().rect_filled(row, 8, fill);
                        let kind = if meta.kind == Kind::Image { TextKind::Image } else { classify(&meta.preview, false) };
                        let tile = Rect::from_center_size(Pos2::new(row.left() + 20.0, row.center().y), Vec2::splat(22.0));
                        self.paint_tile(ui, &ctx, pal, tile, &meta, kind);

                        // accessories on the right: pinned dot + time
                        let painter = ui.painter();
                        let time = painter.text(
                            Pos2::new(row.right() - 10.0, row.center().y),
                            Align2::RIGHT_CENTER,
                            short_time(meta.created, self.tz),
                            FontId::proportional(11.5),
                            pal.faint,
                        );
                        let mut right = time.left() - 10.0;
                        if meta.pinned {
                            painter.circle_filled(Pos2::new(right - 2.0, row.center().y), 3.0, pal.accent);
                            right -= 12.0;
                        }
                        let x = tile.right() + 10.0;
                        let title = match meta.kind {
                            Kind::Text => one_line(&meta.preview),
                            Kind::Image => meta
                                .ocr
                                .as_deref()
                                .map(one_line)
                                .filter(|t| !t.is_empty() && t != "(空白)")
                                .unwrap_or_else(|| format!("图片 {}×{}", meta.width, meta.height)),
                        };
                        let mut job = LayoutJob::single_section(title, TextFormat { font_id: FontId::proportional(13.5), color: pal.text, ..Default::default() });
                        job.wrap.max_width = (right - x).max(30.0);
                        job.wrap.max_rows = 1;
                        job.wrap.break_anywhere = true;
                        job.wrap.overflow_character = Some('…');
                        let galley = painter.layout_job(job);
                        let gy = row.center().y - galley.size().y / 2.0;
                        painter.galley(Pos2::new(x, gy), galley, pal.text);

                        if resp.clicked() {
                            self.selected = i;
                        }
                        if resp.double_clicked() {
                            action = Some(Action::Activate(i, self.cfg.paste_on_enter));
                        }
                        if resp.secondary_clicked() {
                            self.selected = i;
                            self.actions = Some(0);
                        }
                    }
                }
            }
        });
        self.scroll_offset = out.state.clamped_offset().y;
        self.view_height = out.inner_rect.height();
        action
    }

    fn paint_tile(&mut self, ui: &egui::Ui, ctx: &egui::Context, pal: &Palette, tile: Rect, meta: &ItemMeta, kind: TextKind) {
        if meta.kind == Kind::Image {
            if let Some(tex) = self.thumb(ctx, meta.id) {
                let ts = tex.size_vec2();
                // center-crop to a square
                let uv = if ts.x > ts.y {
                    let f = ts.y / ts.x;
                    Rect::from_min_max(Pos2::new((1.0 - f) / 2.0, 0.0), Pos2::new((1.0 + f) / 2.0, 1.0))
                } else {
                    let f = ts.x / ts.y;
                    Rect::from_min_max(Pos2::new(0.0, (1.0 - f) / 2.0), Pos2::new(1.0, (1.0 + f) / 2.0))
                };
                egui::Image::from_texture((tex.id(), tile.size())).uv(uv).corner_radius(5).paint_at(ui, tile);
                ui.painter().rect_stroke(tile, 5, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
                return;
            }
        }
        let p = ui.painter();
        if kind == TextKind::Color {
            if let Some(c) = kinds::parse_color(&meta.preview).map(color32) {
                p.rect_filled(tile, 5, c);
                p.rect_stroke(tile, 5, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
                return;
            }
        }
        let tint = pal.tint(kind);
        p.rect_filled(tile, 5, tint.gamma_multiply(if pal.dark { 0.22 } else { 0.14 }));
        let size = if kind.glyph().chars().count() > 2 { 8.5 } else { 11.0 };
        p.text(tile.center(), Align2::CENTER_CENTER, kind.glyph(), FontId::proportional(size), tint);
    }

    /// Detail pane: content on top, metadata table below (Raycast style).
    fn preview(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let Some(meta) = self.items.get(self.selected).cloned() else { return };
        let ctx = ui.ctx().clone();
        let kind = match self.detail(&ctx, meta.id) {
            Detail::Text { kind, .. } => *kind,
            Detail::Image { .. } => TextKind::Image,
            Detail::Missing => TextKind::Plain,
        };
        let mut rows: Vec<(&str, String)> = vec![("类型", kind.label().to_string()), ("来源", meta.source.clone().unwrap_or_else(|| "本机".into()))];
        rows.push(match meta.kind {
            Kind::Text => ("内容", format!("{} 字符 · {} 行", meta.chars, meta.lines)),
            Kind::Image => ("尺寸", format!("{} × {}", meta.width, meta.height)),
        });
        rows.push(("大小", human_size(meta.bytes)));
        rows.push(("复制时间", abs_time(meta.created, self.tz)));
        let meta_h = 30.0 + rows.len() as f32 * 25.0;

        egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 14, bottom: 8 }).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.spacing_mut().item_spacing.y = 0.0; // rows below are measured exactly
            let body = Vec2::new(ui.available_width(), (ui.available_height() - meta_h).max(60.0));
            ui.allocate_ui_with_layout(body, Layout::top_down(Align::LEFT), |ui| {
                ui.set_min_size(body);
                ui.set_max_height(body.y);
                ui.spacing_mut().item_spacing.y = 6.0;
                self.preview_body(ui, pal, &meta, kind);
            });
            // metadata
            let w = ui.available_width();
            let (title, _) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::hover());
            ui.painter().hline(title.x_range(), title.top() + 6.0, Stroke::new(1.0, pal.border));
            ui.painter().text(Pos2::new(title.left(), title.bottom() - 4.0), Align2::LEFT_BOTTOM, "信息", FontId::proportional(11.5), pal.faint);
            for (k, v) in rows {
                let (r, _) = ui.allocate_exact_size(Vec2::new(w, 25.0), Sense::hover());
                let p = ui.painter();
                p.text(r.left_center(), Align2::LEFT_CENTER, k, FontId::proportional(12.0), pal.dim);
                p.text(r.right_center(), Align2::RIGHT_CENTER, v, FontId::proportional(12.0), pal.text);
                p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, pal.border.gamma_multiply(0.6)));
            }
        });
    }

    fn preview_body(&mut self, ui: &mut egui::Ui, pal: &Palette, meta: &ItemMeta, kind: TextKind) {
        let id = meta.id;
        let Some((_, detail)) = &self.detail else { return };
        match detail {
            Detail::Missing => {
                ui.label(RichText::new("无法读取内容").color(pal.danger));
            }
            Detail::Image { tex } => {
                let ocr_text = meta.ocr.clone().filter(|t| !t.trim().is_empty());
                let full = ui.available_size();
                let img_h = if ocr_text.is_some() { (full.y * 0.66).floor() } else { full.y - 28.0 };
                if let Some(tex) = tex {
                    let ts = tex.size_vec2();
                    let s = (full.x / ts.x).min(img_h / ts.y).min(1.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::new(full.x, img_h), Sense::hover());
                    let img = Rect::from_center_size(r.center(), ts * s);
                    // checkerboard for transparent images
                    paint_checker(ui.painter(), img, pal);
                    egui::Image::from_texture((tex.id(), img.size())).corner_radius(6).paint_at(ui, img);
                }
                match (&ocr_text, &meta.ocr) {
                    (Some(t), _) => {
                        ui.add_space(6.0);
                        ui.label(RichText::new("图中文字").size(11.5).strong().color(pal.dim));
                        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(("ocr", id)).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(t).size(12.5).color(pal.text)).wrap().selectable(true));
                        });
                    }
                    (None, None) if self.ocr_available => {
                        ui.label(RichText::new("正在识别图中文字…").size(11.5).color(pal.faint));
                    }
                    (None, None) => {
                        ui.label(RichText::new("未启用图片文字识别").size(11.5).color(pal.faint));
                    }
                    _ => {
                        ui.label(RichText::new("图中未识别到文字").size(11.5).color(pal.faint));
                    }
                }
            }
            Detail::Text { body, .. } => {
                let body = body.clone();
                match kind {
                    TextKind::Color => color_preview(ui, pal, &body),
                    TextKind::Url => url_preview(ui, pal, body.trim()),
                    TextKind::Path => {
                        ui.add(egui::Label::new(RichText::new(body.trim()).font(FontId::monospace(13.0)).color(pal.text)).wrap().selectable(true));
                        ui.add_space(8.0);
                        let p = std::path::Path::new(body.trim().trim_start_matches("file://"));
                        let target = if p.is_dir() { Some(p) } else { p.parent().filter(|d| d.is_dir()) };
                        match target {
                            Some(dir) => {
                                if ui.button("在文件管理器中打开").clicked() {
                                    open_external(&dir.to_string_lossy());
                                }
                            }
                            None => {
                                ui.label(RichText::new("本机上不存在这个路径").size(11.5).color(pal.faint));
                            }
                        }
                    }
                    _ => {
                        let mono = matches!(kind, TextKind::Json | TextKind::Code);
                        let shown: String = body.chars().take(50_000).collect();
                        egui::ScrollArea::both().auto_shrink([false, false]).id_salt(("preview", id)).show(ui, |ui| {
                            let text = if mono {
                                RichText::new(shown).font(FontId::monospace(12.5))
                            } else {
                                RichText::new(shown).size(13.5)
                            };
                            let label = egui::Label::new(text.color(pal.text)).selectable(true);
                            ui.add(if mono { label.extend() } else { label.wrap() });
                        });
                    }
                }
            }
        }
    }

    fn refresh_sync(&mut self) {
        if let Some(Response::Sync(s)) = self.req(&Request::SyncStatus) {
            if let Some((code, _)) = &self.sync_pairing {
                if s.pairing_code.as_deref() != Some(code.as_str()) {
                    // The pairing window closed (success, failure or timeout).
                    self.sync_pairing = None;
                    let before = self.sync.as_ref().map(|o| o.peers.len()).unwrap_or(0);
                    self.sync_msg = Some(if s.peers.len() > before {
                        ("配对成功，已开始同步".into(), false)
                    } else {
                        ("配对窗口已关闭".into(), false)
                    });
                }
            }
            self.sync = Some(s);
        }
        self.sync_polled = Some(Instant::now());
    }

    fn sync_panel(&mut self, ctx: &egui::Context, pal: &Palette) {
        // Poll status while the dialog is open.
        if self.sync_polled.is_none_or(|t| t.elapsed() > Duration::from_millis(1500)) {
            self.refresh_sync();
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        if let Some(rx) = &self.sync_job {
            if let Ok(res) = rx.try_recv() {
                self.sync_job = None;
                self.sync_msg = Some(match res {
                    Ok(name) => (format!("已与 {name} 配对，开始同步"), false),
                    Err(e) => (e, true),
                });
                self.sync_input.clear();
                self.refresh_sync();
            }
        }
        let info = self.sync.clone();
        let modal = egui::Modal::new(egui::Id::unique("sync-panel"))
            .frame(dialog_frame(pal))
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(RichText::new("局域网同步").size(16.0).strong().color(pal.text));
                ui.label(RichText::new("端到端加密 · 仅同一局域网 · 收到的内容只进入历史，不会改动剪贴板").size(11.5).color(pal.faint));
                ui.add_space(8.0);
                let Some(info) = info else {
                    ui.label(RichText::new("无法获取同步状态").color(pal.danger));
                    return;
                };
                if !info.enabled {
                    ui.label(RichText::new("同步已在设置中关闭（sync_enabled = false）").color(pal.dim));
                    return;
                }
                section(ui, pal, |ui| {
                    ui.label(RichText::new(format!("本机：{}", info.device_name)).color(pal.text));
                    if info.paired {
                        if info.peers.is_empty() {
                            ui.label(RichText::new("暂无设备在线（已配对的设备在同一局域网时会自动连接）").size(12.0).color(pal.faint));
                        } else {
                            for (name, addr) in &info.peers {
                                ui.horizontal(|ui| {
                                    let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                    ui.painter().circle_filled(r.center(), 4.0, pal.ok);
                                    ui.label(RichText::new(name).size(13.0).color(pal.text));
                                    ui.label(RichText::new(addr).size(11.5).color(pal.faint));
                                });
                            }
                        }
                    } else {
                        ui.label(RichText::new("尚未与其他设备配对").size(12.0).color(pal.faint));
                    }
                });
                ui.add_space(8.0);

                // Invite another device.
                if let Some((code, addrs)) = self.sync_pairing.clone() {
                    ui.label(RichText::new("在另一台设备的 copym 里输入配对码：").size(12.5).color(pal.dim));
                    ui.label(RichText::new(&code).monospace().size(30.0).strong().color(pal.accent));
                    if let Some(a) = addrs.first() {
                        ui.label(RichText::new(format!("找不到本机时，输入 {code}@{a}")).size(11.5).color(pal.faint));
                    }
                    ui.label(RichText::new("3 分钟内有效，只能尝试一次").size(11.5).color(pal.faint));
                } else if ui.add(primary_button(pal, "添加设备（生成配对码）")).clicked() {
                    match self.req(&Request::SyncPair) {
                        Some(Response::Pairing { code, addrs, .. }) => {
                            self.sync_pairing = Some((code, addrs));
                            self.sync_msg = None;
                        }
                        _ => self.sync_msg = Some((self.error.take().unwrap_or_else(|| "无法开始配对".into()), true)),
                    }
                    self.refresh_sync();
                }
                ui.add_space(10.0);

                // Join another device.
                ui.label(RichText::new("加入另一台设备").size(12.5).color(pal.dim));
                ui.horizontal(|ui| {
                    let busy = self.sync_job.is_some();
                    ui.add_enabled(
                        !busy,
                        egui::TextEdit::singleline(&mut self.sync_input).hint_text("8 位配对码，可附 @IP:端口").desired_width(270.0).margin(Margin::symmetric(8, 5)),
                    );
                    let label = if busy { "正在配对…" } else { "加入" };
                    if ui.add_enabled(!busy && !self.sync_input.trim().is_empty(), egui::Button::new(label)).clicked() {
                        let input = self.sync_input.trim().to_string();
                        let (code, addr) = match input.split_once('@') {
                            Some((c, a)) => (c.to_string(), Some(a.to_string())),
                            None => (input, None),
                        };
                        let (tx, rx) = mpsc::channel();
                        self.sync_job = Some(rx);
                        std::thread::spawn(move || {
                            let res = Client::connect()
                                .map_err(|e| e.to_string())
                                .and_then(|mut c| c.request(&Request::SyncJoin { code, addr }).map_err(|e| e.to_string()))
                                .and_then(|r| match r {
                                    Response::Joined { name } => Ok(name),
                                    Response::Err(e) => Err(e),
                                    _ => Err("unexpected response".into()),
                                });
                            let _ = tx.send(res);
                        });
                    }
                });

                if let Some((msg, err)) = &self.sync_msg {
                    ui.add_space(4.0);
                    ui.label(RichText::new(msg).size(12.5).color(if *err { pal.danger } else { pal.ok }));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if info.paired
                        && ui.add(egui::Button::new(RichText::new("退出同步组").color(pal.danger))).on_hover_text("忘记同步密钥并断开所有设备").clicked()
                    {
                        self.req(&Request::SyncLeave);
                        self.sync_pairing = None;
                        self.sync_msg = Some(("已退出同步组".into(), false));
                        self.refresh_sync();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("关闭").clicked() {
                            self.sync_open = false;
                        }
                    });
                });
            });
        if modal.should_close() {
            self.sync_open = false;
        }
    }

    fn group_editor(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(mut ed) = self.editor.take() else { return };
        if ed.count_dirty {
            ed.count_dirty = false;
            ed.store_bufs();
            let rule = ed.draft.rule(ed.sel);
            ed.count = match self.req(&Request::GroupCounts { query: String::new(), regex: false, groups: vec![rule] }) {
                Some(Response::Counts(c)) => c.into_iter().next(),
                _ => None,
            };
        }
        let mut keep = true;
        let mut saved = false;
        const BODY_H: f32 = 296.0;
        let modal = egui::Modal::new(egui::Id::unique("group-editor")).frame(dialog_frame(pal).inner_margin(Margin::same(16))).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.spacing_mut().item_spacing.y = 5.0;
            ui.label(RichText::new("管理分组").size(16.0).strong().color(pal.text));
            ui.label(
                RichText::new(format!("分组按顺序对应 {KEY_GROUP}1…9；条目可以同时属于多个分组。规则保存在 groups.toml。"))
                    .size(11.5)
                    .color(pal.faint),
            );
            ui.add_space(8.0);
            ui.horizontal_top(|ui| {
                // ---- left: list of groups
                ui.allocate_ui_with_layout(Vec2::new(150.0, BODY_H), Layout::top_down(Align::LEFT), |ui| {
                    ui.set_min_size(Vec2::new(150.0, BODY_H));
                    let mut select = None;
                    egui::ScrollArea::vertical().id_salt("ged-list").max_height(BODY_H - 70.0).show(ui, |ui| {
                        ui.with_layout(Layout::top_down_justified(Align::LEFT), |ui| {
                            ui.set_width(146.0);
                            for (i, g) in ed.draft.list.iter().enumerate() {
                                let mut label = if i < 9 { format!("{}   {}", i + 1, g.name) } else { format!("     {}", g.name) };
                                if ed.draft.default == g.name {
                                    label.push_str("  · 默认");
                                }
                                if ui.add(egui::Button::selectable(i == ed.sel, label).min_size(Vec2::new(146.0, 26.0))).clicked() {
                                    select = Some(i);
                                }
                            }
                        });
                    });
                    if let Some(i) = select {
                        ed.select(i);
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if ui.small_button("＋ 新建").clicked() {
                            ed.store_bufs();
                            let n = ed.draft.list.len() + 1;
                            ed.draft.list.push(GroupDef { name: format!("分组 {n}"), ..Default::default() });
                            ed.sel = ed.draft.list.len() - 1;
                            ed.load_bufs();
                        }
                        let can_delete = ed.draft.list.len() > 1;
                        if ui.add_enabled(can_delete, egui::Button::new("删除").small()).clicked() {
                            ed.draft.list.remove(ed.sel);
                            ed.sel = ed.sel.min(ed.draft.list.len() - 1);
                            ed.load_bufs();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if ui.add_enabled(ed.sel > 0, egui::Button::new("上移").small()).clicked() {
                            ed.store_bufs();
                            ed.draft.list.swap(ed.sel, ed.sel - 1);
                            ed.sel -= 1;
                        }
                        if ui.add_enabled(ed.sel + 1 < ed.draft.list.len(), egui::Button::new("下移").small()).clicked() {
                            ed.store_bufs();
                            ed.draft.list.swap(ed.sel, ed.sel + 1);
                            ed.sel += 1;
                        }
                    });
                });
                let (line, _) = ui.allocate_exact_size(Vec2::new(1.0, BODY_H), Sense::hover());
                ui.painter().rect_filled(line, 0, pal.border);
                ui.add_space(8.0);
                // ---- right: the selected group
                ui.vertical(|ui| {
                    let sel = ed.sel;
                    let default_name = ed.draft.default.clone();
                    let Some(g) = ed.draft.list.get_mut(sel) else { return };
                    let mut changed = false;
                    let old_name = g.name.clone();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("名称").size(12.0).color(pal.dim));
                        changed |= ui.add(egui::TextEdit::singleline(&mut g.name).desired_width(f32::INFINITY).margin(Margin::symmetric(8, 4))).changed();
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("内容类型").size(12.0).color(pal.dim));
                        for (v, l) in [("", "全部"), ("text", "文本"), ("image", "图片")] {
                            changed |= ui.selectable_value(&mut g.kind, v.to_string(), l).changed();
                        }
                    });
                    changed |= ui.checkbox(&mut g.pinned, "只包含固定的条目").changed();
                    changed |= ui
                        .checkbox(&mut g.rest, "“其他”分组：不属于任何带规则分组的条目")
                        .on_hover_text("排除其他所有分组的内容正则和来源应用正则所匹配的条目")
                        .changed();
                    ui.add_space(4.0);
                    ui.label(RichText::new("内容正则（每行一条，匹配任意一条即可；(?i) 忽略大小写）").size(12.0).color(pal.dim));
                    changed |= ui
                        .add(
                            egui::TextEdit::multiline(&mut ed.patterns)
                                .font(FontId::monospace(12.5))
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text(r"例：^\s*https?://"),
                        )
                        .changed();
                    ui.label(RichText::new("来源正则：来源应用或同步设备名（每行一条，可留空）").size(12.0).color(pal.dim));
                    changed |= ui
                        .add(
                            egui::TextEdit::multiline(&mut ed.apps)
                                .font(FontId::monospace(12.5))
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("例：(?i)chrome|firefox"),
                        )
                        .changed();
                    let g = &ed.draft.list[sel];
                    let mut is_default = default_name == g.name;
                    if ui.checkbox(&mut is_default, "打开弹窗时默认显示这个分组").changed() && is_default {
                        ed.draft.default = g.name.clone();
                    }
                    if ed.draft.default == old_name && old_name != ed.draft.list[sel].name {
                        ed.draft.default = ed.draft.list[sel].name.clone(); // follow renames
                    }
                    if changed {
                        ed.count_dirty = true;
                        ed.error = None;
                    }
                    ui.add_space(4.0);
                    let probe = GroupDef { patterns: ed.patterns.lines().map(String::from).collect(), apps: ed.apps.lines().map(String::from).collect(), ..ed.draft.list[sel].clone() };
                    match probe.validate() {
                        Err(e) => {
                            ui.label(RichText::new(e).size(12.0).color(pal.danger));
                        }
                        Ok(()) => {
                            let txt = match ed.count {
                                Some(Some(n)) => format!("当前历史中有 {n} 条属于这个分组"),
                                _ => String::new(),
                            };
                            ui.label(RichText::new(txt).size(12.0).color(pal.ok));
                        }
                    }
                });
            });
            if let Some(e) = &ed.error {
                ui.label(RichText::new(e).size(12.0).color(pal.danger));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("恢复默认分组").clicked() {
                    ed.draft = Groups::default();
                    ed.sel = 0;
                    ed.load_bufs();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(primary_button(pal, "保存")).clicked() {
                        ed.store_bufs();
                        let mut names = std::collections::HashSet::new();
                        let err = ed
                            .draft
                            .list
                            .iter()
                            .find_map(|g| g.validate().err().map(|e| format!("「{}」：{e}", g.name)))
                            .or_else(|| ed.draft.list.iter().find(|g| !names.insert(g.name.trim().to_string())).map(|g| format!("分组名称重复：{}", g.name)));
                        match err {
                            Some(e) => ed.error = Some(e),
                            None => match groups::save(&ed.draft) {
                                Ok(()) => saved = true,
                                Err(e) => ed.error = Some(format!("保存失败：{e}")),
                            },
                        }
                    }
                    if ui.button("取消").clicked() {
                        keep = false;
                    }
                });
            });
        });
        if modal.should_close() {
            keep = false;
        }
        if saved {
            let current = self.groups.list.get(self.group).map(|g| g.name.clone());
            self.groups = ed.draft;
            self.groups_error = None;
            self.group = current.and_then(|n| self.groups.list.iter().position(|g| g.name == n)).unwrap_or(0);
            self.dirty = true;
            self.notify("分组已保存");
        } else if keep {
            self.editor = Some(ed);
        }
    }

    /// Bottom action bar: context on the left, primary action and the action
    /// panel shortcut on the right.
    fn footer(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), FOOTER_H), Sense::hover());
        let p = ui.painter();
        p.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, pal.border));
        // left: app badge + context
        let badge = Rect::from_center_size(Pos2::new(rect.left() + 22.0, rect.center().y), Vec2::splat(18.0));
        p.rect_filled(badge, 5, pal.accent);
        p.text(badge.center(), Align2::CENTER_CENTER, "c", FontId::proportional(12.0), pal.on_accent);
        let mut ctx_text = format!("剪贴板历史  ·  {}", self.groups.list.get(self.group).map(|g| g.name.as_str()).unwrap_or(""));
        ctx_text.push_str(&format!("  ·  {} 条", self.items.len()));
        if self.paused {
            ctx_text.push_str("  ·  已暂停");
        }
        let left = p.text(Pos2::new(badge.right() + 8.0, rect.center().y), Align2::LEFT_CENTER, ctx_text, FontId::proportional(12.0), pal.dim);
        if let Some(sync) = self.sync.as_ref().filter(|s| s.paired) {
            let n = sync.peers.len();
            let dot = Pos2::new(left.right() + 14.0, rect.center().y);
            p.circle_filled(dot, 3.0, if n > 0 { pal.ok } else { pal.faint });
            p.text(dot + Vec2::new(7.0, 0.0), Align2::LEFT_CENTER, format!("同步 {n} 台"), FontId::proportional(12.0), pal.dim);
        }

        // right: "粘贴 ⏎ | 操作 ⌘K | ⏻"
        let inner = rect.shrink2(Vec2::new(10.0, 6.0));
        let mut open_actions = false;
        let mut copy = false;
        let mut quit = false;
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let quit_key = if cfg!(target_os = "macos") { "⌘Q" } else { "Ctrl+Q" };
            if power_button(ui, pal).on_hover_text(format!("退出 copym（{quit_key}）")).clicked() {
                quit = true;
            }
            let (sep, _) = ui.allocate_exact_size(Vec2::new(9.0, 16.0), Sense::hover());
            ui.painter().vline(sep.center().x, sep.y_range(), Stroke::new(1.0, pal.border));
            if bar_button(ui, pal, "操作", KEY_ACTIONS, false).clicked() {
                open_actions = true;
            }
            let (sep, _) = ui.allocate_exact_size(Vec2::new(9.0, 16.0), Sense::hover());
            ui.painter().vline(sep.center().x, sep.y_range(), Stroke::new(1.0, pal.border));
            let enter = if cfg!(target_os = "macos") { "⏎" } else { "Enter" };
            let label = if self.cfg.paste_on_enter { "粘贴" } else { "复制到剪贴板" };
            if !self.items.is_empty() && bar_button(ui, pal, label, enter, true).clicked() {
                copy = true;
            }
        });
        if open_actions {
            self.actions = if self.actions.is_some() { None } else { Some(0) };
        }
        if copy {
            self.pending_activate = Some(self.cfg.paste_on_enter);
        }
        if quit {
            self.pending_quit = true;
        }
    }

    /// ⌘K panel floating above the bottom-right corner.
    fn action_panel(&mut self, ctx: &egui::Context, pal: &Palette) -> Option<Action> {
        let sel = self.actions?;
        let rows = self.panel_rows();
        let screen = ctx.content_rect();
        let mut clicked = None;
        let mut hovered = None;
        let area = egui::Area::new(egui::Id::unique("action-panel"))
            .order(egui::Order::Foreground)
            .pivot(Align2::RIGHT_BOTTOM)
            .fixed_pos(Pos2::new(screen.right() - 10.0, screen.bottom() - FOOTER_H - 6.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(pal.surface)
                    .stroke(Stroke::new(1.0, pal.border))
                    .corner_radius(12)
                    .inner_margin(Margin::same(6))
                    .shadow(egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if pal.dark { 120 } else { 50 }) })
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        ui.spacing_mut().item_spacing.y = 1.0;
                        let mut idx = 0;
                        for (n, row) in rows.iter().enumerate() {
                            match row {
                                PanelRow::Title(t) => {
                                    if n > 0 {
                                        let (r, _) = ui.allocate_exact_size(Vec2::new(300.0, 9.0), Sense::hover());
                                        ui.painter().hline(r.x_range(), r.center().y, Stroke::new(1.0, pal.border));
                                    }
                                    let (r, _) = ui.allocate_exact_size(Vec2::new(300.0, 24.0), Sense::hover());
                                    let mut job = LayoutJob::single_section(t.clone(), TextFormat { font_id: FontId::proportional(11.5), color: pal.faint, ..Default::default() });
                                    job.wrap.max_width = 280.0;
                                    job.wrap.max_rows = 1;
                                    job.wrap.break_anywhere = true;
                                    job.wrap.overflow_character = Some('…');
                                    let g = ui.painter().layout_job(job);
                                    ui.painter().galley(Pos2::new(r.left() + 10.0, r.center().y - g.size().y / 2.0), g, pal.faint);
                                }
                                PanelRow::Act(_, label, keys) => {
                                    let (r, resp) = ui.allocate_exact_size(Vec2::new(300.0, 32.0), Sense::click());
                                    if idx == sel {
                                        ui.painter().rect_filled(r, 7, pal.selected);
                                    }
                                    let danger = label == "删除";
                                    ui.painter().text(
                                        r.left_center() + Vec2::new(10.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        label,
                                        FontId::proportional(13.5),
                                        if danger { pal.danger } else { pal.text },
                                    );
                                    if !keys.is_empty() {
                                        paint_keycap(ui.painter(), pal, r.right_center() - Vec2::new(10.0, 0.0), keys);
                                    }
                                    if resp.hovered() && ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
                                        hovered = Some(idx);
                                    }
                                    if resp.clicked() {
                                        clicked = Some(idx);
                                    }
                                    idx += 1;
                                }
                            }
                        }
                    });
            });
        if let Some(h) = hovered {
            self.actions = Some(h);
        }
        if let Some(c) = clicked {
            let act = self.panel_acts().get(c).cloned();
            self.actions = None;
            return act.and_then(|a| self.run_act(ctx, a));
        }
        // click outside closes
        if ctx.input(|i| i.pointer.any_click()) && !area.response.contains_pointer() {
            let over_bar = ctx.input(|i| i.pointer.interact_pos()).is_some_and(|p| p.y > screen.bottom() - FOOTER_H);
            if !over_bar {
                self.actions = None;
            }
        }
        None
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if self.close_req.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        // Close when focus moves elsewhere (like a native popup).
        if let Some(focused) = ctx.input(|i| i.viewport().focused) {
            if focused {
                self.had_focus = true;
            } else if self.had_focus && self.cfg.close_on_focus_lost {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }

        let mut action = self.handle_keys(&ctx);
        if self.dirty {
            self.refresh();
        }
        let pal = Palette::new(ui.visuals().dark_mode);
        ctx.all_styles_mut(|s| {
            s.visuals.selection.bg_fill = pal.accent.gamma_multiply(0.35);
            s.visuals.selection.stroke = Stroke::new(1.0, pal.accent);
            s.visuals.extreme_bg_color = if pal.dark { Color32::from_rgb(28, 28, 31) } else { Color32::from_rgb(244, 244, 246) };
            s.visuals.window_fill = pal.surface;
            s.visuals.panel_fill = pal.bg;
            s.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, pal.border);
            s.visuals.widgets.inactive.weak_bg_fill = pal.hover;
            s.visuals.widgets.hovered.weak_bg_fill = pal.selected;
            s.visuals.widgets.inactive.fg_stroke.color = pal.text;
            s.visuals.widgets.noninteractive.fg_stroke.color = pal.text;
        });

        egui::Frame::new()
            .fill(pal.bg)
            .stroke(Stroke::new(1.0, pal.border))
            .corner_radius(CornerRadius::same(12))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                // Drag the borderless window by any empty area.
                let bg = ui.interact(ui.max_rect(), ui.scope_id().with("window-drag"), Sense::click_and_drag());
                if bg.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
                self.header(ui, &pal);
                let banner = self.error.clone().or_else(|| self.groups_error.clone());
                if let Some(err) = banner {
                    egui::Frame::new().inner_margin(Margin::symmetric(14, 6)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.label(RichText::new(err).color(pal.danger).size(12.0));
                            if self.error.is_some() && ui.small_button("重试").clicked() {
                                self.error = None;
                                self.connect();
                                self.dirty = true;
                            }
                        });
                    });
                }
                let body_h = (ui.available_height() - FOOTER_H).max(80.0);
                let w = ui.available_width();
                ui.allocate_ui_with_layout(Vec2::new(w, body_h), Layout::left_to_right(Align::Min), |ui| {
                    let list_w = (w * 0.42).floor();
                    ui.allocate_ui_with_layout(Vec2::new(list_w, body_h), Layout::top_down(Align::LEFT), |ui| {
                        ui.set_min_size(Vec2::new(list_w, body_h));
                        ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                        if let Some(a) = self.list(ui, &pal) {
                            action = Some(a);
                        }
                    });
                    let (line, _) = ui.allocate_exact_size(Vec2::new(1.0, body_h), Sense::hover());
                    ui.painter().vline(line.center().x, line.y_range(), Stroke::new(1.0, pal.border));
                    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), body_h), Layout::top_down(Align::LEFT), |ui| {
                        ui.set_min_size(Vec2::new(ui.available_width(), body_h));
                        ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                        self.preview(ui, &pal);
                    });
                });
                self.footer(ui, &pal);
            });

        if let Some(a) = self.action_panel(&ctx, &pal) {
            action = Some(a);
        }
        if let Some(paste) = self.pending_activate.take() {
            action = Some(Action::Activate(self.selected, paste));
        }
        if std::mem::take(&mut self.pending_quit) {
            self.quit(&ctx);
        }

        // transient notice (e.g. "saved")
        if let Some((msg, at)) = &self.notice {
            if at.elapsed() < Duration::from_millis(2200) {
                let screen = ctx.content_rect();
                egui::Area::new(egui::Id::unique("notice"))
                    .order(egui::Order::Tooltip)
                    .pivot(Align2::CENTER_BOTTOM)
                    .fixed_pos(Pos2::new(screen.center().x, screen.bottom() - FOOTER_H - 10.0))
                    .show(&ctx, |ui| {
                        egui::Frame::new().fill(pal.surface).stroke(Stroke::new(1.0, pal.border)).corner_radius(8).inner_margin(Margin::symmetric(12, 6)).show(ui, |ui| {
                            ui.label(RichText::new(msg).size(12.5).color(pal.text));
                        });
                    });
                ctx.request_repaint_after(Duration::from_millis(200));
            } else {
                self.notice = None;
            }
        }

        // Popups/modals measure themselves in an invisible first pass; make sure
        // the first few frames are actually painted.
        self.frames += 1;
        if self.frames <= 3 {
            trace(&format!("frame {}", self.frames));
        }
        if self.frames < 4 {
            ctx.request_repaint();
        }
        if self.sync_open {
            self.sync_panel(&ctx, &pal);
        }
        if self.editor.is_some() {
            self.group_editor(&ctx, &pal);
        }

        if let Some(a) = action {
            self.perform(&ctx, a);
        }
    }
}

// ---------------------------------------------------------------- widgets --

fn dialog_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.surface)
        .corner_radius(14)
        .stroke(Stroke::new(1.0, pal.border))
        .inner_margin(Margin::same(18))
        .shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(60) })
}

fn section(ui: &mut egui::Ui, pal: &Palette, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(pal.bg).corner_radius(10).inner_margin(Margin::same(10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui)
    });
}

fn primary_button<'a>(pal: &Palette, text: &'a str) -> egui::Button<'a> {
    egui::Button::new(RichText::new(text).color(pal.on_accent).strong()).fill(pal.accent).stroke(Stroke::NONE).corner_radius(8)
}

/// "label [key]" button of the action bar.
fn bar_button(ui: &mut egui::Ui, pal: &Palette, label: &str, key: &str, strong: bool) -> egui::Response {
    let font = FontId::proportional(12.5);
    let text_w = ui.painter().layout_no_wrap(label.to_string(), font.clone(), pal.text).size().x;
    let key_w = ui.painter().layout_no_wrap(key.to_string(), FontId::proportional(11.0), pal.dim).size().x + 10.0;
    let (r, resp) = ui.allocate_exact_size(Vec2::new(text_w + key_w + 22.0, 26.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, 6, pal.hover);
    }
    ui.painter().text(r.left_center() + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, label, font, if strong { pal.text } else { pal.dim });
    paint_keycap(ui.painter(), pal, r.right_center() - Vec2::new(6.0, 0.0), key);
    resp
}

/// Square icon button with a power symbol (quit).
fn power_button(ui: &mut egui::Ui, pal: &Palette) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
    let color = if resp.hovered() { pal.danger } else { pal.faint };
    if resp.hovered() {
        ui.painter().rect_filled(r, 6, pal.hover);
    }
    let c = r.center();
    let radius = 6.0;
    // arc with a gap at the top, plus the vertical bar
    let points: Vec<Pos2> = (0..=24)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + 0.75 + i as f32 / 24.0 * (std::f32::consts::TAU - 1.5);
            c + Vec2::new(a.cos(), a.sin()) * radius
        })
        .collect();
    let stroke = Stroke::new(1.6, color);
    ui.painter().add(egui::Shape::line(points, stroke));
    ui.painter().line_segment([c + Vec2::new(0.0, -radius - 1.0), c + Vec2::new(0.0, -1.0)], stroke);
    resp
}

/// Small rounded key label, right-aligned at `right_center`.
fn paint_keycap(p: &egui::Painter, pal: &Palette, right_center: Pos2, key: &str) {
    let g = p.layout_no_wrap(key.to_string(), FontId::proportional(11.0), pal.dim);
    let size = Vec2::new(g.size().x + 10.0, 20.0);
    let r = Rect::from_min_size(Pos2::new(right_center.x - size.x, right_center.y - size.y / 2.0), size);
    p.rect_filled(r, 5, if pal.dark { Color32::from_rgba_unmultiplied(255, 255, 255, 18) } else { Color32::from_rgba_unmultiplied(0, 0, 0, 14) });
    p.galley(Pos2::new(r.left() + 5.0, r.center().y - g.size().y / 2.0), g, pal.dim);
}

#[allow(dead_code)]
fn keycap(ui: &mut egui::Ui, pal: &Palette, key: &str) {
    egui::Frame::new()
        .fill(pal.surface)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(4)
        .inner_margin(Margin::symmetric(5, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(key).size(10.5).color(pal.dim));
        });
}

fn paint_checker(p: &egui::Painter, r: Rect, pal: &Palette) {
    let (a, b) = if pal.dark {
        (Color32::from_gray(48), Color32::from_gray(58))
    } else {
        (Color32::from_gray(236), Color32::from_gray(250))
    };
    p.rect_filled(r, 6, a);
    let s = 8.0;
    let p = p.with_clip_rect(r);
    let mut y = r.top();
    let mut row = 0;
    while y < r.bottom() {
        let mut x = r.left() + if row % 2 == 0 { 0.0 } else { s };
        while x < r.right() {
            p.rect_filled(Rect::from_min_size(Pos2::new(x, y), Vec2::splat(s)), 0, b);
            x += 2.0 * s;
        }
        y += s;
        row += 1;
    }
}

fn color_preview(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    let Some(rgba) = kinds::parse_color(text) else { return };
    let c = color32(rgba);
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, (ui.available_height() * 0.5).clamp(80.0, 180.0)), Sense::hover());
    paint_checker(ui.painter(), r, pal);
    ui.painter().rect_filled(r, 10, c);
    ui.painter().rect_stroke(r, 10, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
    ui.add_space(10.0);
    egui::Grid::new("color-values").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
        for (k, v) in kinds::color_notations(rgba) {
            ui.label(RichText::new(k).size(11.5).color(pal.faint));
            ui.add(egui::Label::new(RichText::new(v).font(FontId::monospace(13.0)).color(pal.text)).selectable(true));
            ui.end_row();
        }
    });
}

fn url_preview(ui: &mut egui::Ui, pal: &Palette, url: &str) {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after.split(['/', '?', '#']).next().unwrap_or(after);
    let path = &after[host.len()..];
    ui.add_space(6.0);
    ui.label(RichText::new(host).size(19.0).strong().color(pal.text));
    if !path.is_empty() && path != "/" {
        ui.add(egui::Label::new(RichText::new(path).font(FontId::monospace(12.0)).color(pal.dim)).wrap());
    }
    ui.add_space(10.0);
    egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(ui.available_height() - 44.0).show(ui, |ui| {
        ui.add(egui::Label::new(RichText::new(url).font(FontId::monospace(12.0)).color(pal.faint)).wrap().selectable(true));
    });
    ui.add_space(10.0);
    if ui.add(primary_button(pal, "在浏览器中打开")).clicked() {
        open_external(url);
    }
}

// ---------------------------------------------------------------- helpers --

/// `COPYM_TRACE=1`: print startup timings (ms since process start) to stderr.
fn trace(what: &str) {
    if std::env::var_os("COPYM_TRACE").is_some() {
        eprintln!("[{:>6.1} ms] {what}", crate::START.get().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0));
    }
}

fn open_external(target: &str) {
    #[cfg(target_os = "linux")]
    let r = std::process::Command::new("xdg-open").arg(target).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(target).spawn();
    #[cfg(windows)]
    let r = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", target]).spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let r: std::io::Result<()> = Err(std::io::Error::other("unsupported"));
    if let Err(e) = r {
        eprintln!("copym: cannot open {target}: {e}");
    }
}

/// First non-blank line, tabs expanded, trimmed.
fn one_line(s: &str) -> String {
    let s: String = s.chars().take(300).collect();
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.is_empty() { "(空白)".into() } else { line.replace('\t', "  ") }
}

/// Local time zone offset (seconds east of UTC).
fn local_offset() -> i64 {
    #[cfg(unix)]
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&now, &mut tm).is_null() {
            return tm.tm_gmtoff as i64;
        }
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        let mut tz: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let r = GetTimeZoneInformation(&mut tz);
        let bias = tz.Bias + if r == 2 { tz.DaylightBias } else { tz.StandardBias };
        return -(bias as i64) * 60;
    }
    #[allow(unreachable_code)]
    0
}

/// (year, month, day, hour, minute) of a unix time in the local zone.
fn civil(t: i64, tz: i64) -> (i64, u32, u32, u32, u32) {
    let secs = t + tz;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Howard Hinnant's days -> civil algorithm
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (sod / 3600) as u32, (sod % 3600 / 60) as u32)
}

fn abs_time(t: i64, tz: i64) -> String {
    let (y, mo, d, h, mi) = civil(t, tz);
    format!("{y}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// Compact time for list accessories: 刚刚 / 5 分钟 / 3 小时 / 昨天 / 4 天 / 11-02.
fn short_time(t: i64, tz: i64) -> String {
    let now = now_secs();
    let d = (now - t).max(0);
    let days = (now + tz).div_euclid(86_400) - (t + tz).div_euclid(86_400);
    match (d, days) {
        (0..=59, _) => "刚刚".into(),
        (60..=3599, _) => format!("{} 分钟", d / 60),
        (_, 0) => format!("{} 小时", d / 3600),
        (_, 1) => "昨天".into(),
        (_, 2..=6) => format!("{days} 天"),
        _ => {
            let (y, m, dd, ..) = civil(t, tz);
            let (ny, ..) = civil(now, tz);
            if y == ny { format!("{m:02}-{dd:02}") } else { format!("{y}-{m:02}-{dd:02}") }
        }
    }
}

#[allow(dead_code)]
fn relative_time(t: i64) -> String {
    let d = (now_secs() - t).max(0);
    match d {
        0..=59 => "刚刚".into(),
        60..=3599 => format!("{} 分钟前", d / 60),
        3600..=86_399 => format!("{} 小时前", d / 3600),
        86_400..=172_799 => "昨天".into(),
        _ if d < 30 * 86_400 => format!("{} 天前", d / 86_400),
        _ if d < 365 * 86_400 => format!("{} 个月前", d / (30 * 86_400)),
        _ => format!("{} 年前", d / (365 * 86_400)),
    }
}

fn human_size(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f < K {
        format!("{b} B")
    } else if f < K * K {
        format!("{:.1} KB", f / K)
    } else {
        format!("{:.1} MB", f / K / K)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a system font that has them (macOS), ⏎ ⇧ ⌥ used to render as boxes.
    #[test]
    fn key_symbols_have_glyphs() {
        let keys = "⌘⇧⌥⌃⏎⌫⎋↑↓";
        let check = |fonts: FontDefinitions| {
            let ctx = egui::Context::default();
            ctx.set_fonts(fonts);
            // no renderer here: drop the font-atlas upload explicitly
            ctx.run_ui(Default::default(), |_| {}).textures_delta.clear();
            ctx.fonts_mut(|f| f.has_glyphs(&FontId::proportional(13.0), keys))
        };
        assert!(!check(FontDefinitions::default()), "egui's own fonts lack them");
        let mut fonts = FontDefinitions::default();
        add_symbols(&mut fonts);
        assert!(check(fonts));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0, 0), (1970, 1, 1, 0, 0));
        assert_eq!(abs_time(1_795_416_115, 8 * 3600), "2026-11-23 14:41");
        assert_eq!(civil(951_782_400, 0), (2000, 2, 29, 0, 0)); // leap day
        assert_eq!(short_time(now_secs() - 5, 0), "刚刚");
        assert_eq!(short_time(now_secs() - 300, 0), "5 分钟");
    }
}
