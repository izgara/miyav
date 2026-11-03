//! The small Win32 helpers everything else leans on.
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn from_wide(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

/// Copies s into a fixed field, always leaving it terminated.
pub fn wide_into(dst: &mut [u16], s: &str) {
    dst.fill(0);
    let room = dst.len() - 1;
    for (d, c) in dst.iter_mut().take(room).zip(s.encode_utf16()) {
        *d = c;
    }
}

/// Stops a console flashing up every time a child process runs.
pub fn hide_window(cmd: &mut Command) {
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

pub fn show_error(title: &str, body: &str) {
    let (t, b) = (wide(title), wide(body));
    unsafe { MessageBoxW(0, b.as_ptr(), t.as_ptr(), MB_OK | MB_ICONERROR) };
}

// ---- registry ----

pub const HKCU: HKEY = HKEY_CURRENT_USER;
pub const HKLM: HKEY = HKEY_LOCAL_MACHINE;

pub fn reg_string(root: HKEY, path: &str, name: &str) -> String {
    unsafe {
        let mut k = 0;
        if RegOpenKeyExW(root, wide(path).as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut k) != 0 {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        let r = RegQueryValueExW(k, wide(name).as_ptr(), null(), null_mut(), buf.as_mut_ptr().cast(), &mut size);
        RegCloseKey(k);
        if r != 0 {
            return String::new();
        }
        from_wide(&buf)
    }
}

pub fn reg_subkeys(root: HKEY, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let mut k = 0;
        if RegOpenKeyExW(root, wide(path).as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut k) != 0 {
            return out;
        }
        for i in 0.. {
            let mut buf = [0u16; 256];
            let mut n = buf.len() as u32;
            if RegEnumKeyExW(k, i, buf.as_mut_ptr(), &mut n, null(), null_mut(), null_mut(), null_mut()) != 0 {
                break;
            }
            out.push(from_wide(&buf[..n as usize]));
        }
        RegCloseKey(k);
    }
    out
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "Miyav";

pub fn autostart_enabled() -> bool {
    !reg_string(HKCU, RUN_KEY, RUN_NAME).is_empty()
}

/// Adds or removes the Run entry. It starts the app with --tray, so sign-in does
/// not open a window.
pub fn set_autostart(on: bool) -> Result<(), String> {
    unsafe {
        let mut k = 0;
        let r = RegOpenKeyExW(HKCU, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut k);
        if r != 0 {
            return Err(format!("could not open the registry ({r})"));
        }
        let n = wide(RUN_NAME);
        let r = if on {
            let v = wide(&format!("\"{}\" --tray", crate::ffmpeg::exe_path().display()));
            RegSetValueExW(k, n.as_ptr(), 0, REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32)
        } else {
            match RegDeleteValueW(k, n.as_ptr()) {
                2 => 0, // there was no entry to remove
                r => r,
            }
        };
        RegCloseKey(k);
        if r != 0 {
            return Err(format!("could not change the registry ({r})"));
        }
    }
    Ok(())
}

/// Points the Run entry at this copy when the one it named is gone - the app was
/// moved, or reinstalled somewhere else.
pub fn refresh_autostart() {
    let cur = reg_string(HKCU, RUN_KEY, RUN_NAME);
    if cur.is_empty() {
        return;
    }
    let old = cur.trim_end_matches(" --tray").trim_matches('"').to_string();
    if !std::path::Path::new(&old).exists() {
        let _ = set_autostart(true);
    }
}

// ---- keep children from outliving us ----

/// A job object with KILL_ON_JOB_CLOSE: when our process exits for any reason,
/// crash included, Windows closes the handle and takes ffmpeg down with it.
static APP_JOB: OnceLock<isize> = OnceLock::new();

pub fn bind_to_app(pid: u32) {
    let job = *APP_JOB.get_or_init(|| unsafe {
        let h = CreateJobObjectW(null(), null());
        if h != 0 {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            );
        }
        h
    });
    if job == 0 {
        return;
    }
    unsafe {
        let h = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if h != 0 {
            AssignProcessToJobObject(job, h);
            CloseHandle(h);
        }
    }
}

// ---- about this PC ----

/// The display adapters, without the duplicates Windows reports once per output.
pub fn gpu_names() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    unsafe {
        for i in 0.. {
            let mut dd: DISPLAY_DEVICEW = std::mem::zeroed();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if EnumDisplayDevicesW(null(), i, &mut dd, 0) == 0 {
                break;
            }
            let name = from_wide(&dd.DeviceString);
            if !name.is_empty() && !out.contains(&name) && dd.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER == 0 {
                out.push(name);
            }
        }
    }
    out
}

pub fn primary_display() -> (u32, u32, u32) {
    unsafe {
        let mut dm: DEVMODEW = std::mem::zeroed();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(null(), ENUM_CURRENT_SETTINGS, &mut dm) == 0 {
            return (0, 0, 0);
        }
        (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency)
    }
}

pub fn total_ram_gb() -> f64 {
    unsafe {
        let mut ms: MEMORYSTATUSEX = std::mem::zeroed();
        ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut ms) == 0 {
            return 0.0;
        }
        ms.ullTotalPhys as f64 / (1u64 << 30) as f64
    }
}

