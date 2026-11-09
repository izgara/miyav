#![cfg_attr(not(test), windows_subsystem = "windows")]

mod audio;
mod autoclip;
mod ffmpeg;
mod gameicon;
mod games;
mod job;
mod obsrec;
mod recorder;
mod server;
mod settings;
mod storage;
mod tray;
mod update;
mod win;
mod window;
mod winrec;

/// Shows a message box rather than dying silently: there is no console to read.
fn fatal(title: &str, err: &str) -> ! {
    win::show_error(title, err);
    std::process::exit(1);
}

fn main() {
    if !tray::claim_single_instance() {
        return; // the copy already running was asked to show its window
    }
    if let Err(e) = settings::load() {
        fatal("Could not read your settings", &e);
    }
    if !ffmpeg::tools_ready() {
        fatal("ffmpeg.exe is missing", "Miyav cannot record without it. Reinstall the app, or keep ffmpeg.exe beside Miyav.exe.");
    }
    // --tray is how Windows starts the app at sign-in: no window, just the tray icon
    // and the recorder. The first-start wizard still needs the window.
    let in_tray = std::env::args().nth(1).as_deref() == Some("--tray") && settings::get().setup_done;
    let addr = match server::start() {
        Ok(a) => a,
        Err(e) => fatal("Could not start the local service", &e),
    };
    if std::env::var_os("MIYAV_INSTANCE").is_some() {
        let _ = std::fs::write(settings::app_dir().join("addr.txt"), &addr);
    }

    tray::start();
    win::refresh_autostart();
    std::thread::spawn(recorder::start_if_enabled);
    std::thread::spawn(update::watch);
    std::thread::spawn(storage::watch);

    let r = window::run(addr, in_tray);
    tray::remove();
    // Let ffmpeg close its last segment properly rather than be killed with the job
    // object.
    recorder::disable();
    if let Err(e) = r {
        fatal("Could not open the window", &e);
    }
}
