use regex::Regex;
use serde::Serialize;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use crate::job::{Ctx, Progress};
use crate::settings::{self, QUALITY_PRESETS};
use crate::win::hide_window;

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub duration_sec: f64,
    pub width: i32,
    pub height: i32,
    pub size_bytes: u64,
    /// One name per audio track, in order: the track title if it has one (our
    /// recordings do), else "Track N".
    pub audio_tracks: Vec<String>,
    /// The first audio track's codec, "aac" say. Empty without audio.
    pub audio_codec: String,
}

static FFMPEG: OnceLock<PathBuf> = OnceLock::new();
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

pub fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

/// ffmpeg.exe beside our own executable, where the setup and the portable zip put
/// it. MIYAV_FFMPEG is for the live tests.
pub fn path() -> &'static Path {
    FFMPEG.get_or_init(|| {
        if let Some(p) = std::env::var_os("MIYAV_FFMPEG") {
            return p.into();
        }
        exe_path().parent().map(|dir| dir.join("ffmpeg.exe")).unwrap_or_default()
    })
}

pub fn tools_ready() -> bool {
    path().metadata().map(|m| m.len() > 0).unwrap_or(false)
}

pub fn command() -> Command {
    let mut c = Command::new(path());
    hide_window(&mut c);
    c
}

static RE_DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Duration:\s*(\d+):(\d+):(\d+(?:\.\d+)?)").unwrap());
static RE_DIMS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Video:.*?(\d{2,5})x(\d{2,5})").unwrap());
static RE_AUDIO_CODEC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Audio:\s*(\w+)").unwrap());
static RE_TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+(?:title|name)\s*:\s*(.+?)\s*$").unwrap());
static RE_OUT_TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"out_time_us=(\d+)").unwrap());

/// Reads duration and dimensions from ffmpeg's own banner. Using ffmpeg rather
/// than ffprobe means we only ship one binary instead of two nearly identical
/// large executables.
pub fn probe(file: &Path) -> Result<MediaInfo, String> {
    let mut info = MediaInfo { size_bytes: file.metadata().map_err(|e| e.to_string())?.len(), ..Default::default() };
    // "-i <file>" with no output makes ffmpeg print the stream banner and exit
    // non-zero; that exit status is expected, so only the output is inspected.
    let out = command().args(["-hide_banner", "-i"]).arg(file).output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stderr);
    parse_banner(&text, &mut info);
    if info.duration_sec <= 0.0 {
        return Err("could not read the video (is it still being written?)".into());
    }
    Ok(info)
}

fn parse_banner(text: &str, info: &mut MediaInfo) {
    if let Some(m) = RE_DURATION.captures(text) {
        let f = |i: usize| m[i].parse::<f64>().unwrap_or(0.0);
        info.duration_sec = f(1) * 3600.0 + f(2) * 60.0 + f(3);
    }
    if let Some(m) = RE_DIMS.captures(text) {
        info.width = m[1].parse().unwrap_or(0);
        info.height = m[2].parse().unwrap_or(0);
    }
    info.audio_tracks = audio_track_names(text);
    info.audio_codec = RE_AUDIO_CODEC.captures(text).map(|m| m[1].to_string()).unwrap_or_default();
}

/// Reads the audio streams out of ffmpeg's banner. A stream's metadata lines
/// follow its "Stream #" line, up to the next stream.
fn audio_track_names(banner: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_audio = false;
    for line in banner.lines() {
        if line.contains("Stream #") {
            in_audio = line.contains(": Audio:");
            if in_audio {
                names.push(format!("Track {}", names.len() + 1));
            }
            continue;
        }
        if in_audio {
            if let Some(m) = RE_TITLE.captures(line) {
                *names.last_mut().unwrap() = m[1].to_string();
            }
        }
    }
    names
}

