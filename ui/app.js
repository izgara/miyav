const $ = (s) => document.querySelector(s);

const api = async (path, opts) => {
  const res = await fetch(path, opts);
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    const err = new Error(body.error || 'request failed (' + res.status + ')');
    // 499 is this app's own code for "the user pressed Cancel". Stopping a job
    // on purpose is not a failure and must not be dressed up as one.
    err.cancelled = res.status === 499;
    throw err;
  }
  return body;
};
const post = (path, obj) =>
  api(path, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(obj) });

const fmtBytes = (n) => {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB'];
  const i = Math.min(u.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  return (n / Math.pow(1024, i)).toFixed(i ? 1 : 0) + ' ' + u[i];
};
const fmtDate = (v) => {
  const d = typeof v === 'number' ? new Date(v * 1000) : new Date(v);
  return isNaN(d) ? '' : d.toLocaleDateString() + ', ' + d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
};
const fmtTime = (s) => {
  if (!isFinite(s) || s < 0) s = 0;
  const m = Math.floor(s / 60);
  return m + ':' + (s - m * 60).toFixed(1).padStart(4, '0');
};

const state = { settings: null, sel: null, inPt: 0, outPt: 0, groups: [] };

function toast(msg, kind) {
  const t = $('#toast');
  t.textContent = msg;
  t.className = 'toast ' + (kind || '');
  clearTimeout(t._t);
  t._t = setTimeout(() => t.classList.add('hidden'), 5000);
}
// ---------- in-app dialog ----------
// Replaces window.confirm, which renders as an unstyled "127.0.0.1 says" box
// and only offers OK / Cancel.
function ask(opts) {
  return new Promise((resolve) => {
    const back = document.createElement('div');
    back.className = 'modal dialog-back';

    const card = document.createElement('div');
    card.className = 'dialog';

    const h = document.createElement('div');
    h.className = 'dialog-title';
    h.textContent = opts.title || '';
    card.appendChild(h);

    if (opts.body) {
      const b = document.createElement('div');
      b.className = 'dialog-body';
      b.textContent = opts.body;
      card.appendChild(b);
    }
    if (opts.detail) {
      const d = document.createElement('div');
      d.className = 'dialog-detail';
      d.textContent = opts.detail;
      card.appendChild(d);
    }

    const row = document.createElement('div');
    row.className = 'dialog-actions';

    const cancel = document.createElement('button');
    cancel.className = 'btn ghost';
    cancel.textContent = opts.cancelLabel || 'Cancel';

    const ok = document.createElement('button');
    ok.className = 'btn ' + (opts.danger ? 'danger-solid' : 'primary');
    ok.textContent = opts.confirmLabel || 'OK';

    row.appendChild(cancel);
    row.appendChild(ok);
    card.appendChild(row);
    back.appendChild(card);
    document.body.appendChild(back);

    const done = (v) => {
      back.classList.add('closing');
      setTimeout(() => back.remove(), 130);
      document.removeEventListener('keydown', onKey, true);
      resolve(v);
    };
    const onKey = (e) => {
      if (e.key === 'Escape') { e.stopPropagation(); done(false); }
      if (e.key === 'Enter') { e.stopPropagation(); done(true); }
    };

    cancel.addEventListener('click', () => done(false));
    ok.addEventListener('click', () => done(true));
    back.addEventListener('click', (e) => { if (e.target === back) done(false); });
    document.addEventListener('keydown', onKey, true);
    setTimeout(() => ok.focus(), 40);
  });
}

// stagger card entrance so a grid does not pop in all at once
function stagger(container) {
  [...container.children].forEach((el, i) => { el.style.animationDelay = Math.min(i * 22, 320) + 'ms'; });
}

// ---------- boot ----------
async function boot() {
  state.settings = await api('/api/settings');
  if (!state.settings.setupDone) {
    openWizard();
    return;
  }
  $('#app').classList.remove('hidden');
  fillSettings();
  recFill();
  loadRaw();
  pollJob();
  recPoll();
  checkUpdate(false);
}

// ---------- folder picker ----------
// One delegated handler serves every path field.
document.addEventListener('click', async (ev) => {
  const btn = ev.target.closest('.pickbtn');
  if (!btn) return;
  const input = document.getElementById(btn.dataset.pick);
  if (!input) return;

  const label = btn.textContent;
  btn.disabled = true;
  btn.textContent = 'Opening...';
  try {
    const r = await post('/api/pick-folder', {
      title: btn.dataset.title || 'Choose a folder',
      initial: input.value.trim(),
    });
    if (!r.cancelled && r.path) input.value = r.path;
  } catch (e) {
    toast(e.message, 'err');
  } finally {
    btn.disabled = false;
    btn.textContent = label;
  }
});

// ---------- setup wizard ----------
// The first start: scans the PC by itself, the same scan as the Recording tab's, then
// offers the folders and the key. What the scan recommends is what Finish applies, and
// nothing records until then.
const wiz = { rec: null, hotkey: '' };

function wizStep(n) {
  document.querySelectorAll('.wiz-step').forEach((el) => {
    el.classList.toggle('hidden', Number(el.dataset.step) !== n);
  });
}

function openWizard() {
  const s = state.settings;
  $('#wizRaw').value = s.rawFolder;
  $('#wizClips').value = s.clipsFolder;
  $('#wizHotkey').value = s.hotkey;
  $('#wizMic').checked = !!s.recordMic;
  $('#wizKept').textContent = keptText(s.replaySeconds);
  $('#wizard').classList.remove('hidden');
  wizScan();
}

async function wizScan() {
  const msg = $('#wizScanMsg');
  try {
    const s = await api('/api/rec/scan');
    wiz.rec = s.recommended;
    const names = (s.games || []).map((g) => g.name);
    const shown = names.slice(0, 5).join(', ') + (names.length > 5 ? ' and ' + (names.length - 5) + ' more' : '');
    fillScan($('#wizScan'), scanRows(s).concat([
      ['Games', names.length ? shown : 'none found yet; fullscreen games are recorded anyway'],
      ['Will record at', describeRec(s.recommended)],
    ]));
    $('#wizScan').classList.remove('hidden');
    $('#wizKept').textContent = keptText(s.recommended.replaySeconds);
    msg.textContent = 'These settings suit your PC. You can change them later on the Recording tab.';
  } catch (e) {
    msg.textContent = 'Could not check your PC (' + e.message + '). The usual settings will do.';
  }
  $('#wizScanTitle').textContent = 'Your PC';
  $('#wizNext').disabled = false;
}

$('#wizNext').addEventListener('click', () => wizStep(2));
$('#wizBack').addEventListener('click', () => wizStep(1));

$('#wizFinish').addEventListener('click', async () => {
  const btn = $('#wizFinish');
  const msg = $('#wizMsg');
  const raw = $('#wizRaw').value.trim();
  const clips = $('#wizClips').value.trim();
  if (!raw || !clips) { msg.className = 'wiz-msg bad'; msg.textContent = 'Both folders are needed.'; return; }

  btn.disabled = true;
  msg.className = 'wiz-msg muted';
  msg.textContent = 'Saving…';
  try {
    // The server creates the folders if they are not there yet.
    await post('/api/settings', Object.assign({}, wiz.rec, {
      rawFolder: raw,
      clipsFolder: clips,
      hotkey: $('#wizHotkey').value.trim() || state.settings.hotkey,
      recordMic: $('#wizMic').checked,
      recordEnabled: $('#wizRec').checked,
    }));
    // Prove the folder is usable before leaving the wizard.
    await api('/api/local?kind=raw');
    await post('/api/settings', { setupDone: true });
    location.reload();
  } catch (e) {
    // A key another program already owns, most likely.
    msg.className = 'wiz-msg bad';
    msg.textContent = e.message;
    btn.disabled = false;
  }
});

// ---------- tabs ----------
function showPanel(name) {
  document.querySelectorAll('.tab').forEach((x) => x.classList.toggle('active', x.dataset.tab === name));
  document.querySelectorAll('.panel').forEach((x) => x.classList.remove('active'));
  $('#tab-' + name).classList.add('active');
  if (name === 'record') { recFill(); loadGames(); loadStorage(); }
  if (name === 'clips') loadClips();
}
document.querySelectorAll('.tab').forEach((t) => t.addEventListener('click', () => showPanel(t.dataset.tab)));

// ---------- shared card ----------
function mkBtn(label, cls, fn) {
  const b = document.createElement('button');
  b.className = 'btn ' + cls;
  b.textContent = label;
  b.addEventListener('click', (ev) => { ev.stopPropagation(); fn(b); });
  return b;
}

// reload is what runs after this card's file changes on disk. Removing the one
// card is not enough: the same file also sits in the Recent strip, the folder
// counts still claim it, and state.groups would still hand it back on the next
// render - which is the ghost that survived until you left and came back.
function makeCard(f, reload) {
  const card = document.createElement('div');
  card.className = 'card-item';

  const thumb = document.createElement('div');
  thumb.className = 'thumb';
  const img = document.createElement('img');
  img.loading = 'lazy';
  img.src = '/api/thumb?path=' + encodeURIComponent(f.path);
  img.onerror = () => { thumb.classList.add('nothumb'); img.remove(); };
  thumb.appendChild(img);
  thumb.title = 'Open the trimmer';
  thumb.addEventListener('click', () => openEditor(f));
  card.appendChild(thumb);

  const name = document.createElement('div');
  name.className = 'c-name';
  name.textContent = f.name;
  name.title = f.name;
  card.appendChild(name);

  const meta = document.createElement('div');
  meta.className = 'c-meta';
  meta.textContent = fmtBytes(f.size) + ' · ' + fmtDate(f.modified);
  card.appendChild(meta);

  const rowA = document.createElement('div');
  rowA.className = 'c-actions';
  rowA.appendChild(mkBtn('Reveal', 'ghost small', async () => {
    try { await post('/api/reveal', { path: f.path }); } catch (e) { toast(e.message, 'err'); }
  }));
  rowA.appendChild(mkBtn('Edit', 'ghost small', () => openEditor(f)));
  rowA.appendChild(mkBtn('Shrink', 'ghost small', async (b) => {
    b.disabled = true; b.textContent = '…';
    try {
      const r = await post('/api/shrink', { path: f.path });
      if (r.kept) toast('Already compact: it would not get smaller without losing quality, so nothing changed', 'ok');
      else toast('Saved ' + r.name + ' (' + fmtBytes(r.size) + ')', 'ok');
      if (reload) await reload();
    } catch (e) { toast(e.message, e.cancelled ? '' : 'err'); }
    b.disabled = false; b.textContent = 'Shrink';
  }));
  card.appendChild(rowA);

  const rowB = document.createElement('div');
  rowB.className = 'c-actions';
  rowB.appendChild(mkBtn('Delete', 'danger small', async () => {
    const yes = await ask({
      title: 'Delete this file?',
      body: 'It is removed from your disk permanently. There is no trash folder.',
      detail: f.name + '  (' + fmtBytes(f.size) + ')',
      confirmLabel: 'Delete', cancelLabel: 'Keep', danger: true,
    });
    if (!yes) return;
    try {
      await post('/api/local/delete', { path: f.path });
      // Fade first so the click feels instant, then re-read from disk.
      card.style.opacity = '0';
      toast('Deleted.', 'ok');
      if (reload) await reload();
    } catch (e) { card.style.opacity = ''; toast(e.message, 'err'); }
  }));
  card.appendChild(rowB);

  return card;
}

// ---------- raw videos ----------
async function loadRaw() {
  try {
    state.groups = await api('/api/local?kind=raw');
    const all = state.groups.flatMap((g) => g.files);
    $('#rawLabel').textContent = all.length + ' files in ' + state.groups.length + ' folders';

    // Recent: newest 5 across every folder, because "what did I just record"
    // is the most common reason to open this app.
    const recent = all.slice().sort((a, b) => b.modified - a.modified).slice(0, 5);
    const rg = $('#recentGrid');
    rg.innerHTML = '';
    recent.forEach((f) => rg.appendChild(makeCard(f, refreshRaw)));
    stagger(rg);

    const fg = $('#folderGrid');
    fg.innerHTML = '';
    state.groups.forEach((g) => {
      const el = document.createElement('div');
      el.className = 'folder';
      const cover = g.files[0];
      if (cover) {
        const img = document.createElement('img');
        img.loading = 'lazy';
        img.src = '/api/thumb?path=' + encodeURIComponent(cover.path);
        img.onerror = () => img.remove();
        el.appendChild(img);
      }
      const body = document.createElement('div');
      body.className = 'folder-body';
      const nm = document.createElement('div');
      nm.className = 'folder-name';
      nm.textContent = g.name;
      const ct = document.createElement('div');
      ct.className = 'folder-count';
      ct.textContent = g.files.length + (g.files.length === 1 ? ' clip' : ' clips');
      body.appendChild(nm); body.appendChild(ct);
      el.appendChild(body);
      el.addEventListener('click', () => openGame(g));
      fg.appendChild(el);
    });
    stagger(fg);

    if (!state.groups.length) {
      fg.innerHTML = '<p class="muted">No videos yet. Start a game and press ' + state.settings.hotkey + ' to save the ' + keptText(state.settings.replaySeconds) + '.</p>';
    }
  } catch (e) {
    toast(e.message, 'err');
  }
}
$('#rawRefresh').addEventListener('click', loadRaw);
$('#rawOpen').addEventListener('click', () => {
  if (state.settings.rawFolder) post('/api/reveal', { path: state.settings.rawFolder }).catch(() => {});
});

function openGame(g) {
  state.game = g;
  $('#rawHome').classList.add('hidden');
  $('#rawGame').classList.remove('hidden');
  $('#gameTitle').textContent = g.name;
  $('#gameCount').textContent = g.files.length;
  const grid = $('#gameGrid');
  grid.innerHTML = '';
  g.files.forEach((f) => grid.appendChild(makeCard(f, refreshRaw)));
  stagger(grid);
  $('#tab-raw').scrollTop = 0;
}
// Re-reads every raw folder and redraws whichever view is on screen, so a
// refresh never costs you your place. Used by the Refresh button and by
// anything that changes the files underneath it.
async function refreshRaw() {
  const inGame = !$('#rawGame').classList.contains('hidden');
  const name = inGame && state.game ? state.game.name : null;
  await loadRaw();
  if (!name) return;
  const g = state.groups.find((x) => x.name === name);
  // The folder can empty out while you are looking at it, in which case there
  // is nothing left to show and the list is the only sensible place to be.
  if (g) openGame(g);
  else $('#backHome').click();
}
$('#gameRefresh').addEventListener('click', refreshRaw);

$('#gameOpen').addEventListener('click', () => {
  const dir = state.game && state.game.dir;
  if (!dir) return;
  post('/api/reveal', { path: dir }).catch((err) => toast(err.message, 'err'));
});

$('#backHome').addEventListener('click', () => {
  $('#rawGame').classList.add('hidden');
  $('#rawHome').classList.remove('hidden');
  state.game = null;
});

// ---------- clips ----------
async function loadClips() {
  const grid = $('#clipsGrid');
  grid.innerHTML = '<p class="muted">Loading…</p>';
  try {
    const groups = await api('/api/local?kind=clips');
    const files = groups.flatMap((g) => g.files).sort((a, b) => b.modified - a.modified);
    $('#clipsLabel').textContent = files.length + ' clip' + (files.length === 1 ? '' : 's');
    grid.innerHTML = '';
    if (!files.length) { grid.innerHTML = '<p class="muted">No clips yet — trim a recording first.</p>'; return; }
    files.forEach((f) => grid.appendChild(makeCard(f, loadClips)));
    stagger(grid);
  } catch (e) { grid.innerHTML = ''; toast(e.message, 'err'); }
}
$('#clipsRefresh').addEventListener('click', loadClips);

// ---------- trim editor ----------
const P = {
  vid: null, dur: 0, dragging: null, previewing: false, resume: false,
  icons: {
    play: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M8 5.5v13l11-6.5z"/></svg>',
    pause: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M7 5h3.2v14H7zM13.8 5H17v14h-3.2z"/></svg>',
    vol: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M4 9v6h4l5 4V5L8 9H4z"/></svg>',
    mute: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M4 9v6h4l5 4V5L8 9H4zm13.5 3l2.7-2.7-1-1L16.5 11l-2.7-2.7-1 1L15.5 12l-2.7 2.7 1 1L16.5 13l2.7 2.7 1-1z"/></svg>',
    full: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M5 5h5v2H7v3H5V5zm9 0h5v5h-2V7h-3V5zM5 14h2v3h3v2H5v-5zm12 0h2v5h-5v-2h3v-3z"/></svg>',
    exitFull: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M8 5h2v5H5V8h3V5zm6 0h2v3h3v2h-5V5zM5 14h5v5H8v-3H5v-2zm9 0h5v2h-3v3h-2v-5z"/></svg>',
  },
};

function openEditor(f) {
  state.sel = f;
  $('#edName').textContent = f.name;
  $('#editor').classList.remove('hidden');

  // Recordings with several audio tracks (ours keep game, PC audio and mic
  // apart) get a choice of which one leads the cut, or of none at all.
  const row = $('#audioRow');
  row.classList.add('hidden');
  api('/api/probe?path=' + encodeURIComponent(f.path)).then((info) => {
    if (state.sel !== f) return;
    if (!f.size) f.size = info.sizeBytes;
    const tracks = info.audioTracks || [];
    if (!tracks.length) return;
    const sel = $('#audioPick');
    sel.innerHTML = '';
    tracks.forEach((t, i) => {
      const o = document.createElement('option');
      o.value = i;
      o.textContent = t;
      sel.appendChild(o);
    });
    const none = document.createElement('option');
    none.value = -2; // NO_AUDIO in ffmpeg.rs
    none.textContent = 'No sound (removes all audio)';
    sel.appendChild(none);
    sel.value = '0';
    row.classList.remove('hidden');
  }).catch(() => {});

  const v = $('#player');
  P.vid = v;
  v.src = '/media?path=' + encodeURIComponent(f.path);
  v.volume = 1;
  v.onloadedmetadata = () => {
    P.dur = v.duration || 0;
    state.inPt = 0;
    state.outPt = P.dur;
    $('#pDur').textContent = fmtTime(P.dur);
    paintTrim();
  };
  v.ontimeupdate = () => {
    $('#pCur').textContent = fmtTime(v.currentTime);
    paintHead();
    // Preview stops at the out point so you hear/see exactly what gets kept
    if (P.previewing && v.currentTime >= state.outPt) {
      v.pause();
      P.previewing = false;
      $('#pPlaySel').classList.remove('armed');
    }
  };
  v.onplay = () => { $('#pPlay').innerHTML = P.icons.pause; $('#bigPlay').classList.add('hide'); };
  v.onpause = () => { $('#pPlay').innerHTML = P.icons.play; $('#bigPlay').classList.remove('hide'); };
}

function closeEditor() {
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  $('#editor').classList.add('hidden');
  const v = $('#player');
  v.pause();
  v.removeAttribute('src');
  v.load();
  P.previewing = false;
}

function togglePlay() {
  const v = P.vid;
  if (!v) return;
  if (v.paused) v.play().catch(() => {});
  else v.pause();
}

// ---- timeline painting ----
const pct = (t) => (P.dur > 0 ? Math.max(0, Math.min(100, (t / P.dur) * 100)) : 0);

function paintTrim() {
  $('#inLabel').textContent = fmtTime(state.inPt);
  $('#outLabel').textContent = fmtTime(state.outPt);
  $('#lenLabel').textContent = fmtTime(Math.max(0, state.outPt - state.inPt));

  const a = pct(state.inPt), b = pct(state.outPt);
  $('#tlSel').style.left = a + '%';
  $('#tlSel').style.width = Math.max(0, b - a) + '%';
  $('#mkIn').style.left = a + '%';
  $('#mkOut').style.left = b + '%';
  // dim everything outside the kept region
  $('#tlBuf').style.background =
    'linear-gradient(90deg, rgba(8,9,13,.62) ' + a + '%, transparent ' + a + '%, transparent ' + b + '%, rgba(8,9,13,.62) ' + b + '%)';
  paintHead();
}
function paintHead() {
  if (!P.vid) return;
  $('#tlHead').style.left = pct(P.vid.currentTime) + '%';
}

function seekToClientX(clientX) {
  const tl = $('#timeline');
  const r = tl.getBoundingClientRect();
  const ratio = Math.max(0, Math.min(1, (clientX - r.left) / r.width));
  return ratio * P.dur;
}

// ---- dragging the timeline ----
// The playhead and the two markers are one gesture, told apart by what was
// under the pointer when it went down: 'head' scrubs, 'in'/'out' move an edge.
// A press on the bare track still lands exactly where you clicked, so the old
// click-to-seek is unchanged - it is now just the first frame of a drag.
function beginDrag(which, ev) {
  if (!P.dur || !P.vid) return;
  ev.preventDefault();
  ev.stopPropagation();
  P.dragging = which;

  if (which === 'head') {
    // Seeking over and over while playing outruns the decoder and stutters, so
    // playback is parked for the drag and handed back on release.
    P.resume = !P.vid.paused;
    P.vid.pause();
    // A scrub is a deliberate move elsewhere; a running preview would only
    // fight it by stopping at the out point.
    P.previewing = false;
    $('#pPlaySel').classList.remove('armed');
    $('#timeline').classList.add('scrub');
  } else {
    document.getElementById(which === 'in' ? 'mkIn' : 'mkOut').classList.add('drag');
  }
  dragTo(ev.clientX);
}

function dragTo(clientX) {
  const t = seekToClientX(clientX);
  if (P.dragging === 'in') {
    state.inPt = Math.min(t, state.outPt - 0.05);
    P.vid.currentTime = state.inPt;
  } else if (P.dragging === 'out') {
    state.outPt = Math.max(t, state.inPt + 0.05);
    P.vid.currentTime = state.outPt;
  } else {
    P.vid.currentTime = t;
  }
  paintTrim();
}

['mkIn', 'mkOut'].forEach((id) => {
  const el = document.getElementById(id);
  el.addEventListener('pointerdown', (ev) => beginDrag(id === 'mkIn' ? 'in' : 'out', ev));
});
// Anywhere else on the bar scrubs. The markers stop propagation, so a press
// that starts on a handle never reaches this.
$('#timeline').addEventListener('pointerdown', (ev) => beginDrag('head', ev));

// Tracked on the window rather than the bar so the drag survives the pointer
// leaving it: letting go off the edge should not strand a half-finished drag.
window.addEventListener('pointermove', (ev) => {
  if (!P.dragging) return;
  ev.preventDefault();
  dragTo(ev.clientX);
});
window.addEventListener('pointerup', () => {
  if (!P.dragging) return;
  if (P.dragging === 'head') {
    $('#timeline').classList.remove('scrub');
    if (P.resume) P.vid.play().catch(() => {});
    P.resume = false;
  } else {
    document.getElementById(P.dragging === 'in' ? 'mkIn' : 'mkOut').classList.remove('drag');
  }
  P.dragging = null;
});

// ---- transport wiring ----
$('#pPlay').addEventListener('click', togglePlay);
$('#bigPlay').addEventListener('click', togglePlay);
$('#player').addEventListener('click', togglePlay);
$('#pIn').addEventListener('click', () => { state.inPt = Math.min(P.vid.currentTime, state.outPt - 0.05); paintTrim(); });
$('#pOut').addEventListener('click', () => { state.outPt = Math.max(P.vid.currentTime, state.inPt + 0.05); paintTrim(); });
$('#pPlaySel').addEventListener('click', () => {
  if (!P.vid) return;
  P.vid.currentTime = state.inPt;
  P.previewing = true;
  $('#pPlaySel').classList.add('armed');
  P.vid.play().catch(() => {});
});
$('#pMute').addEventListener('click', () => {
  const v = P.vid; if (!v) return;
  v.muted = !v.muted;
  $('#pMute').innerHTML = v.muted ? P.icons.mute : P.icons.vol;
});
$('#pVol').addEventListener('input', (e) => {
  if (!P.vid) return;
  P.vid.volume = Number(e.target.value);
  P.vid.muted = P.vid.volume === 0;
  $('#pMute').innerHTML = P.vid.muted ? P.icons.mute : P.icons.vol;
});

// ---- full screen ----
// The stage goes fullscreen rather than the whole dialog: the point is to watch
// the video, and dragging the trim controls along would only shrink it.
function toggleFullscreen() {
  const st = $('#stage');
  if (!st || !st.requestFullscreen) return;
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  else st.requestFullscreen().catch((e) => toast('Could not go full screen: ' + e.message, 'err'));
}
$('#pFull').addEventListener('click', toggleFullscreen);
$('#player').addEventListener('dblclick', toggleFullscreen);
document.addEventListener('fullscreenchange', () => {
  // Fullscreen can also be left with the browser's own Escape, so the button is
  // repainted from the real state rather than from what we last asked for.
  const on = !!document.fullscreenElement;
  $('#pFull').innerHTML = on ? P.icons.exitFull : P.icons.full;
  $('#pFull').title = on ? 'Leave full screen (F)' : 'Full screen (F)';
});

$('#edClose').addEventListener('click', closeEditor);
$('#editor').addEventListener('click', (e) => { if (e.target.id === 'editor') closeEditor(); });

// ---- keyboard, only while the editor is open ----
document.addEventListener('keydown', (e) => {
  const open = !$('#editor').classList.contains('hidden');
  if (e.key === 'Escape') {
    // In fullscreen, Escape means leave fullscreen. The browser usually eats
    // the key itself, but where it does not, closing the whole editor instead
    // would be a nasty surprise.
    if (document.fullscreenElement) return;
    if (open) closeEditor();
    return;
  }
  if (!open || !P.vid) return;
  if (e.target.tagName === 'INPUT') return;

  const step = e.shiftKey ? 5 : 1 / 30; // shift jumps, otherwise a frame-ish nudge
  if (e.code === 'Space') { e.preventDefault(); togglePlay(); }
  else if (e.key === 'i' || e.key === 'I') { state.inPt = Math.min(P.vid.currentTime, state.outPt - 0.05); paintTrim(); }
  else if (e.key === 'o' || e.key === 'O') { state.outPt = Math.max(P.vid.currentTime, state.inPt + 0.05); paintTrim(); }
  else if (e.key === 'm' || e.key === 'M') { $('#pMute').click(); }
  else if (e.key === 'f' || e.key === 'F') { toggleFullscreen(); }
  else if (e.key === 'ArrowLeft') { e.preventDefault(); P.vid.currentTime = Math.max(0, P.vid.currentTime - step); }
  else if (e.key === 'ArrowRight') { e.preventDefault(); P.vid.currentTime = Math.min(P.dur, P.vid.currentTime + step); }
});

$('#cutBtn').addEventListener('click', async () => {
  if (!state.sel) return;
  if (state.outPt <= state.inPt) { toast('Set an end point after the start point.', 'err'); return; }
  const btn = $('#cutBtn');
  const src = state.sel;
  btn.disabled = true;
  try {
    const audio = $('#audioRow').classList.contains('hidden') ? 0 : Number($('#audioPick').value) || 0;
    const r = await post('/api/cut', { path: src.path, start: state.inPt, end: state.outPt, audio: audio });
    toast('Saved ' + r.name, 'ok');
    closeEditor();

    // Offer to reclaim the space now that the good bit is saved. Never
    // automatic: deleting a source recording has to be a deliberate answer.
    // Deleting a source recording is destructive, so it is either an explicit
    // answer or an explicit setting - never a silent default.
    const always = state.settings && state.settings.alwaysDeleteSource;
    const remove = always || (await ask({
      title: 'Clip saved',
      body: 'Delete the original recording to free up the space?',
      detail: src.name + '  (' + fmtBytes(src.size) + ')',
      confirmLabel: 'Delete original', cancelLabel: 'Keep it', danger: true,
    }));
    if (remove) {
      try {
        await post('/api/local/delete', { path: src.path });
        toast('Original deleted.', 'ok');
      } catch (e) { toast(e.message, 'err'); }
    }
    refreshRaw();
  } catch (e) { toast(e.message, e.cancelled ? '' : 'err'); }
  finally { btn.disabled = false; }
});

// ---------- settings ----------
function fillSettings() {
  const s = state.settings;
  $('#setRaw').value = s.rawFolder || '';
  $('#setClips').value = s.clipsFolder || '';
  $('#setQuality').value = s.quality || 'balanced';
  $('#setAlwaysDel').checked = !!s.alwaysDeleteSource;
}
$('#setSave').addEventListener('click', async () => {
  try {
    await post('/api/settings', {
      rawFolder: $('#setRaw').value.trim(),
      clipsFolder: $('#setClips').value.trim(),
      quality: $('#setQuality').value,
      alwaysDeleteSource: $('#setAlwaysDel').checked,
    });
    state.settings = await api('/api/settings');
    fillSettings();
    $('#setMsg').textContent = 'Saved.';
    loadRaw();
  } catch (e) { $('#setMsg').textContent = e.message; }
});

// ---------- recording ----------
// The replay buffer. Every control saves the moment it changes and the server
// restarts the recorder to match, so there is no Save button to forget.
const REC = { last: null };
const QUALITY_NAMES = { 19: 'High', 23: 'Balanced', 28: 'Small' };
const ENCODER_NAMES = {
  h264_nvenc: 'NVIDIA NVENC', h264_amf: 'AMD AMF',
  h264_mf: 'Windows hardware encoder', libx264: 'CPU (x264)',
};
const baseName = (p) => (p || '').split(/[\\/]/).pop();

// What a PC scan found, as label and value rows for a .scan grid.
function scanRows(s) {
  return [
    ['Graphics', (s.gpus || []).join(', ') || 'unknown'],
    ['Encoder', s.encoder ? ENCODER_NAMES[s.encoder] || s.encoder : 'none works: ' + s.encoderError],
    ['Screen', s.screen.width ? s.screen.width + '×' + s.screen.height + ' at ' + s.screen.hz + ' Hz' : 'unknown'],
    ['Processor', s.cpuThreads + ' threads'],
    ['Memory', Math.round(s.ramGB) + ' GB'],
    ['Free space', Math.round(s.freeGB) + ' GB on the buffer drive'],
  ];
}

function fillScan(out, rows) {
  out.innerHTML = '';
  rows.forEach(([k, v]) => {
    const a = document.createElement('span');
    a.textContent = k;
    const b = document.createElement('b');
    b.textContent = v;
    out.appendChild(a);
    out.appendChild(b);
  });
}

const describeRec = (r) => r.recordFps + ' fps · ' + (QUALITY_NAMES[r.recordQuality] || 'quality ' + r.recordQuality) +
  ' · last ' + r.replaySeconds + ' s kept · automatic encoder';

function keptText(sec) {
  if (sec === 60) return 'last minute';
  return sec % 60 ? 'last ' + sec + ' seconds' : 'last ' + sec / 60 + ' minutes';
}

// A value hand-edited into settings.json may not be one of the options; it is
// added rather than silently shown as something else.
function setSelect(sel, v) {
  v = String(v);
  if (![...sel.options].some((o) => o.value === v)) {
    const o = document.createElement('option');
    o.value = v;
    o.textContent = v;
    sel.appendChild(o);
  }
  sel.value = v;
}

// Only asked for in whole-screen mode, and only offered with more than one monitor.
async function recFillMonitors(s) {
  const row = $('#recMonitorRow');
  const list = s.recordMode === 'screen' ? (await api('/api/rec/monitors').catch(() => [])) || [] : [];
  row.classList.toggle('hidden', list.length < 2);
  const sel = $('#recMonitor');
  sel.replaceChildren(...list.map((m) => {
    const o = document.createElement('option');
    o.value = m.id;
    o.textContent = m.output === null ? m.label + ' (on another graphics card, cannot be recorded)' : m.label;
    o.disabled = m.output === null;
    return o;
  }));
  // A monitor that was unplugged is recorded as the main one; show what is true.
  const usable = list.filter((m) => m.output !== null);
  const cur = usable.find((m) => m.id === s.recordMonitor) || usable.find((m) => m.primary) || usable[0];
  if (cur) sel.value = cur.id;
}

function recFill() {
  const s = state.settings;
  if (!s) return;
  $('#recMode').value = s.recordMode || 'game';
  recFillMonitors(s);
  $('#recHotkey').value = s.hotkey || '';
  $('#recKeyHint').textContent = s.hotkey || '';
  $('#recSwapHotkey').value = s.swapHotkey || '';
  $('#recSwapBack').checked = !!s.swapBackAfterClip;
  $('#recNotify').checked = !s.quietSaves;
  setSelect($('#recStorage'), s.storageLimitGB || 0);
  setSelect($('#recSeconds'), s.replaySeconds || 60);
  setSelect($('#recFps'), s.recordFps || 60);
  setSelect($('#recQuality'), s.recordQuality || 23);
  $('#recEncoder').value = s.recordEncoder || '';
  $('#recCodec').value = s.recordCodec === 'av1' ? 'av1' : 'h264';
  $('#recBufferMem').value = s.replayInMemory ? 'memory' : 'disk';
  $('#recAutoClip').checked = !!s.autoClip;
  $('#recMic').checked = !!s.recordMic;
  $('#recAutostart').checked = !!s.startWithWindows;
}

async function recApply(patch) {
  const msg = $('#recSavedMsg');
  msg.textContent = 'Applying…';
  try {
    await post('/api/settings', patch);
    // Only these restart the recorder (applyRecordSettings on the server).
    const restarts = ['recordMode', 'recordMonitor', 'replaySeconds', 'recordFps', 'recordQuality', 'recordEncoder', 'recordCodec', 'replayInMemory', 'recordMic', 'hotkey']
      .some((k) => k in patch);
    msg.textContent = state.settings.recordEnabled && restarts ? 'Saved. The buffer started over.' : 'Saved.';
  } catch (e) {
    // The server has already put the old settings back; show those.
    msg.textContent = e.message;
    toast(e.message, 'err');
  }
  state.settings = await api('/api/settings').catch(() => state.settings);
  recFill();
  recPoll(true);
}

[['recMode', 'recordMode', String], ['recMonitor', 'recordMonitor', String], ['recSeconds', 'replaySeconds', Number], ['recFps', 'recordFps', Number],
  ['recQuality', 'recordQuality', Number], ['recEncoder', 'recordEncoder', String]].forEach(([id, key, conv]) => {
  $('#' + id).addEventListener('change', (e) => recApply({ [key]: conv(e.target.value) }));
});
$('#recCodec').addEventListener('change', (e) => recApply({ recordCodec: e.target.value }));
$('#recBufferMem').addEventListener('change', (e) => recApply({ replayInMemory: e.target.value === 'memory' }));
$('#recAutoClip').addEventListener('change', (e) => recApply({ autoClip: e.target.checked }));
$('#recMic').addEventListener('change', (e) => recApply({ recordMic: e.target.checked }));
$('#recSwapBack').addEventListener('change', (e) => recApply({ swapBackAfterClip: e.target.checked }));
$('#recAutostart').addEventListener('change', (e) => recApply({ startWithWindows: e.target.checked }));
$('#recNotify').addEventListener('change', (e) => recApply({ quietSaves: !e.target.checked }));
$('#recStorage').addEventListener('change', async (e) => {
  const gb = Number(e.target.value);
  const s = await api('/api/storage').catch(() => null);
  if (gb && s && s.bytes > gb * 1024 ** 3) {
    const ok = await ask({
      title: 'Delete old clips now?',
      body: 'Saved clips take up ' + fmtBytes(s.bytes) + '. Keeping them under ' + gb + ' GB deletes the oldest ones straight away, for good.',
      confirmLabel: 'Delete old clips', danger: true,
    });
    if (!ok) { recFill(); return; }
  }
  await recApply({ storageLimitGB: gb });
  setTimeout(loadStorage, 1500);
});

async function loadStorage() {
  try {
    const s = await api('/api/storage');
    $('#storeUse').textContent = s.count
      ? 'Clips saved by this app take up ' + fmtBytes(s.bytes) + ' (' + s.count + ' clip' + (s.count === 1 ? '' : 's') + ').'
      : 'No clips saved by this app yet.';
  } catch (e) { /* leave the old text */ }
}

// A hotkey box records the next key combination pressed in it, and hands it
// to done. On blur it shows what is saved again (restore).
function hotkeyBox(input, done, restore) {
  input.addEventListener('focus', (e) => { e.target.value = ''; e.target.placeholder = 'Press the keys…'; });
  input.addEventListener('blur', (e) => { e.target.placeholder = 'Click, then press the keys'; restore(); });
  input.addEventListener('keydown', (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.key === 'Escape') { e.target.blur(); return; }
    let key = '';
    const named = ['Insert', 'Delete', 'Home', 'End', 'PageUp', 'PageDown', 'Pause', 'ScrollLock'];
    if (/^F\d{1,2}$/.test(e.key)) key = e.key;
    else if (named.includes(e.key)) key = e.key;
    // The key under Esc, by position: it types something different on every
    // layout (` on US, " on Turkish Q).
    else if (e.code === 'Backquote') key = 'Backquote';
    else if (/^Key[A-Z]$/.test(e.code)) key = e.code.slice(3);
    else if (/^Digit\d$/.test(e.code)) key = e.code.slice(5);
    else if (/^Numpad\d$/.test(e.code)) key = 'Num' + e.code.slice(6);
    if (!key) return; // a modifier on its own: wait for the rest
    const mods = [];
    if (e.ctrlKey) mods.push('Ctrl');
    if (e.altKey) mods.push('Alt');
    if (e.shiftKey) mods.push('Shift');
    if (e.metaKey) mods.push('Win');
    // A bare letter or digit would fire every time you type it, in game chat
    // included; F-keys, Insert, Home and the like are fine alone.
    if (!mods.length && /^[A-Z0-9]$/.test(key)) { e.target.placeholder = 'Add Ctrl, Alt or Shift to a letter or number'; return; }
    const combo = mods.concat([key]).join('+');
    e.target.value = combo;
    done(combo);
    e.target.blur();
  });
}
hotkeyBox($('#recHotkey'), (combo) => recApply({ hotkey: combo }), recFill);
hotkeyBox($('#recSwapHotkey'), (combo) => recApply({ swapHotkey: combo }), recFill);
// The wizard keeps the choice until Finish; the box itself holds it.
hotkeyBox($('#wizHotkey'), (combo) => { wiz.hotkey = combo; },
  () => { $('#wizHotkey').value = wiz.hotkey || state.settings.hotkey; });

