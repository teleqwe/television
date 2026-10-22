const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s) => document.querySelector(s);
const $$ = (s) => document.querySelectorAll(s);
const WIN = navigator.userAgent.includes('Windows');
const SEP = WIN ? '\\' : '/';

const SPEAKER = '<svg viewBox="0 0 16 16"><path d="M2.5 6v4h2.5l3.5 3V3L5 6z"/><path d="M11 5.5a3.5 3.5 0 0 1 0 5M12.8 3.8a6 6 0 0 1 0 8.4"/></svg>';
const MUTED = '<svg viewBox="0 0 16 16"><path d="M2.5 6v4h2.5l3.5 3V3L5 6z"/><path d="m11 6 4 4m0-4-4 4"/></svg>';
const REMOVE = '<svg viewBox="0 0 16 16"><path d="m4.5 4.5 7 7m0-7-7 7"/></svg>';
const INFO = '<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="6.3"/><path d="M8 7.3v3.7M8 5v.01"/></svg>';

let st = null; // current settings
let sources = []; // Windows only: on Linux the system's picker chooses
let last; // last status from the backend
let devices = { out: [], in: [] };
let audioApps = [];

const prettyKey = (hk) => hk.replace(/\bKey([A-Z])\b/g, '$1').replace(/\bDigit(\d)\b/g, '$1').replaceAll('+', ' + ');
const appName = (exe) => exe.replace(/\.exe$/i, '');

function shortPath(p) {
  const parts = p.split(SEP).filter(Boolean);
  return parts.length > 3 ? '…' + SEP + parts.slice(-2).join(SEP) : p;
}

// Rough size of the buffer: (video + audio bitrate) × clip length.
function memEstimate() {
  if (st.rate_control === 'cqp') return 'Size varies with quality';
  const srcs = st.desktop + st.mic + st.apps.length;
  const tracks = srcs + (st.mic + (st.desktop ? 1 : st.apps.length) >= 2 ? 1 : 0);
  const kbps = st.bitrate + tracks * st.audio_bitrate;
  const mb = kbps * 1000 / 8 * (st.clip_seconds + st.keyint + 1) / 1e6;
  const size = mb < 1000 ? Math.round(mb) + ' MB' : (mb / 1000).toFixed(1) + ' GB';
  return `≈ ${size} ${st.buffer_on_disk ? 'on disk' : 'of memory'}`;
}

function setOptions(sel, opts, value) {
  sel.replaceChildren(...opts.map(([v, label]) => new Option(label, v)));
  sel.value = value;
}

function renderSources() {
  const opts = [['auto', 'Automatic'], ...sources.map((s) => [s.id, s.name])];
  // A saved window that isn't listed as-is still shows, so the selection doesn't silently change.
  // Ids are "window:<hwnd>:<exe>"; if the app reopened, the backend follows it to its new window.
  if (st.source.startsWith('window:') && !sources.some((s) => s.id === st.source)) {
    const exe = st.source.split(':')[2];
    const same = exe && sources.find((s) => s.id.startsWith('window:') && s.id.split(':')[2] === exe);
    opts.push([st.source, same ? same.name : 'Closed window']);
  }
  // "monitor:" and unknown monitors mean the primary display, which is listed first.
  const known = opts.some(([v]) => v === st.source);
  setOptions($('#source'), opts, known ? st.source : sources[0]?.id);
}

function renderDevices() {
  for (const sel of $$('[data-devices]')) {
    const list = devices[sel.dataset.devices];
    const value = st[sel.dataset.k];
    const opts = [['', WIN ? 'Windows default' : 'System default'], ...list.map((d) => [d.id, d.name])];
    if (value && !list.some((d) => d.id === value)) opts.push([value, 'Unavailable device']);
    setOptions(sel, opts, value);
  }
}

function renderApps() {
  const rows = st.apps.map((a, i) => {
    const row = document.createElement('div');
    row.className = 'row';
    row.innerHTML = `<label><span class="app-name"></span><small>App track</small></label>
      <div class="ctl end gap"><button class="icon mute" title="Mute"></button><button class="icon remove" title="Remove track">${REMOVE}</button></div>`;
    row.querySelector('.app-name').textContent = appName(a.exe);
    const mute = row.querySelector('.mute');
    mute.classList.toggle('on', a.muted);
    mute.innerHTML = a.muted ? MUTED : SPEAKER;
    mute.title = a.muted ? 'Unmute' : 'Mute';
    mute.onclick = () => update({ apps: st.apps.map((x, j) => (j === i ? { ...x, muted: !x.muted } : x)) });
    row.querySelector('.remove').onclick = () => update({ apps: st.apps.filter((_, j) => j !== i) });
    return row;
  });
  $('#apps').replaceChildren(...rows);
  const added = new Set(st.apps.map((a) => a.exe.toLowerCase()));
  const free = audioApps.filter((exe) => !added.has(exe.toLowerCase()));
  const opts = [['', free.length ? 'Choose an app…' : 'No apps playing sound'], ...free.map((exe) => [exe, appName(exe)])];
  setOptions($('#app-pick'), opts, '');
}

