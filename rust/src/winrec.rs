//! The Windows side of the recorder: the global hotkey, the beeps, and which
//! window is in front.
use std::ffi::c_void;
use std::path::Path;
use std::process::Child;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Media::Audio::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::audio::{method, release};
use crate::games;
use crate::recorder::rec_log;
use crate::win::{from_wide, wide};

/// A window to record, or the whole screen when hwnd is 0.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Target {
    pub hwnd: isize,
    pub app: String, // exe name without .exe; names the clip's folder
    pub pid: u32,    // the game's process, for its own audio track
    /// The game fills its monitor, so that monitor (DXGI output `output`) is
    /// recorded rather than the window. Window capture pushes a fullscreen game's
    /// frames through the compositor and takes its cursor, which players feel as
    /// lag - League most of all.
    pub display: bool,
    pub output: i32,
    /// A display game the player has tabbed away from, recorded through its window
    /// meanwhile so the clip shows the game rather than Discord or a browser. Being
    /// behind other windows it is composed anyway, so window capture costs it
    /// nothing then.
    pub behind: bool,
    /// Recorded by the OBS helper's game capture (obsrec.rs), which needs none of
    /// the above - it follows the game through alt-tabs by itself.
    pub obs: bool,
    /// On the list only because the player added it.
    pub custom: bool,
    /// Recorded through its window, as OBS's hook does not belong in it.
    pub no_hook: bool,
}

// ---- the time, for logs and clip names ----

fn system_time() -> windows_sys::Win32::Foundation::SYSTEMTIME {
    unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    }
}

pub fn local_time() -> String {
    let t = system_time();
    format!("{}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// For file names: no colons.
pub fn local_stamp() -> String {
    local_time().replace(':', "-")
}

// ---- children ----

pub fn bind_child(c: &Child) {
    crate::win::bind_to_app(c.id());
}

pub fn kill_pid(pid: u32) {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h != 0 {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

// ---- global hotkey ----

const WM_HOTKEY_MSG: u32 = 0x0312;

struct Hotkey {
    thread: u32,
    done: mpsc::Receiver<()>,
}

/// RegisterHotKey delivers WM_HOTKEY to the thread that registered it, so the
/// hotkey gets a dedicated OS thread running its own message loop. Changing the
/// hotkey stops that thread and starts a new one.
static HOTKEY: Mutex<Option<Hotkey>> = Mutex::new(None);

// keyHook is the second way a press is seen. RegisterHotKey only fires when
// exactly its modifiers are down, and in a game something else usually is - in
// League, Shift, Ctrl or Alt for casting - so plain Insert went unnoticed most of
// the time. The hook sees the key whatever else is held, and only watches: it never
// stops the key reaching the game. It runs on the hotkey thread.
static HOOK_VK: AtomicU32 = AtomicU32::new(0);
static HOOK_MODS: AtomicU32 = AtomicU32::new(0);
static HOOK_DOWN: AtomicU32 = AtomicU32::new(0); // held, so auto-repeat does not save a clip per repeat
static HOOK_FN: Mutex<Option<fn()>> = Mutex::new(None);
static LAST_FIRE: AtomicI64 = AtomicI64::new(0);

/// Runs the hotkey's action once per press: a plain press reaches both the hook and
/// WM_HOTKEY, a few milliseconds apart.
fn fire() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    if now - LAST_FIRE.swap(now, Ordering::SeqCst) < 400 {
        return;
    }
    if let Some(f) = *HOOK_FN.lock().unwrap() {
        std::thread::spawn(f);
    }
}

/// The hotkey's own modifiers must be down; others may be too.
fn mods_held(mods: u32) -> bool {
    let held = |vks: &[i32]| vks.iter().any(|&vk| unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0);
    (mods & MOD_ALT == 0 || held(&[0x12]))
        && (mods & MOD_CONTROL == 0 || held(&[0x11]))
        && (mods & MOD_SHIFT == 0 || held(&[0x10]))
        && (mods & MOD_WIN == 0 || held(&[0x5B, 0x5C]))
}

unsafe extern "system" fn key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == 0 {
        let vk = (*(lp as *const KBDLLHOOKSTRUCT)).vkCode;
        if vk == HOOK_VK.load(Ordering::Relaxed) {
            match wp as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    if HOOK_DOWN.swap(1, Ordering::Relaxed) == 0 && mods_held(HOOK_MODS.load(Ordering::Relaxed)) {
                        fire();
                    }
                }
                WM_KEYUP | WM_SYSKEYUP => HOOK_DOWN.store(0, Ordering::Relaxed),
                _ => {}
            }
        }
    }
    CallNextHookEx(0, code, wp, lp)
}

