//! Offscreen renders of the popup for checking layouts, themes and languages
//! without opening a window:
//!
//!     COPYM_SHOTS=/tmp/shots cargo test shots -- --ignored --test-threads=1
//!
//! writes one PNG per state (needs a GPU or a software Vulkan driver).

use super::*;
use copym_core::i18n::{set_lang, Lang};
use egui_kittest::Harness;

fn item(id: u64, kind: Kind, preview: &str, mins_ago: i64, pinned: bool) -> ItemMeta {
    ItemMeta {
        id,
        kind,
        preview: preview.into(),
        chars: preview.chars().count() as u32,
        lines: preview.lines().count().max(1) as u32,
        bytes: preview.len() as u64,
        width: if kind == Kind::Image { 640 } else { 0 },
        height: if kind == Kind::Image { 174 } else { 0 },
        created: now_secs() - mins_ago * 60,
        pinned,
        source: Some("Safari".into()),
        hash: id,
        ocr: None,
        board: pinned as u32,
    }
}

fn app(theme: &str, lang: Lang) -> App {
    set_lang(lang);
    let cfg = Config { theme: theme.into(), language: if lang == Lang::En { "en" } else { "zh" }.into(), ..Config::default() };
    let mut app = App::new(cfg, Arc::default(), Arc::default());
    app.error = None;
    app.dirty = false;
    app.items = vec![
        item(1, Kind::Text, "您的验证码是 482913，5 分钟内有效。", 1, false),
        item(2, Kind::Text, "https://github.com/xiecheesejohn/copym/releases/latest", 5, true),
        item(3, Kind::Text, "fn main() {\n    println!(\"hello\");\n}", 30, false),
        item(4, Kind::Text, "Mixed English 与中文 gjpqy 混排 descenders and a long line that gets cut", 90, false),
        item(5, Kind::Text, "~/Documents/copym/README.md", 60 * 26, false),
    ];
    app.layout_list();
    app.selected = 0;
    app.detail = Some((1, Detail::Text { kind: TextKind::Plain, body: "您的验证码是 482913，5 分钟内有效。".into() }));
    app
}

fn shoot(name: &str, app: App, size: Vec2, prepare: impl FnOnce(&mut App)) {
    shoot_click(name, app, size, prepare, None)
}

/// Like [`shoot`], clicking at `click` (points) once the UI is up.
fn shoot_click(name: &str, mut app: App, size: Vec2, prepare: impl FnOnce(&mut App), click: Option<Pos2>) {
    let Some(dir) = std::env::var_os("COPYM_SHOTS") else { return };
    prepare(&mut app);
    let mut fonts_ready = false;
    let mut harness = Harness::builder().with_size(size).with_pixels_per_point(2.0).with_accessibility_check(false).build_ui_state(
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
    for _ in 0..6 {
        harness.step();
    }
    if let Some(pos) = click {
        harness.hover_at(pos);
        harness.step();
        harness.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
        harness.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
        for _ in 0..4 {
            harness.step();
        }
    }
    let image = harness.render().expect("render");
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    image.save(&path).expect("save");
    eprintln!("wrote {}", path.display());
}

const WINDOW: Vec2 = Vec2::new(760.0, 480.0);

#[test]
#[ignore = "renders PNGs; run with COPYM_SHOTS=dir"]
fn shots() {
    for (theme, t) in [("dark", "dark"), ("light", "light")] {
        for (lang, l) in [(Lang::Zh, "zh"), (Lang::En, "en")] {
            shoot(&format!("main-{t}-{l}"), app(theme, lang), WINDOW, |_| {});
            shoot(&format!("actions-{t}-{l}"), app(theme, lang), WINDOW, |a| a.actions = Some(0));
            shoot(&format!("insert-{t}-{l}"), app(theme, lang), WINDOW, |a| {
                a.search_mode = true;
                a.query = "验证码".into();
                a.selected = 2;
            });
            shoot(&format!("command-{t}-{l}"), app(theme, lang), WINDOW, |a| a.cmdline = Some("set bg=light".into()));
            shoot(&format!("help-{t}-{l}"), app(theme, lang), WINDOW, |a| a.help = true);
            shoot(&format!("help-commands-{t}-{l}"), app(theme, lang), WINDOW, |a| {
                a.help = true;
                a.help_scroll = 1000.0;
            });
            let power = Pos2::new(WINDOW.x - 10.0 - 13.0, WINDOW.y - FOOTER_H / 2.0);
            shoot_click(&format!("power-{t}-{l}"), app(theme, lang), WINDOW, |_| {}, Some(power));
            // the group list (top right), with the default marker and counts
            let groups = Pos2::new(WINDOW.x - 12.0 - 40.0, SEARCH_H / 2.0);
            shoot_click(&format!("groups-{t}-{l}"), app(theme, lang), WINDOW, |a| a.counts = vec![Some(5), Some(4), Some(1), Some(1), Some(1)], Some(groups));
            // several pin groups: sections per group, 'key markers, which-key after m
            let boards = |a: &mut App| {
                use crate::groups::GroupDef;
                a.groups.list.push(GroupDef { name: t!("常用回复", "Replies").into(), pinned: true, board: 2, key: "r".into(), ..Default::default() });
                a.groups.list.push(GroupDef { name: t!("服务器", "Servers").into(), pinned: true, board: 3, key: "s".into(), ..Default::default() });
                a.items[0].pinned = true;
                a.items[0].board = 2;
                a.items[2].pinned = true;
                a.items[2].board = 3;
                // the "pinned" group lists them by pin group
                a.items.retain(|m| m.pinned);
                a.items.sort_by_key(|m| m.board);
                a.group = a.groups.list.iter().position(|g| g.pinned).unwrap_or(0);
                a.layout_list();
                a.pending = "m".into();
            };
            shoot(&format!("boards-{t}-{l}"), app(theme, lang), WINDOW, boards);
            shoot(&format!("boards-menu-{t}-{l}"), app(theme, lang), WINDOW, |a| {
                boards(a);
                a.pending.clear();
                a.selected = 0;
                a.actions = Some(0);
            });
            for section in 0..6 {
                shoot(&format!("settings{section}-{t}-{l}"), app(theme, lang), WINDOW, |a| {
                    let mut st = Settings::new(&a.cfg, false);
                    st.section = section;
                    a.settings = Some(st);
                });
            }
        }
    }
    set_lang(Lang::Zh);
}
