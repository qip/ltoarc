'use strict';
// The ltoarc web interface: a small page app over the daemon's JSON API (/api/...).

// ---------------------------------------------------------------- helpers

const $ = (sel, root = document) => root.querySelector(sel);

function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'style') Object.assign(el.style, v);
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'value') el.value = v;
    else if (k === 'checked') el.checked = !!v;
    else if (v === true) el.setAttribute(k, '');
    else el.setAttribute(k, v);
  }
  return add(el, kids);
}

function add(el, kids) {
  for (const k of kids.flat(Infinity)) {
    if (k == null || k === false) continue;
    el.append(k instanceof Node ? k : String(k));
  }
  return el;
}

const enc = encodeURIComponent;
const now = () => Date.now() / 1000;
const num = n => (n == null ? '–' : Number(n).toLocaleString('en-US'));

function bytes(n) {
  if (n == null) return '–';
  const u = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let v = Number(n), i = 0;
  while (v >= 1000 && i < u.length - 1) { v /= 1000; i++; }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${u[i]}`;
}

function when(t) {
  if (!t) return '–';
  const d = new Date(t * 1000), p = n => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function ago(t) {
  if (!t) return '–';
  const s = now() - t, f = Math.abs(s);
  const txt = f < 60 ? 'moments' : f < 3600 ? `${Math.round(f / 60)} min` : f < 172800 ? `${Math.round(f / 3600)} h` : `${Math.round(f / 86400)} days`;
  if (txt === 'moments') return s >= 0 ? 'just now' : 'in a moment';
  return s >= 0 ? `${txt} ago` : `in ${txt}`;
}

function dur(s) {
  if (s == null || s < 0) return '–';
  s = Math.round(s);
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min ${String(s % 60).padStart(2, '0')} s`;
  return `${Math.floor(s / 3600)} h ${String(Math.round((s % 3600) / 60)).padStart(2, '0')} min`;
}

// A time: absolute, or relative with the absolute one on hover.
const stamp = t => h('span', { class: 'nowrap', title: ago(t) }, when(t));
const rel = t => h('span', { class: 'nowrap', title: when(t) }, ago(t));

const TONE = {
  ok: 'ok', complete: 'ok', running: 'info', queued: 'info', partial: 'warn', failed: 'bad', expired: 'warn',
  filling: 'accent', scratch: 'ok', full: '', new: '', blank: '', retiring: 'warn', retired: 'bad', foreign: '',
  cleaning: 'info', attached: 'ok', unattached: 'warn', offline: 'bad', busy: 'info', idle: '',
};
const chip = (text, tone) => h('span', { class: `chip ${tone ?? TONE[text] ?? ''}` }, text);

const btn = (label, onclick, cls = '') => h('button', { class: `btn ${cls}`, type: 'button', onclick }, label);
const empty = text => h('div', { class: 'empty' }, text);
const errorBox = e => h('div', { class: 'error' }, String(e.message || e));
const card = (title, ...body) => h('div', { class: 'card' }, title ? h('div', { class: 'pad', style: { paddingBottom: '0' } }, h('h2', null, title)) : null, body);

function pageHead(title, sub, actions, crumbs) {
  return h('div', { class: 'head' },
    h('div', null, crumbs ? h('div', { class: 'crumbs' }, crumbs) : null, h('h1', null, title), sub ? h('div', { class: 'sub' }, sub) : null),
    actions && actions.length ? h('div', { class: 'actions' }, actions) : null);
}

function section(title, body, extra) {
  return h('div', { class: 'section' }, h('div', { class: 'bar-title' }, h('h2', null, title), extra), body);
}

function facts(pairs) {
  return h('dl', { class: 'facts' }, pairs.filter(Boolean).map(([k, v]) => h('div', null, h('dt', null, k), h('dd', null, v ?? '–'))));
}

function fillbar(used, cap, label) {
  if (!cap) return h('span', { class: 'muted' }, '–');
  const f = Math.max(0, Math.min(1, used / cap));
  return h('div', { class: `fill ${f > 0.97 ? 'full' : f > 0.85 ? 'hi' : ''}` },
    h('div', { class: 'track' }, h('i', { style: { width: `${(f * 100).toFixed(1)}%` } })),
    h('span', null, label ?? `${bytes(used)} of ${bytes(cap)}`));
}

// Used space of a cartridge, from its capacity and room.
const used = v => (v.capacity && v.room != null ? Math.max(0, v.capacity - v.room - reserve(v.capacity)) : null);
const reserve = cap => cap / 100 + 8 * 2 ** 20;

const gen = g => (g ? h('span', { class: 'gen' }, g.replace('LTO-', 'L')) : null);
const tapeLink = bc => h('a', { class: 'mono', href: `#/cartridge/${enc(bc)}` }, bc);
const sessLink = (key, label) => h('a', { class: 'mono', href: `#/session/${enc(key)}` }, label ?? key);

function cmp(a, b) {
  if (a == null && b == null) return 0;
  if (a == null) return 1;
  if (b == null) return -1;
  return typeof a === 'number' && typeof b === 'number' ? a - b : String(a).localeCompare(String(b), 'en', { numeric: true });
}

// A table: cols are {t: title, f: row -> cell, key?: row -> sort value, num?: bool}.
function table(cols, rows, opts = {}) {
  cols = cols.filter(Boolean);
  if (!rows.length) return empty(opts.empty ?? 'Nothing here.');
  let sortBy = opts.sort ?? null, dir = opts.desc ? -1 : 1;
  const tbody = h('tbody');
  const heads = cols.map((c, i) => h('th', {
    class: [c.num && 'num', c.key && 'sortable'].filter(Boolean).join(' ') || null,
    onclick: c.key ? () => { dir = sortBy === i ? -dir : (c.desc ? -1 : 1); sortBy = i; draw(); } : null,
  }, c.t, h('span', { class: 'arrow' })));
  function draw() {
    const list = sortBy == null ? rows : [...rows].sort((a, b) => cmp(cols[sortBy].key(a), cols[sortBy].key(b)) * dir);
    heads.forEach((th, i) => { th.lastChild.textContent = i === sortBy ? (dir > 0 ? '▲' : '▼') : ''; });
    tbody.replaceChildren(...list.map(r => h('tr', {
      class: [opts.click && 'click', opts.selected && opts.selected(r) && 'sel'].filter(Boolean).join(' ') || null,
      onclick: opts.click ? e => { if (!e.target.closest('a,button,input,label')) opts.click(r, e); } : null,
    }, cols.map(c => h('td', { class: c.num ? 'num' : c.cls || null }, c.f(r))))));
  }
  draw();
  return h('div', { class: 'tablewrap' }, h('table', null, h('thead', null, h('tr', null, heads)), tbody));
}

function toast(msg, tone, link) {
  const t = h('div', { class: `toast ${tone || ''}` }, msg, link ? [' ', link] : null);
  $('#toasts').append(t);
  setTimeout(() => t.remove(), tone === 'bad' ? 9000 : 5000);
}

// ---------------------------------------------------------------- API

async function api(path, body) {
  const opt = body === undefined ? {} : {
    method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Ltoarc': '1' }, body: JSON.stringify(body),
  };
  const r = await fetch(`/api/${path}`, opt);
  if (r.status === 401 && path !== 'login') {
    showLogin();
    throw new Error('Log in first.');
  }
  const j = await r.json().catch(() => ({ error: r.statusText }));
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}

async function startTask(body) {
  try {
    const r = await api('task', body);
    toast(`Started: ${r.title}`);
    location.hash = `#/run/${r.run}`;
  } catch (e) {
    toast(e.message, 'bad');
  }
}

function setBusy(running) {
  $('#busy').hidden = !(running || []).some(r => r.kind !== 'queued');
}

// ---------------------------------------------------------------- dialogs

// A modal with buttons [{label, cls, value, primary}]; resolves with the value of the button
// pressed (null when dismissed). `check` may refuse to close (returns an error text).
function modal(title, body, buttons, check) {
  return new Promise(resolve => {
    const err = h('div', { class: 'note-box bad', hidden: true });
    const d = h('dialog', null,
      h('div', { class: 'dh' }, h('h2', null, title)),
      h('div', { class: 'db' }, h('div', { class: 'form' }, body, err)),
      h('div', { class: 'df' }, buttons.map(b => btn(b.label, () => {
        if (b.value != null && check) {
          const msg = check(b.value);
          if (msg) { err.textContent = msg; err.hidden = false; return; }
        }
        d.close(); resolve(b.value ?? null);
      }, b.cls || ''))));
    d.addEventListener('cancel', () => resolve(null));
    d.addEventListener('close', () => d.remove());
    document.body.append(d);
    d.showModal();
    const first = d.querySelector('input,select,textarea');
    if (first) first.focus();
  });
}

async function confirmTask(body, title, text, label = 'Start') {
  const ok = await modal(title, h('p', { style: { margin: 0 } }, text), [{ label: 'Cancel' }, { label, cls: 'primary', value: true }]);
  if (ok) startTask(body);
}

function driveSelect(drives, anyLabel) {
  return h('select', null, anyLabel ? h('option', { value: '' }, anyLabel) : null, drives.map(d => h('option', { value: d }, d)));
}

async function verifyDialog(bc, drives) {
  const sel = driveSelect(drives, 'Any drive that can read it');
  const ok = await modal(`Verify ${bc}`, [
    h('p', { style: { margin: 0 } }, 'Reads the cartridge back and checks every chunk and file hash against the catalog. The drive is paced to the checks, so it should not stop.'),
    h('label', { class: 'field' }, 'Drive', sel),
  ], [{ label: 'Cancel' }, { label: 'Verify', cls: 'primary', value: true }]);
  if (ok) startTask({ kind: 'verify', barcode: bc, drive: sel.value || null });
}

async function loadDialog(bc, drives) {
  const sel = driveSelect(drives, 'Any free drive that can read it');
  const ok = await modal(`Load ${bc}`, [h('label', { class: 'field' }, 'Drive', sel)], [{ label: 'Cancel' }, { label: 'Load', cls: 'primary', value: true }]);
  if (ok) startTask({ kind: 'load', barcode: bc, drive: sel.value || null });
}

