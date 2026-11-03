//! The local HTTP service the page talks to, so
//! the front end in ui/ does not know or care what is behind it.
use include_dir::{include_dir, Dir};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::job::{self, Ctx};
use crate::settings::{self, QUALITY_PRESETS};
use crate::{ffmpeg, games, recorder, storage, update, win, window};

static UI: Dir = include_dir!("$CARGO_MANIFEST_DIR/../ui");

/// What a handler answers with.
pub enum Reply {
    Json(u16, Value),
    /// A file, honouring Range: the <video> scrubber needs it.
    File(PathBuf, &'static str, &'static str),
    Bytes(u16, Vec<u8>, String, &'static str),
    Text(u16, &'static str),
}

fn ok(v: Value) -> Reply {
    Reply::Json(200, v)
}

fn err(code: u16, msg: impl std::fmt::Display) -> Reply {
    Reply::Json(code, json!({"error": msg.to_string()}))
}

pub struct Req {
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: Value,
    pub method: Method,
    pub range: Option<String>,
}

const CANCELLED: &str = "Cancelled.";
const OUTSIDE: &str = "that file is outside your configured folders";

impl Req {
    fn q(&self, k: &str) -> String {
        self.query.get(k).cloned().unwrap_or_default()
    }
    fn s(&self, k: &str) -> String {
        self.body[k].as_str().unwrap_or("").to_string()
    }
}

/// Keeps /media from being turned into a read-anything endpoint: only files
/// inside the two configured folders can be streamed.
pub fn allowed_path(p: &str) -> bool {
    if p.is_empty() {
        return false;
    }
    let s = settings::get();
    let Ok(abs) = std::path::absolute(p) else { return false };
    if abs.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return false;
    }
    let abs = abs.display().to_string().to_lowercase();
    [s.raw_folder, s.clips_folder].iter().any(|root| {
        if root.trim().is_empty() {
            return false;
        }
        let Ok(r) = std::path::absolute(root) else { return false };
        let r = r.display().to_string().to_lowercase();
        abs == r || abs.starts_with(&format!("{}\\", r.trim_end_matches('\\')))
    })
}

// ---- local clip listing ----

#[derive(Serialize)]
struct LocalFile {
    path: String,
    name: String,
    size: u64,
    modified: i64,
}

fn modified_secs(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Walks the folder tree and buckets files by their immediate parent: OBS and
/// Shadowplay both write a subfolder per game, so the top level of the Videos
/// folder is usually empty.
fn scan_videos(root: &str) -> Result<Value, String> {
    if root.trim().is_empty() {
        return Ok(json!([]));
    }
    let root = Path::new(root);
    if !root.exists() {
        return Err(format!("could not open {}", root.display()));
    }
    let mut buckets: HashMap<String, Vec<LocalFile>> = HashMap::new();
    let mut dirs: HashMap<String, String> = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // Unreadable folders are skipped rather than aborting the scan.
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !["mp4", "mkv", "mov", "avi", "webm", "flv", "m4v"].contains(&ext.as_str()) {
                continue;
            }
            let label = if dir == root {
                "Loose files".to_string()
            } else {
                dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            };
            dirs.entry(label.clone()).or_insert_with(|| dir.display().to_string());
            buckets.entry(label).or_default().push(LocalFile {
                path: p.display().to_string(),
                name: e.file_name().to_string_lossy().to_string(),
                size: m.len(),
                modified: modified_secs(&m),
            });
        }
    }
    let mut groups: Vec<(String, Vec<LocalFile>)> = buckets.into_iter().collect();
    for (_, files) in &mut groups {
        files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    }
    // newest activity first, so the game you just played is at the top
    groups.sort_by_key(|(_, f)| std::cmp::Reverse(f[0].modified));
    let out: Vec<Value> = groups
        .into_iter()
        .map(|(name, files)| {
            let dir = dirs.get(&name).cloned().unwrap_or_default();
            json!({"name": name, "dir": dir, "files": files})
        })
        .collect();
    Ok(Value::Array(out))
}

// ---- paths for new clips ----

/// Avoids silently clobbering an existing clip.
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{stem}{ext}"));
    let mut i = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({i}){ext}"));
        i += 1;
    }
    p
}

