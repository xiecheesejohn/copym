//! Shared data types: clipboard content, item metadata and the IPC protocol.

use serde::{Deserialize, Serialize};

/// A captured clipboard payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// Image is always normalised to PNG bytes.
    Image { png: Vec<u8>, width: u32, height: u32 },
}

impl Content {
    pub fn kind(&self) -> Kind {
        match self {
            Content::Text(_) => Kind::Text,
            Content::Image { .. } => Kind::Image,
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            Content::Text(t) => t.len(),
            Content::Image { png, .. } => png.len(),
        }
    }

    pub fn hash(&self) -> u64 {
        use xxhash_rust::xxh3::xxh3_64_with_seed;
        match self {
            Content::Text(t) => xxh3_64_with_seed(t.as_bytes(), 1),
            Content::Image { png, .. } => xxh3_64_with_seed(png, 2),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
}

/// Lightweight description of a history entry (no full payload).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemMeta {
    pub id: u64,
    pub kind: Kind,
    /// Text: first part of the text. Image: empty.
    pub preview: String,
    pub chars: u32,
    pub lines: u32,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    /// Unix seconds of the last time it was copied.
    pub created: i64,
    pub pinned: bool,
    pub source: Option<String>,
    pub hash: u64,
    /// Text recognized in an image (None = not processed yet / not an image).
    pub ocr: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Filter {
    All,
    Text,
    Image,
    Pinned,
}

/// Rules of a history group (a saved filter). Every set condition must hold.
/// Regexes run against the item's text (or the text recognized in an image).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupRule {
    /// `None` = text and images.
    pub kind: Option<Kind>,
    /// Only pinned items.
    pub pinned: bool,
    /// At least one must match (empty = no constraint).
    pub patterns: Vec<String>,
    /// Source application must match at least one (empty = no constraint).
    pub apps: Vec<String>,
    /// None of these may match the content / the source app ("other" groups).
    pub exclude: Vec<String>,
    pub exclude_apps: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// `regex`: treat `query` as a (case-insensitive) regular expression.
    List { query: String, filter: Filter, regex: bool, limit: u32 },
    Get { id: u64 },
    Thumb { id: u64 },
    /// Put the item back on the system clipboard (and move it to the top).
    Activate { id: u64 },
    Delete { id: u64 },
    TogglePin { id: u64 },
    /// Remove every non-pinned item.
    Clear,
    /// `None` toggles.
    SetPaused(Option<bool>),
    Status,
    /// Open the popup, or close it when already open.
    /// Carries the caller's `XDG_ACTIVATION_TOKEN` so Wayland compositors focus the window.
    ToggleGui { activation_token: Option<String> },
    Quit,
    SyncStatus,
    /// Open a pairing window and return a one-time code.
    SyncPair,
    /// Join another device's group. `addr` ("ip:port") skips mDNS discovery.
    SyncJoin { code: String, addr: Option<String> },
    SyncLeave,
    /// List the items of a group (`None` = everything).
    ListGroup { query: String, regex: bool, group: Option<GroupRule>, limit: u32 },
    /// Number of items in each group for the current search; `None` = invalid rule.
    GroupCounts { query: String, regex: bool, groups: Vec<GroupRule> },
    /// Forget all OCR results and recognize every image again.
    RedoOcr,
    /// Send the paste shortcut to the focused window (after the popup closed).
    Paste,
    /// Replace `ignore_apps`: saved to config.toml and used right away.
    SetIgnoredApps { apps: Vec<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncInfo {
    pub enabled: bool,
    pub paired: bool,
    pub device_name: String,
    pub device_id: String,
    pub port: u16,
    /// (name, address)
    pub peers: Vec<(String, String)>,
    pub pairing_code: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Items(Vec<ItemMeta>),
    Content(Option<Content>),
    /// PNG encoded thumbnail.
    Thumb(Option<Vec<u8>>),
    Status { paused: bool, count: usize, backend: String, ocr: Option<String> },
    Sync(SyncInfo),
    Pairing { code: String, addrs: Vec<String>, expires_secs: u64 },
    Joined { name: String },
    Counts(Vec<Option<u32>>),
    Ok,
    Err(String),
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
