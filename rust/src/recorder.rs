//! The recorder is a replay buffer, the same idea as Shadowplay's Instant Replay:
//! ffmpeg captures the screen all the time into a ring of short MPEG-TS segments
//! on disk, and saving a clip is a stream copy of the newest few segments. The
//! copy takes about a second and never re-encodes, and the buffer costs disk
//! rather than RAM.
//!
//! In "game" mode (the default) a watcher waits for a game to come to the front.
//! A game that fills its monitor is recorded by recording that monitor with
//! Desktop Duplication (ddagrab), kept up while the player is tabbed out; a
//! windowed one is recorded on its own with Windows Graphics Capture
//! (gfxcapture), which would cost a fullscreen game latency. "screen" mode records
//! the whole primary monitor with ddagrab. Neither touches the game process - no
//! hook for anti-cheat to object to - and frames stay on the GPU all the way into
//! the hardware encoder, so the CPU barely notices.
//!
//! Games go to OBS's engine first when it is installed (obsrec.rs).
use serde::Serialize;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::audio::{self, AudioTrack};
use crate::obsrec::{self, ObsProc};
use crate::server::{sanitize_stem, unique_path};
use crate::settings::{self, Settings};
use crate::winrec::{self, Target};
use crate::{ffmpeg, storage};

const SEGMENT_SEC: i32 = 2;
/// Segments beyond the buffer length: one being written, plus slack so the oldest
/// segment we copy cannot be overwritten mid-copy.
const SEGMENT_SLACK: i32 = 3;

/// One way of turning d3d11 frames into H.264. H.264 rather than HEVC or AV1
/// because the clips end up in browsers, and every browser plays it. AV1 is an
/// opt-in of the OBS recording only (obsrec.rs).
#[derive(Clone, Copy)]
pub struct RecEncoder {
    pub name: &'static str,
    pub label: &'static str,
    /// Hardware encoders take the GPU frames directly; libx264 needs them copied
    /// back to system memory first.
    pub download: bool,
    args: fn(i32) -> Vec<String>,
}

