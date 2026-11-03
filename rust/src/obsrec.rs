//! Games are recorded with OBS's engine where it is installed beside the app
//! (obs\, see miyav-obs): OBS's game capture hooks the game's own frames, so a clip
//! holds only the game - no notifications popping up over it, no Discord while the
//! player is tabbed out - and costs the game no latency. The hook is OBS's signed
//! one, which anti-cheats know.
//!
//! If the hook does not take within HOOK_WAIT, the game is recorded the ffmpeg way
//! instead.
//!
//! Encoded video is what fills the memory: bitrate times buffer length, so a minute
//! at 30 Mbps is 225 MB. By default the helper writes it to short files instead, and
//! a clip is those files joined. replay_in_memory turns that off.
use serde::Deserialize;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::audio::{TITLE_GAME, TITLE_MIC, TITLE_MIX, TITLE_PC};
use crate::recorder::{concat_list, rec_log, replay_dir, Event, RecEncoder};
use crate::settings::Settings;
use crate::winrec::{self, Target};
use crate::{ffmpeg, win};

const HOOK_WAIT: Duration = Duration::from_secs(20);
const SEGMENT_SEC: i32 = 5;

/// The ceiling on the video bitrate: constant quality on its own lets a busy scene
/// take as much as it likes, and it is those scenes that make clips too big to
/// share. About 37 Mbps at 1080p60.
pub fn max_kbps(w: i32, h: i32, fps: i32) -> i32 {
    let k = (w as i64 * h as i64 * fps as i64 * 3 / 10 / 1000) as i32;
    k.clamp(8000, 80000)
}

/// Whether the GPU has an AV1 encoder. Only NVIDIA: AMD's quality scale for AV1 has
/// not been checked against a real card.
static AV1: LazyLock<bool> = LazyLock::new(|| {
    ffmpeg::command()
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black:s=640x360:r=10"])
        .args(["-frames:v", "3", "-c:v", "av1_nvenc", "-f", "null", "-"])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
});

/// The helper, if the OBS files are installed.
pub fn helper_path() -> Option<PathBuf> {
    let exe = ffmpeg::exe_path();
    let p = exe.parent()?.join("obs").join("bin").join("64bit").join("miyav-obs.exe");
    p.exists().then_some(p)
}

/// OBS encoder ids for the encoder pick_encoder found working. Windows' own encoder
/// has no OBS counterpart; x264 stands in.
fn encoder_id(name: &str) -> &'static str {
    match name {
        "h264_nvenc" => "obs_nvenc_h264_tex",
        "h264_amf" => "h264_texture_amf",
        _ => "obs_x264",
    }
}

#[derive(Deserialize, Clone, Default, Debug)]
struct ObsEvent {
    event: String,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    path: String,
}

#[derive(Default)]
struct Hook {
    hooked: bool,
    hooks: u32, // times it hooked, for the log
    err: String,
}

pub struct ObsProc {
    pid: u32,
    stdin: Mutex<Option<ChildStdin>>,
    pub exited: Event,
    saved: Mutex<Receiver<ObsEvent>>, // "saved" or "split", or "error" while a save is waited for
    started: Instant,
    dir: Option<PathBuf>, // holds the buffer's files; None when it is in memory
    av1: bool,
    hook: Arc<Mutex<Hook>>,
}

/// Launches the helper and has it record t. The canvas is the game's size, so
/// nothing is scaled.
pub fn start(t: &Target, s: &Settings, enc: &RecEncoder) -> Result<Arc<ObsProc>, String> {
    let helper = helper_path().ok_or("OBS is not installed")?;
    let (spec, w, h) = winrec::obs_window(t.hwnd);
    if w < 16 || h < 16 {
        return Err("the game window has no size".into());
    }
    let (w, h) = (w & !1, h & !1); // NV12 wants even sizes

    let mut enc_id = encoder_id(enc.name);
    let mut quality = s.record_quality;
    let mut av1 = false;
    if s.record_codec == "av1" && enc.name == "h264_nvenc" {
        if *AV1 {
            // AV1's scale runs higher for the same picture.
            enc_id = "obs_nvenc_av1_tex";
            quality += 7;
            av1 = true;
        } else {
            rec_log("no AV1 encoder here, recording H.264");
        }
    }
    let dir = replay_dir().join("obs");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let segment = if s.replay_in_memory { 0 } else { SEGMENT_SEC };

    let (p, ready) = spawn(&helper, (segment > 0).then(|| dir.clone()), av1)?;
    if segment > 0 {
        let q = p.clone();
        let secs = s.replay_seconds;
        std::thread::spawn(move || q.prune_loop(secs));
    }
    let start = json!({
        "cmd": "start", "window": spec, "width": w, "height": h,
        "fps": s.record_fps, "encoder": enc_id, "quality": quality,
        "seconds": s.replay_seconds, "dir": dir.display().to_string(), "mic": s.record_mic,
        "max_kbps": max_kbps(w, h, s.record_fps), "segment_sec": segment,
    });
    if let Err(e) = p.send(&start) {
        p.stop();
        return Err(e);
    }
    match ready.recv_timeout(Duration::from_secs(30)) {
        Ok(e) if e.event == "started" => {}
        Ok(e) => {
            p.stop();
            return Err(format!("OBS: {}", e.msg));
        }
        Err(RecvTimeoutError::Disconnected) => return Err("OBS quit while starting".into()),
        Err(RecvTimeoutError::Timeout) => {
            p.stop();
            return Err("OBS took too long to start".into());
        }
    }
    rec_log(&format!("recording {} with OBS game capture ({w}x{h}, {enc_id}), window \"{spec}\"", t.app));
    Ok(p)
}