function fmtSince(unix) {
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unix));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
  return h ? h + 'h ' + m + 'm' : m + 'm ' + (s % 60) + 's';
}

function renderRec(st) {
  // The tray menu can switch recording too, so the status is what decides.
  if (state.settings && st.enabled !== undefined) state.settings.recordEnabled = st.enabled;
  const on = !!(state.settings && state.settings.recordEnabled);
  const dot = $('#recDot');
  let title = 'Recording is off', detail = 'Nothing is recorded until you turn it on.', kind = '';
  if (st.recording) {
    title = 'Recording ' + (st.target || 'the screen');
    detail = st.encoder + ' · last ' + st.bufferSec + ' s kept · running ' + fmtSince(st.since);
    if (st.swapped && st.screen && state.settings && state.settings.swapBackAfterClip) detail += ' · back to games after one clip';
    kind = 'live';
  } else if (on && st.waiting) {
    title = 'Waiting for a game';
    detail = 'Start one - recording begins once it is in front.';
    kind = 'wait';
  } else if (on) {
    title = 'Starting…';
    detail = '';
    kind = 'wait';
  }
  $('#recState').textContent = title;
  $('#recDetail').textContent = detail;
  dot.className = 'rec-dot ' + kind;
  $('#recToggle').textContent = on ? 'Turn off' : 'Turn on';
  $('#recToggle').className = 'btn ' + (on ? 'ghost' : 'primary');
  $('#recSave').disabled = !st.recording;
  const swap = $('#recSwap');
  swap.classList.toggle('hidden', !on);
  swap.textContent = st.screen ? (st.swapped ? 'Back to games only' : 'Switch to games only') : (st.swapped ? 'Back to whole screen' : 'Record whole screen');
  const swapErr = $('#recSwapErr');
  swapErr.textContent = st.swapKeyError || '';
  swapErr.classList.toggle('hidden', !st.swapKeyError);

  const err = $('#recErr');
  err.textContent = st.error || '';
  err.classList.toggle('hidden', !st.error);
  const notice = $('#recNotice');
  notice.textContent = st.notice || '';
  notice.classList.toggle('hidden', !st.notice);

  const pill = $('#recPill');
  pill.classList.toggle('hidden', !on);
  pill.className = 'recpill ' + kind + (on ? '' : ' hidden');
  pill.querySelector('span').textContent = st.recording ? (st.target || 'Screen') : 'Waiting';

  const last = st.lastClip;
  $('#recLast').classList.toggle('hidden', !last);
  if (last) $('#recLastName').textContent = baseName(last);
}