fn strs(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Ordered by preference. Quality runs 1 (best) to 51 for every encoder here.
pub static ENCODERS: [RecEncoder; 4] = [
    RecEncoder {
        name: "h264_nvenc",
        label: "NVIDIA NVENC",
        download: false,
        // No B-frames: at 1440p they more than doubled ffmpeg's memory (660 MB ->
        // 320 MB without), for a few percent of file size.
        args: |q| {
            let mut a = strs(&["-c:v", "h264_nvenc", "-preset", "p4", "-tune", "hq", "-bf", "0", "-rc", "vbr", "-cq"]);
            a.push(q.to_string());
            a.extend(strs(&["-b:v", "0", "-maxrate", "80M", "-bufsize", "160M"]));
            a
        },
    },
    RecEncoder {
        name: "h264_amf",
        label: "AMD AMF",
        download: false,
        args: |q| {
            let mut a = strs(&["-c:v", "h264_amf", "-quality", "balanced", "-rc", "qvbr", "-qvbr_quality_level"]);
            a.push(q.to_string());
            a.extend(strs(&["-maxrate", "80M"]));
            a
        },
    },
    // Windows' own encoder layer: on Intel graphics this is QuickSync, and it
    // beats tying up the CPU. Frames go via system memory, so it comes last of
    // the hardware options.
    RecEncoder {
        name: "h264_mf",
        label: "Windows hardware encoder",
        download: true,
        args: |q| {
            let mut a = strs(&["-c:v", "h264_mf", "-hw_encoding", "1", "-rate_control", "quality", "-quality"]);
            a.push((100 - q * 7 / 4).max(1).to_string());
            a
        },
    },
    RecEncoder {
        name: "libx264",
        label: "CPU (x264)",
        download: true,
        args: |q| {
            let mut a = strs(&["-c:v", "libx264", "-preset", "veryfast", "-crf"]);
            a.push((q + 2).to_string());
            a
        },
    },
];

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecStatus {
    pub recording: bool,
    /// Game mode is on but no game is running yet.
    pub waiting: bool,
    pub target: String, // the game being recorded, "" for the whole screen
    pub encoder: String,
    pub since: i64,
    pub buffer_sec: i32,
    pub last_clip: String,
    #[serde(rename = "error")]
    pub err: String,
    /// Advice about the recording that works but could be better. Not an error.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub notice: String,
    /// The on/off switch, which the tray menu can flip too.
    pub enabled: bool,
    /// A clip the window should open in the editor, once: the "Clip saved"
    /// notification was clicked.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub open: String,
}

static OPEN_REQUEST: Mutex<String> = Mutex::new(String::new());

pub fn request_open(path: &str) {
    *OPEN_REQUEST.lock().unwrap() = path.to_string();
}

pub fn take_open_request() -> String {
    std::mem::take(&mut OPEN_REQUEST.lock().unwrap())
}

/// A flag threads can wait on.
#[derive(Clone, Default)]
pub struct Event(Arc<(Mutex<bool>, Condvar)>);

impl Event {
    pub fn set(&self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }
    pub fn is_set(&self) -> bool {
        *self.0 .0.lock().unwrap()
    }
    /// True if it was set within d.
    pub fn wait(&self, d: Duration) -> bool {
        let g = self.0 .0.lock().unwrap();
        let (g, _) = self.0 .1.wait_timeout_while(g, d, |set| !*set).unwrap();
        *g
    }
}

/// A running ffmpeg.
struct FfProc {
    id: u64,
    pid: u32,
    stdin: Option<ChildStdin>,
    exited: Event,
}

#[derive(Default)]
struct State {
    ff: Option<FfProc>,
    status: RecStatus,
    target: Target,
    enc: Option<RecEncoder>,
    /// Titles of the audio tracks in the current buffer, in track order.
    audio_titles: Vec<String>,
    /// Each ffmpeg run writes its own numbered set of segments. buffer_key says
    /// what the segments on disk were recorded as, so a restart that changes
    /// nothing (the game back in front after alt-tab) keeps the buffer.
    session: i32,
    buffer_key: String,
    buffer_app: String, // what the buffer holds, which names a saved clip's folder
    /// The OBS helper while it records instead of ffmpeg. no_obs holds the
    /// windows it could not hook, which go to ffmpeg from then on.
    obs: Option<Arc<ObsProc>>,
    no_obs: HashSet<isize>,
    next_id: u64,
}

static REC: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static SAVING: Mutex<()> = Mutex::new(());

/// Under %LOCALAPPDATA%: it churns constantly and has no business roaming with
/// the user profile.
pub fn replay_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Miyav").join("replay")
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Tries each encoder for real on a few frames. Asking ffmpeg which encoders it
/// was built with is not enough: NVENC is always listed, and only an actual
/// encode finds out the driver is too old or the GPU is not NVIDIA at all. The
/// frames are generated, not captured: a capture test failed whenever the screen
/// could not be duplicated at that moment - a fullscreen game in front, say - and
/// took the encoder down with it.
pub fn pick_encoder(want: &str) -> Result<RecEncoder, String> {
    let mut tried = Vec::new();
    for e in ENCODERS.iter().filter(|e| want.is_empty() || want == e.name) {
        let out = ffmpeg::command()
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black:s=640x360:r=10", "-frames:v", "3"])
            .args((e.args)(23))
            .args(["-f", "null", "-"])
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => return Ok(*e),
            Ok(o) => tried.push(format!("{}: {}", e.label, ffmpeg::first_line(&String::from_utf8_lossy(&o.stderr), "failed"))),
            Err(err) => tried.push(format!("{}: {err}", e.label)),
        }
    }
    Err(format!("no encoder works on this PC ({})", tried.join("; ")))
}

/// Begins capturing t into the replay buffer. Calling it while already recording
/// is a no-op; to switch targets, stop first.
pub fn start(t: Target) -> Result<(), String> {
    let mut r = REC.lock().unwrap();
    start_locked(&mut r, t)
}

