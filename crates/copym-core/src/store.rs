//! Persistent history on SQLite, built to stay fast with tens of millions of
//! items. Nothing is loaded into memory up front; every lookup goes through
//! an index.
//!
//! Tables:
//! * `items` : metadata only (narrow rows, cheap to scan). `id` doubles as
//!   the sort key: `created << 20 | n`, so newest-first is a plain descending
//!   rowid walk and items received from other devices slot into place by
//!   their original time.
//! * `body`  : text payloads, LZ4-compressed above 256 bytes (cold column,
//!   read only for previews, search hits and rule checks).
//! * `blob`  : PNG payloads and thumbnails.
//! * `ocr`   : text recognized in images ("" = processed, nothing found).
//! * `fts`   : FTS5 trigram index over text / OCR text / source app. It is
//!   contentless with `detail=none` (row ids only, no positions), which keeps
//!   it near the size of the text itself; every candidate is checked again
//!   against the real text, so the index only has to narrow things down.
//! * `views` / `members` : which items belong to each content-based group
//!   rule (regex on text / app), maintained in the same transaction as every
//!   write, with the count kept alongside. Group counts and sparse groups
//!   never scan the history; new rules are built in the background in small
//!   steps. Rules on type / pinned only need no view (indexes + counters).
//! * `tomb`  : content hash -> deleted_at (so deletions sync across devices).
//! * `info`  : small counters and settings.
//!
//! Searches and scans run against a time budget and return a cursor to
//! continue from, so a slow query (a regex over everything) streams results
//! instead of freezing the caller.

