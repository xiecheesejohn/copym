//! Update check: the latest GitHub Release of copym, fetched with the system
//! `curl` (macOS, Windows 10+ and most Linux systems have it), so no TLS stack
//! is linked in. Only the version and the release page are used; the user
//! downloads and installs it.

use copym_core::model::UpdateInfo;

const LATEST: &str = "https://api.github.com/repos/xiecheesejohn/copym/releases/latest";

/// The latest release, if it is newer than this build.
pub fn check() -> Result<Option<UpdateInfo>, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "20", "-H", "Accept: application/vnd.github+json", "-A"])
        .arg(concat!("copym/", env!("CARGO_PKG_VERSION")))
        .arg(LATEST);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash from the daemon
    }
    let out = cmd.output().map_err(|e| tf!("无法运行 curl：{e}", "cannot run curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("404") {
            // a private repository, or no release published yet
            return Err(t!("检查更新失败：没有找到已发布的版本", "update check failed: no published release found").into());
        }
        return Err(tf!("检查更新失败：{}", "update check failed: {}", err.trim().trim_start_matches("curl: ")));
    }
    parse(&out.stdout, env!("CARGO_PKG_VERSION"))
}

fn parse(body: &[u8], current: &str) -> Result<Option<UpdateInfo>, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| tf!("检查更新失败：{e}", "update check failed: {e}"))?;
    let tag = v["tag_name"].as_str().ok_or(t!("检查更新失败：没有版本号", "update check failed: no version"))?;
    let url = v["html_url"].as_str().unwrap_or("https://github.com/xiecheesejohn/copym/releases/latest");
    let version = tag.trim_start_matches('v').to_string();
    Ok(newer(&version, current).then(|| UpdateInfo { version, url: url.to_string() }))
}

/// `a` > `b` for "x.y.z" versions (a pre-release suffix such as "-rc1" is ignored).
fn newer(a: &str, b: &str) -> bool {
    let nums = |v: &str| -> Vec<u64> { v.split(['-', '+']).next().unwrap_or("").split('.').map(|n| n.parse().unwrap_or(0)).collect() };
    let (a, b) = (nums(a), nums(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(!newer("0.1.0-rc1", "0.1.0"));
    }

    #[test]
    fn release_json() {
        let body = br#"{"tag_name":"v0.2.0","html_url":"https://github.com/xiecheesejohn/copym/releases/tag/v0.2.0","assets":[]}"#;
        let u = parse(body, "0.1.0").unwrap().unwrap();
        assert_eq!((u.version.as_str(), u.url.as_str()), ("0.2.0", "https://github.com/xiecheesejohn/copym/releases/tag/v0.2.0"));
        assert!(parse(body, "0.2.0").unwrap().is_none());
        assert!(parse(b"{}", "0.1.0").is_err());
    }
}
