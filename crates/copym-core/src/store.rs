//! Persistent history on top of redb.
//!
//! Layout:
//! * `meta`  : id -> postcard(DiskMeta)   (small, all loaded at start)
//! * `blob`  : id -> raw payload (UTF-8 text or PNG)
//! * `thumb` : id -> PNG thumbnail (images only)
//! * `ocr`   : id -> text recognized in the image ("" = processed, nothing found)
//! * `tomb`  : content hash -> deleted_at (so deletions sync across devices)
//!
//! Entries are ordered by (created, id); "move to top" re-inserts under a new
//! id with a fresh timestamp. Items received from other devices keep their
//! original timestamp and slot into place. Texts and OCR texts are kept in memory for instant
//! search; image payloads stay on disk.

use crate::imgutil;
use crate::model::{now_secs, Content, Filter, GroupRule, ItemMeta, Kind};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;

const META: TableDefinition<u64, &[u8]> = TableDefinition::new("meta");
const BLOB: TableDefinition<u64, &[u8]> = TableDefinition::new("blob");
const THUMB: TableDefinition<u64, &[u8]> = TableDefinition::new("thumb");
const OCR: TableDefinition<u64, &str> = TableDefinition::new("ocr");
const TOMB: TableDefinition<u64, i64> = TableDefinition::new("tomb");
/// Deletions older than this are forgotten.
const TOMB_TTL: i64 = 30 * 86_400;

const PREVIEW_CHARS: usize = 600;
const THUMB_SIZE: u32 = 480;
/// OCR text sent to the GUI per item (search always uses the full text).
const OCR_VIEW_CHARS: usize = 4000;

/// On-disk metadata. postcard is positional: never reorder or extend this
/// struct - add a new table instead (like `ocr`).
#[derive(Serialize, Deserialize)]
struct DiskMeta {
    id: u64,
    kind: Kind,
    preview: String,
    chars: u32,
    lines: u32,
    bytes: u64,
    width: u32,
    height: u32,
    created: i64,
    pinned: bool,
    source: Option<String>,
    hash: u64,
}

impl DiskMeta {
    fn from_meta(m: &ItemMeta) -> Self {
        Self {
            id: m.id,
            kind: m.kind,
            preview: m.preview.clone(),
            chars: m.chars,
            lines: m.lines,
            bytes: m.bytes,
            width: m.width,
            height: m.height,
            created: m.created,
            pinned: m.pinned,
            source: m.source.clone(),
            hash: m.hash,
        }
    }

    fn into_meta(self) -> ItemMeta {
        ItemMeta {
            id: self.id,
            kind: self.kind,
            preview: self.preview,
            chars: self.chars,
            lines: self.lines,
            bytes: self.bytes,
            width: self.width,
            height: self.height,
            created: self.created,
            pinned: self.pinned,
            source: self.source,
            hash: self.hash,
            ocr: None,
        }
    }
}

struct Entry {
    meta: ItemMeta,
    text: Option<String>,
}

impl Entry {
    /// Text that search runs against.
    fn haystack(&self) -> &str {
        self.text.as_deref().or(self.meta.ocr.as_deref()).unwrap_or("")
    }
}

/// A compiled search query.
pub enum Query {
    All,
    /// Lowercased terms, all must match.
    Terms(Vec<String>),
    Regex(regex_lite::Regex),
}

impl Query {
    pub fn parse(query: &str, regex: bool) -> Result<Self, String> {
        if query.trim().is_empty() {
            return Ok(Query::All);
        }
        if regex {
            return regex_lite::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .map(Query::Regex)
                .map_err(|e| format!("正则表达式无效：{e}"));
        }
        Ok(Query::Terms(query.split_whitespace().map(|t| t.to_lowercase()).collect()))
    }

    fn matches(&self, en: &Entry) -> bool {
        let hay = en.haystack();
        let src = en.meta.source.as_deref().unwrap_or("");
        match self {
            Query::All => true,
            Query::Terms(terms) => terms.iter().all(|t| contains_ci(hay, t) || contains_ci(src, t)),
            Query::Regex(re) => re.is_match(hay) || re.is_match(src),
        }
    }
}

/// A compiled [`GroupRule`].
pub struct GroupMatcher {
    kind: Option<Kind>,
    pinned: bool,
    patterns: Vec<regex_lite::Regex>,
    apps: Vec<regex_lite::Regex>,
    exclude: Vec<regex_lite::Regex>,
    exclude_apps: Vec<regex_lite::Regex>,
}