async function recPoll(once) {
  try {
    const st = await api('/api/rec');
    renderRec(st);
    // The "Clip saved" notification was clicked: straight to the editor.
    if (st.open) openEditor({ path: st.open, name: baseName(st.open) });
    // A clip saved by the hotkey shows up without anyone pressing Refresh.
    if (REC.last !== null && st.lastClip && st.lastClip !== REC.last) {
      toast('Clip saved: ' + baseName(st.lastClip), 'ok');
      if ($('#tab-raw').classList.contains('active')) refreshRaw();
    }
    REC.last = st.lastClip || '';
  } catch (e) { /* keep polling */ }
  if (!once) setTimeout(recPoll, 1500);
}

$('#recToggle').addEventListener('click', async (ev) => {
  const btn = ev.currentTarget;
  const on = state.settings.recordEnabled;
  btn.disabled = true;
  try {
    await post(on ? '/api/rec/stop' : '/api/rec/start', {});
  } catch (e) { toast(e.message, 'err'); }
  state.settings = await api('/api/settings').catch(() => state.settings);
  btn.disabled = false;
  recPoll(true);
});

$('#recSwap').addEventListener('click', async (ev) => {
  const btn = ev.currentTarget;
  btn.disabled = true;
  try { await post('/api/rec/swap', {}); } catch (e) { toast(e.message, 'err'); }
  btn.disabled = false;
  recPoll(true);
});

