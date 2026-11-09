//! Each game's own icon, read out of its exe on this PC. Nothing is downloaded.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Shell::SHDefExtractIconW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{games, win};

pub type Pixels = Arc<(usize, usize, Vec<u8>)>;

const SIZE: u32 = 48;
const MAX_DEPTH: usize = 5;
const MAX_ENTRIES: usize = 6000;
/// Installers, crash reporters and anti-cheat that ship beside the game.
const NOT_THE_GAME: [&str; 16] = [
    "unins", "setup", "install", "crash", "report", "redist", "vc_", "dxsetup", "prereq", "easyanticheat", "battleye", "beservice", "updater", "webhelper", "subprocess", "cefprocess",
];

static LIBRARY: LazyLock<Vec<games::GameEntry>> = LazyLock::new(games::game_library);
static CACHE: LazyLock<Mutex<HashMap<String, Option<Pixels>>>> = LazyLock::new(Default::default);

fn plain(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The first letter of each word: "Counter-Strike 2" is cs2.
fn initials(s: &str) -> String {
    s.split(|c: char| !c.is_alphanumeric()).filter_map(|w| w.chars().next()).flat_map(char::to_lowercase).collect()
}

/// How much an exe looks like the game itself.
fn score(stem: &str, game: &str, depth: usize, size: u64) -> i64 {
    let (exe, name) = (plain(stem), plain(game));
    let mut s = (size / (1 << 20)).min(40) as i64 - depth as i64 * 2;
    if exe == name {
        s += 200;
    } else if exe.contains(&name) || name.contains(&exe) {
        s += 100;
    } else if exe == initials(game) {
        s += 90;
    } else if name.get(..5).is_some_and(|p| exe.starts_with(p)) {
        s += 60;
    }
    if exe.contains("shipping") {
        s += 10;
    }
    if exe.contains("launcher") {
        s -= 20;
    }
    s
}

fn find_exe(dir: &Path, game: &str) -> Option<PathBuf> {
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let (mut seen, mut best): (usize, Option<(i64, PathBuf)>) = (0, None);
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return best.map(|b| b.1);
            }
            let p = e.path();
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let name = e.file_name().to_string_lossy().to_lowercase();
            let Some(stem) = name.strip_suffix(".exe") else { continue };
            if NOT_THE_GAME.iter().any(|x| stem.contains(x)) {
                continue;
            }
            let s = score(stem, game, depth, m.len());
            if best.as_ref().is_none_or(|b| s > b.0) {
                best = Some((s, p));
            }
        }
    }
    best.map(|b| b.1)
}

fn bits(dc: HDC, bitmap: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = w;
    info.bmiHeader.biHeight = -h; // top row first
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    let mut out = vec![0u8; w as usize * h as usize * 4];
    let lines = unsafe { GetDIBits(dc, bitmap, 0, h as u32, out.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS) };
    (lines != 0).then_some(out)
}

fn rgba(icon: HICON) -> Option<(usize, usize, Vec<u8>)> {
    unsafe {
        let mut parts: ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut parts) == 0 {
            return None;
        }
        let mut out = None;
        let mut size: BITMAP = std::mem::zeroed();
        if parts.hbmColor != 0 && GetObjectW(parts.hbmColor, std::mem::size_of::<BITMAP>() as i32, (&mut size as *mut BITMAP).cast()) != 0 {
            let (w, h) = (size.bmWidth, size.bmHeight);
            let dc = CreateCompatibleDC(0);
            if let Some(mut px) = bits(dc, parts.hbmColor, w, h) {
                // An old icon has no alpha at all: its mask says what is see-through.
                if px.chunks_exact(4).all(|p| p[3] == 0) {
                    if let Some(mask) = bits(dc, parts.hbmMask, w, h) {
                        for (p, m) in px.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                            p[3] = if m[0] == 0 { 255 } else { 0 };
                        }
                    }
                }
                for p in px.chunks_exact_mut(4) {
                    p.swap(0, 2);
                }
                out = Some((w as usize, h as usize, px));
            }
            DeleteDC(dc);
        }
        if parts.hbmColor != 0 {
            DeleteObject(parts.hbmColor);
        }
        if parts.hbmMask != 0 {
            DeleteObject(parts.hbmMask);
        }
        out
    }
}

fn exe_icon(exe: &Path) -> Option<(usize, usize, Vec<u8>)> {
    let path = win::wide(&exe.display().to_string());
    let mut icon: HICON = 0;
    let hr = unsafe { SHDefExtractIconW(path.as_ptr(), 0, 0, &mut icon, std::ptr::null_mut(), SIZE) };
    if hr != 0 || icon == 0 {
        return None;
    }
    let out = rgba(icon);
    unsafe { DestroyIcon(icon) };
    out
}

fn lookup(name: &str) -> Option<Pixels> {
    let key = plain(name);
    let game = LIBRARY.iter().find(|g| plain(&g.name) == key)?;
    let exe = find_exe(Path::new(&game.dir), &game.name)?;
    exe_icon(&exe).map(Arc::new)
}

/// The icon of the game a recordings folder is named after, if it is installed here.
pub fn icon(name: &str) -> Option<Pixels> {
    if let Some(known) = CACHE.lock().unwrap().get(name) {
        return known.clone();
    }
    let found = lookup(name);
    CACHE.lock().unwrap().insert(name.to_string(), found.clone());
    found
}

/// The icon as a PNG, for the page to show.
pub fn png(name: &str) -> Option<Vec<u8>> {
    let px = icon(name)?;
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, px.0 as u32, px.1 as u32);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().ok()?.write_image_data(&px.2).ok()?;
    }
    Some(out)
}