async function stopRun(r) {
  const ok = await modal(`Stop ${r.title}?`, [
    h('p', { style: { margin: 0 } }, 'It is terminated together with everything it started.'),
    h('div', { class: 'note-box warn' }, 'A backup stopped half-way leaves its cartridge open; the next use of that cartridge recovers it, and the session stays partial. Post hooks of a job (e.g. destroying a snapshot) do not run.'),
  ], [{ label: 'Keep running' }, { label: 'Stop', cls: 'primary', value: true }]);
  if (!ok) return;
  try { await api('stop', { run: r.run }); toast(`Stopping ${r.title}`); } catch (e) { toast(e.message, 'bad'); }
}

async function wearDialog() {
  const retire = h('input', { type: 'checkbox' }), clean = h('input', { type: 'checkbox' });
  const ok = await modal('Check wear', [
    h('p', { style: { margin: 0 } }, 'Compares drive and cartridge statistics with the [wear] thresholds: drives that need cleaning, cartridges to retire, and whether errors follow a cartridge or a drive.'),
    h('label', { class: 'check' }, retire, h('span', null, 'Mark cartridges past their limits as retiring', h('br'), h('small', { class: 'muted' }, 'still read, never written again'))),
    h('label', { class: 'check' }, clean, h('span', null, 'Clean drives that need it', h('br'), h('small', { class: 'muted' }, 'with a cleaning cartridge from their library'))),
  ], [{ label: 'Cancel' }, { label: 'Check', cls: 'primary', value: true }]);
  if (ok) startTask({ kind: 'check', retire: retire.checked, clean: clean.checked });
}

async function backupDialog(prefill = {}) {
  const sources = h('textarea', { rows: 3, placeholder: '/tank/data' }, prefill.sources || '');
  const tape = h('input', { type: 'text', placeholder: 'DC8001L8..', value: prefill.tape || '' });
  const tape2 = h('input', { type: 'text', placeholder: 'optional, e.g. DC5001L5..' });
  const job = h('input', { type: 'text', placeholder: 'optional' });
  const level = h('select', null, ['full', 'incremental', 'differential', 'append'].map(l => h('option', { value: l }, l)));
  const plan = h('input', { type: 'checkbox', checked: true });
  const resume = h('input', { type: 'checkbox' });
  const del = h('input', { type: 'checkbox' });
  const over = h('input', { type: 'checkbox' });
  const confirmWord = h('input', { type: 'text', placeholder: 'type overwrite' });
  const overNote = h('div', { class: 'note-box warn', hidden: true },
    'Each cartridge the session reaches is relabelled and what it held is forgotten. Run the plan first: it lists what each would lose. ',
    h('label', { class: 'field', style: { marginTop: '8px' } }, 'To overwrite, type "overwrite"', confirmWord));
  const sync = () => { overNote.hidden = !over.checked; };
  over.addEventListener('change', sync);
  const v = await modal('Back up now', [
    h('label', { class: 'field' }, 'Sources', h('small', null, 'one absolute path per line'), sources),
    h('div', { class: 'row' }, h('label', { class: 'field' }, 'Cartridges', tape), h('label', { class: 'field' }, 'Second copy', tape2)),
    h('div', { class: 'row' }, h('label', { class: 'field' }, 'Job', h('small', null, 'needed for levels other than full'), job), h('label', { class: 'field' }, 'Level', level)),
    h('label', { class: 'check' }, plan, h('span', null, 'Only show the plan', h('br'), h('small', { class: 'muted' }, 'scans the sources and lists which cartridges it would write to; writes nothing'))),
    h('label', { class: 'check' }, resume, h('span', null, 'Resume', h('br'), h('small', { class: 'muted' }, 'continue the last session of this job (or these sources) that ended partial: what it holds is not saved again'))),
    h('label', { class: 'check' }, del, h('span', null, 'Delete the source files once the session is complete', h('br'), h('small', { class: 'muted' }, 'files changed since they were read stay'))),
    h('label', { class: 'check' }, over, h('span', null, 'Overwrite the cartridges instead of appending')),
    overNote,
  ], [{ label: 'Cancel' }, { label: 'Start', cls: 'primary', value: true }], () => {
    if (!sources.value.trim()) return 'Give at least one source.';
    if (!tape.value.trim()) return 'Give the cartridges to write.';
    if (over.checked && !plan.checked && confirmWord.value.trim() !== 'overwrite') return 'Type "overwrite" to confirm overwriting.';
    return null;
  });
  if (!v) return;
  startTask({
    kind: 'backup', sources: sources.value.split('\n').map(s => s.trim()).filter(Boolean), tape: tape.value, tape2: tape2.value,
    job: job.value, level: level.value, dry_run: plan.checked, overwrite: over.checked, resume: resume.checked, delete_source: del.checked,
  });
}

// Whether a session is offered ticked for copying: complete ones, and partial ones holding
// something no later complete session has.
const worthCopying = s => s.status === 'complete' || (s.status === 'partial' && s.only_here > 0);

function copyNote(s) {
  if (s.status === 'expired') return 'expired';
  if (s.status !== 'partial' || s.only_here == null) return null;
  return s.only_here === 0 ? 'partial; later sessions hold all of it' : `partial; ${num(s.only_here)} entries only here`;
}

// Another copy of sessions (tape to tape, or from the source disks), choosing which.
async function copyDialog(sessions, preselect, attached) {
  const picked = new Set(sessions.filter(preselect).map(x => x.uuid));
  const total = h('div', { class: 'muted', style: { fontSize: '13px' } });
  const sumUp = () => {
    const chosen = sessions.filter(x => picked.has(x.uuid));
    total.textContent = `${chosen.length} of ${sessions.length} session${sessions.length === 1 ? '' : 's'} chosen, ${bytes(chosen.reduce((a, x) => a + x.stored, 0))} to copy`;
  };
  const rows = sessions.map(x => {
    const box = h('input', { type: 'checkbox', checked: picked.has(x.uuid), onchange: e => { e.target.checked ? picked.add(x.uuid) : picked.delete(x.uuid); sumUp(); } });
    const note = copyNote(x);
    return h('label', { class: 'check', style: { padding: '6px 0', borderBottom: '1px solid var(--border)' } }, box,
      h('span', { style: { flex: '1', minWidth: '0' } },
        h('span', { class: 'mono' }, x.short), ' ', x.job ? `${x.job} · ` : '', `${x.level} · ${when(x.started)} · ${bytes(x.stored)}`, ' ', chip(x.status),
        h('br'), h('small', { class: 'muted' }, x.tapes.map(t => `${t.barcode}${x.copies.length > 1 ? ` (copy ${t.copy})` : ''}`).join(', '), note ? ` · ${note}` : '')));
  });
  sumUp();
  const to = h('input', { type: 'text', placeholder: 'DC5001L5..' });
  const fromTape = h('input', { type: 'radio', name: 'copyfrom', checked: true });
  const fromSource = h('input', { type: 'radio', name: 'copyfrom' });
  const plan = h('input', { type: 'checkbox', checked: true });
  const over = h('input', { type: 'checkbox' });
  const word = h('input', { type: 'text', placeholder: 'type overwrite' });
  const overNote = h('div', { class: 'note-box warn', hidden: true },
    'Each destination cartridge is relabelled and what it held is forgotten. ',
    h('label', { class: 'field', style: { marginTop: '8px' } }, 'To overwrite, type "overwrite"', word));
  over.addEventListener('change', () => { overNote.hidden = !over.checked; });
  const v = await modal('Copy sessions', [
    h('div', { style: { maxHeight: '260px', overflow: 'auto' } }, rows),
    total,
    h('label', { class: 'field' }, 'Cartridges for the new copy', h('small', null, 'a barcode, a list or a range; any generation, any library'), to),
    h('div', { class: 'field', style: { gap: '6px' } }, 'Read from',
      h('label', { class: 'check' }, fromTape, h('span', null, 'The cartridges holding them', h('br'), h('small', { class: 'muted' }, 'records are copied as they are, checked on the way; needs a second drive'))),
      h('label', { class: 'check' }, fromSource, h('span', null, 'The source disks', h('br'), h('small', { class: 'muted' }, 'files are read again and checked against the session; files changed since are left out')))),
    attached < 2 ? h('div', { class: 'note-box' }, `This host has ${attached} drive${attached === 1 ? '' : 's'} attached: copying tape to tape needs two. The plan works without.`) : null,
    h('label', { class: 'check' }, plan, h('span', null, 'Only show the plan', h('br'), h('small', { class: 'muted' }, 'which cartridges the copy would use; writes nothing'))),
    h('label', { class: 'check' }, over, h('span', null, 'Overwrite the destination cartridges instead of appending')),
    overNote,
  ], [{ label: 'Cancel' }, { label: 'Start', cls: 'primary', value: true }], () => {
    if (!picked.size) return 'Choose at least one session.';
    if (!to.value.trim()) return 'Give the cartridges for the new copy.';
    if (over.checked && !plan.checked && word.value.trim() !== 'overwrite') return 'Type "overwrite" to confirm overwriting.';
    return null;
  });
  if (!v) return;
  startTask({
    kind: 'copy', sessions: sessions.filter(x => picked.has(x.uuid)).map(x => x.uuid), to: to.value.trim(),
    from_source: fromSource.checked, dry_run: plan.checked, overwrite: over.checked,
  });
}

async function copyFromCartridge(bc) {
  try {
    const d = await api('sessions');
    const on = d.sessions.filter(x => x.tapes.some(t => t.barcode === bc));
    if (!on.length) { toast(`${bc} holds no sessions.`, 'bad'); return; }
    copyDialog(on, worthCopying, d.attached_drives);
  } catch (e) {
    toast(e.message, 'bad');
  }
}

