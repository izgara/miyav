//! What counts as a game. The launchers already know what is installed and
//! where, so a program running from inside a game's install folder is that game
//! - windowed or not - and gets the store's name for it rather than its exe
//! name. Anything else only counts if it takes over the whole monitor the way a
//! fullscreen game does, and never if it is a well-known non-game.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::settings;
use crate::win::{self, HKCU, HKLM};

#[derive(Clone, Default, Serialize)]
pub struct GameEntry {
    pub name: String,
    pub dir: String,
    pub store: String,
}

/// Marks what the player added to the list themselves.
pub const STORE_ADDED: &str = "Added by you";

/// A program the player asked to have recorded. It is matched by exe name alone:
/// Discord, for one, moves to a new folder with every update.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CustomApp {
    pub name: String,
    pub exe: String,
}

/// Exes (lowercase, no .exe) that cover the screen often enough to be mistaken
/// for games: browsers, chat, players, launchers, remote desktops.
static NOT_GAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    "chrome msedge firefox opera opera_gx brave vivaldi thorium arc zen librewolf waterfox msedgewebview2
     discord discordptb discordcanary vesktop serein telegram ayugram kotatogram 64gram luxurygram unigram whatsapp signal slack teams ms-teams zoom skype element
     spotify spotifast zapfast yapfast vlcmpc-hc64 mpc-hc mpc-be64 potplayermini64 potplayer mpv wmplayer microsoft.media.player applemusic itunes
     steam steamwebhelper epicgameslauncher epicwebhelper riotclientservices riotclientux riotclientuxrender leagueclient leagueclientux leagueclientuxrender
     galaxyclient upc ubisoftconnect eadesktop eabackgroundservice battle.net agent xboxpcapp gamebar
     obs64 obs32 medal sharex flameshot greenshot lightshot snippingtool screenclippinghost streamlabs nvidia amdrsserv radeonsoftware
     mstsc msrdc vmware vmware-vmx virtualboxvm parsecd moonlight anydesk teamviewer rustdesk
     code devenv idea64 rider64 pycharm64 winword excel powerpnt onenote outlook olk acrobat acrord32 notepad notepad++ obsidian notion
     explorer applicationframehost systemsettings searchhost startmenuexperiencehost shellexperiencehost lockapp textinputhost
     miyav powershell windowsterminal cmd conhost taskmgr"
        .split_whitespace()
        .collect()
});

/// Library folders that hold things which are not games.
static NOT_GAME_NAMES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)redistributable|steamworks|proton|steamvr|steam linux runtime|^riot client$|^pengu$").unwrap()
});

/// The Riot folders that are games. Others turn up: the Riot Client, tools like
/// Pengu Loader, and VALORANT's "live" if the root is misread.
const RIOT_TITLES: [&str; 5] = ["league of legends", "valorant", "teamfight tactics", "legends of runeterra", "2xko"];

