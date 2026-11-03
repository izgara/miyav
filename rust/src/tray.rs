//! The app lives in the notification area so the replay buffer keeps running with
//! the window closed, and can start there at sign-in. The icon shows the recorder's
//! state: a red dot while recording, amber when something is wrong, grey when
//! recording is off.
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::win::{self, wide, wide_into};
use crate::{recorder, server, settings, window};

const TRAY_CLASS: &str = "MiyavTray";
const WM_TRAY_ICON: u32 = WM_APP + 1; // the icon's callback message
const WM_TRAY_SHOW: u32 = WM_APP + 2; // a second copy of the app asks this one to show itself
const NIN_SELECT_: u32 = 0x0400;
const NIN_KEYSELECT_: u32 = 0x0401;
const NIN_BALLOONUSERCLICK_: u32 = 0x0405;
const HWND_MESSAGE_: HWND = -3;

const MENU_OPEN: usize = 1;
const MENU_SAVE: usize = 2;
const MENU_RECORD: usize = 3;
const MENU_AUTOSTART: usize = 4;
const MENU_QUIT: usize = 5;

const OFF: usize = 0;
const IDLE: usize = 1;
const REC: usize = 2;
const ERR: usize = 3;

struct Tray {
    hwnd: HWND,
    icons: [HICON; 4],
    shown: i32, // which of icons is up, -1 before the first refresh
    tip: String,
    last_clip: String,
    /// The clip the notification up now is about; clicking it opens that clip.
    balloon_clip: String,
}

static TRAY: Mutex<Tray> = Mutex::new(Tray {
    hwnd: 0,
    icons: [0; 4],
    shown: -1,
    tip: String::new(),
    last_clip: String::new(),
    balloon_clip: String::new(),
});

static TASKBAR_CREATED: AtomicIsize = AtomicIsize::new(0);

fn nid(hwnd: HWND, flags: u32) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n.uFlags = flags;
    n.uCallbackMessage = WM_TRAY_ICON;
    n
}

// ---- single instance ----

/// Makes sure only one copy records. A second launch - the Start menu shortcut while
/// the app already sits in the tray, say - asks the running copy to show its window
/// and returns false.
pub fn claim_single_instance() -> bool {
    unsafe {
        // MIYAV_INSTANCE keeps a test copy apart from the one the player is running.
        let name = format!(r"Local\Miyav{}", std::env::var("MIYAV_INSTANCE").unwrap_or_default());
        let h = CreateMutexW(null(), 0, wide(&name).as_ptr());
        if h == 0 || GetLastError() != ERROR_ALREADY_EXISTS {
            return true; // the handle stays open for as long as this process runs
        }
        let w = FindWindowExW(HWND_MESSAGE_, 0, wide(TRAY_CLASS).as_ptr(), null());
        if w != 0 {
            AllowSetForegroundWindow(u32::MAX); // ASFW_ANY: let it come to the front
            PostMessageW(w, WM_TRAY_SHOW, 0, 0);
        }
        false
    }
}

// ---- the tray icon ----

unsafe extern "system" fn wnd_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY_ICON => {
            match (lp & 0xFFFF) as u32 {
                NIN_BALLOONUSERCLICK_ => {
                    let clip = TRAY.lock().unwrap().balloon_clip.clone();
                    if !clip.is_empty() {
                        recorder::request_open(&clip);
                    }
                    window::show();
                }
                NIN_SELECT_ | NIN_KEYSELECT_ => window::show(),
                WM_CONTEXTMENU => menu(h),
                _ => {}
            }
            0
        }
        WM_TRAY_SHOW => {
            window::show();
            0
        }
        WM_TIMER => {
            refresh();
            0
        }
        m if m != 0 && m as isize == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // Explorer restarted and took every tray icon with it.
            add();
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// Puts the icon up. It runs its own thread and message loop, like the hotkey, so it
/// never waits on the WebView.
pub fn start() {
    std::thread::spawn(|| unsafe {
        let inst = GetModuleHandleW(null());
        let class = wide(TRAY_CLASS);
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) as isize, Ordering::Relaxed);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = inst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);
        let h = CreateWindowExW(0, class.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE_, 0, inst, null());
        if h == 0 {
            return;
        }
        {
            let mut t = TRAY.lock().unwrap();
            t.hwnd = h;
            t.icons = make_icons(GetSystemMetrics(SM_CXSMICON) as u32);
            t.shown = -1;
            t.last_clip = recorder::status().last_clip;
        }
        add();
        SetTimer(h, 1, 1000, None);
        let mut m: MSG = std::mem::zeroed();
        while GetMessageW(&mut m, 0, 0, 0) > 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    });
}

