//! The settings dialog: every option of config.toml with an icon, grouped in
//! sections, driven by Vim keys (j/k rows, h/l values, Space toggles, i edits,
//! Tab / gt sections, Ctrl-S or ZZ saves, q / Esc closes) or the mouse.
//! Saving writes only the changed keys (comments in the file stay), applies
//! what the popup uses at once, asks the daemon to reload what it can, and
//! offers a restart for the rest.

use super::*;
use crate::icons;

/// State of the open dialog.
pub(super) struct Settings {
    draft: Config,
    orig: Config,
    pub(super) section: usize,
    /// Focused row of the section (keyboard).
    row: usize,
    /// Rows the section had last frame (to clamp `row`).
    rows: usize,
    /// Keys typed so far ("g", "Z").
    pending: String,
    /// Scroll the focused row into view.
    reveal: bool,
    ignore_patterns: String,
    sync_peers: String,
    app_input: String,
    autostart: bool,
    msg: Option<(String, bool)>,
    /// Saved settings that only take effect after a restart.
    restart: bool,
    /// Frames drawn: a modal measures itself first and shows on a later frame.
    frames: u8,
}

impl Settings {
    pub(super) fn new(cfg: &Config, autostart: bool) -> Self {
        Self {
            draft: cfg.clone(),
            orig: cfg.clone(),
            section: 0,
            row: 0,
            rows: 0,
            pending: String::new(),
            reveal: false,
            ignore_patterns: cfg.ignore_patterns.join("\n"),
            sync_peers: cfg.sync_peers.join("\n"),
            app_input: String::new(),
            autostart,
            msg: None,
            restart: false,
            frames: 0,
        }
    }
}

const SECTIONS: &[(&str, &str, &str)] = &[
    (icons::SLIDERS_HORIZONTAL, "通用", "general"),
    (icons::APP_WINDOW, "弹窗", "popup"),
    (icons::HISTORY, "历史记录", "history"),
    (icons::SHIELD, "隐私", "privacy"),
    (icons::SCAN_TEXT, "文字识别", "text recognition"),
    (icons::WIFI, "局域网同步", "LAN sync"),
];

const CONTROL_W: f32 = 200.0;
const MB: f64 = 1024.0 * 1024.0;

/// What the keyboard asks of the focused row this frame.
#[derive(Clone, Copy, Default)]
struct RowKeys {
    left: bool,
    right: bool,
    activate: bool,
}

/// Hands out row numbers while a section is drawn; the focused row gets the keys.
struct Rows {
    focus: usize,
    count: usize,
    keys: RowKeys,
    reveal: bool,
}

impl Rows {
    fn next(&mut self) -> (bool, RowKeys) {
        let focused = self.count == self.focus;
        self.count += 1;
        (focused, if focused { self.keys } else { RowKeys::default() })
    }
}

/// One setting: icon, title and description on the left, the control on the right.
fn row(ui: &mut egui::Ui, pal: &Palette, rows: &mut Rows, glyph: &str, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui, RowKeys)) {
    let (focused, keys) = rows.next();
    let w = ui.available_width();
    let h = if desc.is_empty() { 34.0 } else { 44.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    if resp.clicked() {
        rows.focus = rows.count - 1;
    }
    if focused {
        ui.painter().rect_filled(rect, 0, pal.selected);
        ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(3.0, h)), 0, pal.accent);
        if rows.reveal {
            ui.scroll_to_rect(rect, None);
        }
    }
    let p = ui.painter();
    p.text(Pos2::new(rect.left() + 20.0, rect.center().y), Align2::CENTER_CENTER, glyph, icon_font(15.0), if focused { pal.accent } else { pal.dim });
    let text_w = (w - 40.0 - CONTROL_W - 12.0).max(80.0);
    let title_color = if focused { pal.accent } else { pal.text };
    let t = elided(p, title, FontId::proportional(13.0), title_color, text_w);
    if desc.is_empty() {
        p.galley(Pos2::new(rect.left() + 40.0, rect.center().y - t.size().y / 2.0), t, title_color);
    } else {
        let d = elided(p, desc, FontId::proportional(11.0), pal.faint, text_w);
        let top = rect.center().y - (t.size().y + d.size().y + 2.0) / 2.0;
        let dh = t.size().y;
        p.galley(Pos2::new(rect.left() + 40.0, top), t, title_color);
        p.galley(Pos2::new(rect.left() + 40.0, top + dh + 2.0), d, pal.faint);
        resp.clone().on_hover_text(desc);
    }
    let ctrl = Rect::from_min_max(Pos2::new(rect.right() - CONTROL_W - 6.0, rect.top()), Pos2::new(rect.right() - 6.0, rect.bottom()));
    ui.scope_builder(egui::UiBuilder::new().max_rect(ctrl).layout(Layout::right_to_left(Align::Center)), |ui| control(ui, keys));
}

