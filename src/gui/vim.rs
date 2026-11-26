//! Vim-style keyboard control of the popup.
//!
//! NORMAL (the list has the keyboard): counts and operators like Vim —
//! `5j`, `gg`, `G`, `12G`, `dd`, `yy`, `gt`, `{` `}` … ; `/` or `i` searches
//! (INSERT), `:` opens the command line (COMMAND), `?` shows every key.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Normal,
    Insert,
    Command,
}

/// A key pressed in NORMAL mode.
enum NKey {
    Char(char),
    Down(isize),
    Up(isize),
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    Enter { shift: bool },
    Esc,
    Group(isize),
    /// ⌘S on macOS: pin / unpin at once.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Pin,
}

impl App {
    pub(super) fn mode(&self) -> Mode {
        if self.cmdline.is_some() {
            Mode::Command
        } else if self.search_mode {
            Mode::Insert
        } else {
            Mode::Normal
        }
    }

    pub(super) fn handle_keys(&mut self, ctx: &egui::Context) -> Option<Action> {
        if self.sync_open || self.editor.is_some() || self.settings.is_some() {
            return None; // dialogs own the keyboard
        }
        if self.help {
            let close = ctx.input_mut(|i| {
                let text = i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "q" || t == "?"));
                i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
                i.consume_key(Modifiers::NONE, Key::Escape) || i.consume_key(Modifiers::NONE, Key::Enter) || text
            });
            if close {
                self.help = false;
            }
            return None;
        }
        if self.actions.is_some() {
            return self.panel_keys(ctx);
        }

        // ---- shortcuts in every mode
        let (mut open_actions, mut quit, mut open_settings, mut regex) = (false, false, false, false);
        let mut group = None;
        let mut quick = None;
        ctx.input_mut(|i| {
            #[cfg(target_os = "macos")]
            {
                open_actions = i.consume_key(Modifiers::MAC_CMD, Key::K);
            }
            #[cfg(not(target_os = "macos"))]
            {
                open_actions = i.consume_key(Modifiers::ALT, Key::K);
            }
            quit = i.consume_key(Modifiers::COMMAND, Key::Q);
            open_settings = i.consume_key(Modifiers::COMMAND, Key::Comma);
            regex = i.consume_key(Modifiers::ALT, Key::R) || i.consume_key(Modifiers::MAC_CMD, Key::R);
            let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            for (n, k) in digits.iter().enumerate() {
                if i.consume_key(Modifiers::COMMAND, *k) {
                    group = Some(n);
                }
                if i.consume_key(Modifiers::ALT, *k) {
                    quick = Some(n);
                }
            }
            if quick.is_some() || open_actions {
                // ⌥1 also types "¡" on macOS - keep it out of the text fields
                i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
            }
        });
        if quit {
            self.quit(ctx);
            return None;
        }
        if open_settings {
            self.open_settings();
        }
        if open_actions {
            self.actions = Some(0);
            return None;
        }
        if regex {
            self.regex = !self.regex;
            self.dirty = true;
        }
        if let Some(g) = group {
            self.set_group(g);
        }
        if let Some(n) = quick {
            return Some(Action::Activate(n, self.cfg.paste_on_enter));
        }
        match self.mode() {
            Mode::Command => self.command_keys(ctx),
            Mode::Insert => self.insert_keys(ctx),
            Mode::Normal => self.normal_keys(ctx),
        }
    }

    // ------------------------------------------------------------- NORMAL --

    fn normal_keys(&mut self, ctx: &egui::Context) -> Option<Action> {
        let mut keys = Vec::new();
        ctx.input_mut(|i| {
            let mut key = |m: Modifiers, k: Key, v: NKey| {
                if i.consume_key(m, k) {
                    keys.push(v);
                }
            };
            key(Modifiers::NONE, Key::ArrowDown, NKey::Down(1));
            key(Modifiers::NONE, Key::ArrowUp, NKey::Up(1));
            key(Modifiers::CTRL, Key::J, NKey::Down(1));
            key(Modifiers::CTRL, Key::N, NKey::Down(1));
            key(Modifiers::CTRL, Key::E, NKey::Down(1));
            key(Modifiers::CTRL, Key::K, NKey::Up(1));
            key(Modifiers::CTRL, Key::P, NKey::Up(1));
            key(Modifiers::CTRL, Key::Y, NKey::Up(1));
            key(Modifiers::CTRL, Key::D, NKey::HalfDown);
            key(Modifiers::CTRL, Key::U, NKey::HalfUp);
            key(Modifiers::CTRL, Key::F, NKey::PageDown);
            key(Modifiers::CTRL, Key::B, NKey::PageUp);
            key(Modifiers::NONE, Key::PageDown, NKey::PageDown);
            key(Modifiers::NONE, Key::PageUp, NKey::PageUp);
            key(Modifiers::NONE, Key::Home, NKey::Top);
            key(Modifiers::NONE, Key::End, NKey::Bottom);
            key(Modifiers::SHIFT, Key::Enter, NKey::Enter { shift: true });
            key(Modifiers::NONE, Key::Enter, NKey::Enter { shift: false });
            key(Modifiers::SHIFT, Key::Tab, NKey::Group(-1));
            key(Modifiers::NONE, Key::Tab, NKey::Group(1));
            key(Modifiers::NONE, Key::Escape, NKey::Esc);
            #[cfg(target_os = "macos")]
            {
                // the macOS habits stay: ⌘S pin, ⌘D delete
                key(Modifiers::MAC_CMD, Key::S, NKey::Pin);
                key(Modifiers::MAC_CMD, Key::D, NKey::Char('x'));
                key(Modifiers::MAC_CMD, Key::F, NKey::Char('/'));
            }
            for e in &i.events {
                if let egui::Event::Text(t) = e {
                    keys.extend(t.chars().map(NKey::Char));
                }
            }
            // nothing is typed into the search box in NORMAL mode
            i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
        });
        let mut action = None;
        for k in keys {
            if let Some(a) = self.normal_key(ctx, k) {
                action = Some(a);
            }
        }
        action
    }

    fn normal_key(&mut self, ctx: &egui::Context, key: NKey) -> Option<Action> {
        let page = ((self.view_height / ROW_H).floor() as isize).max(1);
        let last = self.items.len().saturating_sub(1);
        let paste = self.cfg.paste_on_enter;
        let NKey::Char(c) = key else {
            let count = self.take_count();
            match key {
                NKey::Down(n) => self.move_selection(n * count),
                NKey::Up(n) => self.move_selection(-n * count),
                NKey::HalfDown => self.move_selection(page / 2 * count),
                NKey::HalfUp => self.move_selection(-page / 2 * count),
                NKey::PageDown => self.move_selection(page * count),
                NKey::PageUp => self.move_selection(-page * count),
                NKey::Top => self.goto(0),
                NKey::Bottom => self.goto(last),
                NKey::Enter { shift } => return Some(Action::Activate(self.selected, paste != shift)),
                NKey::Group(d) => self.cycle_group(d * count),
                NKey::Pin => return Some(Action::Pin(self.selected)),
                NKey::Esc => {
                    if !self.pending.is_empty() {
                        self.pending.clear();
                    } else if !self.query.is_empty() {
                        self.query.clear();
                        self.dirty = true;
                    } else {
                        self.close(ctx);
                    }
                }
                NKey::Char(_) => {}
            }
            return None;
        };
        // counts: "12" then a motion; "0" alone is a motion
        let digits_end = self.pending.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(self.pending.len());
        let has_op = digits_end < self.pending.len();
        if c.is_ascii_digit() && !has_op && !(c == '0' && self.pending.is_empty()) {
            self.pending.push(c);
            return None;
        }
        let count_text = self.pending[..digits_end].to_string();
        let op = self.pending[digits_end..].to_string();
        let has_count = !count_text.is_empty();
        let n: usize = count_text.parse().unwrap_or(1).max(1);
        self.pending.clear();
        let sel = self.selected;
        match (op.as_str(), c) {
            ("g", 'g') => self.goto(if has_count { n - 1 } else { 0 }),
            ("g", 't') => {
                if has_count {
                    self.set_group(n - 1);
                } else {
                    self.cycle_group(1);
                }
            }
            ("g", 'T') => self.cycle_group(-(n as isize)),
            ("d", 'd') => return Some(Action::Delete(sel)),
            ("y", 'y') => return Some(Action::Activate(sel, false)),
            ("Z", 'Z') | ("Z", 'Q') => self.close(ctx),
            ("z", _) => self.scroll_to_selected = true,
            // m{key}: put into that pin group (like setting a Vim mark); mm without
            // a group on m toggles the first one
            ("m", _) => match self.groups.by_key(c, true).map(|(_, g)| g.board) {
                Some(b) => return Some(Action::PinTo(sel, b)),
                None if c == 'm' => return Some(Action::Pin(sel)),
                None => self.notify(tf!("没有按键为 {c} 的固定分组", "no pin group on key {c}")),
            },
            // '{key} / `{key}: jump to that group
            ("'", _) | ("`", _) => match self.groups.by_key(c, false).map(|(i, _)| i) {
                Some(i) => self.set_group(i),
                None => self.notify(tf!("没有按键为 {c} 的分组", "no group on key {c}")),
            },
            ("", _) => match c {
                'j' => self.move_selection(n as isize),
                'k' => self.move_selection(-(n as isize)),
                'n' => self.move_selection(n as isize),
                'N' => self.move_selection(-(n as isize)),
                'G' => self.goto(if has_count { n - 1 } else { last }),
                '0' | '^' => self.goto(0),
                '$' => self.goto(last),
                'H' => self.goto(self.visible().0),
                'L' => self.goto(self.visible().1),
                'M' => {
                    let (a, b) = self.visible();
                    self.goto((a + b) / 2);
                }
                '}' => {
                    let next = self.section_starts().into_iter().find(|&s| s > sel).unwrap_or(last);
                    self.goto(next);
                }
                '{' => {
                    let prev = self.section_starts().into_iter().rev().find(|&s| s < sel).unwrap_or(0);
                    self.goto(prev);
                }
                'g' | 'd' | 'y' | 'Z' | 'z' | 'm' | '\'' | '`' => self.pending = format!("{count_text}{c}"),
                '/' => {
                    self.query.clear();
                    self.dirty = true;
                    self.search_mode = true;
                }
                'i' | 'a' | 'I' | 'A' => self.search_mode = true,
                ':' => self.cmdline = Some(String::new()),
                '?' => self.help = true,
                'p' | 'P' => return Some(Action::Activate(sel, true)),
                'Y' => return Some(Action::Activate(sel, false)),
                'x' | 'X' | 'D' => return Some(Action::Delete(sel)),
                'o' | 'O' => self.open_selected(ctx),
                ' ' => self.actions = Some(0),
                'q' => self.close(ctx),
                _ => {}
            },
            _ => {} // unknown combination: dropped, like Vim
        }
        None
    }

    /// The count typed before a non-character key (1 when none).
    fn take_count(&mut self) -> isize {
        let n = self.pending.parse::<isize>().unwrap_or(1).max(1);
        self.pending.clear();
        n
    }

    fn goto(&mut self, i: usize) {
        if !self.items.is_empty() {
            self.selected = i.min(self.items.len() - 1);
            self.scroll_to_selected = true;
        }
    }

    fn cycle_group(&mut self, d: isize) {
        if !self.groups.list.is_empty() {
            let n = self.groups.list.len() as isize;
            self.set_group(((self.group as isize + d).rem_euclid(n)) as usize);
        }
    }

    /// First and last item fully in view.
    fn visible(&self) -> (usize, usize) {
        let top = self.scroll_offset;
        let bottom = top + self.view_height;
        let first = self.item_y.iter().position(|&y| y >= top).unwrap_or(0);
        let last = self.item_y.iter().rposition(|&y| y + ROW_H <= bottom).unwrap_or(first).max(first);
        (first, last)
    }

    /// Items that start a section (pinned, today, …).
    fn section_starts(&self) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut after_section = false;
        for (_, line) in &self.lines {
            match line {
                Line::Section(_) => after_section = true,
                Line::Item(i) => {
                    if after_section {
                        starts.push(*i);
                    }
                    after_section = false;
                }
            }
        }
        starts
    }

    /// `o`: open the selected link or path.
    fn open_selected(&mut self, ctx: &egui::Context) {
        let Some(id) = self.items.get(self.selected).map(|m| m.id) else { return };
        if let Detail::Text { kind, body } = self.detail(ctx, id) {
            let target = body.trim().to_string();
            match kind {
                TextKind::Url => open_external(&target),
                TextKind::Path => {
                    let _ = self.run_act(ctx, Act::OpenPath(target));
                }
                _ => self.notify(t!("不是链接或路径", "Not a link or a path")),
            }
        }
    }

    // ------------------------------------------------------------- INSERT --

    fn insert_keys(&mut self, ctx: &egui::Context) -> Option<Action> {
        let paste = self.cfg.paste_on_enter;
        let mut action = None;
        let (mut nav, mut group, mut esc, mut clear, mut word) = (0isize, 0isize, false, false, false);
        let page = ((self.view_height / ROW_H).floor() as isize).max(1);
        ctx.input_mut(|i| {
            for (m, k, d) in [
                (Modifiers::NONE, Key::ArrowDown, 1isize),
                (Modifiers::CTRL, Key::J, 1),
                (Modifiers::CTRL, Key::N, 1),
                (Modifiers::NONE, Key::ArrowUp, -1),
                (Modifiers::CTRL, Key::K, -1),
                (Modifiers::CTRL, Key::P, -1),
                (Modifiers::NONE, Key::PageDown, page),
                (Modifiers::NONE, Key::PageUp, -page),
                (Modifiers::CTRL, Key::D, page / 2),
            ] {
                if i.consume_key(m, k) {
                    nav = d;
                }
            }
            if i.consume_key(Modifiers::SHIFT, Key::Enter) {
                action = Some(Action::Activate(self.selected, !paste));
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                action = Some(Action::Activate(self.selected, paste));
            }
            if i.consume_key(Modifiers::SHIFT, Key::Tab) {
                group = -1;
            }
            if i.consume_key(Modifiers::NONE, Key::Tab) {
                group = 1;
            }
            clear = i.consume_key(Modifiers::CTRL, Key::U);
            word = i.consume_key(Modifiers::CTRL, Key::W);
            esc = i.consume_key(Modifiers::NONE, Key::Escape) || i.consume_key(Modifiers::CTRL, Key::OpenBracket);
        });
        if nav != 0 {
            self.move_selection(nav);
        }
        if group != 0 {
            self.cycle_group(group);
        }
        if clear && !self.query.is_empty() {
            self.query.clear();
            self.dirty = true;
        }
        if word {
            let kept = self.query.trim_end().trim_end_matches(|c: char| !c.is_whitespace()).to_string();
            self.query = kept;
            self.dirty = true;
        }
        if esc {
            // like Vim: leave INSERT, keep the text; the list takes the keyboard
            if self.cfg.popup_focus == "search" && self.query.is_empty() {
                self.close(ctx);
            } else {
                self.search_mode = false;
                ctx.memory_mut(|m| m.surrender_focus(egui::Id::unique("search")));
            }
        }
        action
    }

    // ------------------------------------------------------------ COMMAND --

    fn command_keys(&mut self, ctx: &egui::Context) -> Option<Action> {
        let (enter, esc) = ctx.input_mut(|i| {
            (i.consume_key(Modifiers::NONE, Key::Enter), i.consume_key(Modifiers::NONE, Key::Escape) || i.consume_key(Modifiers::CTRL, Key::OpenBracket))
        });
        let empty_backspace = self.cmdline.as_deref() == Some("") && ctx.input(|i| i.key_pressed(Key::Backspace));
        if esc || empty_backspace {
            self.cmdline = None;
            return None;
        }
        if enter {
            let cmd = self.cmdline.take().unwrap_or_default();
            return self.execute(ctx, cmd.trim());
        }
        None
    }

    /// Run an ex command (`:q`, `:set bg=dark`, `:12` …).
    pub(super) fn execute(&mut self, ctx: &egui::Context, cmd: &str) -> Option<Action> {
        let cmd = cmd.trim_start_matches(':').trim();
        let (name, arg) = cmd.split_once(char::is_whitespace).map_or((cmd, ""), |(a, b)| (a, b.trim()));
        let sel = self.selected;
        match name {
            "" => {}
            "q" | "quit" | "x" | "wq" | "close" | "clo" => self.close(ctx),
            "q!" | "qa" | "qa!" | "qall" | "quit!" | "qall!" => self.pending_quit = true,
            "set" | "se" => self.set_option(arg),
            "colo" | "colorscheme" | "theme" => self.set_option(&format!("bg={arg}")),
            "lang" | "language" => self.set_option(&format!("lang={arg}")),
            "settings" | "options" | "opt" | "options!" => self.open_settings(),
            "sync" => {
                let _ = self.run_act(ctx, Act::Sync);
            }
            "groups" | "group" | "g" if arg.is_empty() => {
                let _ = self.run_act(ctx, Act::Groups);
            }
            "group" | "g" | "b" | "buffer" => {
                let by_number = arg.parse::<usize>().ok().map(|n| n.saturating_sub(1));
                let by_name = self.groups.list.iter().position(|g| g.name.eq_ignore_ascii_case(arg));
                match by_number.or(by_name) {
                    Some(i) if i < self.groups.list.len() => self.set_group(i),
                    _ => self.notify(tf!("没有这个分组：{arg}", "No such group: {arg}")),
                }
            }
            "bn" | "bnext" => self.cycle_group(1),
            "bp" | "bprev" | "bN" => self.cycle_group(-1),
            "pause" | "resume" => {
                let want = name == "pause";
                if self.paused != want {
                    let _ = self.run_act(ctx, Act::Pause);
                }
            }
            "help" | "h" => self.help = true,
            "update" | "checkupdate" => {
                let _ = self.run_act(ctx, Act::CheckUpdate);
            }
            "regex" => {
                self.regex = true;
                self.dirty = true;
            }
            "noregex" => {
                self.regex = false;
                self.dirty = true;
            }
            "y" | "yank" => return Some(Action::Activate(sel, false)),
            "p" | "put" | "paste" => return Some(Action::Activate(sel, true)),
            "d" | "delete" => return Some(Action::Delete(sel)),
            // :pin [group key / name / number]  (:unpin)
            "pin" | "mark" | "ma" if arg.is_empty() => return Some(Action::Pin(sel)),
            "pin" | "mark" => match self.groups.find_board(arg).map(|g| g.board) {
                Some(b) => return Some(Action::PinTo(sel, b)),
                None => self.notify(tf!("没有这个固定分组：{arg}", "no such pin group: {arg}")),
            },
            "unpin" => return Some(Action::PinTo(sel, 0)),
            "noh" | "nohlsearch" => {
                self.query.clear();
                self.dirty = true;
            }
            _ => match cmd.parse::<usize>() {
                Ok(n) => self.goto(n.saturating_sub(1)),
                Err(_) => self.notify(tf!("E492: 不是命令：{cmd}", "E492: Not an editor command: {cmd}")),
            },
        }
        None
    }

    /// `:set bg=dark`, `:set lang=en`, `:set regex` …
    fn set_option(&mut self, arg: &str) {
        let (key, value) = arg.split_once('=').map_or((arg.trim(), ""), |(k, v)| (k.trim(), v.trim()));
        match key {
            "bg" | "background" | "theme" => match value {
                "dark" | "light" | "system" => self.save_theme(value),
                _ => self.notify(t!("用法：:set bg=dark|light|system", "Usage: :set bg=dark|light|system")),
            },
            "lang" | "language" => match value {
                "zh" | "en" | "auto" => {
                    match crate::config::save_values(&[("language", value.into())]) {
                        Ok(()) => {
                            self.cfg.language = value.into();
                            crate::init_language(value);
                            self.dirty = true;
                        }
                        Err(e) => self.notify(e),
                    }
                }
                _ => self.notify(t!("用法：:set lang=zh|en|auto", "Usage: :set lang=zh|en|auto")),
            },
            "regex" | "re" => {
                self.regex = true;
                self.dirty = true;
            }
            "noregex" | "nore" => {
                self.regex = false;
                self.dirty = true;
            }
            "" => self.open_settings(),
            _ => self.notify(tf!("E518: 未知的选项：{key}", "E518: Unknown option: {key}")),
        }
    }

    pub(super) fn save_theme(&mut self, theme: &str) {
        match crate::config::save_values(&[("theme", theme.into())]) {
            Ok(()) => self.cfg.theme = theme.into(),
            Err(e) => self.notify(e),
        }
    }

    pub(super) fn open_settings(&mut self) {
        self.settings = Some(Settings::new(&self.cfg, self.autostart));
    }

    // --------------------------------------------------------------- help --

    pub(super) fn help_overlay(&mut self, ctx: &egui::Context, pal: &Palette) {
        let rows: Vec<(&str, &str)> = vec![
            ("j k  ↓ ↑", t!("下移 / 上移（可加计数：5j）", "down / up (with a count: 5j)")),
            ("gg G  12G", t!("第一条 / 最后一条 / 第 12 条", "first / last / item 12")),
            ("^d ^u  ^f ^b", t!("半页 / 整页", "half page / page")),
            ("H M L  { }", t!("可见区上中下 / 上一节下一节", "view top/middle/bottom / sections")),
            ("⏎  p", t!("粘贴到之前的窗口", "paste into the previous window")),
            ("yy  ⇧⏎", t!("只复制到剪贴板", "copy only")),
            ("dd  x", t!("删除", "delete")),
            ("m{x}  mm", t!("放进按键为 x 的固定分组（再按取消）/ 默认固定分组", "put into the pin group on key x (again: unpin) / the default one")),
            ("'{x}", t!("跳到按键为 x 的分组", "go to the group on key x")),
            ("o", t!("打开链接或路径", "open link or path")),
            ("/  i", t!("搜索（Esc 回到列表，保留关键词）", "search (Esc back to the list, keeps it)")),
            ("^u ^w", t!("搜索时清空 / 删除一个词", "while searching: clear / delete word")),
            ("gt gT  Tab", t!("下一个 / 上一个分组", "next / previous group")),
            ("Space", t!("操作菜单（字母直接执行）", "action menu (letters run actions)")),
            (":", t!("命令：:q :q! :5 :set bg=dark :set lang=en", "commands: :q :q! :5 :set bg=dark :set lang=en")),
            (":", t!("     :settings :sync :groups :pause :g 2", "     :settings :sync :groups :pause :g 2")),
            ("Esc  q  ZZ", t!("关闭（Esc 先清除计数和搜索）", "close (Esc clears count and search first)")),
            ("?", t!("这份帮助", "this help")),
        ];
        egui::Modal::new(egui::Id::unique("help")).frame(dialog_frame(pal).inner_margin(Margin::same(14))).show(ctx, |ui| {
            let (w, h) = dialog_room(ctx, 600.0);
            ui.set_width(w);
            ui.label(RichText::new(t!("-- 键盘 --", "-- KEYS --")).size(12.5).strong().color(pal.accent));
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(h - 70.0).show(ui, |ui| {
                egui::Grid::new("help-keys").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                    for (k, d) in rows {
                        ui.label(RichText::new(k).size(11.5).color(pal.accent));
                        ui.label(RichText::new(d).size(11.5).color(pal.text));
                        ui.end_row();
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(RichText::new(t!("按 Esc / q / ? 关闭", "Esc / q / ? to close")).size(11.5).color(pal.faint));
        });
    }
}
