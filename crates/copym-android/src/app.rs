//! Platform-neutral logic of the Android app: history store + sync engine.
//! The JNI layer (`lib.rs`) is a thin wrapper around this so it can be unit
//! tested on the host.

use copym_core::imgutil;
use copym_core::groups::{self, Groups};
use copym_core::kinds::{self, classify, TextKind};
use copym_core::model::{now_secs, Content, Filter, GroupRule, ItemMeta, Kind};
use copym_core::store::{GroupMatcher, Query, Store};
use copym_sync::{Engine, ItemKind, SyncItem};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub const MAX_ITEMS: usize = 100_000;
pub const MAX_TEXT_BYTES: usize = 2 << 20;
pub const MAX_IMAGE_BYTES: usize = 8 << 20;
const SYNC_HISTORY_ITEMS: usize = 200;
pub const SYNC_PORT: u16 = 47800;

pub type R<T> = Result<T, String>;

pub struct App {
    store: Mutex<Store>,
    groups: Mutex<Groups>,
    groups_path: PathBuf,
    sync: OnceLock<Engine>,
    /// Bumped on every change so the UI can cheaply poll for updates.
    version: AtomicU64,
}

impl App {
    /// `peers`: extra addresses to dial (for networks that block mDNS).
    pub fn open(data_dir: &str, device_name: &str, port: u16, peers: &str, mdns: bool) -> R<Arc<App>> {
        let static_peers = peers
            .split([',', ' ', '\n'])
            .filter(|p| !p.trim().is_empty())
            .filter_map(|p| {
                let p = p.trim();
                p.parse().ok().or_else(|| format!("{p}:{SYNC_PORT}").parse().ok())
            })
            .collect();
        let dir = PathBuf::from(data_dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let store = Store::open(&dir.join("history.sqlite"), MAX_ITEMS)?;
        let groups_path = dir.join("groups.toml");
        let groups = std::fs::read_to_string(&groups_path).ok().and_then(|s| groups::from_toml(&s).ok()).unwrap_or_default();
        let app = Arc::new(App {
            store: Mutex::new(store),
            groups: Mutex::new(groups),
            groups_path,
            sync: OnceLock::new(),
            version: AtomicU64::new(1),
        });
        let cfg = copym_sync::Config {
            identity_path: dir.join("sync-identity.bin"),
            device_name: device_name.to_string(),
            port,
            static_peers,
            mdns,
        };
        let host = Arc::new(SyncHost(Arc::downgrade(&app)));
        match Engine::start(cfg, host) {
            Ok(engine) => {
                let _ = app.sync.set(engine);
            }
            Err(e) => eprintln!("copym: sync unavailable: {e}"),
        }
        Ok(app)
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn list(&self, query: &str, regex: bool, filter: i32, limit: usize) -> R<String> {
        let q = Query::parse(query, regex)?;
        let filter = match filter {
            1 => Filter::Text,
            2 => Filter::Image,
            3 => Filter::Pinned,
            _ => Filter::All,
        };
        let items: Vec<Value> = self.store.lock().unwrap().list(&q, filter, limit).iter().map(meta_json).collect();
        Ok(Value::Array(items).to_string())
    }

    // ------------------------------------------------------------ groups --

    /// `{"default": name, "group": [{name, kind, pinned, patterns, apps, rest}, ...]}`
    pub fn groups_json(&self) -> String {
        serde_json::to_string(&*self.groups.lock().unwrap()).unwrap_or_default()
    }

    /// Replace all groups (validated) and persist them as groups.toml.
    pub fn set_groups(&self, json: &str) -> R<()> {
        let g: Groups = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if g.list.is_empty() {
            return Err(copym_core::t!("至少需要一个分组", "at least one group is needed").into());
        }
        g.validate()?;
        let tmp = self.groups_path.with_extension("toml.tmp");
        std::fs::write(&tmp, groups::to_toml(&g)?).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.groups_path).map_err(|e| e.to_string())?;
        *self.groups.lock().unwrap() = g;
        self.bump();
        Ok(())
    }

    /// Items of group `index` (out of range = everything), with a `type` field.
    pub fn list_group(&self, query: &str, regex: bool, index: i32, limit: usize) -> R<String> {
        let q = Query::parse(query, regex)?;
        let rule = usize::try_from(index).ok().map(|i| self.groups.lock().unwrap().rule(i)).filter(|r| *r != GroupRule::default());
        let m = rule.as_ref().map(GroupMatcher::new).transpose()?;
        let items: Vec<Value> = self.store.lock().unwrap().list_group(&q, m.as_ref(), None, limit).items.iter().map(meta_json).collect();
        Ok(Value::Array(items).to_string())
    }

    /// Item count per group for the current search: `[n | null, ...]` (null = invalid rule).
    pub fn group_counts(&self, query: &str, regex: bool) -> R<String> {
        let q = Query::parse(query, regex)?;
        let rules = self.groups.lock().unwrap().rules();
        Ok(json!(self.count_rules(&q, &rules)).to_string())
    }

    /// Count for group `index` of an unsaved draft (the group editor).
    pub fn draft_count(&self, groups_json: &str, index: usize) -> R<Option<u32>> {
        let g: Groups = serde_json::from_str(groups_json).map_err(|e| e.to_string())?;
        Ok(self.count_rules(&Query::All, &[g.rule(index)])[0])
    }

    fn count_rules(&self, q: &Query, rules: &[GroupRule]) -> Vec<Option<u32>> {
        let ms: Vec<_> = rules.iter().map(|r| GroupMatcher::new(r).ok()).collect();
        self.store.lock().unwrap().group_counts(q, &ms).counts
    }

    /// Full content prepared for the preview:
    /// `{"type", "text"?, "pretty"?, "color"?: {"argb", "values": [[k, v]...]}}`.
    pub fn detail(&self, id: u64) -> R<String> {
        let Some(content) = self.store.lock().unwrap().get(id)? else { return Err(copym_core::t!("条目不存在", "no such item").into()) };
        Ok(match content {
            Content::Image { width, height, .. } => json!({ "type": "image", "w": width, "h": height }),
            Content::Text(t) => {
                let kind = classify(&t, true);
                let mut v = json!({ "type": kind.as_str(), "label": kind.label(), "text": t });
                if kind == TextKind::Json {
                    v["pretty"] = json!(kinds::pretty_json(&t));
                }
                if let Some(c) = kinds::parse_color(&t).filter(|_| kind == TextKind::Color) {
                    let argb = (u32::from(c[3]) << 24) | (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
                    v["color"] = json!({ "argb": argb, "values": kinds::color_notations(c).map(|(k, s)| json!([k, s])) });
                }
                v
            }
        }
        .to_string())
    }

    pub fn text(&self, id: u64) -> R<Option<String>> {
        Ok(match self.store.lock().unwrap().get(id)? {
            Some(Content::Text(t)) => Some(t),
            _ => None,
        })
    }

    pub fn image(&self, id: u64) -> R<Option<Vec<u8>>> {
        Ok(match self.store.lock().unwrap().get(id)? {
            Some(Content::Image { png, .. }) => Some(png),
            _ => None,
        })
    }

    pub fn thumb(&self, id: u64) -> R<Option<Vec<u8>>> {
        self.store.lock().unwrap().thumb(id)
    }

    /// Record a local copy. Returns false when it was ignored (empty / too big).
    pub fn add_text(&self, text: String, source: Option<String>) -> R<bool> {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return Ok(false);
        }
        self.add(Content::Text(text), source)
    }

    pub fn add_image(&self, png: Vec<u8>, source: Option<String>) -> R<bool> {
        if png.len() > MAX_IMAGE_BYTES {
            return Ok(false);
        }
        let Some((width, height)) = imgutil::png_size(&png) else { return Err("not a PNG image".into()) };
        self.add(Content::Image { png, width, height }, source)
    }

    fn add(&self, content: Content, source: Option<String>) -> R<bool> {
        let item = to_sync_item(&content, now_secs());
        self.store.lock().unwrap().insert(content, source)?;
        self.bump();
        if let (Some(sync), Some(item)) = (self.sync.get(), item) {
            sync.broadcast_item(item);
        }
        Ok(true)
    }

    /// Move an item to the top (after the user copied it).
    pub fn touch(&self, id: u64) -> R<()> {
        let mut store = self.store.lock().unwrap();
        let Some(content) = store.get(id)? else { return Err("item not found".into()) };
        let source = store.source(id);
        store.insert(content, source)?;
        drop(store);
        self.bump();
        Ok(())
    }

    pub fn delete(&self, id: u64) -> R<()> {
        let tombs = self.store.lock().unwrap().delete(id)?;
        self.bump();
        if let Some(s) = self.sync.get() {
            s.broadcast_delete(tombs);
        }
        Ok(())
    }

    pub fn toggle_pin(&self, id: u64) -> R<()> {
        self.store.lock().unwrap().toggle_pin(id)?;
        self.bump();
        Ok(())
    }

    pub fn clear(&self) -> R<()> {
        let tombs = self.store.lock().unwrap().clear()?;
        self.bump();
        if let Some(s) = self.sync.get() {
            s.broadcast_delete(tombs);
        }
        Ok(())
    }

    fn engine(&self) -> R<&Engine> {
        self.sync.get().ok_or_else(|| copym_core::t!("同步不可用", "sync is unavailable").to_string())
    }

    pub fn sync_status(&self) -> String {
        let Some(s) = self.sync.get() else { return json!({ "enabled": false }).to_string() };
        let st = s.status();
        json!({
            "enabled": true,
            "paired": st.paired,
            "name": st.device_name,
            "id": st.device_id,
            "port": st.port,
            "peers": st.peers.iter().map(|p| json!({ "name": p.name, "addr": p.addr.to_string() })).collect::<Vec<_>>(),
            "code": st.pairing_code,
        })
        .to_string()
    }

    pub fn pair_start(&self) -> R<String> {
        let p = self.engine()?.pair_start().map_err(|e| e.to_string())?;
        Ok(json!({
            "code": p.code,
            "addrs": p.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "port": p.port,
            "expires": p.expires_in.as_secs(),
        })
        .to_string())
    }

    /// `code` may carry an address: `1234-5678@192.168.1.5:47800`.
    pub fn join(&self, code: &str) -> R<String> {
        let (code, addr) = match code.trim().split_once('@') {
            Some((c, a)) => (c.trim(), Some(a.trim().parse().map_err(|_| copym_core::tf!("地址无效：{a}", "invalid address: {a}"))?)),
            None => (code.trim(), None),
        };
        let name = self.engine()?.join(code, addr).map_err(|e| e.to_string())?;
        self.bump();
        Ok(name)
    }

    pub fn leave(&self) -> R<()> {
        self.engine()?.leave().map_err(|e| e.to_string())?;
        self.bump();
        Ok(())
    }
}

fn meta_json(m: &ItemMeta) -> Value {
    let kind = if m.kind == Kind::Image { TextKind::Image } else { classify(&m.preview, false) };
    let color = (kind == TextKind::Color)
        .then(|| kinds::parse_color(&m.preview))
        .flatten()
        .map(|c| (u32::from(c[3]) << 24) | (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]));
    json!({
        "type": kind.as_str(),
        "label": kind.label(),
        "color": color,
        "id": m.id,
        "kind": if m.kind == Kind::Image { "image" } else { "text" },
        "preview": m.preview,
        "chars": m.chars,
        "lines": m.lines,
        "bytes": m.bytes,
        "w": m.width,
        "h": m.height,
        "created": m.created,
        "pinned": m.pinned,
        "source": m.source,
        "ocr": m.ocr,
    })
}