pub fn sanitize_stem(name: &str) -> String {
    let stem = match name.rsplit_once('.') {
        Some((s, ext)) if !ext.contains(' ') && ext.len() <= 5 => s,
        _ => name,
    };
    let mut s: String = stem.chars().map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    if s.len() > 80 {
        let mut i = 80;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        s.truncate(i);
    }
    if s.trim().is_empty() {
        s = "clip".into();
    }
    s
}

fn base(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

// ---- jobs ----

fn job_fail(ctx: &Ctx, code: u16, e: String, when_cancelled: &str) -> Reply {
    if ctx.cancelled() {
        return err(499, when_cancelled);
    }
    job::set(|s| s.error = e.clone());
    err(code, e)
}

/// Cuts the selected range out into the Clips folder. Runs synchronously so the UI
/// gets one clear success or one clear error, with progress on /api/job.
fn cut(r: &Req) -> Reply {
    let path = r.s("path");
    if !allowed_path(&path) {
        return err(403, OUTSIDE);
    }
    let s = settings::get();
    if s.clips_folder.trim().is_empty() {
        return err(400, "no clips folder is configured");
    }
    let clips = PathBuf::from(&s.clips_folder);
    if let Err(e) = std::fs::create_dir_all(&clips) {
        return err(500, e);
    }
    let input = PathBuf::from(&path);
    let out = unique_path(&clips, &format!("{}_clip", sanitize_stem(&base(&input))), ".mp4");
    let (ctx, _done) = job::begin(&format!("Cutting {}", base(&out)), Duration::from_secs(1800));

    let (start, end) = (r.body["start"].as_f64().unwrap_or(0.0), r.body["end"].as_f64().unwrap_or(0.0));
    let audio = r.body["audio"].as_i64().unwrap_or(0) as i32;
    if let Err(e) = ffmpeg::cut_lossless(&ctx, &input, &out, start, end, audio, &job::report) {
        // Whatever ffmpeg got as far as writing is not a usable clip.
        let _ = std::fs::remove_file(&out);
        return job_fail(&ctx, 500, e, CANCELLED);
    }
    ok(json!({"name": base(&out), "path": out.display().to_string()}))
}

/// Shrink: re-encode a file at the configured preset, into the Clips folder.
fn shrink(r: &Req) -> Reply {
    let path = r.s("path");
    if !allowed_path(&path) {
        return err(403, OUTSIDE);
    }
    let s = settings::get();
    let input = PathBuf::from(&path);
    // Each pass costs quality, and a second one has nothing to give back for it.
    if sanitize_stem(&base(&input)).ends_with("_small") {
        return ok(json!({"kept": true}));
    }
    let dir = if s.clips_folder.trim().is_empty() {
        input.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        PathBuf::from(&s.clips_folder)
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(500, e);
    }
    let out = unique_path(&dir, &format!("{}_small", sanitize_stem(&base(&input))), ".mp4");
    let (ctx, _done) = job::begin(&format!("Shrinking {}", base(&input)), Duration::from_secs(1800));
    match ffmpeg::encode(&ctx, &input, &out, &s.quality, &job::report) {
        Ok(true) => {}
        Ok(false) => return ok(json!({"kept": true})),
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            return job_fail(&ctx, 500, e, CANCELLED);
        }
    }
    let size = out.metadata().map(|m| m.len()).unwrap_or(0);
    ok(json!({"name": base(&out), "path": out.display().to_string(), "size": size}))
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_lowercase().trim_start_matches('.') {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "flv" => "video/x-flv",
        _ => "application/octet-stream",
    }
}

// ---- thumbnails ----

// one lock per cache file so two requests never encode the same frame twice
static THUMB_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Grabs a frame a few seconds in: frame 0 of a game clip is very often a black
/// fade-in, which makes for a useless thumbnail.
fn make_thumb(src: &str, dest: &Path) -> bool {
    if let Some(d) = dest.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    ["3", "0"].iter().any(|seek| {
        let st = ffmpeg::command()
            .args(["-y", "-hide_banner", "-loglevel", "error", "-ss", seek, "-i", src])
            .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "5"])
            .arg(dest)
            .output();
        st.is_ok_and(|o| o.status.success()) && dest.metadata().map(|m| m.len() > 0).unwrap_or(false)
    })
}