$('#recSave').addEventListener('click', async (ev) => {
  const btn = ev.currentTarget;
  btn.disabled = true;
  try {
    await post('/api/rec/save', {});
    recPoll(true);
  } catch (e) { toast(e.message, 'err'); }
  btn.disabled = false;
});

$('#recPill').addEventListener('click', () => document.querySelector('.tab[data-tab="record"]').click());
$('#recLastOpen').addEventListener('click', () => {
  if (REC.last) openEditor({ path: REC.last, name: baseName(REC.last) });
});
$('#recLastReveal').addEventListener('click', () => {
  if (REC.last) post('/api/reveal', { path: REC.last }).catch((e) => toast(e.message, 'err'));
});

async function loadGames() {
  const list = $('#gameList');
  try {
    const games = (await api('/api/rec/games')) || [];
    list.innerHTML = '';
    if (!games.length) {
      list.innerHTML = '<p class="muted">None found. Fullscreen games are still recorded.</p>';
      return;
    }
    games.slice().sort((a, b) => a.name.localeCompare(b.name)).forEach((g) => {
      const row = document.createElement('div');
      row.className = 'game-row';
      row.title = g.dir;
      const n = document.createElement('span');
      n.textContent = g.name;
      const s = document.createElement('span');
      s.className = 'pillcount';
      s.textContent = g.store;
      row.appendChild(n);
      if (g.store === 'Added by you') {
        const rm = document.createElement('button');
        rm.className = 'btn ghost small';
        rm.textContent = 'Remove';
        rm.addEventListener('click', () => changeApps({ remove: g.dir }));
        row.appendChild(rm);
      } else {
        row.appendChild(s);
      }
      list.appendChild(row);
    });
  } catch (e) { list.innerHTML = ''; toast(e.message, 'err'); }
}