fn to_sync_item(content: &Content, created: i64) -> Option<SyncItem> {
    Some(match content {
        Content::Text(t) => SyncItem { hash: content.hash(), created, kind: ItemKind::Text, data: t.as_bytes().to_vec(), width: 0, height: 0 },
        Content::Image { png, width, height } => {
            if png.len() > MAX_IMAGE_BYTES {
                return None;
            }
            SyncItem { hash: content.hash(), created, kind: ItemKind::Image, data: png.clone(), width: *width, height: *height }
        }
    })
}

/// Bridges the sync engine to the store. Holds a weak reference so the
/// engine's threads never keep the app alive on their own.
struct SyncHost(std::sync::Weak<App>);

impl SyncHost {
    fn with<T: Default>(&self, f: impl FnOnce(&App) -> T) -> T {
        self.0.upgrade().map(|a| f(&a)).unwrap_or_default()
    }
}

impl copym_sync::Host for SyncHost {
    fn inventory(&self) -> Vec<(u64, i64)> {
        self.with(|a| a.store.lock().unwrap().sync_inventory(SYNC_HISTORY_ITEMS, true, MAX_IMAGE_BYTES as u64))
    }

    fn tombstones(&self) -> Vec<(u64, i64)> {
        self.with(|a| a.store.lock().unwrap().tombstones())
    }

