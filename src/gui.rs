//! The popup window (egui / eframe, OpenGL renderer).
//!
//! It is a short-lived process: started by the daemon on hotkey, talks to the
//! daemon over IPC, and exits after an item is chosen. Nothing GUI related
//! stays in memory while the popup is closed.

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
use std::collections::HashMap;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const ROW_H: f32 = 52.0;
const ROW_GAP: f32 = 2.0;
const LIST_LIMIT: u32 = 1000;
const PREVIEW_MAX_SIDE: u32 = 2048;

#[cfg(target_os = "macos")]
const KEY_GROUP: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const KEY_GROUP: &str = "Ctrl+";
#[cfg(target_os = "macos")]
const KEY_PASTE: &str = "⌥";
#[cfg(not(target_os = "macos"))]
const KEY_PASTE: &str = "Alt+";

/// `open`: "" (normal), "sync" (sync dialog) or "groups" (group manager).
pub fn run(open: &str) -> Result<(), String> {
    let open = open.to_string();
    let cfg = Config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("copym")
            .with_app_id("copym")
            .with_inner_size([760.0, 470.0])
            .with_min_inner_size([560.0, 340.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_active(true),
        centered: true,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "copym",
        options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            match cfg.theme.as_str() {
                "dark" => cc.egui_ctx.set_theme(egui::ThemePreference::Dark),
                "light" => cc.egui_ctx.set_theme(egui::ThemePreference::Light),
                _ => cc.egui_ctx.set_theme(egui::ThemePreference::System),
            }
            cc.egui_ctx.all_styles_mut(|s| {
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
            let mut app = App::new(cfg);
            app.sync_open = open == "sync";
            if open == "groups" {
                app.editor = Some(Editor::new(&app.groups, app.group));
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ fonts --

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
    ctx.set_fonts(fonts);
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
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                bg: Color32::from_rgba_unmultiplied(24, 24, 28, 250),
                surface: Color32::from_rgb(33, 33, 39),
                hover: Color32::from_rgb(40, 40, 48),
                selected: Color32::from_rgb(45, 47, 74),
                accent: Color32::from_rgb(140, 150, 255),
                on_accent: Color32::from_rgb(18, 18, 30),
                border: Color32::from_rgba_unmultiplied(255, 255, 255, 20),
                text: Color32::from_rgb(236, 236, 241),
                dim: Color32::from_rgb(160, 161, 175),
                faint: Color32::from_rgb(108, 110, 124),
                danger: Color32::from_rgb(255, 125, 125),
                ok: Color32::from_rgb(92, 205, 140),
                dark,
            }
        } else {
            Self {
                bg: Color32::from_rgba_unmultiplied(251, 251, 253, 252),
                surface: Color32::WHITE,
                hover: Color32::from_rgb(241, 242, 246),
                selected: Color32::from_rgb(232, 234, 255),
                accent: Color32::from_rgb(79, 70, 229),
                on_accent: Color32::WHITE,
                border: Color32::from_rgba_unmultiplied(0, 0, 0, 22),
                text: Color32::from_rgb(24, 24, 27),
                dim: Color32::from_rgb(100, 100, 112),
                faint: Color32::from_rgb(158, 158, 170),
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TextKind {
    Plain,
    Url,
    Email,
    Path,
    Color,
    Json,
    Code,
    Image,
}

impl TextKind {
    fn label(self) -> &'static str {
        match self {
            TextKind::Plain => "文本",
            TextKind::Url => "链接",
            TextKind::Email => "邮箱",
            TextKind::Path => "路径",
            TextKind::Color => "颜色",
            TextKind::Json => "JSON",
            TextKind::Code => "代码",
            TextKind::Image => "图片",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            TextKind::Plain => "Aa",
            TextKind::Url => "↗",
            TextKind::Email => "@",
            TextKind::Path => "/",
            TextKind::Color => "#",
            TextKind::Json => "{ }",
            TextKind::Code => "</>",
            TextKind::Image => "▣",
        }
    }
}

/// Guess what a piece of text is. `complete` = the full text (JSON is then
/// actually parsed); otherwise a cheap guess on the preview.
fn classify(text: &str, complete: bool) -> TextKind {
    let s = text.trim();
    if s.is_empty() {
        return TextKind::Plain;
    }
    let single = !s.contains('\n');
    if single && parse_color(s).is_some() {
        return TextKind::Color;
    }
    if single && !s.contains(char::is_whitespace) {
        let l = s.to_ascii_lowercase();
        if ["http://", "https://", "ftp://"].iter().any(|p| l.starts_with(p)) && l.len() > 8 {
            return TextKind::Url;
        }
        if is_email(s) {
            return TextKind::Email;
        }
    }
    if single && looks_like_path(s) {
        return TextKind::Path;
    }
    if looks_like_json(s) && (!complete || serde_json::from_str::<serde_json::Value>(s).is_ok()) {
        return TextKind::Json;
    }
    if looks_like_code(s) {
        return TextKind::Code;
    }
    TextKind::Plain
}

fn is_email(s: &str) -> bool {
    let Some((user, host)) = s.split_once('@') else { return false };
    !user.is_empty() && host.contains('.') && !host.starts_with('.') && !host.ends_with('.') && !host.contains('@') && s.len() < 256
}

fn looks_like_path(s: &str) -> bool {
    if s.len() > 1024 || s.contains("  ") {
        return false;
    }
    let b = s.as_bytes();
    let unix = (s.starts_with('/') && !s.starts_with("//") && b.len() > 1 && !b[1].is_ascii_whitespace()) || s.starts_with("~/");
    let windows = b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    unix || windows || s.starts_with("file://")
}

fn looks_like_json(s: &str) -> bool {
    if !s.starts_with(['{', '[']) {
        return false;
    }
    let rest = s[1..].trim_start(); // '{' / '[' are one byte
    (s.starts_with('{') && (rest.starts_with('"') || rest.starts_with('}')))
        || (s.starts_with('[') && rest.starts_with(['{', '[', '"', ']', '-', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9']))
}

fn looks_like_code(s: &str) -> bool {
    const HINTS: [&str; 16] = [
        "=>", "->", "::", "fn ", "def ", "function ", "class ", "import ", "#include", "return ", "const ", "let ", "var ",
        "</", "public ", "func ",
    ];
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let score = lines
        .iter()
        .filter(|l| {
            let t = l.trim_end();
            t.ends_with(';')
                || t.ends_with('{')
                || t.trim_start().starts_with('}')
                || l.starts_with("    ")
                || l.starts_with('\t')
                || HINTS.iter().any(|h| t.contains(h))
        })
        .count();
    if lines.len() >= 2 {
        score * 2 >= lines.len()
    } else {
        let t = s.trim_end();
        t.len() < 300 && (t.ends_with(';') && t.contains('(') || t.contains("=>") || t.starts_with("$ ") || t.starts_with("sudo "))
    }
}

fn parse_color(s: &str) -> Option<Color32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix('#') {
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
        return match h.len() {
            3 => Some(Color32::from_rgb(v(0, 1)? * 17, v(1, 1)? * 17, v(2, 1)? * 17)),
            6 => Some(Color32::from_rgb(v(0, 2)?, v(2, 2)?, v(4, 2)?)),
            8 => Some(Color32::from_rgba_unmultiplied(v(0, 2)?, v(2, 2)?, v(4, 2)?, v(6, 2)?)),
            _ => None,
        };
    }
    let l = s.to_ascii_lowercase();
    let inner = l.strip_prefix("rgba(").or_else(|| l.strip_prefix("rgb("))?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split([',', ' ', '/']).filter(|p| !p.is_empty()).collect();
    if !(3..=4).contains(&parts.len()) {
        return None;
    }
    let c = |p: &str| p.parse::<f32>().ok().filter(|v| (0.0..=255.0).contains(v)).map(|v| v.round() as u8);
    let a = match parts.get(3) {
        Some(p) => {
            let v = p.trim_end_matches('%').parse::<f32>().ok()?;
            if p.ends_with('%') { (v * 2.55).round() as u8 } else if v <= 1.0 { (v * 255.0).round() as u8 } else { v as u8 }
        }
        None => 255,
    };
    Some(Color32::from_rgba_unmultiplied(c(parts[0])?, c(parts[1])?, c(parts[2])?, a))
}

fn hsl(c: Color32) -> (u32, u32, u32) {
    let [r, g, b, _] = c.to_srgba_unmultiplied().map(|v| v as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0, 0, (l * 100.0).round() as u32);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    ((h * 60.0).round() as u32 % 360, (s * 100.0).round() as u32, (l * 100.0).round() as u32)
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
    frames: u64,
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
    Activate(usize),
    Delete(usize),
    Pin(usize),
}

impl App {
    fn new(cfg: Config) -> Self {
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
            frames: 0,
        };
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
                        TextKind::Json => serde_json::from_str::<serde_json::Value>(t.trim())
                            .ok()
                            .and_then(|v| serde_json::to_string_pretty(&v).ok())
                            .unwrap_or(t),
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
            Action::Activate(i) => {
                if let Some(id) = self.items.get(i).map(|m| m.id) {
                    if self.req(&Request::Activate { id }).is_some() {
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
        let mut action = None;
        let mut close = false;
        let mut nav = 0isize;
        let mut cycle = 0isize;
        let mut group = None;
        let mut toggle_regex = false;
        let page = ((self.view_height / (ROW_H + ROW_GAP)).floor() as isize).max(1);
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                nav = 1;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                nav = -1;
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) {
                nav = page;
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                nav = -page;
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                action = Some(Action::Activate(self.selected));
            }
            if i.consume_key(Modifiers::COMMAND, Key::P) {
                action = Some(Action::Pin(self.selected));
            }
            if i.consume_key(Modifiers::SHIFT, Key::Delete) || i.consume_key(Modifiers::COMMAND, Key::D) {
                action = Some(Action::Delete(self.selected));
            }
            if i.consume_key(Modifiers::NONE, Key::Tab) {
                cycle = 1;
            }
            if i.consume_key(Modifiers::SHIFT, Key::Tab) {
                cycle = -1;
            }
            if i.consume_key(Modifiers::COMMAND, Key::R) || i.consume_key(Modifiers::ALT, Key::R) {
                toggle_regex = true;
            }
            let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            let mut swallow_text = false;
            for (n, k) in digits.iter().enumerate() {
                if i.consume_key(Modifiers::COMMAND, *k) {
                    group = Some(n);
                }
                if i.consume_key(Modifiers::ALT, *k) {
                    action = Some(Action::Activate(n));
                    swallow_text = true;
                }
            }
            if swallow_text {
                // ⌥1 also types "¡" on macOS - keep it out of the search box.
                i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
        });
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
        if close {
            if self.query.is_empty() {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else {
                self.query.clear();
                self.dirty = true;
            }
        }
        action
    }

    // ------------------------------------------------------------- views --

    fn header(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 38.0), Layout::right_to_left(Align::Center), |ui| {
            self.menu(ui, pal, 34.0);
            let outer_w = ui.available_width();
            let frame = egui::Frame::new()
                .fill(pal.surface)
                .corner_radius(10)
                .stroke(Stroke::new(1.0, if self.search_error.is_some() { pal.danger } else { pal.border }))
                .inner_margin(Margin { left: 34, right: 5, top: 5, bottom: 5 });
            let resp = frame.show(ui, |ui| {
                ui.set_width(outer_w - 34.0 - 5.0 - 2.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let on = self.regex;
                    let b = ui
                        .add(
                            egui::Button::new(RichText::new(".*").monospace().size(12.5).color(if on { pal.on_accent } else { pal.dim }))
                                .fill(if on { pal.accent } else { Color32::TRANSPARENT })
                                .corner_radius(6)
                                .stroke(Stroke::NONE)
                                .min_size(Vec2::new(30.0, 24.0)),
                        )
                        .on_hover_text(format!("正则表达式搜索 ({KEY_GROUP}R)"));
                    if b.clicked() {
                        self.regex = !self.regex;
                        self.dirty = true;
                    }
                    let edit = egui::TextEdit::singleline(&mut self.query)
                        .hint_text(if self.regex { "正则，例如 \\d{6} 或 ^https?://" } else { "搜索文本和图片中的文字" })
                        .desired_width(ui.available_width())
                        .font(FontId::proportional(15.0))
                        .frame(egui::Frame::NONE)
                        .margin(Margin::symmetric(0, 3));
                    let r = ui.add(edit);
                    if r.changed() {
                        self.selected = 0;
                        self.dirty = true;
                    }
                    if !self.sync_open && self.editor.is_none() {
                        r.request_focus();
                    }
                });
            });
            // magnifier icon
            let r = resp.response.rect;
            let c = Pos2::new(r.left() + 18.0, r.center().y - 1.0);
            let p = ui.painter();
            p.circle_stroke(c, 5.5, Stroke::new(1.6, pal.faint));
            p.line_segment([c + Vec2::new(4.0, 4.0), c + Vec2::new(8.0, 8.0)], Stroke::new(1.8, pal.faint));
        });
    }

    fn menu(&mut self, ui: &mut egui::Ui, pal: &Palette, w: f32) {
        let btn = egui::Button::new(RichText::new("•••").size(11.0).color(pal.dim))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE)
            .min_size(Vec2::new(w, 34.0));
        let resp = ui.add(btn).on_hover_text("菜单");
        egui::Popup::menu(&resp).show(|ui| {
            ui.set_min_width(170.0);
            let pause = if self.paused { "继续记录剪贴板" } else { "暂停记录剪贴板" };
            if ui.button(pause).clicked() {
                if let Some(Response::Status { paused, .. }) = self.req(&Request::SetPaused(None)) {
                    self.paused = paused;
                }
            }
            if ui.button("局域网同步…").clicked() {
                self.sync_open = true;
                self.sync_msg = None;
                self.refresh_sync();
            }
            if ui.button("管理分组…").clicked() {
                self.editor = Some(Editor::new(&self.groups, self.group));
            }
            ui.separator();
            let mut auto = self.autostart;
            if ui.checkbox(&mut auto, "登录时自动启动").changed() {
                match crate::autostart::set(auto) {
                    Ok(_) => self.autostart = auto,
                    Err(e) => self.notify(format!("设置自启失败：{e}")),
                }
            }
            if ui.button("打开分组配置文件").clicked() {
                open_external(&groups::path().to_string_lossy());
            }
            if ui.button("打开设置文件").clicked() {
                open_external(&crate::config::config_path().to_string_lossy());
            }
        });
    }

    fn group_bar(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let mut clicked = None;
        let mut make_default = None;
        let mut edit = None;
        egui::ScrollArea::horizontal()
            .id_salt("groups")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, g) in self.groups.list.iter().enumerate() {
                        let on = i == self.group;
                        let (fg, sub) = if on { (pal.on_accent, pal.on_accent.gamma_multiply(0.75)) } else { (pal.dim, pal.faint) };
                        let mut job = LayoutJob::default();
                        if i < 9 {
                            job.append(&format!("{} ", i + 1), 0.0, TextFormat { font_id: FontId::monospace(10.5), color: sub, ..Default::default() });
                        }
                        job.append(&g.name, 0.0, TextFormat { font_id: FontId::proportional(13.0), color: fg, ..Default::default() });
                        let count = match self.counts.get(i) {
                            Some(Some(n)) => format!("  {n}"),
                            Some(None) => "  !".into(),
                            None => String::new(),
                        };
                        if !count.is_empty() {
                            job.append(&count, 0.0, TextFormat { font_id: FontId::proportional(11.5), color: sub, ..Default::default() });
                        }
                        let resp = ui
                            .add(
                                egui::Button::new(job)
                                    .fill(if on { pal.accent } else { Color32::TRANSPARENT })
                                    .stroke(Stroke::NONE)
                                    .corner_radius(13)
                                    .min_size(Vec2::new(0.0, 26.0)),
                            )
                            .on_hover_text(if i < 9 { format!("{KEY_GROUP}{} · 右键：设为默认 / 编辑", i + 1) } else { "右键：设为默认 / 编辑".into() });
                        if resp.clicked() {
                            clicked = Some(i);
                        }
                        resp.context_menu(|ui| {
                            let is_default = self.groups.default == g.name;
                            if ui.add_enabled(!is_default, egui::Button::new("设为默认分组")).clicked() {
                                make_default = Some(i);
                            }
                            if ui.button("编辑分组…").clicked() {
                                edit = Some(i);
                            }
                        });
                    }
                });
            });
        if let Some(i) = clicked {
            self.set_group(i);
        }
        if let Some(i) = make_default {
            self.groups.default = self.groups.list[i].name.clone();
            match groups::save(&self.groups) {
                Ok(()) => self.notify(format!("打开时将默认显示「{}」", self.groups.default)),
                Err(e) => self.notify(format!("保存失败：{e}")),
            }
        }
        if let Some(i) = edit {
            self.editor = Some(Editor::new(&self.groups, i));
        }
    }

    fn list(&mut self, ui: &mut egui::Ui, pal: &Palette) -> Option<Action> {
        let mut action = None;
        if let Some(err) = &self.search_error {
            ui.add_space(10.0);
            ui.label(RichText::new(err).color(pal.danger).size(12.5));
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
            let top = self.selected as f32 * (ROW_H + ROW_GAP);
            let bottom = top + ROW_H;
            if top < self.scroll_offset {
                area = area.vertical_scroll_offset(top);
            } else if bottom > self.scroll_offset + self.view_height && self.view_height > 0.0 {
                area = area.vertical_scroll_offset(bottom - self.view_height);
            }
            self.scroll_to_selected = false;
        }
        let out = area.show_rows(ui, ROW_H, self.items.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = ROW_GAP;
            for i in range {
                let meta = self.items[i].clone();
                let width = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
                let selected = i == self.selected;
                let fill = if selected {
                    pal.selected
                } else if resp.hovered() {
                    pal.hover
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(rect, 9, fill);

                let kind = if meta.kind == Kind::Image { TextKind::Image } else { classify(&meta.preview, false) };
                // leading tile
                let tile = Rect::from_center_size(Pos2::new(rect.left() + 26.0, rect.center().y), Vec2::splat(34.0));
                self.paint_tile(ui, &ctx, pal, tile, &meta, kind);

                let x = tile.right() + 10.0;
                let right_pad = if i < 9 { 44.0 } else { 14.0 };
                let text_w = (rect.right() - x - right_pad).max(40.0);
                let title = match meta.kind {
                    Kind::Text => one_line(&meta.preview),
                    Kind::Image => meta
                        .ocr
                        .as_deref()
                        .map(one_line)
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| format!("{} × {}", meta.width, meta.height)),
                };
                let painter = ui.painter();
                let mut job = LayoutJob::single_section(title, TextFormat { font_id: FontId::proportional(13.5), color: pal.text, ..Default::default() });
                job.wrap.max_width = text_w;
                job.wrap.max_rows = 1;
                job.wrap.break_anywhere = true;
                job.wrap.overflow_character = Some('…');
                let galley = painter.layout_job(job);
                let block_h = galley.size().y + 2.0 + 14.0;
                let top = rect.center().y - block_h / 2.0;
                painter.galley(Pos2::new(x, top), galley, pal.text);
                let mut sub_job = LayoutJob::single_section(
                    subtitle(&meta, kind),
                    TextFormat { font_id: FontId::proportional(11.5), color: pal.faint, ..Default::default() },
                );
                sub_job.wrap.max_width = text_w;
                sub_job.wrap.max_rows = 1;
                sub_job.wrap.overflow_character = Some('…');
                painter.galley(Pos2::new(x, top + block_h - 14.0), painter.layout_job(sub_job), pal.faint);

                let mut rx = rect.right() - 12.0;
                if i < 9 {
                    let key = format!("{KEY_PASTE}{}", i + 1);
                    let kr = painter.text(Pos2::new(rx, rect.center().y), Align2::RIGHT_CENTER, key, FontId::proportional(11.0), if selected { pal.dim } else { pal.faint.gamma_multiply(0.8) });
                    rx = kr.left() - 6.0;
                }
                if meta.pinned {
                    painter.circle_filled(Pos2::new(rx - 3.0, rect.center().y), 3.0, pal.accent);
                }

                if resp.clicked() {
                    self.selected = i;
                }
                if resp.double_clicked() {
                    action = Some(Action::Activate(i));
                }
                resp.context_menu(|ui| {
                    if ui.button("粘贴").clicked() {
                        action = Some(Action::Activate(i));
                        ui.close();
                    }
                    if ui.button(if meta.pinned { "取消固定" } else { "固定" }).clicked() {
                        action = Some(Action::Pin(i));
                        ui.close();
                    }
                    if ui.button(RichText::new("删除").color(pal.danger)).clicked() {
                        action = Some(Action::Delete(i));
                        ui.close();
                    }
                });
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
                egui::Image::from_texture((tex.id(), tile.size())).uv(uv).corner_radius(8).paint_at(ui, tile);
                ui.painter().rect_stroke(tile, 8, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
                return;
            }
        }
        let p = ui.painter();
        if kind == TextKind::Color {
            if let Some(c) = parse_color(&meta.preview) {
                p.rect_filled(tile, 8, c);
                p.rect_stroke(tile, 8, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
                return;
            }
        }
        let tint = pal.tint(kind);
        p.rect_filled(tile, 8, tint.gamma_multiply(if pal.dark { 0.18 } else { 0.12 }));
        let size = if kind.glyph().chars().count() > 2 { 11.0 } else { 14.0 };
        p.text(tile.center(), Align2::CENTER_CENTER, kind.glyph(), FontId::proportional(size), tint);
    }

    fn preview(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let Some(meta) = self.items.get(self.selected).cloned() else { return };
        let ctx = ui.ctx().clone();
        egui::Frame::new()
            .fill(pal.surface)
            .corner_radius(12)
            .stroke(Stroke::new(1.0, pal.border))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                let kind = match self.detail(&ctx, meta.id) {
                    Detail::Text { kind, .. } => *kind,
                    Detail::Image { .. } => TextKind::Image,
                    Detail::Missing => TextKind::Plain,
                };
                // header: type chip + facts
                ui.horizontal(|ui| {
                    let tint = pal.tint(kind);
                    egui::Frame::new().fill(tint.gamma_multiply(if pal.dark { 0.2 } else { 0.12 })).corner_radius(6).inner_margin(Margin::symmetric(8, 2)).show(
                        ui,
                        |ui| {
                            ui.label(RichText::new(kind.label()).size(11.5).strong().color(tint));
                        },
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(facts(&meta)).size(11.5).color(pal.faint));
                    });
                });
                ui.add_space(6.0);
                let footer_h = 20.0;
                let body = Vec2::new(ui.available_width(), (ui.available_height() - footer_h - 8.0).max(60.0));
                ui.allocate_ui_with_layout(body, Layout::top_down(Align::LEFT), |ui| {
                    ui.set_min_size(body);
                    self.preview_body(ui, pal, &meta, kind);
                });
                ui.add_space(4.0);
                let mut info = vec![relative_time(meta.created)];
                if let Some(src) = &meta.source {
                    info.push(format!("来自 {src}"));
                }
                if meta.pinned {
                    info.push("已固定".into());
                }
                ui.label(RichText::new(info.join("  ·  ")).size(11.5).color(pal.faint));
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
                let img_h = if ocr_text.is_some() { (full.y * 0.66).floor() } else { full.y - 20.0 };
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
                    ui.label(RichText::new("来源应用正则（每行一条，可留空）").size(12.0).color(pal.dim));
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

    fn footer(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let enter = if cfg!(target_os = "macos") { "⏎" } else { "Enter" };
            for (k, label) in [
                ("↑↓", "选择"),
                (enter, "粘贴"),
                (&format!("{KEY_GROUP}1-9") as &str, "分组"),
                (&format!("{KEY_PASTE}1-9") as &str, "快速粘贴"),
                ("Esc", "关闭"),
            ] {
                keycap(ui, pal, k);
                ui.label(RichText::new(label).size(11.0).color(pal.faint));
                ui.add_space(4.0);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(sync) = self.sync.as_ref().filter(|s| s.paired) {
                    let n = sync.peers.len();
                    let r = ui
                        .add(egui::Label::new(RichText::new(format!("同步 {n} 台")).size(11.0).color(if n > 0 { pal.ok } else { pal.faint })).sense(Sense::click()))
                        .on_hover_text("局域网同步");
                    if r.clicked() {
                        self.sync_open = true;
                    }
                    ui.label(RichText::new("·").size(11.0).color(pal.faint));
                }
                if self.paused {
                    ui.label(RichText::new("已暂停记录").size(11.0).color(pal.danger));
                    ui.label(RichText::new("·").size(11.0).color(pal.faint));
                }
                ui.label(RichText::new(format!("{} 条", self.items.len())).size(11.0).color(pal.faint));
            });
        });
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

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
            s.visuals.extreme_bg_color = pal.bg;
            s.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, pal.border);
            s.visuals.window_fill = pal.surface;
            s.visuals.panel_fill = pal.bg;
            s.visuals.widgets.inactive.weak_bg_fill = pal.hover;
            s.visuals.widgets.hovered.weak_bg_fill = pal.selected;
        });

        egui::Frame::new()
            .fill(pal.bg)
            .stroke(Stroke::new(1.0, pal.border))
            .corner_radius(CornerRadius::same(16))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                // Drag the borderless window by any empty area.
                let bg = ui.interact(ui.max_rect(), ui.scope_id().with("window-drag"), Sense::click_and_drag());
                if bg.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }

                self.header(ui, &pal);
                ui.add_space(6.0);
                self.group_bar(ui, &pal);
                ui.add_space(4.0);
                let banner = self.error.clone().or_else(|| self.groups_error.clone());
                if let Some(err) = banner {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(err).color(pal.danger).size(12.0));
                        if self.error.is_some() && ui.small_button("重试").clicked() {
                            self.error = None;
                            self.connect();
                            self.dirty = true;
                        }
                    });
                }
                let footer_h = 22.0;
                let body_h = (ui.available_height() - footer_h - 6.0).max(80.0);
                ui.horizontal_top(|ui| {
                    let list_w = (ui.available_width() * 0.47).floor();
                    ui.allocate_ui_with_layout(Vec2::new(list_w, body_h), Layout::top_down(Align::LEFT), |ui| {
                        ui.set_min_size(Vec2::new(list_w, body_h));
                        if let Some(a) = self.list(ui, &pal) {
                            action = Some(a);
                        }
                    });
                    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), body_h), Layout::top_down(Align::LEFT), |ui| {
                        self.preview(ui, &pal);
                    });
                });
                ui.add_space(4.0);
                self.footer(ui, &pal);
            });

        // transient notice (e.g. "saved")
        if let Some((msg, at)) = &self.notice {
            if at.elapsed() < Duration::from_millis(2200) {
                let screen = ctx.content_rect();
                egui::Area::new(egui::Id::unique("notice"))
                    .order(egui::Order::Tooltip)
                    .pivot(Align2::CENTER_BOTTOM)
                    .fixed_pos(Pos2::new(screen.center().x, screen.bottom() - 44.0))
                    .show(&ctx, |ui| {
                        egui::Frame::new().fill(pal.text).corner_radius(8).inner_margin(Margin::symmetric(12, 6)).show(ui, |ui| {
                            ui.label(RichText::new(msg).size(12.5).color(pal.bg.to_opaque()));
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
    let Some(c) = parse_color(text) else { return };
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, (ui.available_height() * 0.5).clamp(80.0, 180.0)), Sense::hover());
    paint_checker(ui.painter(), r, pal);
    ui.painter().rect_filled(r, 10, c);
    ui.painter().rect_stroke(r, 10, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
    ui.add_space(10.0);
    let [cr, cg, cb, ca] = c.to_srgba_unmultiplied();
    let (h, s, l) = hsl(c);
    let rows = [
        ("HEX", if ca == 255 { format!("#{cr:02X}{cg:02X}{cb:02X}") } else { format!("#{cr:02X}{cg:02X}{cb:02X}{ca:02X}") }),
        ("RGB", if ca == 255 { format!("rgb({cr}, {cg}, {cb})") } else { format!("rgba({cr}, {cg}, {cb}, {:.2})", ca as f32 / 255.0) }),
        ("HSL", format!("hsl({h}, {s}%, {l}%)")),
    ];
    egui::Grid::new("color-values").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
        for (k, v) in rows {
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

fn subtitle(m: &ItemMeta, kind: TextKind) -> String {
    let mut parts = vec![kind.label().to_string(), relative_time(m.created)];
    match m.kind {
        Kind::Text if m.lines > 1 => parts.push(format!("{} 行", m.lines)),
        Kind::Image => parts.push(format!("{}×{}", m.width, m.height)),
        _ => {}
    }
    if let Some(src) = &m.source {
        parts.push(src.clone());
    }
    parts.join(" · ")
}

fn facts(m: &ItemMeta) -> String {
    match m.kind {
        Kind::Text => format!("{} 字符 · {} 行 · {}", m.chars, m.lines, human_size(m.bytes)),
        Kind::Image => format!("{} × {} · {}", m.width, m.height, human_size(m.bytes)),
    }
}

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

    #[test]
    fn content_types() {
        assert_eq!(classify("https://example.com/a?b=1", true), TextKind::Url);
        assert_eq!(classify("  #3D5AFE ", true), TextKind::Color);
        assert_eq!(classify("rgba(10, 20, 30, 0.5)", true), TextKind::Color);
        assert_eq!(classify("me@example.com", true), TextKind::Email);
        assert_eq!(classify("/home/me/file.txt", true), TextKind::Path);
        assert_eq!(classify(r"C:\Users\me", true), TextKind::Path);
        assert_eq!(classify(r#"{"a": [1, 2]}"#, true), TextKind::Json);
        assert_eq!(classify(r#"{"a": "#, true), TextKind::Plain); // broken JSON
        assert_eq!(classify("fn main() {\n    println!(\"hi\");\n}", true), TextKind::Code);
        assert_eq!(classify("今天天气不错\n我们去公园吧", true), TextKind::Plain);
        assert_eq!(classify("#hashtag", true), TextKind::Plain);
        assert_eq!(parse_color("#fff"), Some(Color32::WHITE));
        assert_eq!(hsl(Color32::from_rgb(255, 0, 0)), (0, 100, 50));
    }
}
