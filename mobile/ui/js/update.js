/**
 * update.js — in-app update for the Android app.
 *
 * 1. Check: on boot (and on later resumes) at most every 6 h, fetch the latest GitHub
 *    release, pick its APK asset (`*.apk`, not the "hostcontrol" variant) and compare the
 *    tag with the running version (`__TAURI__.app.getVersion()`). Only a strictly newer
 *    semver counts. The last result is cached in localStorage; failures are silent and
 *    retried on the next check.
 * 2. Indicator: `#update-btn` in the top bar (left of the game toggle), hidden otherwise.
 * 3. Sheet: notes + size + download / progress / install, driven by the native updater
 *    (Updater.kt). The plugin's trigger() events never reach JS in this build, so progress
 *    is POLLED via `update_status` while a download runs — independent of the sheet, so the
 *    download continues when it is closed and reopening shows live progress.
 *
 * Plugin commands (snake_case, plugin:pulsar-video|…):
 *   update_download {url, version, size} · update_cancel · update_status {version, size}
 *   update_install {version} → {started} | {needsPermission} | {signatureMismatch} | {error}
 *
 * Android only: never shows in a plain browser or a desktop build.
 *
 * Mock mode (UI review only): localStorage `pulsar.update.mock` = '1' fakes a newer
 * release and a ~4 MB/s download in JS instead of calling the native updater; install
 * then reports the "permission needed" hint once and "started" after that.
 */

import { invoke, hasTauri } from './tauri.js';
import { t, lang } from './i18n.js';

const REPO_API = 'https://api.github.com/repos/PulsarDesk/pulsar-mobile/releases/latest';
const CACHE_KEY = 'pulsar.update.v1';
const CHECK_EVERY_MS = 6 * 3600 * 1000;
const POLL_MS = 250;
const MOCK_KEY = 'pulsar.update.mock';

let mock = null;
try { if (localStorage.getItem(MOCK_KEY) === '1') mock = { start: 0, size: 48 * 1048576, done: false, cancelled: false, installs: 0 }; } catch (_) {}

/** Native updater call, or its JS fake in mock mode. */
async function native(cmd, args) {
	if (!mock) return invoke('plugin:pulsar-video|' + cmd, args);
	const m = mock;
	const elapsed = () => (Date.now() - m.start) / 1000;
	const got = () => Math.min(m.size, Math.round(elapsed() * 4 * 1048576));
	if (cmd === 'update_download') { m.start = Date.now(); m.done = false; m.cancelled = false; return { ok: true }; }
	if (cmd === 'update_cancel') { m.start = 0; m.cancelled = true; return { ok: true }; }
	if (cmd === 'update_install') return ++m.installs === 1 ? { needsPermission: true } : { started: true };
	// update_status
	if (m.done) return { downloaded: true, total: m.size };
	if (!m.start) return { total: m.size };
	if (got() >= m.size) { m.done = true; return { downloaded: true, received: m.size, total: m.size }; }
	const bps = 4 * 1048576 * (0.85 + Math.random() * 0.3);
	return { active: true, received: got(), total: m.size, bps };
}

const isAndroid = hasTauri && /Android/i.test(navigator.userAgent || '');

/** Latest release {version, tag, url, size, body} when it is newer than us, else null. */
let rel = null;
let current = '';
/** 'idle' | 'downloading' | 'downloaded' | 'error' */
let phase = 'idle';
let prog = { received: 0, total: 0, bps: 0 };
let errMsg = '';
/** '' | 'perm' | 'sig' | 'fail' — a note under the install button after update_install. */
let hint = '';
let hintMsg = '';
/** "İndir ve kur" pressed → launch the installer once the download finishes (sheet open). */
let autoInstall = false;
let pollTimer = 0;
let checking = false;

// ---- Version + cache ---------------------------------------------------------

/** "v0.13.1" / "0.1.0-dev.27" → {n:[0,13,1], pre:''|'dev.27'} (null if unparsable). */
function parseVer(v) {
	const m = String(v || '').trim().replace(/^v/i, '').match(/^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?/);
	return m ? { n: [+m[1], +m[2], +m[3]], pre: m[4] || '' } : null;
}

