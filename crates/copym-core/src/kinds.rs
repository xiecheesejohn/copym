//! Content type detection (link, color, JSON, code, path, e-mail) used by
//! the desktop popup and the Android app to pick icons and previews.

use serde::Serialize;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TextKind {
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
    pub fn label(self) -> &'static str {
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

    pub fn glyph(self) -> &'static str {
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

    pub fn as_str(self) -> &'static str {
        match self {
            TextKind::Plain => "plain",
            TextKind::Url => "url",
            TextKind::Email => "email",
            TextKind::Path => "path",
            TextKind::Color => "color",
            TextKind::Json => "json",
            TextKind::Code => "code",
            TextKind::Image => "image",
        }
    }
}

/// Guess what a piece of text is. `complete` = the full text (JSON is then
/// actually parsed); otherwise a cheap guess on a preview.
pub fn classify(text: &str, complete: bool) -> TextKind {
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

/// Pretty-printed JSON (keys keep their order), or `None` if not JSON.
pub fn pretty_json(s: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(s.trim()).ok().and_then(|v| serde_json::to_string_pretty(&v).ok())
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

/// `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(r, g, b)`, `rgba(r, g, b, a)` -> RGBA.
pub fn parse_color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix('#') {
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
        return match h.len() {
            3 => Some([v(0, 1)? * 17, v(1, 1)? * 17, v(2, 1)? * 17, 255]),
            6 => Some([v(0, 2)?, v(2, 2)?, v(4, 2)?, 255]),
            8 => Some([v(0, 2)?, v(2, 2)?, v(4, 2)?, v(6, 2)?]),
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
    Some([c(parts[0])?, c(parts[1])?, c(parts[2])?, a])
}

pub fn hsl([r, g, b, _]: [u8; 4]) -> (u32, u32, u32) {
    let [r, g, b] = [r, g, b].map(|v| v as f32 / 255.0);
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

/// HEX, RGB and HSL notations of a color.
pub fn color_notations(c: [u8; 4]) -> [(&'static str, String); 3] {
    let [r, g, b, a] = c;
    let (h, s, l) = hsl(c);
    [
        ("HEX", if a == 255 { format!("#{r:02X}{g:02X}{b:02X}") } else { format!("#{r:02X}{g:02X}{b:02X}{a:02X}") }),
        ("RGB", if a == 255 { format!("rgb({r}, {g}, {b})") } else { format!("rgba({r}, {g}, {b}, {:.2})", a as f32 / 255.0) }),
        ("HSL", format!("hsl({h}, {s}%, {l}%)")),
    ]
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
        assert_eq!(parse_color("#fff"), Some([255, 255, 255, 255]));
        assert_eq!(hsl([255, 0, 0, 255]), (0, 100, 50));
        assert_eq!(pretty_json(r#"{"b":1,"a":2}"#).unwrap(), "{\n  \"b\": 1,\n  \"a\": 2\n}");
        assert_eq!(color_notations([16, 185, 129, 204])[0].1, "#10B981CC");
    }
}
