//! User-defined history groups (stored as `groups.toml`).
//!
//! A group is a saved filter: content type, pinned, regexes on the content
//! (or the text recognized in an image) and regexes on the source app.
//! An item can belong to several groups. The popup lists them as tabs,
//! selectable with Ctrl/⌘ + 1…9.

use crate::model::{GroupRule, Kind};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GroupDef {
    pub name: String,
    /// "text", "image" or "" (both).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(skip_serializing_if = "is_false")]
    pub pinned: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    /// "Everything else": items not caught by any group that has patterns/apps.
    #[serde(skip_serializing_if = "is_false")]
    pub rest: bool,
    /// Pin group number (> 0): items are put here by hand (`m` + key in the
    /// popup, or the global `hotkey`) instead of matching rules.
    #[serde(skip_serializing_if = "is_zero")]
    pub board: u32,
    /// One character: `'` + key jumps to the group; for pin groups `m` + key
    /// pins the selected item into it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Global shortcut (pin groups): puts the current clipboard into the group.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hotkey: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Groups {
    /// Name of the group selected when the popup opens.
    pub default: String,
    #[serde(rename = "group")]
    pub list: Vec<GroupDef>,
}

impl Default for Groups {
    fn default() -> Self {
        let g = |name: &str, f: &dyn Fn(&mut GroupDef)| {
            let mut d = GroupDef { name: name.into(), ..Default::default() };
            f(&mut d);
            d
        };
        Groups {
            default: crate::t!("全部", "All").into(),
            list: vec![
                g(crate::t!("全部", "All"), &|_| {}),
                g(crate::t!("文本", "Text"), &|d| d.kind = "text".into()),
                g(crate::t!("图片", "Images"), &|d| d.kind = "image".into()),
                g(crate::t!("链接", "Links"), &|d| d.patterns = vec![r"(?i)^\s*(https?|ftp)://\S+\s*$".into()]),
                g(crate::t!("固定", "Pinned"), &|d| {
                    d.pinned = true;
                    d.board = 1;
                    d.key = "0".into();
                }),
            ],
        }
    }
}

pub const HEADER: &str = r#"# copym 分组。每个 [[group]] 是弹窗顶部的一个分组标签，按顺序对应 Ctrl+1…9（macOS 为 ⌘1…9）。
# 也可以在弹窗里右键分组标签 →“管理分组…”图形化编辑（会重写本文件）。
#
#   name      分组名称
#   kind      "text" 只含文本 / "image" 只含图片 / 不写 = 两者都含
#   pinned    true = 只含固定的条目
#   patterns  正则列表：内容（图片则为识别出的文字）匹配任意一条即属于该组；开头加 (?i) 忽略大小写
#   apps      正则列表：来源匹配任意一条（来源应用名，Windows / macOS；或同步过来的设备名）
#   rest      true = “其他”：不属于任何设置了 patterns / apps 的分组的条目
#   board     固定分组编号（1、2、3…，不能重复）：条目是手动放进来的，不按规则匹配
#   key       一个字符：弹窗里 ' + key 跳到该分组；固定分组还可以用 m + key 把选中的条目放进来（再按一次取消）
#   hotkey    固定分组的全局快捷键：不用打开弹窗，把当前剪贴板的内容放进该分组（例如 "Ctrl+Alt+1"，改后重启生效）
#
# 一个条目可以同时属于多个分组。default 是打开弹窗时默认选中的分组。
#
# 示例：
# [[group]]
# name = "验证码"
# patterns = ['(验证码|校验码|code)\D{0,12}\d{4,8}']
#
# [[group]]
# name = "代码"
# apps = ['(?i)code|idea|terminal|konsole']
#
# [[group]]
# name = "常用回复"
# board = 2
# key = "r"
# hotkey = "Ctrl+Alt+R"

"#;

/// Serialize with an explanatory header (the file users edit by hand).
pub fn to_toml(g: &Groups) -> Result<String, String> {
    Ok(format!("{HEADER}{}", toml::to_string_pretty(g).map_err(|e| e.to_string())?))
}

