//! Wire messages (postcard encoded, always inside the encrypted channel).

use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ItemKind {
    Text,
    Image,
}

/// One clipboard entry as exchanged between devices.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncItem {
    /// Content hash (identity of the item across devices).
    pub hash: u64,
    /// Unix seconds when it was copied.
    pub created: i64,
    pub kind: ItemKind,
    /// UTF-8 text, or PNG bytes.
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Msg {
    Hello { device_id: [u8; 16], name: String, proto: u32 },
    /// Recent items (hash, created) and recently deleted hashes (hash, deleted_at).
    Inventory { items: Vec<(u64, i64)>, deleted: Vec<(u64, i64)> },
    Want(Vec<u64>),
    Item(SyncItem),
    Delete(Vec<(u64, i64)>),
    Ping,
}

/// Sent by the inviting device once the pairing handshake succeeded.
#[derive(Debug, Serialize, Deserialize)]
pub struct PairOffer {
    pub group_key: [u8; 32],
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PairAck {
    pub name: String,
}
