use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Reported to the UI during long ffmpeg runs.
#[derive(Clone, Default, Serialize)]
pub struct Progress {
    pub phase: String, // "cut" | "encode" | "download"
    pub percent: f64,
    pub attempt: i32, // encode pass (1 = first CRF try)
    pub crf: i32,
}

/// What the UI polls.
#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    pub running: bool,
    pub label: String,
    pub progress: Progress,
    pub error: String,
    pub result: String,
    /// False for work that cannot be stopped - the first-run ffmpeg download -
    /// so the bar only offers Cancel when it means something.
    pub cancellable: bool,
}

/// Ctx is what long work watches to know when to give up. ffmpeg is killed and
/// downloads stop when it trips, so cancelling really does stop the work rather
/// than just stop watching it.
#[derive(Clone)]
pub struct Ctx {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl Ctx {
    pub fn with_timeout(d: Duration) -> Ctx {
        Ctx { cancelled: Arc::new(AtomicBool::new(false)), deadline: Instant::now() + d }
    }
    pub fn stop(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    /// The user pressed Cancel, as distinct from a genuine failure. The error
    /// itself is no help: a killed ffmpeg reports nothing useful.
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    pub fn done(&self) -> bool {
        self.cancelled() || Instant::now() > self.deadline
    }
}

struct State {
    snap: Snapshot,
    ctx: Option<Ctx>,
    // Rises with every job, so a Cancel pressed just as one finishes cannot
    // reach into whatever started next.
    gen: u64,
}

static JOB: LazyLock<Mutex<State>> =
    LazyLock::new(|| Mutex::new(State { snap: Snapshot::default(), ctx: None, gen: 0 }));

pub fn set(f: impl FnOnce(&mut Snapshot)) {
    f(&mut JOB.lock().unwrap().snap);
}

pub fn snapshot() -> Snapshot {
    JOB.lock().unwrap().snap.clone()
}

/// Ends the job when dropped.
pub struct Done {
    gen: u64,
}

impl Drop for Done {
    fn drop(&mut self) {
        let mut j = JOB.lock().unwrap();
        if j.gen == self.gen {
            j.snap.running = false;
            j.snap.cancellable = false;
            j.ctx = None;
        }
    }
}

/// Starts a cancellable job and hands back what the work must run under.
pub fn begin(label: &str, timeout: Duration) -> (Ctx, Done) {
    let ctx = Ctx::with_timeout(timeout);
    let mut j = JOB.lock().unwrap();
    j.gen += 1;
    j.ctx = Some(ctx.clone());
    j.snap = Snapshot { running: true, label: label.into(), cancellable: true, ..Default::default() };
    let gen = j.gen;
    (ctx, Done { gen })
}

/// Cancels the running job, if there is one. The label changes under the same
/// lock so the bar acknowledges the press straight away, rather than looking
/// ignored for however long ffmpeg takes to die.
pub fn stop() -> bool {
    let mut j = JOB.lock().unwrap();
    let Some(ctx) = j.ctx.clone() else { return false };
    j.snap.label = "Cancelling…".into();
    j.snap.cancellable = false;
    ctx.stop();
    true
}

pub fn report(p: Progress) {
    set(|s| s.progress = p);
}