async function restoreDialog(paths, view) {
  const to = h('input', { type: 'text', placeholder: '/restore' });
  const parent = commonParent(paths);
  const keep = h('input', { type: 'radio', name: 'layout', checked: true });
  const strip = h('input', { type: 'radio', name: 'layout' });
  const copy = h('select', null, h('option', { value: '' }, 'Any copy'), h('option', { value: '1' }, 'Copy 1'), h('option', { value: '2' }, 'Copy 2'));
  const over = h('input', { type: 'checkbox' });
  const what = paths.length === 1 ? paths[0] : `${paths.length} paths`;
  const v = await modal(`Restore ${what}`, [
    paths.length > 1 ? h('div', { class: 'note-box mono', style: { maxHeight: '110px', overflow: 'auto' } }, paths.map(p => h('div', null, p))) : null,
    view.as_of || view.session ? h('div', { class: 'note-box' }, 'As the archive was ', view.as_of ? `at ${view.as_of.replace('T', ' ')}` : `in session ${view.session}`, '.') : null,
    h('label', { class: 'field' }, 'Destination directory', h('small', null, 'on the host running ltoarcd'), to),
    h('label', { class: 'check' }, keep, h('span', null, 'Keep full paths', h('br'), h('small', { class: 'muted mono' }, `${to.placeholder}${paths[0]}`))),
    parent !== '/' ? h('label', { class: 'check' }, strip, h('span', null, `Relative to ${parent}`, h('br'), h('small', { class: 'muted mono' }, `${to.placeholder}${paths[0].slice(parent.length)}`))) : null,
    h('div', { class: 'row' }, h('label', { class: 'field' }, 'Read from', copy), h('span')),
    h('label', { class: 'check' }, over, h('span', null, 'Replace files that already exist there')),
  ], [{ label: 'Cancel' }, { label: 'Preview', value: 'plan' }, { label: 'Restore', cls: 'primary', value: 'go' }],
  () => (to.value.trim().startsWith('/') ? null : 'Give an absolute destination directory.'));
  if (!v) return;
  startTask({
    kind: 'restore', paths, to: to.value.trim(), strip_prefix: strip.checked ? parent : null, copy: copy.value ? Number(copy.value) : null,
    overwrite: over.checked, dry_run: v === 'plan', as_of: view.as_of || null, session: view.session || null,
  });
}

function commonParent(paths) {
  const parts = paths.map(p => p.split('/').slice(0, -1));
  let n = Math.min(...parts.map(p => p.length));
  for (let i = 0; i < n; i++) if (parts.some(p => p[i] !== parts[0][i])) { n = i; break; }
  return parts[0].slice(0, n).join('/') || '/';
}

// ---------------------------------------------------------------- shared pieces

function driveCard(d) {
  const state = d.state !== 'attached' ? chip(d.state) : d.holder ? chip('busy') : chip('idle');
  const since = d.stats && d.stats.data && d.stats.data.head_hours_since_cleaning;
  return h('div', { class: 'card drive' },
    h('div', { class: 'top' },
      h('div', null, h('a', { class: 'name', href: `#/drive/${enc(d.name)}` }, d.name),
        h('div', { class: 'model' }, [d.vendor, d.product].filter(Boolean).join(' '), d.generation ? ` · ${d.generation}` : '', d.bay ? ` · ${d.library} ${d.bay}` : '')),
      state),
    h('div', { class: 'slotline' }, d.loaded ? [h('span', { class: 'muted' }, 'Holds'), tapeLink(d.loaded)] : h('span', { class: 'muted' }, 'No cartridge')),
    h('div', { class: 'who' }, d.holder ? [h('span', { class: 'muted' }, 'In use by '), d.holder] : h('span', { class: 'muted' }, 'Not in use')),
    d.waiting.length ? h('div', { class: 'who' }, h('span', { class: 'muted' }, 'Waiting: '), d.waiting.map(w => `${w.what} (pid ${w.pid}, ${ago(w.since)})`).join('; ')) : null,
    h('div', { class: 'foot' },
      h('span', { class: 'muted', style: { fontSize: '12.5px' } }, since != null ? `${num(since)} head hour${since === 1 ? '' : 's'} since cleaning` : ''),
      h('span', { class: 'chips' },
        d.needs_cleaning ? chip('needs cleaning', 'warn') : null,
        d.loaded && !d.holder && d.state === 'attached'
          ? btn('Unload', () => confirmTask({ kind: 'unload', drive: d.name }, `Unload ${d.loaded}?`, `${d.name} rewinds and unloads it, and the library puts it back in its slot.`, 'Unload'), 'small')
          : null)));
}

function runningList(items) {
  if (!items.length) return h('div', { class: 'card' }, empty('Nothing running.'));
  return h('div', { class: 'card' }, items.map(r => h('div', { class: 'run' },
    h('div', { class: 'what' }, h('span', { class: 'clip', title: r.title, style: { maxWidth: '70ch' } }, r.run ? h('a', { href: `#/run/${r.run}` }, r.title) : r.title)),
    h('div', { class: 'right' },
      r.since ? h('span', { class: 'muted nowrap' }, dur(now() - r.since)) : chip('queued'),
      r.run ? btn('Stop', () => stopRun(r), 'small danger') : null),
    r.progress ? h('div', { class: 'prog', title: r.progress }, r.progress) : null)));
}

function runName(r) {
  return r.job.startsWith('@') ? h('span', null, h('span', { class: 'clip', title: r.run }, r.run), ' ', chip('task')) : h('span', null, r.job);
}

function runsTable(runs, compact) {
  return table([
    { t: 'Run', f: r => h('a', { href: `#/run/${r.id}` }, runName(r)) },
    { t: 'Started', f: r => (compact ? rel(r.started) : stamp(r.started)), key: r => r.started, desc: true },
    { t: 'Took', f: r => dur((r.ended || now()) - r.started), num: true },
    { t: 'Result', f: r => chip(r.status) },
    !compact && { t: 'Summary', f: r => h('span', { class: 'muted' }, r.summary || '') },
  ], runs, { empty: 'No runs yet.' });
}

// "matched 151 MB/s (149-157) · 0 pauses (0.0 s) · 1 cycle"
function writerText(w) {
  const parts = [];
  if (w.matched_mbps != null) parts.push(`matched ${w.matched_mbps.toFixed(0)} MB/s (${w.matched_min_mbps.toFixed(0)}–${w.matched_max_mbps.toFixed(0)})`);
  else parts.push('full speed');
  if (w.pauses != null) parts.push(`${w.pauses} pause${w.pauses === 1 ? '' : 's'}${w.pauses ? ` (${w.paused_seconds.toFixed(1)} s stopped)` : ''}`);
  if (w.cycles != null) parts.push(`${w.cycles} cycle${w.cycles === 1 ? '' : 's'}`);
  return parts.join(' · ');
}

function writersShort(ws) {
  if (!ws || !ws.length) return h('span', { class: 'faint' }, '–');
  return h('span', { class: 'nowrap' }, ws.map(w => {
    const rate = w.matched_mbps != null ? `${w.matched_mbps.toFixed(0)} MB/s` : 'full speed';
    return w.pauses != null ? `${rate}, ${w.pauses} pause${w.pauses === 1 ? '' : 's'}` : rate;
  }).join(' / '));
}

function sessionsTable(list, compact, sel) {
  return table([
    sel && {
      t: '',
      f: s => h('input', { type: 'checkbox', 'aria-label': `select ${s.short}`, checked: sel.has(s.uuid),
        onchange: e => { e.target.checked ? sel.add(s.uuid) : sel.delete(s.uuid); e.target.closest('tr').classList.toggle('sel', e.target.checked); sel.changed(); } }),
    },
    { t: 'Session', f: s => sessLink(s.short), key: s => s.short },
    { t: 'Job', f: s => s.job || h('span', { class: 'faint' }, '–'), key: s => s.job },
    { t: 'Level', f: s => s.level, key: s => s.level },
    { t: 'Started', f: s => (compact ? rel(s.started) : stamp(s.started)), key: s => s.started, desc: true },
    !compact && { t: 'Took', f: s => (s.ended ? dur(s.ended - s.started) : '–'), num: true, key: s => (s.ended || 0) - s.started },
    { t: 'Status', f: s => [chip(s.status), !compact && s.only_here === 0 ? h('div', { class: 'muted', style: { fontSize: '12px' } }, 'covered by later sessions') : null], key: s => s.status },
    !compact && { t: 'Files', f: s => num(s.files), num: true, key: s => s.files },
    { t: 'Data', f: s => bytes(s.bytes), num: true, key: s => s.bytes },
    !compact && { t: 'Cartridges', f: s => h('span', { class: 'chips' }, s.tapes.map(t => h('a', { class: 'chip mono', href: `#/cartridge/${enc(t.barcode)}`, title: `copy ${t.copy}` }, t.barcode, s.copies.length > 1 ? h('span', { class: 'faint' }, `·${t.copy}`) : null))) },
    !compact && { t: 'Drive', f: s => writersShort(s.writers) },
  ], list, { empty: 'No sessions yet.', click: s => { location.hash = `#/session/${s.short}`; }, selected: sel ? s => sel.has(s.uuid) : null });
}

function volumesTable(list) {
  return table([
    { t: 'Cartridge', f: v => tapeLink(v.barcode), key: v => v.barcode },
    { t: 'Gen', f: v => gen(v.generation), key: v => v.generation },
    { t: 'State', f: v => chip(v.kind === 'cleaning' ? 'cleaning' : v.state), key: v => v.state },
    { t: 'Pool', f: v => v.pool || h('span', { class: 'faint' }, '–'), key: v => v.pool },
    { t: 'Where', f: v => (v.place ? `${v.library} ${v.place}` : h('span', { class: 'faint' }, 'not in a library')), key: v => `${v.library} ${v.place}` },
    { t: 'Used', f: v => (v.kind === 'cleaning' ? '' : fillbar(used(v), v.capacity)), key: v => used(v) },
    { t: 'Sessions', f: v => num(v.sessions), num: true, key: v => v.sessions },
    { t: 'Written', f: v => (v.last_written ? rel(v.last_written) : h('span', { class: 'faint' }, 'never')), key: v => v.last_written },
    { t: 'Verified', f: v => (v.verified_at ? rel(v.verified_at) : h('span', { class: 'faint' }, 'never')), key: v => v.verified_at },
    { t: 'Life', f: v => (v.life_percent != null ? `${v.life_percent}%` : '–'), num: true, key: v => v.life_percent },
  ], list, { empty: 'No cartridges registered. Read the library inventory first.', sort: 0, click: v => { location.hash = `#/cartridge/${enc(v.barcode)}`; } });
}