/// `[x]` / `[ ]`.
fn toggle(ui: &mut egui::Ui, pal: &Palette, on: &mut bool, k: RowKeys) {
    let text = if *on { "[x] on " } else { "[ ] off" };
    let resp = ui.add(egui::Label::new(RichText::new(text).size(13.0).color(if *on { pal.accent } else { pal.faint })).sense(Sense::click()));
    if resp.clicked() || k.activate || k.left || k.right {
        *on = !*on;
    }
}

/// A drop-down of (value, label) choices; h/l cycle through them.
fn choice(ui: &mut egui::Ui, id: &str, value: &mut String, options: &[(&str, String)], k: RowKeys) {
    let pos = options.iter().position(|(v, _)| *v == value.as_str()).unwrap_or(0);
    let n = options.len().max(1);
    if k.right || k.activate {
        *value = options[(pos + 1) % n].0.to_string();
    } else if k.left {
        *value = options[(pos + n - 1) % n].0.to_string();
    }
    let current = options.iter().find(|(v, _)| *v == value.as_str()).map(|(_, l)| l.clone()).unwrap_or_else(|| value.clone());
    egui::ComboBox::from_id_salt(id).selected_text(format!("‹ {current} ›")).width(CONTROL_W - 10.0).show_ui(ui, |ui| {
        for (v, l) in options {
            ui.selectable_value(value, v.to_string(), l);
        }
    });
}

/// Single-line text; `i` / Enter starts editing it, Esc / Enter stops.
fn text(ui: &mut egui::Ui, value: &mut String, hint: &str, k: RowKeys) {
    let resp = ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(CONTROL_W - 10.0).margin(Margin::symmetric(8, 4)));
    if k.activate {
        resp.request_focus();
    }
}

/// A number; h/l step it.
fn number<T: egui::emath::Numeric>(ui: &mut egui::Ui, value: &mut T, range: std::ops::RangeInclusive<T>, step: f64, suffix: &str, k: RowKeys) {
    if k.left || k.right {
        let v = value.to_f64() + if k.right { step } else { -step };
        *value = T::from_f64(v.clamp(range.start().to_f64(), range.end().to_f64()));
    }
    ui.add(egui::DragValue::new(value).range(range).speed(step / 10.0).suffix(suffix.to_string()));
}

