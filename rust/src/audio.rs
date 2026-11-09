//! Audio sources for the replay buffer. Each one becomes its own track, so a clip
//! can be re-balanced later; the first track is always a mix, because browsers and most players only play track one.
//!
//! WASAPI capture: ffmpeg has no WASAPI input, so the app captures each source
//! itself and feeds ffmpeg raw float PCM through one named pipe per track. The
//! handful of COM interfaces involved are called through their vtables by hand.
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use windows_core_shim::*;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Com::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;

use crate::recorder::rec_log;
use crate::win::wide;

mod windows_core_shim {
    pub type Unk = *mut std::ffi::c_void;
}

/// Method i of a COM object's vtable (0-2 are IUnknown's).
///
/// # Safety
/// obj must be a live COM object and F the method's real signature.
pub unsafe fn method<F: Copy>(obj: Unk, i: usize) -> F {
    let vtbl = *(obj as *const *const usize);
    std::mem::transmute_copy(&*vtbl.add(i))
}

/// # Safety
/// obj must be null or a live COM object this code holds a reference on.
pub unsafe fn release(obj: Unk) {
    if !obj.is_null() {
        let f: unsafe extern "system" fn(Unk) -> u32 = method(obj, 2);
        f(obj);
    }
}

fn hresult(what: &str, hr: i32) -> Result<(), String> {
    if hr < 0 {
        return Err(format!("{what} failed (0x{:08X})", hr as u32));
    }
    Ok(())
}

/// Every source is converted to this before it reaches ffmpeg, so ffmpeg never has
/// to be told what a device happens to run at.
const AUDIO_RATE: usize = 48000;
const AUDIO_CHANNELS: usize = 2;
const FRAME_BYTES: usize = 4 * AUDIO_CHANNELS; // float32 samples
/// How far behind real time every track runs: a small buffer so packets arriving a
/// few ms late do not leave holes. It is constant, so the audio timestamps are moved
/// back by the same amount and the two line up.
const AUDIO_DELAY_MS: usize = 50;

pub const TITLE_PC: &str = "PC audio";
pub const TITLE_GAME: &str = "Game";
pub const TITLE_MIC: &str = "Microphone";
pub const TITLE_MIX: &str = "PC audio + mic";

#[derive(Clone, Copy)]
enum Kind {
    Desktop, // everything the speakers play
    DesktopButOurs, // the same, minus this app and its children: the save-clip beeps
    Mic,     // the default microphone
    Process(u32), // one program and its children, nothing else
}

/// Starts every source that works. One that fails (no mic plugged in, a game
/// Windows will not let us listen to) is left out rather than stopping the recording.
pub fn open_tracks(pid: u32, mic: bool) -> Vec<AudioTrack> {
    let mut specs = vec![(Kind::Desktop, TITLE_PC)];
    if pid != 0 {
        specs.push((Kind::Process(pid), TITLE_GAME));
    }
    if mic {
        specs.push((Kind::Mic, TITLE_MIC));
    }
    let mut tracks = Vec::new();
    for (kind, title) in specs {
        let opened = match kind {
            // Not on every Windows build, and a clip with the beep beats a clip with no sound.
            Kind::Desktop => AudioTrack::open(Kind::DesktopButOurs, title).or_else(|e| {
                rec_log(&format!("{title} will include the beeps: {e}"));
                AudioTrack::open(Kind::Desktop, title)
            }),
            _ => AudioTrack::open(kind, title),
        };
        match opened {
            Ok(t) => tracks.push(t),
            Err(e) => rec_log(&format!("recording without {title}: {e}")),
        }
    }
    tracks
}

/// Turns the tracks into ffmpeg inputs (numbered from 1; the video is input 0) and
/// output tracks, and returns the output tracks' titles. Output track one is
/// everything the player heard - PC audio and mic mixed - then PC audio, the game
/// and the mic each on their own.
pub fn layout(tracks: &[AudioTrack]) -> (Vec<String>, Vec<String>) {
    let inputs: Vec<(&str, &str)> = tracks.iter().map(|t| (t.title, t.pipe.as_str())).collect();
    layout_of(&inputs)
}