impl GroupMatcher {
    pub fn new(rule: &GroupRule) -> Result<Self, String> {
        let compile = |list: &[String]| -> Result<Vec<regex_lite::Regex>, String> {
            list.iter()
                .filter(|p| !p.trim().is_empty())
                .map(|p| regex_lite::Regex::new(p).map_err(|e| format!("分组规则 {p:?} 无效：{e}")))
                .collect()
        };
        Ok(Self {
            kind: rule.kind,
            pinned: rule.pinned,
            patterns: compile(&rule.patterns)?,
            apps: compile(&rule.apps)?,
            exclude: compile(&rule.exclude)?,
            exclude_apps: compile(&rule.exclude_apps)?,
        })
    }

    fn matches(&self, en: &Entry) -> bool {
        if self.kind.is_some_and(|k| k != en.meta.kind) || (self.pinned && !en.meta.pinned) {
            return false;
        }
        let hay = en.haystack();
        let src = en.meta.source.as_deref().unwrap_or("");
        let any = |rs: &[regex_lite::Regex], s: &str| rs.iter().any(|r| r.is_match(s));
        (self.patterns.is_empty() || any(&self.patterns, hay))
            && (self.apps.is_empty() || any(&self.apps, src))
            && !any(&self.exclude, hay)
            && !(!src.is_empty() && any(&self.exclude_apps, src))
    }
}

pub struct Store {
    db: Database,
    /// Sorted by (created, id) ascending (oldest first).
    entries: Vec<Entry>,
    /// content hash -> deleted_at
    tombs: std::collections::HashMap<u64, i64>,
    next_id: u64,
    max_items: usize,
}

type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