fn spawn(helper: &Path, dir: Option<PathBuf>, av1: bool) -> Result<(Arc<ObsProc>, Receiver<ObsEvent>), String> {
    let mut cmd = Command::new(helper);
    cmd.current_dir(helper.parent().unwrap_or(Path::new(".")));
    win::hide_window(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start OBS: {e}"))?;
    // Windows takes the helper, and the obs-ffmpeg-mux it starts, down with us.
    winrec::bind_child(&child);
    let (saved_tx, saved_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let hook = Arc::new(Mutex::new(Hook::default()));
    let p = Arc::new(ObsProc {
        pid: child.id(),
        stdin: Mutex::new(child.stdin.take()),
        exited: Event::default(),
        saved: Mutex::new(saved_rx),
        started: Instant::now(),
        dir,
        av1,
        hook: hook.clone(),
    });
    let stdout = child.stdout.take().expect("piped stdout");
    let started = p.started;
    std::thread::spawn(move || read_events(stdout, ready_tx, saved_tx, hook, started));
    let exited = p.exited.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        exited.set();
    });
    Ok((p, ready_rx))
}

/// Handles the helper's events. Its log goes to obs.log beside recorder.log; what
/// matters is noted in recorder.log too.
fn read_events(
    stdout: ChildStdout,
    ready: Sender<ObsEvent>,
    saved: Sender<ObsEvent>,
    hook: Arc<Mutex<Hook>>,
    started: Instant,
) {
    let log_path = replay_dir().parent().map(|d| d.join("obs.log")).unwrap_or_default();
    if log_path.metadata().map(|m| m.len() > 4 << 20).unwrap_or(false) {
        let _ = std::fs::rename(&log_path, log_path.with_extension("log.old"));
    }
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).ok();
    if let Some(f) = log.as_mut() {
        let _ = writeln!(f, "---- {}", winrec::local_time());
    }
    let mut starting = true;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(e) = serde_json::from_str::<ObsEvent>(&line) else { continue };
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{} {} {}{}", winrec::local_time(), e.event, e.msg, e.path);
        }
        match e.event.as_str() {
            "started" => {
                starting = false;
                let _ = ready.send(e);
            }
            "error" => {
                if starting {
                    starting = false;
                    let _ = ready.send(e);
                    continue;
                }
                hook.lock().unwrap().err = e.msg.clone();
                rec_log(&format!("OBS: {}", e.msg));
                let _ = saved.send(e);
            }
            "hooked" => {
                let mut h = hook.lock().unwrap();
                h.hooked = true;
                h.hooks += 1;
                if h.hooks == 1 {
                    rec_log(&format!("OBS game capture hooked {} after {:.1}s", e.msg, started.elapsed().as_secs_f64()));
                }
            }
            "saved" | "split" => {
                let _ = saved.send(e);
            }
            "stopped" if e.msg != "0" => {
                rec_log(&format!("OBS replay buffer stopped with code {}", e.msg));
                hook.lock().unwrap().err = format!("OBS replay buffer stopped (code {})", e.msg);
            }
            _ => {}
        }
    }
}