async function changeApps(body) {
  try {
    await post('/api/rec/apps', body);
    $('#appPick').classList.add('hidden');
    await loadGames();
  } catch (e) { toast(e.message, 'err'); }
}

// Adding is picking from what is open, so nobody has to go looking for an exe.
$('#addAppBtn').addEventListener('click', async () => {
  const pick = $('#appPick');
  if (!pick.classList.contains('hidden')) { pick.classList.add('hidden'); return; }
  try {
    const apps = (await api('/api/rec/apps')) || [];
    pick.innerHTML = '';
    const note = document.createElement('p');
    note.className = 'muted';
    note.textContent = apps.length ? 'Open right now. Not here? Open it and press Add an app again.' : 'No programs are open.';
    pick.appendChild(note);
    apps.forEach((a) => {
      const row = document.createElement('div');
      row.className = 'game-row';
      const n = document.createElement('span');
      n.textContent = a.name;
      const add = document.createElement('button');
      add.className = 'btn ghost small';
      add.textContent = 'Add';
      add.addEventListener('click', () => changeApps({ add: a.exe }));
      row.appendChild(n);
      row.appendChild(add);
      pick.appendChild(row);
    });
    pick.classList.remove('hidden');
  } catch (e) { toast(e.message, 'err'); }
});

$('#scanBtn').addEventListener('click', async (ev) => {
  const btn = ev.currentTarget;
  const out = $('#scanOut');
  btn.disabled = true;
  out.textContent = 'Scanning… (this tries each encoder for real, a few seconds)';
  try {
    const s = await api('/api/rec/scan');
    fillScan(out, scanRows(s).concat([['Games', (s.games || []).length + ' installed']]));
    state.scanRec = s.recommended;
    $('#scanRec').textContent = describeRec(s.recommended);
    $('#scanApplyRow').classList.remove('hidden');
  } catch (e) {
    out.textContent = e.message;
  }
  btn.disabled = false;
});
$('#scanApply').addEventListener('click', () => {
  if (state.scanRec) recApply(state.scanRec);
});