function eventsList(events) {
  return table([
    { t: 'When', f: e => rel(e.at) },
    { t: 'What', f: e => chip(e.kind, e.kind === 'request' ? 'warn' : '') },
    { t: 'Subject', f: e => (e.kind === 'tape' || e.kind === 'request' ? tapeLink(e.subject) : e.subject) },
    { t: 'Message', f: e => e.message },
  ], events, { empty: 'No events.' });
}

function filterBox(placeholder, onInput) {
  const i = h('input', { type: 'search', placeholder, oninput: () => onInput(i.value.trim().toLowerCase()) });
  return i;
}

// ---------------------------------------------------------------- pages

async function pOverview() {
  const d = await api('overview');
  $('#host').textContent = d.host;
  setBusy(d.running);
  const dm = d.daemon, st = d.cartridges.states;
  return h('div', null,
    pageHead('Overview', `${d.host} · ltoarc ${d.version}`, [btn('Back up…', () => backupDialog())]),
    h('div', { class: 'grid tiles' },
      tile('Archived', bytes(d.sessions.bytes), `${num(d.sessions.files)} files · ${bytes(d.sessions.stored)} on tape`),
      tile('Sessions', num(d.sessions.count), 'not expired'),
      tile('Cartridges', num(d.cartridges.count), Object.entries(st).map(([k, v]) => `${v} ${k}`).join(' · ') || 'none registered'),
      tile('Scheduler', `${dm.jobs} job${dm.jobs === 1 ? '' : 's'}`,
        dm.stopping ? 'stopping after the running jobs' : dm.next ? h('span', null, 'next: ', h('a', { href: `#/job/${enc(dm.next.job)}` }, dm.next.job), ` ${ago(dm.next.at)}`) : 'nothing scheduled')),
    section('Drives', d.drives.length ? h('div', { class: 'grid drives' }, d.drives.map(driveCard)) : h('div', { class: 'card' }, empty('No drives registered. Run ltoarc hw scan.'))),
    section('Running', runningList(d.running), h('a', { href: '#/activity' }, 'All activity')),
    h('div', { class: 'grid two section' },
      card('Recent runs', runsTable(d.runs, true)),
      card('Recent sessions', sessionsTable(d.recent_sessions, true))),
    section('Events', h('div', { class: 'card' }, eventsList(d.events))));
}
pOverview.refresh = 5000;

function tile(label, value, note) {
  return h('div', { class: 'card tile' }, h('div', { class: 'label' }, label), h('div', { class: 'value' }, value), h('div', { class: 'note' }, note));
}

async function pLibrary() {
  const d = await api('library');
  return h('div', null,
    pageHead('Library', 'Where each cartridge is; drag cartridges to move them', [btn('Read inventory', () => confirmTask({ kind: 'inventory' }, 'Read the library inventories?', 'Reads which cartridges are in which slots and drives, and records new barcodes. Nothing moves.', 'Read'))]),
    d.libraries.length ? d.libraries.map(libraryCard) : h('div', { class: 'card' }, empty('No libraries registered. Run ltoarc hw scan.')),
    d.outside.length ? section('Not in a library', h('div', { class: 'card' }, volumesTable(d.outside))) : null);
}

const placeKey = e => `${e.kind}:${e.n}`;

// A library as it looks (magazines left and right of the drives, front at the outer edges),
// or as a grid when its layout is not known. Cartridges dragged between slots make a plan of
// moves, run as one task; one dropped on a drive is loaded, one dragged out of a drive is
// unloaded into that slot.
function libraryCard(l) {
  const els = new Map(l.elements.map(e => [placeKey(e), e]));
  const start = new Map(l.elements.filter(e => e.kind !== 'drive' && e.volume).map(e => [placeKey(e), e.volume]));
  const home = new Map([...start].map(([k, v]) => [v.barcode, k]));
  let at = new Map(start);
  let dragging = null;
  const map = h('div');
  const bar = h('div', { class: 'planbar', hidden: true });
  const pending = () => [...at].filter(([k, v]) => home.get(v.barcode) !== k).map(([k, v]) => ({ v, from: home.get(v.barcode), to: k }));
  const placeArg = k => { const e = els.get(k); return e.kind === 'ie' ? `ie${e.n}` : String(e.n); };
  const usable = e => e.kind !== 'drive' || (e.drive && e.attached);

  // What a drop of the cartridge at `from` on `to` would do: null when it cannot go there.
  function dropping(from, to) {
    const src = els.get(from), dst = els.get(to);
    if (from === to || !usable(src) || !usable(dst)) return null;
    if (src.kind === 'drive') return dst.kind === 'slot' && !at.get(to) ? 'unload' : null;
    if (dst.kind === 'drive') return 'load';
    return 'move';
  }

  function drop(from, to) {
    const how = dropping(from, to);
    if (!how) return;
    const src = els.get(from), dst = els.get(to);
    if (how === 'move') {
      const a = at.get(from), b = at.get(to);
      at.delete(from);
      at.delete(to);
      at.set(to, a);
      if (b) at.set(from, b);
      draw();
      return;
    }
    if (pending().length) { toast('Move or discard the planned moves first.', 'bad'); return; }
    if (how === 'unload') {
      const bc = src.volume.barcode;
      confirmTask({ kind: 'unload', drive: src.drive, slot: dst.n }, `Unload ${bc} to ${dst.name}?`, `${src.drive} rewinds and unloads it, and the library puts it in ${dst.name}.`, 'Unload');
    } else {
      const v = at.get(from);
      confirmTask({ kind: 'load', barcode: v.barcode, drive: dst.drive }, `Load ${v.barcode} into ${dst.drive}?`,
        dst.volume ? `${dst.drive} first unloads ${dst.volume.barcode}, which goes back to its slot.` : 'The library moves it into the drive, which loads it.', 'Load');
    }
  }

  function place(k) {
    const e = els.get(k);
    const v = e.kind === 'drive' ? e.volume : at.get(k);
    const from = v && e.kind !== 'drive' && home.get(v.barcode) !== k ? els.get(home.get(v.barcode)).name : null;
    const t = slotTile(e, v, { rack: !!l.layout, from });
    if (v && usable(e)) {
      t.draggable = true;
      t.addEventListener('dragstart', ev => { dragging = k; ev.dataTransfer.effectAllowed = 'move'; ev.dataTransfer.setData('text/plain', v.barcode); t.classList.add('dragging'); });
      t.addEventListener('dragend', () => { dragging = null; t.classList.remove('dragging'); });
    }
    t.addEventListener('dragover', ev => { if (dragging && dropping(dragging, k)) { ev.preventDefault(); t.classList.add('over'); } });
    t.addEventListener('dragleave', ev => { if (!t.contains(ev.relatedTarget)) t.classList.remove('over'); });
    t.addEventListener('drop', ev => { ev.preventDefault(); t.classList.remove('over'); const f = dragging; dragging = null; if (f) drop(f, k); });
    return t;
  }

  const none = () => h('div', { class: 'slot none', title: 'no slot reported here' });

  function rack() {
    const side = (name, mags) => h('div', { class: `wing ${name}` },
      h('div', { class: 'wing-head' }, name === 'left' ? [h('span', null, '◂ front'), h('span', null, 'left')] : [h('span', null, 'right'), h('span', null, 'front ▸')]),
      mags.map((rows, i) => h('div', { class: 'mag', title: mags.length === 1 ? `${name} magazine` : mags.length === 2 ? `${i ? 'lower' : 'upper'} ${name} magazine` : `${name} magazine ${mags.length - i}` },
        h('i', { class: 'handle' }),
        h('div', { class: 'cells' }, rows.map(r => (name === 'right' ? [...r].reverse() : r).map(c => (c ? place(placeKey(c)) : none())))))));
    return h('div', { class: 'rack' },
      side('left', l.layout.left),
      h('div', { class: 'bays' }, h('div', { class: 'wing-head' }, h('span', null, 'drives')),
        h('div', { class: 'stack' }, l.elements.filter(e => e.kind === 'drive').map(e => place(placeKey(e))))),
      side('right', l.layout.right));
  }

  function grid() {
    const by = kind => l.elements.filter(e => e.kind === kind).map(e => place(placeKey(e)));
    return [h('div', { class: 'slots' }, by('drive'), by('ie')), h('div', { class: 'slots' }, by('slot'))];
  }

  function draw() {
    const p = pending();
    bar.hidden = !p.length;
    bar.replaceChildren(
      h('span', null, `${p.length} cartridge${p.length === 1 ? '' : 's'} to move`),
      h('span', { class: 'what' }, p.map(m => `${m.v.barcode} ${els.get(m.from).name} → ${els.get(m.to).name}`).join(' · ')),
      h('span', { class: 'acts' },
        btn('Move', () => startTask({ kind: 'move', moves: p.map(m => ({ barcode: m.v.barcode, place: placeArg(m.to) })) }), 'small primary'),
        btn('Discard', () => { at = new Map(start); draw(); }, 'small')));
    map.replaceChildren(...(l.layout ? [rack()] : grid()));
  }
  draw();

  const held = l.elements.filter(e => e.volume).length;
  return h('div', { class: 'card lib' },
    h('h2', null, l.name, h('span', { class: 'muted', style: { fontWeight: '400', fontSize: '13px' } }, `${l.vendor} ${l.product} · serial ${l.serial} · ${held} cartridges`)),
    h('div', { class: 'legend' },
      h('span', null, h('i', { style: { background: 'var(--accent)' } }), 'data on the cartridge'),
      h('span', null, h('i', { style: { background: 'var(--warn)' } }), 'retiring'),
      h('span', null, h('i', { style: { background: 'var(--info)' } }), 'cleaning'),
      h('span', null, `as of ${when(l.last_seen)}`)),
    bar, map);
}