fn thumb(r: &Req) -> Reply {
    let p = r.q("path");
    if !allowed_path(&p) {
        return Reply::Text(403, "forbidden");
    }
    let Ok(m) = std::fs::metadata(&p) else { return Reply::Text(404, "not found") };
    let key = sha1_smol::Sha1::from(format!("{p}|{}", modified_secs(&m))).digest().to_string();
    let cache = settings::app_dir().join("thumbs").join(format!("{key}.jpg"));
    if !cache.exists() {
        let lock = THUMB_LOCKS.lock().unwrap().entry(cache.clone()).or_default().clone();
        let _g = lock.lock().unwrap();
        if !cache.exists() && !make_thumb(&p, &cache) {
            return Reply::Text(500, "could not make a thumbnail");
        }
    }
    Reply::File(cache, "image/jpeg", "public, max-age=86400")
}

// ---- settings ----

fn settings_get() -> Reply {
    let s = settings::get();
    ok(json!({
        "rawFolder": s.raw_folder, "clipsFolder": s.clips_folder,
        "quality": s.quality, "setupDone": s.setup_done,
        "alwaysDeleteSource": s.always_delete_source,
        "presets": &*QUALITY_PRESETS,
        "recordEnabled": s.record_enabled, "recordMode": s.record_mode,
        "replaySeconds": s.replay_seconds,
        "recordFps": s.record_fps, "recordQuality": s.record_quality,
        "recordEncoder": s.record_encoder, "recordCodec": s.record_codec,
        "recordMic": s.record_mic, "hotkey": s.hotkey,
        "replayInMemory": s.replay_in_memory, "autoClip": s.auto_clip,
        "startWithWindows": win::autostart_enabled(),
        "storageLimitGB": s.storage_limit_gb, "quietSaves": s.quiet_saves,
        "version": update::APP_VERSION,
    }))
}

fn settings_post(r: &Req) -> Reply {
    let b = &r.body;
    let before = settings::get();
    settings::update(|s| {
        let st = |k: &str, f: &mut String| {
            if let Some(v) = b[k].as_str() {
                *f = v.to_string();
            }
        };
        let bo = |k: &str, f: &mut bool| {
            if let Some(v) = b[k].as_bool() {
                *f = v;
            }
        };
        let i3 = |k: &str, f: &mut i32| {
            if let Some(v) = b[k].as_i64() {
                *f = v as i32;
            }
        };
        st("rawFolder", &mut s.raw_folder);
        st("clipsFolder", &mut s.clips_folder);
        if let Some(v) = b["quality"].as_str().filter(|v| QUALITY_PRESETS.contains_key(v)) {
            s.quality = v.to_string();
        }
        bo("setupDone", &mut s.setup_done);
        bo("alwaysDeleteSource", &mut s.always_delete_source);
        bo("recordEnabled", &mut s.record_enabled);
        st("recordMode", &mut s.record_mode);
        i3("replaySeconds", &mut s.replay_seconds);
        i3("recordFps", &mut s.record_fps);
        i3("recordQuality", &mut s.record_quality);
        st("recordEncoder", &mut s.record_encoder);
        st("recordCodec", &mut s.record_codec);
        bo("replayInMemory", &mut s.replay_in_memory);
        bo("autoClip", &mut s.auto_clip);
        bo("recordMic", &mut s.record_mic);
        if let Some(v) = b["hotkey"].as_str() {
            s.hotkey = v.trim().to_string();
        }
        if let Some(v) = b["storageLimitGB"].as_i64() {
            s.storage_limit_gb = v;
        }
        bo("quietSaves", &mut s.quiet_saves);
        settings::clamp_record_settings(s);
    });
    // The suggested folders do not exist on a fresh PC.
    for k in ["rawFolder", "clipsFolder"] {
        if let Some(d) = b[k].as_str().filter(|d| !d.trim().is_empty()) {
            let _ = std::fs::create_dir_all(d);
        }
    }
    if b["storageLimitGB"].is_number() {
        storage::enforce_soon();
    }
    if let Err(e) = settings::save() {
        return err(500, e);
    }
    if let Some(on) = b["startWithWindows"].as_bool() {
        if let Err(e) = win::set_autostart(on) {
            return err(500, e);
        }
    }
    if let Err(e) = recorder::apply_settings(&before, &settings::get()) {
        // Put back the recording settings that worked, rather than saving ones
        // that leave recording silently off at the next launch.
        recorder::restore_settings(&before);
        return err(400, e);
    }
    ok(json!({"ok": true}))
}

