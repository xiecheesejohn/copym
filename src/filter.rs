//! User-configurable filters (ignored apps, ignored regex patterns, size limits).

use crate::config::Config;
use crate::model::Content;

pub struct Filters {
    apps: Vec<String>,
    patterns: Vec<regex_lite::Regex>,
    max_text: usize,
    max_image: usize,
    store_images: bool,
}

impl Filters {
    pub fn new(cfg: &Config) -> Self {
        let patterns = cfg
            .ignore_patterns
            .iter()
            .filter_map(|p| match regex_lite::Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("copym: bad ignore_pattern {p:?}: {e}");
                    None
                }
            })
            .collect();
        let mut f = Self { apps: Vec::new(), patterns, max_text: cfg.max_text_bytes, max_image: cfg.max_image_bytes, store_images: cfg.store_images };
        f.set_apps(&cfg.ignore_apps);
        f
    }

    /// Apps whose copies are not recorded (case-insensitive substring of the source).
    pub fn set_apps(&mut self, apps: &[String]) {
        self.apps = apps.iter().map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty()).collect();
    }

    /// Returns `Some(reason)` when the content must not be recorded.
    pub fn reject(&self, content: &Content, source: Option<&str>) -> Option<&'static str> {
        if let Some(src) = source {
            let src = src.to_lowercase();
            if self.apps.iter().any(|a| src.contains(a.as_str())) {
                return Some("ignored application");
            }
        }
        match content {
            Content::Text(t) => {
                if t.trim().is_empty() {
                    return Some("empty text");
                }
                if t.len() > self.max_text {
                    return Some("text too large");
                }
                if self.patterns.iter().any(|r| r.is_match(t)) {
                    return Some("matches ignore pattern");
                }
            }
            Content::Image { png, .. } => {
                if !self.store_images {
                    return Some("images disabled");
                }
                if png.len() > self.max_image {
                    return Some("image too large");
                }
            }
        }
        None
    }
}
