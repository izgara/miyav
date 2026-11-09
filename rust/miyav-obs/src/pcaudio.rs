//! The "PC audio" track without the app's own sounds. OBS's desktop audio capture hears
//! everything the speakers play, the save-clip beeps included, and it has no way to
//! leave a program out. Windows' process loopback does, so this captures with that and
//! hands the audio to libobs through a source of our own. Only Windows builds that have
//! process loopback get it; start() fails on the others and the caller uses OBS's.
use crate::libobs::{call, cstr, SPEAKERS_STEREO};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, Once};
use std::time::Duration;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Com::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Threading::*;

type Unk = *mut c_void;

const SOURCE_ID: &str = "miyav_pc_audio";
const OBS_SOURCE_TYPE_INPUT: i32 = 0;
const OBS_SOURCE_AUDIO: u32 = 1 << 1;
const AUDIO_FORMAT_FLOAT: i32 = 4;
const RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const FRAME_BYTES: u32 = 8;

/// The start of libobs 33's obs_source_info: obs_register_source_s copies only as many
/// bytes as it is told, and an audio-only input needs nothing past these.
#[repr(C)]
struct SourceInfo {
    id: usize,
    kind: i32,
    output_flags: u32,
    get_name: unsafe extern "C" fn(usize) -> usize,
    create: unsafe extern "C" fn(usize, usize) -> usize,
    destroy: unsafe extern "C" fn(usize),
}

/// libobs reads a few fields of the full struct before it copies the part it was given.
#[repr(C)]
struct SourceInfoPadded {
    info: SourceInfo,
    rest: [u64; 64],
}

#[repr(C)]
struct SourceAudio {
    data: [*const u8; 8],
    frames: u32,
    speakers: i32,
    format: i32,
    samples_per_sec: u32,
    timestamp: u64,
}

unsafe extern "C" fn get_name(_: usize) -> usize {
    cstr("PC audio")
}
unsafe extern "C" fn create(_: usize, _: usize) -> usize {
    1 // no state of its own, but null would mean it failed
}
unsafe extern "C" fn destroy(_: usize) {}

static REGISTER: Once = Once::new();

fn register() {
    REGISTER.call_once(|| {
        let p = SourceInfoPadded {
            info: SourceInfo {
                id: cstr(SOURCE_ID),
                kind: OBS_SOURCE_TYPE_INPUT,
                output_flags: OBS_SOURCE_AUDIO,
                get_name,
                create,
                destroy,
            },
            rest: [0; 64],
        };
        call("obs_register_source_s", &[&p as *const SourceInfoPadded as usize, std::mem::size_of::<SourceInfo>()]);
    });
}

/// A running capture; dropping it stops the thread. The source has to outlive it.
pub struct PcAudio {
    pub source: usize,
    quit: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for PcAudio {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts capturing everything except process `pid` and its children.
pub fn start(pid: u32) -> Result<PcAudio, String> {
    register();
    let source = call("obs_source_create", &[cstr(SOURCE_ID), cstr("PC audio"), 0, 0]);
    if source == 0 {
        return Err("could not create the source".into());
    }
    let quit = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();
    let q = quit.clone();
    let thread = std::thread::spawn(move || run(pid, source, q, ready_tx));
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PcAudio { source, quit, thread: Some(thread) }),
        r => {
            let _ = thread.join();
            call("obs_source_release", &[source]);
            Err(match r {
                Ok(Err(e)) => e,
                _ => "capture stopped".into(),
            })
        }
    }
}

fn run(pid: u32, source: usize, quit: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>) {
    unsafe {
        CoInitializeEx(null(), COINIT_MULTITHREADED as u32);
        let mut s = match Stream::open(pid) {
            Ok(s) => {
                let _ = ready.send(Ok(()));
                s
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                CoUninitialize();
                return;
            }
        };
        loop {
            let r = s.drain(source, &quit);
            s.close();
            if r.is_ok() {
                break;
            }
            // The audio device went away; try again until it is back.
            let again = loop {
                std::thread::sleep(Duration::from_secs(1));
                if quit.load(Ordering::SeqCst) {
                    break None;
                }
                if let Ok(n) = Stream::open(pid) {
                    break Some(n);
                }
            };
            match again {
                Some(n) => s = n,
                None => break,
            }
        }
        CoUninitialize();
    }
}

// ---- WASAPI, through vtables as in the app's audio.rs ----