// ---------- updates ----------
// The server checks GitHub's latest release. An install from the setup exe
// updates itself; a portable copy is sent to the releases page.
async function checkUpdate(force) {
  try {
    const u = await api('/api/update' + (force ? '?check=1' : ''));
    state.update = u;
    $('#aboutVersion').textContent = 'Version ' + u.current;
    const b = $('#updBtn');
    b.classList.toggle('hidden', !u.available);
    if (u.available) b.textContent = 'Update to ' + u.latest;
    return u;
  } catch (e) { return null; }
}

$('#updBtn').addEventListener('click', async () => {
  const u = state.update;
  if (!u || !u.available) return;
  const ok = await ask({
    title: 'Update to ' + u.latest + '?',
    body: u.auto
      ? 'The update downloads, the app closes, and the new version opens by itself in a few seconds. Recording starts again on its own.'
      : 'This copy was not installed with the setup, so it cannot update itself. The releases page opens instead.',
    confirmLabel: u.auto ? 'Update now' : 'Open releases page',
  });
  if (!ok) return;
  try {
    const r = await post('/api/update/install', {});
    if (r.installing) toast('Installing ' + u.latest + '… the app reopens by itself.', 'ok');
  } catch (e) { toast(e.message, e.cancelled ? '' : 'err'); }
});