function renderStatus(s) {
  // An error can be non-fatal (e.g. the hotkey is taken) while the buffer still runs.
  const err = s.error;
  const full = s.buffered >= st.clip_seconds;
  $('#dot').className = 'dot ' + (s.running ? 'on' : err ? 'err' : '');
  $('#status').textContent = s.running
    ? `Buffering · ${full ? '' : Math.floor(s.buffered) + ' / '}${st.clip_seconds} s · ${s.height}p${st.fps}`
    : err ? 'Not buffering' : 'Starting…';
  $('#pick').textContent = s.source || 'Choose…';
  $('#error').hidden = !err;
  $('#error').textContent = err || '';
}

function render(s) {
  st = s.settings;
  for (const e of $$('[data-k]')) {
    if (e.id === 'source' || 'devices' in e.dataset || (e === document.activeElement && e.type === 'number')) continue;
    if (e.type === 'checkbox') e.checked = st[e.dataset.k];
    else e.value = String(st[e.dataset.k]);
  }
  renderSources();
  renderDevices();
  renderApps();
  for (const row of $$('[data-show]')) {
    const [k, vals] = row.dataset.show.split('=');
    row.hidden = !vals.split(',').includes(String(st[k]));
  }
  for (const b of $$('[data-mute]')) {
    const on = st[b.dataset.mute];
    b.classList.toggle('on', on);
    b.innerHTML = on ? MUTED : SPEAKER;
    b.title = on ? 'Unmute' : 'Mute';
    b.disabled = !st[b.dataset.mute.replace('_muted', '')];
  }
  $('#hotkey').textContent = prettyKey(st.hotkey);
  $('#save-key').textContent = prettyKey(st.hotkey).replaceAll(' + ', '+');
  $('#folder').textContent = shortPath(st.folder);
  $('#folder').title = st.folder;
  $('#mem').textContent = memEstimate();
  renderStatus(s);
}

let toastTimer;
function toast(msg, isErr = false, action) {
  const t = $('#toast');
  const span = document.createElement('span');
  span.textContent = msg;
  t.replaceChildren(span);
  if (action) {
    const b = document.createElement('button');
    b.textContent = action.label;
    b.onclick = action.run;
    t.append(b);
  }
  t.className = 'toast' + (isErr ? ' err' : '');
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 4000);
}

let updates = 0;
async function update(patch) {
  // Apply locally first so a second change made before this one returns builds on it, and
  // only render the newest reply: an older one would roll settings back.
  st = { ...st, ...patch };
  const seq = ++updates;
  let s;
  try {
    s = await invoke('update_settings', { settings: st });
  } catch (e) {
    toast(String(e), true);
    s = await invoke('get_status');
  }
  if (seq === updates) render((last = s));
}

document.addEventListener('change', (e) => {
  const t = e.target;
  const k = t.dataset.k;
  if (!k) return;
  let v = t.value;
  if (t.type === 'checkbox') v = t.checked;
  else if (t.type === 'number') {
    if (t.value === '') return render(last);
    v = Math.min(Number(t.max), Math.max(Number(t.min), Math.round(Number(v))));
  } else if ('num' in t.dataset) v = Number(v);
  else if ('bool' in t.dataset) v = v === 'true';
  update({ [k]: v });
});

for (const b of $$('[data-mute]')) b.onclick = () => update({ [b.dataset.mute]: !st[b.dataset.mute] });

$('#app-pick').onchange = (e) => e.target.value && update({ apps: [...st.apps, { exe: e.target.value, muted: false }] });
$('#refresh-apps').onclick = async () => {
  audioApps = await invoke('list_audio_apps');
  renderApps();
};

$('#save').onclick = () => invoke('save_clip_now');
$('#open').onclick = () => invoke('open_folder');
$('#folder').onclick = async () => {
  const dir = await invoke('pick_folder');
  if (dir) update({ folder: dir });
};
$('#refresh').onclick = async () => {
  sources = await invoke('list_sources');
  renderSources();
};
$('#pick').onclick = async () => {
  $('#pick').textContent = 'Waiting for the dialog…';
  try {
    render((last = await invoke('pick_source')));
  } catch (e) {
    toast(String(e), true);
  }
};

for (const row of $$('[data-os]')) row.hidden = row.dataset.os !== (WIN ? 'windows' : 'linux');
if (WIN) $('label[for=autostart]').firstChild.textContent = 'Start with Windows';

