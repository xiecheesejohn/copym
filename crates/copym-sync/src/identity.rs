//! Device identity and sync group membership, persisted in a private file.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
pub struct Identity {
    pub device_id: [u8; 16],
    /// Shared secret of the sync group. `None` = not paired with anything.
    pub group_key: Option<[u8; 32]>,
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random number generator unavailable");
    b
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

impl Identity {
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(id) = postcard::from_bytes::<Identity>(&bytes) {
                return Ok(id);
            }
        }
        let id = Identity { device_id: random(), group_key: None };
        id.save(path)?;
        Ok(id)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = postcard::to_stdvec(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("tmp");
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            use std::io::Write;
            let mut f = opts.open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(tmp, path)
    }

    pub fn device_hex(&self) -> String {
        hex(&self.device_id)
    }

    /// Public, non-secret identifier of the group (advertised over mDNS so
    /// devices only try to connect to members of their own group).
    pub fn group_id(&self) -> Option<String> {
        self.group_key.map(|k| {
            let mut h = Sha256::new();
            h.update(b"copym-group-id-v1");
            h.update(k);
            hex(&h.finalize()[..8])
        })
    }
}