const IID_AUDIO_CLIENT: GUID = GUID { data1: 0x1CB9AD4C, data2: 0xDBFA, data3: 0x4C32, data4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2] };
const IID_CAPTURE_CLIENT: GUID = GUID { data1: 0xC8ADBD64, data2: 0xE71E, data3: 0x48A0, data4: [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17] };
const IID_UNKNOWN: GUID = GUID { data1: 0, data2: 0, data3: 0, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_AGILE: GUID = GUID { data1: 0x94EA2B94, data2: 0xE9CC, data3: 0x49E0, data4: [0xC0, 0xFF, 0xEE, 0x64, 0xCA, 0x8F, 0x5B, 0x90] };
const IID_COMPLETION: GUID = GUID { data1: 0x41D949AB, data2: 0x9862, data3: 0x444A, data4: [0x80, 0xF6, 0xC2, 0x61, 0x33, 0x4D, 0xA5, 0xEB] };

const AC_INITIALIZE: usize = 3;
const AC_START: usize = 10;
const AC_STOP: usize = 11;
const AC_SET_EVENT: usize = 13;
const AC_GET_SERVICE: usize = 14;
const CC_GET_BUFFER: usize = 3;
const CC_RELEASE_BUFFER: usize = 4;
const CC_NEXT_PACKET: usize = 5;

const STREAM_LOOPBACK: u32 = 0x0002_0000;
const STREAM_EVENT: u32 = 0x0004_0000;
const STREAM_AUTOCONVERT: u32 = 0x8000_0000;
const STREAM_SRC_DEFAULT: u32 = 0x0800_0000;
const BUFFER_SILENT: u32 = 0x2;

unsafe fn method<F: Copy>(obj: Unk, i: usize) -> F {
    let vtbl = *(obj as *const *const usize);
    std::mem::transmute_copy(&*vtbl.add(i))
}

unsafe fn release(obj: Unk) {
    if !obj.is_null() {
        let f: unsafe extern "system" fn(Unk) -> u32 = method(obj, 2);
        f(obj);
    }
}

fn check(what: &str, hr: i32) -> Result<(), String> {
    if hr < 0 {
        Err(format!("{what}: 0x{:08x}", hr as u32))
    } else {
        Ok(())
    }
}

#[repr(C, packed(2))]
struct WaveFormat {
    tag: u16,
    channels: u16,
    rate: u32,
    avg_bytes: u32,
    block_align: u16,
    bits: u16,
    extra: u16,
}

static FORMAT: WaveFormat = WaveFormat {
    tag: 3, // float
    channels: CHANNELS,
    rate: RATE,
    avg_bytes: RATE * FRAME_BYTES,
    block_align: FRAME_BYTES as u16,
    bits: 32,
    extra: 0,
};

static ACTIVATED: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

fn same(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

unsafe extern "system" fn h_query(this: Unk, riid: *const GUID, ppv: *mut Unk) -> i32 {
    // IAgileObject matters: without it Windows refuses to call back from its own thread.
    let id = &*riid;
    if same(id, &IID_UNKNOWN) || same(id, &IID_AGILE) || same(id, &IID_COMPLETION) {
        *ppv = this;
        return 0;
    }
    *ppv = null_mut();
    0x80004002u32 as i32 // E_NOINTERFACE
}

unsafe extern "system" fn h_ref(_: Unk) -> u32 {
    1 // a static object
}

unsafe extern "system" fn h_done(_: Unk, _op: Unk) -> i32 {
    if let Some(tx) = ACTIVATED.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
    0
}

#[repr(C)]
struct Handler {
    vtbl: *const [usize; 4],
}
unsafe impl Sync for Handler {}
unsafe impl Send for Handler {}

struct Vtbl([usize; 4]);
unsafe impl Sync for Vtbl {}

static HANDLER_VTBL: std::sync::OnceLock<Vtbl> = std::sync::OnceLock::new();
static HANDLER: std::sync::OnceLock<Handler> = std::sync::OnceLock::new();

#[repr(C)]
struct LoopbackParams {
    kind: u32, // AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK
    pid: u32,
    mode: u32, // PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
}

#[repr(C)]
struct PropVariantBlob {
    vt: u16,
    pad: [u16; 3],
    size: u32,
    pad2: u32,
    blob: *const LoopbackParams,
}

struct Stream {
    client: Unk,
    cc: Unk,
    event: HANDLE,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn open_client(pid: u32) -> Result<Unk, String> {
    type Activate = unsafe extern "system" fn(*const u16, *const GUID, *const PropVariantBlob, *const Handler, *mut Unk) -> i32;
    let lib = LoadLibraryW(wide("mmdevapi.dll").as_ptr());
    let Some(f) = GetProcAddress(lib, b"ActivateAudioInterfaceAsync\0".as_ptr()) else {
        return Err("no ActivateAudioInterfaceAsync".into());
    };
    let activate: Activate = std::mem::transmute(f);
    let (tx, rx) = mpsc::channel();
    *ACTIVATED.lock().unwrap() = Some(tx);
    let params = LoopbackParams { kind: 1, pid, mode: 1 };
    let pv = PropVariantBlob { vt: 65, pad: [0; 3], size: std::mem::size_of::<LoopbackParams>() as u32, pad2: 0, blob: &params };
    let vtbl = HANDLER_VTBL.get_or_init(|| Vtbl([h_query as *const () as usize, h_ref as *const () as usize, h_ref as *const () as usize, h_done as *const () as usize]));
    let handler = HANDLER.get_or_init(|| Handler { vtbl: &vtbl.0 });
    let path = wide("VAD\\Process_Loopback");
    let mut op: Unk = null_mut();
    check("activating", activate(path.as_ptr(), &IID_AUDIO_CLIENT, &pv, handler, &mut op))?;
    let got = rx.recv_timeout(Duration::from_secs(5));
    *ACTIVATED.lock().unwrap() = None;
    if got.is_err() {
        release(op);
        return Err("Windows did not answer".into());
    }
    let result: unsafe extern "system" fn(Unk, *mut i32, *mut Unk) -> i32 = method(op, 3);
    let (mut act_hr, mut unk) = (0i32, null_mut());
    let r = result(op, &mut act_hr, &mut unk);
    release(op);
    check("activation", r)?;
    check("activation", act_hr)?;
    let query: unsafe extern "system" fn(Unk, *const GUID, *mut Unk) -> i32 = method(unk, 0);
    let mut client: Unk = null_mut();
    let r = query(unk, &IID_AUDIO_CLIENT, &mut client);
    release(unk);
    check("activation", r)?;
    Ok(client)
}

impl Stream {
    unsafe fn open(pid: u32) -> Result<Stream, String> {
        let client = open_client(pid)?;
        let mut s = Stream { client, cc: null_mut(), event: 0 };
        let r = (|| {
            let init: unsafe extern "system" fn(Unk, i32, u32, i64, i64, *const WaveFormat, *const GUID) -> i32 = method(client, AC_INITIALIZE);
            let flags = STREAM_LOOPBACK | STREAM_EVENT | STREAM_AUTOCONVERT | STREAM_SRC_DEFAULT;
            check("initialize", init(client, 0, flags, 200 * 10000, 0, &FORMAT, null()))?;
            s.event = CreateEventW(null(), 0, 0, null());
            let set_event: unsafe extern "system" fn(Unk, HANDLE) -> i32 = method(client, AC_SET_EVENT);
            set_event(client, s.event);
            let service: unsafe extern "system" fn(Unk, *const GUID, *mut Unk) -> i32 = method(client, AC_GET_SERVICE);
            check("capture client", service(client, &IID_CAPTURE_CLIENT, &mut s.cc))?;
            let start: unsafe extern "system" fn(Unk) -> i32 = method(client, AC_START);
            check("start", start(client))
        })();
        match r {
            Ok(_) => Ok(s),
            Err(e) => {
                s.close();
                Err(e)
            }
        }
    }

    unsafe fn close(&self) {
        if !self.client.is_null() {
            let stop: unsafe extern "system" fn(Unk) -> i32 = method(self.client, AC_STOP);
            stop(self.client);
        }
        release(self.cc);
        release(self.client);
        if self.event != 0 {
            CloseHandle(self.event);
        }
    }

    /// Passes packets to libobs until quit (Ok) or the device fails.
    unsafe fn drain(&self, source: usize, quit: &AtomicBool) -> Result<(), String> {
        let next: unsafe extern "system" fn(Unk, *mut u32) -> i32 = method(self.cc, CC_NEXT_PACKET);
        let get: unsafe extern "system" fn(Unk, *mut *mut u8, *mut u32, *mut u32, *mut u64, *mut u64) -> i32 = method(self.cc, CC_GET_BUFFER);
        let rel: unsafe extern "system" fn(Unk, u32) -> i32 = method(self.cc, CC_RELEASE_BUFFER);
        let mut silence: Vec<u8> = Vec::new();
        loop {
            // The timeout is what notices quit: nothing is signalled while all is quiet.
            WaitForSingleObject(self.event, 100);
            if quit.load(Ordering::SeqCst) {
                return Ok(());
            }
            loop {
                let mut frames = 0u32;
                check("reading", next(self.cc, &mut frames))?;
                if frames == 0 {
                    break;
                }
                let (mut data, mut flags) = (null_mut(), 0u32);
                let hr = get(self.cc, &mut data, &mut frames, &mut flags, null_mut(), null_mut());
                check("reading", hr)?;
                if hr != 0 {
                    break; // AUDCLNT_S_BUFFER_EMPTY
                }
                let mut plane = data as *const u8;
                if flags & BUFFER_SILENT != 0 || data.is_null() {
                    silence.resize((frames * FRAME_BYTES) as usize, 0);
                    plane = silence.as_ptr();
                }
                let mut planes = [null::<u8>(); 8];
                planes[0] = plane;
                // Stamped on arrival, as OBS does for its own process capture.
                let now = call("os_gettime_ns", &[]) as u64;
                let audio = SourceAudio {
                    data: planes,
                    frames,
                    speakers: SPEAKERS_STEREO,
                    format: AUDIO_FORMAT_FLOAT,
                    samples_per_sec: RATE,
                    timestamp: now.saturating_sub(frames as u64 * 1_000_000_000 / RATE as u64),
                };
                call("obs_source_output_audio", &[source, &audio as *const SourceAudio as usize]);
                rel(self.cc, frames);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_libobs() {
        assert_eq!(std::mem::size_of::<SourceInfo>(), 40);
        assert_eq!(std::mem::offset_of!(SourceAudio, frames), 64);
        assert_eq!(std::mem::offset_of!(SourceAudio, timestamp), 80);
        assert_eq!(std::mem::size_of::<LoopbackParams>(), 12);
    }
}