use crate::imgutil;
use crate::model::{now_secs, Content, Filter, GroupRule, ItemMeta, Kind};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// Deletions older than this are forgotten.
const TOMB_TTL: i64 = 30 * 86_400;
const PREVIEW_CHARS: usize = 600;
const THUMB_SIZE: u32 = 480;
/// OCR text sent to the GUI per item (search always uses the full text).
const OCR_VIEW_CHARS: usize = 4000;
/// The full-text index covers the first part of each text (a match further
/// into a huge text is still found by short / regex scans, not by the index).
const INDEX_BYTES: usize = 64 * 1024;
/// Texts shorter than this are stored as they are.
const COMPRESS_MIN: usize = 256;
/// How long one list / count request may scan before it returns what it has.
pub const SEARCH_BUDGET: Duration = Duration::from_millis(80);
/// Group counts during a search stop after this many hits ("1000+").
const COUNT_CAP: usize = 1000;
/// `clear` records tombstones (for sync) for at most this many newest items.
const CLEAR_TOMBS: usize = 10_000;
const ID_SHIFT: u32 = 20;
const SCAN_BATCH: i64 = 256;
/// How long the exact (all-terms) index query may run before the search
/// falls back to one term plus text checks.
const AND_BUDGET: Duration = Duration::from_millis(30);
const BUILD_BATCH: i64 = 4096;
const MAX_VIEWS: usize = 32;
/// Unused group views are dropped after this long.
const VIEW_TTL: i64 = 30 * 86_400;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = -16384;
CREATE TABLE IF NOT EXISTS items(
    id INTEGER PRIMARY KEY,
    kind INTEGER NOT NULL,
    created INTEGER NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0, -- pin group (0 = not pinned)
    hash INTEGER NOT NULL,
    chars INTEGER NOT NULL,
    lines INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    source TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS items_hash ON items(hash);
CREATE INDEX IF NOT EXISTS items_kind ON items(kind, id);
CREATE INDEX IF NOT EXISTS items_pinned ON items(id) WHERE pinned != 0;
CREATE TABLE IF NOT EXISTS body(id INTEGER PRIMARY KEY, z BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS blob(id INTEGER PRIMARY KEY, png BLOB NOT NULL, thumb BLOB);
CREATE TABLE IF NOT EXISTS ocr(id INTEGER PRIMARY KEY, text TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tomb(hash INTEGER PRIMARY KEY, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS info(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS views(
    fp INTEGER PRIMARY KEY,
    rule BLOB NOT NULL,
    cursor INTEGER NOT NULL,
    count INTEGER NOT NULL,
    used INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS members(view INTEGER NOT NULL, id INTEGER NOT NULL, PRIMARY KEY (view, id)) WITHOUT ROWID;
CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
    text, source, content = '', contentless_delete = 1, detail = none, tokenize = 'trigram'
);
";

const COLS: &str = "SELECT i.id, i.kind, i.created, i.pinned, i.hash, i.chars, i.lines, i.bytes, i.width, i.height, i.source";
const WITH_TEXT: &str = ", b.z, o.text FROM items i LEFT JOIN body b ON b.id = i.id LEFT JOIN ocr o ON o.id = i.id";
const NO_TEXT: &str = ", NULL, NULL FROM items i";

type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

fn select(with_text: bool) -> String {
    format!("{COLS}{}", if with_text { WITH_TEXT } else { NO_TEXT })
}

// ------------------------------------------------------------------ rows --

/// One item as read from the database; the text is decoded on first use.
struct Row {
    meta: ItemMeta,
    /// Encoded body (texts) - only when read `with_text`.
    z: Option<Vec<u8>>,
    loaded: bool,
    text: OnceCell<String>,
}

impl Row {
    fn read(r: &rusqlite::Row, with_text: bool) -> rusqlite::Result<Row> {
        let kind = if r.get::<_, i64>(1)? == 1 { Kind::Image } else { Kind::Text };
        Ok(Row {
            meta: ItemMeta {
                id: r.get::<_, i64>(0)? as u64,
                kind,
                preview: String::new(),
                created: r.get(2)?,
                pinned: r.get::<_, i64>(3)? != 0,
                board: r.get::<_, i64>(3)? as u32,
                hash: r.get::<_, i64>(4)? as u64,
                chars: r.get(5)?,
                lines: r.get(6)?,
                bytes: r.get::<_, i64>(7)? as u64,
                width: r.get(8)?,
                height: r.get(9)?,
                source: r.get(10)?,
                ocr: if with_text { r.get(12)? } else { None },
            },
            z: if with_text { r.get(11)? } else { None },
            loaded: with_text,
            text: OnceCell::new(),
        })
    }

    fn text(&self) -> &str {
        self.text.get_or_init(|| self.z.as_deref().map(decode).unwrap_or_default())
    }

    /// Text that search and group rules run against.
    fn haystack(&self) -> &str {
        match self.meta.kind {
            Kind::Text => self.text(),
            Kind::Image => self.meta.ocr.as_deref().unwrap_or(""),
        }
    }

    fn source(&self) -> &str {
        self.meta.source.as_deref().unwrap_or("")
    }

    fn into_meta(self) -> ItemMeta {
        let preview = match self.meta.kind {
            Kind::Text => self.text().chars().take(PREVIEW_CHARS).collect(),
            Kind::Image => String::new(),
        };
        let mut m = self.meta;
        m.preview = preview;
        if let Some(o) = &mut m.ocr {
            if o.len() > OCR_VIEW_CHARS {
                *o = o.chars().take(OCR_VIEW_CHARS).collect();
            }
        }
        m
    }
}

fn encode(t: &str) -> Vec<u8> {
    if t.len() >= COMPRESS_MIN {
        let c = lz4_flex::compress_prepend_size(t.as_bytes());
        if c.len() < t.len() * 9 / 10 {
            let mut v = Vec::with_capacity(c.len() + 1);
            v.push(1);
            v.extend_from_slice(&c);
            return v;
        }
    }
    let mut v = Vec::with_capacity(t.len() + 1);
    v.push(0);
    v.extend_from_slice(t.as_bytes());
    v
}

fn decode(z: &[u8]) -> String {
    let bytes = match z.split_first() {
        Some((1, c)) => lz4_flex::decompress_size_prepended(c).unwrap_or_default(),
        Some((_, raw)) => raw.to_vec(),
        None => Vec::new(),
    };
    String::from_utf8(bytes).unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// The part of a text that goes into the full-text index.
fn index_part(t: &str) -> &str {
    if t.len() <= INDEX_BYTES {
        return t;
    }
    let mut end = INDEX_BYTES;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    &t[..end]
}

// --------------------------------------------------------------- queries --

/// A compiled search query.
pub enum Query {
    All,
    /// Lowercased terms, all must match (the text or the source app).
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
                .map_err(|e| crate::tf!("正则表达式无效：{e}", "invalid regular expression: {e}"));
        }
        Ok(Query::Terms(query.split_whitespace().map(|t| t.to_lowercase()).collect()))
    }

    fn matches(&self, row: &Row) -> bool {
        match self {
            Query::All => true,
            Query::Terms(terms) => terms.iter().all(|t| contains_ci(row.haystack(), t) || contains_ci(row.source(), t)),
            Query::Regex(re) => re.is_match(row.haystack()) || re.is_match(row.source()),
        }
    }

    /// Index terms that could narrow the candidates: a few quoted trigrams of
    /// each search term of 3+ characters. `None` = the index cannot help
    /// (short terms, regex).
    fn fts(&self) -> Option<Vec<String>> {
        let Query::Terms(terms) = self else { return None };
        let mut parts: Vec<String> = Vec::new();
        for t in terms {
            let cs: Vec<char> = t.chars().collect();
            if cs.len() < 3 {
                continue;
            }
            let n = cs.len() - 2;
            let mut picks: Vec<usize> = if n <= 6 { (0..n).collect() } else { (0..6).map(|i| i * (n - 1) / 5).collect() };
            picks.dedup();
            for i in picks {
                let g: String = cs[i..i + 3].iter().collect();
                let q = format!("\"{}\"", g.replace('"', "\"\""));
                if !parts.contains(&q) {
                    parts.push(q);
                }
            }
        }
        (!parts.is_empty()).then_some(parts)
    }
}

/// A compiled [`GroupRule`].
#[derive(Clone)]
pub struct GroupMatcher {
    rule: GroupRule,
    /// Fingerprint of the rule (identifies its view).
    fp: u64,
    /// The rule matches everything.
    all: bool,
    kind: Option<Kind>,
    pinned: bool,
    board: Option<u32>,
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
                .map(|p| regex_lite::Regex::new(p).map_err(|e| crate::tf!("分组规则 {p:?} 无效：{e}", "invalid group rule {p:?}: {e}")))
                .collect()
        };
        let m = Self {
            rule: rule.clone(),
            fp: 0,
            all: false,
            kind: rule.kind,
            pinned: rule.pinned,
            board: rule.board,
            patterns: compile(&rule.patterns)?,
            apps: compile(&rule.apps)?,
            exclude: compile(&rule.exclude)?,
            exclude_apps: compile(&rule.exclude_apps)?,
        };
        let all = m.kind.is_none() && !m.pinned && m.board.is_none() && m.patterns.is_empty() && m.apps.is_empty() && m.exclude.is_empty() && m.exclude_apps.is_empty();
        let key = postcard::to_stdvec(rule).unwrap_or_default();
        Ok(Self { fp: xxhash_rust::xxh3::xxh3_64(&key) >> 1, all, ..m })
    }

    /// A pin group: lists pinned items, which every other group leaves out.
    fn pins(&self) -> bool {
        self.pinned || self.board.is_some()
    }

    /// Only type / pinned conditions: answered from indexes, no view needed.
    fn sql_only(&self) -> bool {
        self.patterns.is_empty() && self.apps.is_empty() && self.exclude.is_empty() && self.exclude_apps.is_empty()
    }

    fn needs_text(&self) -> bool {
        !self.patterns.is_empty() || !self.exclude.is_empty()
    }

    fn matches(&self, row: &Row) -> bool {
        let m = &row.meta;
        if self.kind.is_some_and(|k| k != m.kind) || (self.pinned && !m.pinned) || self.board.is_some_and(|b| b != m.board) {
            return false;
        }
        let src = row.source();
        let any = |rs: &[regex_lite::Regex], s: &str| rs.iter().any(|r| r.is_match(s));
        (self.apps.is_empty() || any(&self.apps, src))
            && !(!src.is_empty() && any(&self.exclude_apps, src))
            && (self.patterns.is_empty() || any(&self.patterns, row.haystack()))
            && !any(&self.exclude, row.haystack())
    }
}

/// One page of a listing.
pub struct Page {
    pub items: Vec<ItemMeta>,
    /// Pass as `before` to continue; `None` = nothing more.
    pub next: Option<u64>,
}

/// Item counts per group.
#[derive(Debug, PartialEq, Eq)]
pub struct Counts {
    /// `None` = invalid rule.
    pub counts: Vec<Option<u32>>,
    /// Some counts are lower bounds (a capped search, a view still building).
    pub more: bool,
}

/// Members of one group rule, maintained on every write.
struct View {
    m: GroupMatcher,
    /// Ids below this have not been checked yet (built newest first); 0 = complete.
    cursor: u64,
    count: u64,
    used: i64,
}

/// Per-view membership changes of one transaction (applied to the in-memory
/// counts after it commits).
type Delta = HashMap<u64, i64>;

// ----------------------------------------------------------------- store --

pub struct Store {
    db: Connection,
    total: u64,
    images: u64,
    max_items: usize,
    views: HashMap<u64, View>,
    checkpointed: Instant,
}

impl Store {
    pub fn open(path: &Path, max_items: usize) -> R<Self> {
        let db = Connection::open(path).map_err(e)?;
        db.execute_batch(SCHEMA).map_err(e)?;
        db.execute("DELETE FROM tomb WHERE at < ?1", [now_secs() - TOMB_TTL]).map_err(e)?;
        let get = |k: &str| -> R<Option<u64>> {
            db.query_row("SELECT value FROM info WHERE key = ?1", [k], |r| r.get::<_, i64>(0)).optional().map(|v| v.map(|v| v as u64)).map_err(e)
        };
        let count = |sql: &str| -> R<u64> { db.query_row(sql, [], |r| r.get::<_, i64>(0)).map(|n| n as u64).map_err(e) };
        let total = match get("total")? {
            Some(n) => n,
            None => count("SELECT count(*) FROM items")?,
        };
        let images = match get("images")? {
            Some(n) => n,
            None => count("SELECT count(*) FROM items WHERE kind = 1")?,
        };
        let mut views = HashMap::new();
        let mut stale = Vec::new();
        {
            let mut st = db.prepare("SELECT fp, rule, cursor, count, used FROM views").map_err(e)?;
            let rows = st
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)))
                .map_err(e)?;
            for (fp, rule, cursor, count, used) in rows.flatten() {
                let m = postcard::from_bytes::<GroupRule>(&rule).ok().and_then(|r| GroupMatcher::new(&r).ok());
                match m {
                    Some(m) if m.fp == fp as u64 && used >= now_secs() - VIEW_TTL => {
                        views.insert(m.fp, View { m, cursor: cursor as u64, count: count as u64, used });
                    }
                    _ => stale.push(fp),
                }
            }
        }
        for fp in stale {
            db.execute("DELETE FROM views WHERE fp = ?1", [fp]).map_err(e)?;
            db.execute("DELETE FROM members WHERE view = ?1", [fp]).map_err(e)?;
        }
        Ok(Self { db, total, images, max_items: max_items.max(1), views, checkpointed: Instant::now() })
    }

    pub fn len(&self) -> usize {
        self.total as usize
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    fn max_id(&self) -> R<u64> {
        self.db.query_row("SELECT max(id) FROM items", [], |r| r.get::<_, Option<i64>>(0)).map(|v| v.unwrap_or(0) as u64).map_err(e)
    }

    /// (id, created, pin group) of the item with this content.
    fn id_of_hash(&self, hash: u64) -> R<Option<(u64, i64, u32)>> {
        self.db
            .prepare_cached("SELECT id, created, pinned FROM items WHERE hash = ?1")
            .and_then(|mut st| st.query_row([hash as i64], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?))).optional())
            .map_err(e)
    }

    fn tomb_of(&self, hash: u64) -> R<Option<i64>> {
        self.db
            .prepare_cached("SELECT at FROM tomb WHERE hash = ?1")
            .and_then(|mut st| st.query_row([hash as i64], |r| r.get(0)).optional())
            .map_err(e)
    }

    /// Insert a local copy (or move to top when an identical item exists).
    /// Returns the new id.
    pub fn insert(&mut self, content: Content, source: Option<String>) -> R<u64> {
        let hash = content.hash();
        let max = self.max_id()?;
        let old = self.id_of_hash(hash)?;
        if let Some((id, _, _)) = old {
            if id == max {
                return Ok(id); // already the newest
            }
        }
        let (board, ocr) = match old {
            Some((id, _, board)) => (board, self.ocr_of(id)?),
            None => (0, None),
        };
        let created = now_secs().max((max >> ID_SHIFT) as i64);
        let id = ((created as u64) << ID_SHIFT).max(max + 1);
        self.write(id, content, source, created, board, ocr, old.map(|o| o.0))
    }

    /// Insert an item received from another device. Keeps its timestamp; does
    /// nothing if we already have it or deleted it after it was copied.
    pub fn insert_synced(&mut self, content: Content, source: Option<String>, created: i64) -> R<Option<u64>> {
        let hash = content.hash();
        if self.id_of_hash(hash)?.is_some() || self.tomb_of(hash)?.is_some_and(|at| at >= created) {
            return Ok(None);
        }
        let created = created.clamp(0, now_secs()); // don't let a fast clock pin items on top
        // a free slot inside that second, spread by the hash
        let mut id = ((created as u64) << ID_SHIFT) | ((hash >> 45) & 0x7_FFFF);
        while self.db.query_row("SELECT 1 FROM items WHERE id = ?1", [id as i64], |_| Ok(())).optional().map_err(e)?.is_some() {
            id += 1;
        }
        self.write(id, content, source, created, 0, None, None).map(Some)
    }

    fn ocr_of(&self, id: u64) -> R<Option<String>> {
        self.db.query_row("SELECT text FROM ocr WHERE id = ?1", [id as i64], |r| r.get(0)).optional().map_err(e)
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        id: u64,
        content: Content,
        source: Option<String>,
        created: i64,
        board: u32,
        ocr: Option<String>,
        replaces: Option<u64>,
    ) -> R<u64> {
        let hash = content.hash();
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
            pinned: board != 0,
            source,
            hash,
            ocr,
            board,
        };
        let text = match &content {
            Content::Text(t) => {
                meta.chars = t.chars().count() as u32;
                meta.lines = t.lines().count().max(1) as u32;
                Some(t.as_str())
            }
            Content::Image { width, height, .. } => {
                meta.width = *width;
                meta.height = *height;
                None
            }
        };
        let thumb = match &content {
            Content::Image { png, .. } => imgutil::png_thumbnail(png, THUMB_SIZE),
            Content::Text(_) => None,
        };
        let pinned_n = self.pinned_count()?;
        let is_image = meta.kind == Kind::Image;
        let row = Row { meta, z: None, loaded: true, text: OnceCell::from(text.unwrap_or("").to_string()) };
        let meta = &row.meta;
        let fps: Vec<u64> = self.views.keys().copied().collect();

        let tx = self.db.transaction().map_err(e)?;
        let mut d = Delta::new();
        let (mut total, mut images) = (self.total, self.images);
        if let Some(old) = replaces {
            unmember(&tx, &fps, &[old], &mut d)?;
            let (n, ni) = delete_rows(&tx, &[old])?;
            total -= n;
            images -= ni;
        }
        tx.prepare_cached(
            "INSERT INTO items(id, kind, created, pinned, hash, chars, lines, bytes, width, height, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )
        .and_then(|mut st| {
            st.execute(params![
                id as i64,
                is_image as i64,
                created,
                board,
                hash as i64,
                meta.chars,
                meta.lines,
                meta.bytes as i64,
                meta.width,
                meta.height,
                meta.source
            ])
        })
        .map_err(e)?;
        total += 1;
        images += is_image as u64;
        match &content {
            Content::Text(t) => {
                tx.prepare_cached("INSERT INTO body(id, z) VALUES (?1, ?2)").and_then(|mut st| st.execute(params![id as i64, encode(t)])).map_err(e)?;
            }
            Content::Image { png, .. } => {
                tx.prepare_cached("INSERT INTO blob(id, png, thumb) VALUES (?1, ?2, ?3)")
                    .and_then(|mut st| st.execute(params![id as i64, png, thumb]))
                    .map_err(e)?;
            }
        }
        if let Some(o) = &meta.ocr {
            tx.execute("INSERT OR REPLACE INTO ocr(id, text) VALUES (?1, ?2)", params![id as i64, o]).map_err(e)?;
        }
        let indexed = text.or(meta.ocr.as_deref()).unwrap_or("");
        fts_insert(&tx, id, indexed, meta.source.as_deref())?;
        member(&tx, &self.views, &row, &mut d)?;
        // A new copy revives content that was deleted earlier.
        tx.prepare_cached("DELETE FROM tomb WHERE hash = ?1").and_then(|mut st| st.execute([hash as i64])).map_err(e)?;

        // Evict the oldest non-pinned items beyond the limit.
        let unpinned = total.saturating_sub(pinned_n + (board != 0) as u64);
        if unpinned > self.max_items as u64 {
            let extra = (unpinned - self.max_items as u64) as i64;
            let ids: Vec<u64> = {
                let mut st = tx.prepare_cached("SELECT id FROM items WHERE pinned = 0 ORDER BY id LIMIT ?1").map_err(e)?;
                st.query_map([extra], |r| r.get::<_, i64>(0)).map_err(e)?.map(|r| r.map(|v| v as u64)).collect::<Result<_, _>>().map_err(e)?
            };
            unmember(&tx, &fps, &ids, &mut d)?;
            let (n, ni) = delete_rows(&tx, &ids)?;
            total -= n;
            images -= ni;
        }
        set_counters(&tx, total, images)?;
        save_delta(&tx, &d)?;
        tx.commit().map_err(e)?;
        self.total = total;
        self.images = images;
        apply_delta(&mut self.views, d);
        Ok(id)
    }

    fn pinned_count(&self) -> R<u64> {
        self.db.query_row("SELECT count(*) FROM items WHERE pinned != 0", [], |r| r.get::<_, i64>(0)).map(|n| n as u64).map_err(e)
    }

    // ---------------------------------------------------------- listing --

    pub fn list(&mut self, query: &Query, filter: Filter, limit: usize) -> Vec<ItemMeta> {
        let rule = match filter {
            Filter::All => GroupRule::default(),
            Filter::Text => GroupRule { kind: Some(Kind::Text), ..Default::default() },
            Filter::Image => GroupRule { kind: Some(Kind::Image), ..Default::default() },
            Filter::Pinned => GroupRule { pinned: true, ..Default::default() },
        };
        let m = GroupMatcher::new(&rule).ok();
        self.list_group(query, m.as_ref(), None, limit).items
    }

    /// Items of a group (`None` = all), newest first. Pinned items only show
    /// in groups that ask for them (`pinned` / `board`), never in the others.
    /// Continue with `before = page.next`.
    pub fn list_group(&mut self, query: &Query, group: Option<&GroupMatcher>, before: Option<u64>, limit: usize) -> Page {
        let deadline = Instant::now() + SEARCH_BUDGET;
        let group = group.filter(|g| !g.all);
        if let Some(g) = group.filter(|g| !g.sql_only()) {
            self.view(g);
        }
        let (rows, next) = self.collect(query, group, before, limit, deadline).unwrap_or_default();
        Page { items: self.finish(rows), next }
    }

    /// Every pinned row, newest first (there are few).
    fn pinned_rows(&self) -> R<Vec<Row>> {
        let mut st = self.db.prepare_cached(&format!("{} WHERE i.pinned != 0 ORDER BY i.id DESC", select(true))).map_err(e)?;
        let rows = st.query_map([], |r| Row::read(r, true)).map_err(e)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Rows matching `query` and `group`. A pin group lists its (few) pinned
    /// items in one page; other groups leave pinned items out.
    fn collect(&mut self, query: &Query, group: Option<&GroupMatcher>, before: Option<u64>, limit: usize, deadline: Instant) -> R<(Vec<Row>, Option<u64>)> {
        let keep = |row: &Row| query.matches(row) && group.is_none_or(|g| g.matches(row));
        let mut out: Vec<Row> = Vec::new();
        if group.is_some_and(|g| g.pins()) {
            if before.is_none() {
                out = self.pinned_rows()?.into_iter().filter(|r| keep(r)).collect();
            }
            return Ok((out, None));
        }
        let mut cursor = before.unwrap_or(i64::MAX as u64);
        let mut found = 0;
        // Adds the matches of one batch; `Some(id)` once the page is full.
        let mut take = |out: &mut Vec<Row>, batch: Vec<Row>, checked: bool| -> Option<u64> {
            for row in batch {
                if row.meta.pinned || !(checked || keep(&row)) {
                    continue;
                }
                let id = row.meta.id;
                out.push(row);
                found += 1;
                if found >= limit {
                    return Some(id);
                }
            }
            None
        };

        if let Some(terms) = query.fts() {
            // 1. All trigrams at once: exact and fast when matches exist. But
            //    FTS5 cannot stop an AND that matches nothing before the end
            //    of the index, so it only gets a short time slice.
            if terms.len() > 1 {
                let until = deadline.min(Instant::now() + AND_BUDGET);
                self.db.progress_handler(1000, Some(move || Instant::now() >= until)).map_err(e)?;
                let and = terms.join(" AND ");
                let db = &self.db;
                let res = (|| -> rusqlite::Result<Option<u64>> {
                    let mut st = db.prepare_cached("SELECT rowid FROM fts WHERE fts MATCH ?1 AND rowid < ?2 ORDER BY rowid DESC")?;
                    let mut rows = st.query(params![and, cursor as i64])?;
                    loop {
                        let mut ids: Vec<u64> = Vec::with_capacity(SCAN_BATCH as usize);
                        while ids.len() < SCAN_BATCH as usize {
                            match rows.next()? {
                                Some(r) => ids.push(r.get::<_, i64>(0)? as u64),
                                None => break,
                            }
                        }
                        let eof = ids.len() < SCAN_BATCH as usize;
                        let batch = fetch_rows_raw(db, &ids, true)?;
                        if let Some(full) = take(&mut out, batch, false) {
                            return Ok(Some(full));
                        }
                        if eof {
                            return Ok(None);
                        }
                        cursor = *ids.last().unwrap();
                    }
                })();
                self.db.progress_handler(0, None::<fn() -> bool>).map_err(e)?;
                match res {
                    Ok(next) => return Ok((out, next)),
                    Err(rusqlite::Error::SqliteFailure(f, _)) if f.code == rusqlite::ErrorCode::OperationInterrupted => {}
                    Err(err) => return Err(e(err)),
                }
                if Instant::now() >= deadline {
                    return Ok((out, Some(cursor)));
                }
            }
            // 2. The rarest trigram alone, in time windows (one term honours the
            //    rowid bounds); the text check covers the other terms.
            let m = self.rarest(&terms, cursor)?;
            // The index is read in id (= time) windows sized by how fast the last one went:
            // a query whose terms are all common but never occur together would
            // otherwise merge millions of entries before reporting anything,
            // and the deadline could not stop it at a known position.
            let mut st = self.db.prepare_cached("SELECT rowid FROM fts WHERE fts MATCH ?1 AND rowid < ?2 AND rowid >= ?3 ORDER BY rowid DESC").map_err(e)?;
            let mut window: u64 = (7 * 86_400) << ID_SHIFT;
            loop {
                let started = Instant::now();
                let lo = cursor.saturating_sub(window);
                let mut rows = st.query(params![m, cursor as i64, lo as i64]).map_err(e)?;
                loop {
                    let mut ids: Vec<u64> = Vec::with_capacity(SCAN_BATCH as usize);
                    while ids.len() < SCAN_BATCH as usize {
                        match rows.next().map_err(e)? {
                            Some(r) => ids.push(r.get::<_, i64>(0).map_err(e)? as u64),
                            None => break,
                        }
                    }
                    let window_done = ids.len() < SCAN_BATCH as usize;
                    if let Some(full) = take(&mut out, self.rows_by_ids(&ids, true)?, false) {
                        return Ok((out, Some(full)));
                    }
                    if window_done {
                        break;
                    }
                    cursor = *ids.last().unwrap();
                    if Instant::now() >= deadline {
                        return Ok((out, Some(cursor)));
                    }
                }
                if lo == 0 {
                    return Ok((out, None));
                }
                cursor = lo;
                // cheap windows grow fast; an expensive one (common terms that
                // never meet) stops growing, so the deadline overshoots by at
                // most about one window
                let took = started.elapsed();
                window = if took < Duration::from_millis(5) {
                    window.saturating_mul(8)
                } else if took < Duration::from_millis(20) {
                    window.saturating_mul(2)
                } else if took > Duration::from_millis(40) {
                    (window / 2).max(3600 << ID_SHIFT)
                } else {
                    window
                };
                if Instant::now() >= deadline {
                    return Ok((out, Some(cursor)));
                }
            }
        }

        let need_text = !matches!(query, Query::All) || group.is_some_and(|g| g.needs_text());
        let view = group.and_then(|g| self.views.get(&g.fp)).filter(|v| v.cursor == 0 && matches!(query, Query::All));
        loop {
            // the next batch below the cursor, newest first
            let (batch, last) = if let Some(v) = view {
                let ids: Vec<u64> = {
                    let mut st = self.db.prepare_cached("SELECT id FROM members WHERE view = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3").map_err(e)?;
                    st.query_map(params![v.m.fp as i64, cursor as i64, SCAN_BATCH], |r| r.get::<_, i64>(0))
                        .map_err(e)?
                        .map(|r| r.map(|v| v as u64))
                        .collect::<Result<_, _>>()
                        .map_err(e)?
                };
                (self.rows_by_ids(&ids, false)?, ids.last().copied())
            } else {
                let kind = group.and_then(|g| g.kind);
                let sql = format!(
                    "{} WHERE i.id < ?1 AND i.pinned = 0{} ORDER BY i.id DESC LIMIT ?2",
                    select(need_text),
                    match kind {
                        Some(Kind::Text) => " AND i.kind = 0",
                        Some(Kind::Image) => " AND i.kind = 1",
                        None => "",
                    }
                );
                let mut st = self.db.prepare_cached(&sql).map_err(e)?;
                let rows: Vec<Row> = st.query_map(params![cursor as i64, SCAN_BATCH], |r| Row::read(r, need_text)).map_err(e)?.filter_map(Result::ok).collect();
                let last = rows.last().map(|r| r.meta.id);
                (rows, last)
            };
            let Some(last) = last else { return Ok((out, None)) };
            if let Some(full) = take(&mut out, batch, view.is_some()) {
                return Ok((out, Some(full)));
            }
            cursor = last;
            if Instant::now() >= deadline {
                return Ok((out, Some(cursor)));
            }
        }
    }

    /// The index term with the fewest hits in the day below `cursor`.
    fn rarest(&self, terms: &[String], cursor: u64) -> R<String> {
        if terms.len() == 1 {
            return Ok(terms[0].clone());
        }
        let lo = cursor.saturating_sub(86_400 << ID_SHIFT);
        let mut st = self
            .db
            .prepare_cached("SELECT count(*) FROM (SELECT rowid FROM fts WHERE fts MATCH ?1 AND rowid < ?2 AND rowid >= ?3 LIMIT 1000)")
            .map_err(e)?;
        let mut best = (i64::MAX, &terms[0]);
        for t in terms {
            let n: i64 = st.query_row(params![t, cursor as i64, lo as i64], |r| r.get(0)).map_err(e)?;
            if n < best.0 {
                best = (n, t);
                if n == 0 {
                    break;
                }
            }
        }
        Ok(best.1.clone())
    }

    fn rows_by_ids(&self, ids: &[u64], with_text: bool) -> R<Vec<Row>> {
        fetch_rows(&self.db, ids, with_text)
    }

    /// Rows to metadata, loading the text for the previews where needed.
    fn finish(&self, rows: Vec<Row>) -> Vec<ItemMeta> {
        let missing: Vec<u64> = rows.iter().filter(|r| !r.loaded).map(|r| r.meta.id).collect();
        let mut loaded: HashMap<u64, Row> =
            self.rows_by_ids(&missing, true).unwrap_or_default().into_iter().map(|r| (r.meta.id, r)).collect();
        rows.into_iter().map(|r| if r.loaded { r } else { loaded.remove(&r.meta.id).unwrap_or(r) }).map(Row::into_meta).collect()
    }

    /// How many items of each group match the search.
    pub fn group_counts(&mut self, query: &Query, groups: &[Option<GroupMatcher>]) -> Counts {
        let deadline = Instant::now() + SEARCH_BUDGET;
        if matches!(query, Query::All) {
            let fps: Vec<u64> = groups.iter().flatten().filter(|g| !g.sql_only()).map(|g| g.fp).collect();
            for g in groups.iter().flatten().filter(|g| !g.sql_only()) {
                self.view(g);
            }
            // small histories finish right here; big ones keep building in the background
            while Instant::now() < deadline && self.build_step(&fps).unwrap_or(false) {}
            let mut more = false;
            let counts = groups
                .iter()
                .map(|g| {
                    g.as_ref().map(|g| {
                        if g.sql_only() {
                            return self.sql_count(g) as u32;
                        }
                        let v = &self.views[&g.fp];
                        more |= v.cursor != 0;
                        // pinned items only show in pin groups
                        let pinned: i64 = self
                            .db
                            .query_row("SELECT count(*) FROM items i JOIN members m ON m.view = ?1 AND m.id = i.id WHERE i.pinned != 0", [g.fp as i64], |r| r.get(0))
                            .unwrap_or(0);
                        v.count.saturating_sub(pinned as u64) as u32
                    })
                })
                .collect();
            return Counts { counts, more };
        }
        let (rows, next) = self.collect(query, None, None, COUNT_CAP, deadline).unwrap_or_default();
        let pinned: Vec<Row> = self.pinned_rows().unwrap_or_default().into_iter().filter(|r| query.matches(r)).collect();
        let counts = groups
            .iter()
            .map(|g| g.as_ref().map(|g| if g.pins() { &pinned } else { &rows }.iter().filter(|r| g.matches(r)).count() as u32))
            .collect();
        Counts { counts, more: next.is_some() }
    }

    /// Count of a type / pinned rule, from counters and the pinned index.
    /// Other groups leave pinned items out.
    fn sql_count(&self, g: &GroupMatcher) -> u64 {
        let kind = match g.kind {
            None => "",
            Some(Kind::Text) => " AND kind = 0",
            Some(Kind::Image) => " AND kind = 1",
        };
        let board = if g.board.is_some() { " AND pinned = ?1" } else { " AND ?1 IS NOT NULL" };
        let sql = format!("SELECT count(*) FROM items WHERE pinned != 0{kind}{board}");
        let pinned = self.db.query_row(&sql, [g.board.unwrap_or(0) as i64], |r| r.get::<_, i64>(0)).map(|n| n as u64).unwrap_or(0);
        if g.pins() {
            return pinned;
        }
        let all = match g.kind {
            None => self.total,
            Some(Kind::Text) => self.total - self.images,
            Some(Kind::Image) => self.images,
        };
        all.saturating_sub(pinned)
    }

    // ------------------------------------------------------------ views --

    /// The view of a content rule, created on first use.
    fn view(&mut self, g: &GroupMatcher) {
        let now = now_secs();
        if let Some(v) = self.views.get_mut(&g.fp) {
            if now - v.used > 3600 {
                let _ = self.db.execute("UPDATE views SET used = ?2 WHERE fp = ?1", params![g.fp as i64, now]);
            }
            v.used = now;
            return;
        }
        if self.views.len() >= MAX_VIEWS {
            if let Some(old) = self.views.iter().min_by_key(|(_, v)| v.used).map(|(k, _)| *k) {
                self.views.remove(&old);
                let _ = self.db.execute("DELETE FROM views WHERE fp = ?1", [old as i64]);
                let _ = self.db.execute("DELETE FROM members WHERE view = ?1", [old as i64]);
            }
        }
        let cursor = self.max_id().unwrap_or(0) + 1;
        let Ok(rule) = postcard::to_stdvec(&g.rule) else { return };
        let _ = self.db.execute("DELETE FROM members WHERE view = ?1", [g.fp as i64]);
        let _ = self.db.execute(
            "INSERT OR REPLACE INTO views(fp, rule, cursor, count, used) VALUES (?1, ?2, ?3, 0, ?4)",
            params![g.fp as i64, rule, cursor as i64, now],
        );
        self.views.insert(g.fp, View { m: g.clone(), cursor, count: 0, used: now });
    }

    /// Check one batch of items for one unfinished view (of `fps` if given,
    /// else the most recently used); every unfinished view at the same
    /// position shares the batch. Returns whether work remains.
    fn build_step(&mut self, fps: &[u64]) -> R<bool> {
        let pick = self
            .views
            .iter()
            .filter(|(fp, v)| v.cursor != 0 && (fps.is_empty() || fps.contains(fp)))
            .max_by_key(|(_, v)| v.used)
            .map(|(fp, _)| *fp);
        let Some(fp) = pick else { return Ok(false) };
        let cursor = self.views[&fp].cursor;
        let group: Vec<u64> = self.views.iter().filter(|(_, v)| v.cursor == cursor).map(|(k, _)| *k).collect();
        let need_text = group.iter().any(|k| self.views[k].m.needs_text());
        let rows: Vec<Row> = {
            let mut st = self.db.prepare_cached(&format!("{} WHERE i.id < ?1 ORDER BY i.id DESC LIMIT ?2", select(need_text))).map_err(e)?;
            st.query_map(params![cursor as i64, BUILD_BATCH], |r| Row::read(r, need_text)).map_err(e)?.filter_map(Result::ok).collect()
        };
        let next = match rows.last() {
            Some(last) if rows.len() as i64 == BUILD_BATCH => last.meta.id,
            _ => 0,
        };
        let tx = self.db.transaction().map_err(e)?;
        let mut added: Vec<(u64, u64)> = Vec::new();
        for k in &group {
            let m = &self.views[k].m;
            let mut n = 0;
            for r in rows.iter().filter(|r| m.matches(r)) {
                n += tx
                    .prepare_cached("INSERT OR IGNORE INTO members(view, id) VALUES (?1, ?2)")
                    .and_then(|mut st| st.execute(params![*k as i64, r.meta.id as i64]))
                    .map_err(e)? as u64;
            }
            tx.execute("UPDATE views SET cursor = ?2, count = count + ?3 WHERE fp = ?1", params![*k as i64, next as i64, n as i64]).map_err(e)?;
            added.push((*k, n));
        }
        tx.commit().map_err(e)?;
        for (k, n) in added {
            let v = self.views.get_mut(&k).unwrap();
            v.count += n;
            v.cursor = next;
        }
        Ok(self.views.values().any(|v| v.cursor != 0))
    }

    /// Background upkeep for up to `budget`: builds group views, checkpoints
    /// the WAL. Returns whether more work is waiting.
    pub fn background_work(&mut self, budget: Duration) -> bool {
        let end = Instant::now() + budget;
        let mut more = false;
        while Instant::now() < end {
            more = self.build_step(&[]).unwrap_or(false);
            if !more {
                break;
            }
        }
        // checkpoint here, in the background, so writers rarely pay for it
        if self.checkpointed.elapsed() >= Duration::from_secs(10) {
            self.checkpointed = Instant::now();
            let _ = self.db.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        }
        more
    }

    /// Start views over (after changes that affect everything).
    fn reset_views(&mut self, only_text: bool) -> R<()> {
        let cursor = self.max_id()? + 1;
        let tx = self.db.transaction().map_err(e)?;
        for (fp, v) in self.views.iter_mut().filter(|(_, v)| !only_text || v.m.needs_text()) {
            tx.execute("DELETE FROM members WHERE view = ?1", [*fp as i64]).map_err(e)?;
            tx.execute("UPDATE views SET cursor = ?2, count = 0 WHERE fp = ?1", params![*fp as i64, cursor as i64]).map_err(e)?;
            v.cursor = cursor;
            v.count = 0;
        }
        tx.commit().map_err(e)
    }

    // ------------------------------------------------------------ items --

    pub fn get(&self, id: u64) -> R<Option<Content>> {
        let row: Option<(i64, u32, u32)> = self
            .db
            .query_row("SELECT kind, width, height FROM items WHERE id = ?1", [id as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
            .map_err(e)?;
        let Some((kind, width, height)) = row else { return Ok(None) };
        if kind == 1 {
            let png: Option<Vec<u8>> = self.db.query_row("SELECT png FROM blob WHERE id = ?1", [id as i64], |r| r.get(0)).optional().map_err(e)?;
            Ok(png.map(|png| Content::Image { png, width, height }))
        } else {
            let z: Option<Vec<u8>> = self.db.query_row("SELECT z FROM body WHERE id = ?1", [id as i64], |r| r.get(0)).optional().map_err(e)?;
            Ok(z.map(|z| Content::Text(decode(&z))))
        }
    }

    pub fn source(&self, id: u64) -> Option<String> {
        self.db.query_row("SELECT source FROM items WHERE id = ?1", [id as i64], |r| r.get(0)).ok().flatten()
    }

    pub fn thumb(&self, id: u64) -> R<Option<Vec<u8>>> {
        self.db.query_row("SELECT thumb FROM blob WHERE id = ?1", [id as i64], |r| r.get(0)).optional().map(Option::flatten).map_err(e)
    }

    /// Images that have not been through OCR yet, newest first.
    pub fn pending_ocr(&self) -> Vec<u64> {
        let Ok(mut st) = self.db.prepare(
            "SELECT i.id FROM items i WHERE i.kind = 1 AND NOT EXISTS (SELECT 1 FROM ocr o WHERE o.id = i.id) ORDER BY i.id DESC",
        ) else {
            return Vec::new();
        };
        st.query_map([], |r| r.get::<_, i64>(0)).map(|rows| rows.filter_map(Result::ok).map(|v| v as u64).collect()).unwrap_or_default()
    }

    pub fn needs_ocr(&self, id: u64) -> bool {
        self.db
            .query_row(
                "SELECT 1 FROM items i WHERE i.id = ?1 AND i.kind = 1 AND NOT EXISTS (SELECT 1 FROM ocr o WHERE o.id = i.id)",
                [id as i64],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Hash of an item, used to find it again after it was moved to the top.
    pub fn hash_of(&self, id: u64) -> Option<u64> {
        self.db.query_row("SELECT hash FROM items WHERE id = ?1", [id as i64], |r| r.get::<_, i64>(0)).ok().map(|h| h as u64)
    }

    /// Store OCR text for the image with this content hash (ids change when an
    /// item is moved to the top while OCR is running).
    pub fn set_ocr(&mut self, hash: u64, text: String) -> R<()> {
        let row: Option<(i64, Option<String>)> = self
            .db
            .query_row("SELECT id, source FROM items WHERE hash = ?1 AND kind = 1", [hash as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(e)?;
        let Some((id, source)) = row else { return Ok(()) };
        let tx = self.db.transaction().map_err(e)?;
        tx.execute("INSERT OR REPLACE INTO ocr(id, text) VALUES (?1, ?2)", params![id, text]).map_err(e)?;
        tx.execute("DELETE FROM fts WHERE rowid = ?1", [id]).map_err(e)?;
        fts_insert(&tx, id as u64, &text, source.as_deref())?;
        let mut d = Delta::new();
        for row in fetch_rows(&tx, &[id as u64], true)? {
            member(&tx, &self.views, &row, &mut d)?;
        }
        save_delta(&tx, &d)?;
        tx.commit().map_err(e)?;
        apply_delta(&mut self.views, d);
        Ok(())
    }

    /// Forget all OCR results so every image is recognized again (after the
    /// OCR pipeline improved). Returns how many images will be redone.
    pub fn reset_ocr(&mut self) -> R<usize> {
        let images: Vec<(i64, Option<String>)> = {
            let mut st = self.db.prepare("SELECT id, source FROM items WHERE kind = 1").map_err(e)?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e)?.filter_map(Result::ok).collect()
        };
        let tx = self.db.transaction().map_err(e)?;
        tx.execute("DELETE FROM ocr", []).map_err(e)?;
        for (id, source) in &images {
            tx.execute("DELETE FROM fts WHERE rowid = ?1", [id]).map_err(e)?;
            fts_insert(&tx, *id as u64, "", source.as_deref())?;
        }
        tx.commit().map_err(e)?;
        // views that look at the text start over
        self.reset_views(true)?;
        Ok(images.len())
    }

    pub fn info(&self, key: &str) -> Option<u64> {
        self.db.query_row("SELECT value FROM info WHERE key = ?1", [format!("user.{key}")], |r| r.get::<_, i64>(0)).ok().map(|v| v as u64)
    }

    pub fn set_info(&mut self, key: &str, value: u64) -> R<()> {
        self.db
            .execute("INSERT OR REPLACE INTO info(key, value) VALUES (?1, ?2)", params![format!("user.{key}"), value as i64])
            .map(|_| ())
            .map_err(e)
    }

    /// Delete one item. Returns the tombstone to propagate: (hash, deleted_at).
    pub fn delete(&mut self, id: u64) -> R<Vec<(u64, i64)>> {
        self.remove_ids(&[id])
    }

    /// Delete all non-pinned items. Returns the tombstones (for the newest
    /// items only: older ones are long out of the sync window).
    pub fn clear(&mut self) -> R<Vec<(u64, i64)>> {
        let now = now_secs();
        let tombs: Vec<(u64, i64)> = {
            let mut st = self.db.prepare("SELECT hash, created FROM items WHERE pinned = 0 ORDER BY id DESC LIMIT ?1").map_err(e)?;
            st.query_map([CLEAR_TOMBS as i64], |r| Ok((r.get::<_, i64>(0)? as u64, now.max(r.get(1)?)))).map_err(e)?.filter_map(Result::ok).collect()
        };
        let keep = self.collect_pinned_for_index()?;
        let tx = self.db.transaction().map_err(e)?;
        tx.execute("DELETE FROM items WHERE pinned = 0", []).map_err(e)?;
        for t in ["body", "blob", "ocr", "members"] {
            tx.execute(&format!("DELETE FROM {t} WHERE id NOT IN (SELECT id FROM items)"), []).map_err(e)?;
        }
        // rebuilding the index from the few pinned items beats deleting row by row
        tx.execute("INSERT INTO fts(fts) VALUES ('delete-all')", []).map_err(e)?;
        for (id, text, source) in &keep {
            fts_insert(&tx, *id, text, source.as_deref())?;
        }
        for (h, at) in &tombs {
            tx.execute("INSERT OR REPLACE INTO tomb(hash, at) VALUES (?1, ?2)", params![*h as i64, at]).map_err(e)?;
        }
        let total = keep.len() as u64;
        let images: u64 = tx.query_row("SELECT count(*) FROM items WHERE kind = 1", [], |r| r.get::<_, i64>(0)).map_err(e)? as u64;
        set_counters(&tx, total, images)?;
        tx.execute("UPDATE views SET count = (SELECT count(*) FROM members WHERE view = views.fp)", []).map_err(e)?;
        tx.commit().map_err(e)?;
        self.total = total;
        self.images = images;
        for (fp, v) in self.views.iter_mut() {
            v.count = self.db.query_row("SELECT count FROM views WHERE fp = ?1", [*fp as i64], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64;
        }
        Ok(tombs)
    }

    /// (id, indexed text, source) of the pinned items.
    fn collect_pinned_for_index(&self) -> R<Vec<(u64, String, Option<String>)>> {
        let mut st = self.db.prepare(&format!("{} WHERE i.pinned != 0", select(true))).map_err(e)?;
        let rows = st.query_map([], |r| Row::read(r, true)).map_err(e)?;
        Ok(rows.filter_map(Result::ok).map(|r| (r.meta.id, index_part(r.haystack()).to_string(), r.meta.source.clone())).collect())
    }

    /// Apply deletions made on another device. Items copied again after the
    /// deletion survive. Pinned items are never removed remotely.
    pub fn delete_synced(&mut self, deleted: &[(u64, i64)]) -> R<usize> {
        let mut ids = Vec::new();
        for (h, at) in deleted {
            if let Some((id, created, board)) = self.id_of_hash(*h)? {
                if board == 0 && created <= *at {
                    ids.push(id);
                }
            }
        }
        let n = ids.len();
        self.remove_ids(&ids)?;
        // Remember the deletions too, so we don't fetch them back from a third device.
        let tx = self.db.transaction().map_err(e)?;
        for (h, at) in deleted {
            tx.execute("INSERT INTO tomb(hash, at) VALUES (?1, ?2) ON CONFLICT(hash) DO UPDATE SET at = max(at, excluded.at)", params![*h as i64, at])
                .map_err(e)?;
        }
        tx.commit().map_err(e)?;
        Ok(n)
    }

    pub fn tombstones(&self) -> Vec<(u64, i64)> {
        let Ok(mut st) = self.db.prepare("SELECT hash, at FROM tomb") else { return Vec::new() };
        st.query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?))).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
    }

    /// Recent items offered to other devices: (hash, created), newest first.
    pub fn sync_inventory(&self, limit: usize, images: bool, max_image_bytes: u64) -> Vec<(u64, i64)> {
        let Ok(mut st) = self.db.prepare("SELECT hash, created FROM items WHERE kind = 0 OR (?1 AND bytes <= ?2) ORDER BY id DESC LIMIT ?3") else {
            return Vec::new();
        };
        st.query_map(params![images, max_image_bytes.min(i64::MAX as u64) as i64, limit as i64], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Which of a peer's items we don't have and haven't deleted since.
    pub fn sync_missing(&self, items: &[(u64, i64)]) -> Vec<u64> {
        items
            .iter()
            .filter(|(h, created)| {
                matches!(self.id_of_hash(*h), Ok(None)) && self.tomb_of(*h).ok().flatten().is_none_or(|at| at < *created)
            })
            .map(|(h, _)| *h)
            .collect()
    }

    /// Full content of the item with this hash, plus its timestamp.
    pub fn get_by_hash(&self, hash: u64) -> R<Option<(Content, i64)>> {
        let Some((id, created, _)) = self.id_of_hash(hash)? else { return Ok(None) };
        Ok(self.get(id)?.map(|c| (c, created)))
    }

    fn remove_ids(&mut self, ids: &[u64]) -> R<Vec<(u64, i64)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let now = now_secs();
        let tombs: Vec<(u64, i64)> = self.rows_by_ids(ids, false)?.iter().map(|r| (r.meta.hash, now.max(r.meta.created))).collect();
        let tx = self.db.transaction().map_err(e)?;
        for (h, at) in &tombs {
            tx.execute("INSERT OR REPLACE INTO tomb(hash, at) VALUES (?1, ?2)", params![*h as i64, at]).map_err(e)?;
        }
        let fps: Vec<u64> = self.views.keys().copied().collect();
        let mut d = Delta::new();
        unmember(&tx, &fps, ids, &mut d)?;
        let (n, ni) = delete_rows(&tx, ids)?;
        let (total, images) = (self.total - n, self.images - ni);
        set_counters(&tx, total, images)?;
        save_delta(&tx, &d)?;
        tx.commit().map_err(e)?;
        self.total = total;
        self.images = images;
        apply_delta(&mut self.views, d);
        Ok(tombs)
    }

    /// Pin into pin group 1, or unpin.
    pub fn toggle_pin(&mut self, id: u64) -> R<()> {
        let board: Option<i64> = self.db.query_row("SELECT pinned FROM items WHERE id = ?1", [id as i64], |r| r.get(0)).optional().map_err(e)?;
        match board {
            Some(b) => self.set_pin(id, if b == 0 { 1 } else { 0 }),
            None => Ok(()),
        }
    }

    /// Put the newest item (the current clipboard) into pin group `board`.
    /// Returns its id, `None` when the history is empty.
    pub fn pin_latest(&mut self, board: u32) -> R<Option<u64>> {
        let id = self.max_id()?;
        if id == 0 {
            return Ok(None);
        }
        self.set_pin(id, board)?;
        Ok(Some(id))
    }

    /// Put an item into pin group `board` (0 = unpin).
    pub fn set_pin(&mut self, id: u64, board: u32) -> R<()> {
        let tx = self.db.transaction().map_err(e)?;
        if tx.execute("UPDATE items SET pinned = ?2 WHERE id = ?1", params![id as i64, board as i64]).map_err(e)? == 0 {
            return Ok(());
        }
        let mut d = Delta::new();
        for row in fetch_rows(&tx, &[id], true)? {
            member(&tx, &self.views, &row, &mut d)?;
        }
        save_delta(&tx, &d)?;
        tx.commit().map_err(e)?;
        apply_delta(&mut self.views, d);
        Ok(())
    }

    /// Load many texts quickly (benchmarks, imports): one transaction per
    /// chunk, no dedup bookkeeping beyond skipping known hashes, no eviction.
    #[doc(hidden)]
    pub fn bulk_insert_texts(&mut self, texts: impl IntoIterator<Item = (String, i64, Option<String>)>) -> R<usize> {
        let mut max = self.max_id()?;
        let mut n = 0;
        let mut it = texts.into_iter().peekable();
        while it.peek().is_some() {
            let tx = self.db.transaction().map_err(e)?;
            let mut added = 0u64;
            for (t, created, source) in it.by_ref().take(20_000) {
                let hash = xxhash_rust::xxh3::xxh3_64_with_seed(t.as_bytes(), 1);
                let id = ((created.max(0) as u64) << ID_SHIFT).max(max + 1);
                let ins = tx
                    .prepare_cached(
                        "INSERT OR IGNORE INTO items(id, kind, created, pinned, hash, chars, lines, bytes, width, height, source)
                         VALUES (?1, 0, ?2, 0, ?3, ?4, ?5, ?6, 0, 0, ?7)",
                    )
                    .and_then(|mut st| {
                        st.execute(params![
                            id as i64,
                            created,
                            hash as i64,
                            t.chars().count() as i64,
                            t.lines().count().max(1) as i64,
                            t.len() as i64,
                            source
                        ])
                    })
                    .map_err(e)?;
                if ins == 0 {
                    continue;
                }
                max = id;
                tx.prepare_cached("INSERT INTO body(id, z) VALUES (?1, ?2)").and_then(|mut st| st.execute(params![id as i64, encode(&t)])).map_err(e)?;
                fts_insert(&tx, id, &t, source.as_deref())?;
                added += 1;
            }
            set_counters(&tx, self.total + added, self.images)?;
            tx.commit().map_err(e)?;
            self.total += added;
            n += added as usize;
        }
        self.reset_views(false)?;
        Ok(n)
    }
}

fn fts_insert(tx: &Transaction, id: u64, text: &str, source: Option<&str>) -> R<()> {
    tx.prepare_cached("INSERT INTO fts(rowid, text, source) VALUES (?1, ?2, ?3)")
        .and_then(|mut st| st.execute(params![id as i64, index_part(text), source.unwrap_or("")]))
        .map(|_| ())
        .map_err(e)
}

/// Delete items and everything attached; returns (items, images) deleted.
fn delete_rows(tx: &Transaction, ids: &[u64]) -> R<(u64, u64)> {
    let (mut n, mut ni) = (0, 0);
    for id in ids {
        let id = *id as i64;
        let kind: Option<i64> =
            tx.prepare_cached("DELETE FROM items WHERE id = ?1 RETURNING kind").and_then(|mut st| st.query_row([id], |r| r.get(0)).optional()).map_err(e)?;
        if let Some(k) = kind {
            n += 1;
            ni += (k == 1) as u64;
        }
        for sql in ["DELETE FROM body WHERE id = ?1", "DELETE FROM blob WHERE id = ?1", "DELETE FROM ocr WHERE id = ?1", "DELETE FROM fts WHERE rowid = ?1"] {
            tx.prepare_cached(sql).and_then(|mut st| st.execute([id])).map_err(e)?;
        }
    }
    Ok((n, ni))
}

fn set_counters(tx: &Transaction, total: u64, images: u64) -> R<()> {
    let mut st = tx.prepare_cached("INSERT OR REPLACE INTO info(key, value) VALUES (?1, ?2)").map_err(e)?;
    st.execute(params!["total", total as i64]).map_err(e)?;
    st.execute(params!["images", images as i64]).map_err(e)?;
    Ok(())
}

/// Rows by id, in the order given (missing ones skipped).
fn fetch_rows(db: &Connection, ids: &[u64], with_text: bool) -> R<Vec<Row>> {
    fetch_rows_raw(db, ids, with_text).map_err(e)
}

fn fetch_rows_raw(db: &Connection, ids: &[u64], with_text: bool) -> rusqlite::Result<Vec<Row>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let mut st = db.prepare(&format!("{} WHERE i.id IN ({list})", select(with_text)))?;
    let rows = st.query_map([], |r| Row::read(r, with_text))?.collect::<rusqlite::Result<Vec<Row>>>()?;
    let mut by_id: HashMap<u64, Row> = rows.into_iter().map(|r| (r.meta.id, r)).collect();
    Ok(ids.iter().filter_map(|i| by_id.remove(i)).collect())
}

/// Take items out of every view.
fn unmember(tx: &Transaction, fps: &[u64], ids: &[u64], d: &mut Delta) -> R<()> {
    for fp in fps {
        for id in ids {
            let n = tx
                .prepare_cached("DELETE FROM members WHERE view = ?1 AND id = ?2")
                .and_then(|mut st| st.execute(params![*fp as i64, *id as i64]))
                .map_err(e)?;
            *d.entry(*fp).or_default() -= n as i64;
        }
    }
    Ok(())
}

/// (Re)decide one item's membership in every view that has reached it.
fn member(tx: &Transaction, views: &HashMap<u64, View>, row: &Row, d: &mut Delta) -> R<()> {
    for (fp, v) in views {
        if row.meta.id < v.cursor {
            continue; // the builder gets there later
        }
        let n = if v.m.matches(row) {
            tx.prepare_cached("INSERT OR IGNORE INTO members(view, id) VALUES (?1, ?2)").and_then(|mut st| st.execute(params![*fp as i64, row.meta.id as i64])).map_err(e)? as i64
        } else {
            -(tx.prepare_cached("DELETE FROM members WHERE view = ?1 AND id = ?2").and_then(|mut st| st.execute(params![*fp as i64, row.meta.id as i64])).map_err(e)? as i64)
        };
        *d.entry(*fp).or_default() += n;
    }
    Ok(())
}

fn save_delta(tx: &Transaction, d: &Delta) -> R<()> {
    for (fp, n) in d.iter().filter(|(_, n)| **n != 0) {
        tx.prepare_cached("UPDATE views SET count = count + ?2 WHERE fp = ?1").and_then(|mut st| st.execute(params![*fp as i64, n])).map_err(e)?;
    }
    Ok(())
}

fn apply_delta(views: &mut HashMap<u64, View>, d: Delta) {
    for (fp, n) in d {
        if let Some(v) = views.get_mut(&fp) {
            v.count = (v.count as i64 + n).max(0) as u64;
        }
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
        let first = n[0];
        let (lo, up) = (first.to_ascii_lowercase(), first.to_ascii_uppercase());
        // jump between candidate first bytes (memchr-like), then compare
        return h[..=h.len() - n.len()]
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == lo || **b == up)
            .any(|(i, _)| h[i..i + n.len()].eq_ignore_ascii_case(n));
    }
    hay.to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests;