// ---- the PC scan ----

/// What this machine has, and the recording settings that suit it. Run from setup
/// and from the Recording page; applying the result is a separate, explicit step.
fn scan_pc() -> Value {
    let (w, h, hz) = win::primary_display();
    let dir = recorder::replay_dir();
    let _ = std::fs::create_dir_all(&dir);
    let free = win::free_disk_gb(&dir);
    let mut games = games::game_library();
    games.sort_by(|a, b| a.name.cmp(&b.name));
    let enc = recorder::pick_encoder("");
    let (mut hardware, mut encoder, mut enc_err) = (false, String::new(), None);
    match enc {
        Ok(e) => {
            encoder = e.name.to_string();
            hardware = !e.download || e.name == "h264_mf";
        }
        Err(e) => enc_err = Some(e),
    }
    // x264 at 60 fps on a 1440p desktop costs a game real frames.
    let (mut fps, quality) = if hardware { (60, 23) } else { (30, 25) };
    if hz > 0 && (hz as i32) < fps {
        fps = hz as i32;
    }
    // The buffer needs a few hundred MB; leave a nearly full drive alone.
    let replay = if free > 0.0 && free < 5.0 { 30 } else { 60 };
    let mut v = json!({
        "gpus": win::gpu_names(), "encoder": encoder, "hardware": hardware,
        "screen": {"width": w, "height": h, "hz": hz},
        "cpuThreads": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "ramGB": win::total_ram_gb(), "freeGB": free, "games": games,
        // "" keeps choosing at start, which survives a GPU swap
        "recommended": {"recordEncoder": "", "recordFps": fps, "recordQuality": quality, "replaySeconds": replay},
    });
    if let Some(e) = enc_err {
        v["encoderError"] = e.into();
    }
    v
}

// ---- routes ----