impl Store {
    pub fn open(path: &Path, max_items: usize) -> R<Self> {
        let db = Database::create(path).map_err(e)?;
        {
            // make sure tables exist
            let w = db.begin_write().map_err(e)?;
            w.open_table(META).map_err(e)?;
            w.open_table(BLOB).map_err(e)?;
            w.open_table(THUMB).map_err(e)?;
            w.open_table(OCR).map_err(e)?;
            {
                // prune old tombstones
                let mut t = w.open_table(TOMB).map_err(e)?;
                let cutoff = now_secs() - TOMB_TTL;
                t.retain(|_, at| at >= cutoff).map_err(e)?;
            }
            w.commit().map_err(e)?;
        }
        let mut tombs = std::collections::HashMap::new();
        let mut entries = Vec::new();
        {
            let r = db.begin_read().map_err(e)?;
            let meta = r.open_table(META).map_err(e)?;
            let blob = r.open_table(BLOB).map_err(e)?;
            let ocr = r.open_table(OCR).map_err(e)?;
            for row in r.open_table(TOMB).map_err(e)?.iter().map_err(e)? {
                let (k, v) = row.map_err(e)?;
                tombs.insert(k.value(), v.value());
            }
            for row in meta.iter().map_err(e)? {
                let (_k, v) = row.map_err(e)?;
                let Ok(dm) = postcard::from_bytes::<DiskMeta>(v.value()) else { continue };
                let mut m = dm.into_meta();
                let text = if m.kind == Kind::Text {
                    blob.get(m.id).map_err(e)?.map(|b| String::from_utf8_lossy(b.value()).into_owned())
                } else {
                    m.ocr = ocr.get(m.id).map_err(e)?.map(|t| t.value().to_string());
                    None
                };
                entries.push(Entry { meta: m, text });
            }
        }
        entries.sort_by_key(|en| (en.meta.created, en.meta.id));
        let next_id = entries.iter().map(|en| en.meta.id + 1).max().unwrap_or(1);
        Ok(Self { db, entries, tombs, next_id, max_items: max_items.max(1) })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Insert a local copy (or move to top when an identical item exists).
    /// Returns the new id.
    pub fn insert(&mut self, content: Content, source: Option<String>) -> R<u64> {
        let hash = content.hash();
        let mut pinned = false;
        let mut ocr = None;
        let mut removed: Option<u64> = None;
        if let Some(pos) = self.entries.iter().rposition(|en| en.meta.hash == hash) {
            if pos + 1 == self.entries.len() {
                return Ok(self.entries[pos].meta.id); // already the newest
            }
            let old = self.entries.remove(pos);
            pinned = old.meta.pinned;
            ocr = old.meta.ocr;
            removed = Some(old.meta.id);
        }
        let created = now_secs().max(self.entries.last().map(|en| en.meta.created).unwrap_or(0));
        self.insert_inner(content, source, created, pinned, ocr, removed)
    }

    /// Insert an item received from another device. Keeps its timestamp; does
    /// nothing if we already have it or deleted it after it was copied.
    pub fn insert_synced(&mut self, content: Content, source: Option<String>, created: i64) -> R<Option<u64>> {
        let hash = content.hash();
        if self.entries.iter().any(|en| en.meta.hash == hash) || self.tombs.get(&hash).is_some_and(|at| *at >= created) {
            return Ok(None);
        }
        let created = created.min(now_secs()); // don't let a fast clock pin items on top
        self.insert_inner(content, source, created, false, None, None).map(Some)
    }

    fn insert_inner(
        &mut self,
        content: Content,
        source: Option<String>,
        created: i64,
        pinned: bool,
        ocr: Option<String>,
        removed: Option<u64>,
    ) -> R<u64> {
        let hash = content.hash();
        let id = self.next_id;
        self.next_id += 1;

        let mut meta = ItemMeta {
            id,
            kind: content.kind(),
            preview: String::new(),
            chars: 0,
            lines: 0,
            bytes: content.byte_len() as u64,
            width: 0,
            height: 0,
            created,
            pinned,
            source,
            hash,
            ocr,
        };
        let mut thumb = None;
        let (payload, text): (&[u8], Option<String>) = match &content {
            Content::Text(t) => {
                meta.preview = t.chars().take(PREVIEW_CHARS).collect();
                meta.chars = t.chars().count() as u32;
                meta.lines = t.lines().count().max(1) as u32;
                (t.as_bytes(), Some(t.clone()))
            }
            Content::Image { png, width, height } => {
                meta.width = *width;
                meta.height = *height;
                thumb = imgutil::png_thumbnail(png, THUMB_SIZE);
                (png.as_slice(), None)
            }
        };

        // Evict the oldest non-pinned items beyond the limit.
        let mut evict = Vec::new();
        let unpinned = self.entries.iter().filter(|en| !en.meta.pinned).count() + usize::from(!pinned);
        if unpinned > self.max_items {
            let mut extra = unpinned - self.max_items;
            for en in &self.entries {
                if extra == 0 {
                    break;
                }
                if !en.meta.pinned {
                    evict.push(en.meta.id);
                    extra -= 1;
                }
            }
        }

        let w = self.db.begin_write().map_err(e)?;
        {
            let mut tm = w.open_table(META).map_err(e)?;
            let mut tb = w.open_table(BLOB).map_err(e)?;
            let mut tt = w.open_table(THUMB).map_err(e)?;
            let mut to = w.open_table(OCR).map_err(e)?;
            for old in removed.iter().chain(evict.iter()) {
                tm.remove(*old).map_err(e)?;
                tb.remove(*old).map_err(e)?;
                tt.remove(*old).map_err(e)?;
                to.remove(*old).map_err(e)?;
            }
            let mb = postcard::to_stdvec(&DiskMeta::from_meta(&meta)).map_err(e)?;
            tm.insert(id, mb.as_slice()).map_err(e)?;
            tb.insert(id, payload).map_err(e)?;
            if let Some(t) = &thumb {
                tt.insert(id, t.as_slice()).map_err(e)?;
            }
            if let Some(o) = &meta.ocr {
                to.insert(id, o.as_str()).map_err(e)?;
            }
            // A new copy revives content that was deleted earlier.
            w.open_table(TOMB).map_err(e)?.remove(hash).map_err(e)?;
        }
        w.commit().map_err(e)?;
        self.tombs.remove(&hash);

        if !evict.is_empty() {
            self.entries.retain(|en| !evict.contains(&en.meta.id));
        }
        let pos = self.entries.partition_point(|en| (en.meta.created, en.meta.id) < (created, id));
        self.entries.insert(pos, Entry { meta, text });
        Ok(id)
    }

    pub fn list(&self, query: &Query, filter: Filter, limit: usize) -> Vec<ItemMeta> {
        self.list_where(query, limit, |en| match filter {
            Filter::All => true,
            Filter::Text => en.meta.kind == Kind::Text,
            Filter::Image => en.meta.kind == Kind::Image,
            Filter::Pinned => en.meta.pinned,
        })
    }

    /// Items of a group (`None` = all), pinned first, newest first.
    pub fn list_group(&self, query: &Query, group: Option<&GroupMatcher>, limit: usize) -> Vec<ItemMeta> {
        self.list_where(query, limit, |en| group.is_none_or(|g| g.matches(en)))
    }

    /// How many items of each group match the search.
    pub fn group_counts(&self, query: &Query, groups: &[Option<GroupMatcher>]) -> Vec<Option<u32>> {
        let mut counts: Vec<Option<u32>> = groups.iter().map(|g| g.as_ref().map(|_| 0)).collect();
        for en in self.entries.iter().filter(|en| query.matches(en)) {
            for (g, c) in groups.iter().zip(counts.iter_mut()) {
                if let (Some(g), Some(c)) = (g, c) {
                    if g.matches(en) {
                        *c += 1;
                    }
                }
            }
        }
        counts
    }

    fn list_where(&self, query: &Query, limit: usize, keep: impl Fn(&Entry) -> bool) -> Vec<ItemMeta> {
        let mut pinned = Vec::new();
        let mut rest = Vec::new();
        for en in self.entries.iter().rev() {
            if pinned.len() + rest.len() >= limit {
                break;
            }
            if keep(en) && query.matches(en) {
                let mut m = en.meta.clone();
                if let Some(o) = &mut m.ocr {
                    if o.len() > OCR_VIEW_CHARS {
                        *o = o.chars().take(OCR_VIEW_CHARS).collect();
                    }
                }
                if m.pinned { pinned.push(m) } else { rest.push(m) }
            }
        }
        pinned.extend(rest);
        pinned
    }

    pub fn get(&self, id: u64) -> R<Option<Content>> {
        let Some(en) = self.entries.iter().find(|en| en.meta.id == id) else { return Ok(None) };
        if let Some(t) = &en.text {
            return Ok(Some(Content::Text(t.clone())));
        }
        let r = self.db.begin_read().map_err(e)?;
        let tb = r.open_table(BLOB).map_err(e)?;
        Ok(tb.get(id).map_err(e)?.map(|b| Content::Image {
            png: b.value().to_vec(),
            width: en.meta.width,
            height: en.meta.height,
        }))
    }

    pub fn source(&self, id: u64) -> Option<String> {
        self.entries.iter().find(|en| en.meta.id == id).and_then(|en| en.meta.source.clone())
    }

    pub fn thumb(&self, id: u64) -> R<Option<Vec<u8>>> {
        let r = self.db.begin_read().map_err(e)?;
        let tt = r.open_table(THUMB).map_err(e)?;
        Ok(tt.get(id).map_err(e)?.map(|b| b.value().to_vec()))
    }

    /// Images that have not been through OCR yet, newest first.
    pub fn pending_ocr(&self) -> Vec<u64> {
        self.entries.iter().rev().filter(|en| en.meta.kind == Kind::Image && en.meta.ocr.is_none()).map(|en| en.meta.id).collect()
    }

    pub fn needs_ocr(&self, id: u64) -> bool {
        self.entries.iter().any(|en| en.meta.id == id && en.meta.kind == Kind::Image && en.meta.ocr.is_none())
    }

    /// Hash of an item, used to find it again after it was moved to the top.
    pub fn hash_of(&self, id: u64) -> Option<u64> {
        self.entries.iter().find(|en| en.meta.id == id).map(|en| en.meta.hash)
    }

    /// Store OCR text for the image with this content hash (ids change when an
    /// item is moved to the top while OCR is running).
    pub fn set_ocr(&mut self, hash: u64, text: String) -> R<()> {
        let Some(en) = self.entries.iter_mut().rev().find(|en| en.meta.hash == hash && en.meta.kind == Kind::Image) else {
            return Ok(());
        };
        let w = self.db.begin_write().map_err(e)?;
        w.open_table(OCR).map_err(e)?.insert(en.meta.id, text.as_str()).map_err(e)?;
        w.commit().map_err(e)?;
        en.meta.ocr = Some(text);
        Ok(())
    }

    /// Delete one item. Returns the tombstone to propagate: (hash, deleted_at).
    pub fn delete(&mut self, id: u64) -> R<Vec<(u64, i64)>> {
        self.remove_ids(&[id])
    }

    /// Delete all non-pinned items. Returns the tombstones.
    pub fn clear(&mut self) -> R<Vec<(u64, i64)>> {
        let ids: Vec<u64> = self.entries.iter().filter(|en| !en.meta.pinned).map(|en| en.meta.id).collect();
        self.remove_ids(&ids)
    }

    /// Apply deletions made on another device. Items copied again after the
    /// deletion survive. Pinned items are never removed remotely.
    pub fn delete_synced(&mut self, deleted: &[(u64, i64)]) -> R<usize> {
        let ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|en| !en.meta.pinned && deleted.iter().any(|(h, at)| *h == en.meta.hash && en.meta.created <= *at))
            .map(|en| en.meta.id)
            .collect();
        let n = ids.len();
        self.remove_ids(&ids)?;
        // Remember the deletions too, so we don't fetch them back from a third device.
        let w = self.db.begin_write().map_err(e)?;
        {
            let mut t = w.open_table(TOMB).map_err(e)?;
            for (h, at) in deleted {
                if self.tombs.get(h).is_none_or(|old| old < at) {
                    t.insert(*h, *at).map_err(e)?;
                    self.tombs.insert(*h, *at);
                }
            }
        }
        w.commit().map_err(e)?;
        Ok(n)
    }