/** Is version string `a` strictly newer than `b`? (prerelease < release of the same core) */
export function isNewer(a, b) {
	const x = parseVer(a), y = parseVer(b);
	if (!x || !y) return false;
	for (let i = 0; i < 3; i++) if (x.n[i] !== y.n[i]) return x.n[i] > y.n[i];
	return !x.pre && !!y.pre;
}

function readCache() {
	try { return JSON.parse(localStorage.getItem(CACHE_KEY) || 'null'); } catch (_) { return null; }
}
function writeCache(v) {
	try { localStorage.setItem(CACHE_KEY, JSON.stringify(v)); } catch (_) {}
}

/** GitHub release JSON → {version, tag, url, size, body} or null (no usable APK asset). */
function fromRelease(j) {
	if (!j || !j.tag_name || !Array.isArray(j.assets)) return null;
	const a = j.assets.find((x) => x && /\.apk$/i.test(x.name || '') && !/hostcontrol/i.test(x.name || ''));
	if (!a || !a.browser_download_url) return null;
	return {
		tag: j.tag_name,
		version: String(j.tag_name).replace(/^v/i, ''),
		url: a.browser_download_url,
		size: a.size || 0,
		body: j.body || '',
	};
}

/** Check (rate-limited to once per 6 h through the cache). Silent on any failure. */
async function check() {
	if (checking) return;
	checking = true;
	try {
		if (!current) {
			try { current = await window.__TAURI__.app.getVersion(); } catch (_) { return; }
		}
		if (mock) {
			setRelease({
				tag: 'v9.9.9', version: '9.9.9', url: '', size: mock.size,
				body: '## Features\n* **update:** in-app update sheet\n* **splash:** animated launch splash\n\n## Bug Fixes\n* waiting screen hides the overlay button',
			});
			return;
		}
		const c = readCache();
		let latest = c && c.rel;
		if (!c || !(Date.now() - c.at < CHECK_EVERY_MS)) {
			const r = await fetch(REPO_API, { headers: { Accept: 'application/vnd.github+json' } });
			if (!r.ok) return; // retried next time (cache timestamp not advanced)
			latest = fromRelease(await r.json());
			writeCache({ at: Date.now(), rel: latest });
		}
		setRelease(latest && isNewer(latest.version, current) ? latest : null);
	} catch (_) {
		/* offline / rate-limited — silent */
	} finally {
		checking = false;
	}
}

function setRelease(r) {
	const changed = (r && r.version) !== (rel && rel.version);
	rel = r;
	const btn = document.getElementById('update-btn');
	if (btn) btn.hidden = !rel;
	if (rel && changed) refreshStatus();
}

// ---- Native status / polling -------------------------------------------------

async function status() {
	return native('update_status', { version: rel.version, size: rel.size });
}

/** Re-derive `phase` from the native side (on-disk APK / running download / last error). */
async function refreshStatus() {
	if (!rel) return;
	try {
		const s = await status();
		if (!s) return;
		applyStatus(s);
	} catch (_) {}
}

function applyStatus(s) {
	const prev = phase;
	prog = { received: s.received || 0, total: s.total || rel.size || 0, bps: s.bps || 0 };
	if (s.active) phase = 'downloading';
	else if (s.downloaded) phase = 'downloaded';
	else if (s.error) { phase = 'error'; errMsg = s.error; }
	else phase = 'idle';

	if (phase === 'downloading') schedulePoll();
	if (prev !== phase) {
		renderActions();
		if (prev === 'downloading' && phase === 'downloaded' && autoInstall && sheetOpen()) {
			autoInstall = false;
			install();
		}
		if (phase !== 'downloading') autoInstall = false;
	} else {
		renderProgress();
	}
}

function schedulePoll() {
	if (pollTimer) return;
	pollTimer = setTimeout(async () => {
		pollTimer = 0;
		await refreshStatus();
	}, POLL_MS);
}

// ---- Actions -----------------------------------------------------------------

