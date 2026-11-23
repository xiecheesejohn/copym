//! Text recognition for image items, so screenshots become searchable.
//!
//! Engines (`ocr_engine = "auto"` picks the first available):
//! * macOS   - Apple Vision (built in, zh-Hans/zh-Hant/en/...)
//! * Windows - Windows.Media.Ocr (built in, uses the user's language packs)
//! * any OS  - Tesseract CLI (`tesseract` + language packs, e.g. chi_sim)
//!
//! OCR runs on a single low-priority background thread; it never blocks
//! clipboard capture.

use crate::config::Config;
use crate::imgutil;
use std::path::PathBuf;

/// Bump when the recognition pipeline improves: the daemon then re-runs OCR
/// on every stored image once. 1 = raw images, 2 = preprocessing + tiling.
pub const REVISION: u64 = 2;

pub struct Ocr {
    engine: Engine,
}

enum Engine {
    Tesseract { bin: String, langs: String, tessdata: Option<PathBuf> },
    #[cfg(target_os = "macos")]
    Vision { langs: Vec<String> },
    #[cfg(windows)]
    WinRt,
}

impl Ocr {
    pub fn detect(cfg: &Config) -> Option<Self> {
        if !cfg.ocr_enabled {
            return None;
        }
        let want = cfg.ocr_engine.to_ascii_lowercase();
        if want == "off" {
            return None;
        }
        if want == "auto" || want == "native" {
            #[cfg(target_os = "macos")]
            return Some(Self { engine: Engine::Vision { langs: vision_langs(&cfg.ocr_languages) } });
            #[cfg(windows)]
            if winrt::available() {
                return Some(Self { engine: Engine::WinRt });
            }
        }
        if want == "auto" || want == "tesseract" {
            if let Some(bin) = find_tesseract() {
                let tessdata = Some(models_dir()).filter(|d| has_models(d));
                let langs = tesseract_langs(&bin, tessdata.as_deref(), &cfg.ocr_languages)?;
                return Some(Self { engine: Engine::Tesseract { bin, langs, tessdata } });
            }
        }
        None
    }

    pub fn name(&self) -> String {
        match &self.engine {
            Engine::Tesseract { langs, tessdata, .. } => {
                format!("tesseract ({langs}{})", if tessdata.is_some() { ", 高精度模型" } else { "" })
            }
            #[cfg(target_os = "macos")]
            Engine::Vision { langs } => format!("apple vision ({})", langs.join(", ")),
            #[cfg(windows)]
            Engine::WinRt => format!("windows.media.ocr ({})", winrt::language()),
        }
    }

    /// Recognize text in a PNG. Returns normalized text (may be empty).
    pub fn recognize(&self, png: &[u8]) -> Result<String, String> {
        let (w, h) = imgutil::png_size(png).ok_or("not a png")?;
        if (w as u64) * (h as u64) > 50_000_000 {
            return Err("image too large for OCR".into());
        }
        let (w, h, mut rgba) = imgutil::decode_png(png).ok_or("cannot decode png")?;
        imgutil::flatten_on_white(&mut rgba);
        let raw = match &self.engine {
            Engine::Tesseract { bin, langs, tessdata } => {
                let input = tesseract_input(w, h, &rgba).ok_or("cannot encode image")?;
                tesseract(bin, langs, tessdata.as_deref(), &input)?
            }
            #[cfg(target_os = "macos")]
            Engine::Vision { langs } => by_tiles(w, h, &rgba, |tw, th, tile| vision::recognize(tile, tw, th, langs))?,
            #[cfg(windows)]
            Engine::WinRt => by_tiles(w, h, &rgba, |tw, th, tile| winrt::recognize(tile, tw, th))?,
        };
        Ok(normalize(&raw))
    }
}

// ----------------------------------------------------------- preprocessing --