fn route(r: &Req) -> Reply {
    match r.path.as_str() {
        "/api/settings" if r.method == Method::Get => settings_get(),
        "/api/settings" => settings_post(r),
        "/api/local" => {
            let s = settings::get();
            let dir = if r.q("kind") == "clips" { s.clips_folder } else { s.raw_folder };
            match scan_videos(&dir) {
                Ok(v) => ok(v),
                Err(e) => err(400, e),
            }
        }
        "/api/probe" => {
            let p = r.q("path");
            if !allowed_path(&p) {
                return err(403, OUTSIDE);
            }
            match ffmpeg::probe(Path::new(&p)) {
                Ok(i) => ok(serde_json::to_value(i).unwrap()),
                Err(e) => err(400, e),
            }
        }
        "/api/job" => ok(serde_json::to_value(job::snapshot()).unwrap()),
        "/api/job/cancel" => ok(json!({"cancelled": job::stop()})),
        "/media" => {
            let p = r.q("path");
            if !allowed_path(&p) {
                return Reply::Text(403, "forbidden");
            }
            let path = PathBuf::from(p);
            if !path.exists() {
                return Reply::Text(404, "not found");
            }
            let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            Reply::File(path, mime_for_ext(&ext), "")
        }
        "/api/cut" => cut(r),
        "/api/shrink" => shrink(r),
        "/api/thumb" => thumb(r),
        // Permanent delete. There is deliberately no trash folder.
        "/api/local/delete" => {
            let p = r.s("path");
            if !allowed_path(&p) {
                return err(403, OUTSIDE);
            }
            match std::fs::remove_file(&p) {
                Ok(_) => ok(json!({"ok": true})),
                Err(e) => err(500, format!("could not delete the file: {e}")),
            }
        }
        "/api/reveal" => {
            let p = r.s("path");
            if !allowed_path(&p) {
                return err(403, OUTSIDE);
            }
            win::reveal_in_explorer(&p);
            ok(json!({"ok": true}))
        }
        "/api/window" => match window::action(&r.q("do")) {
            Ok(m) => ok(json!({"maximized": m})),
            Err(e) => err(if e == "unknown window action" { 400 } else { 500 }, e),
        },
        "/api/pick-folder" => {
            let title = r.s("title");
            let title = if title.is_empty() { "Choose a folder".to_string() } else { title };
            let path = win::pick_folder(&title, &r.s("initial"), window::hwnd());
            ok(json!({"cancelled": path.is_empty(), "path": path}))
        }
        "/api/rec" => {
            let mut st = recorder::status();
            st.open = recorder::take_open_request();
            ok(serde_json::to_value(st).unwrap())
        }
        "/api/rec/start" => {
            if let Err(e) = recorder::set_enabled(true) {
                let _ = recorder::set_enabled(false);
                return err(400, e);
            }
            ok(serde_json::to_value(recorder::status()).unwrap())
        }
        "/api/rec/stop" => match recorder::set_enabled(false) {
            Ok(_) => ok(serde_json::to_value(recorder::status()).unwrap()),
            Err(e) => err(500, e),
        },
        "/api/rec/save" => {
            let secs = r.body["seconds"].as_i64().filter(|&s| s > 0).unwrap_or(settings::get().replay_seconds as i64);
            match recorder::save_clip(secs as i32, None) {
                Ok(out) => ok(json!({"name": base(Path::new(&out)), "path": out})),
                Err(e) => err(500, e),
            }
        }
        "/api/rec/scan" => ok(scan_pc()),
        "/api/rec/games" => {
            let mut games = games::game_library();
            for a in settings::get().custom_apps {
                games.push(games::GameEntry { name: a.name, dir: a.exe, store: games::STORE_ADDED.into() });
            }
            ok(serde_json::to_value(games).unwrap())
        }
        "/api/rec/apps" if r.method == Method::Get => ok(serde_json::to_value(recorder::running_apps()).unwrap()),
        "/api/rec/apps" => {
            let res = match r.s("remove") {
                rem if !rem.is_empty() => games::remove_custom_app(&rem),
                _ => games::add_custom_app(&r.s("add")),
            };
            match res {
                Ok(_) => ok(serde_json::to_value(settings::get().custom_apps).unwrap()),
                Err(e) => err(400, e),
            }
        }
        "/api/update" => {
            let (mut l, checked) = update::latest();
            if !r.q("check").is_empty() || !checked {
                if update::check(Duration::from_secs(20)).is_ok() {
                    l = update::latest().0;
                }
            }
            ok(json!({
                "current": update::APP_VERSION, "latest": l.version,
                "available": update::newer_version(&l.version, update::APP_VERSION),
                "auto": update::self_updatable(),
            }))
        }
        "/api/update/install" => {
            let l = update::latest().0;
            if !update::newer_version(&l.version, update::APP_VERSION) || l.setup_url.is_empty() {
                return err(400, "there is no update to install");
            }
            if !update::self_updatable() {
                win::open_browser(update::RELEASES_PAGE);
                return ok(json!({"opened": true}));
            }
            let (ctx, done) = job::begin(&format!("Downloading update {}", l.version), Duration::from_secs(900));
            let setup = match update::download_setup(&ctx, &l) {
                Ok(s) => s,
                Err(e) => return job_fail(&ctx, 500, e.clone(), &e),
            };
            drop(done);
            // /S: silent. The installer closes this app, replaces it and starts
            // the new one (see miyav.nsi).
            if let Err(e) = std::process::Command::new(setup).arg("/S").spawn() {
                return err(500, format!("could not start the update: {e}"));
            }
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                window::quit_app();
            });
            ok(json!({"installing": true}))
        }
        "/api/storage" => {
            let files = storage::recorded_files();
            let total: u64 = files.iter().map(|f| f.size).sum();
            ok(json!({"bytes": total, "count": files.len(), "limitGB": settings::get().storage_limit_gb}))
        }
        p if p.starts_with("/api/") => err(404, "no such route"),
        p => static_file(p),
    }
}

