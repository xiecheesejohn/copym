//! Location and loading of `groups.toml` (next to `config.toml`). The group
//! model itself lives in `copym_core::groups`.

pub use copym_core::groups::{GroupDef, Groups};
use std::path::PathBuf;

pub fn path() -> PathBuf {
    crate::config::config_path().with_file_name("groups.toml")
}

/// Load the groups; writes the defaults on first run. The error (if any)
/// describes a broken file - the defaults are used in that case.
pub fn load() -> (Groups, Option<String>) {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(s) => match copym_core::groups::from_toml(&s) {
            Ok(g) => (g, None),
            Err(e) => (Groups::default(), Some(format!("{} 有误，已使用默认分组：{e}", path.display()))),
        },
        Err(_) => {
            let g = Groups::default();
            let _ = save(&g);
            (g, None)
        }
    }
}

pub fn save(g: &Groups) -> Result<(), String> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, copym_core::groups::to_toml(g)?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}