/// Tesseract's LSTM models want text around 20-40 px high; ordinary
/// screenshots have 12-16 px text. Grayscale, light background and a 2x
/// upscale take Chinese CER from ~45% to ~8% (measured, see README).
fn tesseract_input(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut gray = imgutil::to_gray(rgba);
    let mean = gray.iter().map(|&v| v as u64).sum::<u64>() / gray.len().max(1) as u64;
    if mean < 110 {
        // dark theme: light text on dark background
        gray.iter_mut().for_each(|v| *v = 255 - *v);
    }
    let mut factor = match text_height(w, h, &gray) {
        // measured best: 11 px lines -> 2x, 24 px lines -> as is
        Some(t) if t < 9 => 3,
        Some(t) if t < 20 => 2,
        Some(_) => 1,
        None => 1,
    };
    while factor > 1 && (w as u64) * (h as u64) * (factor as u64).pow(2) > 24_000_000 {
        factor -= 1;
    }
    let (w, h, gray) = imgutil::upscale_gray(w, h, &gray, factor);
    imgutil::encode_gray_png(w, h, &gray)
}

/// Median height (px) of the horizontal bands that contain dark "ink" -
/// roughly the height of a text line. `None` if there is no text-like ink.
fn text_height(w: u32, h: u32, gray: &[u8]) -> Option<u32> {
    let w = w as usize;
    let min_ink = (w / 400).max(1);
    let mut runs = Vec::new();
    let mut run = 0u32;
    for y in 0..h as usize {
        let row = &gray[y * w..(y + 1) * w];
        let ink = row.iter().filter(|&&v| v < 128).count();
        // ink, but not a solid bar (rules, filled buttons)
        if ink >= min_ink && ink < w * 9 / 10 {
            run += 1;
        } else if run > 0 {
            runs.push(run);
            run = 0;
        }
    }
    if run > 0 {
        runs.push(run);
    }
    runs.retain(|&r| r >= 4); // noise, underlines
    if runs.is_empty() {
        return None;
    }
    runs.sort_unstable();
    Some(runs[runs.len() / 2])
}

/// Long screenshots get downscaled inside the native engines until the text
/// is unreadable; recognize them in overlapping, roughly page-shaped tiles.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn by_tiles(w: u32, h: u32, rgba: &[u8], mut run: impl FnMut(u32, u32, &[u8]) -> Result<String, String>) -> Result<String, String> {
    let mut parts = Vec::new();
    for (y0, y1) in tile_rows(w, h) {
        let tile = imgutil::crop_rows(w, rgba, y0, y1);
        parts.push(run(w, y1 - y0, &tile)?);
    }
    Ok(merge_tiles(&parts))
}

fn tile_rows(w: u32, h: u32) -> Vec<(u32, u32)> {
    let tile = (w * 3 / 2).max(1200);
    if h <= tile * 4 / 3 {
        return vec![(0, h)];
    }
    let overlap = tile / 8;
    let mut out = Vec::new();
    let mut y = 0;
    loop {
        let end = (y + tile).min(h);
        out.push((y, end));
        if end == h {
            return out;
        }
        y = end - overlap;
    }
}

/// Join tile results, dropping lines repeated in the overlap.
fn merge_tiles(parts: &[String]) -> String {
    let key = |l: &str| l.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let mut lines: Vec<String> = Vec::new();
    for part in parts {
        let recent: Vec<String> = lines.iter().rev().take(12).map(|l| key(l)).collect();
        let mut skipping = true;
        for l in part.lines().filter(|l| !l.trim().is_empty()) {
            if skipping && recent.contains(&key(l)) {
                continue;
            }
            skipping = false;
            lines.push(l.to_string());
        }
    }
    lines.join("\n")
}

// ------------------------------------------------------------ best models --

/// Where `copym ocr models` puts tessdata_best models; used when present.
pub fn models_dir() -> PathBuf {
    crate::config::data_dir().join("tessdata")
}

fn has_models(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "traineddata")))
}

