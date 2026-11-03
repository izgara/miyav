//! The few libobs calls the helper needs, made through obs.dll's C exports. The structs
//! mirror libobs 33's headers.
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::*;

pub const VIDEO_FORMAT_NV12: i32 = 2;
pub const VIDEO_CS_709: i32 = 2;
pub const VIDEO_RANGE_PARTIAL: i32 = 1;
pub const SCALE_BICUBIC: i32 = 2;
pub const SPEAKERS_STEREO: i32 = 2;
pub const BOUNDS_SCALE_INNER: usize = 2;
pub const LOG_INFO: i32 = 300;

static OBS: Mutex<HMODULE> = Mutex::new(0);
static PROCS: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn load_obs(bin: &Path) -> Result<(), String> {
    let path = wide(&bin.join("obs.dll").display().to_string());
    // Its own folder comes first for what it links, as it would if it were the exe's.
    let h = unsafe { LoadLibraryExW(path.as_ptr(), 0, LOAD_WITH_ALTERED_SEARCH_PATH) };
    if h == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    *OBS.lock().unwrap() = h;
    Ok(())
}

fn proc_addr(name: &str) -> usize {
    let mut p = PROCS.lock().unwrap();
    if let Some(&a) = p.get(name) {
        return a;
    }
    let c = format!("{name}\0");
    let a = unsafe { GetProcAddress(*OBS.lock().unwrap(), c.as_ptr()) }
        .map(|f| f as usize)
        .unwrap_or_else(|| panic!("obs.dll has no {name}"));
    p.insert(name.to_string(), a);
    a
}

/// Calls an export with integer and pointer arguments. On x64 Windows there is one
/// calling convention, so the count of arguments is all a signature needs.
pub fn call(name: &str, a: &[usize]) -> usize {
    let f = proc_addr(name);
    type F0 = unsafe extern "system" fn() -> usize;
    type F1 = unsafe extern "system" fn(usize) -> usize;
    type F2 = unsafe extern "system" fn(usize, usize) -> usize;
    type F3 = unsafe extern "system" fn(usize, usize, usize) -> usize;
    type F4 = unsafe extern "system" fn(usize, usize, usize, usize) -> usize;
    type F5 = unsafe extern "system" fn(usize, usize, usize, usize, usize) -> usize;
    unsafe {
        match *a {
            [] => std::mem::transmute::<usize, F0>(f)(),
            [x] => std::mem::transmute::<usize, F1>(f)(x),
            [x, y] => std::mem::transmute::<usize, F2>(f)(x, y),
            [x, y, z] => std::mem::transmute::<usize, F3>(f)(x, y, z),
            [x, y, z, w] => std::mem::transmute::<usize, F4>(f)(x, y, z, w),
            [x, y, z, w, v] => std::mem::transmute::<usize, F5>(f)(x, y, z, w, v),
            _ => panic!("too many arguments for {name}"),
        }
    }
}

pub fn call_bool(name: &str, a: &[usize]) -> bool {
    call(name, a) & 0xff != 0
}

static PINNED: LazyLock<Mutex<HashMap<String, &'static [u8]>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// A NUL-terminated copy of s. libobs keeps some of the pointers it is handed, so the
/// copies are kept alive for the life of the process.
pub fn cstr(s: &str) -> usize {
    let mut p = PINNED.lock().unwrap();
    let b = p.entry(s.to_string()).or_insert_with(|| {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        Box::leak(v.into_boxed_slice())
    });
    b.as_ptr() as usize
}

pub fn from_c(p: usize) -> String {
    if p == 0 {
        return String::new();
    }
    unsafe { CStr::from_ptr(p as *const i8) }.to_string_lossy().to_string()
}

#[repr(C)]
pub struct ObsVideoInfo {
    pub graphics_module: usize,
    pub fps_num: u32,
    pub fps_den: u32,
    pub base_w: u32,
    pub base_h: u32,
    pub out_w: u32,
    pub out_h: u32,
    pub output_format: i32,
    pub adapter: u32,
    pub gpu_conversion: bool,
    pub colorspace: i32,
    pub range: i32,
    pub scale_type: i32,
}

#[repr(C)]
pub struct ObsAudioInfo {
    pub samples_per_sec: u32,
    pub speakers: i32,
}

#[repr(C)]
pub struct CallData {
    stack: usize,
    size: usize,
    capacity: usize,
    fixed: bool,
}

impl Default for CallData {
    fn default() -> Self {
        CallData { stack: 0, size: 0, capacity: 0, fixed: false }
    }
}

impl CallData {
    pub fn free(&mut self) {
        if !self.fixed && self.stack != 0 {
            call("bfree", &[self.stack]);
        }
        *self = CallData::default();
    }

    pub fn str(&self, name: &str) -> String {
        let mut p = 0usize;
        let this = self as *const CallData as usize;
        if !call_bool("calldata_get_string", &[this, cstr(name), &mut p as *mut usize as usize]) {
            return String::new();
        }
        from_c(p)
    }
}

#[repr(C)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

/// A value for an obs_data_t.
pub enum V {
    S(String),
    I(i64),
    B(bool),
}

/// Builds an obs_data_t. The caller releases it.
pub fn obs_data(m: &[(&str, V)]) -> usize {
    let d = call("obs_data_create", &[]);
    for (k, v) in m {
        match v {
            V::S(s) => call("obs_data_set_string", &[d, cstr(k), cstr(s)]),
            V::I(i) => call("obs_data_set_int", &[d, cstr(k), *i as usize]),
            V::B(b) => call("obs_data_set_bool", &[d, cstr(k), *b as usize]),
        };
    }
    d
}

pub fn release_data(d: usize) {
    call("obs_data_release", &[d]);
}

/// Formats a libobs log line. va_list on x64 Windows is a plain pointer, so it passes
/// straight through to the CRT that OBS itself uses.
pub fn vsnprintf(format: *const i8, args: *mut c_void) -> String {
    type F = unsafe extern "system" fn(u64, *mut u8, usize, *const i8, *mut c_void, *mut c_void) -> i32;
    static F_ADDR: LazyLock<usize> = LazyLock::new(|| unsafe {
        let lib = LoadLibraryW(wide("ucrtbase.dll").as_ptr());
        GetProcAddress(lib, c"__stdio_common_vsprintf".as_ptr().cast()).map(|f| f as usize).unwrap_or(0)
    });
    let fallback = || from_c(format as usize);
    if *F_ADDR == 0 {
        return fallback();
    }
    let f: F = unsafe { std::mem::transmute(*F_ADDR) };
    let mut buf = [0u8; 4096];
    const STANDARD_SNPRINTF: u64 = 2; // _CRT_INTERNAL_PRINTF_STANDARD_SNPRINTF_BEHAVIOR
    let n = unsafe { f(STANDARD_SNPRINTF, buf.as_mut_ptr(), buf.len(), format, std::ptr::null_mut(), args) };
    if n < 0 {
        return fallback();
    }
    let n = (n as usize).min(buf.len() - 1);
    String::from_utf8_lossy(&buf[..n]).to_string()
}