fn add() {
    {
        let mut t = TRAY.lock().unwrap();
        let mut n = nid(t.hwnd, NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        n.hIcon = t.icons[IDLE];
        wide_into(&mut n.szTip, "Miyav");
        // Version 4 sends clicks as NIN_SELECT and the menu key as WM_CONTEXTMENU.
        n.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &n);
            Shell_NotifyIconW(NIM_SETVERSION, &n);
        }
        t.shown = -1;
        t.tip.clear();
    }
    refresh();
}

/// Takes the icon down as the app exits, rather than leaving it for Windows to notice
/// is dead when the mouse next passes over it.
pub fn remove() {
    let t = TRAY.lock().unwrap();
    if t.hwnd != 0 {
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid(t.hwnd, 0)) };
    }
}

/// Runs every second and only touches the icon when the recorder's state changed.
fn refresh() {
    let st = recorder::status();
    let key = settings::get().hotkey;
    let (icon, tip) = if st.recording {
        let what = if st.target.is_empty() { "the screen" } else { &st.target };
        (REC, format!("Recording {what}\n{key} saves a clip"))
    } else if st.enabled && !st.err.is_empty() {
        (ERR, st.err.clone())
    } else if st.enabled {
        (IDLE, format!("Waiting for a game\n{key} saves a clip"))
    } else {
        (OFF, "Recording is off".to_string())
    };
    let tip = format!("Miyav\n{tip}");
    let clip;
    {
        let mut t = TRAY.lock().unwrap();
        if icon as i32 != t.shown || tip != t.tip {
            t.shown = icon as i32;
            t.tip = tip.clone();
            let mut n = nid(t.hwnd, NIF_ICON | NIF_TIP | NIF_SHOWTIP);
            n.hIcon = t.icons[icon];
            wide_into(&mut n.szTip, &tip);
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
        }
        clip = !st.last_clip.is_empty() && st.last_clip != t.last_clip;
        t.last_clip = st.last_clip.clone();
    }
    if clip && !settings::get().quiet_saves {
        let game = Path::new(&st.last_clip)
            .parent()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let title = if game.is_empty() { "Clip saved".to_string() } else { format!("Clip saved - {game}") };
        balloon(&title, "Click to watch and trim it.");
        TRAY.lock().unwrap().balloon_clip = st.last_clip;
    }
}

/// Shows a Windows notification from the tray icon. It is silent: the hotkey already
/// beeps, and Windows holds it back over a fullscreen game. With the OBS engine it
/// never shows up in a clip either; OBS records the game, not the screen.
pub fn balloon(title: &str, text: &str) {
    let mut t = TRAY.lock().unwrap();
    if t.hwnd == 0 {
        return;
    }
    t.balloon_clip.clear();
    let mut n = nid(t.hwnd, NIF_INFO);
    n.dwInfoFlags = NIIF_NOSOUND;
    wide_into(&mut n.szInfoTitle, title);
    wide_into(&mut n.szInfo, text);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
}

