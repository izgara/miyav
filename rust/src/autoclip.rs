//! Automatic clips: the games below report kills through interfaces their makers
//! provide for this - League's Live Client Data API on 127.0.0.1:2999, CS2's Game
//! State Integration, which posts to us once told to by a config file - so nothing
//! reads the game's memory. A kill is saved a few seconds later, with what led up to
//! it; kills close together make one clip.
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::recorder::{self, rec_log};
use crate::{games, settings};

const LEAD: Duration = Duration::from_secs(15); // kept before the first kill
const TAIL: Duration = Duration::from_secs(6); // waited after the last one
const CS2_GSI_PORT: u16 = 38292;

/// Merges kills into clips.
#[derive(Default)]
pub struct Clipper {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Clipper {
    fn kill(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// How many seconds to save if a clip is ready: the action has ended, or has run
    /// as long as the buffer holds.
    fn due(&mut self, now: Instant, buffer_sec: i32) -> Option<i32> {
        let (first, last) = (self.first?, self.last?);
        let span = now.duration_since(first) + LEAD;
        if now.duration_since(last) < TAIL && span < Duration::from_secs(buffer_sec as u64) {
            return None;
        }
        self.first = None;
        self.last = None;
        Some(((span.as_secs_f64() + 0.5) as i32).min(buffer_sec))
    }
}

/// Runs for as long as game recording is on. The setting is read each time round, so
/// flipping it does not start the buffer over.
pub fn run(cancel: Arc<AtomicBool>) {
    let clipper = Arc::new(Mutex::new(Clipper::default()));
    let mut lol = LeaguePoll::default();
    let mut gsi: Option<Cs2Gsi> = None;
    let mut config_gone = false;
    let saving = Arc::new(AtomicBool::new(false));
    while !cancel.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(1));
        let s = settings::get();
        if !s.auto_clip {
            gsi = None;
            if !config_gone {
                remove_cs2_config();
                config_gone = true;
            }
            lol = LeaguePoll::default();
            continue;
        }
        config_gone = false;
        if gsi.is_none() {
            gsi = start_cs2_gsi(clipper.clone());
        }
        let cur = recorder::running();
        match &cur {
            Some(t) if is_league(&t.app) => {
                let n = lol.poll();
                if n > 0 {
                    rec_log(&format!("auto clip: {n} kill(s) in {}", t.app));
                    clipper.lock().unwrap().kill(Instant::now());
                }
            }
            _ => lol = LeaguePoll::default(),
        }
        if cur.is_none() {
            continue;
        }
        let due = clipper.lock().unwrap().due(Instant::now(), s.replay_seconds);
        if let Some(secs) = due {
            if !saving.swap(true, Ordering::SeqCst) {
                let sv = saving.clone();
                std::thread::spawn(move || {
                    match recorder::save_clip(secs, None) {
                        Ok(out) => rec_log(&format!("auto clip saved {out} ({secs} s)")),
                        Err(e) => rec_log(&format!("auto clip failed: {e}")),
                    }
                    sv.store(false, Ordering::SeqCst);
                });
            }
        }
    }
}

fn is_league(app: &str) -> bool {
    app.to_lowercase().contains("league of legends")
}

pub fn is_cs2(app: &str) -> bool {
    let a = app.to_lowercase();
    a == "cs2" || a.contains("counter-strike")
}

// ---- League ----

/// A connector that trusts anything: the API's certificate is Riot's own and the
/// address is this PC.
static LEAGUE: LazyLock<ureq::Agent> = LazyLock::new(|| {
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .expect("Windows TLS");
    ureq::AgentBuilder::new().tls_connector(Arc::new(tls)).timeout(Duration::from_millis(1500)).build()
});

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "PascalCase", default)]
struct LeagueEvent {
    #[serde(rename = "EventID")]
    event_id: i64,
    event_name: String,
    killer_name: String,
}

#[derive(Default)]
struct LeaguePoll {
    seen: bool,
    last: i64,
    me: String,
}