fn start_locked(r: &mut State, t: Target) -> Result<(), String> {
    if r.ff.is_some() || r.obs.is_some() {
        return Ok(());
    }
    if !ffmpeg::tools_ready() {
        return Err("ffmpeg.exe is missing - reinstall Miyav".into());
    }
    let s = settings::get();
    if r.enc.is_none() {
        match pick_encoder(&s.record_encoder) {
            Ok(e) => r.enc = Some(e),
            Err(e) => {
                r.status.err = e.clone();
                return Err(e);
            }
        }
    }
    let enc = r.enc.unwrap();

    if t.hwnd != 0 && !t.no_hook && !r.no_obs.contains(&t.hwnd) && obsrec::helper_path().is_some() {
        match obsrec::start(&t, &s, &enc) {
            Ok(p) => {
                use_obs(r, p, t, &s, &enc);
                return Ok(());
            }
            Err(e) => {
                rec_log(&format!("OBS cannot record {}, using ffmpeg: {e}", t.app));
                mark_no_obs(r, t.hwnd);
            }
        }
    }

    let dir = replay_dir();
    let fps = s.record_fps;
    let mut args = strs(&["-hide_banner", "-loglevel", "error", "-nostats", "-f", "lavfi", "-i"]);
    let window = t.hwnd != 0 && (!t.display || t.behind);
    if window {
        args.push(format!("gfxcapture=hwnd={}:max_framerate={fps}", t.hwnd));
    } else if t.display {
        // A game capped at the recording fps would beat against a grab clock of
        // the same rate: a frame landing just after a tick becomes a repeat and
        // the next one is skipped, which judders. Polling at twice the rate and
        // taking only new frames, stamped when they appeared, keeps every game
        // frame exactly once.
        args.push(format!("ddagrab=output_idx={}:framerate={}:dup_frames=0:draw_mouse=1", t.output, 2 * fps));
    } else {
        args.push(format!("ddagrab=output_idx={}:framerate={fps}:draw_mouse=1", t.output));
    }
    let tracks = audio::open_tracks(t.pid, s.record_mic);
    let (audio_args, titles) = audio::layout(&tracks);
    args.extend(audio_args);

    // Segments from an earlier run can only join a clip if they were recorded the
    // same way - same game, same encoding, same audio tracks - or the stream copy
    // that saves the clip would splice together incompatible streams. Behind is
    // left out: the window of a game that fills its monitor is the monitor's size,
    // so both halves of an alt-tab join up.
    let key = format!("{}|{}|{}|{fps}|{}|{}|{titles:?}", t.app, t.display, t.output, s.record_quality, enc.name);
    if key != r.buffer_key {
        let _ = std::fs::remove_dir_all(&dir);
        r.buffer_key = key;
        r.buffer_app = t.app.clone();
    } else {
        prune_segments(&dir, s.replay_seconds);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    r.session += 1;

    if window {
        // Windows Graphics Capture only delivers a frame when the window changes;
        // cfr fills the gaps so every segment is a steady fps.
        args.extend(strs(&["-fps_mode:v", "cfr", "-r:v"]));
        args.push(fps.to_string());
    }
    let mut filters = Vec::new();
    if t.display && !window {
        // Thin a faster game down to about the recording fps: keep a frame once
        // 3/4 of a frame time has passed since the last one kept.
        filters.push(format!("select='isnan(prev_selected_t)+gte(t-prev_selected_t\\,{:.4})'", 0.75 / fps as f64));
        args.extend(strs(&["-fps_mode:v", "vfr"]));
    }
    if enc.download {
        filters.push("hwdownload,format=bgra,format=yuv420p".to_string());
    }
    if !filters.is_empty() {
        args.push("-vf".into());
        args.push(filters.join(","));
    }
    args.extend((enc.args)(s.record_quality));
    // One keyframe per segment, so every segment can start a clip.
    let g = (fps * SEGMENT_SEC).to_string();
    args.extend(["-g".into(), g.clone(), "-keyint_min".into(), g]);
    args.extend(strs(&["-f", "segment", "-segment_time"]));
    args.push(SEGMENT_SEC.to_string());
    args.extend(strs(&["-segment_format", "mpegts", "-reset_timestamps", "1", "-segment_wrap"]));
    args.push((s.replay_seconds / SEGMENT_SEC + SEGMENT_SLACK).to_string());
    args.push(dir.join(format!("s{:03}-%03d.ts", r.session)).display().to_string());

    let mut child = ffmpeg::command()
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;
    let what = if t.app.is_empty() { "the screen" } else { &t.app };
    let how = if t.behind {
        "window capture while tabbed out".to_string()
    } else if window {
        "window capture".to_string()
    } else {
        format!("display {}", t.output)
    };
    rec_log(&format!("recording {what} ({how}) with {}, audio {titles:?}", enc.label));
    rec_log(&format!("  ffmpeg {}", args.join(" ")));
    // If the app dies without stopping us, Windows kills ffmpeg too rather than
    // leaving it recording the desktop forever.
    winrec::bind_child(&child);

    r.next_id += 1;
    let id = r.next_id;
    let exited = Event::default();
    r.ff = Some(FfProc { id, pid: child.id(), stdin: child.stdin.take(), exited: exited.clone() });
    r.target = t.clone();
    r.audio_titles = titles;
    let waiting = r.status.waiting;
    let last = r.status.last_clip.clone();
    // Handing over between monitor and window keeps it: the same game, same advice.
    let notice = std::mem::take(&mut r.status.notice);
    r.status = RecStatus {
        notice,
        recording: true,
        target: t.app.clone(),
        encoder: enc.label.to_string(),
        since: now_unix(),
        buffer_sec: s.replay_seconds,
        last_clip: last,
        waiting,
        ..Default::default()
    };

    let stderr = child.stderr.take();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut e) = stderr {
            use std::io::Read;
            let _ = e.read_to_string(&mut text);
        }
        let st = child.wait();
        drop(tracks);
        {
            let mut r = REC.lock().unwrap();
            if r.ff.as_ref().is_some_and(|f| f.id == id) {
                r.ff = None;
                r.status.recording = false;
                if st.is_ok_and(|s| s.success()) {
                    // Windows ended the capture by itself; fullscreen games do
                    // this on alt-tab. The watcher picks the game up again.
                    rec_log(&format!("capture of {} ended by Windows", t.app));
                } else {
                    let msg = format!("recording stopped: {}", ffmpeg::first_line(&text, "ffmpeg quit"));
                    rec_log(&format!("{msg} ({})", t.app));
                    r.status.err = msg;
                }
            }
        }
        exited.set();
    });
    Ok(())
}

