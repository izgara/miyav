//! miyav-obs is the replay buffer recorded with OBS's own engine (libobs) rather
//! than ffmpeg. Games are captured through OBS's game-capture hook, so clips hold only
//! the game: no notifications, no Discord over it when the player tabs out. The hook
//! has to be OBS's signed, unmodified one, which is why this runs against the official
//! OBS release's binaries instead of a build of our own: anti-cheats know that
//! signature.
//!
//! It lives in OBS's bin\64bit folder (libobs finds obs-ffmpeg-mux.exe and its data
//! relative to the running exe and the working directory) and is driven by the app over
//! stdin/stdout, one JSON object per line. libobs is GPL, so this program is too; it
//! only talks to the app through that pipe.
mod libobs;
mod pcaudio;

use libobs::*;
use serde::Deserialize;
use serde_json::json;
use std::ffi::c_void;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Commands, from the app.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Command {
    cmd: String,
    // start
    window: String, // game window as "title:class:exe.exe"; empty captures any fullscreen app
    width: u32,     // canvas size: the game is fitted into it
    height: u32,
    fps: u32,
    encoder: String, // an OBS video encoder id, e.g. obs_nvenc_h264_tex
    quality: i64,    // constant quality, lower is better
    seconds: i64,    // replay buffer length
    dir: String,     // where saved clips go
    mic: bool,       // the default microphone
    max_size_mb: i64,
    max_kbps: i64,    // ceiling on the video bitrate, NVENC and x264
    segment_sec: i64, // >0: buffer in files of this length in dir, not in memory
    exclude_pid: u32, // >0: PC audio leaves this process out, the app and its beeps
}

static OUT: Mutex<()> = Mutex::new(());

/// Events, to the app.
fn emit(event: &str, msg: &str, path: &str) {
    let _g = OUT.lock().unwrap();
    let mut v = json!({"event": event});
    if !msg.is_empty() {
        v["msg"] = msg.into();
    }
    if !path.is_empty() {
        v["path"] = path.into();
    }
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

fn logf(msg: &str) {
    emit("log", msg, "");
}

fn fatal(msg: &str) -> ! {
    emit("error", msg, "");
    std::process::exit(1);
}

unsafe extern "C" fn log_handler(lvl: i32, msg: *const i8, args: *mut c_void, _: *mut c_void) {
    if lvl <= LOG_INFO {
        emit("log", &vsnprintf(msg, args), "");
    }
}

fn main() {
    let Ok(exe) = std::env::current_exe() else { fatal("cannot find myself") };
    let bin = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    // libobs looks for ../../data/libobs relative to the working directory.
    if let Err(e) = std::env::set_current_dir(&bin) {
        fatal(&e.to_string());
    }
    // win-capture fetches updates of its game compatibility list from obsproject.com on
    // every start, and firewalls ask the player about it. The list it ships with will
    // do: its libcurl is pointed at a proxy that is not there, so the check fails at
    // once without leaving the PC. Set before obs.dll loads, as the CRT copies the
    // environment when it starts.
    for k in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        std::env::set_var(k, "http://127.0.0.1:9");
    }
    std::env::set_var("NO_PROXY", "");
    if let Err(e) = load_obs(&bin) {
        fatal(&format!("loading obs.dll: {e}"));
    }
    call("base_set_log_handler", &[log_handler as *const () as usize, 0]);
    // Plugins keep a little state of their own; it goes beside the app's, not into the
    // install folder.
    let mut conf = 0;
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        let dir = PathBuf::from(base).join("Miyav").join("obs");
        if std::fs::create_dir_all(&dir).is_ok() {
            conf = cstr(&dir.display().to_string().replace('\\', "/"));
        }
    }
    if !call_bool("obs_startup", &[cstr("en-US"), conf, 0]) {
        fatal("obs_startup failed");
    }
    emit("ready", "", "");

    // A plain path, no "..": the hook DLL is handed to the game from under here.
    let core = bin.ancestors().nth(2).unwrap_or(&bin).join("core");
    let mut rec: Option<Recording> = None;
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let c: Command = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                emit("error", &format!("bad command: {e}"), "");
                continue;
            }
        };
        match c.cmd.as_str() {
            "start" => {
                if rec.is_some() {
                    emit("error", "already recording", "");
                    continue;
                }
                match start(&c, &core) {
                    Ok(r) => {
                        rec = Some(r);
                        emit("started", "", "");
                    }
                    Err(e) => emit("error", &e, ""),
                }
            }
            "save" | "split" => match &rec {
                None => emit("error", "not recording", ""),
                Some(r) => r.proc(if c.cmd == "save" { "save" } else { "split_file" }),
            },
            "quit" => {
                if let Some(mut r) = rec.take() {
                    r.stop();
                }
                call("obs_shutdown", &[]);
                emit("bye", "", "");
                return;
            }
            other => emit("error", &format!("unknown command {other}"), ""),
        }
    }
    // stdin closed: the app is gone.
    if let Some(mut r) = rec.take() {
        r.stop();
    }
    call("obs_shutdown", &[]);
}

