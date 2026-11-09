use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

use crate::games::CustomApp;

/// A named quality target. It encodes to a *quality* level rather than a size, so
/// a 15s clip and a 10min clip look the same.
#[derive(Clone, Serialize)]
pub struct QualityPreset {
    pub crf: i32, // libx264, used only without a working GPU encoder
    pub preset: &'static str,
    /// Added to the GPU encoders' quality for this height (ffmpeg::gpu_cq).
    #[serde(rename = "gpuOffset")]
    pub gpu_offset: i32,
    #[serde(rename = "maxHeight")]
    pub max_height: i32, // 0 = keep source resolution
    #[serde(rename = "audioKbps")]
    pub audio_kbps: i32,
}

/// Re-encoding is always lossy. "balanced" is where game footage measured VMAF 95+
/// against the recording, see ffmpeg::gpu_cq.
pub static QUALITY_PRESETS: LazyLock<HashMap<&'static str, QualityPreset>> = LazyLock::new(|| {
    HashMap::from([
        ("high", QualityPreset { crf: 16, preset: "faster", gpu_offset: -2, max_height: 0, audio_kbps: 192 }),
        ("balanced", QualityPreset { crf: 18, preset: "faster", gpu_offset: 0, max_height: 0, audio_kbps: 192 }),
        ("small", QualityPreset { crf: 23, preset: "faster", gpu_offset: 3, max_height: 1080, audio_kbps: 128 }),
    ])
});

pub const DEFAULT_QUALITY: &str = "balanced";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub raw_folder: String,
    pub clips_folder: String,
    pub quality: String,
    /// The first-start wizard is finished. Nothing records before that.
    pub setup_done: bool,
    /// When true a cut deletes its source recording without asking.
    pub always_delete_source: bool,

    // Replay buffer - see recorder.rs.
    pub record_enabled: bool,
    pub record_mode: String, // "game" follows the game window, "screen" records the monitor
    /// Windows' name of the monitor "screen" mode records (\.DISPLAY2); "" is the main one.
    pub record_monitor: String,
    pub replay_seconds: i32,
    pub record_fps: i32,
    pub record_quality: i32, // 1 best .. 51 worst; 23 is typical
    pub record_encoder: String, // "" picks the best that works
    pub record_codec: String, // "av1" where the GPU can; anything else is H.264
    pub record_mic: bool,
    pub hotkey: String,
    /// Flips between games only and whole screen until the app restarts (recorder.rs).
    pub swap_hotkey: String,
    /// After a flip to whole screen, one saved clip flips back.
    pub swap_back_after_clip: bool,
    /// Keeps OBS's buffer in RAM rather than in files on disk.
    pub replay_in_memory: bool,
    /// Saves a clip of every kill, where the game reports them (autoclip.rs).
    pub auto_clip: bool,
    /// Recorded like games though nothing says they are (games.rs).
    pub custom_apps: Vec<CustomApp>,
    /// Caps what the recorder's own clips may take up; the oldest go first
    /// (storage.rs). 0 keeps everything.
    #[serde(rename = "storageLimitGB")]
    pub storage_limit_gb: i64,
    /// Turns off the "Clip saved" notification.
    pub quiet_saves: bool,
    /// The "still running in the tray" note is shown the first time the window
    /// is closed, not every time.
    pub tray_hint_shown: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            raw_folder: default_folder("Miyav"),
            clips_folder: default_folder("Miyav Clips"),
            quality: DEFAULT_QUALITY.into(),
            setup_done: false,
            always_delete_source: false,
            record_enabled: false,
            record_mode: "game".into(),
            record_monitor: String::new(),
            replay_seconds: 60,
            record_fps: 60,
            record_quality: 23,
            record_encoder: String::new(),
            record_codec: String::new(),
            record_mic: false,
            // A letter rather than an F-key: on most laptops F-keys need Fn held
            // as well. Not Alt+Z (GeForce Experience) or Alt+R (AMD), and games
            // seldom bind Alt+C.
            hotkey: "Alt+C".into(),
            swap_hotkey: "Ctrl+Alt+Insert".into(),
            swap_back_after_clip: true,
            replay_in_memory: false,
            auto_clip: false,
            custom_apps: Vec::new(),
            storage_limit_gb: 50,
            quiet_saves: false,
            tray_hint_shown: false,
        }
    }
}