unsafe fn menu(h: HWND) {
    let st = recorder::status();
    let s = settings::get();
    let m = CreatePopupMenu();
    let add = |flags: u32, id: usize, text: &str| {
        let t = wide(&text.replace('&', "&&"));
        AppendMenuW(m, flags, id, t.as_ptr());
    };
    let check = |on: bool| if on { MF_CHECKED } else { 0 };
    let state = if st.recording && !st.target.is_empty() {
        format!("Recording {}", st.target)
    } else if st.recording {
        "Recording the screen".to_string()
    } else if st.enabled {
        "Waiting for a game".to_string()
    } else {
        "Recording is off".to_string()
    };
    add(MF_GRAYED, 0, &state);
    add(MF_SEPARATOR, 0, "");
    add(0, MENU_OPEN, "Open Miyav");
    add(if st.enabled { 0 } else { MF_GRAYED }, MENU_SAVE, &format!("Save clip now\t{}", s.hotkey));
    add(check(st.enabled), MENU_RECORD, "Recording on");
    add(check(win::autostart_enabled()), MENU_AUTOSTART, "Start with Windows");
    add(MF_SEPARATOR, 0, "");
    add(0, MENU_QUIT, "Quit");
    SetMenuDefaultItem(m, MENU_OPEN as u32, 0);

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    // Without being in front, the menu would not close when clicked away from.
    SetForegroundWindow(h);
    let cmd = TrackPopupMenu(m, TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_RETURNCMD, pt.x, pt.y, 0, h, null_mut());
    PostMessageW(h, WM_NULL, 0, 0);
    DestroyMenu(m);

    match cmd as usize {
        MENU_OPEN => window::show(),
        MENU_SAVE => {
            std::thread::spawn(recorder::save_from_hotkey);
        }
        MENU_RECORD => {
            let on = !st.enabled;
            std::thread::spawn(move || {
                if let Err(e) = recorder::set_enabled(on) {
                    let _ = recorder::set_enabled(false);
                    balloon("Could not start recording", &e);
                }
            });
        }
        MENU_AUTOSTART => {
            if let Err(e) = win::set_autostart(!win::autostart_enabled()) {
                std::thread::spawn(move || balloon("Could not change that", &e));
            }
        }
        MENU_QUIT => window::quit_app(),
        _ => {}
    }
}

// ---- icons ----

struct Image {
    size: usize,
    px: Vec<u8>, // RGBA
}

/// Draws the four states from the app icon: off is greyed, idle is the plain icon,
/// and recording and trouble add a red or amber dot.
fn make_icons(size: u32) -> [HICON; 4] {
    let mut out = [0; 4];
    let Some(base) = app_icon_image(size as usize) else { return out };
    let mut grey = base.px.clone();
    for p in grey.chunks_exact_mut(4) {
        // Greyed and dimmed, but opaque, so it still shows on a light taskbar.
        let l = ((299 * p[0] as u32 + 587 * p[1] as u32 + 114 * p[2] as u32) * 3 / 4000) as u8;
        p[0] = l;
        p[1] = l;
        p[2] = l;
    }
    let imgs = [
        Image { size: base.size, px: grey },
        Image { size: base.size, px: base.px.clone() },
        with_dot(&base, [0xE5, 0x38, 0x3B]),
        with_dot(&base, [0xF5, 0xA5, 0x24]),
    ];
    for (i, img) in imgs.iter().enumerate() {
        if let Some(data) = encode_png(img) {
            // PNG data is a valid icon resource since Vista; 0x30000 is the icon
            // format version the call expects.
            out[i] = unsafe {
                CreateIconFromResourceEx(data.as_ptr(), data.len() as u32, 1, 0x0003_0000, size as i32, size as i32, 0)
            };
        }
    }
    out
}

fn encode_png(img: &Image) -> Option<Vec<u8>> {
    let mut b = Vec::new();
    {
        let mut e = png::Encoder::new(&mut b, img.size as u32, img.size as u32);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().ok()?.write_image_data(&img.px).ok()?;
    }
    Some(b)
}