fn static_file(p: &str) -> Reply {
    let rel = p.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let Some(f) = UI.get_file(rel) else { return Reply::Text(404, "not found") };
    let ct = match rel.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    Reply::Bytes(200, f.contents().to_vec(), ct.to_string(), "")
}

pub fn app_icon() -> &'static [u8] {
    UI.get_file("app.ico").map(|f| f.contents()).unwrap_or(&[])
}

// ---- HTTP plumbing ----

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("a valid header")
}

/// "bytes=a-b" against a file of this size: start and length.
pub fn parse_range(spec: &str, size: u64) -> Option<(u64, u64)> {
    let (a, b) = spec.strip_prefix("bytes=")?.split(',').next()?.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        let n = n.min(size);
        return (n > 0).then_some((size - n, n));
    }
    let start: u64 = a.parse().ok()?;
    if start >= size {
        return None;
    }
    let end = if b.is_empty() { size - 1 } else { b.parse::<u64>().ok()?.min(size - 1) };
    (end >= start).then_some((start, end - start + 1))
}

fn answer(req: tiny_http::Request, reply: Reply, range: Option<String>) {
    let _ = match reply {
        Reply::Json(code, v) => req.respond(
            Response::from_string(v.to_string())
                .with_status_code(code)
                .with_header(header("content-type", "application/json")),
        ),
        Reply::Text(code, t) => req.respond(Response::from_string(t).with_status_code(code)),
        Reply::Bytes(code, b, ct, cache) => {
            let mut res = Response::from_data(b).with_status_code(code);
            if !ct.is_empty() {
                res = res.with_header(header("content-type", &ct));
            }
            if !cache.is_empty() {
                res = res.with_header(header("cache-control", cache));
            }
            req.respond(res)
        }
        Reply::File(path, ct, cache) => {
            let Ok(mut f) = std::fs::File::open(&path) else {
                let _ = req.respond(Response::from_string("not found").with_status_code(404));
                return;
            };
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            let mut headers = vec![header("content-type", ct), header("accept-ranges", "bytes")];
            if !cache.is_empty() {
                headers.push(header("cache-control", cache));
            }
            let (status, start, len) = match range.as_deref().map(|r| parse_range(r, size)) {
                None => (200, 0, size),
                Some(Some((s, n))) => {
                    headers.push(header("content-range", &format!("bytes {s}-{}/{size}", s + n - 1)));
                    (206, s, n)
                }
                Some(None) => {
                    headers.push(header("content-range", &format!("bytes */{size}")));
                    let _ = req.respond(Response::empty(416).with_header(headers.pop().unwrap()));
                    return;
                }
            };
            if f.seek(SeekFrom::Start(start)).is_err() {
                let _ = req.respond(Response::empty(500));
                return;
            }
            req.respond(Response::new(StatusCode(status), headers, f.take(len), Some(len as usize), None))
        }
    };
}

fn parse_query(q: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(q.as_bytes()).into_owned().collect()
}

/// Only the app's own page may use this service. It is on loopback, but any web page
/// open in a browser can send requests to loopback too, and could otherwise change
/// the folders and delete files. The Host check stops DNS rebinding; the other two
/// stop requests from other origins, which browsers always label.
fn from_own_page(own_host: &str, host: Option<&str>, origin: Option<&str>, fetch_site: Option<&str>) -> bool {
    host == Some(own_host)
        && origin.is_none_or(|o| o.strip_prefix("http://") == Some(own_host))
        && fetch_site.is_none_or(|s| s == "same-origin" || s == "none")
}