/// Streams -progress output so the UI gets a real percentage. The context is
/// watched throughout: tripping it kills ffmpeg.
fn run(
    ctx: &Ctx,
    args: &[String],
    duration_sec: f64,
    phase: &str,
    attempt: i32,
    crf: i32,
    on_progress: &(dyn Fn(Progress) + Sync),
) -> Result<(), String> {
    let mut cmd = command();
    if phase == "encode" {
        // A game may be running, and it comes first.
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    let mut child = cmd
        .args(["-y", "-hide_banner", "-loglevel", "error", "-progress", "pipe:1", "-nostats"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let err_buf = std::sync::Arc::new(Mutex::new(String::new()));
    let eb = err_buf.clone();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        *eb.lock().unwrap() = s;
    });
    std::thread::scope(|sc| {
        sc.spawn(|| {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(m) = RE_OUT_TIME.captures(&line) {
                    if duration_sec > 0.0 {
                        let us: f64 = m[1].parse().unwrap_or(0.0);
                        let pct = (us / 1e6 / duration_sec * 100.0).min(99.0);
                        on_progress(Progress { phase: phase.into(), percent: pct, attempt, crf });
                    }
                }
            }
        });
        let status = loop {
            if ctx.done() {
                let _ = child.kill();
                let _ = child.wait();
                return Err("cancelled".to_string());
            }
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(e.to_string()),
            }
        };
        if !status.success() {
            std::thread::sleep(Duration::from_millis(50));
            let mut detail = err_buf.lock().unwrap().trim().to_string();
            if detail.len() > 400 {
                let mut i = detail.len() - 400;
                while !detail.is_char_boundary(i) {
                    i += 1;
                }
                detail = detail[i..].to_string();
            }
            if detail.is_empty() {
                detail = status.to_string();
            }
            return Err(format!("ffmpeg failed: {detail}"));
        }
        on_progress(Progress { phase: phase.into(), percent: 100.0, attempt, crf });
        Ok(())
    })
}

/// Copies the selected range without re-encoding. This is exactly what
/// LosslessCut does, and it takes about a second regardless of clip length.
/// Placing -ss before -i seeks by keyframe, so the cut can land slightly before
/// the requested start - that is the price of not re-encoding.
///
/// main_audio picks which audio track comes first - the one browsers and
/// most players play - for a recording that keeps game, mic and the mix apart; -1
/// keeps the order. Every track is kept either way, so nothing is lost.
pub fn cut_lossless(
    ctx: &Ctx,
    input: &Path,
    out: &Path,
    start: f64,
    end: f64,
    main_audio: i32,
    on_progress: &(dyn Fn(Progress) + Sync),
) -> Result<(), String> {
    if end <= start {
        return Err("the end point must come after the start point".into());
    }
    let mut args: Vec<String> = vec![
        "-ss".into(),
        format!("{start:.3}"),
        "-to".into(),
        format!("{end:.3}"),
        "-i".into(),
        input.display().to_string(),
        "-map".into(),
        "0:v:0".into(),
    ];
    let n = probe(input).map(|i| i.audio_tracks.len() as i32).unwrap_or(0);
    args.extend(audio_map_args(main_audio, n));
    for a in ["-c", "copy", "-avoid_negative_ts", "make_zero", "-movflags", "+faststart"] {
        args.push(a.into());
    }
    args.push(out.display().to_string());
    run(ctx, &args, end - start, "cut", 1, 0, on_progress)
}

fn audio_map_args(main_audio: i32, n: i32) -> Vec<String> {
    let mut args = Vec::new();
    if main_audio <= 0 || main_audio >= n {
        args.extend(["-map".to_string(), "0:a?".to_string()]);
        return args;
    }
    args.extend(["-map".to_string(), format!("0:a:{main_audio}")]);
    for i in (0..n).filter(|&i| i != main_audio) {
        args.extend(["-map".to_string(), format!("0:a:{i}")]);
    }
    for i in 0..n {
        args.push(format!("-disposition:a:{i}"));
        args.push(if i == 0 { "default" } else { "0" }.to_string());
    }
    args
}

/// A result has to save this much to be worth a second generation of the video.
const MIN_SAVING_PERCENT: u64 = 5;

/// NVENC's quality level at each height for the "balanced" preset. Measured
/// 2026-11-02 on Dota 2, Fallout 4, Rust and The Witcher 3 re-encoding NVENC CQ 23
/// recordings: the highest level whose mean VMAF stayed 95+ against the recording
/// in every game. Fewer pixels show damage sooner, so 720p and 1080p need a lower
/// level than 1440p.
fn gpu_cq(height: i32) -> i32 {
    if height > 1080 { 25 } else { 24 }
}