/// A byte size edited in MB; h/l step by 1 MB.
fn megabytes(ui: &mut egui::Ui, bytes: &mut usize, max_mb: f64, k: RowKeys) {
    let mut mb = *bytes as f64 / MB;
    if k.left || k.right {
        mb = (mb + if k.right { 1.0 } else { -1.0 }).clamp(0.1, max_mb);
    }
    let changed = ui.add(egui::DragValue::new(&mut mb).range(0.1..=max_mb).speed(0.1).max_decimals(1).suffix(" MB")).changed();
    if changed || k.left || k.right {
        *bytes = (mb * MB) as usize;
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// Keyboard of the dialog while no text field is being edited.
#[derive(Default)]
struct DialogKeys {
    row: isize,
    first: bool,
    last: bool,
    section: isize,
    row_keys: RowKeys,
    save: bool,
    close: bool,
}

/// Is a text field being edited? Only then the keys belong to egui. Any
/// other control that took the focus (Tab, a click on a drop-down, toggle or
/// number) gives it back, or j/k/arrows would stop moving between rows.
///
/// egui drops the focus on Esc before we see the key, so a field that was
/// edited in the previous frame still counts: its Esc only leaves the field.
pub(super) fn dialog_focus(ctx: &egui::Context) -> bool {
    let id = egui::Id::unique("copym-dialog-editing");
    let now = ctx.text_edit_focused();
    let before = ctx.data_mut(|d| {
        let before = d.get_temp::<bool>(id).unwrap_or(false);
        d.insert_temp(id, now);
        before
    });
    if now {
        return true;
    }
    if before && ctx.input(|i| i.key_pressed(Key::Escape)) {
        return true;
    }
    ctx.memory_mut(|m| {
        if let Some(id) = m.focused() {
            m.surrender_focus(id);
        }
    });
    false
}

fn dialog_keys(ctx: &egui::Context, pending: &mut String) -> DialogKeys {
    let mut k = DialogKeys::default();
    let editing = dialog_focus(ctx);
    let popup = egui::Popup::is_any_open(ctx);
    ctx.input_mut(|i| {
        k.save = i.consume_key(Modifiers::CTRL, Key::S) || i.consume_key(Modifiers::COMMAND, Key::S);
        if popup {
            return; // an open drop-down list takes the keys (Esc closes it)
        }
        if editing {
            // Esc leaves the text field (back to NORMAL), not the dialog
            if i.consume_key(Modifiers::NONE, Key::Escape) || i.consume_key(Modifiers::CTRL, Key::OpenBracket) {
                pending.push('\u{0}'); // marker: surrender focus below
            }
            return;
        }
        let mut key = |m: Modifiers, key: Key| i.consume_key(m, key);
        if key(Modifiers::NONE, Key::ArrowDown) || key(Modifiers::CTRL, Key::J) || key(Modifiers::CTRL, Key::N) {
            k.row = 1;
        }
        if key(Modifiers::NONE, Key::ArrowUp) || key(Modifiers::CTRL, Key::K) || key(Modifiers::CTRL, Key::P) {
            k.row = -1;
        }
        if key(Modifiers::NONE, Key::ArrowLeft) {
            k.row_keys.left = true;
        }
        if key(Modifiers::NONE, Key::ArrowRight) {
            k.row_keys.right = true;
        }
        if key(Modifiers::NONE, Key::Enter) {
            k.row_keys.activate = true;
        }
        if key(Modifiers::SHIFT, Key::Tab) {
            k.section = -1;
        }
        if key(Modifiers::NONE, Key::Tab) {
            k.section = 1;
        }
        if key(Modifiers::NONE, Key::Escape) {
            k.close = true;
        }
        let chars: Vec<char> = i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.clone()) } else { None }).flat_map(|t| t.chars().collect::<Vec<_>>()).collect();
        i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
        for c in chars {
            let op = std::mem::take(pending);
            match (op.as_str(), c) {
                ("g", 'g') => k.first = true,
                ("g", 't') => k.section = 1,
                ("g", 'T') => k.section = -1,
                ("Z", 'Z') => {
                    k.save = true;
                    k.close = true;
                }
                ("Z", 'Q') => k.close = true,
                (_, 'g') | (_, 'Z') => *pending = c.to_string(),
                (_, 'j') => k.row = 1,
                (_, 'k') => k.row = -1,
                (_, 'G') => k.last = true,
                (_, 'h') => k.row_keys.left = true,
                (_, 'l') => k.row_keys.right = true,
                (_, ' ') | (_, 'i') | (_, 'a') => k.row_keys.activate = true,
                (_, ']') | (_, 'L') => k.section = 1,
                (_, '[') | (_, 'H') => k.section = -1,
                (_, 'q') => k.close = true,
                _ => {}
            }
        }
    });
    k
}