pub fn set_hotkey(spec: &str, f: Option<fn()>) -> Result<(), String> {
    let mut hk = HOTKEY.lock().unwrap();
    if let Some(h) = hk.take() {
        unsafe { PostThreadMessageW(h.thread, WM_QUIT, 0, 0) };
        let _ = h.done.recv();
    }
    let Some(f) = f.filter(|_| !spec.is_empty()) else { return Ok(()) };
    let (mods, vk) = parse_hotkey(spec)?;
    // RegisterHotKey swallows the key. The key under Esc opens the console in games
    // and types " on Turkish keyboards, so it is only watched, by the hook, and
    // keeps doing its job.
    let hook_only = vk == backquote_vk();
    *HOOK_FN.lock().unwrap() = Some(f);
    HOOK_VK.store(vk, Ordering::SeqCst);
    HOOK_MODS.store(mods, Ordering::SeqCst);
    HOOK_DOWN.store(0, Ordering::SeqCst);

    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
    let (done_tx, done_rx) = mpsc::channel();
    let spec = spec.to_string();
    std::thread::spawn(move || unsafe {
        let _done = done_tx;
        if !hook_only && RegisterHotKey(0, 1, mods | MOD_NOREPEAT, vk) == 0 {
            let _ = ready_tx.send(Err(format!(
                "{spec} is already used by another program (Shadowplay uses Alt+F10) - pick a different hotkey"
            )));
            return;
        }
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), GetModuleHandleW(std::ptr::null()), 0);
        if hook == 0 {
            if hook_only {
                let _ = ready_tx.send(Err(format!("{spec} cannot be watched on this PC - pick a different hotkey")));
                return;
            }
            rec_log(&format!("keyboard hook unavailable, {spec} only works with no other keys held"));
        }
        let _ = ready_tx.send(Ok(GetCurrentThreadId()));
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            if msg.message == WM_HOTKEY_MSG {
                fire();
            }
        }
        if hook != 0 {
            UnhookWindowsHookEx(hook);
        }
        if !hook_only {
            UnregisterHotKey(0, 1);
        }
    });
    match ready_rx.recv() {
        Ok(Ok(thread)) => {
            *hk = Some(Hotkey { thread, done: done_rx });
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the hotkey thread stopped".into()),
    }
}

/// The key under Esc, "Backquote" in a hotkey. It is named by where it sits (scan
/// code 0x29), as the browser names it; which virtual key it sends depends on the
/// keyboard layout.
fn backquote_vk() -> u32 {
    match unsafe { MapVirtualKeyW(0x29, MAPVK_VSC_TO_VK) } {
        0 => 0xC0, // VK_OEM_3
        vk => vk,
    }
}

/// The keys besides F-keys, letters and digits that make sense as a clip hotkey -
/// the ones games rarely use, which is why Insert and Home are popular choices.
fn named_key(p: &str) -> u32 {
    match p {
        "INSERT" => 0x2D,
        "DELETE" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" => 0x21,
        "PAGEDOWN" => 0x22,
        "PAUSE" => 0x13,
        "SCROLLLOCK" => 0x91,
        _ => match p.strip_prefix("NUM").and_then(|d| d.parse::<u32>().ok()) {
            Some(d) if d <= 9 => 0x60 + d,
            _ => 0,
        },
    }
}