    fn missing(&self, items: &[(u64, i64)]) -> Vec<u64> {
        self.with(|a| a.store.lock().unwrap().sync_missing(items))
    }

    fn fetch(&self, hash: u64) -> Option<SyncItem> {
        self.0.upgrade().and_then(|a| {
            let (content, created) = a.store.lock().unwrap().get_by_hash(hash).ok()??;
            to_sync_item(&content, created)
        })
    }

    fn receive(&self, item: SyncItem, from: &str) {
        let content = match item.kind {
            ItemKind::Text => match String::from_utf8(item.data) {
                Ok(t) if t.len() <= MAX_TEXT_BYTES => Content::Text(t),
                _ => return,
            },
            ItemKind::Image => {
                if item.data.len() > MAX_IMAGE_BYTES || imgutil::png_size(&item.data) != Some((item.width, item.height)) {
                    return;
                }
                Content::Image { png: item.data, width: item.width, height: item.height }
            }
        };
        self.with(|a| match a.store.lock().unwrap().insert_synced(content, Some(from.to_string()), item.created) {
            Ok(Some(_)) => a.bump(),
            Ok(None) => {}
            Err(e) => eprintln!("copym: store error: {e}"),
        })
    }

    fn receive_delete(&self, deleted: &[(u64, i64)]) {
        if deleted.is_empty() {
            return;
        }
        self.with(|a| match a.store.lock().unwrap().delete_synced(deleted) {
            Ok(n) if n > 0 => a.bump(),
            Ok(_) => {}
            Err(e) => eprintln!("copym: store error: {e}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("copym-android-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn local_history_roundtrip() {
        let app = App::open(&tmp("local"), "test-phone", 0, "", false).unwrap();
        let v0 = app.version();
        assert!(app.add_text("hello android".into(), None).unwrap());
        assert!(!app.add_text("   ".into(), None).unwrap());
        assert!(app.version() > v0);
        let list: Value = serde_json::from_str(&app.list("ANDROID", false, 0, 50).unwrap()).unwrap();
        let id = list[0]["id"].as_u64().unwrap();
        assert_eq!(list[0]["kind"], "text");
        assert_eq!(app.text(id).unwrap().as_deref(), Some("hello android"));
        app.toggle_pin(id).unwrap();
        let pinned: Value = serde_json::from_str(&app.list("", false, 3, 50).unwrap()).unwrap();
        assert_eq!(pinned.as_array().unwrap().len(), 1);
        assert!(app.list("(", true, 0, 50).is_err());
        app.delete(id).unwrap();
        assert_eq!(app.list("", false, 0, 50).unwrap(), "[]");
        let st: Value = serde_json::from_str(&app.sync_status()).unwrap();
        assert_eq!(st["paired"], false);
    }

    #[test]
    fn groups_and_types() {
        let dir = tmp("groups");
        let app = App::open(&dir, "test-phone", 0, "", false).unwrap();
        app.add_text("https://example.com/x".into(), None).unwrap();
        app.add_text("#10B981".into(), None).unwrap();
        app.add_text("您的验证码是 482913".into(), Some("desk".into())).unwrap();
        let g: Value = serde_json::from_str(&app.groups_json()).unwrap();
        assert_eq!(g["group"][3]["name"], "链接");
        let links: Value = serde_json::from_str(&app.list_group("", false, 3, 50).unwrap()).unwrap();
        assert_eq!(links.as_array().unwrap().len(), 1);
        assert_eq!(links[0]["type"], "url");
        let all: Value = serde_json::from_str(&app.list_group("", false, 0, 50).unwrap()).unwrap();
        assert_eq!(all[1]["type"], "color");
        assert_eq!(all[1]["color"], 0xFF10B981u32);

        let mut g: Value = g;
        g["group"].as_array_mut().unwrap().push(json!({"name": "验证码", "patterns": ["验证码\\D*\\d{4,8}"]}));
        g["default"] = json!("验证码");
        app.set_groups(&g.to_string()).unwrap();
        assert_eq!(app.draft_count(&g.to_string(), 5).unwrap(), Some(1));
        let counts: Value = serde_json::from_str(&app.group_counts("", false).unwrap()).unwrap();
        assert_eq!(counts, json!([3, 3, 0, 1, 0, 1]));
        let mut bad = g.clone();
        bad["group"][5]["patterns"] = json!(["("]);
        assert!(app.set_groups(&bad.to_string()).is_err());
        // persisted
        drop(app);
        let reopened = App::open(&dir, "test-phone", 0, "", false).unwrap();
        let g2: Value = serde_json::from_str(&reopened.groups_json()).unwrap();
        assert_eq!(g2["default"], "验证码");

        let id = all[1]["id"].as_u64().unwrap();
        let d: Value = serde_json::from_str(&reopened.detail(id).unwrap()).unwrap();
        assert_eq!(d["type"], "color");
        assert_eq!(d["color"]["values"][0][1], "#10B981");
    }

    #[test]
    fn two_phones_pair_and_sync() {
        let a = App::open(&tmp("a"), "phone-a", 0, "", false).unwrap();
        let a_port = serde_json::from_str::<Value>(&a.sync_status()).unwrap()["port"].as_u64().unwrap();
        let b = App::open(&tmp("b"), "phone-b", 0, &format!("127.0.0.1:{a_port}"), false).unwrap();
        a.add_text("before pairing".into(), None).unwrap();
        let pair: Value = serde_json::from_str(&a.pair_start().unwrap()).unwrap();
        let code = format!("{}@127.0.0.1:{}", pair["code"].as_str().unwrap(), pair["port"]);
        assert_eq!(b.join(&code).unwrap(), "phone-a");
        let wait = |app: &App, needle: &str| {
            for _ in 0..100 {
                if app.list(needle, false, 0, 10).unwrap() != "[]" {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            false
        };
        assert!(wait(&b, "before pairing"), "catch-up after pairing");
        b.add_text("from phone b".into(), None).unwrap();
        assert!(wait(&a, "from phone b"), "live push");
        let list: Value = serde_json::from_str(&a.list("from phone b", false, 0, 10).unwrap()).unwrap();
        assert_eq!(list[0]["source"], "phone-b");
    }
}