impl App {
    pub(super) fn settings_panel(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(mut st) = self.settings.take() else { return };
        if st.frames < 3 {
            st.frames += 1;
            ctx.request_repaint();
        }
        let keys = dialog_keys(ctx, &mut st.pending);
        if st.pending.contains('\u{0}') {
            st.pending.clear();
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
        if keys.section != 0 {
            st.section = (st.section as isize + keys.section).rem_euclid(SECTIONS.len() as isize) as usize;
            st.row = 0;
        }
        if keys.row != 0 {
            st.row = (st.row as isize + keys.row).clamp(0, st.rows.saturating_sub(1) as isize) as usize;
            st.reveal = true;
        }
        if keys.first {
            st.row = 0;
            st.reveal = true;
        }
        if keys.last {
            st.row = st.rows.saturating_sub(1);
            st.reveal = true;
        }
        let mut keep = !keys.close;
        let mut save = keys.save;
        let mut restart = false;
        let modal = egui::Modal::new(egui::Id::unique("settings")).frame(dialog_frame(pal).inner_margin(Margin::same(16))).show(ctx, |ui| {
            let (w, h) = dialog_room(ctx, 660.0);
            ui.set_width(w);
            ui.horizontal(|ui| {
                ui.label(RichText::new(t!("-- 设置 --", "-- SETTINGS --")).size(13.0).strong().color(pal.accent));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(t!("j/k 选择  h/l 修改  \u{f1050} 切换  i 编辑  Tab 分类  ^S 保存  ZZ 保存并关闭  q 关闭", "j/k move  h/l change  \u{f1050} toggle  i edit  Tab section  ^S save  ZZ save+close  q close"))
                            .size(10.5)
                            .color(pal.faint),
                    );
                });
            });
            ui.add_space(8.0);
            let body_h = (h - 110.0).clamp(160.0, 420.0);
            ui.horizontal_top(|ui| {
                // ---- sections
                ui.allocate_ui_with_layout(Vec2::new(160.0, body_h), Layout::top_down(Align::LEFT), |ui| {
                    ui.set_min_size(Vec2::new(160.0, body_h));
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, (glyph, zh, en)) in SECTIONS.iter().enumerate() {
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(156.0, 30.0), Sense::click());
                        let current = i == st.section;
                        if current {
                            ui.painter().rect_filled(r, 0, pal.selected);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(r, 0, pal.hover);
                        }
                        let color = if current { pal.accent } else { pal.dim };
                        ui.painter().text(r.left_center() + Vec2::new(16.0, 0.0), Align2::CENTER_CENTER, *glyph, icon_font(14.0), color);
                        let label = elided(ui.painter(), &format!("{}{}", if current { "▸ " } else { "  " }, t!(*zh, *en)), FontId::proportional(12.5), color, r.width() - 40.0);
                        ui.painter().galley(Pos2::new(r.left() + 30.0, r.center().y - label.size().y / 2.0), label, color);
                        if resp.clicked() {
                            st.section = i;
                            st.row = 0;
                        }
                    }
                });
                let (line, _) = ui.allocate_exact_size(Vec2::new(1.0, body_h), Sense::hover());
                ui.painter().rect_filled(line, 0, pal.border);
                ui.add_space(10.0);
                // ---- settings of the section
                let rest = ui.available_width();
                ui.allocate_ui_with_layout(Vec2::new(rest, body_h), Layout::top_down(Align::LEFT), |ui| {
                    egui::ScrollArea::vertical().id_salt(("settings", st.section)).max_height(body_h).auto_shrink([false, false]).show(ui, |ui| {
                        ui.set_width(rest - 12.0);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let mut rows = Rows { focus: st.row, count: 0, keys: keys.row_keys, reveal: st.reveal };
                        self.settings_section(ui, pal, &mut st, &mut rows);
                        st.row = rows.focus;
                        st.rows = rows.count;
                        st.reveal = false;
                    });
                });
            });
            ui.add_space(8.0);
            if let Some((msg, err)) = &st.msg {
                ui.label(RichText::new(msg).size(12.0).color(if *err { pal.danger } else { pal.ok }));
            }
            ui.horizontal(|ui| {
                if ui.button(format!("{}  {}", icons::FILE_COG, t!("打开配置文件", "open config file"))).on_hover_text(crate::config::config_path().display().to_string()).clicked() {
                    open_external(&crate::config::config_path().to_string_lossy());
                }
                if st.restart && ui.button(RichText::new(format!("{}  {}", icons::ROTATE_CW, t!("立即重启 copym", "restart copym now"))).color(pal.accent)).clicked() {
                    restart = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(primary_button(pal, t!(":w 保存", ":w save"))).clicked() {
                        save = true;
                    }
                    if ui.button(t!(":q 关闭", ":q close")).clicked() {
                        keep = false;
                    }
                });
            });
        });
        if modal.should_close() && !ctx.text_edit_focused() && !egui::Popup::is_any_open(ctx) {
            keep = false;
        }
        if save {
            self.save_settings(ctx, &mut st);
        }
        if restart {
            restart_copym();
        }
        if keep {
            self.settings = Some(st);
        }
    }

    fn settings_section(&mut self, ui: &mut egui::Ui, pal: &Palette, st: &mut Settings, rows: &mut Rows) {
        let d = &mut st.draft;
        let restart_note = t!("重启后生效", "after a restart");
        match st.section {
            0 => {
                row(ui, pal, rows, icons::LANGUAGES, t!("语言", "language"), "", |ui, k| {
                    choice(ui, "language", &mut d.language, &[("auto", t!("跟随系统", "system").into()), ("zh", "中文".into()), ("en", "English".into())], k);
                });
                row(ui, pal, rows, icons::SUN_MOON, t!("外观", "appearance"), t!("深色 / 浅色，或跟随系统切换", "dark, light, or follow the system"), |ui, k| {
                    choice(ui, "theme", &mut d.theme, &theme_options(), k);
                });
                row(ui, pal, rows, icons::KEYBOARD, t!("全局快捷键", "global shortcut"), restart_note, |ui, k| text(ui, &mut d.hotkey, "Alt+C", k));
                row(ui, pal, rows, icons::POWER, t!("登录时自动启动", "start at login"), "", |ui, k| toggle(ui, pal, &mut st.autostart, k));
                row(ui, pal, rows, icons::DOWNLOAD, t!("自动检查更新", "check for updates"), t!("每天一次，只提示不自动安装", "daily; never installs by itself"), |ui, k| {
                    toggle(ui, pal, &mut d.check_updates, k)
                });
                row(ui, pal, rows, icons::TYPE, t!("界面字体", "font"), t!("字体文件路径，留空用内置等宽字体", "font file; empty = built-in monospace"), |ui, k| {
                    text(ui, &mut d.font, t!("内置", "built-in"), k)
                });
            }
            1 => {
                row(ui, pal, rows, icons::TEXT_CURSOR_INPUT, t!("打开时的模式", "mode on open"), t!("列表 = NORMAL，搜索框 = INSERT", "list = NORMAL, search = INSERT"), |ui, k| {
                    choice(ui, "popup_focus", &mut d.popup_focus, &[("list", "NORMAL".into()), ("search", "INSERT".into())], k);
                });
                row(ui, pal, rows, icons::CLIPBOARD_PASTE, t!("回车直接粘贴", "Enter pastes"), t!("关闭后只复制到剪贴板", "off: Enter only copies"), |ui, k| {
                    toggle(ui, pal, &mut d.paste_on_enter, k)
                });
                let default_paste = if cfg!(target_os = "macos") { "cmd+v" } else { "ctrl+v" };
                row(ui, pal, rows, icons::COMMAND, t!("粘贴按键", "paste keys"), t!("模拟的粘贴快捷键，留空用默认", "keys sent to paste; empty = default"), |ui, k| {
                    text(ui, &mut d.paste_shortcut, default_paste, k)
                });
                row(ui, pal, rows, icons::PANEL_TOP_CLOSE, t!("失去焦点时关闭", "close on focus loss"), "", |ui, k| toggle(ui, pal, &mut d.close_on_focus_lost, k));
                row(ui, pal, rows, icons::ZAP, t!("弹窗常驻后台", "keep the popup loaded"), t!("打开更快，多占约 35 MB；重启后生效", "opens faster, ~35 MB more; after a restart"), |ui, k| {
                    toggle(ui, pal, &mut d.keep_popup_loaded, k)
                });
            }
            2 => {
                row(ui, pal, rows, icons::HISTORY, t!("最多保留条数", "items to keep"), t!("固定的条目不计入", "pinned items do not count"), |ui, k| {
                    number(ui, &mut d.max_items, 10..=100_000_000, 100.0, "", k)
                });
                row(ui, pal, rows, icons::FILE_TEXT, t!("文本大小上限", "largest text"), "", |ui, k| megabytes(ui, &mut d.max_text_bytes, 256.0, k));
                row(ui, pal, rows, icons::IMAGE, t!("记录图片", "record images"), "", |ui, k| toggle(ui, pal, &mut d.store_images, k));
                row(ui, pal, rows, icons::IMAGE_PLUS, t!("图片大小上限", "largest image"), "", |ui, k| megabytes(ui, &mut d.max_image_bytes, 512.0, k));
                row(ui, pal, rows, icons::TIMER, t!("检查间隔", "poll interval"), t!("没有剪贴板通知时（X11 等）", "where the clipboard has no notifications"), |ui, k| {
                    number(ui, &mut d.poll_interval_ms, 100..=10_000, 100.0, " ms", k)
                });
                if cfg!(target_os = "linux") {
                    row(ui, pal, rows, icons::ROTATE_CCW, t!("剪贴板被清空时恢复", "restore an emptied clipboard"), t!("Wayland：复制的程序退出后", "Wayland: after the copying app quits"), |ui, k| {
                        toggle(ui, pal, &mut d.restore_empty_clipboard, k)
                    });
                }
            }
            3 => {
                row(ui, pal, rows, icons::SHIELD_CHECK, t!("跳过密码等敏感内容", "skip passwords"), t!("应用标记为隐私的内容不记录", "content apps mark as sensitive"), |ui, k| {
                    toggle(ui, pal, &mut d.respect_password_hints, k)
                });
                let apps_desc = if cfg!(target_os = "linux") {
                    t!("Linux 上无法得知来源应用，此项不生效", "Linux cannot tell the source app; no effect here")
                } else {
                    t!("名称包含即匹配，不区分大小写", "matches when the name contains it, any case")
                };
                row(ui, pal, rows, icons::BAN, t!("不记录的应用", "ignored apps"), apps_desc, |ui, k| {
                    let add = ui.add(egui::Button::new(t!("添加", "add")));
                    let input = ui.add(egui::TextEdit::singleline(&mut st.app_input).hint_text(if cfg!(windows) { "chrome.exe" } else { t!("应用名", "app name") }).desired_width(110.0));
                    if k.activate {
                        input.request_focus();
                    }
                    let enter = input.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if (add.clicked() || enter) && !st.app_input.trim().is_empty() {
                        d.ignore_apps.push(st.app_input.trim().to_string());
                        st.app_input.clear();
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(40.0);
                    let mut remove = None;
                    for (i, a) in d.ignore_apps.iter().enumerate() {
                        if ui.small_button(format!("{a}  ×")).on_hover_text(t!("移除", "remove")).clicked() {
                            remove = Some(i);
                        }
                    }
                    if let Some(i) = remove {
                        d.ignore_apps.remove(i);
                    }
                });
                ui.add_space(6.0);
                let mut focus_patterns = false;
                row(ui, pal, rows, icons::REGEX, t!("不记录的内容", "ignored content"), t!("正则表达式，每行一条", "regular expressions, one per line"), |_, k| focus_patterns = k.activate);
                ui.horizontal(|ui| {
                    ui.add_space(40.0);
                    let r = ui.add(egui::TextEdit::multiline(&mut st.ignore_patterns).font(FontId::monospace(12.5)).desired_rows(3).desired_width(f32::INFINITY).hint_text(r"^\d{6}$"));
                    if focus_patterns {
                        r.request_focus();
                    }
                });
            }
            4 => {
                row(ui, pal, rows, icons::SCAN_TEXT, t!("识别图片中的文字", "recognize text in images"), t!("可搜索图片里的文字；重启后生效", "makes images searchable; after a restart"), |ui, k| {
                    toggle(ui, pal, &mut d.ocr_enabled, k)
                });
                let mut engines = vec![("auto", t!("自动", "automatic").to_string()), ("native", t!("系统自带", "system").to_string())];
                if cfg!(target_os = "linux") {
                    engines = vec![("auto", t!("自动", "automatic").to_string()), ("ppocr", t!("内置 PP-OCR", "built-in PP-OCR").to_string())];
                }
                engines.push(("tesseract", "Tesseract".into()));
                engines.push(("off", t!("关闭", "off").into()));
                row(ui, pal, rows, icons::CPU, t!("识别引擎", "engine"), restart_note, |ui, k| choice(ui, "ocr_engine", &mut d.ocr_engine, &engines, k));
                row(ui, pal, rows, icons::LANGUAGES, t!("Tesseract 语言", "Tesseract languages"), t!("只用于 Tesseract 引擎", "only for the Tesseract engine"), |ui, k| {
                    text(ui, &mut d.ocr_languages, "chi_sim+eng", k)
                });
            }
            _ => {
                row(ui, pal, rows, icons::WIFI, t!("启用局域网同步", "LAN sync"), t!("端到端加密；配对用 :sync", "end-to-end encrypted; pair with :sync"), |ui, k| {
                    toggle(ui, pal, &mut d.sync_enabled, k)
                });
                row(ui, pal, rows, icons::LAPTOP, t!("本机名称", "device name"), t!("其他设备上显示的名字", "shown on other devices"), |ui, k| {
                    text(ui, &mut d.device_name, t!("主机名", "host name"), k)
                });
                row(ui, pal, rows, icons::NETWORK, t!("端口", "port"), "", |ui, k| number(ui, &mut d.sync_port, 1024..=65535, 1.0, "", k));
                row(ui, pal, rows, icons::IMAGES, t!("同步图片", "sync images"), "", |ui, k| toggle(ui, pal, &mut d.sync_images, k));
                row(ui, pal, rows, icons::IMAGE, t!("同步图片大小上限", "largest synced image"), "", |ui, k| megabytes(ui, &mut d.sync_max_image_bytes, 256.0, k));
                row(ui, pal, rows, icons::LIST_ORDERED, t!("连接时补发的条数", "items sent on connect"), "", |ui, k| {
                    number(ui, &mut d.sync_history_items, 0..=1000, 10.0, "", k)
                });
                let mut focus_peers = false;
                row(ui, pal, rows, icons::SERVER, t!("固定地址", "fixed addresses"), t!("找不到设备时，每行一个 IP:端口", "when devices are not found: IP:port per line"), |_, k| focus_peers = k.activate);
                ui.horizontal(|ui| {
                    ui.add_space(40.0);
                    let r = ui.add(egui::TextEdit::multiline(&mut st.sync_peers).font(FontId::monospace(12.5)).desired_rows(2).desired_width(f32::INFINITY).hint_text("192.168.1.20:47800"));
                    if focus_peers {
                        r.request_focus();
                    }
                });
                ui.label(RichText::new(t!("同步相关设置重启后生效", "sync settings take effect after a restart")).size(11.0).color(pal.faint));
            }
        }
    }

    fn save_settings(&mut self, ctx: &egui::Context, st: &mut Settings) {
        st.draft.ignore_patterns = lines(&st.ignore_patterns);
        st.draft.sync_peers = lines(&st.sync_peers);
        if let Some(bad) = st.draft.ignore_patterns.iter().find_map(|p| regex_lite::Regex::new(p).err().map(|e| format!("{p}: {e}"))) {
            st.msg = Some((tf!("正则有误：{bad}", "Invalid regular expression: {bad}"), true));
            return;
        }
        if st.autostart != self.autostart {
            match crate::autostart::set(st.autostart) {
                Ok(_) => self.autostart = st.autostart,
                Err(e) => {
                    st.msg = Some((tf!("自动启动设置失败：{e}", "Could not change start at login: {e}"), true));
                    return;
                }
            }
        }
        let changed = st.draft.changed(&st.orig);
        if changed.is_empty() {
            st.msg = Some((t!("已保存", "Saved").into(), false));
            return;
        }
        let values: Vec<(&str, toml_edit::Value)> = changed.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        if let Err(e) = crate::config::save_values(&values) {
            st.msg = Some((e, true));
            return;
        }
        // the popup's own settings apply right away
        if st.draft.font != self.cfg.font {
            install_fonts(ctx, &st.draft.font);
        }
        if st.draft.language != self.cfg.language {
            crate::init_language(&st.draft.language);
        }
        self.cfg = st.draft.clone();
        // the daemon reloads filters and the paste keys; the rest needs a restart
        let _ = self.req(&Request::ReloadConfig);
        st.restart |= changed.iter().any(|(k, _)| crate::config::RESTART_KEYS.contains(&k.as_str()));
        st.orig = st.draft.clone();
        st.msg = Some(if st.restart {
            (t!("已保存。部分设置重启 copym 后生效", "Saved. Some settings take effect after restarting copym").into(), false)
        } else {
            (t!("已保存", "Saved").into(), false)
        });
        self.dirty = true;
    }
}