/// Reads "Alt+F10", "Ctrl+Shift+S", "Insert", "Backquote" and the like.
pub fn parse_hotkey(spec: &str) -> Result<(u32, u32), String> {
    let (mut mods, mut vk) = (0u32, 0u32);
    let parts: Vec<&str> = spec.split('+').collect();
    for (i, p) in parts.iter().enumerate() {
        let p = p.trim().to_uppercase();
        if i + 1 < parts.len() {
            match p.as_str() {
                "ALT" => mods |= MOD_ALT,
                "CTRL" | "CONTROL" => mods |= MOD_CONTROL,
                "SHIFT" => mods |= MOD_SHIFT,
                "WIN" => mods |= MOD_WIN,
                _ => {}
            }
            continue;
        }
        let b = p.as_bytes();
        if let Some(n) = p.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)) {
            vk = 0x70 + n - 1;
        } else if b.len() == 1 && b[0].is_ascii_alphanumeric() {
            vk = b[0] as u32;
        } else if p == "BACKQUOTE" {
            vk = backquote_vk();
        } else {
            vk = named_key(&p);
        }
    }
    if vk == 0 {
        return Err(format!("\"{spec}\" is not a hotkey this app understands"));
    }
    Ok((mods, vk))
}

// ---- beeps ----

/// A 16-bit mono WAV of the notes played back to back.
fn tone_wav(freqs: &[f64], each_ms: u32) -> Vec<u8> {
    const RATE: usize = 22050;
    let n = each_ms as usize * RATE / 1000;
    let mut pcm = Vec::with_capacity(2 * n * freqs.len());
    for &f in freqs {
        for i in 0..n {
            // Short fades on both ends, or every note starts with a click.
            let env = (i.min(n - i) as f64 / (RATE as f64 / 200.0)).min(1.0);
            let v = (0.3 * 32767.0 * env * (2.0 * std::f64::consts::PI * f * i as f64 / RATE as f64).sin()) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
        }
    }
    let le = |v: usize| (v as u32).to_le_bytes();
    let mut w = b"RIFF".to_vec();
    w.extend_from_slice(&le(36 + pcm.len()));
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&le(16));
    w.extend_from_slice(&[1, 0, 1, 0]); // PCM, mono
    w.extend_from_slice(&le(RATE));
    w.extend_from_slice(&le(RATE * 2));
    w.extend_from_slice(&[2, 0, 16, 0]); // block align, bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&le(pcm.len()));
    w.extend_from_slice(&pcm);
    w
}

// Saved: two quick rising notes. Failed: one low, longer buzz.
static OK_TONE: LazyLock<Vec<u8>> = LazyLock::new(|| tone_wav(&[880.0, 1320.0], 90));
static FAIL_TONE: LazyLock<Vec<u8>> = LazyLock::new(|| tone_wav(&[220.0], 350));

/// The only feedback that reaches a player in a fullscreen game. It plays our own
/// tones: system sounds are whatever the user's sound scheme says, and on plenty of
/// PCs "ok" and "error" are the same ding.
pub fn beep(ok: bool) {
    let w: &Vec<u8> = if ok { &OK_TONE } else { &FAIL_TONE };
    unsafe { PlaySoundW(w.as_ptr() as *const u16, 0, SND_MEMORY | SND_ASYNC) };
}

// ---- what is being played ----

/// Names the program in front; good enough to keep each game's clips in its own
/// folder.
pub fn foreground_app() -> String {
    window_app(foreground_window())
}

pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow() }
}

pub fn window_alive(h: isize) -> bool {
    unsafe { IsWindow(h) != 0 }
}

/// An exclusive-fullscreen game minimizes on alt-tab, and a minimized window has
/// nothing for window capture to see.
pub fn window_minimized(h: isize) -> bool {
    unsafe { IsIconic(h) != 0 }
}

/// The desktop, taskbar and Start menu cover the screen too, but are never games.
const SHELL_CLASSES: [&str; 5] =
    ["Progman", "WorkerW", "Shell_TrayWnd", "Windows.UI.Core.CoreWindow", "ApplicationFrameWindow"];

fn class_name(h: isize) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) };
    from_wide(&buf[..n.max(0) as usize])
}

fn window_title(h: isize) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) };
    from_wide(&buf[..n.max(0) as usize])
}

/// The full path of the program behind a window.
pub fn window_exe(h: isize) -> String {
    if h == 0 {
        return String::new();
    }
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(h, &mut pid);
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p == 0 {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let r = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size);
        CloseHandle(p);
        if r == 0 {
            return String::new();
        }
        from_wide(&buf[..size as usize])
    }
}