/// Makes p the recording.
fn use_obs(r: &mut State, p: Arc<ObsProc>, mut t: Target, s: &Settings, enc: &RecEncoder) {
    t.obs = true;
    t.behind = false;
    r.obs = Some(p.clone());
    r.target = t.clone();
    r.audio_titles = obsrec::titles(s.record_mic);
    // The ffmpeg segments on disk are of something else now.
    r.buffer_key.clear();
    r.buffer_app = t.app.clone();
    if let Ok(rd) = std::fs::read_dir(replay_dir()) {
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "ts") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let (waiting, last) = (r.status.waiting, r.status.last_clip.clone());
    r.status = RecStatus {
        recording: true,
        target: t.app.clone(),
        encoder: format!("OBS game capture, {}{}", enc.label, p.codec_label()),
        since: now_unix(),
        buffer_sec: s.replay_seconds,
        last_clip: last,
        waiting,
        ..Default::default()
    };
    std::thread::spawn(move || {
        p.exited.wait(Duration::from_secs(u32::MAX as u64));
        let mut r = REC.lock().unwrap();
        if r.obs.as_ref().is_some_and(|o| Arc::ptr_eq(o, &p)) {
            r.obs = None;
            r.status.recording = false;
            let mut e = "OBS stopped recording".to_string();
            let last = p.last_err();
            if !last.is_empty() {
                e = format!("{e}: {last}");
            }
            rec_log(&format!("{e} ({})", t.app));
            r.status.err = e;
        }
    });
}

fn mark_no_obs(r: &mut State, hwnd: isize) {
    r.no_obs.retain(|&h| winrec::window_alive(h));
    r.no_obs.insert(hwnd);
}

/// OBS is recording a game it has never managed to hook. The game is marked to be
/// recorded by ffmpeg, and the caller restarts the recording.
fn obs_gave_up() -> bool {
    let mut r = REC.lock().unwrap();
    if !r.obs.as_ref().is_some_and(|o| o.gave_up()) {
        return false;
    }
    let h = r.target.hwnd;
    mark_no_obs(&mut r, h);
    true
}

/// Asks ffmpeg to finish cleanly ("q" on stdin) so the last segment is valid, and
/// kills it if it has not gone within a few seconds.
pub fn stop() {
    let (ff, p) = {
        let mut r = REC.lock().unwrap();
        r.target = Target::default();
        r.status.recording = false;
        r.status.target.clear();
        r.status.err.clear();
        r.status.notice.clear();
        (r.ff.take(), r.obs.take())
    };
    stop_ffmpeg(ff);
    if let Some(p) = p {
        p.stop();
    }
}

fn stop_ffmpeg(ff: Option<FfProc>) {
    let Some(mut f) = ff else { return };
    if let Some(mut i) = f.stdin.take() {
        let _ = i.write_all(b"q");
    }
    if !f.exited.wait(Duration::from_secs(5)) {
        winrec::kill_pid(f.pid);
        f.exited.wait(Duration::from_secs(5));
    }
}

/// Moves the recording to t without a hole: the new ffmpeg starts while the old
/// one keeps going, and the old one is only stopped once the new one has written
/// video. The clip repeats the fraction of a second they overlap rather than
/// missing the time ffmpeg takes to start.
fn switch(t: Target) -> Result<(), String> {
    let mut r = REC.lock().unwrap();
    let old = r.ff.take(); // lets start run; the old one's exit no longer counts as ours
    let target = r.target.clone();
    if let Err(e) = start_locked(&mut r, t) {
        if r.ff.is_none() {
            r.status.recording = old.is_some();
            r.ff = old;
            r.target = target;
        }
        return Err(e);
    }
    let first = replay_dir().join(format!("s{:03}-000.ts", r.session));
    drop(r);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if first.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop_ffmpeg(old);
    Ok(())
}

pub fn status() -> RecStatus {
    let r = REC.lock().unwrap();
    let mut st = r.status.clone();
    st.waiting = st.waiting && !st.recording;
    st.enabled = settings::get().record_enabled;
    st
}

