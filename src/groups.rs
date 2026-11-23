//! User-defined history groups (`groups.toml` next to `config.toml`).
//!
//! A group is a saved filter: content type, pinned, regexes on the content
//! (or the text recognized in an image) and regexes on the source app.
//! An item can belong to several groups. The popup lists them as tabs,
//! selectable with Ctrl/⌘ + 1…9.

use crate::model::{GroupRule, Kind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}

fn is_false(b: &bool) -> bool {
    !*b
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
            default: "全部".into(),
            list: vec![
                g("全部", &|_| {}),
                g("文本", &|d| d.kind = "text".into()),
                g("图片", &|d| d.kind = "image".into()),
                g("链接", &|d| d.patterns = vec![r"(?i)^\s*(https?|ftp)://\S+\s*$".into()]),
                g("固定", &|d| d.pinned = true),
            ],
        }
    }
}

const HEADER: &str = r#"# copym 分组。每个 [[group]] 是弹窗顶部的一个分组标签，按顺序对应 Ctrl+1…9（macOS 为 ⌘1…9）。
# 也可以在弹窗里右键分组标签 →“管理分组…”图形化编辑（会重写本文件）。
#
#   name      分组名称
#   kind      "text" 只含文本 / "image" 只含图片 / 不写 = 两者都含
#   pinned    true = 只含固定的条目
#   patterns  正则列表：内容（图片则为识别出的文字）匹配任意一条即属于该组；开头加 (?i) 忽略大小写
#   apps      正则列表：来源应用名称匹配任意一条（Windows / macOS 能取到来源应用）
#   rest      true = “其他”：不属于任何设置了 patterns / apps 的分组的条目
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

"#;

pub fn path() -> PathBuf {
    crate::config::config_path().with_file_name("groups.toml")
}

/// Load the groups; writes the defaults on first run. The error (if any)
/// describes a broken file - the defaults are used in that case.
pub fn load() -> (Groups, Option<String>) {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str::<Groups>(&s) {
            Ok(g) if !g.list.is_empty() => (g, None),
            Ok(_) => (Groups::default(), Some(format!("{} 中没有分组，已使用默认分组", path.display()))),
            Err(e) => (Groups::default(), Some(format!("{} 格式有误，已使用默认分组：{e}", path.display()))),
        },
        Err(_) => {
            let g = Groups::default();
            let _ = save(&g);
            (g, None)
        }
    }
}

pub fn save(g: &Groups) -> Result<(), String> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = toml::to_string_pretty(g).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, format!("{HEADER}{body}")).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

impl Groups {
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
        }
    }

    /// First invalid regex, if any (for the editor).
    pub fn validate(&self) -> Result<(), String> {
        for p in self.patterns.iter().chain(&self.apps).filter(|p| !p.trim().is_empty()) {
            regex_lite::Regex::new(p).map_err(|e| format!("{p}：{e}"))?;
        }
        if self.name.trim().is_empty() {
            return Err("分组名称不能为空".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_rest_rule() {
        let mut g = Groups::default();
        g.list.push(GroupDef { name: "其他".into(), kind: "text".into(), rest: true, ..Default::default() });
        g.list.push(GroupDef { name: "IDE".into(), apps: vec!["(?i)code".into()], ..Default::default() });
        let text = format!("{HEADER}{}", toml::to_string_pretty(&g).unwrap());
        let back: Groups = toml::from_str(&text).unwrap();
        assert_eq!(back, g);
        let rest = g.rule(5);
        assert_eq!(rest.kind, Some(Kind::Text));
        assert_eq!(rest.exclude.len(), 1); // the link pattern
        assert_eq!(rest.exclude_apps, vec!["(?i)code".to_string()]);
        assert_eq!(g.default_index(), 0);
        assert!(GroupDef { name: "x".into(), patterns: vec!["(".into()], ..Default::default() }.validate().is_err());
    }
}