$('#updCheck').addEventListener('click', async () => {
  const msg = $('#updMsg');
  msg.textContent = 'Checking…';
  const u = await checkUpdate(true);
  if (!u) msg.textContent = 'Could not reach GitHub.';
  else if (u.available) msg.textContent = u.latest + ' is out - use the Update button at the top.';
  else msg.textContent = 'You have the latest version.';
});

// ---------- job bar ----------
async function pollJob() {
  try {
    const j = await api('/api/job');
    const bar = $('#jobBar');
    if (j.running) {
      bar.classList.remove('hidden');
      $('#jobText').textContent = j.label + ' — ' + Math.round(j.progress.percent || 0) + '%';
      bar.querySelector('.jobfill').style.width = (j.progress.percent || 0) + '%';
      // Only offered while there is something the server can actually stop, and
      // re-armed from the server's answer rather than from the last click - so
      // the button is never live for a job that has already been cancelled.
      const cb = $('#jobCancel');
      cb.classList.toggle('hidden', !j.cancellable);
      cb.disabled = !j.cancellable;
    } else bar.classList.add('hidden');
  } catch (e) { /* keep polling */ }
  setTimeout(pollJob, 700);
}

$('#jobCancel').addEventListener('click', async (ev) => {
  ev.currentTarget.disabled = true;
  try { await post('/api/job/cancel', {}); } catch (e) { toast(e.message, 'err'); }
});

boot().catch((e) => toast(e.message, 'err'));
