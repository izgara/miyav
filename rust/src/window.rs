//! The window, and the WebView behind it, exist only while it is open. Closing it
//! really closes it - the WebView's browser processes take a hundred MB or so - and
//! the app carries on in the tray, recording. Opening it again from the tray makes a
//! new one.
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::WindowExtWindows;
use tao::window::{Window, WindowBuilder};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows_sys::Win32::UI::Shell::ExtractIconW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use wry::{WebContext, WebView, WebViewBuilder};

use crate::win::wide;
use crate::{ffmpeg, settings, tray};

#[derive(Debug, Clone, Copy)]
enum Ev {
    Open,
    Quit,
}

static HWND_NOW: AtomicIsize = AtomicIsize::new(0);
static QUITTING: AtomicBool = AtomicBool::new(false);
static PROXY: OnceLock<EventLoopProxy<Ev>> = OnceLock::new();

pub fn hwnd() -> HWND {
    HWND_NOW.load(Ordering::SeqCst)
}

/// Brings the window to the front, making one if there is none.
pub fn show() {
    let h = hwnd();
    if h == 0 {
        if let Some(p) = PROXY.get() {
            let _ = p.send_event(Ev::Open);
        }
        return;
    }
    unsafe {
        ShowWindow(h, if IsIconic(h) != 0 { SW_RESTORE } else { SW_SHOW });
        SetForegroundWindow(h);
    }
}

pub fn quit_app() {
    QUITTING.store(true, Ordering::SeqCst);
    if let Some(p) = PROXY.get() {
        let _ = p.send_event(Ev::Quit);
    }
}

/// The page's own title-bar buttons. Returns whether the window is maximized.
pub fn action(what: &str) -> Result<bool, String> {
    let h = hwnd();
    if h == 0 {
        return Err("the window is not ready yet".into());
    }
    unsafe {
        match what {
            "minimize" => {
                ShowWindow(h, SW_MINIMIZE);
            }
            "maximize" => {
                ShowWindow(h, if IsZoomed(h) != 0 { SW_RESTORE } else { SW_MAXIMIZE });
            }
            "close" => {
                PostMessageW(h, WM_CLOSE, 0, 0);
            }
            "drag" => {
                // Hand the drag to Windows so snapping and multi-monitor work the same
                // as on a normal title bar.
                ReleaseCapture();
                const HT_CAPTION: usize = 2;
                SendMessageW(h, WM_NCLBUTTONDOWN, HT_CAPTION, 0);
            }
            _ => return Err("unknown window action".into()),
        }
        Ok(IsZoomed(h) != 0)
    }
}

/// Removes the pale 1px DWM border and darkens the caption, so no light strip shows
/// at the top.
fn style_window(h: HWND) {
    unsafe {
        let set = |attr: DWMWINDOWATTRIBUTE, v: u32| {
            DwmSetWindowAttribute(h, attr as u32, (&v as *const u32).cast(), 4);
        };
        set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
        set(DWMWA_BORDER_COLOR, 0xFFFF_FFFE); // DWMWA_COLOR_NONE
        set(DWMWA_CAPTION_COLOR, 0x0014_0F0D); // 0x00BGR - matches --bg (#0d0f14)

        // The first icon out of our own executable rather than a resource id, so it
        // cannot drift if the resource layout changes.
        let exe = wide(&ffmpeg::exe_path().display().to_string());
        let icon = ExtractIconW(GetModuleHandleW(std::ptr::null()), exe.as_ptr(), 0);
        if icon != 0 && icon != 1 {
            SendMessageW(h, WM_SETICON, 1, icon); // ICON_BIG
            SendMessageW(h, WM_SETICON, 0, icon); // ICON_SMALL
        }
    }
}

/// Says once, ever, where the app went when the window closed.
fn first_close_hint() {
    if settings::get().tray_hint_shown {
        return;
    }
    settings::update(|s| s.tray_hint_shown = true);
    let _ = settings::save();
    tray::balloon("Still running", "Miyav keeps recording here. Right-click this icon to quit.");
}

/// Runs the window for as long as the app lives; returns when it is told to quit.
pub fn run(addr: String, in_tray: bool) -> Result<(), String> {
    let mut el = EventLoopBuilder::<Ev>::with_user_event().build();
    let _ = PROXY.set(el.create_proxy());
    let mut context = WebContext::new(Some(settings::app_dir().join("webview")));
    let mut open: Option<(Window, WebView)> = None;
    let mut fatal: Option<String> = None;
    let mut first = !in_tray;

    el.run_return(|event, target, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(tao::event::StartCause::Init) if first => {
                first = false;
                if let Err(e) = open_window(target, &addr, &mut context, &mut open) {
                    fatal = Some(e);
                    *flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(Ev::Open) => {
                if open.is_none() {
                    if let Err(e) = open_window(target, &addr, &mut context, &mut open) {
                        crate::win::show_error("Could not open the window", &e);
                    }
                } else {
                    show();
                }
            }
            Event::UserEvent(Ev::Quit) => {
                HWND_NOW.store(0, Ordering::SeqCst);
                open = None;
                *flow = ControlFlow::Exit;
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                HWND_NOW.store(0, Ordering::SeqCst);
                open = None;
                if QUITTING.load(Ordering::SeqCst) {
                    *flow = ControlFlow::Exit;
                } else {
                    std::thread::spawn(first_close_hint);
                }
            }
            _ => {}
        }
    });
    match fatal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn open_window(
    target: &tao::event_loop::EventLoopWindowTarget<Ev>,
    addr: &str,
    context: &mut WebContext,
    open: &mut Option<(Window, WebView)>,
) -> Result<(), String> {
    let window = WindowBuilder::new()
        .with_title("Miyav")
        .with_inner_size(LogicalSize::new(1200.0, 800.0))
        .build(target)
        .map_err(|e| e.to_string())?;
    if let Some(m) = window.current_monitor() {
        let (ms, ws) = (m.size(), window.outer_size());
        let pos = m.position();
        window.set_outer_position(tao::dpi::PhysicalPosition::new(
            pos.x + (ms.width as i32 - ws.width as i32) / 2,
            pos.y + (ms.height as i32 - ws.height as i32) / 2,
        ));
    }
    let h = window.hwnd() as HWND;
    let webview = WebViewBuilder::with_web_context(context)
        .with_url(addr)
        .with_devtools(std::env::var_os("MIYAV_DEBUG").is_some())
        .build(&window)
        .map_err(|e| format!("WebView2 is required. Install the Microsoft Edge WebView2 Runtime and try again ({e})"))?;
    HWND_NOW.store(h, Ordering::SeqCst);
    style_window(h);
    *open = Some((window, webview));
    Ok(())
}