pub fn from_toml(s: &str) -> Result<Groups, String> {
    let mut g: Groups = toml::from_str(s).map_err(|e| e.to_string())?;
    if g.list.is_empty() {
        return Err(crate::t!("没有任何分组", "there are no groups").into());
    }
    g.localize();
    // the built-in pin group used to answer to m (mm); it is m0 / '0 now
    for d in g.list.iter_mut().filter(|d| d.board == 1 && d.key == "m" && BUILT_IN.iter().any(|(zh, en)| d.name == *zh || d.name == *en)) {
        d.key = "0".into();
    }
    Ok(g)
}

/// Names of the built-in groups in both languages.
const BUILT_IN: [(&str, &str); 5] = [("全部", "All"), ("文本", "Text"), ("图片", "Images"), ("链接", "Links"), ("固定", "Pinned")];

impl Groups {
    /// Built-in group names (still as created, in either language) follow
    /// the current UI language; names the user chose stay as they are.
    pub fn localize(&mut self) {
        let fix = |name: &mut String| {
            if let Some((zh, en)) = BUILT_IN.iter().find(|(zh, en)| name == zh || name == en) {
                let want = crate::t!(*zh, *en);
                if name != want {
                    *name = want.to_string();
                }
            }
        };
        let names: Vec<String> = self.list.iter().map(|g| g.name.clone()).collect();
        for g in &mut self.list {
            fix(&mut g.name);
        }
        // `default` points at a group by name: follow its rename
        if let Some(i) = names.iter().position(|n| *n == self.default) {
            self.default = self.list[i].name.clone();
        }
    }

    pub fn default_index(&self) -> usize {
        self.list.iter().position(|g| g.name == self.default).unwrap_or(0)
    }

    /// Rule for group `i`; "rest" groups exclude what the rule-based groups catch.
    pub fn rule(&self, i: usize) -> GroupRule {
        let Some(g) = self.list.get(i) else { return GroupRule::default() };
        let mut r = g.base_rule();
        if g.rest {
            for (j, o) in self.list.iter().enumerate() {
                if j != i && !o.rest {
                    r.exclude.extend(o.patterns.iter().filter(|p| !p.trim().is_empty()).cloned());
                    r.exclude_apps.extend(o.apps.iter().filter(|p| !p.trim().is_empty()).cloned());
                }
            }
        }
        r
    }

    pub fn rules(&self) -> Vec<GroupRule> {
        (0..self.list.len()).map(|i| self.rule(i)).collect()
    }

    /// Pin groups: (index in the list, definition).
    pub fn boards(&self) -> impl Iterator<Item = (usize, &GroupDef)> {
        self.list.iter().enumerate().filter(|(_, g)| g.board > 0)
    }

    /// The pin group that `TogglePin` / a bare "pin" uses.
    pub fn first_board(&self) -> u32 {
        self.boards().next().map(|(_, g)| g.board).unwrap_or(1)
    }

    /// Group whose `key` is `c` (pin groups only when `boards_only`).
    pub fn by_key(&self, c: char, boards_only: bool) -> Option<(usize, &GroupDef)> {
        self.list.iter().enumerate().find(|(_, g)| (!boards_only || g.board > 0) && g.key.chars().next() == Some(c) && g.key.chars().count() == 1)
    }

    /// Pin group by number, key or name (for `copym pin`).
    pub fn find_board(&self, s: &str) -> Option<&GroupDef> {
        let s = s.trim();
        self.boards().map(|(_, g)| g).find(|g| g.board.to_string() == s || g.key == s || g.name == s)
    }

    pub fn board_name(&self, board: u32) -> Option<&str> {
        self.boards().find(|(_, g)| g.board == board).map(|(_, g)| g.name.as_str())
    }

    /// A number no pin group uses yet.
    pub fn next_board(&self) -> u32 {
        self.list.iter().map(|g| g.board).max().unwrap_or(0) + 1
    }