    pub fn tombstones(&self) -> Vec<(u64, i64)> {
        self.tombs.iter().map(|(h, at)| (*h, *at)).collect()
    }

    /// Recent items offered to other devices: (hash, created), newest first.
    pub fn sync_inventory(&self, limit: usize, images: bool, max_image_bytes: u64) -> Vec<(u64, i64)> {
        self.entries
            .iter()
            .rev()
            .filter(|en| match en.meta.kind {
                Kind::Text => true,
                Kind::Image => images && en.meta.bytes <= max_image_bytes,
            })
            .take(limit)
            .map(|en| (en.meta.hash, en.meta.created))
            .collect()
    }

    /// Which of a peer's items we don't have and haven't deleted since.
    pub fn sync_missing(&self, items: &[(u64, i64)]) -> Vec<u64> {
        items
            .iter()
            .filter(|(h, created)| {
                !self.entries.iter().any(|en| en.meta.hash == *h) && self.tombs.get(h).is_none_or(|at| at < created)
            })
            .map(|(h, _)| *h)
            .collect()
    }

    /// Full content of the item with this hash, plus its timestamp.
    pub fn get_by_hash(&self, hash: u64) -> R<Option<(Content, i64)>> {
        let Some(en) = self.entries.iter().rev().find(|en| en.meta.hash == hash) else { return Ok(None) };
        let created = en.meta.created;
        Ok(self.get(en.meta.id)?.map(|c| (c, created)))
    }