/// Download the high-accuracy Tesseract models (tessdata_best) with curl.
pub fn download_models(langs: &str) -> Result<PathBuf, String> {
    let dir = models_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for lang in langs.split('+').map(str::trim).filter(|l| !l.is_empty()) {
        if !lang.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("invalid language name: {lang}"));
        }
        let dest = dir.join(format!("{lang}.traineddata"));
        if dest.exists() {
            println!("  {lang}: already present");
            continue;
        }
        let url = format!("https://github.com/tesseract-ocr/tessdata_best/raw/main/{lang}.traineddata");
        println!("  {lang}: downloading {url}");
        let part = dir.join(format!("{lang}.traineddata.part"));
        let ok = std::process::Command::new("curl")
            .args(["-fL", "--retry", "2", "--progress-bar", "-o"])
            .arg(&part)
            .arg(&url)
            .status()
            .map_err(|e| format!("cannot run curl: {e}"))?
            .success();
        if !ok {
            let _ = std::fs::remove_file(&part);
            return Err(format!("download of {lang} failed"));
        }
        std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

/// "chi_sim+chi_tra+eng" (Tesseract names) -> Vision language codes.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn vision_langs(spec: &str) -> Vec<String> {
    let mut out: Vec<String> = spec
        .split('+')
        .filter_map(|l| match l.trim() {
            "chi_sim" => Some("zh-Hans"),
            "chi_tra" => Some("zh-Hant"),
            "eng" => Some("en-US"),
            "jpn" => Some("ja-JP"),
            "kor" => Some("ko-KR"),
            "fra" => Some("fr-FR"),
            "deu" => Some("de-DE"),
            "spa" => Some("es-ES"),
            "rus" => Some("ru-RU"),
            _ => None,
        })
        .map(String::from)
        .collect();
    out.dedup();
    if out.is_empty() {
        out = vec!["zh-Hans".into(), "zh-Hant".into(), "en-US".into()];
    }
    out
}

/// Lower the priority of the current thread (and the processes it spawns).
pub fn lower_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        // On Linux nice values are per-thread; children inherit them.
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
}

// --------------------------------------------------------------- tesseract --

fn find_tesseract() -> Option<String> {
    let exe = if cfg!(windows) { "tesseract.exe" } else { "tesseract" };
    let mut dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", r"C:\Program Files\Tesseract-OCR"]
            .iter()
            .map(std::path::PathBuf::from),
    );
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
}

/// Keep only the configured languages that are actually installed.
fn tesseract_langs(bin: &str, tessdata: Option<&std::path::Path>, wanted: &str) -> Option<String> {
    let mut cmd = std::process::Command::new(bin);
    if let Some(d) = tessdata {
        cmd.arg("--tessdata-dir").arg(d);
    }
    let out = cmd.arg("--list-langs").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let installed: Vec<&str> = text.lines().skip(1).map(str::trim).filter(|l| !l.is_empty()).collect();
    let langs: Vec<&str> = wanted.split('+').map(str::trim).filter(|l| installed.contains(l)).collect();
    if !langs.is_empty() {
        Some(langs.join("+"))
    } else if installed.contains(&"eng") {
        Some("eng".into())
    } else {
        None
    }
}

fn tesseract(bin: &str, langs: &str, tessdata: Option<&std::path::Path>, png: &[u8]) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(bin);
    cmd.args(["stdin", "stdout"]);
    if let Some(d) = tessdata {
        cmd.arg("--tessdata-dir").arg(d);
    }
    cmd.args(["-l", langs, "--psm", "3", "--dpi", "300"])
        .env("OMP_THREAD_LIMIT", "1") // stay light: one core
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000 | 0x0000_4000); // CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let data = png.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.read_to_end(&mut s);
        s
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("tesseract timed out".into());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let _ = writer.join();
    let out = reader.join().map_err(|_| "reader panicked")?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// ---------------------------------------------------------------- normalize --

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF |
        0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FFFF)
}