/// The foreground window if it is a game (see games.rs): any decent-sized window of
/// an installed game, or otherwise an unknown program that covers its whole monitor
/// the way fullscreen and borderless games do.
pub fn game_in_front() -> Target {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == 0 || IsWindowVisible(hwnd) == 0 || SHELL_CLASSES.contains(&class_name(hwnd).as_str()) {
            return Target::default();
        }
        let exe = window_exe(hwnd);
        let custom = games::custom_app(&exe);
        if exe.is_empty() || (custom.is_none() && games::is_not_game(&exe)) {
            return Target::default();
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let mut wr: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut wr) == 0 {
            return Target::default();
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi) == 0 {
            return Target::default();
        }
        let m = mi.rcMonitor;
        let fills = wr.left <= m.left && wr.top <= m.top && wr.right >= m.right && wr.bottom >= m.bottom;
        // A game filling its monitor is recorded as that monitor (see Target),
        // unless the monitor is on a GPU ddagrab cannot reach.
        let target = |name: String| {
            let mut t = Target { hwnd, app: name, pid, ..Default::default() };
            if fills {
                if let Some(out) = dxgi_output_of(mon) {
                    t.display = true;
                    t.output = out;
                }
            }
            t
        };
        let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);

        if let Some(c) = custom {
            // Any window the player would call the app, not its splash or a tooltip.
            if w < 400 || h < 300 {
                return Target::default();
            }
            if games::custom_no_hook(&exe) {
                return Target { hwnd, app: c.name, pid, custom: true, no_hook: true, ..Default::default() };
            }
            let mut t = target(c.name);
            t.custom = true;
            return t;
        }
        let known = games::game_for_exe(&exe).map(|g| g.name).or_else(|| java_game(hwnd, &exe));
        if let Some(name) = known {
            // Windowed counts for a known game, but not the small launcher and
            // crash-reporter windows that live in the same folder.
            if w < 960 || h < 540 {
                return Target::default();
            }
            return target(name);
        }
        if games::per_user_app(&exe) {
            return Target::default();
        }
        // A maximized app on a monitor without a taskbar covers the whole monitor as
        // well. Games have neither a title bar nor the maximized state.
        if IsZoomed(hwnd) != 0 || GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CAPTION == WS_CAPTION || !fills {
            return Target::default();
        }
        target(games::exe_name(&exe))
    }
}

/// Which output of the first GPU shows monitor mon - the number ddagrab's
/// output_idx wants, since it captures from the default adapter. A monitor plugged
/// into another GPU is not found.
fn dxgi_output_of(mon: HMONITOR) -> Option<i32> {
    #[repr(C)]
    struct Guid(u32, u16, u16, [u8; 8]);
    const IID_FACTORY1: Guid = Guid(0x770aae78, 0xf26f, 0x4dba, [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87]);
    #[repr(C)]
    struct OutputDesc {
        name: [u16; 32],
        coords: RECT,
        attached: i32,
        rotation: u32,
        monitor: HMONITOR,
    }
    type Create = unsafe extern "system" fn(*const Guid, *mut *mut c_void) -> i32;
    type EnumAt = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32;
    type GetDesc = unsafe extern "system" fn(*mut c_void, *mut OutputDesc) -> i32;
    unsafe {
        let dxgi = LoadLibraryW(wide("dxgi.dll").as_ptr());
        let create = GetProcAddress(dxgi, b"CreateDXGIFactory1\0".as_ptr())?;
        let create: Create = std::mem::transmute(create);
        let mut factory = null_mut();
        if create(&IID_FACTORY1, &mut factory) < 0 {
            return None;
        }
        let mut adapter = null_mut();
        let enum_adapters: EnumAt = method(factory, 12); // EnumAdapters1
        let r = enum_adapters(factory, 0, &mut adapter);
        let mut found = None;
        if r >= 0 {
            let enum_outputs: EnumAt = method(adapter, 7);
            for i in 0.. {
                let mut output = null_mut();
                if enum_outputs(adapter, i, &mut output) < 0 {
                    break;
                }
                let get_desc: GetDesc = method(output, 7);
                let mut desc: OutputDesc = std::mem::zeroed();
                let hr = get_desc(output, &mut desc);
                release(output);
                if hr >= 0 && desc.monitor == mon {
                    found = Some(i as i32);
                    break;
                }
            }
            release(adapter);
        }
        release(factory);
        found
    }
}