// A place in a library: a slot, I/E slot or drive bay, and the cartridge in it. In the rack
// slots go by their number; `from` marks a cartridge a plan moves there.
function slotTile(e, v, opt = {}) {
  const isDrive = e.kind === 'drive';
  const label = isDrive ? (opt.rack ? e.drive || 'empty bay' : `${e.drive || 'drive'} · ${e.name}`) : opt.rack && e.kind === 'slot' ? String(e.n) : e.name;
  const where = h('div', { class: 'where' }, h('span', null, label), v ? gen(v.generation) : null);
  const cls = ['slot', isDrive && 'drive', e.kind === 'ie' && 'ie', opt.from && 'moved'];
  const name = isDrive && e.drive ? `${e.drive} (${e.name})` : e.name;
  if (!v) return h('div', { class: [...cls, 'empty'].filter(Boolean).join(' '), title: name }, where, h('div', { class: 'line' }, isDrive ? (!e.drive ? 'no drive' : e.attached ? 'no cartridge' : 'not attached here') : 'empty'));
  const u = used(v);
  const state = v.kind === 'cleaning' ? 'cleaning' : v.state;
  const sessions = v.sessions ? `${v.sessions} session${v.sessions === 1 ? '' : 's'}` : null;
  const line = opt.from ? `from ${opt.from}` : v.kind === 'cleaning' ? 'cleaning cartridge' : sessions ? (opt.rack ? `${bytes(v.room)} free` : `${sessions} · ${bytes(v.room)} free`) : v.labelled ? 'labelled, empty' : state;
  return h('a', { class: [...cls, state].filter(Boolean).join(' '), href: `#/cartridge/${enc(v.barcode)}`, title: [`${v.barcode} in ${name}`, state, sessions, v.room != null && v.kind !== 'cleaning' ? `${bytes(v.room)} free` : null].filter(Boolean).join(' · ') },
    where, h('div', { class: 'bc' }, v.barcode), h('div', { class: 'line' }, line),
    u != null && v.kind !== 'cleaning' ? h('div', { class: 'track' }, h('i', { style: { width: `${Math.min(100, (100 * u) / v.capacity).toFixed(1)}%` } })) : null);
}

// Move a cartridge to another slot of its library (one already there changes places with it).
async function moveDialog(v) {
  let l;
  try { l = (await api('library')).libraries.find(x => x.name === v.library); } catch (e) { toast(e.message, 'bad'); return; }
  if (!l) { toast(`${v.barcode} is in no library.`, 'bad'); return; }
  const places = l.elements.filter(e => e.kind !== 'drive' && !(e.volume && e.volume.barcode === v.barcode));
  const firstFree = places.find(e => e.kind === 'slot' && !e.volume);
  const sel = h('select', null, places.map(e => h('option', { value: e.kind === 'ie' ? `ie${e.n}` : String(e.n), selected: e === firstFree },
    `${e.name}: ${e.volume ? `${e.volume.barcode} (they change places)` : 'empty'}`)));
  const ok = await modal(`Move ${v.barcode}`, [
    h('p', { style: { margin: 0 } }, `In ${v.library} ${v.place}. An I/E slot is where cartridges are taken out of the library.`),
    h('label', { class: 'field' }, 'To', sel),
  ], [{ label: 'Cancel' }, { label: 'Move', cls: 'primary', value: true }]);
  if (ok) startTask({ kind: 'move', moves: [{ barcode: v.barcode, place: sel.value }], swap: true });
}

async function pCartridges() {
  const d = await api('volumes');
  const box = h('div');
  const draw = f => box.replaceChildren(volumesTable(d.volumes.filter(v => !f || [v.barcode, v.state, v.pool, v.generation, v.library].join(' ').toLowerCase().includes(f))));
  draw('');
  const total = d.volumes.reduce((a, v) => a + (v.capacity || 0), 0), free = d.volumes.reduce((a, v) => a + (v.kind === 'data' && v.writable ? v.room || 0 : 0), 0);
  return h('div', null,
    pageHead('Cartridges', `${d.volumes.length} registered · ${bytes(free)} free for new sessions of ${bytes(total)}`),
    h('div', { class: 'card' }, h('div', { class: 'table-tools' }, filterBox('Filter by barcode, state, pool…', draw)), box));
}

async function pCartridge([bc]) {
  const v = await api(`volume?barcode=${enc(bc)}`);
  const inDrive = v.place && v.place.startsWith('drive');
  const actions = [];
  if (v.kind !== 'cleaning') actions.push(btn('Verify…', () => verifyDialog(v.barcode, v.drives)));
  if (v.sessions) actions.push(btn('Copy…', () => copyFromCartridge(v.barcode)));
  if (inDrive) actions.push(btn('Unload', () => confirmTask({ kind: 'unload', drive: v.drives.length === 1 ? v.drives[0] : null }, `Unload ${v.barcode}?`, 'The drive rewinds and unloads it, and the library puts it back in its slot.', 'Unload')));
  else if (v.library) actions.push(btn('Load…', () => loadDialog(v.barcode, v.drives)), btn('Move…', () => moveDialog(v)));
  const u = used(v);
  return h('div', null,
    pageHead(h('span', { class: 'mono' }, v.barcode), h('span', { class: 'chips' }, chip(v.kind === 'cleaning' ? 'cleaning' : v.state), gen(v.generation), v.pool ? chip(`pool ${v.pool}`) : null), actions, h('a', { href: '#/cartridges' }, 'Cartridges')),
    h('div', { class: 'card pad' }, facts([
      ['Where', v.place ? `${v.library} ${v.place}` : 'not in a library'],
      ['Capacity', bytes(v.capacity)],
      v.kind !== 'cleaning' && ['Used', fillbar(u, v.capacity)],
      v.kind !== 'cleaning' && ['Room for new sessions', v.room != null ? h('span', null, bytes(v.room), h('span', { class: 'muted' }, ` (${v.room_how})`)) : '–'],
      ['Sessions', num(v.sessions)],
      ['Last written', v.last_written ? stamp(v.last_written) : 'never'],
      ['Last verified', v.verified_at ? stamp(v.verified_at) : 'never'],
      ['Loads', num(v.loads)],
      ['Life left', v.life_percent != null ? `${v.life_percent}%` : '–'],
      ['Labelled', v.labelled ? 'yes' : 'no'],
      v.uuid && ['Volume UUID', h('span', { class: 'mono' }, v.uuid)],
    ])),
    section('Sessions on it', h('div', { class: 'card' }, table([
      { t: 'Session', f: p => (p.session ? sessLink(p.session) : '–') },
      { t: 'Job', f: p => p.job || '–' },
      { t: 'Level', f: p => p.level || '–' },
      { t: 'Started', f: p => stamp(p.started) },
      { t: 'Status', f: p => chip(p.status || '?') },
      { t: 'Copy', f: p => p.copy, num: true },
      { t: 'Part', f: p => p.seq, num: true },
      { t: 'Blocks', f: p => h('span', { class: 'mono' }, `${p.start}..${p.end ?? '?'}`), num: true },
      { t: 'Verified', f: p => (p.verified_at ? rel(p.verified_at) : h('span', { class: 'faint' }, 'never')) },
    ], v.parts, { empty: 'Nothing written to it yet.' }))),
    section('Wear history', h('div', { class: 'card' }, table([
      { t: 'Taken', f: r => stamp(r.at) },
      { t: 'Event', f: r => r.event },
      { t: 'Drive', f: r => r.drive || '–' },
      { t: 'Loads', f: r => num(r.data.load_count), num: true },
      { t: 'Written', f: r => (r.data.mib_written_life != null ? bytes(r.data.mib_written_life * 1048576) : '–'), num: true },
      { t: 'Read', f: r => (r.data.mib_read_life != null ? bytes(r.data.mib_read_life * 1048576) : '–'), num: true },
      { t: 'Errors (unrec. w/r)', f: r => `${r.data.unrecovered_write_errors ?? '–'} / ${r.data.unrecovered_read_errors ?? '–'}`, num: true },
      { t: 'Free', f: r => (r.data.remaining_mib != null ? bytes(r.data.remaining_mib * 1048576) : '–'), num: true },
      { t: 'Life', f: r => (r.data.volume_lifetime_remaining != null ? `${r.data.volume_lifetime_remaining}%` : '–'), num: true },
    ], v.stats, { empty: 'No statistics recorded yet.' }))),
    section('Events', h('div', { class: 'card' }, table([
      { t: 'When', f: e => stamp(e.at) }, { t: 'What', f: e => chip(e.kind) }, { t: 'Message', f: e => e.message },
    ], v.events, { empty: 'No events.' }))));
}