fn layout_of(tracks: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    let mut args: Vec<String> = Vec::new();
    let mut titles: Vec<String> = Vec::new();
    let input = |title: &str| {
        tracks.iter().position(|t| t.0 == title).map(|i| format!("{}:a", i + 1))
    };
    for (_, pipe) in tracks {
        // Raw PCM: no header to probe, so the format has to be spelled out. Audio
        // runs AUDIO_DELAY behind real time, so its timestamps are moved back by
        // that much. Moving the video forward instead would line up the same, but
        // ffmpeg then holds the video input back to wait for the audio, and ddagrab
        // loses frames while it is not being read.
        args.extend([
            "-itsoffset".to_string(),
            format!("-{:.3}", AUDIO_DELAY_MS as f64 / 1000.0),
            "-thread_queue_size".to_string(),
            "1024".to_string(),
            "-f".to_string(),
            "f32le".to_string(),
            "-ar".to_string(),
            AUDIO_RATE.to_string(),
            "-ac".to_string(),
            AUDIO_CHANNELS.to_string(),
            "-i".to_string(),
            pipe.to_string(),
        ]);
    }
    args.extend(["-map".to_string(), "0:v".to_string()]);
    let (pc, mic, game) = (input(TITLE_PC), input(TITLE_MIC), input(TITLE_GAME));
    match (&pc, &mic) {
        (Some(p), Some(m)) => {
            // PC audio on its own as well, so a clip can drop the mic later without
            // re-recording anything.
            args.extend([
                "-filter_complex".to_string(),
                format!("[{p}][{m}]amix=inputs=2:normalize=0[mix]"),
                "-map".to_string(),
                "[mix]".to_string(),
                "-map".to_string(),
                p.clone(),
            ]);
            titles.extend([TITLE_MIX.to_string(), TITLE_PC.to_string()]);
        }
        (Some(p), None) => {
            args.extend(["-map".to_string(), p.clone()]);
            titles.push(TITLE_PC.to_string());
        }
        _ => {}
    }
    if let Some(g) = game {
        args.extend(["-map".to_string(), g]);
        titles.push(TITLE_GAME.to_string());
    }
    if let Some(m) = mic {
        args.extend(["-map".to_string(), m]);
        titles.push(TITLE_MIC.to_string());
    }
    if !titles.is_empty() {
        args.extend(["-c:a".to_string(), "aac".to_string(), "-b:a".to_string(), "160k".to_string()]);
    }
    (args, titles)
}

// ---- WASAPI ----