impl LeaguePoll {
    fn poll(&mut self) -> usize {
        let get = |p: &str| -> Option<Value> {
            LEAGUE.get(&format!("https://127.0.0.1:2999/liveclientdata/{p}")).call().ok()?.into_json().ok()
        };
        if self.me.is_empty() {
            let Some(name) = get("activeplayername") else { return 0 };
            self.me = name.as_str().unwrap_or("").to_string();
            if self.me.is_empty() {
                return 0;
            }
        }
        let Some(data) = get("eventdata") else { return 0 };
        let events: Vec<LeagueEvent> = serde_json::from_value(data["Events"].clone()).unwrap_or_default();
        self.update(&events)
    }

    /// Counts the player's kills among the events not seen yet. Events from before
    /// the first look (the app started mid game) are not news.
    fn update(&mut self, events: &[LeagueEvent]) -> usize {
        let me = league_name(&self.me);
        let (mut kills, mut top) = (0, -1);
        for e in events {
            top = top.max(e.event_id);
            if self.seen && e.event_id > self.last && e.event_name == "ChampionKill" && league_name(&e.killer_name) == me {
                kills += 1;
            }
        }
        self.seen = true;
        self.last = top;
        kills
    }
}

/// The active player comes as "Name#TAG"; events may leave the tag off.
fn league_name(s: &str) -> String {
    s.split('#').next().unwrap_or("").trim().to_lowercase()
}

// ---- Counter-Strike 2 ----

#[derive(Default)]
struct KillCount {
    kills: i64,
    known: bool,
}

impl KillCount {
    /// Counts new kills of the player themselves. When the player is dead and
    /// spectating, the state is someone else's and leaves the count alone.
    fn update(&mut self, st: &Value) -> i64 {
        let provider = st["provider"]["steamid"].as_str();
        let player = st["player"]["steamid"].as_str();
        let Some(k) = st["player"]["match_stats"]["kills"].as_i64() else { return 0 };
        if player.is_none() || player != provider {
            return 0;
        }
        let n = if self.known && k > self.kills { k - self.kills } else { 0 };
        self.kills = k;
        self.known = true;
        n
    }
}

/// Stops listening when dropped.
struct Cs2Gsi {
    server: Arc<tiny_http::Server>,
}

impl Drop for Cs2Gsi {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

/// Listens for CS2 and tells it where we are. No CS2, no listener.
fn start_cs2_gsi(clipper: Arc<Mutex<Clipper>>) -> Option<Cs2Gsi> {
    let cfg = cs2_config_path()?;
    let token = match cs2_token() {
        Ok(t) => t,
        Err(e) => {
            rec_log(&format!("auto clip: {e}"));
            return None;
        }
    };
    let body = format!(
        "\"Miyav\"\n{{\n\t\"uri\" \"http://127.0.0.1:{CS2_GSI_PORT}/\"\n\t\"timeout\" \"1.0\"\n\t\"buffer\" \"0.1\"\n\t\"throttle\" \"0.5\"\n\t\"heartbeat\" \"30\"\n\t\"auth\" {{ \"token\" \"{token}\" }}\n\t\"data\" {{ \"provider\" \"1\" \"player_id\" \"1\" \"player_match_stats\" \"1\" }}\n}}\n"
    );
    if let Err(e) = std::fs::write(&cfg, body) {
        rec_log(&format!("auto clip: could not write the CS2 config: {e}"));
        return None;
    }
    let server = match tiny_http::Server::http(("127.0.0.1", CS2_GSI_PORT)) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            rec_log(&format!("auto clip: CS2 port taken: {e}"));
            return None;
        }
    };
    let s2 = server.clone();
    std::thread::spawn(move || {
        let mut count = KillCount::default();
        for mut req in s2.incoming_requests() {
            let mut raw = Vec::new();
            let _ = req.as_reader().take(1 << 20).read_to_end(&mut raw);
            let _ = req.respond(tiny_http::Response::empty(200));
            let Ok(st) = serde_json::from_slice::<Value>(&raw) else { continue };
            if st["auth"]["token"].as_str() != Some(token.as_str()) {
                continue;
            }
            let n = count.update(&st);
            if n > 0 {
                if let Some(t) = recorder::running().filter(|t| is_cs2(&t.app)) {
                    rec_log(&format!("auto clip: {n} kill(s) in {}", t.app));
                    clipper.lock().unwrap().kill(Instant::now());
                }
            }
        }
    });
    Some(Cs2Gsi { server })
}