async function pDrive([name]) {
  const d = await api(`drive?name=${enc(name)}`);
  const alerts = (d.stats && d.stats.data && d.stats.data.tapealerts) || [];
  const actions = [];
  if (d.loaded && !d.holder) actions.push(btn('Unload', () => confirmTask({ kind: 'unload', drive: d.name }, `Unload ${d.loaded}?`, `${d.name} rewinds and unloads it, and the library puts it back in its slot.`, 'Unload')));
  if (d.library) actions.push(btn('Clean', () => confirmTask({ kind: 'clean', drive: d.name }, `Clean ${d.name}?`, 'Loads a cleaning cartridge from its library, waits for the cleaning to finish and puts the cartridge back. Only clean when the drive asks for it: cleaning wears the head.', 'Clean')));
  const n = k => r => num(r.data[k]);
  return h('div', null,
    pageHead(d.name, h('span', { class: 'chips' }, chip(d.state), d.holder ? chip('busy') : null, d.needs_cleaning ? chip('needs cleaning', 'warn') : null), actions, h('a', { href: '#/' }, 'Overview')),
    h('div', { class: 'card pad' }, facts([
      ['Model', [d.vendor, d.product, d.revision].filter(Boolean).join(' ')],
      ['Serial', h('span', { class: 'mono' }, d.serial)],
      ['Generation', d.generation],
      ['Where', d.library ? `${d.library} ${d.bay}` : 'standalone'],
      ['Holds', d.loaded ? tapeLink(d.loaded) : 'no cartridge'],
      ['In use by', d.holder || 'nobody'],
      ['Waiting', d.waiting.length ? d.waiting.map(w => `${w.what} (pid ${w.pid})`).join('; ') : 'nobody'],
      ['TapeAlerts', alerts.length ? alerts.map(a => `${a.flag} ${a.name}`).join(', ') : 'none'],
    ])),
    section('Statistics', h('div', { class: 'card' }, table([
      { t: 'Taken', f: r => stamp(r.at) },
      { t: 'Event', f: r => r.event },
      { t: 'Loads', f: n('lifetime_volume_loads'), num: true },
      { t: 'Cleanings', f: n('lifetime_cleanings'), num: true },
      { t: 'Power-on h', f: n('lifetime_power_on_hours'), num: true },
      { t: 'Head h', f: n('lifetime_head_hours'), num: true },
      { t: 'Since cleaning', f: n('head_hours_since_cleaning'), num: true },
      { t: 'Hard errors (w/r)', f: r => `${r.data.hard_write_errors ?? '–'} / ${r.data.hard_read_errors ?? '–'}`, num: true },
    ], d.history, { empty: 'No statistics recorded yet.' }))));
}

async function pSessions(_, q) {
  const d = await api('sessions');
  const box = h('div');
  const sel = new Set();
  const selbar = h('div', { class: 'selbar', hidden: true });
  sel.changed = () => {
    selbar.hidden = sel.size === 0;
    selbar.replaceChildren(`${sel.size} selected`, h('span', { style: { flex: '1' } }),
      btn('Copy…', () => copyDialog(d.sessions.filter(s => sel.has(s.uuid)), () => true, d.attached_drives), 'small primary'),
      btn('Clear', () => { sel.clear(); draw(fb.value.trim().toLowerCase()); sel.changed(); }, 'small'));
  };
  const draw = f => box.replaceChildren(sessionsTable(d.sessions.filter(s => !f || [s.short, s.job, s.level, s.status, ...s.tapes.map(t => t.barcode)].join(' ').toLowerCase().includes(f)), false, sel));
  const fb = filterBox('Filter by job, level, status, cartridge…', draw);
  fb.value = q.get('q') || '';
  draw(fb.value.toLowerCase());
  return h('div', null,
    pageHead('Sessions', `${d.sessions.length} recorded, newest first; tick sessions to copy them`),
    h('div', { class: 'card' }, h('div', { class: 'table-tools' }, fb), selbar, box));
}

async function pSession([key]) {
  const s = await api(`session?id=${enc(key)}`);
  const st = s.stats || {};
  const ratio = s.stored && s.bytes ? `${(s.bytes / s.stored).toFixed(2)}×` : '–';
  const writers = s.writers || [];
  return h('div', null,
    pageHead(h('span', null, 'Session ', h('span', { class: 'mono' }, s.short)), h('span', { class: 'chips' }, chip(s.status), chip(s.level), s.job ? chip(`job ${s.job}`, 'accent') : null, s.encrypted ? chip('encrypted') : null),
      [h('a', { class: 'btn', href: `#/browse?session=${enc(s.short)}` }, 'Browse this session')], h('a', { href: '#/sessions' }, 'Sessions')),
    h('div', { class: 'card pad' }, facts([
      ['Started', stamp(s.started)],
      ['Ended', s.ended ? stamp(s.ended) : '–'],
      ['Took', s.ended ? dur(s.ended - s.started) : '–'],
      ['Files', num(s.files)],
      ['Data', bytes(s.bytes)],
      ['On tape', h('span', null, bytes(s.stored), h('span', { class: 'muted' }, ` (${ratio})`))],
      ['Sources', (s.sources || []).length ? h('span', { class: 'mono' }, s.sources.join(', ')) : '–'],
      ['Host', s.host],
      ['After', s.parent ? sessLink(s.parent) : '–'],
      ['Followed by', s.children.length ? h('span', { class: 'chips' }, s.children.map(c => sessLink(c))) : '–'],
      st.packs != null && ['Packs', num(st.packs)],
      st.warnings ? ['Warnings', num(st.warnings)] : null,
      st.changed ? ['Changed while read', num(st.changed)] : null,
      s.expired_at ? ['Expired', stamp(s.expired_at)] : null,
      ['UUID', h('span', { class: 'mono' }, s.uuid)],
    ])),
    section('How the drives wrote', h('div', { class: 'card' }, writers.length ? table([
      { t: 'Drive', f: (w, i) => `#${writers.indexOf(w) + 1}` },
      { t: 'Written', f: w => bytes(w.bytes), num: true },
      { t: 'Matched rate', f: w => (w.matched_mbps != null ? `${w.matched_mbps.toFixed(0)} MB/s (${w.matched_min_mbps.toFixed(0)}–${w.matched_max_mbps.toFixed(0)})` : 'full speed'), num: true },
      { t: 'Pauses', f: w => (w.pauses != null ? `${w.pauses} (${w.paused_seconds.toFixed(1)} s)` : '–'), num: true },
      { t: 'Cycles', f: w => num(w.cycles), num: true },
      { t: 'Speed changes', f: w => num(w.speed_changes), num: true },
      { t: 'Buffer ran dry', f: w => num(w.underruns), num: true },
      { t: 'Streaming', f: w => (w.streaming_seconds != null ? dur(w.streaming_seconds) : '–'), num: true },
    ], writers) : empty('Not recorded for this session (written before ltoarc kept drive reports).'))),
    section('Cartridges', h('div', { class: 'card' }, table([
      { t: 'Copy', f: p => p.copy, num: true },
      { t: 'Part', f: p => p.seq, num: true },
      { t: 'Cartridge', f: p => tapeLink(p.barcode) },
      { t: 'Blocks', f: p => h('span', { class: 'mono' }, `${p.start}..${p.end ?? '?'}`) },
      { t: 'Volume catalog at', f: p => h('span', { class: 'mono' }, p.catalog ?? '–') },
      { t: 'Verified', f: p => (p.verified_at ? stamp(p.verified_at) : h('span', { class: 'faint' }, 'never')) },
    ], s.parts))));
}

// --- browse

async function pBrowse(_, q) {
  let path = q.get('path') || '/';
  const view = { as_of: q.get('as_of') || '', session: q.get('session') || '' };
  const pattern = q.get('find') || '';
  const viewQs = `${view.as_of ? `&as_of=${enc(view.as_of.replace('T', ' '))}` : ''}${view.session ? `&session=${enc(view.session)}` : ''}`;
  const go = (p, extra = {}) => {
    const n = new URLSearchParams();
    n.set('path', p);
    const v = { ...view, ...extra };
    if (v.as_of) n.set('as_of', v.as_of);
    if (v.session) n.set('session', v.session);
    if (extra.find) n.set('find', extra.find);
    location.hash = `#/browse?${n}`;
  };
  let data = pattern ? await api(`find?pattern=${enc(pattern)}&under=${enc(path)}${viewQs}`) : await api(`ls?path=${enc(path)}${viewQs}`);
  // A file's path (a link to the file): show its folder, with the file open.
  let file = null;
  if (!pattern && data.self && data.self.kind !== 'dir') {
    file = path;
    path = path.replace(/\/[^/]*$/, '') || '/';
    data = await api(`ls?path=${enc(path)}${viewQs}`);
  }
  const entries = pattern ? data.matches.map(m => ({ ...m, name: m.path })) : data.entries;
  const selected = new Set();
  const layout = h('div', { class: 'browse' });
  const panel = h('div', { class: 'card panel', hidden: true });
  let current = null;

  // breadcrumbs
  const parts = path.split('/').filter(Boolean);
  const crumbs = h('div', { class: 'pathbar' },
    h('a', { href: '#', onclick: e => { e.preventDefault(); go('/'); } }, 'archive'),
    parts.map((p, i) => [h('span', { class: 'sep' }, '/'), i === parts.length - 1 && !pattern ? h('span', null, p) : h('a', { href: '#', onclick: e => { e.preventDefault(); go(`/${parts.slice(0, i + 1).join('/')}`); } }, p)]),
    pattern ? [h('span', { class: 'sep' }, ' · '), h('span', null, `matching ${pattern}`)] : null);

  // view controls
  const asOf = h('input', { type: 'datetime-local', value: view.as_of ? view.as_of.replace(' ', 'T').slice(0, 16) : '' });
  const sess = h('input', { type: 'text', placeholder: 'session', value: view.session, style: { width: '110px' } });
  const find = h('input', { type: 'search', placeholder: 'Find, e.g. *.mov', value: pattern, style: { width: '200px' } });
  find.addEventListener('keydown', e => { if (e.key === 'Enter') go(path, { find: find.value.trim() }); });
  const apply = () => go(path, { as_of: asOf.value, session: sess.value.trim(), find: pattern });
  asOf.addEventListener('change', apply);
  sess.addEventListener('keydown', e => { if (e.key === 'Enter') apply(); });

  const selbar = h('div', { class: 'selbar', hidden: true });
  const updateSel = () => {
    selbar.hidden = selected.size === 0;
    selbar.replaceChildren(`${selected.size} selected`, h('span', { style: { flex: '1' } }),
      btn('Restore…', () => restoreDialog([...selected], view), 'small primary'), btn('Clear', () => { selected.clear(); render(); }, 'small'));
  };

  async function openPanel(p, objectId) {
    current = p;
    layout.classList.add('open');
    panel.hidden = false;
    panel.replaceChildren(h('div', { class: 'pbody' }, 'Loading…'));
    try {
      const s = await api(`stat?path=${enc(p)}${viewQs}${objectId ? `&object=${objectId}` : ''}`);
      if (current !== p) return;
      panel.replaceChildren(filePanel(s, view, () => { layout.classList.remove('open'); panel.hidden = true; current = null; }, id => openPanel(p, id)));
    } catch (e) {
      panel.replaceChildren(h('div', { class: 'pbody' }, errorBox(e)));
    }
  }

  const listBox = h('div');
  const render = () => {
    listBox.replaceChildren(table([
      {
        t: h('input', { type: 'checkbox', 'aria-label': 'select all', onchange: e => { entries.forEach(x => (e.target.checked ? selected.add(x.path) : selected.delete(x.path))); render(); } }),
        f: x => h('input', { type: 'checkbox', checked: selected.has(x.path), onchange: e => { e.target.checked ? selected.add(x.path) : selected.delete(x.path); updateSel(); e.target.closest('tr').classList.toggle('sel', e.target.checked); } }),
      },
      {
        t: 'Name', key: x => x.name,
        f: x => (x.kind === 'dir'
          ? h('a', { href: '#', onclick: e => { e.preventDefault(); go(x.path); } }, h('span', { class: 'ico dir' }, '▸'), x.name, '/')
          : h('span', null, h('span', { class: 'ico' }, x.kind === 'symlink' ? '↪' : '·'), x.name, x.target ? h('span', { class: 'muted' }, ` → ${x.target}`) : null)),
      },
      { t: 'Size', f: x => (x.kind === 'dir' ? '' : bytes(x.size)), num: true, key: x => (x.kind === 'dir' ? -1 : x.size) },
      { t: 'Modified', f: x => (x.mtime ? stamp(x.mtime) : ''), key: x => x.mtime },
      { t: 'Owner', f: x => (x.user ? `${x.user}:${x.group}` : ''), cls: 'muted' },
      { t: 'Mode', f: x => h('span', { class: 'mono muted' }, x.mode || '') },
      { t: '', f: x => [x.status && x.status !== 'ok' ? chip(x.status, 'warn') : null, x.by_reference ? chip('by ref.', 'info') : null] },
    ], entries, {
      empty: pattern ? 'No matches.' : 'Nothing archived here in this view.',
      click: x => (x.kind === 'dir' ? go(x.path) : openPanel(x.path)),
      selected: x => selected.has(x.path),
    }));
    updateSel();
  };
  render();

  const total = data.total;
  layout.append(h('div', { class: 'card' },
    h('div', { class: 'table-tools' }, crumbs, h('span', { class: 'grow' }),
      h('label', { class: 'muted', style: { fontSize: '13px' } }, 'As of ', asOf), sess, find,
      view.as_of || view.session ? btn('Latest', () => { location.hash = `#/browse?path=${enc(path)}`; }, 'small') : null),
    selbar, listBox,
    total > entries.length ? h('div', { class: 'empty' }, `Showing ${num(entries.length)} of ${num(total)}.`) : null), panel);

  const actions = [];
  if (path !== '/' && !pattern) actions.push(btn('Restore this folder…', () => restoreDialog([path], view)));
  const head = pageHead('Browse', view.session ? `Session ${view.session} only` : view.as_of ? `The archive as it was at ${view.as_of.replace('T', ' ')}` : 'The latest version of every file', actions);
  if (file) setTimeout(() => openPanel(file), 0);
  return h('div', null, head, layout);
}