/// Keeps a hand-edited settings.json from starting a recorder that cannot work.
pub fn clamp_record_settings(s: &mut Settings) {
    let d = Settings::default();
    if s.record_mode != "game" && s.record_mode != "screen" {
        s.record_mode = d.record_mode;
    }
    if !(10..=600).contains(&s.replay_seconds) {
        s.replay_seconds = d.replay_seconds;
    }
    if !(10..=240).contains(&s.record_fps) {
        s.record_fps = d.record_fps;
    }
    if !(1..=51).contains(&s.record_quality) {
        s.record_quality = d.record_quality;
    }
    if s.hotkey.is_empty() {
        s.hotkey = d.hotkey;
    }
    if s.storage_limit_gb < 0 {
        s.storage_limit_gb = 0;
    }
}

static SETTINGS: LazyLock<RwLock<Settings>> = LazyLock::new(|| RwLock::new(Settings::default()));

pub fn get() -> Settings {
    SETTINGS.read().unwrap().clone()
}

pub fn update(f: impl FnOnce(&mut Settings)) {
    f(&mut SETTINGS.write().unwrap());
}

/// %APPDATA%\Miyav - settings, ffmpeg and thumbnails live here.
pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Miyav")
}

fn path() -> PathBuf {
    app_dir().join("settings.json")
}

/// A folder under the user's Videos. Cut clips sit beside the recordings rather
/// than inside them, or they would turn up among the recordings, whose sub-folders
/// are scanned too.
pub fn default_folder(name: &str) -> String {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
    home.join("Videos").join(name).display().to_string()
}

/// Reads the settings, or writes the defaults when there are none yet.
pub fn load() -> Result<(), String> {
    std::fs::create_dir_all(app_dir()).map_err(|e| e.to_string())?;
    let raw = match std::fs::read(path()) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return save(),
        Err(e) => return Err(e.to_string()),
    };
    let Ok(mut s) = serde_json::from_slice::<Settings>(&raw) else {
        // A corrupt file must not stop the app starting: keep the bad copy for
        // diagnosis and carry on with defaults.
        let _ = std::fs::rename(path(), path().with_extension("json.corrupt"));
        return save();
    };
    if !QUALITY_PRESETS.contains_key(s.quality.as_str()) {
        s.quality = DEFAULT_QUALITY.into();
    }
    clamp_record_settings(&mut s);
    if s.setup_done {
        for dir in [&s.raw_folder, &s.clips_folder] {
            let _ = std::fs::create_dir_all(dir);
        }
    } else {
        s.record_enabled = false;
    }
    *SETTINGS.write().unwrap() = s;
    Ok(())
}

pub fn save() -> Result<(), String> {
    let out = serde_json::to_vec_pretty(&*SETTINGS.read().unwrap()).map_err(|e| e.to_string())?;
    let tmp = path().with_extension("json.tmp");
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    // atomic replace so a crash mid-write cannot truncate the config
    std::fs::rename(&tmp, path()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_their_defaults() {
        let s: Settings = serde_json::from_str(
            r#"{"recordFps":120,"storageLimitGB":20,"replayInMemory":true,"autoClip":true,
                "customApps":[{"name":"Discord","exe":"Discord.exe"}],"unknownField":1}"#,
        )
        .unwrap();
        assert_eq!(s.record_fps, 120);
        assert_eq!(s.storage_limit_gb, 20);
        assert!(s.replay_in_memory && s.auto_clip);
        assert_eq!(s.custom_apps[0].exe, "Discord.exe");
        assert_eq!(s.hotkey, "Alt+C");
        assert!(!s.setup_done && !s.record_enabled && !s.record_mic);
        let back = serde_json::to_value(&s).unwrap();
        for k in ["storageLimitGB", "recordCodec", "trayHintShown"] {
            assert!(back.get(k).is_some(), "{k}");
        }
    }
}