/// What is being recorded, if anything is.
pub fn running() -> Option<Target> {
    let r = REC.lock().unwrap();
    (r.ff.is_some() || r.obs.is_some()).then(|| r.target.clone())
}

/// A running ffmpeg that has not written a segment for three segments' time. A new
/// one is due every SEGMENT_SEC.
fn stalled() -> bool {
    let (session, since, on) = {
        let r = REC.lock().unwrap();
        (r.session, r.status.since, r.ff.is_some())
    };
    let limit = (3 * SEGMENT_SEC) as i64;
    if !on || now_unix() - since < limit {
        return false;
    }
    let prefix = format!("s{session:03}-");
    let mut newest = since;
    if let Ok(rd) = std::fs::read_dir(replay_dir()) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                if let Some(t) = e.metadata().ok().and_then(|m| m.modified().ok()) {
                    newest = newest.max(t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
                }
            }
        }
    }
    now_unix() - newest > limit
}

pub fn set_err(e: &str) {
    REC.lock().unwrap().status.err = e.to_string();
}

/// Copies the last `seconds` of the buffer into the raw recordings folder, under a
/// subfolder named after the game - the same per-game layout OBS and Shadowplay
/// use, so the Raw Videos tab groups it. ready, if given, is called as soon as the
/// footage is known to be there - before the copy, which for a minute of 1440p
/// takes a few seconds.
pub fn save_clip(seconds: i32, ready: Option<&(dyn Fn() + Sync)>) -> Result<String, String> {
    // Two presses of the hotkey in quick succession would otherwise race on the
    // same segments; the second simply waits for the first.
    let _saving = SAVING.lock().unwrap();
    let (p, titles) = {
        let r = REC.lock().unwrap();
        (r.obs.clone(), r.audio_titles.clone())
    };
    if let Some(p) = p {
        let out = clip_path()?;
        p.save(seconds, &out, &titles, ready)?;
        return Ok(finish_save(out));
    }

    // Not only while ffmpeg runs: just after an alt-tab ended the capture, the
    // last minute of the game is still on disk and still worth saving.
    let window = Duration::from_secs((seconds + SEGMENT_SEC * SEGMENT_SLACK) as u64);
    let n = (seconds as f64 / SEGMENT_SEC as f64).ceil() as usize + 1;
    let segs = match newest_segments(&replay_dir(), n, window) {
        Ok(s) => s,
        Err(e) => {
            if !status().recording {
                return Err("the replay buffer is not running".into());
            }
            return Err(e);
        }
    };
    let out = clip_path()?;
    let list = replay_dir().join("concat.txt");
    std::fs::write(&list, concat_list(&segs)).map_err(|e| e.to_string())?;
    if let Some(f) = ready {
        f();
    }
    // The newest segment is still being written, so ffmpeg may grumble about a
    // truncated packet at the very end. That is expected and harmless.
    let mut cmd = ffmpeg::command();
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i"]);
    cmd.arg(&list).args(["-map", "0", "-c", "copy"]);
    for (i, title) in REC.lock().unwrap().audio_titles.iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("title={title}"));
    }
    // No +faststart: it writes the whole file a second time to move the index to
    // the front, doubling the wait. Cuts and encodes add it.
    cmd.arg(&out);
    let o = cmd.stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        let _ = std::fs::remove_file(&out);
        return Err(format!("could not save the clip: {}", ffmpeg::first_line(&String::from_utf8_lossy(&o.stderr), "ffmpeg failed")));
    }
    Ok(finish_save(out))
}

fn finish_save(out: PathBuf) -> String {
    let out = out.display().to_string();
    REC.lock().unwrap().status.last_clip = out.clone();
    storage::note_recorded(&out);
    out
}

/// concat demuxer syntax: single quotes, with ' written as '\''
pub fn concat_list(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|f| format!("file '{}'\n", f.display().to_string().replace('\\', "/").replace('\'', r"'\''")))
        .collect()
}

/// Where a new clip goes: the raw folder, in a subfolder named after the game.
fn clip_path() -> Result<PathBuf, String> {
    let mut app = REC.lock().unwrap().buffer_app.clone();
    if app.is_empty() {
        app = winrec::foreground_app();
    }
    if app.is_empty() {
        app = "Desktop".into();
    }
    let root = settings::get().raw_folder;
    let root = if root.trim().is_empty() { settings::default_folder("Miyav") } else { root };
    let dir = Path::new(&root).join(sanitize_stem(&app));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stem = sanitize_stem(&format!("{app} {}", winrec::local_stamp()));
    Ok(unique_path(&dir, &stem, ".mp4"))
}