function filePanel(s, view, close, pick) {
  const c = s.current;
  const top = h('div', { class: 'phead' }, h('div', { style: { minWidth: '0' } }, h('h2', { style: { overflowWrap: 'anywhere' } }, s.path.split('/').pop()), h('div', { class: 'muted mono', style: { fontSize: '12px', overflowWrap: 'anywhere' } }, s.path)), btn('×', close, 'small'));
  if (!c) return h('div', null, top, h('div', { class: 'pbody' }, empty('Not in this view (deleted, or not yet archived).'), versionsList(s, null, pick)));
  return h('div', null, top, h('div', { class: 'pbody' },
    h('div', { style: { display: 'flex', gap: '8px' } }, btn('Restore…', () => restoreDialog([s.path], view), 'primary small'), c.status !== 'ok' ? chip(c.status, 'warn') : null),
    h('dl', { class: 'kv' },
      h('dt', null, 'Size'), h('dd', null, `${bytes(c.size)} (${num(c.size)} bytes)`),
      h('dt', null, 'Modified'), h('dd', null, stamp(c.mtime)),
      h('dt', null, 'Owner'), h('dd', null, `${c.user}:${c.group}`),
      h('dt', null, 'Mode'), h('dd', { class: 'mono' }, c.mode),
      c.target ? [h('dt', null, 'Target'), h('dd', { class: 'mono' }, c.target)] : null,
      h('dt', null, 'Chunks'), h('dd', null, num(c.chunks)),
      c.data_from ? [h('dt', null, 'Data'), h('dd', null, 'stored with ', h('span', { class: 'mono' }, c.data_from.path), ' in session ', sessLink(c.data_from.session))] : null),
    c.locations.length ? h('div', null, h('h3', null, 'On tape'), h('dl', { class: 'kv' }, c.locations.map(l => [h('dt', null, `copy ${l.copy}`), h('dd', null, tapeLink(l.barcode), h('span', { class: 'muted mono' }, ` block ${num(l.block)}`))]))) : null,
    c.hashes.length ? h('div', null, h('h3', null, 'Hashes'), h('dl', { class: 'kv' }, c.hashes.map(x => [h('dt', null, x.algo), h('dd', { class: 'digest' }, x.digest)]))) : null,
    c.xattrs.length ? h('div', null, h('h3', null, 'Extended attributes'), h('dl', { class: 'kv' }, c.xattrs.map(x => [h('dt', { class: 'mono', style: { fontSize: '12px' } }, x.name), h('dd', { class: 'digest' }, x.value)]))) : null,
    versionsList(s, c.id, pick)));
}

function versionsList(s, currentId, pick) {
  if (!s.versions.length) return null;
  return h('div', null, h('h3', null, `Versions (${s.versions.length})`), h('div', { class: 'versions' }, s.versions.map(v => {
    const gone = v.kind === 'whiteout';
    return h('button', { type: 'button', class: v.id === currentId ? 'on' : null, onclick: () => !gone && pick(v.id), disabled: gone },
      h('span', null, gone ? h('span', { class: 'muted' }, 'deleted') : bytes(v.size)),
      h('span', { class: 'muted' }, when(v.started)),
      h('small', null, `session ${v.session_short}`, v.job ? ` · ${v.job}` : '', v.session_status !== 'complete' ? ` · ${v.session_status}` : '', v.status !== 'ok' && !gone ? ` · ${v.status}` : ''));
  })));
}

// --- jobs and activity

async function pJobs() {
  const d = await api('jobs');
  setBusy(d.running);
  return h('div', null,
    pageHead('Jobs', 'From the jobs file; ltoarcd runs them on their schedules', [
      btn('Back up…', () => backupDialog()), btn('Check wear…', wearDialog),
    ]),
    h('div', { class: 'card' }, table([
      { t: 'Job', f: j => h('a', { href: `#/job/${enc(j.name)}` }, j.name), key: j => j.name },
      { t: 'Kind', f: j => j.kind, key: j => j.kind },
      { t: 'Schedule', f: j => (j.schedule ? h('span', null, h('span', { class: 'mono' }, j.schedule), j.next ? h('div', { class: 'muted', style: { fontSize: '12px' } }, `next ${ago(j.next)}`) : null) : h('span', { class: 'faint' }, 'on request')), key: j => j.next },
      { t: 'State', f: j => (j.state === 'idle' || j.state === 'queued' ? chip(j.state) : h('span', null, chip('running'), ' ', h('span', { class: 'muted' }, dur(now() - j.state.running)))) },
      { t: 'Last run', f: j => (j.last ? h('span', null, chip(j.last.status), ' ', h('a', { href: `#/run/${j.last.id}` }, ago(j.last.started))) : h('span', { class: 'faint' }, 'never')), key: j => j.last && j.last.started },
      { t: 'Result', f: j => h('span', { class: 'muted' }, (j.last && j.last.summary) || '') },
      { t: '', f: j => btn('Run now', async () => { try { await api('run-job', { name: j.name }); toast(`Queued ${j.name}`); render(); } catch (e) { toast(e.message, 'bad'); } }, 'small') },
    ], d.jobs, { empty: 'No jobs in the jobs file.', sort: 0 })));
}
pJobs.refresh = 5000;

async function pJob([name]) {
  const [jobs, runs] = await Promise.all([api('jobs'), api(`runs?job=${enc(name)}&limit=200`)]);
  const j = jobs.jobs.find(x => x.name === name);
  if (!j) throw new Error(`No job ${name} in the jobs file.`);
  const cfg = Object.entries(j.config).filter(([, v]) => v != null && v !== false && !(Array.isArray(v) && !v.length) && !(typeof v === 'object' && !Array.isArray(v) && !Object.keys(v).length));
  const show = v => (Array.isArray(v) ? h('span', { class: 'mono' }, v.join('\n')) : typeof v === 'object' ? h('span', { class: 'mono' }, Object.entries(v).map(([a, b]) => `${a} = ${b}`).join('\n')) : h('span', { class: 'mono' }, String(v)));
  return h('div', null,
    pageHead(name, h('span', { class: 'chips' }, chip(j.kind, 'accent'), j.state === 'idle' || j.state === 'queued' ? chip(j.state) : chip('running')),
      [btn('Run now', async () => { try { await api('run-job', { name }); toast(`Queued ${name}`); render(); } catch (e) { toast(e.message, 'bad'); } }, 'primary')], h('a', { href: '#/jobs' }, 'Jobs')),
    h('div', { class: 'card pad' }, facts([
      ['Next run', j.next ? h('span', null, stamp(j.next), h('span', { class: 'muted' }, ` (${ago(j.next)})`)) : 'on request'],
      ...cfg.map(([k, v]) => [k.replace(/_/g, ' '), h('span', { style: { whiteSpace: 'pre-wrap' } }, show(v))]),
    ])),
    section('Runs', h('div', { class: 'card' }, runsTable(runs.runs, false))));
}