pub fn exe_name(exe: &str) -> String {
    Path::new(exe).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

fn clean(p: &str) -> String {
    p.trim_end_matches(['\\', '/']).replace('/', "\\")
}

struct Library {
    games: Vec<GameEntry>,
    loaded: Option<Instant>,
}

static LIBRARY: LazyLock<Mutex<Library>> = LazyLock::new(|| Mutex::new(Library { games: Vec::new(), loaded: None }));

/// Re-read every few minutes, so a game installed while the app is open is
/// picked up without a restart.
pub fn game_library() -> Vec<GameEntry> {
    let mut lib = LIBRARY.lock().unwrap();
    if let Some(t) = lib.loaded {
        if t.elapsed() < Duration::from_secs(300) {
            return lib.games.clone();
        }
    }
    let mut all = Vec::new();
    for mut g in [steam_games(), epic_games(), riot_games(), xbox_games(), registry_games()].concat() {
        if !g.dir.is_empty() && !g.name.is_empty() && !NOT_GAME_NAMES.is_match(&g.name) {
            g.dir = clean(&g.dir);
            all.push(g);
        }
    }
    // Longest folder first, so a game nested inside another's folder matches the
    // inner one.
    all.sort_by_key(|g| std::cmp::Reverse(g.dir.len()));
    lib.games = all.clone();
    lib.loaded = Some(Instant::now());
    all
}

/// The installed game an exe belongs to: added by the player, known to the store
/// that installed it, else the exe itself if it is a known game.
pub fn game_for_exe(exe: &str) -> Option<GameEntry> {
    let parent = || Path::new(exe).parent().map(|p| p.display().to_string()).unwrap_or_default();
    if let Some(a) = custom_app(exe) {
        return Some(GameEntry { name: a.name, dir: parent(), store: STORE_ADDED.into() });
    }
    let lower = exe.to_lowercase();
    for g in game_library() {
        if lower.starts_with(&format!("{}\\", g.dir.to_lowercase())) {
            return Some(g);
        }
    }
    known_game_for_exe(exe).map(|name| GameEntry { name, dir: parent(), store: "Known game".into() })
}

pub fn custom_app(exe: &str) -> Option<CustomApp> {
    let base = Path::new(exe).file_name()?.to_string_lossy().to_string();
    settings::get().custom_apps.into_iter().find(|a| a.exe.eq_ignore_ascii_case(&base))
}

/// An added program that is not a game is recorded through its window. OBS's
/// hook would fail on it after 20s at best, and has no business inside a chat
/// client or a browser.
pub fn custom_no_hook(exe: &str) -> bool {
    is_not_game(exe) || per_user_app(exe)
}

pub fn add_custom_app(exe: &str) -> Result<(), String> {
    let base = Path::new(exe).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    if exe_name(&base).is_empty() {
        return Err("no program given".into());
    }
    settings::update(|s| {
        if !s.custom_apps.iter().any(|a| a.exe.eq_ignore_ascii_case(&base)) {
            s.custom_apps.push(CustomApp { name: exe_name(&base), exe: base.clone() });
        }
    });
    settings::save()
}

/// Takes it off the list, and stops recording it if it is what is being recorded.
pub fn remove_custom_app(exe: &str) -> Result<(), String> {
    let mut name = String::new();
    settings::update(|s| {
        s.custom_apps.retain(|a| {
            let found = a.exe.eq_ignore_ascii_case(exe);
            if found {
                name = a.name.clone();
            }
            !found
        })
    });
    if let Some(cur) = crate::recorder::running() {
        if cur.custom && cur.app == name {
            crate::recorder::stop();
        }
    }
    settings::save()
}

pub fn is_not_game(exe: &str) -> bool {
    NOT_GAMES.contains(exe_name(exe).to_lowercase().as_str())
}

/// A window class that belongs to a desktop app framework rather than a game
/// engine: Qt (Telegram forks, Flameshot's capture overlay), Chromium/Electron/
/// CEF, WinForms, WPF, WinUI, Tk, wxWidgets, GTK, and winit's default class
/// (egui apps). Their media viewers and screenshot overlays cover the monitor
/// without a title bar just as a game does. Only consulted for programs no store
/// or the known-games list claims; engines use their own classes (UnityWndClass,
/// UnrealWindow, SDL_app, GLFW30, ...).
pub fn app_window_class(class: &str) -> bool {
    (class.starts_with("Qt") && class.contains("QWindow"))
        || ["Chrome_WidgetWin_", "WindowsForms10.", "HwndWrapper[", "WinUIDesktopWin32WindowClass", "TkTopLevel", "wxWindow", "gdkSurface", "gdkWindow"]
            .iter()
            .any(|p| class.starts_with(p))
        || class == "Window Class"
}

/// An exe installed for the current user only, under %LOCALAPPDATA%\Programs:
/// where Electron and Tauri apps (chat clients and the like) go, and games almost
/// never do. Such an exe only counts as a game when a store or the known-games
/// list says so.
pub fn per_user_app(exe: &str) -> bool {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else { return false };
    let dir = format!("{}\\", PathBuf::from(local).join("Programs").display()).to_lowercase();
    exe.to_lowercase().starts_with(&dir)
}

// ---- Steam ----

static VDF_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""path"\s+"([^"]+)""#).unwrap());
static VDF_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""name"\s+"([^"]+)""#).unwrap());
static VDF_INSTALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""installdir"\s+"([^"]+)""#).unwrap());

fn steam_root() -> String {
    let p = win::reg_string(HKCU, r"Software\Valve\Steam", "SteamPath");
    if !p.is_empty() {
        return clean(&p);
    }
    win::reg_string(HKLM, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath")
}

fn steam_games() -> Vec<GameEntry> {
    let root = steam_root();
    if root.is_empty() {
        return Vec::new();
    }
    let mut libs = vec![root.clone()];
    if let Ok(b) = std::fs::read(Path::new(&root).join("steamapps").join("libraryfolders.vdf")) {
        let text = String::from_utf8_lossy(&b);
        libs.extend(VDF_PATH.captures_iter(&text).map(|m| m[1].replace(r"\\", "\\")));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for lib in libs {
        if !seen.insert(clean(&lib).to_lowercase()) {
            continue;
        }
        let apps = Path::new(&lib).join("steamapps");
        let Ok(rd) = std::fs::read_dir(&apps) else { continue };
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().to_string();
            if !(f.starts_with("appmanifest_") && f.ends_with(".acf")) {
                continue;
            }
            let Ok(b) = std::fs::read(e.path()) else { continue };
            let text = String::from_utf8_lossy(&b);
            if let (Some(n), Some(d)) = (VDF_NAME.captures(&text), VDF_INSTALL.captures(&text)) {
                out.push(GameEntry {
                    name: n[1].to_string(),
                    dir: apps.join("common").join(&d[1]).display().to_string(),
                    store: "Steam".into(),
                });
            }
        }
    }
    out
}

// ---- Epic ----

fn program_data() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| r"C:\ProgramData".into())
}

fn epic_games() -> Vec<GameEntry> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Item {
        display_name: String,
        install_location: String,
    }
    let dir = program_data().join("Epic").join("EpicGamesLauncher").join("Data").join("Manifests");
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "item"))
        .filter_map(|e| serde_json::from_slice::<Item>(&std::fs::read(e.path()).ok()?).ok())
        .map(|m| GameEntry { name: m.display_name, dir: m.install_location, store: "Epic".into() })
        .collect()
}

// ---- Riot ----

fn riot_games() -> Vec<GameEntry> {
    #[derive(Deserialize)]
    struct Installs {
        #[serde(default)]
        associated_client: HashMap<String, String>,
    }
    let Ok(b) = std::fs::read(program_data().join("Riot Games").join("RiotClientInstalls.json")) else {
        return Vec::new();
    };
    let Ok(m) = serde_json::from_slice::<Installs>(&b) else { return Vec::new() };
    // The install list only names some games (TFT is often missing), but they
    // all sit side by side in the Riot Games folder.
    let mut roots: HashMap<String, PathBuf> = HashMap::new();
    let mut add = |p: PathBuf| {
        roots.insert(p.display().to_string().to_lowercase(), p);
    };
    add(PathBuf::from(format!("{}\\", std::env::var("SystemDrive").unwrap_or_default())).join("Riot Games"));
    for dir in m.associated_client.keys() {
        // League is at <root>\League of Legends, VALORANT at <root>\VALORANT\live.
        let mut d = PathBuf::from(clean(dir));
        if d.file_name().is_some_and(|n| n.eq_ignore_ascii_case("live")) {
            d.pop();
        }
        if let Some(root) = d.parent() {
            add(root.to_path_buf());
        }
    }
    let mut out = Vec::new();
    for root in roots.values() {
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && RIOT_TITLES.contains(&name.to_lowercase().as_str()) {
                out.push(GameEntry { name, dir: e.path().display().to_string(), store: "Riot".into() });
            }
        }
    }
    out
}

// ---- Xbox app (PC Game Pass) ----

fn xbox_games() -> Vec<GameEntry> {
    let mut out = Vec::new();
    for d in 'C'..='Z' {
        let Ok(rd) = std::fs::read_dir(format!(r"{d}:\XboxGames")) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && name != "GameSave" {
                out.push(GameEntry { name, dir: e.path().display().to_string(), store: "Xbox".into() });
            }
        }
    }
    out
}

/// The launchers that only record installs in the registry: GOG Galaxy and
/// Ubisoft Connect.
fn registry_games() -> Vec<GameEntry> {
    let mut out = Vec::new();
    const GOG: &str = r"SOFTWARE\WOW6432Node\GOG.com\Games";
    for id in win::reg_subkeys(HKLM, GOG) {
        let k = format!(r"{GOG}\{id}");
        out.push(GameEntry {
            name: win::reg_string(HKLM, &k, "gameName"),
            dir: win::reg_string(HKLM, &k, "path"),
            store: "GOG".into(),
        });
    }
    const UBI: &str = r"SOFTWARE\WOW6432Node\Ubisoft\Launcher\Installs";
    for id in win::reg_subkeys(HKLM, UBI) {
        let dir = clean(&win::reg_string(HKLM, &format!(r"{UBI}\{id}"), "InstallDir"));
        if !dir.is_empty() {
            let name = Path::new(&dir).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            out.push(GameEntry { name, dir, store: "Ubisoft".into() });
        }
    }
    out
}

// ---- known games ----

/// known-games.tsv.gz, generated by tools/update_games.py: "pattern<TAB>name" lines.
static KNOWN_GZ: &[u8] = include_bytes!("../data/known-games.tsv.gz");

static KNOWN: LazyLock<HashMap<String, Vec<(String, String)>>> = LazyLock::new(|| {
    let mut by_base: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut text = String::new();
    if flate2::read::GzDecoder::new(KNOWN_GZ).read_to_string(&mut text).is_ok() {
        for line in std::io::BufRead::lines(text.as_bytes()).map_while(Result::ok) {
            if let Some((p, name)) = line.split_once('\t') {
                let base = p.rsplit('/').next().unwrap_or(p).to_string();
                by_base.entry(base).or_default().push((p.to_string(), name.to_string()));
            }
        }
    }
    by_base
});

/// The game an exe is, by the end of its path; the longest pattern wins.
pub fn known_game_for_exe(exe: &str) -> Option<String> {
    let p = clean(exe).to_lowercase().replace('\\', "/");
    let base = p.rsplit('/').next().unwrap_or(&p);
    KNOWN
        .get(base)?
        .iter()
        .filter(|(pat, _)| p == *pat || p.ends_with(&format!("/{pat}")))
        .max_by_key(|(pat, _)| pat.len())
        .map(|(_, name)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_games() {
        for (exe, want) in [
            // A plain exe name matches wherever the game is installed.
            (r"D:\Games\Overwatch\_retail_\Overwatch.exe", Some("Overwatch")),
            // A pattern with a folder needs that folder right above the exe.
            (r"E:\itch\Terra Invicta\TerraInvicta.exe", Some("Terra Invicta")),
            (r"E:\itch\Other\TerraInvicta.exe", None),
            (r"C:\Some\action.exe", None),
            (r"C:\Program Files\Microsoft Office\EXCEL.EXE", None),
            // Generic names and crash handlers never count on their own.
            (r"C:\Some\Game.exe", None),
            (r"C:\Games\Foo\UnityCrashHandler64.exe", None),
            (r"C:\Windows\notepad.exe", None),
        ] {
            assert_eq!(known_game_for_exe(exe).as_deref(), want, "{exe}");
        }
    }

    #[test]
    fn custom_apps_and_riot() {
        settings::update(|s| s.custom_apps = vec![CustomApp { name: "Discord".into(), exe: "Discord.exe".into() }]);
        // Matched wherever it is, as Discord's folder changes with each update.
        let exe = r"C:\Users\x\AppData\Local\Discord\app-1.0.9203\discord.exe";
        let g = game_for_exe(exe).unwrap();
        assert_eq!((g.name.as_str(), g.store.as_str()), ("Discord", STORE_ADDED));
        assert!(custom_no_hook(exe));
        assert!(!custom_no_hook(r"D:\Games\Some Game\game.exe"));
        assert!(custom_app(r"C:\Other\chrome.exe").is_none());
        settings::update(|s| s.custom_apps.clear());
        assert!(is_not_game(r"C:\x\Serein.exe"));
        assert!(is_not_game(r"C:\AMD\Portable\AyuGram\LuxuryGram.exe"));
        assert!(is_not_game(r"C:\Program Files\Flameshot\bin\flameshot.exe"));
    }

    #[test]
    fn app_window_classes() {
        for c in ["Qt662QWindowIcon", "Qt5153QWindowToolSaveBits", "Chrome_WidgetWin_1", "WindowsForms10.Window.8.app.0.1", "HwndWrapper[App;;1]", "Window Class"] {
            assert!(app_window_class(c), "{c}");
        }
        for c in ["UnityWndClass", "UnrealWindow", "SDL_app", "GLFW30", "Valve001", "RiotWindowClass", "LWJGL"] {
            assert!(!app_window_class(c), "{c}");
        }
    }

    // Riot's install list names VALORANT as <root>\VALORANT\live, which once put
    // a game called "live" on the list, along with whatever else was in the folder.
    #[test]
    fn riot_games_only_games() {
        let tmp = std::env::temp_dir().join(format!("miyav-riot-{}", std::process::id()));
        let root = tmp.join("Riot Games");
        for d in ["League of Legends", r"VALORANT\live", "Teamfight Tactics", "Riot Client", "pengu-loader"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let data = tmp.join("data");
        std::fs::create_dir_all(data.join("Riot Games")).unwrap();
        let slash = root.display().to_string().replace('\\', "/");
        let list = format!(r#"{{"associated_client":{{"{slash}/League of Legends/":"x","{slash}/VALORANT/live/":"x"}}}}"#);
        std::fs::write(data.join("Riot Games").join("RiotClientInstalls.json"), list).unwrap();
        std::env::set_var("ProgramData", &data);
        std::env::set_var("SystemDrive", tmp.join("nothing"));
        let mut names: Vec<String> = riot_games().into_iter().map(|g| g.name).collect();
        names.sort();
        assert_eq!(names, ["League of Legends", "Teamfight Tactics", "VALORANT"]);
        let _ = std::fs::remove_dir_all(tmp);
    }
}
