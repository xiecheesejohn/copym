// No console window on Windows (CLI commands re-attach to the parent console).
#![cfg_attr(windows, windows_subsystem = "windows")]

use copym_core::{imgutil, model, store};

mod autostart;
mod clip;
mod config;
mod daemon;
mod filter;
mod groups;
mod gui;
mod ipc;
#[cfg(target_os = "linux")]
mod kde_hotkey;
mod ocr;
mod paste;
#[cfg(target_os = "linux")]
mod ppocr;

use model::{Filter, Kind, Request, Response};

const HELP: &str = "copym - fast, modern clipboard manager

USAGE:
    copym [COMMAND]

COMMANDS:
    show            Toggle the clipboard popup (default; starts the daemon if needed)
    daemon          Run the background daemon in the foreground
    list [N]        Print the latest N items (default 20)
    select ID       Put item ID back on the clipboard
    delete ID       Delete item ID (also on synced devices)
    pin ID          Pin / unpin item ID
    search [-r] Q   Search text and text inside images (-r: regular expression)
    pause | resume  Stop / restart recording
    clear           Remove all non-pinned items
    status          Show daemon status
    quit            Stop copym (daemon and popup)
    sync            Show LAN sync status and connected devices
    sync pair       Show a one-time code to add another device (valid 3 min)
    sync join CODE[@IP:PORT]  Join the devices of CODE (address optional)
    sync leave      Stop syncing and forget the group key
    groups          Open the group manager (groups.toml)
    ocr FILE.png    Recognize the text in an image (shows the engine used)
    ocr models      Download high-accuracy Tesseract models (Linux; Tesseract engine)
    ocr redo        Recognize all stored images again
    autostart on|off  Start the daemon at login
    config          Print the config file path
    help            Show this help
";

/// Process start, for `COPYM_TRACE` timings.
pub static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() {
    START.get_or_init(std::time::Instant::now);
    // Older macOS versions pass "-psn_…" when an .app is opened from Finder.
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("-psn_")).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("show");

    #[cfg(windows)]
    if !matches!(cmd, "daemon" | "gui" | "groups" | "show") {
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
        }
    }

    let result = match cmd {
        "daemon" => daemon::run(),
        "gui" => gui::run(args.get(1).map(String::as_str).unwrap_or("")),
        "popup-host" => gui::host(),
        "groups" => gui::run("groups"),
        "ocr" => ocr_cmd(&args[1..]),
        "show" | "toggle" => show(),
        "list" => list(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(20)),
        "search" => {
            let regex = args.get(1).is_some_and(|a| a == "-r" || a == "--regex");
            let q = args[if regex { 2 } else { 1 }.min(args.len())..].join(" ");
            print_items(Request::List { query: q, filter: Filter::All, regex, limit: 50 })
        }
        "delete" | "rm" => match args.get(1).and_then(|n| n.parse().ok()) {
            Some(id) => simple(Request::Delete { id }),
            None => Err("usage: copym delete ID".into()),
        },
        "pin" | "unpin" => match args.get(1).and_then(|n| n.parse().ok()) {
            Some(id) => simple(Request::TogglePin { id }),
            None => Err("usage: copym pin ID".into()),
        },
        "select" => match args.get(1).and_then(|n| n.parse().ok()) {
            Some(id) => simple(Request::Activate { id }),
            None => Err("usage: copym select ID".into()),
        },
        "pause" => simple(Request::SetPaused(Some(true))),
        "resume" => simple(Request::SetPaused(Some(false))),
        "clear" => simple(Request::Clear),
        "status" => simple(Request::Status),
        "quit" | "stop" => simple(Request::Quit),
        "sync" => sync_cmd(&args[1..]),
        "autostart" => match args.get(1).map(String::as_str) {
            Some("on") => autostart::set(true).map(|p| println!("autostart enabled: {p}")),
            Some("off") => autostart::set(false).map(|p| println!("autostart disabled: {p}")),
            _ => Err("usage: copym autostart on|off".into()),
        },
        "config" => {
            config::Config::load(); // writes the default file if missing
            println!("{}", config::config_path().display());
            Ok(())
        }
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("copym {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!("unknown command {other:?}\n\n{HELP}")),
    };
    if let Err(e) = result {
        eprintln!("copym: {e}");
        std::process::exit(1);
    }
}

