//! UI language: Chinese or English, chosen once at start (config `language`,
//! or the system language). Strings live next to their use as pairs:
//! `t!("中文", "English")` for fixed text, `tf!("{n} 条", "{n} items")` for
//! formatted text (arguments are captured like in `format!`).

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// "zh" / "en" / anything else = follow `system` (a BCP 47 tag such as "zh-CN", "en-US").
    pub fn resolve(setting: &str, system: Option<&str>) -> Lang {
        match setting.trim().to_lowercase().as_str() {
            "zh" | "zh-cn" | "chinese" | "中文" => Lang::Zh,
            "en" | "english" => Lang::En,
            _ => match system {
                Some(tag) if !tag.to_lowercase().starts_with("zh") => Lang::En,
                _ => Lang::Zh,
            },
        }
    }
}

pub fn set_lang(lang: Lang) {
    ENGLISH.store(lang == Lang::En, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if is_en() { Lang::En } else { Lang::Zh }
}

#[inline]
pub fn is_en() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Fixed text in the current language.
#[macro_export]
macro_rules! t {
    ($zh:expr, $en:expr $(,)?) => {
        if $crate::i18n::is_en() { $en } else { $zh }
    };
}

/// Formatted text in the current language (a `String`).
#[macro_export]
macro_rules! tf {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_en() { format!($en $(, $arg)*) } else { format!($zh $(, $arg)*) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_and_format() {
        assert_eq!(Lang::resolve("auto", Some("zh-Hans-CN")), Lang::Zh);
        assert_eq!(Lang::resolve("auto", Some("en-US")), Lang::En);
        assert_eq!(Lang::resolve("", Some("de-DE")), Lang::En);
        assert_eq!(Lang::resolve("auto", None), Lang::Zh);
        assert_eq!(Lang::resolve("en", Some("zh-CN")), Lang::En);
        // the global language stays Chinese in tests (they run in parallel)
        let n = 3;
        assert_eq!(tf!("{n} 条", "{n} items"), "3 条");
        assert_eq!(t!("删除", "Delete"), "删除");
    }
}
