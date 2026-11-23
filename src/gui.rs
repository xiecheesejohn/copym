//! The popup window (egui / eframe, OpenGL renderer).
//!
//! It is a short-lived process: started by the daemon on hotkey, talks to the
//! daemon over IPC, and exits after an item is chosen. Nothing GUI related
//! stays in memory while the popup is closed.

use crate::config::Config;
use crate::imgutil;
use crate::ipc::Client;
use crate::model::{now_secs, Content, Filter, ItemMeta, Kind, Request, Response, SyncInfo};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Key,
    Layout, Margin, Modifiers, Rect, RichText, Sense, Stroke, TextFormat, TextureHandle, TextureOptions,
    ViewportCommand, Vec2,
};
use std::collections::HashMap;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const ROW_H: f32 = 70.0;
const ROW_GAP: f32 = 4.0;
const LIST_LIMIT: u32 = 1000;

pub fn run(open_sync: bool) -> Result<(), String> {
    let cfg = Config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("copym")
            .with_app_id("copym")
            .with_inner_size([820.0, 540.0])
            .with_min_inner_size([520.0, 340.0])
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
                s.interaction.selectable_labels = false;
            });
            let mut app = App::new(cfg);
            app.sync_open = open_sync;
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

struct Palette {
    bg: Color32,
    surface: Color32,
    hover: Color32,
    selected: Color32,
    accent: Color32,
    border: Color32,
    text: Color32,
    dim: Color32,
    danger: Color32,
}

impl Palette {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                bg: Color32::from_rgba_unmultiplied(22, 23, 29, 250),
                surface: Color32::from_rgb(31, 32, 41),
                hover: Color32::from_rgb(39, 41, 52),
                selected: Color32::from_rgb(44, 54, 92),
                accent: Color32::from_rgb(122, 146, 255),
                border: Color32::from_rgb(52, 54, 66),
                text: Color32::from_rgb(230, 232, 239),
                dim: Color32::from_rgb(139, 144, 163),
                danger: Color32::from_rgb(255, 120, 120),
            }
        } else {
            Self {
                bg: Color32::from_rgba_unmultiplied(248, 248, 251, 252),
                surface: Color32::WHITE,
                hover: Color32::from_rgb(238, 240, 246),
                selected: Color32::from_rgb(224, 231, 255),
                accent: Color32::from_rgb(70, 98, 230),
                border: Color32::from_rgb(222, 224, 232),
                text: Color32::from_rgb(27, 29, 36),
                dim: Color32::from_rgb(107, 112, 128),
                danger: Color32::from_rgb(210, 50, 50),
            }
        }
    }
}

// -------------------------------------------------------------------- app --

struct App {
    cfg: Config,
    client: Option<Client>,
    error: Option<String>,
    query: String,
    filter: Filter,
    items: Vec<ItemMeta>,
    selected: usize,
    dirty: bool,
    paused: bool,
    thumbs: HashMap<u64, Option<TextureHandle>>,
    full: Option<(u64, Option<Content>)>,
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
    frames: u64,
}

enum Action {
    Activate(usize),
    Delete(usize),
    Pin(usize),
}

