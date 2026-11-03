//! Clips the recorder saves pile up fast - a minute of 1440p is a few hundred MB
//! - so they can be kept under a size limit, oldest deleted first. Only files the
//! recorder itself wrote count, listed in recorded.json: the raw folder may be
//! OBS's or Shadowplay's too, and those recordings are the user's business. Clips
//! cut from them (the clips folder) are never touched.
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::recorder::rec_log;
use crate::settings;

static RECORDED: Mutex<Vec<String>> = Mutex::new(Vec::new());
static STORAGE: Mutex<()> = Mutex::new(());

fn list_path() -> PathBuf {
    settings::app_dir().join("recorded.json")
}

fn save_locked(paths: &[String]) {
    if let Ok(b) = serde_json::to_vec(paths) {
        let tmp = list_path().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(tmp, list_path());
        }
    }
}

/// Adds a freshly saved clip, then enforces the limit.
pub fn note_recorded(path: &str) {
    {
        let mut r = RECORDED.lock().unwrap();
        r.push(path.to_string());
        save_locked(&r);
    }
    std::thread::spawn(enforce_limit);
}

pub struct RecordedFile {
    pub path: String,
    pub size: u64,
    modified: SystemTime,
}

/// Every listed clip still on disk, oldest first. Ones that are gone - deleted
/// after a cut, moved by hand - drop off the list.
pub fn recorded_files() -> Vec<RecordedFile> {
    let mut r = RECORDED.lock().unwrap();
    let mut files = Vec::new();
    let mut keep = Vec::new();
    for p in r.iter() {
        let Ok(m) = std::fs::metadata(p) else { continue };
        if m.is_dir() {
            continue;
        }
        keep.push(p.clone());
        files.push(RecordedFile {
            path: p.clone(),
            size: m.len(),
            modified: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    if keep.len() != r.len() {
        *r = keep;
        save_locked(&r);
    }
    files.sort_by_key(|f| f.modified);
    files
}

/// Deletes the oldest recorded clips until they fit under the limit. The newest
/// one always stays, however large.
fn enforce_limit() {
    let limit = settings::get().storage_limit_gb.max(0) as u64 * (1 << 30);
    if limit == 0 {
        return;
    }
    let _g = STORAGE.lock().unwrap();
    let files = recorded_files();
    let mut total: u64 = files.iter().map(|f| f.size).sum();
    for f in files.iter().take(files.len().saturating_sub(1)) {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(&f.path).is_ok() {
            total -= f.size;
            rec_log(&format!("storage limit: deleted {}", f.path));
        }
    }
    recorded_files(); // prune the deleted ones from the list
}

pub fn enforce_soon() {
    std::thread::spawn(enforce_limit);
}

/// Applies the limit at start and hourly, so lowering it takes effect without
/// waiting for the next clip.
pub fn watch() {
    if let Ok(b) = std::fs::read(list_path()) {
        if let Ok(v) = serde_json::from_slice::<Vec<String>>(&b) {
            *RECORDED.lock().unwrap() = v;
        }
    }
    loop {
        enforce_limit();
        std::thread::sleep(Duration::from_secs(3600));
    }
}
