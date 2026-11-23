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

pub struct Ocr {
    engine: Engine,
}

enum Engine {
    Tesseract { bin: String, langs: String },
    #[cfg(target_os = "macos")]
    Vision,
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
            return Some(Self { engine: Engine::Vision });
            #[cfg(windows)]
            if winrt::available() {
                return Some(Self { engine: Engine::WinRt });
            }
        }
        if want == "auto" || want == "tesseract" {
            if let Some(bin) = find_tesseract() {
                let langs = tesseract_langs(&bin, &cfg.ocr_languages)?;
                return Some(Self { engine: Engine::Tesseract { bin, langs } });
            }
        }
        None
    }

    pub fn name(&self) -> String {
        match &self.engine {
            Engine::Tesseract { langs, .. } => format!("tesseract ({langs})"),
            #[cfg(target_os = "macos")]
            Engine::Vision => "apple vision".into(),
            #[cfg(windows)]
            Engine::WinRt => "windows.media.ocr".into(),
        }
    }

    /// Recognize text in a PNG. Returns normalized text (may be empty).
    pub fn recognize(&self, png: &[u8]) -> Result<String, String> {
        let (w, h) = imgutil::png_size(png).ok_or("not a png")?;
        if (w as u64) * (h as u64) > 50_000_000 {
            return Err("image too large for OCR".into());
        }
        let raw = match &self.engine {
            Engine::Tesseract { bin, langs } => tesseract(bin, langs, png)?,
            #[cfg(target_os = "macos")]
            Engine::Vision => vision::recognize(png)?,
            #[cfg(windows)]
            Engine::WinRt => winrt::recognize(png)?,
        };
        Ok(normalize(&raw))
    }
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
fn tesseract_langs(bin: &str, wanted: &str) -> Option<String> {
    let out = std::process::Command::new(bin).arg("--list-langs").output().ok()?;
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

fn tesseract(bin: &str, langs: &str, png: &[u8]) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(bin);
    cmd.args(["stdin", "stdout", "-l", langs, "--psm", "3"])
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
    use objc2::rc::autoreleasepool;
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
    use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel};

    pub fn recognize(png: &[u8]) -> Result<String, String> {
        autoreleasepool(|_| {
            let data = NSData::with_bytes(png);
            let handler =
                VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &NSDictionary::new());
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(true);
            let langs = ["zh-Hans", "zh-Hant", "en-US", "ja-JP", "ko-KR"].map(NSString::from_str);
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&langs));
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
    use windows::Graphics::Imaging::BitmapDecoder;
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

    fn init() {
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
        }
    }

    pub fn available() -> bool {
        init();
        OcrEngine::TryCreateFromUserProfileLanguages().is_ok()
    }

    pub fn recognize(png: &[u8]) -> Result<String, String> {
        init();
        inner(png).map_err(|e| e.to_string())
    }

    fn inner(png: &[u8]) -> windows::core::Result<String> {
        // The engine refuses images larger than MaxImageDimension.
        let max = OcrEngine::MaxImageDimension().unwrap_or(10_000);
        let scaled;
        let mut bytes = png;
        if let Some((w, h)) = imgutil::png_size(png) {
            if w > max || h > max {
                if let Some(t) = imgutil::png_thumbnail(png, max) {
                    scaled = t;
                    bytes = &scaled;
                }
            }
        }
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(bytes)?;
        writer.StoreAsync()?.join()?;
        writer.FlushAsync()?.join()?;
        writer.DetachStream()?;
        stream.Seek(0)?;
        let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
        let bitmap = decoder.GetSoftwareBitmapAsync()?.join()?;
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
        let result = engine.RecognizeAsync(&bitmap)?.join()?;
        let mut lines = Vec::new();
        for line in result.Lines()? {
            lines.push(line.Text()?.to_string());
        }
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalize_cjk() {
        assert_eq!(super::normalize("  剪 贴 板  历史 \n\n hello   world 中 文 "), "剪贴板历史\nhello world中文");
    }
}