/// Trim lines, drop empty ones and remove the spaces OCR engines put between
/// CJK characters ("剪 贴 板" -> "剪贴板").
fn normalize(raw: &str) -> String {
    let mut out = String::new();
    for line in raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let chars: Vec<char> = line.chars().collect();
        let mut s = String::with_capacity(line.len());
        for (i, &c) in chars.iter().enumerate() {
            if c.is_whitespace() {
                let prev = s.chars().last();
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if prev.is_some_and(is_cjk) || next.is_some_and(|n| is_cjk(*n)) || prev.is_some_and(char::is_whitespace) {
                    continue;
                }
                s.push(' ');
            } else {
                s.push(c);
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&s);
    }
    out
}

// ------------------------------------------------------------- apple vision --

#[cfg(target_os = "macos")]
mod vision {
    use crate::imgutil;
    use objc2::rc::autoreleasepool;
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
    use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel};

    /// `rgba`: one (already flattened) tile.
    pub fn recognize(rgba: &[u8], w: u32, h: u32, langs: &[String]) -> Result<String, String> {
        let png = imgutil::encode_png(w, h, rgba).ok_or("cannot encode tile")?;
        autoreleasepool(|_| {
            let data = NSData::with_bytes(&png);
            let handler =
                VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &NSDictionary::new());
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(true);
            // Only the configured languages: adding ja/ko makes Vision read
            // Chinese characters as Japanese kanji.
            let langs: Vec<_> = langs.iter().map(|l| NSString::from_str(l)).collect();
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&langs));
            // Default is 1/32 of the image height, which drops normal-size
            // text in tall screenshots; accept text down to ~8 px.
            request.setMinimumTextHeight((8.0 / h.max(1) as f32).min(1.0 / 32.0));
            let as_request: &VNRequest = &request;
            let requests = NSArray::from_slice(&[as_request]);
            handler.performRequests_error(&requests).map_err(|e| e.to_string())?;
            let mut lines = Vec::new();
            if let Some(results) = request.results() {
                for obs in results.iter() {
                    let candidates = obs.topCandidates(1);
                    if let Some(best) = candidates.firstObject() {
                        lines.push(best.string().to_string());
                    }
                }
            }
            Ok(lines.join("\n"))
        })
    }
}

// ---------------------------------------------------------- windows winrt --

#[cfg(windows)]
mod winrt {
    use crate::imgutil;
    use windows::core::HSTRING;
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::BitmapDecoder;
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

