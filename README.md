# Miyav

Miyav is a lightweight game clip recorder for Windows. It is an alternative to NVIDIA
ShadowPlay and to the replay buffer in OBS Studio.

It keeps the last minute of your game ready. When something good happens, you press
a key and that minute is saved as a video file. You can then trim it without losing
quality, and share the file however you like.

There is no account, no sign-in and no cloud. Your clips stay on your PC. It is free
and open source.

## Install

Download from the [releases page](https://github.com/izgara/miyav/releases/latest):

- `Miyav-x.y.z-setup.exe` (24 MB) installs for your Windows user, without an admin
  prompt, and updates itself.
- `Miyav-x.y.z-portable.zip` (36 MB) runs from any folder. It tells you when there is
  a new version, but you update it by hand.

The releases are not code-signed, so Windows SmartScreen may warn you the first time.
Choose "More info", then "Run anyway".

## Use

1. Open Miyav. A short setup scans your PC (graphics card, encoders, screen, memory, disk
   and installed games) and picks the recording settings that suit it. It suggests
   folders for your clips, which you can change, and a key for saving clips.
2. Play. Press **Alt+C** to save the last minute. Two rising beeps mean the clip is
   saved; a low buzz means it failed.
3. Open the clip in Miyav to trim it, or find it in `Videos\Miyav`.

You can run the same scan again later from the Recording tab.

Closing the window keeps Miyav running in the tray, by the clock. Right-click the tray
icon to quit.

## Features

**Recording**

- Replay buffer of 30 seconds to 5 minutes, kept on disk or in memory.
- Games only (follows the game you are playing) or the whole screen.
- 30, 60 or 120 fps.
- Encodes on NVIDIA (NVENC), AMD (AMF) or Intel hardware, or on the CPU with x264.
  It picks the best one that works.
- H.264 by default. AV1 is optional, on newer NVIDIA cards.
- Clips have a mix of all audio as the first track, plus separate tracks for the game,
  PC audio and your microphone. The microphone is off until you turn it on.
- Optionally saves a clip of each of your kills in League of Legends and CS2.
- Keeps saved clips under a disk limit, 50 GB by default, deleting the oldest first.
  Only clips Miyav recorded count, and you can turn the limit off.

**Finding games**

- Knows what is installed through Steam, Epic, Riot, GOG, Ubisoft and the Xbox app.
- Recognises about 11,000 more games by their exe.
- Records other games when they are fullscreen, and leaves well-known programs like
  browsers alone.
- You can add any other program by hand.

**Trimming**

- The editor cuts without re-encoding, so it takes about a second and the picture is
  identical. The cut starts on a keyframe, so it can land slightly before the point
  you chose.
- You choose which audio track comes first in the clip. The others are kept.
- It also lists recordings from OBS or ShadowPlay if you point it at their folder.

**Shrinking**

- The Shrink button re-encodes a clip to a smaller file, keeping its resolution and
  frame rate, for sending somewhere with a size limit. It uses your graphics card when
  it can.
- This one does lose some quality. The original is kept, and if the result would not be
  at least 5% smaller, it is thrown away.

## ShadowPlay and OBS

ShadowPlay only works on NVIDIA cards and is closed source. Miyav also works on AMD and
Intel, or with no GPU encoder at all.

OBS Studio is a full recording and streaming studio, and its replay buffer has to be set
up by hand. Miyav does the one job of clipping and sets itself up from a scan of your PC.
It records games with OBS's own engine, so the capture itself is the same. If you stream,
or need scenes and overlays, OBS is the right tool.

## Memory use

Measured on a 2560x1440 screen at 60 fps with an NVIDIA card, whole screen mode:

| State | Miyav.exe | ffmpeg | Window (WebView2) |
| --- | --- | --- | --- |
| In the tray, recording off | 13 MB | none | none |
| In the tray, recording | 20 MB | 118 MB | none |
| Window open | about 29 MB | as above | about 340 MB |

The window is the expensive part. Closing it really frees that memory, and recording
carries on. These figures do not cover recording a game through the OBS engine, which
runs as its own process instead of ffmpeg.

## How games are captured

In "Games only" mode, Miyav records a game with OBS Studio's game capture, using OBS's
own signed files, unmodified. Clips then show only the game: no notifications and
nothing from alt-tabbing. "Whole screen" mode records the monitor instead.

If a game cannot be hooked within 20 seconds, Miyav falls back to capturing its window
or screen with ffmpeg.

CS2's Trusted Mode blocks game capture unless the game is started with
`-allow_third_party_software`. Miyav does not set that itself: it tells you about it and
uses the fallback.

The automatic kill clips use only what the games provide for this: League's Live Client
Data API, and CS2's Game State Integration, for which Miyav writes a config file into
CS2's `cfg` folder and removes it when you turn the feature off. Miyav never reads a
game's memory.

## Privacy

- Nothing is uploaded. Miyav has no servers.
- There is no telemetry and no analytics.
- The only thing it fetches from the internet is the latest version number from GitHub,
  shortly after start and every six hours. An update is downloaded only when you press
  the update button.
- The app's interface is a local page served on `127.0.0.1`, which refuses requests from
  other websites.

## Where things are

| What | Where |
| --- | --- |
| Saved clips | `Videos\Miyav`, in a folder per game |
| Trimmed and shrunk clips | `Videos\Miyav Clips` |
| Settings and thumbnails | `%APPDATA%\Miyav` |
| Replay buffer | `%LOCALAPPDATA%\Miyav\replay` |
| The installed app | `%LOCALAPPDATA%\Programs\Miyav` |

You can change both video folders in Settings. Uninstalling leaves your videos and
settings in place. Delete those folders by hand if you want them gone.

## Requirements and limits

- 64-bit Windows 10 or 11.
- The Microsoft Edge WebView2 runtime, which comes with Windows 11 and current Windows 10.
- NVENC needs NVIDIA driver 531 or newer.
- Windows only. There is no Linux or macOS version.
- Shrinking on AMD and Intel hardware has not been tested on real cards yet.
- Most testing so far has been on NVIDIA.

## How it works

Miyav is a small Rust program with a plain HTML, CSS and JavaScript interface. The Rust
side runs a local HTTP server on a random loopback port, and the interface is a WebView2
window pointed at it.

Recording runs in a separate process, and one of two engines does it:

- **OBS engine, for games.** `miyav-obs.exe` is a small program that drives OBS's libobs
  through its exports and talks to the app with JSON over stdin and stdout. It keeps the
  buffer as 5-second Matroska files, or in memory with OBS's own replay buffer.
- **ffmpeg, for everything else.** ffmpeg records into 2-second MPEG-TS segments. A
  fullscreen game is captured through its monitor with `ddagrab` while it is in front and
  through its window with `gfxcapture` while you are tabbed out, so clips show the game
  rather than your desktop. Audio is captured by the app with WASAPI and passed to ffmpeg
  over named pipes.

Saving a clip joins the newest segments into one MP4 with a stream copy, so nothing is
re-encoded.

The bundled ffmpeg is built by this repository rather than downloaded. It pins the NVENC
headers so older NVIDIA drivers keep working, leaves out codecs the app never uses, and
carries a small patch so `ddagrab` survives a game taking over the screen.

### Source layout

| Path | What it is |
| --- | --- |
| `rust/src/main.rs` | Start-up |
| `rust/src/server.rs` | The local HTTP routes, trimming and shrinking jobs, the PC scan |
| `rust/src/recorder.rs` | The replay buffer, ffmpeg engine, saving clips |
| `rust/src/obsrec.rs` | Runs and talks to `miyav-obs.exe` |
| `rust/src/audio.rs` | WASAPI capture |
| `rust/src/games.rs` | What counts as a game |
| `rust/src/winrec.rs` | The hotkey, finding the game window, beeps |
| `rust/src/autoclip.rs` | Automatic kill clips |
| `rust/src/ffmpeg.rs` | Running ffmpeg: trimming, shrinking, reading file details |
| `rust/src/storage.rs` | The disk limit |
| `rust/src/update.rs` | Update checks against GitHub releases |
| `rust/src/settings.rs` | Settings and their defaults |
| `rust/src/tray.rs`, `window.rs`, `win.rs` | Tray icon, window, Win32 helpers |
| `rust/src/job.rs` | Progress and cancelling of long jobs |
| `rust/miyav-obs/` | The OBS engine helper |
| `rust/data/known-games.tsv.gz` | The list of known games |
| `ui/` | The interface, embedded in the exe |
| `ffmpeg/` | Scripts that build `ffmpeg.exe`, and the `ddagrab` patch |
| `obs/fetch.sh` | Downloads the official OBS release and picks out the files needed |
| `tools/` | Scripts that regenerate the icon and the list of games |
| `miyav.nsi`, `build.sh` | The installer and the build |

## Build from source

The build runs on Linux and cross-compiles for Windows. You need Docker, `makensis`, `zip`,
`unzip` and `curl`.

```sh
./build.sh
```

The first build also compiles ffmpeg, which takes about ten minutes, and downloads OBS
Studio. Everything ends up in `dist/`. Set `DOCKER="sudo docker"` if your user cannot run
Docker directly.

OBS's files must stay exactly as released: games accept the hook because it is OBS's signed
DLL, so never rebuild or patch them. To move to a newer OBS, change `OBS_VERSION` and
`OBS_SHA256` in `obs/fetch.sh`, and check `rust/miyav-obs` against the new libobs headers,
because its structs mirror them.

To release a new version, change the number in `VERSION`, build, and attach the two files
in `dist/` to a GitHub release tagged `vx.y.z`. Installed copies find it from there.

### Tests

The tests cross-compile on Linux and run on Windows:

```sh
cd rust
cargo test --release --no-run --target x86_64-pc-windows-gnu
```

Copy the test exe from the output to a Windows PC, with `WebView2Loader.dll` beside it, and
run it. To include the test that records through the real OBS engine, set `MIYAV_OBS_DIR` to
a folder holding the OBS files and `miyav-obs.exe`, and `MIYAV_FFMPEG` to `ffmpeg.exe`.

To run a test copy of the app beside an installed one, set `MIYAV_INSTANCE`, `APPDATA` and
`LOCALAPPDATA`. It then writes its address to `addr.txt` in its settings folder.

## License

Miyav is licensed under the [GNU General Public License, version 3 or later](LICENSE).

Releases include OBS Studio files, ffmpeg and other third-party parts under their own
licences. See [NOTICE.md](NOTICE.md). Miyav is not affiliated with OBS Studio, NVIDIA, or
any game or store it mentions.