    fn remove_ids(&mut self, ids: &[u64]) -> R<Vec<(u64, i64)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let now = now_secs();
        let tombs: Vec<(u64, i64)> =
            self.entries.iter().filter(|en| ids.contains(&en.meta.id)).map(|en| (en.meta.hash, now.max(en.meta.created))).collect();
        let w = self.db.begin_write().map_err(e)?;
        {
            let mut tomb = w.open_table(TOMB).map_err(e)?;
            for (h, at) in &tombs {
                tomb.insert(*h, *at).map_err(e)?;
            }
            let mut tm = w.open_table(META).map_err(e)?;
            let mut tb = w.open_table(BLOB).map_err(e)?;
            let mut tt = w.open_table(THUMB).map_err(e)?;
            let mut to = w.open_table(OCR).map_err(e)?;
            for id in ids {
                tm.remove(*id).map_err(e)?;
                tb.remove(*id).map_err(e)?;
                tt.remove(*id).map_err(e)?;
                to.remove(*id).map_err(e)?;
            }
        }
        w.commit().map_err(e)?;
        self.entries.retain(|en| !ids.contains(&en.meta.id));
        self.tombs.extend(tombs.iter().copied());
        Ok(tombs)
    }

    pub fn toggle_pin(&mut self, id: u64) -> R<()> {
        let Some(en) = self.entries.iter_mut().find(|en| en.meta.id == id) else { return Ok(()) };
        en.meta.pinned = !en.meta.pinned;
        let mb = postcard::to_stdvec(&DiskMeta::from_meta(&en.meta)).map_err(e)?;
        let w = self.db.begin_write().map_err(e)?;
        w.open_table(META).map_err(e)?.insert(id, mb.as_slice()).map_err(e)?;
        w.commit().map_err(e)?;
        Ok(())
    }
}