    /// Every group valid, and no key / pin number / shortcut used twice.
    pub fn validate(&self) -> Result<(), String> {
        let mut names = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        let mut boards = std::collections::HashSet::new();
        let mut hotkeys = std::collections::HashSet::new();
        for g in &self.list {
            g.validate().map_err(|e| format!("「{}」：{e}", g.name))?;
            if !names.insert(g.name.trim()) {
                return Err(crate::tf!("分组名称重复：{}", "duplicate group name: {}", g.name));
            }
            if !g.key.is_empty() && !keys.insert(g.key.as_str()) {
                return Err(crate::tf!("按键 {} 被多个分组使用", "key {} is used by several groups", g.key));
            }
            if g.board > 0 && !boards.insert(g.board) {
                return Err(crate::tf!("固定分组编号 {} 重复", "pin group number {} is used twice", g.board));
            }
            if !g.hotkey.trim().is_empty() && !hotkeys.insert(g.hotkey.trim().to_lowercase()) {
                return Err(crate::tf!("快捷键 {} 被多个分组使用", "shortcut {} is used by several groups", g.hotkey));
            }
        }
        Ok(())
    }
}

impl GroupDef {
    fn base_rule(&self) -> GroupRule {
        GroupRule {
            kind: match self.kind.as_str() {
                "text" => Some(Kind::Text),
                "image" => Some(Kind::Image),
                _ => None,
            },
            pinned: self.pinned,
            patterns: self.patterns.clone(),
            apps: self.apps.clone(),
            exclude: Vec::new(),
            exclude_apps: Vec::new(),
            board: (self.board > 0).then_some(self.board),
        }
    }

    /// First invalid regex, if any (for the editor).
    pub fn validate(&self) -> Result<(), String> {
        for p in self.patterns.iter().chain(&self.apps).filter(|p| !p.trim().is_empty()) {
            regex_lite::Regex::new(p).map_err(|e| format!("{p}：{e}"))?;
        }
        if self.name.trim().is_empty() {
            return Err(crate::t!("分组名称不能为空", "a group needs a name").into());
        }
        let n = self.key.chars().count();
        if n > 1 || self.key.chars().any(char::is_whitespace) {
            return Err(crate::t!("按键只能是一个字符", "the key must be one character").into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built-in names follow the language (in either direction); the user's
    /// own names and the default pointer stay consistent.
    #[test]
    fn built_in_names_follow_the_language() {
        let mut g = Groups { default: "Text".into(), list: Vec::new() };
        for name in ["全部", "Text", "图片", "Links", "Pinned", "我的分组"] {
            g.list.push(GroupDef { name: name.into(), ..Default::default() });
        }
        g.localize();
        let names: Vec<&str> = g.list.iter().map(|d| d.name.as_str()).collect();
        let want = [crate::t!("全部", "All"), crate::t!("文本", "Text"), crate::t!("图片", "Images"), crate::t!("链接", "Links"), crate::t!("固定", "Pinned"), "我的分组"];
        assert_eq!(names, want);
        assert_eq!(g.default, crate::t!("文本", "Text"));
    }

    /// Old files put the built-in pin group on m; it moves to 0 (m0), a
    /// pin group of the user's own keeps its key.
    #[test]
    fn built_in_pin_group_moves_to_0() {
        let old = "default = \"全部\"\n[[group]]\nname = \"全部\"\n[[group]]\nname = \"固定\"\npinned = true\nboard = 1\nkey = \"m\"\n[[group]]\nname = \"Mine\"\nboard = 2\nkey = \"m\"\n";
        let g = from_toml(old).unwrap();
        let keys: Vec<&str> = g.list.iter().map(|d| d.key.as_str()).collect();
        assert_eq!(keys, ["", "0", "m"]);
        assert_eq!(Groups::default().list[4].key, "0");
    }

    #[test]
    fn roundtrip_and_rest_rule() {
        let mut g = Groups::default();
        g.list.push(GroupDef { name: crate::t!("其他", "Other").into(), kind: "text".into(), rest: true, ..Default::default() });
        g.list.push(GroupDef { name: "IDE".into(), apps: vec!["(?i)code".into()], ..Default::default() });
        let back = from_toml(&to_toml(&g).unwrap()).unwrap();
        assert_eq!(back, g);
        let rest = g.rule(5);
        assert_eq!(rest.kind, Some(Kind::Text));
        assert_eq!(rest.exclude.len(), 1); // the link pattern
        assert_eq!(rest.exclude_apps, vec!["(?i)code".to_string()]);
        assert_eq!(g.default_index(), 0);
        assert!(GroupDef { name: "x".into(), patterns: vec!["(".into()], ..Default::default() }.validate().is_err());
    }
}