impl ObsProc {
    pub fn codec_label(&self) -> &'static str {
        if self.av1 {
            ", AV1"
        } else {
            ""
        }
    }

    pub fn last_err(&self) -> String {
        self.hook.lock().unwrap().err.trim().to_string()
    }

    fn send(&self, v: &serde_json::Value) -> Result<(), String> {
        let mut g = self.stdin.lock().unwrap();
        let i = g.as_mut().ok_or("OBS is not running")?;
        writeln!(i, "{v}").map_err(|_| "OBS is not running".to_string())
    }

    /// The hook never took. Some games cannot be hooked at all (an anti-cheat that
    /// refuses it, a graphics API OBS does not support).
    pub fn gave_up(&self) -> bool {
        !self.hook.lock().unwrap().hooked && self.started.elapsed() > HOOK_WAIT
    }

    pub fn stop(&self) {
        let _ = self.send(&json!({"cmd": "quit"}));
        drop(self.stdin.lock().unwrap().take());
        if !self.exited.wait(Duration::from_secs(10)) {
            winrec::kill_pid(self.pid);
            self.exited.wait(Duration::from_secs(5));
        }
    }

    fn segments(&self) -> Vec<PathBuf> {
        let Some(dir) = &self.dir else { return Vec::new() };
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                n.starts_with("seg-") && n.ends_with(".mkv")
            })
            .collect();
        files.sort(); // their names are their start times
        files
    }

    /// Deletes files too old for any clip.
    fn prune_loop(&self, buffer_sec: i32) {
        let keep = (buffer_sec / SEGMENT_SEC + 3) as usize;
        while !self.exited.wait(Duration::from_secs(SEGMENT_SEC as u64)) {
            let files = self.segments();
            for f in files.iter().take(files.len().saturating_sub(keep)) {
                let _ = std::fs::remove_file(f);
            }
        }
    }

    fn wait_saved(&self, d: Duration) -> Result<ObsEvent, String> {
        let g = self.saved.lock().unwrap();
        let deadline = Instant::now() + d;
        while Instant::now() < deadline {
            match g.recv_timeout(Duration::from_millis(100)) {
                Ok(e) => return Ok(e),
                Err(_) if self.exited.is_set() => return Err("OBS quit while saving".into()),
                Err(_) => {}
            }
        }
        Err("OBS took too long to save the replay".into())
    }

    /// Closes the file being written, so the newest moments can be read back, and
    /// joins enough of the files for `seconds` into one.
    fn join_segments(&self, seconds: i32, ready: Option<&(dyn Fn() + Sync)>) -> Result<PathBuf, String> {
        self.send(&json!({"cmd": "split"}))?;
        if let Some(f) = ready {
            f();
        }
        let e = self.wait_saved(Duration::from_secs(10))?;
        if e.event != "split" {
            return Err(format!("OBS could not save the replay: {}", e.msg));
        }
        let next = PathBuf::from(e.path.replace('/', "\\"));
        // The old file is closed once the new one has been opened.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !next.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let files = choose_segments(self.segments(), &next, seconds);
        if files.is_empty() {
            return Err("nothing has been recorded yet".into());
        }
        let dir = self.dir.as_ref().expect("segments have a folder");
        let list = dir.join("join.txt");
        std::fs::write(&list, concat_list(&files)).map_err(|e| e.to_string())?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let full = dir.join(format!("join-{stamp}.mp4"));
        let o = ffmpeg::command()
            .args(["-y", "-hide_banner", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i"])
            .arg(&list)
            .args(["-map", "0", "-c", "copy"])
            .arg(&full)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&list);
        if !o.status.success() {
            let _ = std::fs::remove_file(&full);
            return Err(format!("could not join the replay: {}", ffmpeg::first_line(&String::from_utf8_lossy(&o.stderr), "ffmpeg failed")));
        }
        Ok(full)
    }

    /// Gets the buffer into one file: from the helper's memory, or by joining its
    /// files.
    fn buffer_file(&self, seconds: i32, ready: Option<&(dyn Fn() + Sync)>) -> Result<PathBuf, String> {
        if self.dir.is_some() {
            return self.join_segments(seconds, ready);
        }
        self.send(&json!({"cmd": "save"}))?;
        if let Some(f) = ready {
            f();
        }
        let e = self.wait_saved(Duration::from_secs(90))?;
        if e.event != "saved" || e.path.is_empty() {
            return Err(format!("OBS could not save the replay: {}", e.msg));
        }
        Ok(PathBuf::from(e.path.replace('/', "\\")))
    }

    /// Cuts the last `seconds` of the buffer into out with the tracks titled. The cut
    /// is a stream copy, from the keyframe at or before the cut point (the helper
    /// puts one every second).
    pub fn save(&self, seconds: i32, out: &Path, titles: &[String], ready: Option<&(dyn Fn() + Sync)>) -> Result<(), String> {
        while self.saved.lock().unwrap().try_recv().is_ok() {}
        let full = self.buffer_file(seconds, ready)?;
        let mut cmd = ffmpeg::command();
        cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-sseof"]);
        cmd.arg(format!("-{seconds}")).arg("-i").arg(&full).args(["-map", "0", "-c", "copy"]);
        for (i, t) in titles.iter().enumerate() {
            cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("title={t}"));
        }
        cmd.arg(out);
        let o = cmd.stdin(Stdio::null()).output().map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&full);
        let o = o?;
        if !o.status.success() {
            let _ = std::fs::remove_file(out);
            return Err(format!("could not save the clip: {}", ffmpeg::first_line(&String::from_utf8_lossy(&o.stderr), "ffmpeg failed")));
        }
        Ok(())
    }
}