/** @param {boolean} [thenInstall] "İndir ve kur" → open the installer when done. */
async function download(thenInstall = true) {
	if (!rel) return;
	hint = ''; errMsg = '';
	autoInstall = thenInstall;
	phase = 'downloading';
	prog = { received: 0, total: rel.size || 0, bps: 0 };
	renderActions();
	try {
		await native('update_download', { url: rel.url, version: rel.version, size: rel.size });
		schedulePoll();
	} catch (e) {
		phase = 'error';
		errMsg = String((e && e.message) || e || '');
		autoInstall = false;
		renderActions();
	}
}

async function cancel() {
	autoInstall = false;
	try { await native('update_cancel', {}); } catch (_) {}
	phase = 'idle';
	prog = { received: 0, total: rel ? rel.size : 0, bps: 0 };
	renderActions();
	setTimeout(refreshStatus, 400);
}

async function install() {
	if (!rel) return;
	hint = ''; hintMsg = '';
	let r = null;
	try {
		r = await native('update_install', { version: rel.version });
	} catch (e) {
		r = { error: String((e && e.message) || e || '') };
	}
	if (r && r.needsPermission) hint = 'perm';
	else if (r && r.signatureMismatch) hint = 'sig';
	else if (r && r.started) hint = '';
	else { hint = 'fail'; hintMsg = (r && r.error) || ''; }
	renderActions();
}

// ---- Sheet -------------------------------------------------------------------