fn ocr_cmd(args: &[String]) -> Result<(), String> {
    let cfg = config::Config::load();
    match args.first().map(String::as_str) {
        Some("models") => {
            let langs = args.get(1).cloned().unwrap_or_else(|| cfg.ocr_languages.clone());
            println!("下载 Tesseract 高精度模型（tessdata_best）：{langs}");
            let dir = ocr::download_models(&langs)?;
            println!("完成：{}\n重启守护进程后生效（copym quit && copym），然后运行 copym ocr redo 重新识别已有图片。", dir.display());
            Ok(())
        }
        Some("redo") => match client()?.request(&Request::RedoOcr).map_err(|e| e.to_string())? {
            Response::Counts(c) => {
                println!("正在后台重新识别 {} 张图片", c.first().copied().flatten().unwrap_or(0));
                Ok(())
            }
            Response::Err(e) => Err(e),
            _ => Err("unexpected response".into()),
        },
        Some(path) => {
            let o = ocr::Ocr::detect(&cfg).ok_or("没有可用的 OCR 引擎（ocr_enabled / ocr_engine 配置）")?;
            let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let t = std::time::Instant::now();
            let text = o.recognize(&bytes)?;
            eprintln!("[{} · {:.1}s]", o.name(), t.elapsed().as_secs_f32());
            println!("{text}");
            Ok(())
        }
        None => Err("用法：copym ocr FILE.png | copym ocr models [chi_sim+eng] | copym ocr redo".into()),
    }
}

fn client() -> Result<ipc::Client, String> {
    ipc::Client::connect().map_err(|e| format!("daemon not running ({e}); start it with `copym daemon`"))
}

fn show() -> Result<(), String> {
    daemon::ensure_running()?;
    let token = std::env::var("XDG_ACTIVATION_TOKEN").ok();
    match client()?.request(&Request::ToggleGui { activation_token: token }).map_err(|e| e.to_string())? {
        Response::Err(e) => Err(e),
        _ => Ok(()),
    }
}

fn simple(req: Request) -> Result<(), String> {
    match client()?.request(&req).map_err(|e| e.to_string())? {
        Response::Err(e) => Err(e),
        Response::Status { paused, count, backend, ocr } => {
            println!(
                "recording: {}\nitems:     {count}\nbackend:   {backend}\nocr:       {}",
                if paused { "paused" } else { "on" },
                ocr.as_deref().unwrap_or("unavailable")
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

fn list(n: u32) -> Result<(), String> {
    print_items(Request::List { query: String::new(), filter: Filter::All, regex: false, limit: n })
}

fn print_items(req: Request) -> Result<(), String> {
    let items = match client()?.request(&req).map_err(|e| e.to_string())? {
        Response::Items(items) => items,
        Response::Err(e) => return Err(e),
        _ => return Err("unexpected response".into()),
    };
    for it in items {
        let pin = if it.pinned { "*" } else { " " };
        match it.kind {
            Kind::Text => {
                let line: String = it.preview.chars().take(100).collect::<String>().replace(['\n', '\r', '\t'], " ");
                println!("{pin}{:>6}  {line}", it.id);
            }
            Kind::Image => {
                let text: String = it.ocr.as_deref().unwrap_or("").chars().take(80).collect::<String>().replace('\n', " ");
                println!("{pin}{:>6}  [image {}x{}] {text}", it.id, it.width, it.height)
            }
        }
    }
    Ok(())
}

fn sync_cmd(args: &[String]) -> Result<(), String> {
    let req = match args.first().map(String::as_str) {
        None | Some("status") => Request::SyncStatus,
        Some("pair") => Request::SyncPair,
        Some("join") => {
            let arg = args.get(1).ok_or("usage: copym sync join CODE[@IP:PORT]")?;
            let (code, addr) = match arg.split_once('@') {
                Some((c, a)) => (c.to_string(), Some(a.to_string())),
                None => (arg.clone(), None),
            };
            println!("正在查找并连接正在配对的设备…");
            Request::SyncJoin { code, addr }
        }
        Some("leave") => Request::SyncLeave,
        Some(other) => return Err(format!("unknown sync command {other:?} (status | pair | join | leave)")),
    };
    match client()?.request(&req).map_err(|e| e.to_string())? {
        Response::Err(e) => Err(e),
        Response::Sync(s) => {
            if !s.enabled {
                println!("同步：已关闭（config.toml 中 sync_enabled = false）");
                return Ok(());
            }
            println!("本机：    {}  (端口 {}, id {})", s.device_name, s.port, &s.device_id[..8.min(s.device_id.len())]);
            println!("状态：    {}", if s.paired { "已加入同步组" } else { "未配对 - 运行 `copym sync pair` 添加设备" });
            if let Some(c) = s.pairing_code {
                println!("配对中：  {c}");
            }
            if s.peers.is_empty() {
                println!("已连接：  无");
            } else {
                println!("已连接：");
                for (name, addr) in s.peers {
                    println!("  - {name}  ({addr})");
                }
            }
            Ok(())
        }
        Response::Pairing { code, addrs, expires_secs } => {
            println!("在另一台设备上运行：\n\n    copym sync join {code}\n");
            if let Some(a) = addrs.first() {
                println!("如果找不到设备（局域网屏蔽了 mDNS），改用：\n\n    copym sync join {code}@{a}\n");
            }
            println!("配对码 {} 分钟内有效，只能尝试一次。", expires_secs / 60);
            Ok(())
        }
        Response::Joined { name } => {
            println!("已与 {name} 配对，开始同步。");
            Ok(())
        }
        _ => {
            println!("已退出同步组。");
            Ok(())
        }
    }
}