// Info icons: one per row with a data-tip, shown in a floating tooltip on hover or focus.
const tip = $('#tip');
function showTip(btn) {
  tip.textContent = btn.closest('[data-tip]').dataset.tip;
  tip.hidden = false;
  const r = btn.getBoundingClientRect();
  const w = tip.offsetWidth, h = tip.offsetHeight;
  const left = Math.max(12, Math.min(r.left - 10, innerWidth - w - 12));
  const below = r.bottom + 8 + h < innerHeight;
  tip.style.left = left + 'px';
  tip.style.top = (below ? r.bottom + 8 : r.top - h - 8) + 'px';
}
for (const row of $$('[data-tip]')) {
  const label = row.querySelector('label');
  const name = document.createElement('span');
  name.className = 'name';
  name.append(label.firstChild.textContent.trim());
  label.firstChild.replaceWith(name);
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'info';
  btn.setAttribute('aria-label', 'About this setting');
  btn.innerHTML = INFO;
  btn.onclick = (e) => e.preventDefault(); // don't toggle the labelled control
  btn.onmouseenter = btn.onfocus = () => showTip(btn);
  btn.onmouseleave = btn.onblur = () => (tip.hidden = true);
  name.append(btn);
}
$('main').addEventListener('scroll', () => (tip.hidden = true));

// Hotkey capture: click the field, press a combination, Esc to cancel.
const hk = $('#hotkey');
let listening = false;
hk.onclick = () => {
  listening = true;
  hk.classList.add('listening');
  hk.textContent = 'Press keys…';
};
const stopListening = () => {
  listening = false;
  hk.classList.remove('listening');
  hk.textContent = prettyKey(st.hotkey);
};
hk.onblur = stopListening;
document.addEventListener('keydown', (e) => {
  if (!listening) return;
  e.preventDefault();
  if (e.code === 'Escape') return stopListening();
  const mods = [e.ctrlKey && 'Ctrl', e.altKey && 'Alt', e.shiftKey && 'Shift', e.metaKey && 'Super'].filter(Boolean);
  if (/^(Control|Alt|Shift|Meta|OS)(Left|Right)?$/.test(e.code)) {
    hk.textContent = mods.join(' + ') + ' + …';
    return;
  }
  // A plain key would be taken over everywhere (you couldn't type it), so only F-keys and
  // Pause/Print Screen work alone.
  if (!mods.length && !/^(F\d+|Pause|PrintScreen|ScrollLock)$/.test(e.code)) {
    hk.textContent = 'Add Ctrl, Alt or Shift…';
    return;
  }
  stopListening();
  update({ hotkey: [...mods, e.code].join('+') });
});

// Preview: polls the frame being recorded while the switch is on. Remembered per window.
const pv = $('#preview'), pvImg = $('#preview-img');
let pvTimer;
let pvBusy = false;
async function pollPreview() {
  if (document.hidden || pvBusy) return; // minimized, or the last frame is still on its way
  pvBusy = true;
  let buf = await invoke('get_preview').finally(() => (pvBusy = false));
  if (Array.isArray(buf)) buf = new Uint8Array(buf).buffer; // IPC fell back to JSON
  if (buf.byteLength < 8 || !pv.checked) return;
  const [w, h] = new Uint32Array(buf, 0, 2);
  pvImg.width = w;
  pvImg.height = h;
  pvImg.getContext('2d').putImageData(new ImageData(new Uint8ClampedArray(buf, 8), w, h), 0, 0);
  pvImg.hidden = false;
}
function setPreview(on) {
  pv.checked = on;
  clearInterval(pvTimer);
  pvImg.hidden = true;
  if (on) {
    pollPreview(); // asks for the first frame; it shows on the next poll
    pvTimer = setInterval(pollPreview, 500);
  }
  try { localStorage.setItem('preview', on ? '1' : ''); } catch {}
}
pv.onchange = () => setPreview(pv.checked);
try { setPreview(!!localStorage.getItem('preview')); } catch {}

// The global hotkey or tray can save while the window is open.
listen('clip', ({ payload }) => {
  if (payload.Ok) toast(`Saved ${payload.Ok.split(SEP).pop()}`, false, { label: 'Show', run: () => invoke('open_folder') });
  else toast(payload.Err, true);
});

(async () => {
  [last, sources, devices.out, devices.in, audioApps] = await Promise.all([
    invoke('get_status'), invoke('list_sources'),
    invoke('list_audio_devices', { mic: false }), invoke('list_audio_devices', { mic: true }), invoke('list_audio_apps'),
  ]);
  render(last);
  // Settings only change through update(); polling just refreshes the status line.
  let busy = false;
  setInterval(async () => {
    if (busy) return; // a restart holds the backend; don't pile up calls
    busy = true;
    try {
      renderStatus((last = await invoke('get_status')));
    } finally {
      busy = false;
    }
  }, 1000);
})();