/// Takes the smallest PNG in the embedded app.ico that is at least size wide, scaled
/// to size, so every state is drawn at the size it is shown.
fn app_icon_image(size: usize) -> Option<Image> {
    let ico = server::app_icon();
    let entry = choose_ico_entry(ico, size)?;
    let e = &ico[6 + 16 * entry..];
    let len = u32::from_le_bytes(e[8..12].try_into().ok()?) as usize;
    let off = u32::from_le_bytes(e[12..16].try_into().ok()?) as usize;
    let data = ico.get(off..off + len)?;
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut r = dec.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba {
        return None;
    }
    let (sw, sh) = (info.width as usize, info.height as usize);
    let mut px = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let s = (y * sh / size * sw + x * sw / size) * 4;
            px[(y * size + x) * 4..][..4].copy_from_slice(&buf[s..s + 4]);
        }
    }
    Some(Image { size, px })
}

fn choose_ico_entry(ico: &[u8], size: usize) -> Option<usize> {
    if ico.len() < 6 {
        return None;
    }
    let n = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let (mut best, mut best_w) = (None, 0);
    for i in (0..n).take_while(|i| 6 + 16 * i + 16 <= ico.len()) {
        let w = match ico[6 + 16 * i] {
            0 => 256,
            w => w as usize,
        };
        let (fits, best_fits) = (w >= size, best_w >= size);
        if best.is_none() || (fits && (!best_fits || w < best_w)) || (!fits && !best_fits && w > best_w) {
            best = Some(i);
            best_w = w;
        }
    }
    best
}

/// Draws a status dot in the bottom-right corner, ringed in white so it reads on both
/// light and dark taskbars.
fn with_dot(base: &Image, c: [u8; 3]) -> Image {
    let mut px = base.px.clone();
    let n = base.size as f64;
    let (cx, cy, r) = (n * 0.72, n * 0.72, n * 0.27);
    let ring = (n / 16.0).max(1.0);
    const SS: usize = 4; // samples per side, for smooth edges
    for y in 0..base.size {
        for x in 0..base.size {
            let (mut in_dot, mut in_ring) = (0.0, 0.0);
            for sy in 0..SS {
                for sx in 0..SS {
                    let d = (x as f64 + (sx as f64 + 0.5) / SS as f64 - cx)
                        .hypot(y as f64 + (sy as f64 + 0.5) / SS as f64 - cy);
                    if d <= r - ring {
                        in_dot += 1.0;
                    } else if d <= r {
                        in_ring += 1.0;
                    }
                }
            }
            in_dot /= (SS * SS) as f64;
            in_ring /= (SS * SS) as f64;
            let cov: f64 = in_dot + in_ring;
            if cov == 0.0 {
                continue;
            }
            let p = &mut px[(y * base.size + x) * 4..][..4];
            let a = p[3] as f64 / 255.0;
            let out_a = cov + a * (1.0 - cov);
            for k in 0..3 {
                let top = (c[k] as f64 * in_dot + 255.0 * in_ring) / cov;
                p[k] = ((top * cov + p[k] as f64 * a * (1.0 - cov)) / out_a).round() as u8;
            }
            p[3] = (out_a * 255.0).round() as u8;
        }
    }
    Image { size: base.size, px }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_draw() {
        let base = app_icon_image(32).expect("the embedded icon has a PNG");
        assert_eq!(base.px.len(), 32 * 32 * 4);
        let dot = with_dot(&base, [255, 0, 0]);
        assert_ne!(dot.px, base.px);
        // The dot's centre is its colour, solid.
        let i = ((32.0 * 0.72) as usize * 32 + (32.0 * 0.72) as usize) * 4;
        assert_eq!(&dot.px[i..i + 4], &[255, 0, 0, 255]);
        let data = encode_png(&dot).unwrap();
        let h = unsafe { CreateIconFromResourceEx(data.as_ptr(), data.len() as u32, 1, 0x0003_0000, 32, 32, 0) };
        assert_ne!(h, 0);
    }

    #[test]
    fn notify_data_is_the_size_windows_expects() {
        assert_eq!(std::mem::size_of::<NOTIFYICONDATAW>(), 976);
    }
}