/// Where CS2 looks for integrations; only an installed CS2 has it.
fn cs2_config_path() -> Option<PathBuf> {
    games::game_library().into_iter().find_map(|g| {
        if g.store != "Steam" || g.name != "Counter-Strike 2" {
            return None;
        }
        let dir = PathBuf::from(g.dir).join("game").join("csgo").join("cfg");
        dir.is_dir().then(|| dir.join("gamestate_integration_miyav_clips.cfg"))
    })
}

fn remove_cs2_config() {
    if let Some(cfg) = cs2_config_path() {
        let _ = std::fs::remove_file(cfg);
    }
}

/// Stays the same between runs: CS2 reads its config once, at launch.
fn cs2_token() -> Result<String, String> {
    let path = settings::app_dir().join("cs2gsi.token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        if t.len() == 32 {
            return Ok(t);
        }
    }
    let mut b = [0u8; 16];
    random(&mut b)?;
    let token: String = b.iter().map(|x| format!("{x:02x}")).collect();
    std::fs::write(&path, &token).map_err(|e| e.to_string())?;
    Ok(token)
}

fn random(buf: &mut [u8]) -> Result<(), String> {
    use windows_sys::Win32::Security::Cryptography::*;
    let r = unsafe {
        BCryptGenRandom(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    if r != 0 {
        return Err("could not make a token".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(id: i64, name: &str, killer: &str) -> LeagueEvent {
        LeagueEvent { event_id: id, event_name: name.into(), killer_name: killer.into() }
    }

    #[test]
    fn league_kills() {
        let mut p = LeaguePoll { me: "Miyav#EUW".into(), ..Default::default() };
        let mut e = vec![ev(0, "GameStart", ""), ev(1, "ChampionKill", "Miyav")];
        assert_eq!(p.update(&e), 0, "kills from before the first look counted");
        e.extend([ev(2, "ChampionKill", "Someone"), ev(3, "ChampionKill", "miyav#EUW"), ev(4, "DragonKill", "Miyav")]);
        assert_eq!(p.update(&e), 1);
        assert_eq!(p.update(&e), 0, "the same events counted twice");
    }

    #[test]
    fn league_event_json() {
        let e: Vec<LeagueEvent> = serde_json::from_value(json!([
            {"EventID": 7, "EventName": "ChampionKill", "EventTime": 1.5, "KillerName": "A", "VictimName": "B"}
        ]))
        .unwrap();
        assert_eq!((e[0].event_id, e[0].event_name.as_str(), e[0].killer_name.as_str()), (7, "ChampionKill", "A"));
    }

    #[test]
    fn cs2_kills() {
        let mut g = KillCount::default();
        for (i, (id, kills, want)) in [
            ("me", 3, 0),    // first look
            ("me", 4, 1),    // a kill
            ("other", 9, 0), // spectating someone
            ("me", 6, 2),    // two more
            ("me", 0, 0),    // a new match
            ("me", 1, 1),
        ]
        .into_iter()
        .enumerate()
        {
            let st = json!({"provider": {"steamid": "me"}, "player": {"steamid": id, "match_stats": {"kills": kills}}});
            assert_eq!(g.update(&st), want, "step {i}");
        }
    }

    #[test]
    fn clipper_merges() {
        let mut c = Clipper::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert_eq!(c.due(t0, 60), None, "a clip with no kill");
        c.kill(t0);
        c.kill(at(4));
        assert_eq!(c.due(at(8), 60), None, "saved while the action was still going");
        assert_eq!(c.due(at(10), 60), Some(25));
        assert_eq!(c.due(at(30), 60), None, "the same kills saved twice");
        // Kills that never stop are saved when the buffer is full, capped to it.
        c.kill(t0);
        c.kill(at(50));
        assert_eq!(c.due(at(50), 60), Some(60));
    }

    #[test]
    fn names() {
        assert!(is_league("League of Legends"));
        assert!(is_cs2("Counter-Strike 2") && is_cs2("cs2"));
        assert!(!is_cs2("VALORANT"));
    }
}
