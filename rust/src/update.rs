//! Update checks against this project's GitHub releases, at start and every few
//! hours. An install made by the setup exe updates itself: the new setup is
//! downloaded and run silently, and starts the app again when it is done. A
//! portable copy is pointed at the releases page instead.
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::job::{self, Ctx, Progress};

const RELEASES_API: &str = "https://api.github.com/repos/izgara/miyav/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/izgara/miyav/releases/download/";
pub const RELEASES_PAGE: &str = "https://github.com/izgara/miyav/releases/latest";

pub const APP_VERSION: &str = match option_env!("MIYAV_VERSION") {
    Some(v) => v,
    None => "dev",
};

/// Only connecting is bounded here; each request sets its own overall timeout.
static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    let tls = native_tls::TlsConnector::new().expect("Windows TLS");
    ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(tls))
        .timeout_connect(Duration::from_secs(20))
        .user_agent(concat!("miyav/", env!("CARGO_PKG_VERSION")))
        .build()
});

#[derive(Clone, Default)]
pub struct Latest {
    pub version: String,
    pub setup_url: String,
    pub setup_size: u64,
}

struct State {
    latest: Latest,
    checked: Option<Instant>,
    notified: String, // the version the tray has already announced
}

static UPDATE: LazyLock<Mutex<State>> =
    LazyLock::new(|| Mutex::new(State { latest: Latest::default(), checked: None, notified: String::new() }));

/// Whether a is a later x.y.z than b. "dev" builds never offer an update.
pub fn newer_version(a: &str, b: &str) -> bool {
    if b == "dev" || a.is_empty() {
        return false;
    }
    let num = |s: &str, i: usize| s.split('.').nth(i).and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);
    for i in 0..a.split('.').count().max(b.split('.').count()) {
        let (x, y) = (num(a, i), num(b, i));
        if x != y {
            return x > y;
        }
    }
    false
}

/// The version and setup exe of a release, as GitHub's API describes it. A setup
/// is only accepted from this project's own release downloads.
fn parse_release(release: &Value) -> Latest {
    let version = release["tag_name"].as_str().unwrap_or("").trim_start_matches('v').to_string();
    let setup = release["assets"].as_array().and_then(|assets| {
        assets.iter().find(|a| {
            a["name"].as_str().is_some_and(|n| n.ends_with("-setup.exe"))
                && a["browser_download_url"].as_str().is_some_and(|u| u.starts_with(DOWNLOAD_PREFIX))
        })
    });
    Latest {
        version,
        setup_url: setup.and_then(|a| a["browser_download_url"].as_str()).unwrap_or("").to_string(),
        setup_size: setup.and_then(|a| a["size"].as_u64()).unwrap_or(0),
    }
}

pub fn check(timeout: Duration) -> Result<(), String> {
    let res = AGENT
        .get(RELEASES_API)
        .set("accept", "application/vnd.github+json")
        .timeout(timeout)
        .call()
        .map_err(|e| format!("update check: {e}"))?;
    let mut raw = String::new();
    res.into_reader().take(1 << 20).read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let release: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let l = parse_release(&release);
    let announce;
    {
        let mut u = UPDATE.lock().unwrap();
        announce = newer_version(&l.version, APP_VERSION) && u.notified != l.version;
        if announce {
            u.notified = l.version.clone();
        }
        u.latest = l.clone();
        u.checked = Some(Instant::now());
    }
    if announce {
        crate::tray::balloon("Update available", &format!("Miyav {} is out. Open the app to update.", l.version));
    }
    Ok(())
}

/// Checks shortly after start, then every six hours.
pub fn watch() {
    std::thread::sleep(Duration::from_secs(15));
    loop {
        let _ = check(Duration::from_secs(30));
        std::thread::sleep(Duration::from_secs(6 * 3600));
    }
}

/// Installed by the setup exe, per user, so its setup can be run over it without
/// asking for admin rights.
pub fn self_updatable() -> bool {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else { return false };
    let exe = crate::ffmpeg::exe_path();
    exe.parent().is_some_and(|d| d.starts_with(Path::new(&local).join("Programs")))
}

pub fn latest() -> (Latest, bool) {
    let u = UPDATE.lock().unwrap();
    (u.latest.clone(), u.checked.is_some())
}

pub fn download_setup(ctx: &Ctx, l: &Latest) -> Result<PathBuf, String> {
    let name = l.setup_url.rsplit('/').next().unwrap_or("");
    let name = Path::new(name).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if name.is_empty() {
        return Err("the release has no setup to download".into());
    }
    let res = AGENT
        .get(&l.setup_url)
        .timeout(Duration::from_secs(900))
        .call()
        .map_err(|e| format!("downloading the update: {e}"))?;
    let total = if l.setup_size > 0 {
        l.setup_size
    } else {
        res.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0)
    };
    let out = std::env::temp_dir().join(name);
    let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
    let mut body = res.into_reader();
    let mut buf = vec![0u8; 256 << 10];
    let mut got = 0u64;
    loop {
        if ctx.done() {
            drop(f);
            let _ = std::fs::remove_file(&out);
            return Err("cancelled".into());
        }
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        if total > 0 {
            job::report(Progress { phase: "download".into(), percent: got as f64 * 100.0 / total as f64, ..Default::default() });
        }
    }
    drop(f);
    if l.setup_size > 0 && got != l.setup_size {
        let _ = std::fs::remove_file(&out);
        return Err("the update download was incomplete".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions() {
        assert!(newer_version("1.4.2", "1.4.1"));
        assert!(newer_version("1.10.0", "1.9.9"));
        assert!(!newer_version("1.4.1", "1.4.1"));
        assert!(!newer_version("1.4.0", "1.4.1"));
        assert!(newer_version("1.4.1", "1.4"));
        assert!(!newer_version("9.9.9", "dev"));
        assert!(!newer_version("", "1.0.0"));
    }

    #[test]
    fn reads_a_release() {
        let url = "https://github.com/izgara/miyav/releases/download/v1.2.0/Miyav-1.2.0-setup.exe";
        let l = parse_release(&json!({"tag_name": "v1.2.0", "assets": [
            {"name": "Miyav-1.2.0-portable.zip", "browser_download_url": "https://github.com/izgara/miyav/x.zip", "size": 5},
            {"name": "Miyav-1.2.0-setup.exe", "browser_download_url": url, "size": 9},
        ]}));
        assert_eq!((l.version.as_str(), l.setup_url.as_str(), l.setup_size), ("1.2.0", url, 9));
    }

    #[test]
    fn refuses_a_setup_from_anywhere_else() {
        let l = parse_release(&json!({"tag_name": "v1.2.0", "assets": [
            {"name": "Miyav-1.2.0-setup.exe", "browser_download_url": "https://example.com/Miyav-1.2.0-setup.exe", "size": 9},
        ]}));
        assert_eq!(l.setup_url, "");
    }
}