impl App {
    fn new(cfg: Config) -> Self {
        let mut app = Self {
            cfg,
            client: None,
            error: None,
            query: String::new(),
            filter: Filter::All,
            items: Vec::new(),
            selected: 0,
            dirty: true,
            paused: false,
            thumbs: HashMap::new(),
            full: None,
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

    fn refresh(&mut self) {
        self.dirty = false;
        let req = Request::List { query: self.query.clone(), filter: self.filter, regex: self.regex, limit: LIST_LIMIT };
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

    fn full_content(&mut self, id: u64) -> Option<Content> {
        if let Some((fid, c)) = &self.full {
            if *fid == id {
                return c.clone();
            }
        }
        let c = match self.req(&Request::Get { id }) {
            Some(Response::Content(c)) => c,
            _ => None,
        };
        self.full = Some((id, c.clone()));
        c
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
        if self.sync_open {
            return None; // the sync dialog owns the keyboard
        }
        let mut action = None;
        let mut close = false;
        let mut nav = 0isize;
        let mut cycle = false;
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
                cycle = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::R) || i.consume_key(Modifiers::ALT, Key::R) {
                toggle_regex = true;
            }
            let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            for (n, k) in digits.iter().enumerate() {
                if i.consume_key(Modifiers::ALT, *k) || i.consume_key(Modifiers::COMMAND, *k) {
                    action = Some(Action::Activate(n));
                }
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
        if cycle {
            self.filter = match self.filter {
                Filter::All => Filter::Text,
                Filter::Text => Filter::Image,
                Filter::Image => Filter::Pinned,
                Filter::Pinned => Filter::All,
            };
            self.dirty = true;
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
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let logo = ui.add(
                egui::Label::new(RichText::new("◆ copym").size(15.0).strong().color(pal.accent)).sense(Sense::click_and_drag()),
            );
            if logo.drag_started() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }

            let right_w = 440.0;
            let search_w = (ui.available_width() - right_w).max(140.0);
            let edit = egui::TextEdit::singleline(&mut self.query)
                .hint_text(if self.regex { "正则搜索，例如 \\d{6} 或 ^https?://" } else { "搜索文本和图片中的文字…" })
                .desired_width(search_w)
                .font(FontId::proportional(14.5))
                .margin(Margin::symmetric(10, 7))
                .frame(
                    egui::Frame::new()
                        .fill(pal.surface)
                        .corner_radius(8)
                        .stroke(Stroke::new(1.0, pal.border)),
                );
            let resp = ui.add(edit);
            if resp.changed() {
                self.selected = 0;
                self.dirty = true;
            }
            if !self.sync_open {
                resp.request_focus();
            }

            let re_on = self.regex;
            let re_btn = ui
                .add(
                    egui::Button::new(RichText::new(".*").monospace().size(13.0).color(if re_on { pal.accent } else { pal.dim }))
                        .fill(if re_on { pal.selected } else { Color32::TRANSPARENT })
                        .corner_radius(6)
                        .stroke(Stroke::new(1.0, if re_on { pal.accent } else { pal.border })),
                )
                .on_hover_text("正则表达式搜索 (Ctrl+R)");
            if re_btn.clicked() {
                self.regex = !self.regex;
                self.dirty = true;
            }

            for (f, label) in [(Filter::All, "全部"), (Filter::Text, "文本"), (Filter::Image, "图片"), (Filter::Pinned, "固定")] {
                let on = self.filter == f;
                let text = RichText::new(label).size(13.0).color(if on { pal.accent } else { pal.dim });
                let r = ui.add(egui::Button::new(text).fill(if on { pal.selected } else { Color32::TRANSPARENT }).corner_radius(6).stroke(Stroke::NONE));
                if r.clicked() {
                    self.filter = f;
                    self.selected = 0;
                    self.dirty = true;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let close = ui.add(egui::Button::new(RichText::new("×").size(18.0).color(pal.dim)).frame(false)).on_hover_text("关闭 (Esc)");
                if close.clicked() {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
                let (icon, tip) = if self.paused { ("继续", "继续记录剪贴板") } else { ("暂停", "暂停记录剪贴板") };
                let pause = ui
                    .add(egui::Button::new(RichText::new(icon).size(13.0).color(if self.paused { pal.danger } else { pal.dim })).frame(false))
                    .on_hover_text(tip);
                let peers = self.sync.as_ref().map(|s| s.peers.len()).unwrap_or(0);
                let sync_label = if peers > 0 { format!("同步 · {peers}") } else { "同步".to_string() };
                let sync_btn = ui
                    .add(egui::Button::new(RichText::new(sync_label).size(13.0).color(if peers > 0 { pal.accent } else { pal.dim })).frame(false))
                    .on_hover_text("局域网加密同步");
                if sync_btn.clicked() {
                    self.sync_open = true;
                    self.sync_msg = None;
                    self.refresh_sync();
                }
                if pause.clicked() {
                    if let Some(Response::Status { paused, .. }) = self.req(&Request::SetPaused(None)) {
                        self.paused = paused;
                    }
                }
            });
        });
    }

    fn list(&mut self, ui: &mut egui::Ui, pal: &Palette) -> Option<Action> {
        let mut action = None;
        if let Some(err) = &self.search_error {
            ui.add_space(8.0);
            ui.label(RichText::new(err).color(pal.danger).size(12.5));
            return None;
        }
        if self.items.is_empty() {
            ui.centered_and_justified(|ui| {
                let msg = if self.query.is_empty() && self.filter == Filter::All { "剪贴板历史为空\n复制点什么试试吧" } else { "没有匹配的条目" };
                ui.label(RichText::new(msg).color(pal.dim).size(14.0));
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
                let width = ui.available_width() - 6.0;
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
                let selected = i == self.selected;
                let painter = ui.painter();
                let fill = if selected {
                    pal.selected
                } else if resp.hovered() {
                    pal.hover
                } else {
                    Color32::TRANSPARENT
                };
                painter.rect_filled(rect, 8, fill);
                if selected {
                    let bar = Rect::from_min_size(rect.min + Vec2::new(0.0, 12.0), Vec2::new(3.0, ROW_H - 24.0));
                    painter.rect_filled(bar, 2, pal.accent);
                }

                // index hint
                let badge = if i < 9 { format!("{}", i + 1) } else { String::new() };
                painter.text(rect.left_center() + Vec2::new(16.0, 0.0), Align2::CENTER_CENTER, badge, FontId::monospace(11.0), pal.dim);

                let mut x = rect.left() + 30.0;
                if meta.kind == Kind::Image {
                    if let Some(tex) = self.thumb(&ctx, meta.id) {
                        let ts = tex.size_vec2();
                        let s = ((ROW_H - 12.0) / ts.y).min(64.0 / ts.x);
                        let size = ts * s;
                        let r = Rect::from_min_size(egui::pos2(x, rect.center().y - size.y / 2.0), size);
                        ui.painter().image(tex.id(), r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                        x += 72.0;
                    }
                }
                let painter = ui.painter();
                let text_w = rect.right() - x - 34.0;
                let title = if meta.kind == Kind::Text {
                    clean_preview(&meta.preview)
                } else {
                    format!("图片  {} × {}", meta.width, meta.height)
                };
                let mut job = LayoutJob::single_section(title, TextFormat { font_id: FontId::proportional(13.5), color: pal.text, ..Default::default() });
                job.wrap.max_width = text_w.max(40.0);
                job.wrap.max_rows = if meta.kind == Kind::Text { 2 } else { 1 };
                job.wrap.break_anywhere = true;
                job.wrap.overflow_character = Some('…');
                let galley = painter.layout_job(job);
                // Center title + subtitle as one block.
                let sub_h = 15.0;
                let block_h = galley.size().y + 3.0 + sub_h;
                let top = rect.center().y - block_h / 2.0;
                let sub_y = top + galley.size().y + 3.0;
                painter.galley(egui::pos2(x, top), galley, pal.text);

                let sub = subtitle(&meta);
                painter.text(egui::pos2(x, sub_y), Align2::LEFT_TOP, sub, FontId::proportional(11.5), pal.dim);

                if meta.pinned {
                    painter.text(rect.right_top() + Vec2::new(-14.0, 14.0), Align2::CENTER_CENTER, "📌", FontId::proportional(12.0), pal.accent);
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

    fn preview(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let Some(meta) = self.items.get(self.selected).cloned() else { return };
        let ctx = ui.ctx().clone();
        egui::Frame::new()
            .fill(pal.surface)
            .corner_radius(10)
            .stroke(Stroke::new(1.0, pal.border))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                let info_h = 46.0;
                let body_h = (ui.available_height() - info_h).max(60.0);
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), body_h), Layout::top_down(Align::LEFT), |ui| {
                    match meta.kind {
                        Kind::Text => {
                            let text = match self.full_content(meta.id) {
                                Some(Content::Text(t)) => t,
                                _ => meta.preview.clone(),
                            };
                            let shown: String = text.chars().take(50_000).collect();
                            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(("preview", meta.id)).show(ui, |ui| {
                                ui.add(egui::Label::new(RichText::new(shown).font(FontId::monospace(12.5)).color(pal.text)).wrap());
                            });
                        }
                        Kind::Image => {
                            let ocr_text = meta.ocr.clone().filter(|t| !t.trim().is_empty());
                            let full = ui.available_size();
                            let img_h = if ocr_text.is_some() { (full.y * 0.62).floor() } else { full.y - 22.0 };
                            if let Some(tex) = self.thumb(&ctx, meta.id) {
                                let ts = tex.size_vec2();
                                let s = (full.x / ts.x).min(img_h / ts.y).min(2.0);
                                ui.allocate_ui_with_layout(Vec2::new(full.x, img_h), Layout::centered_and_justified(egui::Direction::TopDown), |ui| {
                                    ui.add(egui::Image::from_texture((tex.id(), ts * s)).corner_radius(6));
                                });
                            }
                            match (&ocr_text, &meta.ocr) {
                                (Some(t), _) => {
                                    ui.add_space(6.0);
                                    ui.label(RichText::new("图中文字").size(11.5).strong().color(pal.dim));
                                    egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(("ocr", meta.id)).show(ui, |ui| {
                                        ui.add(egui::Label::new(RichText::new(t).size(12.5).color(pal.text)).wrap().selectable(true));
                                    });
                                }
                                (None, None) if self.ocr_available => {
                                    ui.label(RichText::new("正在识别图中文字…").size(11.5).color(pal.dim));
                                }
                                (None, None) => {
                                    ui.label(RichText::new("未启用图片文字识别（见 README 的 OCR 一节）").size(11.5).color(pal.dim));
                                }
                                _ => {
                                    ui.label(RichText::new("图中未识别到文字").size(11.5).color(pal.dim));
                                }
                            }
                        }
                    }
                });
                ui.separator();
                let mut info = vec![match meta.kind {
                    Kind::Text => format!("{} 字符 · {} 行", meta.chars, meta.lines),
                    Kind::Image => format!("{} × {}", meta.width, meta.height),
                }];
                info.push(human_size(meta.bytes));
                if let Some(src) = &meta.source {
                    info.push(format!("来自 {src}"));
                }
                info.push(relative_time(meta.created));
                ui.label(RichText::new(info.join("  ·  ")).size(11.5).color(pal.dim));
            });
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
            .frame(egui::Frame::new().fill(pal.surface).corner_radius(12).stroke(Stroke::new(1.0, pal.border)).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                ui.set_width(440.0);
                ui.label(RichText::new("局域网同步").size(17.0).strong().color(pal.text));
                ui.label(RichText::new("端到端加密 · 仅同一局域网 · 收到的内容只进入历史，不会改动剪贴板").size(11.5).color(pal.dim));
                ui.add_space(6.0);
                ui.separator();
                let Some(info) = info else {
                    ui.label(RichText::new("无法获取同步状态").color(pal.danger));
                    return;
                };
                if !info.enabled {
                    ui.label(RichText::new("同步已在配置中关闭（sync_enabled = false）").color(pal.dim));
                    return;
                }
                ui.label(RichText::new(format!("本机：{}", info.device_name)).color(pal.text));
                if info.paired {
                    if info.peers.is_empty() {
                        ui.label(RichText::new("暂无设备在线（已配对的设备在同一局域网时会自动连接）").size(12.0).color(pal.dim));
                    } else {
                        ui.label(RichText::new("已连接：").size(12.5).color(pal.dim));
                        for (name, addr) in &info.peers {
                            ui.label(RichText::new(format!("●  {name}   {addr}")).size(12.5).color(pal.accent));
                        }
                    }
                } else {
                    ui.label(RichText::new("尚未与其他设备配对").size(12.0).color(pal.dim));
                }
                ui.add_space(4.0);
                ui.separator();

                // Invite another device.
                if let Some((code, addrs)) = self.sync_pairing.clone() {
                    ui.label(RichText::new("在另一台设备的 copym 里输入配对码：").size(12.5).color(pal.dim));
                    ui.label(RichText::new(&code).monospace().size(30.0).strong().color(pal.accent));
                    if let Some(a) = addrs.first() {
                        ui.label(RichText::new(format!("找不到本机时，输入 {code}@{a}")).size(11.5).color(pal.dim));
                    }
                    ui.label(RichText::new("3 分钟内有效，只能尝试一次").size(11.5).color(pal.dim));
                } else if ui.button("添加设备（生成配对码）").clicked() {
                    match self.req(&Request::SyncPair) {
                        Some(Response::Pairing { code, addrs, .. }) => {
                            self.sync_pairing = Some((code, addrs));
                            self.sync_msg = None;
                        }
                        _ => self.sync_msg = Some((self.error.take().unwrap_or_else(|| "无法开始配对".into()), true)),
                    }
                    self.refresh_sync();
                }
                ui.add_space(6.0);

                // Join another device.
                ui.label(RichText::new("加入另一台设备：").size(12.5).color(pal.dim));
                ui.horizontal(|ui| {
                    let busy = self.sync_job.is_some();
                    ui.add_enabled(
                        !busy,
                        egui::TextEdit::singleline(&mut self.sync_input).hint_text("8 位配对码，可附 @IP:端口").desired_width(260.0),
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
                    ui.label(RichText::new(msg).size(12.5).color(if *err { pal.danger } else { pal.accent }));
                }
                ui.add_space(8.0);
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

    fn footer(&self, ui: &mut egui::Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            let hints = if cfg!(target_os = "macos") {
                "↑↓ 选择   ⏎ 粘贴   ⌘P 固定   ⌘D 删除   ⌥1-9 快速粘贴   ⌘R 正则   Tab 分类   Esc 关闭"
            } else {
                "↑↓ 选择   Enter 粘贴   Ctrl+P 固定   Shift+Del 删除   Alt+1-9 快速粘贴   Ctrl+R 正则   Tab 分类   Esc 关闭"
            };
            ui.label(RichText::new(hints).size(11.0).color(pal.dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut s = format!("{} 条", self.items.len());
                if let Some(sync) = self.sync.as_ref().filter(|s| s.paired) {
                    s.push_str(&format!(" · 同步 {} 台在线", sync.peers.len()));
                }
                if self.paused {
                    s.push_str(" · 已暂停");
                }
                ui.label(RichText::new(s).size(11.0).color(if self.paused { pal.danger } else { pal.dim }));
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
            s.visuals.selection.bg_fill = pal.accent.gamma_multiply(0.45);
            s.visuals.extreme_bg_color = pal.surface;
        });

        egui::Frame::new()
            .fill(pal.bg)
            .stroke(Stroke::new(1.0, pal.border))
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                self.header(ui, &pal);
                ui.add_space(6.0);
                if let Some(err) = self.error.clone() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("⚠ {err}")).color(pal.danger).size(12.0));
                        if ui.small_button("重试").clicked() {
                            self.error = None;
                            self.connect();
                            self.dirty = true;
                        }
                    });
                }
                let footer_h = 22.0;
                let body_h = (ui.available_height() - footer_h - 6.0).max(80.0);
                ui.horizontal_top(|ui| {
                    let list_w = (ui.available_width() * 0.56).floor();
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


        // Popups/modals measure themselves in an invisible first pass; make sure
        // the first few frames are actually painted.
        self.frames += 1;
        if self.frames < 4 {
            ctx.request_repaint();
        }
        if self.sync_open {
            self.sync_panel(&ctx, &pal);
        }

        if let Some(a) = action {
            self.perform(&ctx, a);
        }
    }
}

// ---------------------------------------------------------------- helpers --

fn clean_preview(s: &str) -> String {
    let s: String = s.chars().take(400).collect();
    let mut lines = s.lines().map(|l| l.replace('\t', "    ")).filter(|l| !l.trim().is_empty());
    let mut out = String::new();
    for (n, l) in lines.by_ref().take(2).enumerate() {
        if n > 0 {
            out.push('\n');
        }
        out.push_str(l.trim_end());
    }
    if out.is_empty() {
        out = "(空白)".into();
    }
    out
}

fn subtitle(m: &ItemMeta) -> String {
    let mut parts = vec![relative_time(m.created)];
    match m.kind {
        Kind::Text => {
            if m.lines > 1 {
                parts.push(format!("{} 行", m.lines));
            }
            parts.push(format!("{} 字符", m.chars));
        }
        Kind::Image => {
            parts.push(human_size(m.bytes));
            if let Some(line) = m.ocr.as_deref().and_then(|t| t.lines().find(|l| !l.trim().is_empty())) {
                let snippet: String = line.chars().take(24).collect();
                parts.push(format!("「{snippet}」"));
            }
        }
    }
    if let Some(src) = &m.source {
        parts.push(src.clone());
    }
    parts.join(" · ")
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