/// Deletes segments too old to be part of any clip: those left by earlier runs,
/// which segment_wrap of the current run will never overwrite.
fn prune_segments(dir: &Path, buffer_sec: i32) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let keep = Duration::from_secs((buffer_sec + SEGMENT_SEC * SEGMENT_SLACK) as u64);
    for e in rd.flatten() {
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|a| a > keep);
        if old && e.path().extension().is_some_and(|x| x == "ts") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Up to n segment files from the last max_age, oldest first. segment_wrap reuses
/// names, so modification time is the only reliable order.
fn newest_segments(dir: &Path, n: usize, max_age: Duration) -> Result<Vec<PathBuf>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut segs: Vec<(SystemTime, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "ts") {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        let Ok(t) = m.modified() else { continue };
        if m.len() == 0 || t.elapsed().is_ok_and(|a| a > max_age) {
            continue;
        }
        segs.push((t, p));
    }
    if segs.is_empty() {
        return Err("nothing has been recorded yet".into());
    }
    segs.sort();
    let skip = segs.len().saturating_sub(n);
    Ok(segs.into_iter().skip(skip).map(|s| s.1).collect())
}

/// What the global hotkey runs: it cannot show an error dialog over a fullscreen
/// game, so success and failure are told apart by sound.
///
/// The beep comes the moment the footage is known to be there, not when the copy
/// finishes a few seconds later: a press with no answer gets pressed again. If the
/// copy then fails, the buzz follows.
pub fn save_from_hotkey() {
    let start = Instant::now();
    match save_clip(settings::get().replay_seconds, Some(&|| winrec::beep(true))) {
        Ok(out) => rec_log(&format!("saved {out} in {:.1}s", start.elapsed().as_secs_f64())),
        Err(e) => {
            set_err(&e);
            rec_log(&format!("save failed: {e}"));
            winrec::beep(false);
        }
    }
}

static REC_LOG: Mutex<()> = Mutex::new(());

/// Keeps a short history of what the recorder did, because a hotkey press over a
/// game leaves nothing else behind to look at afterwards.
pub fn rec_log(msg: &str) {
    let _g = REC_LOG.lock().unwrap();
    let Some(dir) = replay_dir().parent().map(Path::to_path_buf) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("recorder.log");
    if path.metadata().map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {msg}", winrec::local_time());
    }
}

// ---- turning recording on and off ----