/// Binds to a random loopback port so nothing is reachable off-machine.
pub fn start() -> Result<String, String> {
    let server = Server::http("127.0.0.1:0").map_err(|e| e.to_string())?;
    let addr = format!("http://{}", server.server_addr());
    let own_host = server.server_addr().to_string();
    std::thread::spawn(move || {
        // A thread per request: a cut answers only when it is done,
        // and video streams for as long as the player is open.
        for mut req in server.incoming_requests() {
            let own_host = own_host.clone();
            std::thread::spawn(move || {
                let head = |name: &'static str| {
                    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str().to_string())
                };
                let (host, origin, site) = (head("host"), head("origin"), head("sec-fetch-site"));
                if !from_own_page(&own_host, host.as_deref(), origin.as_deref(), site.as_deref()) {
                    answer(req, Reply::Text(403, "forbidden"), None);
                    return;
                }
                let url = req.url().to_string();
                let (path, query) = url.split_once('?').unwrap_or((&url, ""));
                let range = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("range"))
                    .map(|h| h.value.as_str().to_string());
                let mut raw = Vec::new();
                let _ = req.as_reader().take(16 << 20).read_to_end(&mut raw);
                let r = Req {
                    path: path.to_string(),
                    query: parse_query(query),
                    body: serde_json::from_slice(&raw).unwrap_or(Value::Null),
                    method: req.method().clone(),
                    range: range.clone(),
                };
                let reply = route(&r);
                answer(req, reply, r.range);
            });
        }
    });
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 100)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 10)));
        assert_eq!(parse_range("bytes=90-500", 100), Some((90, 10)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 10)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    #[test]
    fn stems() {
        assert_eq!(sanitize_stem("a:b?c.mp4"), "a_b_c");
        assert_eq!(sanitize_stem(".mp4"), "clip");
        assert_eq!(sanitize_stem("Apex Legends 2026-11-01 18-44-32"), "Apex Legends 2026-11-01 18-44-32");
    }

    #[test]
    fn allowed_paths() {
        settings::update(|s| {
            s.raw_folder = r"C:\Videos\Miyav".into();
            s.clips_folder = String::new();
        });
        assert!(allowed_path(r"C:\Videos\Miyav\Apex\a.mp4"));
        assert!(allowed_path(r"c:\videos\miyav\a.mp4"));
        assert!(!allowed_path(r"C:\Videos\Miyav Clips\a.mp4"));
        assert!(!allowed_path(r"C:\Windows\win.ini"));
        assert!(!allowed_path(r"C:\Videos\Miyav\..\..\Windows\win.ini"));
        assert!(!allowed_path(""));
    }


    #[test]
    fn only_the_own_page_gets_in() {
        let own = "127.0.0.1:5000";
        assert!(from_own_page(own, Some(own), None, None));
        assert!(from_own_page(own, Some(own), Some("http://127.0.0.1:5000"), Some("same-origin")));
        assert!(from_own_page(own, Some(own), None, Some("none")));
        // Another site's fetch, form or image.
        assert!(!from_own_page(own, Some(own), Some("https://evil.example"), Some("cross-site")));
        assert!(!from_own_page(own, Some(own), None, Some("cross-site")));
        assert!(!from_own_page(own, Some(own), Some("http://127.0.0.1:5001"), None));
        // DNS rebinding: the right address under another name.
        assert!(!from_own_page(own, Some("evil.example:5000"), None, None));
        assert!(!from_own_page(own, None, None, None));
    }

    #[test]
    fn serves_a_range_over_http() {
        let dir = std::env::temp_dir().join(format!("miyav-http-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.mp4");
        std::fs::write(&f, b"0123456789").unwrap();
        settings::update(|s| s.raw_folder = dir.display().to_string());
        let addr = start().unwrap();
        let q: String = url::form_urlencoded::byte_serialize(f.display().to_string().as_bytes()).collect();
        let res = ureq::get(&format!("{addr}/media?path={q}")).set("range", "bytes=2-5").call().unwrap();
        assert_eq!(res.status(), 206);
        assert_eq!(res.header("content-range"), Some("bytes 2-5/10"));
        assert_eq!(res.into_string().unwrap(), "2345");
        assert_eq!(ureq::get(&format!("{addr}/")).call().unwrap().status(), 200);
        assert_eq!(ureq::get(&format!("{addr}/api/job")).call().unwrap().status(), 200);
        let foreign = ureq::get(&format!("{addr}/api/job")).set("origin", "https://evil.example").call();
        assert!(matches!(foreign, Err(ureq::Error::Status(403, _))));
        let _ = std::fs::remove_dir_all(dir);
    }
}
