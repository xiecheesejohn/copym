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

mod settings;
mod vim;
use vim::Mode;
#[cfg(test)]
mod shots;
use settings::Settings;

const ROW_H: f32 = 30.0;
const SECTION_H: f32 = 28.0;
const SEARCH_H: f32 = 56.0;
const FOOTER_H: f32 = 40.0;
/// Items per page; more load as the list nears its end.
const PAGE: u32 = 200;
const PREVIEW_MAX_SIDE: u32 = 2048;

#[cfg(target_os = "macos")]
const KEY_GROUP: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const KEY_GROUP: &str = "Ctrl+";

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
    let after = open_window(None, Config::load(), open, None, Arc::new(AtomicBool::new(false)), None)?;
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
    // macOS: after the first show the window stays alive and is hidden / shown
    // again (see `Resident`), so this thread shows it directly.
    let resident = Resident { open: open.clone(), show: Arc::default(), running: Arc::default(), notice: Arc::default() };
    let (tx, rx) = mpsc::channel::<Option<(Option<String>, Option<String>)>>();
    {
        let (ctx, open, close, resident) = (ctx.clone(), open.clone(), close.clone(), resident.clone());
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                trace("toggle received");
                let mut parts = line.split('\t').map(|p| (p != "-" && !p.is_empty()).then(|| p.to_string()));
                let _cmd = parts.next();
                let token = parts.next().flatten();
                let prev = parts.next().flatten();
                if open.swap(true, Ordering::SeqCst) {
                    // Already open (or opening): the hotkey closes it.
                    close.store(true, Ordering::SeqCst);
                    ctx.request_repaint();
                } else if resident.running.load(Ordering::SeqCst) {
                    resident.show.store(true, Ordering::SeqCst);
                    resident::unhide();
                    ctx.request_repaint();
                } else if tx.send(Some((token, prev))).is_err() {
                    break;
                }
            }
            // stdin closed: the daemon is gone.
            if resident.running.load(Ordering::SeqCst) {
                std::process::exit(0);
            }
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
        let keep = cfg!(target_os = "macos").then(|| resident.clone());
        if keep.is_some() {
            resident.running.store(true, Ordering::SeqCst);
        }
        let after = open_window(Some(ctx.clone()), Config::load(), "", notice.take(), close.clone(), keep.clone());
        if keep.is_some() {
            // only returns when the user quit copym
            return after.map(|_| ());
        }
        shown = true;
        open.store(false, Ordering::SeqCst);
        match after {
            Ok(after) => notice = finish(&after, prev.as_deref()).map(|e| tf!("自动粘贴失败：{e}（内容已在剪贴板）", "auto paste failed: {e} (the content is on the clipboard)")),
            Err(e) => {
                eprintln!("copym: popup: {e}");
                return Err(e);
            }
        }
    }
    Ok(())
}

/// macOS popup host: the event loop must keep running on the main thread, or
/// macOS marks the process "not responding" and `NSApplication.hide` (focus
/// back to the previous app) never completes. So, as CopyQ does, the window is
/// created once and closing only hides the app; the stdin thread shows it again.
#[derive(Clone)]
struct Resident {
    /// Shared with the stdin thread: the popup is visible (or being shown).
    open: Arc<AtomicBool>,
    /// Set by the stdin thread: show again, starting fresh.
    show: Arc<AtomicBool>,
    /// The window exists (after the first show).
    running: Arc<AtomicBool>,
    /// Paste error from the last close, shown next time.
    notice: Arc<Mutex<Option<String>>>,
}

mod resident {
    /// Show the hidden app again (from any thread).
    pub fn unhide() {
        #[cfg(target_os = "macos")]
        dispatch2::DispatchQueue::main().exec_async(|| {
            if let Some(mtm) = objc2::MainThreadMarker::new() {
                let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
                app.unhide(None);
                #[allow(deprecated)] // `activate` needs macOS 14
                app.activateIgnoringOtherApps(true);
            }
        });
    }
}

fn open_window(
    ctx: Option<egui::Context>,
    cfg: Config,
    open: &str,
    notice: Option<String>,
    close: Arc<AtomicBool>,
    resident: Option<Resident>,
) -> Result<AfterClose, String> {
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
            app.resident = resident;
            trace("app state");
            if let Some(n) = notice {
                app.notify(n);
            }
            app.sync_open = open == "sync";
            if open == "actions" {
                app.actions = Some(0);
            }
            if let Some(section) = open.strip_prefix("settings") {
                let mut st = Settings::new(&app.cfg, app.autostart);
                st.section = section.trim_start_matches(':').parse().unwrap_or(0);
                app.settings = Some(st);
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
    install_fonts(ctx, &cfg.font);
    // Linux: follow the desktop's light/dark setting (see `App::wants_dark`).
    #[cfg(target_os = "linux")]
    {
        let ctx = ctx.clone();
        crate::system_theme::watch(move || ctx.request_repaint());
    }
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.scroll.bar_width = 6.0;
        s.spacing.scroll.floating = true;
        s.interaction.selectable_labels = false;
        s.visuals.window_corner_radius = CornerRadius::same(2);
        s.visuals.menu_corner_radius = CornerRadius::same(2);
        s.visuals.text_cursor.blink = true;
        for w in [
            &mut s.visuals.widgets.inactive,
            &mut s.visuals.widgets.hovered,
            &mut s.visuals.widgets.active,
            &mut s.visuals.widgets.noninteractive,
            &mut s.visuals.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(2);
        }
    });
}

/// After the window closed: give focus back to the previous window and paste.
/// Returns the paste error, if any.
fn finish(after: &AfterClose, prev: Option<&str>) -> Option<String> {
    restore_focus(prev);
    paste_after(after)
}

/// Ask the daemon to paste (it waits for the focus change first).
fn paste_after(after: &AfterClose) -> Option<String> {
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
/// assets/fonts). System fonts lack some of them (⏎ ⇧ ⌥ on macOS) and egui drew them as boxes.
static SYMBOLS_FONT: &[u8] = include_bytes!("../assets/fonts/copym-symbols.ttf");

/// Lucide icons (ISC license, see assets/fonts), drawn with `icon()`.
static ICONS_FONT: &[u8] = include_bytes!("../assets/fonts/copym-icons.ttf");

/// Maple Mono NF CN Regular (OFL-1.1, see assets/fonts), zstd-compressed:
/// monospace Latin, CJK and Nerd Font icons, the same on every platform.
static MAIN_FONT_ZST: &[u8] = include_bytes!("../assets/fonts/maple-mono-nf-cn.ttf.zst");

/// The bundled main font, decompressed once per process.
fn main_font() -> Option<Arc<FontData>> {
    static FONT: std::sync::OnceLock<Option<Arc<FontData>>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        use std::io::Read;
        let mut bytes = Vec::with_capacity(21 << 20);
        let mut dec = ruzstd::decoding::StreamingDecoder::new(MAIN_FONT_ZST).ok()?;
        dec.read_to_end(&mut bytes).ok()?;
        Some(Arc::new(FontData::from_owned(bytes)))
    })
    .clone()
}

fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}


/// UI font: `font` from the config, else the platform's own UI font with CJK
/// (PingFang SC, Microsoft YaHei UI, fontconfig's sans-serif for zh-cn).
/// egui's bundled fonts stay as fallback (they have no CJK glyphs).
fn install_fonts(ctx: &egui::Context, custom: &str) {
    let mut fonts = FontDefinitions::default();
    // Terminal look: one monospace face for everything. The bundled Maple
    // Mono (Latin, CJK, Nerd Font icons; egui's Hack if it cannot load), the
    // system CJK font for what it lacks, then emoji and icon fallbacks.
    let mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let mut family = Vec::new();
    if let Some((bytes, index)) = custom_font(custom) {
        let mut data = FontData::from_owned(bytes);
        data.index = index;
        fonts.font_data.insert("custom".into(), Arc::new(data));
        family.push("custom".to_string());
    }
    match main_font() {
        Some(data) => {
            fonts.font_data.insert("maple-mono".into(), data);
            family.push("maple-mono".into());
        }
        None => family.extend(mono.first().cloned()),
    }
    if let Some((bytes, index)) = find_ui_font("") {
        let mut data = FontData::from_owned(bytes);
        data.index = index;
        fonts.font_data.insert("system-cjk".into(), Arc::new(data));
        family.push("system-cjk".into());
    }
    family.extend(mono.iter().skip(1).cloned());
    fonts.font_data.insert("copym-icons".into(), Arc::new(FontData::from_static(ICONS_FONT)));
    // last fallback, so icons can sit inline in ordinary text
    family.push("copym-icons".into());
    fonts.families.insert(FontFamily::Proportional, family.clone());
    fonts.families.insert(FontFamily::Monospace, family);
    fonts.families.insert(FontFamily::Name("icons".into()), vec!["copym-icons".into()]);
    add_symbols(&mut fonts);
    ctx.set_fonts(fonts);
}

/// The `font` setting: a font file (first face of a collection).
fn custom_font(path: &str) -> Option<(Vec<u8>, u32)> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let path = match path.strip_prefix("~/").zip(dirs::home_dir()) {
        Some((rest, home)) => home.join(rest),
        None => path.into(),
    };
    FontFile::new(path, None).load()
}

/// First in every family, so keycaps look the same on every platform (the
/// font has no other glyphs).
fn add_symbols(fonts: &mut FontDefinitions) {
    fonts.font_data.insert("copym-symbols".into(), Arc::new(FontData::from_static(SYMBOLS_FONT)));
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, "copym-symbols".into());
    }
}

/// A font file and the PostScript name of the face to use inside a collection
/// (None: the given index).
struct FontFile {
    path: std::path::PathBuf,
    face: Option<&'static str>,
    index: u32,
}

impl FontFile {
    fn new(path: impl Into<std::path::PathBuf>, face: Option<&'static str>) -> Self {
        Self { path: path.into(), face, index: 0 }
    }