static MODULES: OnceLock<()> = OnceLock::new();

/// Loads only the plugins the replay buffer uses, from the official OBS layout:
/// core\<name>\<name>.dll with its data beside it.
fn load_modules(core: &Path) -> Result<(), String> {
    if MODULES.get().is_some() {
        return Ok(());
    }
    for name in ["win-capture", "win-wasapi", "obs-ffmpeg", "obs-nvenc", "obs-x264"] {
        let dir = core.join(name);
        let slash = |p: PathBuf| p.display().to_string().replace('\\', "/");
        let mut module = 0usize;
        let r = call(
            "obs_open_module",
            &[&mut module as *mut usize as usize, cstr(&slash(dir.join(format!("{name}.dll")))), cstr(&slash(dir.join("data")))],
        ) as i32;
        if r != 0 {
            return Err(format!("opening OBS module {name}: error {r}"));
        }
        if !call_bool("obs_init_module", &[module]) {
            logf(&format!("OBS module {name} did not initialize"));
        }
    }
    call("obs_post_load_modules", &[]);
    let _ = MODULES.set(());
    Ok(())
}

#[derive(Default)]
struct Recording {
    output: usize,
    game: usize,
    scene: usize,
    sources: Vec<usize>,
    encoders: Vec<usize>,
    pc: Option<pcaudio::PcAudio>, // before the sources: it feeds one of them
}

/// Signalled when the replay buffer has finished stopping.
static STOPPED: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

/// Lays the tracks out as the ffmpeg recorder does (audio.rs): with a mic, PC audio +
/// mic mixed, then PC audio, the game and the mic each alone; without one, PC audio then
/// the game. Each source gets a bit per track it is heard on.
fn audio_mixers(mic: bool) -> (u32, u32, u32, usize) {
    if mic {
        (1 | 2, 4, 1 | 8, 4)
    } else {
        (1, 2, 0, 2)
    }
}

/// The video encoder's settings.
fn video_settings(enc: &str, q: i64, max_kbps: i64) -> Vec<(&'static str, V)> {
    let mut vs = vec![("keyint_sec", V::I(1))];
    if enc.contains("nvenc") {
        // lookahead is on by default since OBS 31. At 1440p it holds 150 MB of frames,
        // and the player gains nothing from it in a replay buffer.
        vs.extend([("preset", V::S("p4".into())), ("tune", V::S("hq".into())), ("bf", V::I(0)), ("lookahead", V::B(false))]);
        if max_kbps > 0 {
            vs.extend([("rate_control", V::S("CQVBR".into())), ("target_quality", V::I(q)), ("max_bitrate", V::I(max_kbps))]);
        } else {
            vs.extend([("rate_control", V::S("CQP".into())), ("cqp", V::I(q))]);
        }
    } else if enc.contains("amf") {
        vs.extend([("rate_control", V::S("CQP".into())), ("cqp", V::I(q))]);
    } else {
        vs.extend([("rate_control", V::S("CRF".into())), ("crf", V::I(q + 2)), ("preset", V::S("veryfast".into()))]);
        if max_kbps > 0 {
            vs.push(("x264opts", V::S(format!("vbv-maxrate={max_kbps} vbv-bufsize={}", 2 * max_kbps))));
        }
    }
    vs
}