/// Case-insensitive substring search; `needle` must already be lowercase.
fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        if n.len() > h.len() {
            return false;
        }
        return h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    hay.to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("copym-test-{tag}-{}-{}.redb", std::process::id(), now_secs()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn q(s: &str) -> Query {
        Query::parse(s, false).unwrap()
    }

    #[test]
    fn insert_dedup_evict_search() {
        let p = tmp("basic");
        let mut s = Store::open(&p, 3).unwrap();
        for t in ["alpha", "Beta", "gamma", "delta"] {
            s.insert(Content::Text(t.into()), None).unwrap();
        }
        assert_eq!(s.len(), 3); // alpha evicted
        let ids: Vec<_> = s.list(&q(""), Filter::All, 10).iter().map(|m| m.preview.clone()).collect();
        assert_eq!(ids, ["delta", "gamma", "Beta"]);
        // dedup moves to top
        s.insert(Content::Text("Beta".into()), None).unwrap();
        assert_eq!(s.list(&q(""), Filter::All, 10)[0].preview, "Beta");
        assert_eq!(s.len(), 3);
        // case-insensitive search
        assert_eq!(s.list(&q("bet"), Filter::All, 10).len(), 1);
        // pin survives eviction
        let gamma = s.list(&q("gamma"), Filter::All, 1)[0].id;
        s.toggle_pin(gamma).unwrap();
        for t in ["x1", "x2", "x3", "x4"] {
            s.insert(Content::Text(t.into()), None).unwrap();
        }
        let l = s.list(&q(""), Filter::All, 10);
        assert_eq!(l[0].preview, "gamma");
        assert_eq!(l.len(), 4);
        drop(s);
        // persistence
        let s = Store::open(&p, 3).unwrap();
        assert_eq!(s.len(), 4);
        assert_eq!(s.list(&q("中"), Filter::All, 10).len(), 0);
        drop(s);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn groups_by_rules() {
        let p = tmp("groups");
        let mut s = Store::open(&p, 50).unwrap();
        s.insert(Content::Text("https://example.com/a".into()), Some("Firefox".into())).unwrap();
        s.insert(Content::Text("fn main() {}".into()), Some("Code".into())).unwrap();
        let code = s.insert(Content::Text("SELECT 1;".into()), None).unwrap();
        s.toggle_pin(code).unwrap();
        let png = imgutil::encode_png(2, 2, &[255; 16]).unwrap();
        s.insert(Content::Image { png, width: 2, height: 2 }, None).unwrap();

        let rule = |f: &dyn Fn(&mut GroupRule)| {
            let mut r = GroupRule::default();
            f(&mut r);
            GroupMatcher::new(&r).unwrap()
        };
        let link = rule(&|r| r.patterns = vec![r"^\s*https?://".into()]);
        let from_code = rule(&|r| r.apps = vec!["(?i)^code$".into()]);
        let images = rule(&|r| r.kind = Some(Kind::Image));
        let pinned = rule(&|r| r.pinned = true);
        let other = rule(&|r| {
            r.kind = Some(Kind::Text);
            r.exclude = vec![r"^\s*https?://".into()];
            r.exclude_apps = vec!["(?i)^code$".into()];
        });
        let names = |g: &GroupMatcher| -> Vec<String> { s.list_group(&q(""), Some(g), 10).into_iter().map(|m| m.preview).collect() };
        assert_eq!(names(&link), ["https://example.com/a"]);
        assert_eq!(names(&from_code), ["fn main() {}"]);
        assert_eq!(s.list_group(&q(""), Some(&images), 10).len(), 1);
        assert_eq!(names(&pinned), ["SELECT 1;"]);
        assert_eq!(names(&other), ["SELECT 1;"]);
        assert_eq!(s.list_group(&q(""), None, 10).len(), 4);

        let groups = vec![Some(link), Some(images), None];
        assert_eq!(s.group_counts(&q(""), &groups), vec![Some(1), Some(1), None]);
        assert_eq!(s.group_counts(&q("example"), &groups), vec![Some(1), Some(0), None]);
        assert!(GroupMatcher::new(&GroupRule { patterns: vec!["(".into()], ..Default::default() }).is_err());
        drop(s);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn regex_and_ocr_search() {
        let p = tmp("regex");
        let mut s = Store::open(&p, 50).unwrap();
        s.insert(Content::Text("订单号 A-2024-0042".into()), None).unwrap();
        s.insert(Content::Text("call me at 555-1234".into()), None).unwrap();
        let png = imgutil::encode_png(2, 2, &[255; 16]).unwrap();
        let img = s.insert(Content::Image { png, width: 2, height: 2 }, None).unwrap();
        assert_eq!(s.pending_ocr(), vec![img]);

        let re = |p: &str| Query::parse(p, true).unwrap();
        assert_eq!(s.list(&re(r"\b\d{3}-\d{4}\b"), Filter::All, 10).len(), 1); // not "024-0042"
        assert_eq!(s.list(&re(r"\d{3}-\d{4}"), Filter::All, 10).len(), 2);
        assert_eq!(s.list(&re(r"^订单号\s+a-\d+"), Filter::All, 10).len(), 1); // case-insensitive
        assert!(Query::parse("(unclosed", true).is_err());

        // OCR text becomes searchable, by plain and regex queries
        let h = s.hash_of(img).unwrap();
        s.set_ocr(h, "发票 金额 ¥128.00\nInvoice".into()).unwrap();
        assert!(s.pending_ocr().is_empty());
        assert_eq!(s.list(&q("发票"), Filter::Image, 10).len(), 1);
        assert_eq!(s.list(&q("INVOICE"), Filter::All, 10)[0].kind, Kind::Image);
        assert_eq!(s.list(&re(r"¥\d+\.\d{2}"), Filter::All, 10).len(), 1);
        drop(s);
        // OCR text persists
        let s = Store::open(&p, 50).unwrap();
        assert_eq!(s.list(&q("发票"), Filter::All, 10).len(), 1);
        drop(s);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sync_merge_order_and_tombstones() {
        let p = tmp("sync");
        let mut s = Store::open(&p, 50).unwrap();
        let now = now_secs();
        s.insert(Content::Text("local new".into()), None).unwrap();
        // An older item from another device slots in below, keeps its time.
        let old = Content::Text("from phone".into());
        assert!(s.insert_synced(old.clone(), Some("Phone".into()), now - 3600).unwrap().is_some());
        let l = s.list(&q(""), Filter::All, 10);
        assert_eq!(l[0].preview, "local new");
        assert_eq!((l[1].preview.as_str(), l[1].source.as_deref()), ("from phone", Some("Phone")));
        // Already present -> ignored, not moved.
        assert!(s.insert_synced(old.clone(), None, now).unwrap().is_none());
        assert_eq!(s.list(&q(""), Filter::All, 10)[0].preview, "local new");

        // Local delete produces a tombstone; the peer's stale copy is not fetched back.
        let id = s.list(&q("phone"), Filter::All, 1)[0].id;
        let tombs = s.delete(id).unwrap();
        assert_eq!(tombs.len(), 1);
        assert!(s.sync_missing(&[(old.hash(), now - 3600)]).is_empty());
        assert!(s.insert_synced(old.clone(), None, now - 3600).unwrap().is_none());
        // ...but a newer copy of the same content is accepted.
        assert_eq!(s.sync_missing(&[(old.hash(), now + 10)]), vec![old.hash()]);

        // Remote deletion removes older copies, keeps pinned ones.
        let keep = Content::Text("pinned".into());
        let kid = s.insert(keep.clone(), None).unwrap();
        s.toggle_pin(kid).unwrap();
        let gone = Content::Text("gone".into());
        s.insert(gone.clone(), None).unwrap();
        let n = s.delete_synced(&[(gone.hash(), now + 100), (keep.hash(), now + 100)]).unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.list(&q("gone"), Filter::All, 10).len(), 0);
        assert_eq!(s.list(&q("pinned"), Filter::All, 10).len(), 1);

        // Tombstones survive a restart; copying again revives the content.
        drop(s);
        let mut s = Store::open(&p, 50).unwrap();
        assert!(s.tombstones().iter().any(|(h, _)| *h == gone.hash()));
        s.insert(gone.clone(), None).unwrap();
        assert!(!s.tombstones().iter().any(|(h, _)| *h == gone.hash()));
        assert_eq!(s.sync_inventory(10, true, u64::MAX)[0].0, gone.hash());
        drop(s);
        let _ = std::fs::remove_file(&p);
    }
}