/// (value, label) of the `theme` setting.
pub(super) fn theme_options() -> Vec<(&'static str, String)> {
    vec![("system", t!("跟随系统", "System").into()), ("light", t!("浅色", "Light").into()), ("dark", t!("深色", "Dark").into())]
}

/// Quit the daemon (and this popup) and start a new one, from a helper process.
pub(super) fn restart_copym() {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("restart").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    }
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{kittest::Queryable, Harness};

    fn harness() -> Harness<'static, App> {
        let mut app = App::new(Config::default(), Arc::default(), Arc::default());
        app.error = None;
        app.settings = Some(Settings::new(&app.cfg, false));
        let mut fonts_ready = false;
        let mut harness = Harness::builder()
            .with_size(Vec2::new(760.0, 480.0))
            .with_accessibility_check(false)
            .build_ui_state(
            move |ui, app: &mut App| {
                // fonts apply from the next frame: draw nothing until then
                if !fonts_ready {
                    setup_ctx(ui.ctx(), &app.cfg);
                    fonts_ready = true;
                    return;
                }
                app.draw(ui)
            },
            app,
        );
        harness.run_steps(4);
        harness
    }

    fn press(h: &mut Harness<'static, App>, key: Key) {
        h.key_press(key);
        h.run_steps(2);
    }

    fn typed(h: &mut Harness<'static, App>, text: &str) {
        h.event(egui::Event::Text(text.into()));
        h.run_steps(2);
    }

    fn row(h: &Harness<'static, App>) -> (usize, usize) {
        let st = h.state().settings.as_ref().expect("settings open");
        (st.section, st.row)
    }

    /// Tab focused the first control of the next section (a drop-down in
    /// "popup"); a focused control used to swallow j/k as if typing.
    #[test]
    fn rows_move_after_switching_sections() {
        let mut h = harness();
        press(&mut h, Key::Tab);
        assert_eq!(row(&h), (1, 0));
        typed(&mut h, "j");
        assert_eq!(row(&h), (1, 1));
        press(&mut h, Key::ArrowDown);
        assert_eq!(row(&h), (1, 2));
        typed(&mut h, "k");
        assert_eq!(row(&h), (1, 1));
        // every section
        for s in 2..SECTIONS.len() {
            press(&mut h, Key::Tab);
            assert_eq!(row(&h), (s, 0), "section {s}");
            typed(&mut h, "j");
            assert_eq!(row(&h), (s, 1), "section {s}");
        }
    }

    /// Clicking a control (here the drop-down) must not take the keys away either.
    #[test]
    fn rows_move_after_clicking_a_control() {
        let mut h = harness();
        press(&mut h, Key::Tab);
        h.get_by_value("‹ NORMAL ›").click();
        h.run_steps(2);
        press(&mut h, Key::Escape); // closes the drop-down list, not the dialog
        assert!(h.state().settings.is_some());
        typed(&mut h, "j");
        assert_eq!(row(&h).1, 1);
    }

    /// The group manager uses the same rules: Tab must not steal j/k.
    #[test]
    fn group_editor_keys() {
        let mut h = harness();
        h.state_mut().settings = None;
        let ed = Editor::new(&h.state().groups, 0);
        h.state_mut().editor = Some(ed);
        h.run_steps(3);
        press(&mut h, Key::Tab);
        press(&mut h, Key::Tab);
        typed(&mut h, "j");
        assert_eq!(h.state().editor.as_ref().expect("editor open").sel, 1);
        typed(&mut h, "k");
        assert_eq!(h.state().editor.as_ref().unwrap().sel, 0);
        typed(&mut h, "q");
        assert!(h.state().editor.is_none());
    }

    /// While a text field is edited, j is text and Esc leaves the field.
    #[test]
    fn text_fields_keep_their_keys() {
        let mut h = harness();
        press(&mut h, Key::Tab);
        typed(&mut h, "j");
        typed(&mut h, "j"); // "paste keys" row
        assert_eq!(row(&h), (1, 2));
        typed(&mut h, "i");
        typed(&mut h, "j");
        assert_eq!(row(&h), (1, 2));
        assert_eq!(h.state().settings.as_ref().unwrap().draft.paste_shortcut, "j");
        press(&mut h, Key::Escape);
        assert!(h.state().settings.is_some());
        typed(&mut h, "j");
        assert_eq!(row(&h), (1, 3));
    }
}