/// The files from before `next`, and enough of them: the last was cut short by the
/// split, so one more than a sum suggests. Empty files are skipped.
fn choose_segments(all: Vec<PathBuf>, next: &Path, seconds: i32) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = all
        .into_iter()
        .take_while(|f| f.as_path() < next)
        .filter(|f| f.metadata().map(|m| m.len() > 0).unwrap_or(false))
        .collect();
    let n = (seconds / SEGMENT_SEC + 2) as usize;
    if files.len() > n {
        files.drain(..files.len() - n);
    }
    files
}

/// The helper's audio tracks, laid out as audio::layout lays out the ffmpeg
/// recorder's.
pub fn titles(mic: bool) -> Vec<String> {
    let t: &[&str] = if mic { &[TITLE_MIX, TITLE_PC, TITLE_GAME, TITLE_MIC] } else { &[TITLE_PC, TITLE_GAME] };
    t.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_ceiling() {
        assert_eq!(max_kbps(1920, 1080, 60), 37324);
        assert_eq!(max_kbps(640, 360, 30), 8000);
        assert_eq!(max_kbps(3840, 2160, 120), 80000);
    }

    #[test]
    fn segments_chosen() {
        let dir = std::env::temp_dir().join(format!("miyav-seg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut all = Vec::new();
        for i in 0..8 {
            let f = dir.join(format!("seg-{i:02}.mkv"));
            std::fs::write(&f, if i == 2 { &b""[..] } else { &b"x"[..] }).unwrap();
            all.push(f);
        }
        let next = dir.join("seg-06.mkv");
        // 5 s of clip: 3 files, none from `next` on.
        let got = choose_segments(all.clone(), &next, 5);
        let names: Vec<_> = got.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["seg-03.mkv", "seg-04.mkv", "seg-05.mkv"]);
        // The empty one is skipped when reached.
        let got = choose_segments(all, &next, 60);
        assert_eq!(got.len(), 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Runs the real helper and saves a real clip from its files. The window it is
    /// pointed at does not exist, so nothing is hooked and the video is black: it
    /// tests the buffer, not the capture. Only runs when asked:
    ///
    ///   set MIYAV_OBS_DIR=<an OBS install's bin\64bit with miyav-obs.exe in it>
    ///   set MIYAV_FFMPEG=<ffmpeg.exe>
    ///   cargo test obs_disk_buffer_live -- --nocapture
    #[test]
    fn obs_disk_buffer_live() {
        let Some(bin) = std::env::var_os("MIYAV_OBS_DIR") else { return };
        for (enc, q, codec) in [("obs_nvenc_h264_tex", 23, "h264"), ("obs_nvenc_av1_tex", 30, "av1")] {
            let dir = std::env::temp_dir().join(format!("miyav-live-{}-{codec}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let helper = Path::new(&bin).join("miyav-obs.exe");
            let (p, ready) = spawn(&helper, Some(dir.clone()), codec == "av1").unwrap();
            let q2 = p.clone();
            std::thread::spawn(move || q2.prune_loop(10)); // keeps 5 files
            p.send(&json!({"cmd": "start", "window": "nosuch:nosuch:nosuchgame_miyav.exe",
                "width": 1920, "height": 1080, "fps": 60, "encoder": enc, "quality": q, "seconds": 30,
                "dir": dir.display().to_string(), "mic": true, "max_kbps": 37000, "segment_sec": SEGMENT_SEC}))
            .unwrap();
            let e = ready.recv_timeout(Duration::from_secs(30)).unwrap();
            assert_eq!(e.event, "started", "{}", e.msg);
            // Long enough that pruning has happened and the newest file is open.
            std::thread::sleep(Duration::from_secs(33));
            assert!(p.segments().len() <= 6, "{} files on disk", p.segments().len());
            let out = dir.join("clip.mp4");
            let t = Instant::now();
            p.save(12, &out, &titles(true), None).unwrap();
            println!("{codec}: saved in {:?}", t.elapsed());
            let info = ffmpeg::probe(&out).unwrap();
            assert!((10.5..14.0).contains(&info.duration_sec), "clip is {} s", info.duration_sec);
            assert_eq!(info.audio_tracks, titles(true));
            p.stop();
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