pub fn free_disk_gb(dir: &std::path::Path) -> f64 {
    let mut free = 0u64;
    let p = wide(&dir.display().to_string());
    if unsafe { GetDiskFreeSpaceExW(p.as_ptr(), &mut free, null_mut(), null_mut()) } == 0 {
        return 0.0;
    }
    free as f64 / (1u64 << 30) as f64
}

// ---- pickers and the shell ----

#[repr(C)]
struct BROWSEINFOW {
    hwnd_owner: HWND,
    pidl_root: *const std::ffi::c_void,
    display_name: *mut u16,
    title: *const u16,
    flags: u32,
    callback: Option<unsafe extern "system" fn(HWND, u32, LPARAM, LPARAM) -> i32>,
    lparam: LPARAM,
    image: i32,
}

#[link(name = "shell32")]
extern "system" {
    fn SHBrowseForFolderW(bi: *const BROWSEINFOW) -> *mut std::ffi::c_void;
    fn SHGetPathFromIDListW(pidl: *const std::ffi::c_void, path: *mut u16) -> i32;
}

/// Opens the Windows folder chooser and returns the selected path, or "" if the
/// user cancelled. Runs on an HTTP thread of its own, as the shell dialog needs
/// an STA thread.
pub fn pick_folder(title: &str, initial: &str, owner: HWND) -> String {
    use windows_sys::Win32::System::Com::*;

    unsafe extern "system" fn callback(hwnd: HWND, msg: u32, _l: LPARAM, data: LPARAM) -> i32 {
        const BFFM_INITIALIZED: u32 = 1;
        const BFFM_SETSELECTIONW: u32 = 0x0467;
        if msg == BFFM_INITIALIZED && data != 0 {
            SendMessageW(hwnd, BFFM_SETSELECTIONW, 1, data);
        }
        0
    }
    unsafe {
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let t = wide(title);
        let init = wide(initial);
        let mut display = [0u16; 260];
        let bi = BROWSEINFOW {
            hwnd_owner: owner,
            pidl_root: null(),
            display_name: display.as_mut_ptr(),
            title: t.as_ptr(),
            flags: 0x1 | 0x40 | 0x10, // file system dirs, new dialog style, edit box
            callback: Some(callback),
            lparam: if initial.is_empty() { 0 } else { init.as_ptr() as LPARAM },
            image: 0,
        };
        let pidl = SHBrowseForFolderW(&bi);
        let mut path = String::new();
        if !pidl.is_null() {
            let mut buf = [0u16; 260];
            if SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) != 0 {
                path = from_wide(&buf);
            }
            CoTaskMemFree(pidl as *const _);
        }
        CoUninitialize();
        path
    }
}

pub fn reveal_in_explorer(path: &str) {
    // explorer returns a non-zero exit code even when it works, so it is ignored
    let _ = Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
}

pub fn open_browser(u: &str) {
    let mut c = Command::new("rundll32");
    c.args(["url.dll,FileProtocolHandler", u]);
    hide_window(&mut c);
    let _ = c.spawn();
}