const CLSID_ENUMERATOR: GUID = GUID { data1: 0xBCDE0395, data2: 0xE52F, data3: 0x467C, data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E] };
const IID_ENUMERATOR: GUID = GUID { data1: 0xA95664D2, data2: 0x9614, data3: 0x4F35, data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6] };
const IID_AUDIO_CLIENT: GUID = GUID { data1: 0x1CB9AD4C, data2: 0xDBFA, data3: 0x4C32, data4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2] };
const IID_CAPTURE_CLIENT: GUID = GUID { data1: 0xC8ADBD64, data2: 0xE71E, data3: 0x48A0, data4: [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17] };
const IID_UNKNOWN: GUID = GUID { data1: 0, data2: 0, data3: 0, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_AGILE: GUID = GUID { data1: 0x94EA2B94, data2: 0xE9CC, data3: 0x49E0, data4: [0xC0, 0xFF, 0xEE, 0x64, 0xCA, 0x8F, 0x5B, 0x90] };
const IID_COMPLETION: GUID = GUID { data1: 0x41D949AB, data2: 0x9862, data3: 0x444A, data4: [0x80, 0xF6, 0xC2, 0x61, 0x33, 0x4D, 0xA5, 0xEB] };

fn same(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

// IAudioClient
const AC_INITIALIZE: usize = 3;
const AC_START: usize = 10;
const AC_STOP: usize = 11;
const AC_SET_EVENT: usize = 13;
const AC_GET_SERVICE: usize = 14;
// IAudioCaptureClient
const CC_GET_BUFFER: usize = 3;
const CC_RELEASE_BUFFER: usize = 4;
const CC_NEXT_PACKET: usize = 5;

const STREAM_LOOPBACK: u32 = 0x0002_0000;
const STREAM_EVENT: u32 = 0x0004_0000;
const STREAM_AUTOCONVERT: u32 = 0x8000_0000; // let Windows resample to our format
const STREAM_SRC_DEFAULT: u32 = 0x0800_0000;
const BUFFER_SILENT: u32 = 0x2;

/// WAVEFORMATEX for 48 kHz float stereo.
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

static WAVE_FORMAT: WaveFormat = WaveFormat {
    tag: 3,
    channels: AUDIO_CHANNELS as u16,
    rate: AUDIO_RATE as u32,
    avg_bytes: (AUDIO_RATE * FRAME_BYTES) as u32,
    block_align: FRAME_BYTES as u16,
    bits: 32,
    extra: 0,
};

/// An IAudioClient on the default speakers or mic.
unsafe fn open_device_client(capture: bool) -> Result<Unk, String> {
    let mut en: Unk = null_mut();
    hresult(
        "opening audio devices",
        CoCreateInstance(&CLSID_ENUMERATOR, null_mut(), CLSCTX_ALL, &IID_ENUMERATOR, &mut en),
    )?;
    let flow = if capture { 1 } else { 0 }; // eCapture / eRender
    let default_endpoint: unsafe extern "system" fn(Unk, i32, i32, *mut Unk) -> i32 = method(en, 4);
    let mut dev: Unk = null_mut();
    let r = default_endpoint(en, flow, 0, &mut dev);
    release(en);
    hresult("finding the default device", r)?;
    let activate: unsafe extern "system" fn(Unk, *const GUID, u32, *const c_void, *mut Unk) -> i32 = method(dev, 3);
    let mut client: Unk = null_mut();
    let r = activate(dev, &IID_AUDIO_CLIENT, CLSCTX_ALL, null(), &mut client);
    release(dev);
    hresult("activating the device", r)?;
    Ok(client)
}

// ---- process loopback (one program's audio) ----
//
// Process loopback is only reachable through ActivateAudioInterfaceAsync, which
// reports back through a COM completion handler. Ours is a static object with a
// hand-made vtable; activations are rare (once per game) so they simply take turns.

static ACTIVATION: Mutex<()> = Mutex::new(());
static ACTIVATED: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

unsafe extern "system" fn h_query(this: Unk, riid: *const GUID, ppv: *mut Unk) -> i32 {
    // IAgileObject matters: without it Windows refuses to call us back from its own
    // thread.
    let id = &*riid;
    if same(id, &IID_UNKNOWN) || same(id, &IID_AGILE) || same(id, &IID_COMPLETION) {
        *ppv = this;
        return 0;
    }
    *ppv = null_mut();
    0x80004002u32 as i32 // E_NOINTERFACE
}

// A static object: no refcount.
unsafe extern "system" fn h_ref(_: Unk) -> u32 {
    1
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
    kind: u32, // AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK = 1
    pid: u32,
    mode: u32, // 0: only the process tree, 1: everything but it
}

#[repr(C)]
struct PropVariantBlob {
    vt: u16,
    pad: [u16; 3],
    size: u32,
    pad2: u32,
    blob: *const LoopbackParams,
}

unsafe fn open_process_client(pid: u32, mode: u32) -> Result<Unk, String> {
    type Activate = unsafe extern "system" fn(*const u16, *const GUID, *const PropVariantBlob, *const Handler, *mut Unk) -> i32;
    let lib = LoadLibraryW(wide("mmdevapi.dll").as_ptr());
    let Some(f) = GetProcAddress(lib, b"ActivateAudioInterfaceAsync\0".as_ptr()) else {
        return Err("this version of Windows cannot record a single program's audio".into());
    };
    let activate: Activate = std::mem::transmute(f);
    let _turn = ACTIVATION.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    *ACTIVATED.lock().unwrap() = Some(tx);

    let params = LoopbackParams { kind: 1, pid, mode };
    let pv = PropVariantBlob {
        vt: 65, // VT_BLOB
        pad: [0; 3],
        size: std::mem::size_of::<LoopbackParams>() as u32,
        pad2: 0,
        blob: &params,
    };
    let vtbl = HANDLER_VTBL.get_or_init(|| Vtbl([h_query as *const () as usize, h_ref as *const () as usize, h_ref as *const () as usize, h_done as *const () as usize]));
    let handler = HANDLER.get_or_init(|| Handler { vtbl: &vtbl.0 });
    let path = wide("VAD\\Process_Loopback");
    let mut op: Unk = null_mut();
    hresult("starting process audio capture", activate(path.as_ptr(), &IID_AUDIO_CLIENT, &pv, handler, &mut op))?;
    let got = rx.recv_timeout(Duration::from_secs(5));
    *ACTIVATED.lock().unwrap() = None;
    if got.is_err() {
        release(op);
        return Err("Windows did not answer the process audio request".into());
    }
    let result: unsafe extern "system" fn(Unk, *mut i32, *mut Unk) -> i32 = method(op, 3);
    let (mut act_hr, mut unk) = (0i32, null_mut());
    let r = result(op, &mut act_hr, &mut unk);
    release(op);
    hresult("process audio capture", r)?;
    hresult("process audio capture", act_hr)?;
    let query: unsafe extern "system" fn(Unk, *const GUID, *mut Unk) -> i32 = method(unk, 0);
    let mut client: Unk = null_mut();
    let r = query(unk, &IID_AUDIO_CLIENT, &mut client);
    release(unk);
    hresult("process audio capture", r)?;
    Ok(client)
}

struct Stream {
    client: Unk,
    cc: Unk,
    event: HANDLE,
}

unsafe fn start_stream(kind: Kind) -> Result<Stream, String> {
    let mut flags = STREAM_EVENT | STREAM_AUTOCONVERT | STREAM_SRC_DEFAULT;
    let client = match kind {
        Kind::Desktop => {
            flags |= STREAM_LOOPBACK;
            open_device_client(false)?
        }
        Kind::Mic => open_device_client(true)?,
        Kind::Process(pid) => {
            flags |= STREAM_LOOPBACK;
            open_process_client(pid, 0)?
        }
        Kind::DesktopButOurs => {
            flags |= STREAM_LOOPBACK;
            open_process_client(std::process::id(), 1)?
        }
    };
    let mut s = Stream { client, cc: null_mut(), event: 0 };
    let r = (|| {
        let init: unsafe extern "system" fn(Unk, i32, u32, i64, i64, *const WaveFormat, *const GUID) -> i32 =
            method(client, AC_INITIALIZE);
        const BUFFER_HNS: i64 = 200 * 10000; // 200 ms
        hresult("starting audio capture", init(client, 0, flags, BUFFER_HNS, 0, &WAVE_FORMAT, null()))?;
        s.event = CreateEventW(null(), 0, 0, null());
        let set_event: unsafe extern "system" fn(Unk, HANDLE) -> i32 = method(client, AC_SET_EVENT);
        set_event(client, s.event);
        let service: unsafe extern "system" fn(Unk, *const GUID, *mut Unk) -> i32 = method(client, AC_GET_SERVICE);
        hresult("starting audio capture", service(client, &IID_CAPTURE_CLIENT, &mut s.cc))?;
        let start: unsafe extern "system" fn(Unk) -> i32 = method(client, AC_START);
        hresult("starting audio capture", start(client))
    })();
    match r {
        Ok(_) => Ok(s),
        Err(e) => {
            s.close();
            Err(e)
        }
    }
}

impl Stream {
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
}

// ---- a track: one source, one pipe ----

pub struct AudioTrack {
    pub pipe: String, // what ffmpeg opens
    pub title: &'static str,
    handle: HANDLE,
    quit: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

static PIPE_SEQ: AtomicU32 = AtomicU32::new(0);

type Pending = Arc<Mutex<Vec<u8>>>;

impl AudioTrack {
    /// Starts capturing a source. It returns once capture is known to work, so a
    /// missing mic just means one track fewer rather than no recording.
    fn open(kind: Kind, title: &'static str) -> Result<AudioTrack, String> {
        let pipe = format!(r"\\.\pipe\miyav-{}-{}", std::process::id(), PIPE_SEQ.fetch_add(1, Ordering::SeqCst) + 1);
        const OUT_BUF: u32 = 1 << 20;
        let handle = unsafe {
            CreateNamedPipeW(wide(&pipe).as_ptr(), PIPE_ACCESS_OUTBOUND, 0, 1, OUT_BUF, 0, 0, null())
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err("could not create the audio pipe".into());
        }
        let quit = Arc::new(AtomicBool::new(false));
        let pending: Pending = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (q, p) = (quit.clone(), pending.clone());
        let cap = std::thread::spawn(move || capture(kind, title, q, p, ready_tx));
        let mut track = AudioTrack { pipe, title, handle, quit, threads: vec![cap] };
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("audio capture stopped".into()),
        }
        let (q, h) = (track.quit.clone(), handle);
        track.threads.push(std::thread::spawn(move || feed(h, q, pending)));
        Ok(track)
    }
}

/// Stops capture and releases the pipe.
impl Drop for AudioTrack {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::SeqCst);
        // A feed still waiting for ffmpeg to connect is woken by connecting to the
        // pipe ourselves.
        unsafe {
            let h = CreateFileW(wide(&self.pipe).as_ptr(), GENERIC_READ, 0, null(), OPEN_EXISTING, 0, 0);
            if h != INVALID_HANDLE_VALUE {
                CloseHandle(h);
            }
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        unsafe { CloseHandle(self.handle) };
    }
}

/// Runs the WASAPI client on its own COM thread and queues what it hears for feed.
/// If the device goes away (headset unplugged), it keeps trying to reopen the
/// source; feed fills the gap with silence meanwhile.
fn capture(kind: Kind, title: &str, quit: Arc<AtomicBool>, pending: Pending, ready: mpsc::Sender<Result<(), String>>) {
    unsafe {
        CoInitializeEx(null(), COINIT_MULTITHREADED as u32);
        let mut s = match start_stream(kind) {
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
            let r = drain(&s, &quit, &pending);
            s.close();
            let Err(e) = r else { break };
            rec_log(&format!("{title}: capture lost ({e}), reconnecting"));
            let again = loop {
                std::thread::sleep(Duration::from_secs(1));
                if quit.load(Ordering::SeqCst) {
                    break None;
                }
                if let Ok(n) = start_stream(kind) {
                    rec_log(&format!("{title}: capture back"));
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

/// Queues packets until quit (Ok) or the device fails (its error).
unsafe fn drain(s: &Stream, quit: &AtomicBool, pending: &Pending) -> Result<(), String> {
    let next: unsafe extern "system" fn(Unk, *mut u32) -> i32 = method(s.cc, CC_NEXT_PACKET);
    let get: unsafe extern "system" fn(Unk, *mut *mut u8, *mut u32, *mut u32, *mut u64, *mut u64) -> i32 =
        method(s.cc, CC_GET_BUFFER);
    let rel: unsafe extern "system" fn(Unk, u32) -> i32 = method(s.cc, CC_RELEASE_BUFFER);
    loop {
        // The timeout is what notices quit: loopback sends no events at all while
        // nothing is playing.
        WaitForSingleObject(s.event, 100);
        if quit.load(Ordering::SeqCst) {
            return Ok(());
        }
        loop {
            let mut frames = 0u32;
            hresult("reading audio", next(s.cc, &mut frames))?;
            if frames == 0 {
                break;
            }
            let (mut data, mut flags) = (null_mut(), 0u32);
            let hr = get(s.cc, &mut data, &mut frames, &mut flags, null_mut(), null_mut());
            hresult("reading audio", hr)?;
            if hr != 0 {
                break; // AUDCLNT_S_BUFFER_EMPTY
            }
            let n = frames as usize * FRAME_BYTES;
            {
                let mut p = pending.lock().unwrap();
                if flags & BUFFER_SILENT != 0 || data.is_null() {
                    let len = p.len();
                    p.resize(len + n, 0);
                } else {
                    p.extend_from_slice(std::slice::from_raw_parts(data, n));
                }
            }
            rel(s.cc, frames);
        }
    }
}

/// How much of the queue to write now, and how much to drop from its front, given
/// what is due. Pure, so the pacing rules can be tested.
fn pace(pending: usize, want: usize, depth: usize, primed: &mut bool) -> (usize, usize) {
    // Too much queued (a device clock running fast) is trimmed back.
    let excess = pending.saturating_sub(depth + want + depth);
    let trim = excess - excess % FRAME_BYTES;
    let pending = pending - trim;
    if !*primed && pending >= depth {
        *primed = true;
    }
    let mut take = 0;
    if *primed {
        take = want.min(pending);
        // Ran dry: the source went quiet (or hiccuped). Queue up again before
        // playing anything more, so the delay stays the same.
        *primed = take == want;
    }
    (trim, take)
}

/// Writes to ffmpeg at exactly the wall-clock rate. Raw PCM has no timestamps -
/// ffmpeg times it by counting samples - so the stream has to be continuous: speakers
/// playing nothing produce no packets at all, and without silence in their place the
/// audio would slide ahead of the video.
///
/// Sound is only played once AUDIO_DELAY of it is queued, which keeps the delay the
/// same whether the audio has been running for an hour or just started after a
/// silence.
fn feed(pipe: HANDLE, quit: Arc<AtomicBool>, pending: Pending) {
    unsafe {
        const ERROR_PIPE_CONNECTED_: u32 = 535;
        if ConnectNamedPipe(pipe, null_mut()) == 0 && GetLastError() != ERROR_PIPE_CONNECTED_ {
            return;
        }
    }
    if quit.load(Ordering::SeqCst) {
        return;
    }
    let depth = AUDIO_DELAY_MS * AUDIO_RATE / 1000 * FRAME_BYTES;
    pending.lock().unwrap().clear(); // heard before ffmpeg was listening

    let start = Instant::now();
    let mut written = 0usize;
    let mut primed = false;
    let mut out: Vec<u8> = Vec::new();
    loop {
        std::thread::sleep(Duration::from_millis(10));
        if quit.load(Ordering::SeqCst) {
            return;
        }
        let due = (start.elapsed().as_secs_f64() * AUDIO_RATE as f64) as usize;
        let due = due.saturating_sub(written);
        if due == 0 {
            continue;
        }
        let want = due * FRAME_BYTES;
        out.clear();
        {
            let mut p = pending.lock().unwrap();
            let (trim, take) = pace(p.len(), want, depth, &mut primed);
            p.drain(..trim);
            out.extend(p.drain(..take));
        }
        out.resize(want, 0);
        let mut n = 0u32;
        if unsafe { WriteFile(pipe, out.as_ptr(), out.len() as u32, &mut n, null_mut()) } == 0 {
            return; // ffmpeg is gone
        }
        written += due;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_with_everything() {
        let (args, titles) = layout_of(&[(TITLE_PC, "p1"), (TITLE_GAME, "p2"), (TITLE_MIC, "p3")]);
        assert_eq!(titles, [TITLE_MIX, TITLE_PC, TITLE_GAME, TITLE_MIC]);
        let s = args.join(" ");
        assert!(s.contains("[1:a][3:a]amix=inputs=2:normalize=0[mix]"), "{s}");
        assert!(s.contains("-i p1") && s.contains("-i p2") && s.contains("-i p3"));
        assert!(s.contains("-itsoffset -0.050"));
        assert!(s.ends_with("-c:a aac -b:a 160k"));
    }

    #[test]
    fn layout_without_mic_or_any() {
        let (_, titles) = layout_of(&[(TITLE_PC, "p")]);
        assert_eq!(titles, [TITLE_PC]);
        let (args, titles) = layout_of(&[]);
        assert!(titles.is_empty());
        assert_eq!(args, ["-map", "0:v"]);
    }

    #[test]
    fn pacing() {
        let depth = 800;
        let mut primed = false;
        // Not enough queued: silence.
        assert_eq!(pace(400, 80, depth, &mut primed), (0, 0));
        assert!(!primed);
        // Enough: plays.
        assert_eq!(pace(800, 80, depth, &mut primed), (0, 80));
        assert!(primed);
        // Runs dry: plays what is left, then queues up again.
        assert_eq!(pace(40, 80, depth, &mut primed), (0, 40));
        assert!(!primed);
        // Too much queued: trimmed to whole frames.
        let mut p = true;
        let (trim, take) = pace(4000, 80, depth, &mut p);
        assert_eq!((trim, take), (4000 - 1680, 80));
        assert_eq!(trim % FRAME_BYTES, 0);
    }

    #[test]
    fn wave_format_is_the_size_windows_expects() {
        assert_eq!(std::mem::size_of::<WaveFormat>(), 18);
    }
}