    fn load(&self) -> Option<(Vec<u8>, u32)> {
        let bytes = std::fs::read(&self.path).ok()?;
        let index = match self.face {
            Some(name) => face_index(&bytes, name)?,
            None => self.index,
        };
        // only fonts egui can actually read
        skrifa::FontRef::from_index(&bytes, index).ok()?;
        Some((bytes, index))
    }
}

/// Index of the face with this PostScript name in a font file or collection.
fn face_index(bytes: &[u8], postscript: &str) -> Option<u32> {
    use skrifa::{raw::FileRef, string::StringId, MetadataProvider};
    let file = FileRef::new(bytes).ok()?;
    file.fonts().enumerate().find_map(|(i, font)| {
        let name = font.ok()?.localized_strings(StringId::POSTSCRIPT_NAME).english_or_first()?.to_string();
        (name == postscript).then_some(i as u32)
    })
}

fn find_ui_font(custom: &str) -> Option<(Vec<u8>, u32)> {
    let mut candidates: Vec<FontFile> = Vec::new();
    let _ = custom; // see `custom_font`
    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("fc-match").args(["-f", "%{file}\n%{index}", "sans-serif:lang=zh-cn"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        let mut it = s.lines();
        if let Some(file) = it.next() {
            // fontconfig encodes variable-font named instances in the high 16 bits.
            let index = it.next().and_then(|i| i.trim().parse::<u32>().ok()).unwrap_or(0) & 0xFFFF;
            candidates.push(FontFile { index, ..FontFile::new(file, None) });
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
        .map(|p| FontFile::new(p, None)),
    );
    #[cfg(windows)]
    {
        let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
        // Microsoft YaHei UI is the second face of msyh.ttc (the first one has
        // taller line metrics meant for documents).
        candidates.push(FontFile::new(dir.join("msyh.ttc"), Some("MicrosoftYaHeiUI")));
        candidates.extend(["msyh.ttc", "msyh.ttf", "msjh.ttc", "YuGothR.ttc", "malgun.ttf", "simsun.ttc"].map(|f| FontFile::new(dir.join(f), None)));
    }
    #[cfg(target_os = "macos")]
    {
        // Since macOS 11 PingFang lives in a hidden asset directory; Core Text knows where.
        for name in ["PingFangSC-Regular", ".PingFangSC-Regular"] {
            if let Some(path) = mac::font_path(name) {
                candidates.push(FontFile::new(path, Some("PingFangSC-Regular")));
            }
        }
        candidates.push(FontFile::new("/System/Library/Fonts/PingFang.ttc", Some("PingFangSC-Regular")));
        candidates.extend(
            [
                "/System/Library/Fonts/Hiragino Sans GB.ttc",
                "/System/Library/Fonts/STHeiti Medium.ttc",
                "/System/Library/Fonts/STHeiti Light.ttc",
                "/Library/Fonts/Arial Unicode.ttf",
            ]
            .map(|p| FontFile::new(p, None)),
        );
    }
    candidates.iter().find_map(FontFile::load)
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2_core_foundation::{CFString, CFURL};
    use objc2_core_text::{kCTFontURLAttribute, CTFont};

    /// File of the installed font with this PostScript name.
    pub fn font_path(postscript: &str) -> Option<std::path::PathBuf> {
        let font = unsafe { CTFont::with_name(&CFString::from_str(postscript), 13.0, std::ptr::null()) };
        let url = unsafe { font.attribute(kCTFontURLAttribute) }?.downcast::<CFURL>().ok()?;
        url.to_file_path()
    }
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
    /// Phosphor terminal: green on near-black (dark), dark green on paper (light).
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                bg: Color32::from_rgba_unmultiplied(8, 12, 9, 252),
                surface: Color32::from_rgb(12, 20, 14),
                hover: Color32::from_rgba_unmultiplied(51, 255, 102, 12),
                selected: Color32::from_rgba_unmultiplied(51, 255, 102, 34),
                accent: Color32::from_rgb(51, 255, 102),
                on_accent: Color32::from_rgb(6, 12, 8),
                border: Color32::from_rgba_unmultiplied(51, 255, 102, 70),
                text: Color32::from_rgb(178, 247, 192),
                dim: Color32::from_rgb(96, 186, 118),
                faint: Color32::from_rgb(52, 120, 70),
                danger: Color32::from_rgb(255, 92, 92),
                ok: Color32::from_rgb(51, 255, 102),
                dark,
            }
        } else {
            Self {
                bg: Color32::from_rgba_unmultiplied(245, 242, 230, 252),
                surface: Color32::from_rgb(251, 249, 240),
                hover: Color32::from_rgba_unmultiplied(0, 110, 45, 14),
                selected: Color32::from_rgba_unmultiplied(0, 110, 45, 34),
                accent: Color32::from_rgb(0, 128, 55),
                on_accent: Color32::from_rgb(251, 249, 240),
                border: Color32::from_rgba_unmultiplied(0, 100, 40, 80),
                text: Color32::from_rgb(18, 52, 28),
                dim: Color32::from_rgb(56, 104, 70),
                faint: Color32::from_rgb(120, 140, 118),
                danger: Color32::from_rgb(190, 40, 40),
                ok: Color32::from_rgb(0, 128, 55),
                dark,
            }
        }
    }

    /// ANSI-like color of a content type tag.
    fn tint(&self, k: TextKind) -> Color32 {
        let (l, d) = match k {
            TextKind::Url | TextKind::Email => ((0, 120, 150), (80, 220, 240)),
            TextKind::Code => ((0, 128, 55), (51, 255, 102)),
            TextKind::Json => ((150, 110, 0), (240, 210, 80)),
            TextKind::Path | TextKind::Color => ((140, 40, 140), (230, 120, 230)),
            TextKind::Image => ((40, 80, 170), (120, 170, 255)),
            TextKind::Plain => ((56, 104, 70), (96, 186, 118)),
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
    /// Some counts are lower bounds ("n+").
    counts_more: bool,
    items: Vec<ItemMeta>,
    /// Cursor of the next page (`None` = everything loaded).
    next: Option<u64>,
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
    settings: Option<Settings>,
    sync_polled: Option<Instant>,
    sync_input: String,
    /// (text, is_error)
    sync_msg: Option<(String, bool)>,
    /// Code + addresses of the pairing window we opened.
    sync_pairing: Option<(String, Vec<String>)>,
    sync_job: Option<mpsc::Receiver<Result<String, String>>>,
    /// Newer release (from the daemon's daily check).
    update: Option<copym_core::model::UpdateInfo>,
    update_job: Option<mpsc::Receiver<Result<Option<copym_core::model::UpdateInfo>, String>>>,
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
    pending_restart: bool,
    /// Local time zone offset in seconds.
    tz: i64,
    frames: u64,
    /// Keyboard goes to the search box (Vim INSERT; otherwise NORMAL, the list).
    search_mode: bool,
    /// `:` command line being typed (Vim COMMAND mode).
    cmdline: Option<String>,
    /// Count / operator typed so far in NORMAL mode ("12", "g", "3d" …).
    pending: String,
    /// Key help (`?`).
    help: bool,
    /// Scroll offset of the help page (j/k move it).
    help_scroll: f32,
    /// Set by the popup host when the hotkey asks the window to close.
    close_req: Arc<AtomicBool>,
    after: Arc<Mutex<AfterClose>>,
    /// macOS: the window is kept and hidden instead of closed.
    resident: Option<Resident>,
    /// Hidden (resident mode): skip frames until shown again.
    hidden: bool,
}

#[derive(Clone)]
enum Line {
    Section(String),
    Item(usize),
}

#[derive(Clone)]
enum Act {
    Paste,
    Copy,
    Pin,
    PinTo(u32),
    OpenUrl(String),
    OpenPath(String),
    Delete,
    Pause,
    Sync,
    Groups,
    /// Stop recording copies from this app.
    IgnoreApp(String),
    Theme,
    CheckUpdate,
    OpenUpdate(String),
    Settings,
    Restart,
    Quit,
}

enum PanelRow {
    Title(String),
    /// action, icon, label, key hint
    Act(Act, &'static str, String, String),
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
    /// Pin into the first pin group, or unpin.
    Pin(usize),
    /// Put into pin group n (again = unpin; 0 = unpin).
    PinTo(usize, u32),
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
            counts_more: false,
            next: None,
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
            settings: None,
            sync_polled: None,
            sync_input: String::new(),
            sync_msg: None,
            sync_pairing: None,
            sync_job: None,
            update: None,
            update_job: None,
            editor: None,
            lines: Vec::new(),
            item_y: Vec::new(),
            list_height: 0.0,
            actions: None,
            pending_activate: None,
            pending_quit: false,
            pending_restart: false,
            tz: local_offset(),
            frames: 0,
            search_mode: false,
            cmdline: None,
            pending: String::new(),
            help: false,
            help_scroll: 0.0,
            close_req,
            after,
            resident: None,
            hidden: false,
        };
        app.search_mode = app.cfg.popup_focus == "search";
        app.connect();
        if let Some(Response::Status { paused, ocr, .. }) = app.req(&Request::Status) {
            app.paused = paused;
            app.ocr_available = ocr.is_some();
        }
        app.refresh_sync();
        if let Some(Response::Update(u)) = app.req(&Request::CheckUpdate { now: false }) {
            app.update = u;
        }
        app
    }

    fn connect(&mut self) {
        match Client::connect() {
            Ok(c) => {
                self.client = Some(c);
                self.error = None;
            }
            Err(e) => self.error = Some(tf!("无法连接 copym 守护进程：{e}", "cannot connect to the copym daemon: {e}")),
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
                self.error = Some(tf!("守护进程连接中断：{e}", "lost the connection to the daemon: {e}"));
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
        let req = Request::ListGroup { query: self.query.clone(), regex: self.regex, group, before: None, limit: PAGE };
        self.search_error = None;
        self.next = None;
        match self.req(&req) {
            Some(Response::Page { items, next }) => {
                self.items = items;
                self.next = next;
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
            Some(Response::GroupCounts { counts, more }) => {
                self.counts = counts;
                self.counts_more = more;
            }
            _ => {
                self.counts.clear();
                if self.client.is_some() {
                    self.error = None; // already reported as search_error
                }
            }
        }
    }

    /// Load the next page when the list is short (a search still streaming
    /// in) or the cursor / scroll position nears the end.
    fn load_more(&mut self, ctx: &egui::Context) {
        let Some(before) = self.next else { return };
        let near_end = self.selected + 50 >= self.items.len()
            || (self.view_height > 0.0 && self.scroll_offset + self.view_height * 2.0 >= self.list_height);
        if self.items.len() >= PAGE as usize && !near_end {
            return;
        }
        let rule = self.groups.rule(self.group);
        let group = (rule != GroupRule::default()).then_some(rule);
        let req = Request::ListGroup { query: self.query.clone(), regex: self.regex, group, before: Some(before), limit: PAGE };
        match self.req(&req) {
            Some(Response::Page { items, next }) => {
                let known: std::collections::HashSet<u64> = self.items.iter().map(|m| m.id).collect();
                self.items.extend(items.into_iter().filter(|m| !known.contains(&m.id)));
                self.next = next;
                self.layout_list();
                ctx.request_repaint();
            }
            _ => self.next = None,
        }
    }

    /// Split the list into sections: pinned, today, yesterday, this week, older.
    fn layout_list(&mut self) {
        let today = (now_secs() + self.tz).div_euclid(86_400);
        let mut lines = Vec::with_capacity(self.items.len() + 5);
        let mut item_y = Vec::with_capacity(self.items.len());
        let mut y = 4.0;
        let mut last = String::new();
        let several = self.groups.boards().nth(1).is_some();
        for (i, m) in self.items.iter().enumerate() {
            let sec: String = if m.pinned {
                // one section per pin group when there are several
                match self.groups.board_name(m.board).filter(|_| several) {
                    Some(name) => name.to_string(),
                    None => t!("已固定", "pinned").into(),
                }
            } else {
                match today - (m.created + self.tz).div_euclid(86_400) {
                    ..=0 => t!("今天", "today"),
                    1 => t!("昨天", "yesterday"),
                    2..=6 => t!("本周", "this week"),
                    _ => t!("更早", "older"),
                }
                .into()
            };
            if sec != last {
                lines.push((y, Line::Section(sec.clone())));
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
                        self.close(ctx);
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
                if let Some(m) = self.items.get(i) {
                    let board = if m.pinned { 0 } else { self.groups.first_board() };
                    self.perform(ctx, Action::PinTo(i, board));
                }
            }
            Action::PinTo(i, board) => {
                if let Some((id, current)) = self.items.get(i).map(|m| (m.id, m.board)) {
                    let board = if board == current { 0 } else { board };
                    if let Some(Response::Ok) = self.req(&Request::SetPin { id, board }) {
                        match self.groups.board_name(board) {
                            Some(name) => self.notify(tf!("已放进「{name}」", "put into “{name}”")),
                            None => self.notify(t!("已取消固定", "unpinned")),
                        }
                    }
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


    /// Close the popup: end the window, or (resident mode) hide the app, which
    /// gives the focus back to the previous app, and paste in the background.
    fn close(&mut self, ctx: &egui::Context) {
        let Some(r) = &self.resident else {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        };
        if self.hidden {
            return;
        }
        self.hidden = true;
        let after = std::mem::take(&mut *self.after.lock().unwrap());
        restore_focus(None);
        r.open.store(false, Ordering::SeqCst);
        let notice = r.notice.clone();
        std::thread::spawn(move || {
            if let Some(e) = paste_after(&after) {
                *notice.lock().unwrap() = Some(tf!("自动粘贴失败：{e}（内容已在剪贴板）", "auto paste failed: {e} (the content is on the clipboard)"));
            }
        });
    }

    /// Resident mode: shown again - start from a fresh state, like a new window.
    fn reset_for_show(&mut self, ctx: &egui::Context) {
        let notice = self.resident.as_ref().and_then(|r| r.notice.lock().unwrap().take());
        let mut app = App::new(Config::load(), self.after.clone(), self.close_req.clone());
        app.resident = self.resident.take();
        *self = app;
        self.close_req.store(false, Ordering::SeqCst);
        if let Some(n) = notice {
            self.notify(n);
        }
        egui::Popup::close_all(ctx);
        ctx.memory_mut(|m| m.data.clear());
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    /// Stop copym (daemon and popup).
    fn quit(&mut self, ctx: &egui::Context) {
        let _ = self.req(&Request::Quit);
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    /// Restart copym (daemon and popup) to apply settings and hotkeys: a
    /// detached `copym restart` stops this daemon and starts a new one.
    fn restart(&mut self) {
        match crate::daemon::spawn_detached("restart") {
            Ok(()) => self.notify(t!("正在重启 copym…", "restarting copym…")),
            Err(e) => self.notify(tf!("重启失败：{e}", "restart failed: {e}")),
        }
    }

    /// Rows of the action panel for the selected item.
    /// Rows of the action menu. The key column is the row's letter: pressing
    /// it runs the row (which-key style); j/k/⏎ work too.
    fn panel_rows(&self) -> Vec<PanelRow> {
        use crate::icons as ic;
        let mut rows = Vec::new();
        let act = |a: Act, glyph: &'static str, label: String, key: &str| PanelRow::Act(a, glyph, label, key.to_string());
        if let Some(m) = self.items.get(self.selected) {
            let title = match m.kind {
                Kind::Text => one_line(&m.preview),
                Kind::Image => tf!("图片 {}×{}", "image {}×{}", m.width, m.height),
            };
            rows.push(PanelRow::Title(title));
            rows.push(act(Act::Paste, ic::CLIPBOARD_PASTE, t!("粘贴到当前窗口", "paste to the active window").into(), "p"));
            rows.push(act(Act::Copy, ic::COPY, t!("复制到剪贴板", "yank to clipboard").into(), "y"));
            if let Some((id, Detail::Text { kind, body })) = &self.detail {
                if *id == m.id {
                    match kind {
                        TextKind::Url => rows.push(act(Act::OpenUrl(body.trim().into()), ic::EXTERNAL_LINK, t!("在浏览器中打开", "open in browser").into(), "o")),
                        TextKind::Path => rows.push(act(Act::OpenPath(body.trim().into()), ic::FOLDER_OPEN, t!("在文件管理器中打开", "show in file manager").into(), "o")),
                        _ => {}
                    }
                }
            }
            let boards: Vec<(u32, String, String)> = self.groups.boards().map(|(_, g)| (g.board, g.name.clone(), g.key.clone())).collect();
            if boards.len() <= 1 {
                let (pin_icon, pin_label) = if m.pinned { (ic::PIN_OFF, t!("取消固定", "unpin")) } else { (ic::PIN, t!("固定", "pin")) };
                rows.push(act(Act::Pin, pin_icon, pin_label.into(), "m"));
            } else {
                if m.pinned {
                    let from = self.groups.board_name(m.board).unwrap_or("").to_string();
                    rows.push(act(Act::PinTo(0), ic::PIN_OFF, tf!("取消固定（从「{from}」）", "unpin (from “{from}”)"), "m"));
                }
                // one row per other pin group; the key column shows the NORMAL-mode command
                for (b, name, key) in boards.into_iter().filter(|(b, _, _)| *b != m.board) {
                    let hint = if key.is_empty() { String::new() } else { format!("m{key}") };
                    rows.push(act(Act::PinTo(b), ic::PIN, tf!("放进「{name}」", "put into “{name}”"), &hint));
                }
            }
            rows.push(act(Act::Delete, ic::TRASH_2, t!("删除", "delete").into(), "d"));
            if let Some(app) = m.source.as_deref().filter(|a| self.is_local_app(a)) {
                rows.push(act(Act::IgnoreApp(app.to_string()), ic::BAN, tf!("不再记录「{app}」的复制", "stop recording “{app}”"), "b"));
            }
        }
        rows.push(PanelRow::Title("copym".into()));
        let (pause_icon, pause_label) = if self.paused { (ic::PLAY, t!("继续记录剪贴板", "resume recording")) } else { (ic::PAUSE, t!("暂停记录剪贴板", "pause recording")) };
        rows.push(act(Act::Pause, pause_icon, pause_label.into(), "r"));
        let (theme_icon, theme_label) = match self.cfg.theme.as_str() {
            "light" => (ic::SUN, t!("外观：浅色", "appearance: light")),
            "dark" => (ic::MOON, t!("外观：深色", "appearance: dark")),
            _ => (ic::SUN_MOON, t!("外观：跟随系统", "appearance: system")),
        };
        rows.push(act(Act::Theme, theme_icon, theme_label.into(), "t"));
        rows.push(act(Act::Sync, ic::WIFI, t!("局域网同步…", "LAN sync…").into(), "s"));
        rows.push(act(Act::Groups, ic::LAYERS, t!("管理分组…", "manage groups…").into(), "g"));
        rows.push(act(Act::Settings, ic::SETTINGS, t!("设置…", "settings…").into(), ","));
        match &self.update {
            Some(u) => rows.push(act(Act::OpenUpdate(u.url.clone()), ic::DOWNLOAD, tf!("下载新版本 {}", "download version {}", u.version), "u")),
            None => rows.push(act(Act::CheckUpdate, ic::REFRESH_CW, tf!("检查更新（v{}）", "check for updates (v{})", env!("CARGO_PKG_VERSION")), "u")),
        }
        rows.push(act(Act::Restart, ic::REFRESH_CW, t!("重启 copym", "restart copym").into(), "R"));
        rows.push(act(Act::Quit, ic::LOG_OUT, t!("退出 copym", "quit copym").into(), "Q"));
        rows
    }

    fn panel_acts(&self) -> Vec<Act> {
        self.panel_rows().into_iter().filter_map(|r| if let PanelRow::Act(a, ..) = r { Some(a) } else { None }).collect()
    }

    fn panel_keys(&mut self, ctx: &egui::Context) -> Option<Action> {
        let rows = self.panel_rows();
        let keys: Vec<String> = rows.iter().filter_map(|r| if let PanelRow::Act(_, _, _, k) = r { Some(k.clone()) } else { None }).collect();
        let n = keys.len();
        let mut sel = self.actions.unwrap_or(0);
        let mut run = None;
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
                run = Some(sel);
            }
            #[cfg(target_os = "macos")]
            let toggle = i.consume_key(Modifiers::MAC_CMD, Key::K);
            #[cfg(not(target_os = "macos"))]
            let toggle = i.consume_key(Modifiers::ALT, Key::K);
            if toggle || i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
            for e in &i.events {
                let egui::Event::Text(t) = e else { continue };
                for c in t.chars() {
                    match c {
                        'j' => sel = (sel + 1) % n.max(1),
                        'k' => sel = (sel + n.max(1) - 1) % n.max(1),
                        'l' => run = Some(sel),
                        'h' | 'q' | ' ' => close = true,
                        'G' => sel = n.saturating_sub(1),
                        _ => {
                            if let Some(pos) = keys.iter().position(|k| k.chars().eq(std::iter::once(c))) {
                                run = Some(pos);
                            }
                        }
                    }
                }
            }
            i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
        });
        self.actions = Some(sel);
        if close {
            self.actions = None;
        }
        if let Some(r) = run {
            let act = self.panel_acts().get(r).cloned();
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
            Act::Restart => self.pending_restart = true,
            Act::Pin => return Some(Action::Pin(self.selected)),
            Act::PinTo(0) => {
                let current = self.items.get(self.selected).map(|m| m.board).unwrap_or(0);
                return Some(Action::PinTo(self.selected, current));
            }
            Act::PinTo(b) => return Some(Action::PinTo(self.selected, b)),
            Act::Delete => return Some(Action::Delete(self.selected)),
            Act::OpenUrl(u) => open_external(&u),
            Act::OpenPath(p) => {
                let path = std::path::Path::new(p.trim_start_matches("file://"));
                let dir = if path.is_dir() { Some(path) } else { path.parent().filter(|d| d.is_dir()) };
                match dir {
                    Some(d) => open_external(&d.to_string_lossy()),
                    None => self.notify(t!("本机上不存在这个路径", "this path does not exist here")),
                }
            }
            Act::Pause => {
                if let Some(Response::Status { paused, .. }) = self.req(&Request::SetPaused(None)) {
                    self.paused = paused;
                    self.notify(if paused { t!("已暂停记录", "recording paused") } else { t!("继续记录", "recording resumed") });
                }
            }
            Act::Sync => {
                self.sync_open = true;
                self.sync_msg = None;
                self.refresh_sync();
            }
            Act::Groups => self.editor = Some(Editor::new(&self.groups, self.group)),
            Act::IgnoreApp(app) => {
                let mut apps = self.cfg.ignore_apps.clone();
                apps.push(app.clone());
                if self.set_ignored(apps) {
                    self.notify(tf!("不再记录「{app}」的复制（可在“不记录的应用…”里恢复）", "no longer recording “{app}” (undo in settings › privacy)"));
                } else {
                    let e = self.error.take().unwrap_or_default();
                    self.notify(tf!("设置失败：{e}", "failed: {e}"));
                }
            }
            Act::Theme => {
                let next = match self.cfg.theme.as_str() {
                    "system" | "" => "light",
                    "light" => "dark",
                    _ => "system",
                };
                self.save_theme(next);
            }
            Act::OpenUpdate(url) => open_external(&url),
            Act::CheckUpdate => {
                if self.update_job.is_none() {
                    let (tx, rx) = mpsc::channel();
                    self.update_job = Some(rx);
                    std::thread::spawn(move || {
                        let res = Client::connect().map_err(|e| e.to_string()).and_then(|mut c| c.request(&Request::CheckUpdate { now: true }).map_err(|e| e.to_string())).and_then(|r| match r {
                            Response::Update(u) => Ok(u),
                            Response::Err(e) => Err(e),
                            _ => Err("unexpected response".into()),
                        });
                        let _ = tx.send(res);
                    });
                    self.notify(t!("正在检查更新…", "checking for updates…"));
                }
            }
            Act::Settings => self.open_settings(),
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
                        .corner_radius(2)
                        .min_size(Vec2::new(30.0, 26.0)),
                )
                .on_hover_text(t!("正则表达式搜索（:set regex）", "regular expressions (:set regex)"));
            if b.clicked() {
                self.regex = !self.regex;
                self.dirty = true;
            }
            let hint = match (self.search_mode, self.regex) {
                (_, true) => t!("正则，例如 \\d{6} 或 ^https?://", "regex, e.g. \\d{6} or ^https?://"),
                (true, false) => t!("搜索剪贴板历史…", "search clipboard history…"),
                (false, false) => t!("按 / 搜索，? 查看按键", "/ to search, ? for keys"),
            };
            let edit = egui::TextEdit::singleline(&mut self.query)
                .hint_text(RichText::new(hint).color(pal.faint))
                .desired_width(ui.available_width())
                .font(FontId::proportional(15.0))
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
            if self.search_mode && !self.sync_open && self.editor.is_none() && self.settings.is_none() && self.actions.is_none() {
                r.request_focus();
            }
        });
        // shell prompt
        let p = ui.painter();
        let color = if self.search_error.is_some() {
            pal.danger
        } else if self.search_mode {
            pal.accent
        } else {
            pal.dim
        };
        let prompt = if self.regex { "~/" } else { ">" };
        p.text(Pos2::new(rect.left() + 18.0, rect.center().y), Align2::LEFT_CENTER, prompt, FontId::proportional(16.0), color);
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, pal.border));
    }

    /// "全部 ⌄" button opening the list of groups (like Raycast's type filter).
    fn group_dropdown(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let full = self.groups.list.get(self.group).map(|g| g.name.clone()).unwrap_or_default();
        // long names would squeeze the search field
        let mut name = full.clone();
        let width = |t: &str| ui.painter().layout_no_wrap(t.to_string(), FontId::proportional(13.0), pal.dim).size().x;
        if width(&name) > 180.0 {
            while !name.is_empty() && width(&format!("{name}…")) > 180.0 {
                name.pop();
            }
            name.push('…');
        }
        let resp = ui
            .add(
                egui::Button::new(RichText::new(format!("{name}    ")).size(13.0).color(pal.dim))
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::new(1.0, pal.border))
                    .corner_radius(2)
                    .min_size(Vec2::new(0.0, 28.0)),
            )
            .on_hover_text(tf!("{full}\n分组（{KEY_GROUP}1…9 / Tab 切换）", "{full}\ngroup (gt / gT / Tab, {KEY_GROUP}1…9)"));
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
                let label = if self.groups.default == g.name { tf!("{}  · 默认", "{}  · default", g.name) } else { g.name.clone() };
                p.text(row.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(13.0), pal.text);
                let mut right = match self.counts.get(i) {
                    Some(Some(n)) if self.counts_more && i > 0 => format!("{n}+"),
                    Some(Some(n)) => n.to_string(),
                    Some(None) => t!("规则无效", "invalid rule").into(),
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
            ui.painter().text(row.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, t!("管理分组…", "manage groups…"), FontId::proportional(13.0), pal.text);
            ui.painter().text(row.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, t!("右键：设为默认", "right click: make default"), FontId::proportional(11.0), pal.faint);
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
                Ok(()) => self.notify(tf!("打开时将默认显示「{}」", "“{}” now opens by default", self.groups.default)),
                Err(e) => self.notify(tf!("保存失败：{e}", "save failed: {e}")),
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
                    t!("没有匹配的条目", "no matches")
                } else if self.group == 0 {
                    t!("剪贴板历史为空\n复制点什么试试吧", "clipboard history is empty\ncopy something")
                } else {
                    t!("这个分组里还没有内容", "nothing in this group yet")
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
                        let y = rect.bottom() - 15.0;
                        let p = ui.painter();
                        let label = p.text(Pos2::new(rect.left() + 30.0, y), Align2::LEFT_CENTER, name, FontId::proportional(11.5), pal.faint);
                        let stroke = Stroke::new(1.0, pal.border.gamma_multiply(0.6));
                        p.hline(rect.left() + 12.0..=label.left() - 6.0, y, stroke);
                        p.hline(label.right() + 6.0..=rect.right() - 12.0, y, stroke);
                    }
                    Line::Item(i) => {
                        let meta = self.items[i].clone();
                        let row = rect.shrink2(Vec2::new(4.0, 0.0));
                        let resp = ui.interact(row, egui::Id::unique(("row", meta.id)), Sense::click());
                        let selected = i == self.selected;
                        let fill = if selected {
                            pal.selected
                        } else if resp.hovered() {
                            pal.hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        ui.painter().rect_filled(row, 0, fill);
                        if selected {
                            // cursor bar, like a selected line in a terminal UI
                            let bar = Rect::from_min_size(row.min, Vec2::new(3.0, row.height()));
                            ui.painter().rect_filled(bar, 0, pal.accent);
                        }
                        let kind = if meta.kind == Kind::Image { TextKind::Image } else { classify(&meta.preview, false) };
                        // line number (relative to the cursor, like `set relativenumber`, absolute on the cursor line)
                        let num = if selected { format!("{}", i + 1) } else { format!("{}", (i as isize - self.selected as isize).unsigned_abs()) };
                        ui.painter().text(
                            Pos2::new(row.left() + 34.0, row.center().y),
                            Align2::RIGHT_CENTER,
                            num,
                            FontId::proportional(11.5),
                            if selected { pal.accent } else { pal.faint },
                        );
                        let tile = Rect::from_min_size(Pos2::new(row.left() + 42.0, row.center().y - 9.0), Vec2::new(36.0, 18.0));
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
                            let key = self.groups.boards().nth(1).and(self.groups.boards().find(|(_, g)| g.board == meta.board)).map(|(_, g)| g.key.clone()).filter(|k| !k.is_empty());
                            match key {
                                // several pin groups: show which one (its key, like a Vim mark)
                                Some(k) => {
                                    let r = painter.text(Pos2::new(right, row.center().y), Align2::RIGHT_CENTER, format!("'{k}"), FontId::monospace(11.5), pal.accent);
                                    right = r.left() - 8.0;
                                }
                                None => {
                                    painter.circle_filled(Pos2::new(right - 2.0, row.center().y), 3.0, pal.accent);
                                    right -= 12.0;
                                }
                            }
                        }
                        let x = tile.right() + 10.0;
                        let title = match meta.kind {
                            Kind::Text => one_line(&meta.preview),
                            Kind::Image => meta
                                .ocr
                                .as_deref()
                                .map(one_line)
                                .filter(|t| !t.is_empty() && t != t!("(空白)", "(blank)"))
                                .unwrap_or_else(|| tf!("图片 {}×{}", "image {}×{}", meta.width, meta.height)),
                        };
                        let mut job = LayoutJob::single_section(title, TextFormat { font_id: FontId::proportional(13.0), color: if selected { pal.accent } else { pal.text }, ..Default::default() });
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

    /// Type column of a row: a terminal-style tag ("url", "code" …), a color
    /// swatch for colors, a small thumbnail for images.
    fn paint_tile(&mut self, ui: &egui::Ui, ctx: &egui::Context, pal: &Palette, tile: Rect, meta: &ItemMeta, kind: TextKind) {
        let p = ui.painter();
        if meta.kind == Kind::Image {
            if let Some(tex) = self.thumb(ctx, meta.id) {
                let ts = tex.size_vec2();
                let r = Rect::from_center_size(tile.center(), Vec2::new(tile.width(), tile.height()));
                let s = (r.width() / ts.x).min(r.height() / ts.y);
                let img = Rect::from_center_size(r.center(), ts * s);
                egui::Image::from_texture((tex.id(), img.size())).paint_at(ui, img);
                p.rect_stroke(img, 0, Stroke::new(1.0, pal.border), egui::StrokeKind::Outside);
                return;
            }
        }
        if kind == TextKind::Color {
            if let Some(c) = kinds::parse_color(&meta.preview).map(color32) {
                let sw = Rect::from_center_size(tile.center(), Vec2::new(tile.width() - 6.0, tile.height() - 4.0));
                p.rect_filled(sw, 0, c);
                p.rect_stroke(sw, 0, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
                return;
            }
        }
        let tag = match kind {
            TextKind::Plain => "txt",
            TextKind::Url => "url",
            TextKind::Email => "mail",
            TextKind::Path => "path",
            TextKind::Color => "hex",
            TextKind::Json => "json",
            TextKind::Code => "code",
            TextKind::Image => "img",
        };
        p.text(tile.left_center(), Align2::LEFT_CENTER, tag, FontId::proportional(11.5), pal.tint(kind));
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
        let mut rows: Vec<(&str, String)> = vec![(t!("类型", "type"), kind.label().to_string()), (t!("来源", "source"), meta.source.clone().unwrap_or_else(|| t!("本机", "this device").into()))];
        rows.push(match meta.kind {
            Kind::Text => (t!("内容", "content"), tf!("{} 字符 · {} 行", "{} chars · {} lines", meta.chars, meta.lines)),
            Kind::Image => (t!("尺寸", "dimensions"), format!("{} × {}", meta.width, meta.height)),
        });
        rows.push((t!("大小", "size"), human_size(meta.bytes)));
        rows.push((t!("复制时间", "copied"), abs_time(meta.created, self.tz)));
        let mut meta_h = 30.0 + rows.len() as f32 * 25.0;

        egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 14, bottom: 8 }).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.spacing_mut().item_spacing.y = 0.0; // rows below are measured exactly
            // a short window: the content wins, the info table goes
            let show_meta = ui.available_height() >= meta_h + 120.0;
            if !show_meta {
                meta_h = 0.0;
            }
            let body = Vec2::new(ui.available_width(), (ui.available_height() - meta_h).max(60.0));
            ui.allocate_ui_with_layout(body, Layout::top_down(Align::LEFT), |ui| {
                ui.set_min_size(body);
                ui.set_max_height(body.y);
                ui.spacing_mut().item_spacing.y = 6.0;
                self.preview_body(ui, pal, &meta, kind);
            });
            if !show_meta {
                return;
            }
            // metadata
            let w = ui.available_width();
            let (title, _) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::hover());
            ui.painter().hline(title.x_range(), title.top() + 6.0, Stroke::new(1.0, pal.border));
            ui.painter().text(Pos2::new(title.left(), title.bottom() - 4.0), Align2::LEFT_BOTTOM, t!("信息", "info"), FontId::proportional(11.5), pal.faint);
            for (k, v) in rows {
                let (r, _) = ui.allocate_exact_size(Vec2::new(w, 25.0), Sense::hover());
                let p = ui.painter();
                let key = p.text(r.left_center(), Align2::LEFT_CENTER, k, FontId::proportional(12.0), pal.dim);
                let value = elided(p, &v, FontId::proportional(12.0), pal.text, r.right() - key.right() - 16.0);
                p.galley(Pos2::new(r.right() - value.size().x, r.center().y - value.size().y / 2.0), value, pal.text);
                p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, pal.border.gamma_multiply(0.6)));
            }
        });
    }

    fn preview_body(&mut self, ui: &mut egui::Ui, pal: &Palette, meta: &ItemMeta, kind: TextKind) {
        let id = meta.id;
        let Some((_, detail)) = &self.detail else { return };
        match detail {
            Detail::Missing => {
                ui.label(RichText::new(t!("无法读取内容", "cannot read the content")).color(pal.danger));
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
                        ui.label(RichText::new(t!("图中文字", "text in the image")).size(11.5).strong().color(pal.dim));
                        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(("ocr", id)).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(t).size(12.5).color(pal.text)).wrap().selectable(true));
                        });
                    }
                    (None, None) if self.ocr_available => {
                        ui.label(RichText::new(t!("正在识别图中文字…", "recognizing text…")).size(11.5).color(pal.faint));
                    }
                    (None, None) => {
                        ui.label(RichText::new(t!("未启用图片文字识别", "text recognition is off")).size(11.5).color(pal.faint));
                    }
                    _ => {
                        ui.label(RichText::new(t!("图中未识别到文字", "no text found in the image")).size(11.5).color(pal.faint));
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
                                if ui.button(t!("在文件管理器中打开", "show in file manager")).clicked() {
                                    open_external(&dir.to_string_lossy());
                                }
                            }
                            None => {
                                ui.label(RichText::new(t!("本机上不存在这个路径", "this path does not exist here")).size(11.5).color(pal.faint));
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
                        (t!("配对成功，已开始同步", "paired, syncing").into(), false)
                    } else {
                        (t!("配对窗口已关闭", "pairing window closed").into(), false)
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
                    Ok(name) => (tf!("已与 {name} 配对，开始同步", "paired with {name}, syncing"), false),
                    Err(e) => (e, true),
                });
                self.sync_input.clear();
                self.refresh_sync();
            }
        }
        let info = self.sync.clone();
        if dialog_vim_keys(ctx).2 {
            self.sync_open = false;
        }
        let modal = egui::Modal::new(egui::Id::unique("sync-panel"))
            .frame(dialog_frame(pal))
            .show(ctx, |ui| {
                let (w, h) = dialog_room(ctx, 420.0);
                ui.set_width(w);
                egui::ScrollArea::vertical().max_height(h).auto_shrink([false, true]).show(ui, |ui| {
                    ui.label(RichText::new(t!("局域网同步", "LAN sync")).size(16.0).strong().color(pal.text));
                    ui.label(RichText::new(t!("端到端加密 · 仅同一局域网 · 收到的内容只进入历史，不会改动剪贴板", "end-to-end encrypted · same LAN only · received items go to history, never to the clipboard")).size(11.5).color(pal.faint));
                    ui.add_space(8.0);
                    let Some(info) = info else {
                        ui.label(RichText::new(t!("无法获取同步状态", "cannot get the sync status")).color(pal.danger));
                        return;
                    };
                    if !info.enabled {
                        ui.label(RichText::new(t!("同步已在设置中关闭（sync_enabled = false）", "sync is off in the settings (sync_enabled = false)")).color(pal.dim));
                        return;
                    }
                    section(ui, pal, |ui| {
                        ui.label(RichText::new(tf!("本机：{}", "this device: {}", info.device_name)).color(pal.text));
                        if info.paired {
                            if info.peers.is_empty() {
                                ui.label(RichText::new(t!("暂无设备在线（已配对的设备在同一局域网时会自动连接）", "no device online (paired devices connect when on the same LAN)")).size(12.0).color(pal.faint));
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
                            ui.label(RichText::new(t!("尚未与其他设备配对", "not paired with any device")).size(12.0).color(pal.faint));
                        }
                    });
                    ui.add_space(8.0);

                    // Invite another device.
                    if let Some((code, addrs)) = self.sync_pairing.clone() {
                        ui.label(RichText::new(t!("在另一台设备的 copym 里输入配对码：", "enter this code in copym on the other device:")).size(12.5).color(pal.dim));
                        ui.label(RichText::new(&code).monospace().size(30.0).strong().color(pal.accent));
                        if let Some(a) = addrs.first() {
                            ui.label(RichText::new(tf!("找不到本机时，输入 {code}@{a}", "if not found, enter {code}@{a}")).size(11.5).color(pal.faint));
                        }
                        ui.label(RichText::new(t!("3 分钟内有效，只能尝试一次", "valid for 3 minutes, one attempt")).size(11.5).color(pal.faint));
                    } else if ui.add(primary_button(pal, t!("添加设备（生成配对码）", "add a device (pairing code)"))).clicked() {
                        match self.req(&Request::SyncPair) {
                            Some(Response::Pairing { code, addrs, .. }) => {
                                self.sync_pairing = Some((code, addrs));
                                self.sync_msg = None;
                            }
                            _ => self.sync_msg = Some((self.error.take().unwrap_or_else(|| t!("无法开始配对", "cannot start pairing").into()), true)),
                        }
                        self.refresh_sync();
                    }
                    ui.add_space(10.0);

                    // Join another device.
                    ui.label(RichText::new(t!("加入另一台设备", "join another device")).size(12.5).color(pal.dim));
                    ui.horizontal(|ui| {
                        let busy = self.sync_job.is_some();
                        ui.add_enabled(
                            !busy,
                            egui::TextEdit::singleline(&mut self.sync_input).hint_text(t!("8 位配对码，可附 @IP:端口", "8-digit code, optionally @IP:port")).desired_width(270.0).margin(Margin::symmetric(8, 5)),
                        );
                        let label = if busy { t!("正在配对…", "pairing…") } else { t!("加入", "join") };
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
                            && ui.add(egui::Button::new(RichText::new(t!("退出同步组", "leave the sync group")).color(pal.danger))).on_hover_text(t!("忘记同步密钥并断开所有设备", "forget the sync key and disconnect all devices")).clicked()
                        {
                            self.req(&Request::SyncLeave);
                            self.sync_pairing = None;
                            self.sync_msg = Some((t!("已退出同步组", "left the sync group").into(), false));
                            self.refresh_sync();
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button(t!("关闭", "close")).clicked() {
                                self.sync_open = false;
                            }
                        });
                    });
                });
            });
        if modal.should_close() {
            self.sync_open = false;
        }
    }

    /// Dark or light: the `theme` setting, or the system's.
    fn wants_dark(&self, ctx: &egui::Context) -> bool {
        match self.cfg.theme.as_str() {
            "dark" => true,
            "light" => false,
            _ => {
                #[cfg(target_os = "linux")]
                if let Some(dark) = crate::system_theme::dark() {
                    return dark;
                }
                ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark)
            }
        }
    }

    /// A source that is an app on this machine (synced items carry the device name).
    fn is_local_app(&self, source: &str) -> bool {
        let lower = source.to_lowercase();
        let ignored = self.cfg.ignore_apps.iter().any(|a| !a.trim().is_empty() && lower.contains(&a.trim().to_lowercase()));
        let peer = self.sync.as_ref().is_some_and(|s| s.peers.iter().any(|(n, _)| n == source));
        !ignored && !peer
    }

    /// Save `ignore_apps` through the daemon (it applies them right away).
    fn set_ignored(&mut self, mut apps: Vec<String>) -> bool {
        apps.retain(|a| !a.trim().is_empty());
        apps.dedup();
        if self.req(&Request::SetIgnoredApps { apps: apps.clone() }).is_none() {
            return false;
        }
        self.cfg.ignore_apps = apps;
        true
    }


    fn group_editor(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(mut ed) = self.editor.take() else { return };
        if ed.count_dirty {
            ed.count_dirty = false;
            ed.store_bufs();
            let rule = ed.draft.rule(ed.sel);
            ed.count = match self.req(&Request::GroupCounts { query: String::new(), regex: false, groups: vec![rule] }) {
                Some(Response::GroupCounts { counts, .. }) => counts.into_iter().next(),
                _ => None,
            };
        }
        let mut keep = true;
        let mut saved = false;
        // keyboard: j/k groups, Ctrl-S save, q / Esc close; Esc in a field only leaves it
        let (nav, save_key, close_key) = dialog_vim_keys(ctx);
        if nav != 0 && !ed.draft.list.is_empty() {
            ed.store_bufs();
            let n = ed.draft.list.len() as isize;
            ed.select((ed.sel as isize + nav).rem_euclid(n) as usize);
        }
        if close_key {
            keep = false;
        }
        const BODY_H: f32 = 296.0;
        let modal = egui::Modal::new(egui::Id::unique("group-editor")).frame(dialog_frame(pal).inner_margin(Margin::same(16))).show(ctx, |ui| {
            let (w, h) = dialog_room(ctx, 560.0);
            ui.set_width(w);
            ui.spacing_mut().item_spacing.y = 5.0;
            ui.label(RichText::new(t!("管理分组", "manage groups")).size(16.0).strong().color(pal.text));
            ui.label(
                RichText::new(tf!("分组按顺序对应 {KEY_GROUP}1…9；条目可以同时属于多个分组。规则保存在 groups.toml。", "groups map to {KEY_GROUP}1…9 in order; an item can be in several groups. Saved in groups.toml."))
                    .size(11.5)
                    .color(pal.faint),
            );
            ui.add_space(8.0);
            // title and buttons stay; the body scrolls when the window is short
            egui::ScrollArea::vertical().id_salt("ged-body").max_height((h - 100.0).max(120.0)).auto_shrink([false, true]).show(ui, |ui| {
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
                                        label.push_str(t!("  · 默认", "  · default"));
                                    }
                                                    let button = egui::Button::selectable(i == ed.sel, label).min_size(Vec2::new(146.0, 26.0)).truncate();
                                    if ui.add(button).on_hover_text(&g.name).clicked() {
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
                            if ui.small_button(t!("+ 新建", "+ new")).clicked() {
                                ed.store_bufs();
                                let n = ed.draft.list.len() + 1;
                                ed.draft.list.push(GroupDef { name: tf!("分组 {n}", "group {n}"), ..Default::default() });
                                ed.sel = ed.draft.list.len() - 1;
                                ed.load_bufs();
                            }
                            let can_delete = ed.draft.list.len() > 1;
                            if ui.add_enabled(can_delete, egui::Button::new(t!("删除", "delete")).small()).clicked() {
                                ed.draft.list.remove(ed.sel);
                                ed.sel = ed.sel.min(ed.draft.list.len() - 1);
                                ed.load_bufs();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if ui.add_enabled(ed.sel > 0, egui::Button::new(t!("上移", "up")).small()).clicked() {
                                ed.store_bufs();
                                ed.draft.list.swap(ed.sel, ed.sel - 1);
                                ed.sel -= 1;
                            }
                            if ui.add_enabled(ed.sel + 1 < ed.draft.list.len(), egui::Button::new(t!("下移", "down")).small()).clicked() {
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
                        let next_board = ed.draft.next_board();
                        let Some(g) = ed.draft.list.get_mut(sel) else { return };
                        let mut changed = false;
                        let old_name = g.name.clone();
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(t!("名称", "name")).size(12.0).color(pal.dim));
                            changed |= ui.add(egui::TextEdit::singleline(&mut g.name).desired_width(f32::INFINITY).margin(Margin::symmetric(8, 4))).changed();
                        });
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(t!("内容类型", "content")).size(12.0).color(pal.dim));
                            for (v, l) in [("", t!("全部", "all")), ("text", t!("文本", "text")), ("image", t!("图片", "images"))] {
                                changed |= ui.selectable_value(&mut g.kind, v.to_string(), l).changed();
                            }
                        });
                        changed |= ui.checkbox(&mut g.pinned, t!("只包含固定的条目", "pinned items only")).changed();
                        changed |= ui
                            .checkbox(&mut g.rest, t!("“其他”分组：不属于任何带规则分组的条目", "“other” group: items no rule-based group takes"))
                            .on_hover_text(t!("排除其他所有分组的内容正则和来源应用正则所匹配的条目", "leaves out what the content and source rules of the other groups match"))
                            .changed();
                        let mut is_board = g.board > 0;
                        if ui
                            .checkbox(&mut is_board, t!("固定分组：条目手动放进来（m + 按键，或全局快捷键）", "pin group: items are put here by hand (m + key, or a global shortcut)"))
                            .changed()
                        {
                            g.board = if is_board { next_board } else { 0 };
                            g.pinned = is_board;
                            changed = true;
                        }
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(t!("按键", "key")).size(12.0).color(pal.dim));
                            changed |= ui.add(egui::TextEdit::singleline(&mut g.key).char_limit(1).desired_width(24.0).margin(Margin::symmetric(6, 4))).changed();
                            let hint = if g.board > 0 { t!("' 跳到分组 · m 放进分组", "' go to · m put into") } else { t!("' 跳到分组", "' go to the group") };
                            ui.label(RichText::new(hint).size(11.5).color(pal.faint));
                        });
                        if g.board > 0 {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(t!("全局快捷键", "global shortcut")).size(12.0).color(pal.dim));
                                changed |= ui
                                    .add(egui::TextEdit::singleline(&mut g.hotkey).hint_text("Ctrl+Alt+1").desired_width(150.0).margin(Margin::symmetric(8, 4)))
                                    .on_hover_text(t!("不用打开弹窗，把当前剪贴板的内容放进这个分组；重启 copym 后生效", "puts the current clipboard into this group without opening the popup; takes effect after restarting copym"))
                                    .changed();
                            });
                        }
                        let rules_shown = g.board == 0;
                        ui.add_space(4.0);
                        if rules_shown {
                        ui.label(RichText::new(t!("内容正则（每行一条，匹配任意一条即可；(?i) 忽略大小写）", "content regexes (one per line, any may match; (?i) ignores case)")).size(12.0).color(pal.dim));
                        changed |= ui
                            .add(
                                egui::TextEdit::multiline(&mut ed.patterns)
                                    .font(FontId::monospace(12.5))
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY)
                                    .hint_text(t!(r"例：^\s*https?://", r"e.g. ^\s*https?://")),
                            )
                            .changed();
                        ui.label(RichText::new(t!("来源正则：来源应用或同步设备名（每行一条，可留空）", "source regexes: app or synced device name (one per line, optional)")).size(12.0).color(pal.dim));
                        changed |= ui
                            .add(
                                egui::TextEdit::multiline(&mut ed.apps)
                                    .font(FontId::monospace(12.5))
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY)
                                    .hint_text(t!("例：(?i)chrome|firefox", "e.g. (?i)chrome|firefox")),
                            )
                            .changed();
                        }
                        let g = &ed.draft.list[sel];
                        let mut is_default = default_name == g.name;
                        if ui.checkbox(&mut is_default, t!("打开弹窗时默认显示这个分组", "show this group when the popup opens")).changed() && is_default {
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
                                    Some(Some(n)) => tf!("当前历史中有 {n} 条属于这个分组", "{n} items of the history are in this group"),
                                    _ => String::new(),
                                };
                                ui.label(RichText::new(txt).size(12.0).color(pal.ok));
                            }
                        }
                    });
            });
            });
            if let Some(e) = &ed.error {
                ui.label(RichText::new(e).size(12.0).color(pal.danger));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button(t!("恢复默认分组", "reset to default groups")).clicked() {
                    ed.draft = Groups::default();
                    ed.sel = 0;
                    ed.load_bufs();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(primary_button(pal, t!("保存", "save"))).on_hover_text("Ctrl-S").clicked() || save_key {
                        ed.store_bufs();
                        match ed.draft.validate().err() {
                            Some(e) => ed.error = Some(e),
                            None => match groups::save(&ed.draft) {
                                Ok(()) => saved = true,
                                Err(e) => ed.error = Some(tf!("保存失败：{e}", "save failed: {e}")),
                            },
                        }
                    }
                    if ui.button(t!("取消", "cancel")).clicked() {
                        keep = false;
                    }
                });
            });
        });
        if modal.should_close() {
            keep = false;
        }
        if saved {
            let shortcuts = |g: &Groups| -> Vec<(u32, String)> { g.boards().map(|(_, d)| (d.board, d.hotkey.trim().to_string())).collect() };
            let restart = shortcuts(&self.groups) != shortcuts(&ed.draft);
            let current = self.groups.list.get(self.group).map(|g| g.name.clone());
            self.groups = ed.draft;
            self.groups_error = None;
            self.group = current.and_then(|n| self.groups.list.iter().position(|g| g.name == n)).unwrap_or(0);
            self.dirty = true;
            if restart {
                self.notify(t!("分组已保存，全局快捷键在重启 copym 后生效（:restart）", "groups saved; global shortcuts take effect after a restart (:restart)"));
            } else {
                self.notify(t!("分组已保存", "groups saved"));
            }
        } else if keep {
            self.editor = Some(ed);
        }
    }

    /// Bottom action bar: context on the left, primary action and the action
    /// panel shortcut on the right.
    /// Vim-like status line: mode, context, pending keys, key hints; the
    /// `:` command line replaces it in COMMAND mode.
    fn footer(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), FOOTER_H), Sense::hover());
        ui.painter().hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, pal.border));
        let mode = self.mode();
        let (label, color) = match mode {
            Mode::Normal => (" NORMAL ", pal.accent),
            Mode::Insert => (" INSERT ", pal.tint(TextKind::Url)),
            Mode::Command => (" COMMAND ", pal.tint(TextKind::Json)),
        };
        let g = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(12.0), pal.on_accent);
        let block = Rect::from_min_size(Pos2::new(rect.left() + 10.0, rect.center().y - 10.0), Vec2::new(g.size().x + 4.0, 20.0));
        ui.painter().rect_filled(block, 0, color);
        ui.painter().galley(Pos2::new(block.left() + 2.0, block.center().y - g.size().y / 2.0), g, pal.on_accent);

        if let Some(cmd) = &mut self.cmdline {
            let r = Rect::from_min_max(Pos2::new(block.right() + 10.0, rect.top() + 6.0), Pos2::new(rect.right() - 12.0, rect.bottom() - 6.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(r).layout(Layout::left_to_right(Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(RichText::new(":").font(FontId::proportional(13.5)).color(pal.text));
                let edit = egui::TextEdit::singleline(cmd)
                    .id(egui::Id::unique("cmdline"))
                    .font(FontId::proportional(13.5))
                    .text_color(pal.text)
                    .frame(egui::Frame::NONE)
                    .margin(Margin::ZERO)
                    .desired_width(ui.available_width());
                ui.add(edit).request_focus();
            });
            return;
        }

        // right: key hints (clickable)
        let inner = rect.shrink2(Vec2::new(10.0, 6.0));
        let mut open_actions = false;
        let mut copy = false;
        let mut quit = false;
        let mut restart = false;
        let mut close = false;
        let mut help = false;
        let mut open_url = None;
        let buttons = ui.scope_builder(egui::UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let power = power_button(ui, pal).on_hover_text(t!("关闭 / 重启 / 退出", "close / restart / quit"));
            egui::Popup::menu(&power).show(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let entries = [
                    (crate::icons::X, t!("关闭弹窗", "close popup"), "Esc  :q", &mut close),
                    (crate::icons::REFRESH_CW, t!("重启 copym", "restart copym"), ":restart", &mut restart),
                    (crate::icons::LOG_OUT, t!("退出 copym", "quit copym"), ":q!", &mut quit),
                ];
                for (icon, label, key, flag) in entries {
                    let (row, resp) = ui.allocate_exact_size(Vec2::new(200.0, 28.0), Sense::click());
                    let p = ui.painter();
                    if resp.hovered() {
                        p.rect_filled(row, 6, pal.hover);
                    }
                    p.text(row.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, icon, icon_font(14.0), pal.dim);
                    p.text(row.left_center() + Vec2::new(32.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(13.0), pal.text);
                    p.text(row.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, key, FontId::proportional(11.5), pal.faint);
                    if resp.clicked() {
                        *flag = true;
                        ui.close();
                    }
                }
            });
            if bar_button(ui, pal, t!("帮助", "help"), "?", false).clicked() {
                help = true;
            }
            if bar_button(ui, pal, t!("菜单", "menu"), "\u{f1050}", false).clicked() {
                open_actions = true;
            }
            let label = if self.cfg.paste_on_enter { t!("粘贴", "paste") } else { t!("复制", "yank") };
            if !self.items.is_empty() && bar_button(ui, pal, label, "⏎", true).clicked() {
                copy = true;
            }
            if let Some(u) = &self.update {
                let link = ui
                    .add(egui::Label::new(RichText::new(tf!("[新版本 {}]", "[v{} available]", u.version)).size(12.0).color(pal.accent)).sense(Sense::click()))
                    .on_hover_text(t!("打开下载页面", "open the download page"))
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if link.clicked() {
                    open_url = Some(u.url.clone());
                }
            }
        });
        if let Some(url) = open_url {
            open_external(&url);
        }

        // middle: group · count · state, then the keys typed so far (showcmd)
        let p = ui.painter();
        // the group's count when known (pages load lazily), else what is loaded
        let total = match self.counts.get(self.group).copied().flatten() {
            Some(n) => format!("{n}{}", if self.counts_more { "+" } else { "" }),
            None => format!("{}{}", self.items.len(), if self.next.is_some() { "+" } else { "" }),
        };
        let mut ctx_text = format!("{}  {}", self.groups.list.get(self.group).map(|g| g.name.as_str()).unwrap_or(""), tf!("{} 条", "{} items", total));
        if !self.items.is_empty() {
            ctx_text = format!("{}  {}/{}", ctx_text, self.selected + 1, total);
        }
        if self.next.is_some() && self.items.len() < PAGE as usize && !self.query.is_empty() {
            ctx_text.push_str(t!("  搜索中…", "  searching…"));
        }
        if self.paused {
            ctx_text.push_str(t!("  [已暂停]", "  [paused]"));
        }
        if let Some(sync) = self.sync.as_ref().filter(|s| s.paired) {
            ctx_text.push_str(&tf!("  同步:{}", "  sync:{}", sync.peers.len()));
        }
        // which-key: after m / ' list the groups that answer to a key
        let marks = |boards_only: bool| -> String {
            self.groups
                .list
                .iter()
                .filter(|g| (!boards_only || g.board > 0) && !g.key.is_empty())
                .map(|g| format!("{} {}", g.key, g.name))
                .collect::<Vec<_>>()
                .join("  ")
        };
        match self.pending.trim_start_matches(|c: char| c.is_ascii_digit()) {
            "m" => ctx_text = format!("{}  {}", t!("放进:", "put into:"), marks(true)),
            "'" | "`" => ctx_text = format!("{}  {}", t!("跳到:", "go to:"), marks(false)),
            _ => {}
        }
        let x = block.right() + 10.0;
        let showcmd_w = if self.pending.is_empty() { 0.0 } else { 60.0 };
        let room = buttons.response.rect.left() - 12.0 - showcmd_w;
        let left = elided(p, &ctx_text, FontId::proportional(12.0), pal.dim, room - x);
        p.galley(Pos2::new(x, rect.center().y - left.size().y / 2.0), left, pal.dim);
        if !self.pending.is_empty() {
            p.text(Pos2::new(room + showcmd_w, rect.center().y), Align2::RIGHT_CENTER, &self.pending, FontId::proportional(12.5), pal.accent);
        }
        if open_actions {
            self.actions = if self.actions.is_some() { None } else { Some(0) };
        }
        if copy {
            self.pending_activate = Some(self.cfg.paste_on_enter);
        }
        if quit {
            self.pending_quit = true;
        }
        if restart {
            self.pending_restart = true;
        }
        if close {
            self.close(&ui.ctx().clone());
        }
        if help {
            self.help = true;
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
                        egui::ScrollArea::vertical().max_height(screen.height() - FOOTER_H - 30.0).show(ui, |ui| {
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
                                PanelRow::Act(a, glyph, label, keys) => {
                                    let (r, resp) = ui.allocate_exact_size(Vec2::new(300.0, 32.0), Sense::click());
                                    if idx == sel {
                                        ui.painter().rect_filled(r, 7, pal.selected);
                                    }
                                    let danger = matches!(a, Act::Delete);
                                    let color = if danger { pal.danger } else { pal.text };
                                    ui.painter().text(r.left_center() + Vec2::new(20.0, 0.0), Align2::CENTER_CENTER, *glyph, icon_font(15.0), if danger { pal.danger } else { pal.dim });
                                    let key_w = if keys.is_empty() { 0.0 } else { paint_keycap(ui.painter(), pal, r.right_center() - Vec2::new(10.0, 0.0), keys) + 14.0 };
                                    let label = elided(ui.painter(), label, FontId::proportional(13.5), color, r.width() - 46.0 - key_w);
                                    ui.painter().galley(Pos2::new(r.left() + 36.0, r.center().y - label.size().y / 2.0), label, color);
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
        self.draw(ui);
    }
}

impl App {
    fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        if self.resident.as_ref().is_some_and(|r| r.show.swap(false, Ordering::SeqCst)) {
            self.reset_for_show(&ctx);
        }
        if self.hidden {
            return;
        }
        if self.close_req.swap(false, Ordering::SeqCst) {
            self.close(&ctx);
        }
        // Close when focus moves elsewhere (like a native popup).
        if let Some(focused) = ctx.input(|i| i.viewport().focused) {
            if focused {
                self.had_focus = true;
            } else if self.had_focus && self.cfg.close_on_focus_lost {
                self.close(&ctx);
            }
        }

        let mut action = self.handle_keys(&ctx);
        if self.dirty {
            self.refresh();
        } else {
            self.load_more(&ctx);
        }
        let dark = self.wants_dark(&ctx);
        let theme = if dark { egui::Theme::Dark } else { egui::Theme::Light };
        if ctx.theme() != theme {
            ctx.set_theme(theme);
            ctx.request_repaint();
        }
        let pal = Palette::new(dark);
        ctx.all_styles_mut(|s| {
            let v = &mut s.visuals;
            v.selection.bg_fill = pal.accent.gamma_multiply(if pal.dark { 0.3 } else { 0.25 });
            v.selection.stroke = Stroke::new(1.0, pal.accent);
            v.extreme_bg_color = if pal.dark { Color32::from_rgb(4, 8, 5) } else { Color32::from_rgb(238, 234, 218) };
            v.faint_bg_color = pal.hover;
            v.code_bg_color = pal.hover;
            v.hyperlink_color = pal.accent;
            v.window_fill = pal.surface;
            v.window_stroke = Stroke::new(1.0, pal.border);
            v.panel_fill = pal.bg;
            v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(if pal.dark { 110 } else { 40 }) };
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal.border);
            v.widgets.noninteractive.fg_stroke.color = pal.text;
            v.widgets.inactive.bg_fill = pal.hover;
            v.widgets.inactive.weak_bg_fill = pal.hover;
            v.widgets.inactive.bg_stroke = Stroke::new(1.0, pal.border);
            v.widgets.inactive.fg_stroke.color = pal.text;
            v.widgets.hovered.bg_fill = pal.selected;
            v.widgets.hovered.weak_bg_fill = pal.selected;
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, pal.border.gamma_multiply(1.6));
            v.widgets.hovered.fg_stroke.color = pal.text;
            v.widgets.active.bg_fill = pal.selected;
            v.widgets.active.weak_bg_fill = pal.selected;
            v.widgets.active.fg_stroke.color = pal.text;
            v.widgets.open.weak_bg_fill = pal.selected;
        });

        egui::Frame::new()
            .fill(pal.bg)
            .stroke(Stroke::new(1.0, pal.border))
            .corner_radius(CornerRadius::same(3))
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
                            if self.error.is_some() && ui.small_button(t!("重试", "retry")).clicked() {
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

        if pal.dark {
            // faint CRT scanlines
            let r = ctx.content_rect();
            let p = ui.painter();
            let mut y = r.top();
            while y < r.bottom() {
                p.hline(r.x_range(), y, Stroke::new(1.0, Color32::from_black_alpha(26)));
                y += 3.0;
            }
        }

        if let Some(a) = self.action_panel(&ctx, &pal) {
            action = Some(a);
        }
        if let Some(paste) = self.pending_activate.take() {
            action = Some(Action::Activate(self.selected, paste));
        }
        if std::mem::take(&mut self.pending_quit) {
            self.quit(&ctx);
        }
        if std::mem::take(&mut self.pending_restart) {
            self.restart();
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
        if let Some(rx) = &self.update_job {
            match rx.try_recv() {
                Ok(res) => {
                    self.update_job = None;
                    match res {
                        Ok(Some(u)) => {
                            self.notify(tf!("有新版本 {}，在操作面板里选“下载新版本”", "version {} is available: menu › download", u.version));
                            self.update = Some(u);
                        }
                        Ok(None) => self.notify(tf!("已是最新版本（v{}）", "up to date (v{})", env!("CARGO_PKG_VERSION"))),
                        Err(e) => self.notify(e),
                    }
                }
                Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
                Err(mpsc::TryRecvError::Disconnected) => self.update_job = None,
            }
        }
        if self.sync_open {
            self.sync_panel(&ctx, &pal);
        }
        if self.editor.is_some() {
            self.group_editor(&ctx, &pal);
        }
        if self.settings.is_some() {
            self.settings_panel(&ctx, &pal);
        }
        if self.help {
            self.help_overlay(&ctx, &pal);
        }

        if let Some(a) = action {
            self.perform(&ctx, a);
        }
    }
}

// ---------------------------------------------------------------- widgets --

/// Width and the content height a dialog gets: `want` wide, but always inside
/// the window (frame margins and a small gap left around it).
fn dialog_room(ctx: &egui::Context, want: f32) -> (f32, f32) {
    let screen = ctx.content_rect();
    (want.min(screen.width() - 60.0).max(200.0), (screen.height() - 70.0).max(120.0))
}

/// Keys of a dialog with text fields: (j/k step, Ctrl-S, q/Esc close).
/// While a field is being edited only Esc is taken (it leaves the field).
fn dialog_vim_keys(ctx: &egui::Context) -> (isize, bool, bool) {
    let editing = settings::dialog_focus(ctx);
    let (nav, save, esc) = ctx.input_mut(|i| {
        let save = i.consume_key(Modifiers::CTRL, Key::S) || i.consume_key(Modifiers::COMMAND, Key::S);
        if editing {
            let leave = i.consume_key(Modifiers::NONE, Key::Escape);
            return (0, save, leave);
        }
        let mut nav = 0;
        let mut close = i.consume_key(Modifiers::NONE, Key::Escape);
        for e in &i.events {
            if let egui::Event::Text(t) = e {
                for c in t.chars() {
                    match c {
                        'j' => nav = 1,
                        'k' => nav = -1,
                        'q' => close = true,
                        _ => {}
                    }
                }
            }
        }
        i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
        (nav, save, close)
    });
    if editing && esc {
        // Esc in a text field: back to NORMAL, the dialog stays
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
        return (0, save, false);
    }
    (nav, save, esc)
}

/// One line of text, cut with "…" to fit `max_w`.
fn elided(p: &egui::Painter, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::single_section(text.to_string(), TextFormat { font_id: font, color, ..Default::default() });
    job.wrap.max_width = max_w.max(12.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    p.layout_job(job)
}

fn dialog_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.surface)
        .corner_radius(2)
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
        ui.painter().rect_filled(r, 0, pal.hover);
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
/// Returns the keycap's width.
fn paint_keycap(p: &egui::Painter, pal: &Palette, right_center: Pos2, key: &str) -> f32 {
    let g = p.layout_no_wrap(key.to_string(), FontId::proportional(11.0), pal.dim);
    let size = Vec2::new(g.size().x + 10.0, 20.0);
    let r = Rect::from_min_size(Pos2::new(right_center.x - size.x, right_center.y - size.y / 2.0), size);
    p.rect_stroke(r, 0, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
    p.galley(Pos2::new(r.left() + 5.0, r.center().y - g.size().y / 2.0), g, pal.dim);
    size.x
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
    if ui.add(primary_button(pal, t!("在浏览器中打开", "open in browser"))).clicked() {
        open_external(url);
    }
}

// ---------------------------------------------------------------- helpers --

/// `COPYM_TRACE=1`: print timings to stderr - wall clock (seconds within the
/// minute, comparable between the daemon and the popup process) and ms since
/// this process started. Hotkey to first frame = "frame 1" minus "hotkey".
pub(crate) fn trace(what: &str) {
    if std::env::var_os("COPYM_TRACE").is_some() {
        let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let since_start = crate::START.get().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
        eprintln!("[{:06.3} s | {since_start:>9.1} ms] {} {what}", (wall.as_millis() % 60_000) as f64 / 1000.0, std::process::id());
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
    if line.is_empty() { t!("(空白)", "(blank)").into() } else { line.replace('\t', "  ") }
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

/// Compact age, terminal style: now / 5m / 3h / 1d / 4d / 11-02.
fn short_time(t: i64, tz: i64) -> String {
    let now = now_secs();
    let d = (now - t).max(0);
    let days = (now + tz).div_euclid(86_400) - (t + tz).div_euclid(86_400);
    match (d, days) {
        (0..=59, _) => "now".into(),
        (60..=3599, _) => format!("{}m", d / 60),
        (_, 0) => format!("{}h", d / 3600),
        (_, 1..=6) => format!("{days}d"),
        _ => {
            let (y, m, dd, ..) = civil(t, tz);
            let (ny, ..) = civil(now, tz);
            if y == ny { format!("{m:02}-{dd:02}") } else { format!("{y}-{m:02}-{dd:02}") }
        }
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

    /// Every non-ASCII character in the UI's strings has a glyph in the
    /// bundled fonts, so no platform falls back to the system font (or boxes).
    #[test]
    fn bundled_fonts_cover_the_ui() {
        use skrifa::MetadataProvider;
        let t = std::time::Instant::now();
        let main = main_font().expect("bundled font decodes");
        eprintln!("decompressed {} bytes in {:?}", (*main.font).as_ref().len(), t.elapsed());
        let fonts: Vec<skrifa::FontRef> = [(*main.font).as_ref(), SYMBOLS_FONT, ICONS_FONT].iter().map(|b| skrifa::FontRef::new(b).unwrap()).collect();
        let sources = [include_str!("gui.rs"), include_str!("gui/settings.rs"), include_str!("gui/vim.rs"), include_str!("icons.rs")];
        let mut missing: Vec<char> = sources
            .iter()
            .flat_map(|s| s.lines().filter(|l| !l.trim_start().starts_with("//")))
            .flat_map(str::chars)
            .filter(|c| !c.is_ascii() && !c.is_control())
            .filter(|&c| !fonts.iter().any(|f| f.charmap().map(c).is_some()))
            .collect();
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "no glyph for {missing:?}");
    }

    /// Without a system font that has them (macOS), ⏎ ⇧ ⌥ used to render as boxes.
    #[test]
    fn key_symbols_have_glyphs() {
        let keys = "⌘⇧⌥⌃⏎⌫⎋↑↓↗▣"; // keycaps and item-kind tiles
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
        assert_eq!(short_time(now_secs() - 5, 0), "now");
        assert_eq!(short_time(now_secs() - 300, 0), "5m");
    }
}