/// Ordered by preference, GPU first. All H.264 High 8-bit with AAC and faststart, as
/// that is what Discord and every browser play without transcoding. The CPU comes
/// last, on half the cores, as it took 8x the CPU time of NVENC.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ShrinkEncoder {
    Nvenc,
    /// For older NVIDIA cards, which refuse some of what Nvenc asks for.
    NvencPlain,
    Amf,
    /// Windows' encoder layer: QuickSync on Intel, as our ffmpeg has no h264_qsv.
    WindowsHardware,
    Cpu,
}

const SHRINK_ENCODERS: [ShrinkEncoder; 5] =
    [ShrinkEncoder::Nvenc, ShrinkEncoder::NvencPlain, ShrinkEncoder::Amf, ShrinkEncoder::WindowsHardware, ShrinkEncoder::Cpu];

fn strings(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// The encoder's half of the command. offset raises the GPU quality level, and crf
/// is what libx264 uses in its place; the number returned is the one to show.
fn video_args(enc: ShrinkEncoder, height: i32, offset: i32, crf: i32, preset: &str) -> (Vec<String>, i32) {
    let cq = gpu_cq(height) + offset;
    let nvenc_rate = |cq: i32| {
        // The cap is well above anything these levels need: the hard scenes that
        // dropped furthest did not improve when it was raised.
        let mut a = strings(&["-c:v", "h264_nvenc", "-preset", "p7", "-tune", "hq", "-rc", "vbr", "-cq"]);
        a.push(cq.to_string());
        a.extend(strings(&["-b:v", "0", "-maxrate", "80M", "-bufsize", "160M", "-spatial-aq", "1", "-aq-strength", "4"]));
        a
    };
    let args = match enc {
        ShrinkEncoder::Nvenc => {
            let mut a = nvenc_rate(cq);
            a.extend(strings(&["-multipass", "fullres", "-temporal-aq", "1", "-rc-lookahead", "32", "-bf", "3", "-b_ref_mode", "middle"]));
            a
        }
        ShrinkEncoder::NvencPlain => nvenc_rate(cq),
        ShrinkEncoder::Amf => {
            let (i, p, b) = (cq - 4, cq - 2, cq);
            let mut a = strings(&["-c:v", "h264_amf", "-quality", "quality", "-rc", "cqp", "-qp_i"]);
            a.push(i.to_string());
            a.push("-qp_p".into());
            a.push(p.to_string());
            a.push("-qp_b".into());
            a.push(b.to_string());
            a
        }
        ShrinkEncoder::WindowsHardware => {
            let mut a = strings(&["-c:v", "h264_mf", "-hw_encoding", "1", "-rate_control", "quality", "-quality"]);
            a.push((80 - offset * 5).clamp(1, 100).to_string());
            a
        }
        ShrinkEncoder::Cpu => {
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2;
            let mut a = strings(&["-c:v", "libx264", "-crf"]);
            a.push(crf.to_string());
            a.extend(["-preset".to_string(), preset.to_string(), "-threads".to_string(), threads.max(2).to_string()]);
            a
        }
    };
    (args, if enc == ShrinkEncoder::Cpu { crf } else { cq })
}

fn audio_args(codec: &str, kbps: i32) -> Vec<String> {
    if codec == "aac" {
        // Already what everything plays: copying it keeps it exactly as it was.
        strings(&["-c:a", "copy"])
    } else {
        vec!["-c:a".into(), "aac".into(), "-b:a".into(), format!("{kbps}k")]
    }
}

/// Worth keeping, compared with the original?
fn keep_encode(input: u64, output: u64) -> bool {
    output * 100 <= input * (100 - MIN_SAVING_PERCENT)
}

/// Re-encodes at a quality preset, keeping the resolution and frame rate unless the
/// preset caps it. Returns false when nothing was gained, and then out does not
/// exist: the caller carries on with the original.
pub fn encode(
    ctx: &Ctx,
    input: &Path,
    out: &Path,
    preset_name: &str,
    on_progress: &(dyn Fn(Progress) + Sync),
) -> Result<bool, String> {
    let preset = QUALITY_PRESETS.get(preset_name).unwrap_or_else(|| &QUALITY_PRESETS[settings::DEFAULT_QUALITY]);
    let info = probe(input)?;
    let mut last_error = "no encoder works on this PC".to_string();
    for &enc in SHRINK_ENCODERS.iter() {
        let mut args: Vec<String> = ["-i", &input.display().to_string()].iter().map(|s| s.to_string()).collect();
        // The first audio track only: it is the one a browser would play, and
        // for our recordings it is the mix of everything.
        args.extend(strings(&["-map", "0:v:0", "-map", "0:a:0?"]));
        let (video, shown) = video_args(enc, info.height, preset.gpu_offset, preset.crf, preset.preset);
        args.extend(video);
        if enc != ShrinkEncoder::WindowsHardware {
            // h264_mf refuses this by name, and takes no other spelling of it.
            args.extend(strings(&["-profile:v", "high"]));
        }
        args.extend(strings(&["-pix_fmt", if enc == ShrinkEncoder::WindowsHardware { "nv12" } else { "yuv420p" }, "-g", "240"]));
        if preset.max_height > 0 && info.height > preset.max_height {
            // quoted so ffmpeg does not treat the comma as a filter separator
            args.push("-vf".into());
            args.push(format!("scale=-2:'min({},ih)'", preset.max_height));
        }
        args.extend(audio_args(&info.audio_codec, preset.audio_kbps));
        args.extend(["-movflags".to_string(), "+faststart".to_string(), out.display().to_string()]);

        if let Err(e) = run(ctx, &args, info.duration_sec, "encode", 1, shown, on_progress) {
            if ctx.done() {
                return Err(e);
            }
            // Not a card this one runs on, or its sessions are taken: try the next.
            last_error = e;
            continue;
        }
        let size = out.metadata().map_err(|e| e.to_string())?.len();
        if keep_encode(info.size_bytes, size) {
            return Ok(true);
        }
        let _ = std::fs::remove_file(out);
        return Ok(false);
    }
    Err(last_error)
}

/// The first line of ffmpeg's complaint, else the error it died with.
pub fn first_line(text: &str, err: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return err.to_string();
    }
    t.lines().next().unwrap_or(t).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BANNER: &str = "Input #0, matroska,webm, from 'x.mkv':\n  Duration: 00:01:02.50, start: 0.000000, bitrate: 500 kb/s\n  Stream #0:0: Video: h264 (High), yuv420p, 2560x1440, 60 fps\n  Stream #0:1: Audio: aac (LC), 48000 Hz, stereo\n    Metadata:\n      title           : PC audio + mic\n  Stream #0:2: Audio: aac (LC), 48000 Hz, stereo\n";

    #[test]
    fn shrink_quality_by_height() {
        assert_eq!(gpu_cq(720), 24);
        assert_eq!(gpu_cq(1080), 24);
        assert_eq!(gpu_cq(1440), 25);
        let (a, shown) = video_args(ShrinkEncoder::Nvenc, 1440, 3, 18, "faster");
        assert_eq!(shown, 28);
        assert!(a.windows(2).any(|w| w == ["-cq", "28"]));
        let (a, shown) = video_args(ShrinkEncoder::Cpu, 1440, 0, 18, "faster");
        assert_eq!(shown, 18);
        assert!(a.windows(2).any(|w| w == ["-crf", "18"]));
    }

    #[test]
    fn shrink_audio_is_copied_when_it_can_be() {
        assert_eq!(audio_args("aac", 192), ["-c:a", "copy"]);
        assert_eq!(audio_args("opus", 192), ["-c:a", "aac", "-b:a", "192k"]);
    }

    #[test]
    fn shrink_keeps_only_what_is_worth_it() {
        assert!(keep_encode(100, 90));
        assert!(!keep_encode(100, 97));
        assert!(!keep_encode(100, 120));
    }

    #[test]
    fn reads_the_banner() {
        let mut i = MediaInfo::default();
        parse_banner(BANNER, &mut i);
        assert_eq!(i.duration_sec, 62.5);
        assert_eq!((i.width, i.height), (2560, 1440));
        assert_eq!(i.audio_tracks, ["PC audio + mic", "Track 2"]);
    }

    #[test]
    fn audio_order() {
        assert_eq!(audio_map_args(0, 3), ["-map", "0:a?"]);
        assert_eq!(audio_map_args(5, 3), ["-map", "0:a?"]);
        let a = audio_map_args(2, 3);
        assert_eq!(a[..6], ["-map", "0:a:2", "-map", "0:a:0", "-map", "0:a:1"]);
        assert_eq!(a[6..8], ["-disposition:a:0", "default"]);
    }
}