fn start(c: &Command, core: &Path) -> Result<Recording, String> {
    let fps = if c.fps == 0 { 60 } else { c.fps };
    if c.width == 0 || c.height == 0 {
        return Err("canvas size missing".into());
    }
    let ovi = ObsVideoInfo {
        graphics_module: cstr("libobs-d3d11"),
        fps_num: fps,
        fps_den: 1,
        base_w: c.width,
        base_h: c.height,
        out_w: c.width,
        out_h: c.height,
        output_format: VIDEO_FORMAT_NV12,
        adapter: 0,
        gpu_conversion: true,
        colorspace: VIDEO_CS_709,
        range: VIDEO_RANGE_PARTIAL,
        scale_type: SCALE_BICUBIC,
    };
    let r = call("obs_reset_video", &[&ovi as *const ObsVideoInfo as usize]) as i32;
    if r != 0 {
        return Err(format!("starting OBS video failed ({r})"));
    }
    let oai = ObsAudioInfo { samples_per_sec: 48000, speakers: SPEAKERS_STEREO };
    if !call_bool("obs_reset_audio", &[&oai as *const ObsAudioInfo as usize]) {
        return Err("starting OBS audio failed".into());
    }
    load_modules(core)?;

    let mut r = Recording::default();
    let mut gs = vec![
        ("capture_cursor", V::B(true)),
        ("anti_cheat_hook", V::B(true)),
        ("capture_overlays", V::B(false)),
        ("allow_transparency", V::B(false)),
    ];
    if c.window.is_empty() {
        gs.push(("capture_mode", V::S("any_fullscreen".into())));
    } else {
        gs.extend([
            ("capture_mode", V::S("window".into())),
            ("window", V::S(c.window.clone())),
            ("priority", V::I(2)), // match by exe
        ]);
    }
    let d = obs_data(&gs);
    r.game = call("obs_source_create", &[cstr("game_capture"), cstr("game"), d, 0]);
    release_data(d);
    if r.game == 0 {
        return Err("could not create the game capture".into());
    }
    r.sources.push(r.game);
    let sh = call("obs_source_get_signal_handler", &[r.game]);
    call("signal_handler_connect", &[sh, cstr("hooked"), hooked_cb as *const () as usize, 0]);
    call("signal_handler_connect", &[sh, cstr("unhooked"), unhooked_cb as *const () as usize, 0]);

    r.scene = call("obs_scene_create", &[cstr("scene")]);
    let item = call("obs_scene_add", &[r.scene, r.game]);
    call("obs_sceneitem_set_bounds_type", &[item, BOUNDS_SCALE_INNER]);
    let b = Vec2 { x: c.width as f32, y: c.height as f32 };
    call("obs_sceneitem_set_bounds", &[item, &b as *const Vec2 as usize]);
    call("obs_set_output_source", &[0, call("obs_scene_get_source", &[r.scene])]);

    let (pc_mix, game_mix, mic_mix, tracks) = audio_mixers(c.mic);
    let audio = |r: &mut Recording, channel: usize, id: &str, name: &str, mixers: u32, settings: &[(&'static str, V)]| {
        let d = obs_data(settings);
        let src = call("obs_source_create", &[cstr(id), cstr(name), d, 0]);
        release_data(d);
        if src == 0 {
            logf(&format!("recording without {name}"));
            return;
        }
        call("obs_source_set_audio_mixers", &[src, mixers as usize]);
        call("obs_set_output_source", &[channel, src]);
        r.sources.push(src);
    };
    let ours = (c.exclude_pid != 0).then(|| pcaudio::start(c.exclude_pid));
    match ours {
        Some(Ok(p)) => {
            call("obs_source_set_audio_mixers", &[p.source, pc_mix as usize]);
            call("obs_set_output_source", &[1, p.source]);
            r.sources.push(p.source);
            r.pc = Some(p);
        }
        other => {
            if let Some(Err(e)) = other {
                logf(&format!("PC audio will include the beeps: {e}"));
            }
            audio(&mut r, 1, "wasapi_output_capture", "PC audio", pc_mix, &[("device_id", V::S("default".into()))]);
        }
    }
    if !c.window.is_empty() {
        // The game's process alone, found by the same window as the video.
        audio(
            &mut r,
            2,
            "wasapi_process_output_capture",
            "Game",
            game_mix,
            &[("window", V::S(c.window.clone())), ("priority", V::I(2))],
        );
    }
    if c.mic {
        audio(&mut r, 3, "wasapi_input_capture", "Microphone", mic_mix, &[("device_id", V::S("default".into()))]);
    }

    let enc = if c.encoder.is_empty() { "obs_nvenc_h264_tex" } else { &c.encoder };
    let q = if c.quality <= 0 { 20 } else { c.quality };
    let d = obs_data(&video_settings(enc, q, c.max_kbps));
    let venc = call("obs_video_encoder_create", &[cstr(enc), cstr("video"), d, 0]);
    release_data(d);
    if venc == 0 {
        r.release();
        return Err(format!("could not create the {enc} encoder"));
    }
    r.encoders.push(venc);
    call("obs_encoder_set_video", &[venc, call("obs_get_video", &[])]);

    let secs = if c.seconds <= 0 { 60 } else { c.seconds };
    let max_mb = if c.max_size_mb <= 0 { 4096 } else { c.max_size_mb };
    if c.segment_sec > 0 {
        // The buffer as a run of short files, which the app trims and joins: the encoded
        // video then costs no memory at all. Matroska reads back while still being
        // written, up to its last cluster; a short cluster keeps the newest second of the
        // game in the clip.
        let first = Path::new(&c.dir).join("seg-00000000-000000.mkv").display().to_string().replace('\\', "/");
        let d = obs_data(&[
            ("path", V::S(first)),
            ("directory", V::S(c.dir.clone())),
            ("format", V::S("seg-%CCYY%MM%DD-%hh%mm%ss".into())),
            ("extension", V::S("mkv".into())),
            ("allow_spaces", V::B(false)),
            ("split_file", V::B(true)),
            ("max_time_sec", V::I(c.segment_sec)),
            ("max_size_mb", V::I(0)),
            ("muxer_settings", V::S("cluster_time_limit=1000".into())),
        ]);
        r.output = call("obs_output_create", &[cstr("ffmpeg_muxer"), cstr("replay"), d, 0]);
        release_data(d);
    } else {
        let d = obs_data(&[
            ("directory", V::S(c.dir.clone())),
            ("format", V::S("Replay %CCYY-%MM-%DD %hh-%mm-%ss".into())),
            ("extension", V::S("mp4".into())),
            ("allow_spaces", V::B(true)),
            ("max_time_sec", V::I(secs)),
            ("max_size_mb", V::I(max_mb)),
        ]);
        r.output = call("obs_output_create", &[cstr("replay_buffer"), cstr("replay"), d, 0]);
        release_data(d);
    }
    if r.output == 0 {
        r.release();
        return Err("could not create the replay buffer".into());
    }
    call("obs_output_set_video_encoder", &[r.output, venc]);
    for i in 0..tracks {
        let d = obs_data(&[("bitrate", V::I(160))]);
        let aenc = call("obs_audio_encoder_create", &[cstr("ffmpeg_aac"), cstr(&format!("audio{i}")), d, i, 0]);
        release_data(d);
        if aenc == 0 {
            r.release();
            return Err("could not create the AAC encoder".into());
        }
        r.encoders.push(aenc);
        call("obs_encoder_set_audio", &[aenc, call("obs_get_audio", &[])]);
        call("obs_output_set_audio_encoder", &[r.output, aenc, i]);
    }
    let osh = call("obs_output_get_signal_handler", &[r.output]);
    call("signal_handler_connect", &[osh, cstr("saved"), saved_cb as *const () as usize, r.output]);
    call("signal_handler_connect", &[osh, cstr("stop"), stop_cb as *const () as usize, r.output]);
    call("signal_handler_connect", &[osh, cstr("file_changed"), split_cb as *const () as usize, r.output]);

    if !call_bool("obs_output_start", &[r.output]) {
        let e = format!("replay buffer did not start: {}", from_c(call("obs_output_get_last_error", &[r.output])));
        r.release();
        return Err(e);
    }
    Ok(r)
}

impl Recording {
    /// "save" writes out the memory buffer; "split_file" closes the file being written
    /// at the next keyframe, so what has been recorded so far can be read back whole.
    /// file_changed answers it.
    fn proc(&self, name: &str) {
        let mut cd = CallData::default();
        let ph = call("obs_output_get_proc_handler", &[self.output]);
        call("proc_handler_call", &[ph, cstr(name), &mut cd as *mut CallData as usize]);
        cd.free();
    }

    /// Ends the recording and frees all of it. libobs crashes on shutdown if anything
    /// is still alive, and the output stops asynchronously.
    fn stop(&mut self) {
        if call_bool("obs_output_active", &[self.output]) {
            let (tx, rx) = mpsc::channel();
            *STOPPED.lock().unwrap() = Some(tx);
            call("obs_output_stop", &[self.output]);
            if rx.recv_timeout(Duration::from_secs(5)).is_err() {
                logf("replay buffer took too long to stop");
                call("obs_output_force_stop", &[self.output]);
            }
        }
        self.release();
    }

    fn release(&mut self) {
        self.pc = None;
        for ch in 0..6 {
            call("obs_set_output_source", &[ch, 0]);
        }
        if self.output != 0 {
            call("obs_output_release", &[self.output]);
        }
        for &e in &self.encoders {
            call("obs_encoder_release", &[e]);
        }
        if self.scene != 0 {
            call("obs_scene_release", &[self.scene]);
        }
        for &s in &self.sources {
            call("obs_source_release", &[s]);
        }
        *self = Recording::default();
    }
}

unsafe extern "C" fn saved_cb(output: usize, _cd: usize) {
    let mut out = CallData::default();
    let ph = call("obs_output_get_proc_handler", &[output]);
    call("proc_handler_call", &[ph, cstr("get_last_replay"), &mut out as *mut CallData as usize]);
    emit("saved", "", &out.str("path"));
    out.free();
}

unsafe extern "C" fn split_cb(_: usize, cd: *mut CallData) {
    emit("split", "", &(*cd).str("next_file"));
}

unsafe extern "C" fn stop_cb(_: usize, cd: usize) {
    let mut code = 0i64;
    call("calldata_get_data", &[cd, cstr("code"), &mut code as *mut i64 as usize, 8]);
    emit("stopped", &code.to_string(), "");
    if let Some(tx) = STOPPED.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
}

unsafe extern "C" fn hooked_cb(_: usize, cd: *mut CallData) {
    emit("hooked", &(*cd).str("executable"), "");
}

unsafe extern "C" fn unhooked_cb(_: usize, _: usize) {
    emit("unhooked", "", "");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(vs: &'a [(&'static str, V)], k: &str) -> Option<&'a V> {
        vs.iter().find(|(n, _)| *n == k).map(|(_, v)| v)
    }

    #[test]
    fn nvenc_settings() {
        let vs = video_settings("obs_nvenc_h264_tex", 23, 37000);
        assert!(matches!(get(&vs, "lookahead"), Some(V::B(false))));
        assert!(matches!(get(&vs, "rate_control"), Some(V::S(s)) if s == "CQVBR"));
        assert!(matches!(get(&vs, "target_quality"), Some(V::I(23))));
        assert!(matches!(get(&vs, "max_bitrate"), Some(V::I(37000))));
        let vs = video_settings("obs_nvenc_h264_tex", 23, 0);
        assert!(matches!(get(&vs, "rate_control"), Some(V::S(s)) if s == "CQP"));
    }

    #[test]
    fn x264_settings() {
        let vs = video_settings("obs_x264", 23, 37000);
        assert!(matches!(get(&vs, "crf"), Some(V::I(25))));
        assert!(matches!(get(&vs, "x264opts"), Some(V::S(s)) if s == "vbv-maxrate=37000 vbv-bufsize=74000"));
    }

    #[test]
    fn mixers() {
        assert_eq!(audio_mixers(true), (3, 4, 9, 4));
        assert_eq!(audio_mixers(false), (1, 2, 0, 2));
    }

    #[test]
    fn struct_layouts_match_libobs() {
        assert_eq!(std::mem::size_of::<ObsVideoInfo>(), 56);
        assert_eq!(std::mem::size_of::<ObsAudioInfo>(), 8);
        assert_eq!(std::mem::size_of::<CallData>(), 32);
    }
}