static WATCHER: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// Registers the hotkey and either starts recording the screen or, in game mode,
/// starts watching for a game to record.
pub fn enable() -> Result<(), String> {
    let s = settings::get();
    if let Err(e) = winrec::set_hotkey(&s.hotkey, Some(save_from_hotkey)) {
        set_err(&e);
        return Err(e);
    }
    let mut w = WATCHER.lock().unwrap();
    if w.is_some() {
        return Ok(());
    }
    if s.record_mode == "screen" {
        return start(Target::default());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *w = Some(cancel.clone());
    REC.lock().unwrap().status.waiting = true;
    let c2 = cancel.clone();
    std::thread::spawn(move || watch_for_games(c2));
    std::thread::spawn(move || crate::autoclip::run(cancel));
    Ok(())
}

pub fn disable() {
    if let Some(c) = WATCHER.lock().unwrap().take() {
        c.store(true, Ordering::SeqCst);
    }
    REC.lock().unwrap().status.waiting = false;
    stop();
    let _ = winrec::set_hotkey("", None);
}

/// Follows whatever game is in front. A window has to stay in front for two checks
/// before recording switches to it, so alt-tabbing through a fullscreen video does
/// not throw away the game's buffer.
fn watch_for_games(cancel: Arc<AtomicBool>) {
    let (mut pending, mut started) = (0isize, 0isize);
    let far_past = Instant::now();
    let mut started_at = far_past;
    let mut resumed_at: Option<Instant> = None;
    let mut switch_failed: Option<Instant> = None;
    let mut closed_at: Option<Instant> = None;
    // The game being recorded as a monitor. It stays recorded for as long as it is
    // open, alt-tabbed or not: through its window while it is behind (see
    // Target::behind), so the clip keeps showing the game.
    let mut game = Target::default();
    // Windows that could not be captured (protected content, an elevated process)
    // are not retried every two seconds for as long as they are open.
    let mut failed: HashSet<isize> = HashSet::new();
    let older = |t: Option<Instant>, d: Duration| t.is_none_or(|t| t.elapsed() >= d);

    let mut n = 0u64;
    loop {
        n += 1;
        std::thread::sleep(Duration::from_millis(250));
        if cancel.load(Ordering::SeqCst) {
            return;
        }

        // Alt-tab: hand the game over to window capture, and back to its monitor
        // when it is in front again. Checked four times a second, as until the
        // handover the clip shows whatever replaced the game.
        if let Some(cur) = running() {
            if !cur.obs && game.hwnd != 0 && cur.hwnd == game.hwnd && older(switch_failed, Duration::from_secs(3)) {
                let want = game_view(&game, cur.behind);
                if want.behind != cur.behind {
                    if let Err(e) = switch(want) {
                        switch_failed = Some(Instant::now());
                        rec_log(&format!("could not switch {} capture: {e}", game.app));
                    }
                }
            }
        }
        if n % 4 != 0 {
            continue;
        }

        let cur_opt = running();
        let mut recording = cur_opt.is_some();
        let mut cur = cur_opt.unwrap_or_default();
        if game.hwnd != 0 && !winrec::window_alive(game.hwnd) {
            if !recording {
                rec_log(&format!("{} closed", game.app));
            }
            game = Target::default();
        }
        if recording && cur.hwnd != 0 && !winrec::window_alive(cur.hwnd) {
            // The game closed; wait for the next one. OBS's buffer goes with it,
            // so it is kept for a buffer's length in case the end of the game is
            // still to be clipped.
            let keep = Duration::from_secs(settings::get().replay_seconds as u64);
            if !cur.obs || closed_at.is_some_and(|t| t.elapsed() > keep) {
                stop();
                recording = false;
                started = 0;
                closed_at = None;
                rec_log(&format!("{} closed", cur.app));
            } else if closed_at.is_none() {
                closed_at = Some(Instant::now());
            }
        }
        // OBS never hooked the game: record it with ffmpeg instead.
        if recording && cur.obs && winrec::window_alive(cur.hwnd) && obs_gave_up() {
            rec_log(&format!("OBS could not hook {}, recording it with ffmpeg", cur.app));
            stop();
            let mut t = cur.clone();
            t.obs = false;
            if let Err(e) = start(t.clone()) {
                recording = false;
                started = 0;
                rec_log(&format!("could not record {}: {e}", t.app));
            } else if crate::autoclip::is_cs2(&t.app) {
                warn_cs2_hook();
            }
        }
        // A fullscreen game that drops out of exclusive mode and back, even for a
        // moment, can leave Desktop Duplication handing out no frames at all;
        // ffmpeg then waits forever and the buffer silently empties. Starting the
        // capture again gets the frames back.
        if recording && stalled() {
            stop();
            recording = false;
            started = 0;
            rec_log(&format!("capture of {} stalled, starting it again", cur.app));
        }
        // The capture of an open game ended: an exclusive-fullscreen game switching
        // modes on alt-tab ends it, as does the stall above. It is started again
        // straight away, buffer intact, whatever is in front.
        if !recording && game.hwnd != 0 && older(resumed_at, Duration::from_secs(3)) {
            resumed_at = Some(Instant::now());
            match start(game_view(&game, false)) {
                Err(e) => rec_log(&format!("could not resume {}: {e}", game.app)),
                Ok(_) => {
                    recording = true;
                    started = 0;
                    cur = game.clone();
                }
            }
        }
        // ffmpeg ended on a window that is still open. Straight after starting, that
        // means capture cannot work on it (its error is in the status) and it is not
        // retried. After a good run it is just Windows ending the capture -
        // fullscreen games do on alt-tab - and the game is picked up again, buffer
        // intact, when it is back in front.
        if !recording && started != 0 {
            if winrec::window_alive(started) && started_at.elapsed() < Duration::from_secs(10) {
                failed.insert(started);
            }
            started = 0;
        }
        failed.retain(|&h| winrec::window_alive(h));

        let cand = winrec::game_in_front();
        if cand.hwnd == 0 || failed.contains(&cand.hwnd) || (recording && cand.hwnd == cur.hwnd) {
            pending = 0;
            continue;
        }
        // Tabbing from a game to Discord must not throw the game's buffer away: an
        // added app only takes over once no game is being recorded.
        if cand.custom && recording && !cur.custom {
            pending = 0;
            continue;
        }
        if cand.hwnd != pending {
            pending = cand.hwnd;
            continue;
        }
        pending = 0;
        closed_at = None;
        stop();
        if let Err(e) = start(cand.clone()) {
            failed.insert(cand.hwnd);
            rec_log(&format!("could not record {}: {e}", cand.app));
            set_err(&format!("could not record {}: {e}", cand.app));
            continue;
        }
        started = cand.hwnd;
        started_at = Instant::now();
        game = if cand.display { cand } else { Target::default() };
    }
}

/// What to tell a player whose CS2 refused OBS's hook. CS2's Trusted Mode blocks it
/// unless the game was started with this option, which is the player's call: Valve
/// warns it can lower their Trust Factor. Clips still work without it.
pub const CS2_HOOK_NOTICE: &str = "CS2's Trusted Mode blocked game capture, so this is being recorded from the screen: notifications and what you tab to can end up in clips. To use game capture, add -allow_third_party_software to CS2's launch options in Steam and restart it. Valve warns this can lower your Trust Factor, so it is your call. Clips work either way.";

static CS2_BALLOON: AtomicBool = AtomicBool::new(false);

/// Says so on the Recording page for as long as this recording lasts, and in a
/// notification once per run of the app.
fn warn_cs2_hook() {
    REC.lock().unwrap().status.notice = CS2_HOOK_NOTICE.to_string();
    if !CS2_BALLOON.swap(true, Ordering::SeqCst) {
        crate::tray::balloon("CS2 blocked game capture", "Still recording, from the screen. Open the app to see why.");
    }
}

/// How to record game right now: its monitor while it is in front, its window
/// while it is behind. A minimized window has nothing to capture, so a game that
/// minimizes on alt-tab (exclusive fullscreen) stays on its monitor; one already on
/// window capture stays there, its last frame held.
fn game_view(game: &Target, behind: bool) -> Target {
    let mut g = game.clone();
    g.behind = winrec::foreground_window() != g.hwnd && (behind || !winrec::window_minimized(g.hwnd));
    g
}

/// Called once at launch.
pub fn start_if_enabled() {
    if settings::get().record_enabled {
        let _ = enable();
    }
}

/// Restarts the recorder when a recording setting changed, so the Recording page
/// has no separate "apply" step. A restart starts the buffer over, so it only
/// happens when something really changed. The error is what the page shows: a
/// hotkey another app owns, say.
pub fn apply_settings(old: &Settings, cur: &Settings) -> Result<(), String> {
    let key = |s: &Settings| {
        format!(
            "{} {} {} {} {} {} {} {} {} {}",
            s.record_enabled, s.record_mode, s.replay_seconds, s.record_fps, s.record_quality,
            s.record_encoder, s.record_codec, s.replay_in_memory, s.record_mic, s.hotkey
        )
    };
    if key(old) == key(cur) {
        return Ok(());
    }
    disable();
    if old.record_encoder != cur.record_encoder {
        REC.lock().unwrap().enc = None;
    }
    if !cur.record_enabled {
        return Ok(());
    }
    let r = enable();
    if r.is_err() {
        disable(); // no half-on state: a hotkey with nothing recording
    }
    r
}

pub fn restore_settings(old: &Settings) {
    let failed = settings::get();
    settings::update(|s| {
        s.record_enabled = old.record_enabled;
        s.record_mode = old.record_mode.clone();
        s.replay_seconds = old.replay_seconds;
        s.record_fps = old.record_fps;
        s.record_quality = old.record_quality;
        s.record_encoder = old.record_encoder.clone();
        s.record_codec = old.record_codec.clone();
        s.replay_in_memory = old.replay_in_memory;
        s.record_mic = old.record_mic;
        s.hotkey = old.hotkey.clone();
    });
    let _ = settings::save();
    let _ = apply_settings(&failed, &settings::get());
}

/// The on/off switch: remembered, so the choice survives a restart of the app.
pub fn set_enabled(on: bool) -> Result<(), String> {
    let before = settings::get();
    settings::update(|s| s.record_enabled = on);
    settings::save()?;
    apply_settings(&before, &settings::get())
}

pub fn running_apps() -> Vec<crate::winrec::RunningApp> {
    winrec::running_apps()
}

/// Dropping the tracks stops their capture: a Vec so the thread that waits on
/// ffmpeg can own them.
#[allow(dead_code)]
type Tracks = Vec<AudioTrack>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_quoting() {
        let files = [PathBuf::from(r"C:\a\it's.ts")];
        assert_eq!(concat_list(&files), "file 'C:/a/it'\\''s.ts'\n");
    }

    #[test]
    fn encoder_args() {
        assert!((ENCODERS[0].args)(23).contains(&"23".to_string()));
        assert!((ENCODERS[3].args)(23).contains(&"25".to_string()));
        assert!((ENCODERS[2].args)(23).contains(&"60".to_string()));
    }

    #[test]
    fn notice_only_when_set() {
        let v = serde_json::to_value(RecStatus::default()).unwrap();
        assert!(v.get("notice").is_none());
        let st = RecStatus { notice: CS2_HOOK_NOTICE.into(), ..Default::default() };
        let v = serde_json::to_value(st).unwrap();
        assert!(v["notice"].as_str().unwrap().contains("-allow_third_party_software"));
    }

    #[test]
    fn event_waits() {
        let e = Event::default();
        assert!(!e.wait(Duration::from_millis(1)));
        e.set();
        assert!(e.wait(Duration::from_secs(5)) && e.is_set());
    }
}