/// A Java game window - Minecraft from any launcher - by its title, since its exe
/// is only ever java(w).exe, wherever the launcher keeps it.
fn java_game(hwnd: isize, exe: &str) -> Option<String> {
    let n = games::exe_name(exe).to_lowercase();
    if n != "javaw" && n != "java" {
        return None;
    }
    let t = window_title(hwnd).trim().to_lowercase();
    ["minecraft", "lunar client", "badlion"].iter().any(|p| t.starts_with(p)).then(|| "Minecraft".to_string())
}

/// What a clip of a window is filed under: the installed game's name, else the exe
/// name minus ".exe"; "" for our own window and Explorer, which are never what
/// anyone wants clipped.
pub fn window_app(hwnd: isize) -> String {
    let exe = window_exe(hwnd);
    if exe.is_empty() {
        return String::new();
    }
    if let Some(g) = games::game_for_exe(&exe) {
        return g.name;
    }
    if let Some(n) = java_game(hwnd, &exe) {
        return n;
    }
    let name = games::exe_name(&exe);
    if name.eq_ignore_ascii_case("explorer") || name.eq_ignore_ascii_case("Miyav") {
        return String::new();
    }
    name
}

/// OBS's game capture looks windows up as "title:class:exe.exe", with ':' and '#'
/// escaped as OBS escapes them; and the window's size, which the recording canvas
/// takes.
pub fn obs_window(hwnd: isize) -> (String, i32, i32) {
    let esc = |s: &str| s.replace('#', "#22").replace(':', "#3A");
    let exe = window_exe(hwnd);
    let base = Path::new(&exe).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let spec = format!("{}:{}:{}", esc(&window_title(hwnd)), esc(&class_name(hwnd)), esc(&base));
    let mut cr: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(hwnd, &mut cr) };
    (spec, cr.right - cr.left, cr.bottom - cr.top)
}

/// A program with a window open, offered for adding.
#[derive(Clone, Serialize)]
pub struct RunningApp {
    pub name: String,
    pub exe: String,
}

/// Windows' own programs, which have windows but are nobody's idea of something to
/// record.
const NOT_OFFERED: [&str; 10] = [
    "explorer", "miyav", "applicationframehost", "systemsettings", "searchhost",
    "startmenuexperiencehost", "shellexperiencehost", "lockapp", "textinputhost", "msedgewebview2",
];

unsafe extern "system" fn enum_app(hwnd: HWND, data: LPARAM) -> BOOL {
    let out = &mut *(data as *mut Vec<RunningApp>);
    if IsWindowVisible(hwnd) == 0
        || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0
        || window_title(hwnd).is_empty()
        || SHELL_CLASSES.contains(&class_name(hwnd).as_str())
    {
        return 1;
    }
    let exe = window_exe(hwnd);
    let name = games::exe_name(&exe);
    let key = name.to_lowercase();
    if !exe.is_empty() && !NOT_OFFERED.contains(&key.as_str()) && !out.iter().any(|a| a.name.to_lowercase() == key) {
        let base = Path::new(&exe).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        out.push(RunningApp { name, exe: base });
    }
    1
}

/// The programs with a window open, one row per program.
pub fn running_apps() -> Vec<RunningApp> {
    let mut out: Vec<RunningApp> = Vec::new();
    unsafe { EnumWindows(Some(enum_app), &mut out as *mut _ as LPARAM) };
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys() {
        assert_eq!(parse_hotkey("Alt+F10").unwrap(), (MOD_ALT, 0x79));
        assert_eq!(parse_hotkey("Ctrl+Shift+S").unwrap(), (MOD_CONTROL | MOD_SHIFT, 0x53));
        assert_eq!(parse_hotkey("Insert").unwrap(), (0, 0x2D));
        assert_eq!(parse_hotkey("Num5").unwrap(), (0, 0x65));
        assert_eq!(parse_hotkey("7").unwrap(), (0, 0x37));
        assert!(parse_hotkey("Backquote").unwrap().1 != 0);
        assert!(parse_hotkey("Alt+Banana").is_err());
        assert!(parse_hotkey("").is_err());
    }

    #[test]
    fn tones_are_wavs() {
        let w = tone_wav(&[880.0], 10);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize, w.len() - 8);
        assert_eq!(&w[36..40], b"data");
    }

    #[test]
    fn apps_list_runs() {
        let _ = running_apps();
        assert!(local_stamp().len() >= 19 && !local_stamp().contains(':'));
    }
}