async function pActivity(_, q) {
  const job = q.get('job') || '';
  const [jobs, runs] = await Promise.all([api('jobs'), api(`runs?limit=200${job ? `&job=${enc(job)}` : ''}`)]);
  setBusy(jobs.running);
  const names = [...new Set([...jobs.jobs.map(j => j.name), ...runs.runs.map(r => r.job)])].sort();
  const pick = h('select', { onchange: e => { location.hash = `#/activity${e.target.value ? `?job=${enc(e.target.value)}` : ''}`; } },
    h('option', { value: '' }, 'All jobs and tasks'), names.map(n => h('option', { value: n, selected: n === job }, n.startsWith('@') ? `${n.slice(1)} (task)` : n)));
  return h('div', null,
    pageHead('Activity', 'Job runs, and tasks started here', [
      btn('Read inventory', () => confirmTask({ kind: 'inventory' }, 'Read the library inventories?', 'Reads which cartridges are in which slots and drives, and records new barcodes. Nothing moves.', 'Read')),
      btn('Check wear…', wearDialog),
    ]),
    section('Running', runningList(jobs.running)),
    section('History', h('div', { class: 'card' }, h('div', { class: 'table-tools' }, pick), runsTable(runs.runs, false))));
}
pActivity.refresh = 5000;

const PROGRESS = /^\s*\d[\d,]* entries, \S+ \S+( of \S+ \S+ \(\d+%\))?, [\d.]+ MB\/s/;

function logLine(line) {
  const t = line.trimStart();
  const cls = /^(error|HASH MISMATCH|ERROR)/.test(t) ? 'l-bad'
    : /^(warning|WARNING|SHORT)/.test(t) ? 'l-warn'
    : PROGRESS.test(line) ? 'l-prog'
    : /^(session|tape|drive|data|dest|source|matched|tapes|copy 2|entries|read|restored|verified)\b/.test(t) ? 'l-key' : null;
  return h('span', { class: cls }, `${line}\n`);
}

async function pRun([id]) {
  let d = await api(`log?run=${enc(id)}`);
  const pre = h('pre');
  const progress = h('div', { class: 'progress-line', hidden: true });
  let carry = '';
  let lastProgress = null;
  const append = text => {
    const atEnd = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 40;
    const lines = (carry + text).split('\n');
    carry = lines.pop();
    add(pre, lines.map(l => { if (PROGRESS.test(l)) lastProgress = l.trim(); return logLine(l); }));
    if (atEnd) pre.scrollTop = pre.scrollHeight;
  };
  if (d.skipped) add(pre, h('span', { class: 'l-prog' }, `… (the first ${bytes(d.from)} of the log are not shown)\n`));
  append(d.text);
  if (!d.text) add(pre, h('span', { class: 'l-prog' }, 'No output yet.\n'));
  let offset = d.offset;
  const head = h('div');
  const meta = h('span');
  const drawHead = () => {
    const r = d.run, running = d.running;
    const title = r.job.startsWith('@') ? r.run : `${r.job} · run ${r.run}`;
    head.replaceChildren(pageHead(title, h('span', { class: 'chips' }, chip(running ? 'running' : r.status), r.job.startsWith('@') ? chip('task') : h('a', { href: `#/job/${enc(r.job)}` }, `job ${r.job}`)),
      running ? [btn('Stop', () => stopRun({ run: r.id, title }), 'danger')] : [], h('a', { href: '#/activity' }, 'Activity')));
    meta.textContent = `started ${when(r.started)} · ${running ? `running for ${dur(now() - r.started)}` : `took ${dur((r.ended || r.started) - r.started)}`}`;
    progress.hidden = !(running && lastProgress);
    progress.textContent = lastProgress || '';
    if (!running && r.summary) {
      progress.hidden = false;
      progress.textContent = r.summary;
      progress.style.background = r.status === 'ok' ? 'var(--ok-soft)' : 'var(--bad-soft)';
      progress.style.color = r.status === 'ok' ? 'var(--ok)' : 'var(--bad)';
    }
  };
  drawHead();
  if (d.running) {
    every(1000, async () => {
      const n = await api(`log?run=${enc(id)}&from=${offset}`);
      if (n.offset > offset) append(n.text);
      offset = n.offset;
      d = n;
      drawHead();
      if (!n.running) {
        if (carry) { add(pre, logLine(carry)); carry = ''; }
        stopTimers();
      }
    });
  }
  setTimeout(() => { pre.scrollTop = pre.scrollHeight; }, 0);
  return h('div', null, head, progress,
    h('div', { class: 'logbox' }, h('div', { class: 'lh' }, meta, h('span', null, 'output')), pre));
}

// ---------------------------------------------------------------- router

const ROUTES = [
  [/^\/?$/, pOverview, 'overview'],
  [/^\/library$/, pLibrary, 'library'],
  [/^\/cartridges$/, pCartridges, 'cartridges'],
  [/^\/cartridge\/(.+)$/, pCartridge, 'cartridges'],
  [/^\/drive\/(.+)$/, pDrive, 'overview'],
  [/^\/sessions$/, pSessions, 'sessions'],
  [/^\/session\/(.+)$/, pSession, 'sessions'],
  [/^\/browse$/, pBrowse, 'browse'],
  [/^\/jobs$/, pJobs, 'jobs'],
  [/^\/job\/(.+)$/, pJob, 'jobs'],
  [/^\/activity$/, pActivity, 'activity'],
  [/^\/run\/(\d+)$/, pRun, 'activity'],
];

let timers = [];
function every(ms, fn) {
  const id = setInterval(() => { if (!document.hidden) fn().catch(() => {}); }, ms);
  timers.push(id);
}
function stopTimers() {
  timers.forEach(clearInterval);
  timers = [];
}

let seq = 0;
async function render() {
  stopTimers();
  const mine = ++seq;
  const raw = location.hash.slice(1) || '/';
  const [path, qs] = raw.split('?');
  const q = new URLSearchParams(qs || '');
  const main = $('#main');
  if (!main) return; // the login form is up
  const route = ROUTES.find(([re]) => re.test(path));
  document.querySelectorAll('#nav a').forEach(a => a.classList.toggle('on', route && a.dataset.nav === route[2]));
  if (!route) { main.replaceChildren(errorBox(new Error('No such page.'))); return; }
  const [re, page] = route;
  const args = path.match(re).slice(1).map(decodeURIComponent);
  if (!main.firstChild || main.dataset.page !== path) main.replaceChildren(h('div', { class: 'spinner' }, 'Loading…'));
  main.dataset.page = path;
  try {
    const node = await page(args, q);
    if (mine === seq) main.replaceChildren(node);
  } catch (e) {
    if (mine === seq) main.replaceChildren(errorBox(e));
    return;
  }
  if (page.refresh && mine === seq) {
    every(page.refresh, async () => {
      // Leave the page alone while someone works in it.
      if (document.querySelector('dialog[open]') || main.contains(document.activeElement) && /INPUT|SELECT|TEXTAREA/.test(document.activeElement.tagName)) return;
      const y = window.scrollY;
      const node = await page(args, q);
      if (mine === seq) { main.replaceChildren(node); window.scrollTo(0, y); }
    });
  }
}

// ---------------------------------------------------------------- login

function showLogin(msg) {
  stopTimers();
  if ($('.login')) return;
  const input = h('input', { type: 'password', placeholder: 'access token or login link', autocomplete: 'current-password' });
  const err = h('div', { class: 'error', hidden: !msg }, msg || '');
  const submit = async e => {
    e.preventDefault();
    try {
      const v = input.value.trim();
      const m = v.match(/#login=(.+)$/);
      await api('login', { token: m ? decodeURIComponent(m[1]) : v });
      location.reload();
    } catch (x) {
      err.textContent = x.message; err.hidden = false;
    }
  };
  document.body.replaceChildren(h('div', { class: 'login' }, h('form', { class: 'card', onsubmit: submit },
    h('div', { style: { display: 'flex', gap: '10px', alignItems: 'center' } }, h('img', { src: '/icon.svg', width: 30, height: 30, alt: '' }), h('h1', null, 'ltoarc')),
    h('p', { class: 'muted', style: { margin: 0 } }, 'On the host running ltoarcd, ', h('code', null, 'ltoarc daemon url'), ' prints a link that logs you in. Or paste the token from the daemon\'s state directory (', h('code', null, 'web.token'), ').'),
    input, err, h('button', { class: 'btn primary', type: 'submit' }, 'Log in'))));
  input.focus();
}

// The token of a login link (`ltoarc daemon url` prints one: .../#login=TOKEN), if the address
// is one.
function linkToken() {
  const m = location.hash.match(/^#login=(.+)$/);
  return m ? decodeURIComponent(m[1]) : null;
}

// Log in with a login link's token and drop it from the address; false, with the login form
// shown, if it is refused.
async function linkLogin(token) {
  history.replaceState(null, '', '#/');
  try {
    await api('login', { token });
    return true;
  } catch (e) {
    showLogin(e.message);
    return false;
  }
}

async function boot() {
  $('#logout').addEventListener('click', async () => { await api('logout', {}).catch(() => {}); showLogin(); });
  // A login link opened in a tab already on this page changes only the hash, which loads
  // nothing: log in then, and load the page again.
  window.addEventListener('hashchange', async () => {
    const token = linkToken();
    if (token === null) return render();
    if (await linkLogin(token)) location.reload();
  });
  const token = linkToken();
  if (token !== null && !(await linkLogin(token))) return;
  await render();
  setInterval(async () => {
    if (document.hidden || $('.login')) return;
    try { setBusy((await api('jobs')).running); } catch (_) { /* shown on the next page load */ }
  }, 15000);
}

document.addEventListener('DOMContentLoaded', boot);