    fn init() {
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
        }
    }

    /// Prefer a Chinese engine when its OCR language pack is installed (it
    /// also reads Latin text); otherwise the user's profile languages.
    fn engine() -> windows::core::Result<OcrEngine> {
        for tag in ["zh-Hans-CN", "zh-Hans", "zh-Hant-TW", "zh-Hant"] {
            if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(tag)) {
                if OcrEngine::IsLanguageSupported(&lang).unwrap_or(false) {
                    if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                        return Ok(e);
                    }
                }
            }
        }
        OcrEngine::TryCreateFromUserProfileLanguages()
    }

    pub fn available() -> bool {
        init();
        engine().is_ok()
    }

    pub fn language() -> String {
        init();
        engine()
            .and_then(|e| e.RecognizerLanguage())
            .and_then(|l| l.LanguageTag())
            .map(|t| t.to_string())
            .unwrap_or_else(|_| "?".into())
    }

    pub fn recognize(rgba: &[u8], w: u32, h: u32) -> Result<String, String> {
        init();
        inner(rgba, w, h).map_err(|e| e.to_string())
    }

    fn inner(rgba: &[u8], w: u32, h: u32) -> windows::core::Result<String> {
        // Small text: upscale 2x (the engine is weak below ~20 px); never
        // exceed MaxImageDimension.
        let max = OcrEngine::MaxImageDimension().unwrap_or(10_000);
        let png = if w.max(h) * 2 <= max.min(3000) {
            let mut big = vec![0u8; (w * h * 16) as usize];
            for y in 0..h * 2 {
                for x in 0..w * 2 {
                    let s = (((y / 2) * w + x / 2) * 4) as usize;
                    let d = ((y * w * 2 + x) * 4) as usize;
                    big[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
                }
            }
            imgutil::encode_png(w * 2, h * 2, &big)
        } else if w.max(h) > max {
            let (tw, th, t) = imgutil::thumbnail(w, h, rgba, max);
            imgutil::encode_png(tw, th, &t)
        } else {
            imgutil::encode_png(w, h, rgba)
        }
        .unwrap_or_default();
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(&png)?;
        writer.StoreAsync()?.join()?;
        writer.FlushAsync()?.join()?;
        writer.DetachStream()?;
        stream.Seek(0)?;
        let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
        let bitmap = decoder.GetSoftwareBitmapAsync()?.join()?;
        let result = engine()?.RecognizeAsync(&bitmap)?.join()?;
        let mut lines = Vec::new();
        for line in result.Lines()? {
            lines.push(line.Text()?.to_string());
        }
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_and_merge() {
        assert_eq!(tile_rows(1000, 1500), vec![(0, 1500)]);
        let t = tile_rows(1080, 6000);
        assert!(t.len() >= 3);
        assert_eq!(t[0].0, 0);
        assert_eq!(t.last().unwrap().1, 6000);
        assert!(t.windows(2).all(|p| p[1].0 < p[0].1)); // overlapping
        let parts = ["第一行\n第二行\n第 三 行".to_string(), "第三行\n第四行".to_string(), "第四行\n第五行".to_string()];
        assert_eq!(merge_tiles(&parts), "第一行\n第二行\n第 三 行\n第四行\n第五行");
        assert_eq!(vision_langs("chi_sim+chi_tra+eng"), ["zh-Hans", "zh-Hant", "en-US"]);
        assert_eq!(vision_langs("xyz"), ["zh-Hans", "zh-Hant", "en-US"]);
    }

    #[test]
    fn tesseract_input_upscales_and_inverts() {
        // dark 2x2 image -> inverted, 4x4 gray PNG
        let rgba = [20u8, 20, 20, 255].repeat(4);
        let png = tesseract_input(2, 2, &rgba).unwrap();
        let (w, h, px) = imgutil::decode_png(&png).unwrap();
        assert_eq!((w, h), (2, 2)); // no text found: no upscale
        assert!(px[0] > 200);
        // a 10 px "text line" on white -> upscaled 3x; a 40 px one -> kept
        let page = |line: u32| -> Vec<u8> {
            (0..100u32).flat_map(|y| (0..100u32).map(move |x| if (20..20 + line).contains(&y) && x % 3 == 0 { 0 } else { 255 })).collect()
        };
        assert_eq!(text_height(100, 100, &page(10)), Some(10));
        assert_eq!(text_height(100, 100, &page(40)), Some(40));
    }

    /// Dev aid: `COPYM_OCR_DUMP=dir cargo test dump_tesseract_input -- --ignored`
    /// writes the preprocessed image of every `bench-*.png` in `dir`.
    #[test]
    #[ignore]
    fn dump_tesseract_input() {
        let dir = std::path::PathBuf::from(std::env::var("COPYM_OCR_DUMP").unwrap());
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("bench-") || name.contains("-prep") || name.contains("-rust") {
                continue;
            }
            let (w, h, mut rgba) = imgutil::decode_png(&std::fs::read(e.path()).unwrap()).unwrap();
            imgutil::flatten_on_white(&mut rgba);
            let out = tesseract_input(w, h, &rgba).unwrap();
            std::fs::write(dir.join(name.replace(".png", "-rust.png")), out).unwrap();
        }
    }

    #[test]
    fn normalize_cjk() {
        assert_eq!(super::normalize("  剪 贴 板  历史 \n\n hello   world 中 文 "), "剪贴板历史\nhello world中文");
    }
}