function esc(s) {
	return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

/** Markdown release notes → readable plain text (headings/bold/links stripped). */
function plainNotes(md) {
	return String(md || '')
		.replace(/\r/g, '')
		.replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')      // [text](url) → text
		.replace(/^#{1,6}\s*/gm, '')                   // headings
		.replace(/\*\*([^*]+)\*\*/g, '$1')             // bold
		.replace(/^\s*[*-]\s+/gm, '• ')                // bullets
		.replace(/\s*\(\s*\)/g, '')                    // emptied "( )" after link strip
		.replace(/\n{3,}/g, '\n\n')
		.trim();
}

function mb(bytes) {
	const v = (bytes || 0) / 1048576;
	try { return new Intl.NumberFormat(lang, { minimumFractionDigits: 1, maximumFractionDigits: 1 }).format(v); }
	catch (_) { return v.toFixed(1); }
}

function eta(left, bps) {
	if (!(bps > 0) || !(left > 0)) return '';
	const s = Math.ceil(left / bps);
	return s < 60 ? t('m.update.etaSec', { n: s }) : t('m.update.etaMin', { n: Math.ceil(s / 60) });
}

const sheetEl = () => document.getElementById('update-sheet');
const sheetOpen = () => !!sheetEl();

function openSheet() {
	if (!rel || sheetOpen()) return;
	const backdrop = document.createElement('div');
	backdrop.className = 'sheet-backdrop';
	backdrop.id = 'update-backdrop';
	const sheet = document.createElement('div');
	sheet.className = 'sheet';
	sheet.id = 'update-sheet';
	sheet.setAttribute('role', 'dialog');
	sheet.setAttribute('aria-modal', 'true');
	document.body.appendChild(backdrop);
	document.body.appendChild(sheet);
	renderSheet();
	requestAnimationFrame(() => { backdrop.classList.add('open'); sheet.classList.add('open'); });
	const close = () => {
		backdrop.classList.remove('open'); sheet.classList.remove('open');
		setTimeout(() => { backdrop.remove(); sheet.remove(); }, 280);
		autoInstall = false; // closed mid-download → don't pop the installer later
	};
	backdrop.addEventListener('click', close);
	refreshStatus();
}

function renderSheet() {
	const sheet = sheetEl();
	if (!sheet || !rel) return;
	const notes = plainNotes(rel.body);
	sheet.innerHTML =
		'<div class="sheet-handle"></div>' +
		'<div class="upd-title">' + esc(t('m.update.title')) + '</div>' +
		'<div class="upd-ver">' + esc(current) + ' → <b>' + esc(rel.version) + '</b></div>' +
		'<div class="upd-notes" id="upd-notes"></div>' +
		'<div class="upd-size">' + esc(t('m.update.size', { mb: mb(rel.size) })) + '</div>' +
		'<div id="upd-actions" style="display:flex;flex-direction:column;gap:14px;"></div>';
	const n = sheet.querySelector('#upd-notes');
	n.textContent = notes || t('m.update.noNotes');
	// Scrolling the notes must not drag the sheet (sheet-swipe.js listens on document).
	n.addEventListener('touchstart', (e) => e.stopPropagation(), { passive: true });
	renderActions();
}

function renderActions() {
	const box = document.getElementById('upd-actions');
	if (!box || !rel) return;
	let h = '';
	if (phase === 'downloading') {
		h =
			'<div class="upd-track"><div class="upd-fill" id="upd-fill"></div></div>' +
			'<div class="upd-stats"><span id="upd-bytes"></span><span id="upd-speed"></span></div>' +
			'<button class="btn btn-ghost full" id="upd-cancel" data-action="cancel">' + esc(t('m.common.cancel')) + '</button>';
	} else if (phase === 'downloaded') {
		h = '<button class="btn btn-primary" id="upd-install">' + esc(t('m.update.install')) + '</button>';
		if (hint === 'perm') h += '<div class="upd-hint">' + esc(t('m.update.needsPermission')) + '</div>';
		else if (hint === 'sig') h += '<div class="upd-hint warn">' + esc(t('m.update.signatureMismatch')) + '</div>';
		else if (hint === 'fail') h += '<div class="upd-hint warn">' + esc(t('m.update.installFailed') + (hintMsg ? ' (' + hintMsg + ')' : '')) + '</div>';
		h += '<button class="upd-link" id="upd-redownload">' + esc(t('m.update.redownload')) + '</button>';
	} else if (phase === 'error') {
		h =
			'<div class="upd-hint warn">' + esc(t('m.update.failed') + (errMsg ? ' (' + errMsg + ')' : '')) + '</div>' +
			'<button class="btn btn-primary" id="upd-retry">' + esc(t('m.update.retry')) + '</button>';
	} else {
		h = '<button class="btn btn-primary" id="upd-download">' + esc(t('m.update.download')) + '</button>';
	}
	box.innerHTML = h;
	box.querySelector('#upd-download')?.addEventListener('click', () => download());
	box.querySelector('#upd-retry')?.addEventListener('click', () => download());
	box.querySelector('#upd-redownload')?.addEventListener('click', () => download(false));
	box.querySelector('#upd-install')?.addEventListener('click', install);
	box.querySelector('#upd-cancel')?.addEventListener('click', cancel);
	renderProgress();
}

function renderProgress() {
	const fill = document.getElementById('upd-fill');
	if (!fill) return;
	const total = prog.total || (rel && rel.size) || 0;
	fill.style.width = (total ? Math.min(100, (prog.received / total) * 100) : 0).toFixed(1) + '%';
	const bytes = document.getElementById('upd-bytes');
	if (bytes) bytes.textContent = t('m.update.progress', { got: mb(prog.received), total: mb(total) });
	const speed = document.getElementById('upd-speed');
	if (speed) {
		const parts = [];
		if (prog.bps > 0) parts.push(t('m.update.speed', { v: mb(prog.bps) }));
		const e = eta(total - prog.received, prog.bps);
		if (e) parts.push(e);
		speed.textContent = parts.join(' · ');
	}
}

// ---- Boot --------------------------------------------------------------------

/** Wire the indicator + start checking. No-op off Android (browser preview, desktop). */
export function initUpdate() {
	const btn = document.getElementById('update-btn');
	if (!btn) return;
	btn.addEventListener('click', openSheet);
	if (!isAndroid) return;
	check();
	// Long-lived app: re-check when brought back (the 6 h cache gate keeps it cheap).
	document.addEventListener('visibilitychange', () => {
		if (document.visibilityState === 'visible') { check(); if (rel) refreshStatus(); }
	});
	setInterval(check, 30 * 60 * 1000);
	window.addEventListener('langchange', () => { if (sheetOpen()) renderSheet(); });
}

// Test/preview hook: drive the sheet without Tauri (e.g. a headless screenshot run).
export const _debug = {
	show(r, cur) { current = cur || '0.13.0'; setRelease(r); },
	set(p, extra) {
		phase = p;
		Object.assign(prog, (extra && extra.prog) || {});
		hint = (extra && extra.hint) || '';
		errMsg = (extra && extra.err) || '';
		renderActions();
	},
	open: () => openSheet(),
};
