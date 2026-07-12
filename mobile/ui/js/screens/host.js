/**
 * screens/host.js — Cihazım (My Device / host) screen
 *
 * Full W3-host implementation per §2.7 + DT-host design spec.
 *
 * Features:
 *   - Online / offline toggle with live status indicator
 *   - Large stable device ID with copy + share
 *   - OTP chip with copy + rotate
 *   - Unattended-access toggle (reads cfg, no inline edit here — see settings)
 *   - Live connected-peers list with per-peer kick
 *   - Touch-first incoming-connection approval bottom-sheet with 30s countdown
 *   - Accessibility permission entry
 *
 * Events listened:
 *   host-password        { password }
 *   session-request      { reqId, peer, hasPassword }
 *   host-peer-connected  { sid, peer }
 *   host-peer-disconnected { sid }
 *
 * Commands called:
 *   go_online, go_offline, new_password,
 *   respond_request, disconnect_session,
 *   open_a11y_settings, a11y_enabled
 *
 * Design: indigo accent, large mono ID, bottom-sheet approval, ≥44px tap targets.
 */

import { registerScreen } from '../router.js';
import { invoke, listen, hasTauri, clipboard, share } from '../tauri.js';
import { t } from '../i18n.js';
import { getConfig } from '../store/config.js';

// ── State ─────────────────────────────────────────────────────────────────────

/** @type {{ sid: number, peer: string, elapsed: number }[]} */
let activePeers = [];

/** @type {{ reqId: number, peer: string, hasPassword: boolean, name?: string|null, id?: string|null } | null} */
let pendingRequest = null;

/** Auto-deny countdown timer id */
let countdownTimer = null;
let countdownRemaining = 30;

/** Whether we are currently online */
let isOnline = false;

/** Current device id (u32) or null */
let myId = null;

/** Current OTP */
let currentOtp = '';
// Offline (relay unreachable) LAN direct-connect address "ip:port", shown in place of
// the relay-only 9-digit id. Empty when registered online.
let myAddr = '';

/** Whether a go_online call is in flight */
let going = false;
// One-time guard: the a11y-sheet resume re-check listener is wired only once.
let wiredA11yResume = false;
let wiredNotifStop = false;

/** Elapsed seconds counter for peer rows */
let elapsedInterval = null;

// ── DOM helpers ───────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);

function qs(sel, ctx) {
	return (ctx || document).querySelector(sel);
}

// ── Toast ─────────────────────────────────────────────────────────────────────

let toastTimer = null;

function showToast(msg, durationMs = 2200) {
	let el = $('host-toast');
	if (!el) return;
	el.textContent = msg;
	el.classList.add('show');
	clearTimeout(toastTimer);
	toastTimer = setTimeout(() => el && el.classList.remove('show'), durationMs);
}

// ── ID / OTP formatting ───────────────────────────────────────────────────────

function fmtId(id) {
	// 9-digit → "123 456 789"
	return String(id).padStart(9, '0').replace(/(\d{3})(\d{3})(\d{3})/, '$1 $2 $3');
}

// ── Elapsed time label ────────────────────────────────────────────────────────

function fmtElapsed(sec) {
	if (sec < 60)  return t('host.elapsedSec', { n: sec });
	if (sec < 3600) return t('host.elapsedMin', { n: Math.floor(sec / 60) });
	return t('host.elapsedHour', { h: Math.floor(sec / 3600), m: Math.floor((sec % 3600) / 60) });
}

// ── Render helpers ────────────────────────────────────────────────────────────

/** Render the status pill + online/offline toggle button. */
function renderStatus() {
	const statusEl = $('h-status');
	const onlineBtn = $('h-online-btn');
	const idSection = $('h-id-section');
	const pwSection = $('h-pw-section');
	const peerSection = $('h-peer-section');

	if (!statusEl) return;

	// Connected-devices list is only meaningful while online — hide it when offline
	// (you can't have peers connected to an offline host).
	if (peerSection) peerSection.style.display = isOnline ? '' : 'none';

	if (isOnline) {
		statusEl.className = 'statusline live';
		statusEl.innerHTML = `<span class="d"></span><span>${t('status.online')}</span>`;
		if (onlineBtn) {
			onlineBtn.textContent = t('m.host.goOffline');
			onlineBtn.className = 'btn btn-danger full';
			onlineBtn.disabled = false;
		}
		if (idSection) idSection.style.display = '';
		if (pwSection) pwSection.style.display = '';
		showLocalIp();
		applyUnattendedHost();
	} else {
		const lip = $('h-local-ip');
		if (lip) lip.style.display = 'none';
		statusEl.className = 'statusline';
		statusEl.innerHTML = `<span class="d"></span><span>${t('status.offline')}</span>`;
		if (onlineBtn) {
			onlineBtn.textContent = going ? t('status.connecting') : t('m.host.goOnline');
			onlineBtn.className = 'btn btn-primary';
			onlineBtn.disabled = going;
		}
		if (idSection) idSection.style.display = going ? '' : 'none';
		if (pwSection) pwSection.style.display = going ? '' : 'none';
	}
}

/** Fetch + show this device's local LAN IP (so peers on the same network can
 *  connect by address). Best-effort; hidden if unavailable. */
async function showLocalIp() {
	const box = $('h-local-ip');
	const val = $('h-local-ip-val');
	if (!box || !val) return;
	try {
		const ip = await invoke('local_ip');
		if (ip) {
			// Append the node's bound port so peers can reach this device by IP:port.
			let txt = ip;
			try { const p = await invoke('node_port'); if (p && p > 0) txt = `${ip}:${p}`; } catch (_) {}
			val.textContent = txt; box.style.display = '';
		} else box.style.display = 'none';
	} catch (_) { box.style.display = 'none'; }
}

/** When unattended access is on the host issues no one-time password — show a
 *  warning banner in place of the OTP row (desktop SelfCard parity). */
async function applyUnattendedHost() {
	let on = false;
	try { on = !!(await getConfig()).unattendedAccess; } catch (_) {}
	const info = $('h-unattended-info');
	const row  = $('h-otp-row');
	if (info) info.style.display = on ? '' : 'none';
	if (row)  row.style.display  = on ? 'none' : 'flex';
}

/** Render the device ID block. */
function renderId() {
	const idEl = $('h-id');
	if (!idEl) return;
	if (myAddr) {
		// Offline: the 9-digit id only resolves via the relay, so show the LAN
		// direct-connect address instead.
		idEl.textContent = myAddr;
		idEl.classList.remove('ph');
	} else if (myId) {
		idEl.textContent = fmtId(myId);
		idEl.classList.remove('ph');
	} else {
		idEl.textContent = '--- --- ---';
		idEl.classList.add('ph');
	}
}

/** Render the OTP chip. */
function renderOtp() {
	const otpEl = $('h-otp');
	if (otpEl) otpEl.textContent = currentOtp || '----';
}

/** Render the peer list. */
function renderPeers() {
	const listEl = $('h-peer-list');
	const emptyEl = $('h-peer-empty');
	if (!listEl) return;

	if (activePeers.length === 0) {
		listEl.innerHTML = '';
		if (emptyEl) emptyEl.style.display = '';
		return;
	}

	if (emptyEl) emptyEl.style.display = 'none';

	listEl.innerHTML = activePeers.map(p => `
		<div class="item h-peer-row" data-sid="${p.sid}">
			<div class="ic">
				<svg viewBox="0 0 24 24" fill="none" width="20" height="20">
					<rect x="6" y="3" width="12" height="18" rx="2.5"
						stroke="currentColor" stroke-width="2"/>
					<path d="M11 18h2" stroke="currentColor"
						stroke-width="2" stroke-linecap="round"/>
				</svg>
			</div>
			<div class="meta">
				<div class="id">${p.peer}</div>
				<div class="when">${fmtElapsed(p.elapsed)}</div>
			</div>
			<button
				class="btn btn-ghost h-kick-btn"
				data-sid="${p.sid}"
				aria-label="${t('home.kick')} — ${p.peer}"
				style="padding:10px 14px;font-size:13px;"
			>${t('home.kickLabel')}</button>
		</div>
	`).join('');

	// Attach kick handlers.
	listEl.querySelectorAll('.h-kick-btn').forEach(btn => {
		btn.addEventListener('click', (e) => {
			e.stopPropagation();
			const sid = Number(btn.dataset.sid);
			kickPeer(sid);
		});
	});
}

// ── Approval sheet ────────────────────────────────────────────────────────────

function openApprovalSheet(req) {
	pendingRequest = req;
	countdownRemaining = 30;

	const sheet = $('h-req-sheet');
	const backdrop = $('h-req-backdrop');
	if (!sheet || !backdrop) return;

	// Populate.
	const nameEl   = $('h-req-name');
	const detailEl = $('h-req-detail');
	const pwEl     = $('h-req-pw-status');
	const denyCnt  = $('h-req-deny-cnt');

	// Identity: prefer the beacon name (present only when the host verified it belongs to
	// THIS peer), then the relay id (grouped), then the ip. The most human of those is the
	// primary line; whatever's left goes on the detail line.
	const idFmt = req.id ? fmtId(req.id) : '';
	const primary = req.name || idFmt || req.peer;
	const rest = [];
	if (req.name && idFmt) rest.push(idFmt);
	if (primary !== req.peer) rest.push(req.peer);
	if (nameEl) nameEl.textContent = primary;
	if (detailEl) detailEl.textContent = rest.join(' · ');

	if (pwEl) pwEl.textContent = req.hasPassword ? t('approve.pwOk') : t('approve.pwNone');

	// The auto-deny countdown lives ON the Deny button now, e.g. "Reddet (20s)".
	const setDeny = () => { if (denyCnt) denyCnt.textContent = '(' + countdownRemaining + 's)'; };
	setDeny();

	sheet.classList.add('open');
	backdrop.classList.add('open');

	// Start 30s countdown.
	clearInterval(countdownTimer);
	countdownTimer = setInterval(() => {
		countdownRemaining--;
		setDeny();
		if (countdownRemaining <= 0) {
			clearInterval(countdownTimer);
			countdownTimer = null;
			closeApprovalSheet();
		}
	}, 1000);
}

function closeApprovalSheet() {
	clearInterval(countdownTimer);
	countdownTimer = null;
	pendingRequest = null;

	const sheet = $('h-req-sheet');
	const backdrop = $('h-req-backdrop');
	if (sheet) sheet.classList.remove('open');
	if (backdrop) backdrop.classList.remove('open');
}

// ── Commands ──────────────────────────────────────────────────────────────────

// Tap "Go online": gate on BOTH permissions the remote-control experience needs —
//   • accessibility  → lets the peer TOUCH/control (required for any control)
//   • draw-over-apps  → shows the injected CURSOR on-screen (optional; captured into
//                       the stream so the operator can aim)
// Missing accessibility OR (missing cursor overlay and not yet dismissed) → show the
// sheet so the user grants what's missing. Granting accessibility alone used to slip
// straight online, never prompting for the cursor overlay — now it re-prompts for the
// cursor. "Go online anyway" dismisses the cursor prompt so it never nags again.
async function goOnline() {
	if (going || isOnline) return;
	let a11y = false, overlay = false;
	if (hasTauri) {
		try { a11y = await invoke('a11y_enabled', {}); } catch (_) {}
		try { overlay = await invoke('overlay_granted', {}); } catch (_) {}
	}
	const cursorDismissed = localStorage.getItem('pulsar.cursorPromptDismissed') === '1';
	if (!a11y || (!overlay && !cursorDismissed)) { openA11ySheet(a11y, overlay); return; }
	doGoOnline();
}

// Reflect the live permission state in the sheet: a ✓/✕ per row, and only surface the
// grant button for a permission that's still missing. The cursor row/button stays hidden
// unless accessibility is ALREADY granted and the overlay still isn't — on stock Android
// enabling the accessibility service auto-grants draw-over-apps, so the cursor prompt is
// only a fallback for OEMs that skip that auto-grant.
function openA11ySheet(a11y = false, overlay = false) {
	const sheet = $('h-a11y-sheet');
	const backdrop = $('h-a11y-backdrop');
	if (!sheet || !backdrop) { doGoOnline(); return; } // sheet missing → don't block
	// Row status marks.
	const ctlMark = $('h-a11y-ctl-mark');
	if (ctlMark) { ctlMark.textContent = a11y ? '✓' : '✕'; ctlMark.style.color = a11y ? 'var(--ok)' : 'var(--danger)'; }
	const curMark = $('h-a11y-cur-mark');
	if (curMark) { curMark.textContent = overlay ? '✓' : '🖱️'; }
	const needCursor = a11y && !overlay;
	const curRow = $('h-a11y-cur-row'); if (curRow) curRow.style.display = needCursor ? 'flex' : 'none';
	// Hide a grant button once its permission is granted (cursor: only in the fallback case).
	const gA = $('h-a11y-grant');   if (gA) gA.style.display = a11y ? 'none' : '';
	const gC = $('h-a11y-cursor');  if (gC) gC.style.display = needCursor ? '' : 'none';
	sheet.classList.add('open');
	backdrop.classList.add('open');
}

function closeA11ySheet() {
	$('h-a11y-sheet')?.classList.remove('open');
	$('h-a11y-backdrop')?.classList.remove('open');
}

// (Swipe-down-to-dismiss is now handled globally for ALL sheets in sheet-swipe.js.)

async function doGoOnline() {
	if (going) return;
	going = true;
	renderStatus();

	// Android 13+: incoming-connection notifications need the POST_NOTIFICATIONS
	// runtime grant; ask now (fires the system dialog once) so a backgrounded host
	// can actually alert on connection requests.
	if (hasTauri) invoke('notif_permission', {}).catch(() => {});

	try {
		// Read relay/name/netmode from localStorage (set by config screen).
		const relay   = localStorage.getItem('pulsar.relay')   || '';
		const name    = localStorage.getItem('pulsar.name')    || '';
		const netmode = localStorage.getItem('pulsar.netmode') || 'auto';
		const r = await invoke('go_online', { relay, name, netmode });
		if (r && r.ok) {
			isOnline = true;
			myId = r.id;
			myAddr = r.addr || ''; // set only when the relay was unreachable (offline)
			currentOtp = r.password;
			renderId();
			renderOtp();
			// Start elapsed counter.
			startElapsedCounter();
		} else {
			showToast(t('m.error'));
		}
	} catch (e) {
		showToast(t('m.error') + ': ' + String(e));
	} finally {
		going = false;
		renderStatus();
	}
}

async function goOffline() {
	try {
		await invoke('go_offline', {});
	} catch (_) {}

	isOnline = false;
	myId = null;
	myAddr = '';
	currentOtp = '';
	activePeers = [];
	stopElapsedCounter();
	renderId();
	renderOtp();
	renderPeers();
	renderStatus();
}

async function rotatePassword() {
	try {
		const r = await invoke('new_password', {});
		if (r && r.password) {
			currentOtp = r.password;
			renderOtp();
			showToast(t('m.host.password') + ': ' + r.password);
		}
	} catch (e) {
		showToast(t('m.error'));
	}
}

async function kickPeer(sid) {
	try {
		await invoke('disconnect_session', { sid });
	} catch (_) {}
	// Optimistic removal (event will confirm).
	activePeers = activePeers.filter(p => p.sid !== sid);
	renderPeers();
}

async function respondRequest(allow) {
	const req = pendingRequest;
	closeApprovalSheet();
	if (!req) return;
	try {
		await invoke('respond_request', { reqId: req.reqId, allow });
	} catch (_) {}
}

async function refreshA11y() {
	if (!hasTauri) return;
	try {
		const on = await invoke('a11y_enabled', {});
		const statusEl = $('ctrl-status');
		const openBtn  = $('h-a11y-btn');
		if (statusEl) {
			statusEl.textContent = on
				? t('m.host.a11yOn')
				: t('m.host.a11yHint');
		}
		if (openBtn) {
			openBtn.textContent = on
				? t('m.host.a11yOnBtn')
				: t('m.host.a11yOpen');
			openBtn.disabled = on;
		}
	} catch (_) {}
}

// ── Elapsed counter ───────────────────────────────────────────────────────────

function startElapsedCounter() {
	stopElapsedCounter();
	elapsedInterval = setInterval(() => {
		activePeers.forEach(p => { p.elapsed++; });
		renderPeers();
	}, 1000);
}

function stopElapsedCounter() {
	if (elapsedInterval != null) {
		clearInterval(elapsedInterval);
		elapsedInterval = null;
	}
}

// ── Copy / share helpers ──────────────────────────────────────────────────────

async function copyId() {
	const val = myAddr || (myId != null ? fmtId(myId) : '');
	if (!val) return;
	await clipboard(val);
	showToast(t('m.copied'));
}

async function shareId() {
	const val = myAddr || (myId != null ? fmtId(myId) : '');
	if (!val) return;
	const id = val;
	// Copy to the clipboard AND open the system share sheet.
	await clipboard(id);
	showToast(t('m.copied'));
	await share({ title: 'Pulsar', text: 'Pulsar ID: ' + id });
}

async function copyOtp() {
	if (!currentOtp) return;
	await clipboard(currentOtp);
	showToast(t('m.copied'));
}

// ── Mount DOM ─────────────────────────────────────────────────────────────────

function buildDOM(root) {
	root.innerHTML = `
<div class="host-screen" style="animation:rise 0.4s var(--ease) both">

  <!-- Title -->
  <h2 class="title">${t('nav.host')}</h2>
  <p class="sub">${t('m.host.sub')}</p>

  <!-- Status card -->
  <div class="card" style="gap:12px">
    <div id="h-status" class="statusline">
      <span class="d"></span>
      <span>${t('status.offline')}</span>
    </div>

    <!-- ID block (hidden until online) -->
    <div id="h-id-section" style="display:none;text-align:center">
      <div class="sect-label" style="margin-bottom:6px">${t('m.host.online')}</div>
      <div id="h-id" class="idbig ph">--- --- ---</div>
      <div id="h-local-ip" style="margin-top:8px;font-size:12.5px;color:var(--text-muted);display:none">
        <span data-i18n="m.host.localAddr">${t('m.host.localAddr')}</span> · <span id="h-local-ip-val" class="mono"></span>
      </div>
      <!-- ID action row -->
      <div style="display:flex;gap:10px;margin-top:12px;justify-content:center">
        <button id="h-copy-id-btn" class="btn btn-ghost"
          aria-label="${t('home.copyId')}" style="flex:1;max-width:140px">
          <svg viewBox="0 0 24 24" fill="none" width="17" height="17">
            <rect x="9" y="9" width="11" height="11" rx="2" stroke="currentColor" stroke-width="1.8"/>
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" stroke="currentColor" stroke-width="1.8"/>
          </svg>
          ${t('m.copy')}
        </button>
        <button id="h-share-id-btn" class="btn btn-ghost"
          aria-label="${t('m.host.share')}" style="flex:1;max-width:140px">
          <svg viewBox="0 0 24 24" fill="none" width="17" height="17">
            <path d="M4 12v7a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-7M16 6l-4-4-4 4M12 2v13"
              stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>
          </svg>
          ${t('m.host.share')}
        </button>
      </div>
    </div>

    <!-- OTP block -->
    <div id="h-pw-section" style="display:none">
      <div class="sect-label" style="margin-bottom:6px">${t('m.host.password')}</div>
      <div id="h-unattended-info" style="display:none">
        <div class="host-unattended-banner">
          <strong data-i18n="m.host.unattendedOn">${t('m.host.unattendedOn')}</strong>
          <span data-i18n="m.host.unattendedOnHint">${t('m.host.unattendedOnHint')}</span>
        </div>
      </div>
      <div id="h-otp-row" style="display:flex;align-items:center;gap:10px">
        <div id="h-otp" class="pw-chip" style="flex:1;min-width:0;text-align:center;letter-spacing:0.04em;font-size:17px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">
          ----
        </div>
        <button id="h-copy-otp-btn" class="icon-btn"
          title="${t('m.copy')}" aria-label="${t('m.copy')} OTP"
          style="background:var(--accent-soft);color:var(--accent)">
          <svg viewBox="0 0 24 24" fill="none" width="18" height="18">
            <rect x="9" y="9" width="11" height="11" rx="2" stroke="currentColor" stroke-width="1.8"/>
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" stroke="currentColor" stroke-width="1.8"/>
          </svg>
        </button>
        <button id="h-rotate-otp-btn" class="icon-btn"
          title="${t('m.host.rotate')}" aria-label="${t('m.host.rotate')}"
          style="background:var(--accent-soft);color:var(--accent)">
          <svg viewBox="0 0 24 24" fill="none" width="18" height="18">
            <path d="M1 4v6h6M23 20v-6h-6" stroke="currentColor" stroke-width="1.8"
              stroke-linecap="round" stroke-linejoin="round"/>
            <path d="M20.49 9A9 9 0 0 0 5.64 5.64L1 10m22 4-4.64 4.36A9 9 0 0 1 3.51 15"
              stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>
          </svg>
        </button>
      </div>
    </div>

    <!-- Online / offline toggle -->
    <button id="h-online-btn" class="btn btn-primary">
      ${t('m.host.goOnline')}
    </button>
  </div>

  <!-- Connected peers (only meaningful while online) -->
  <div class="card" id="h-peer-section" style="margin-top:14px">
    <div class="sect-label">${t('m.host.peerList')}</div>
    <div id="h-peer-list" class="row-list"></div>
    <div id="h-peer-empty" class="empty" style="padding:20px 0 4px">
      ${t('m.host.noPeers')}
    </div>
  </div>

  <!-- Accessibility permission is no longer a persistent card here; it is asked
       for via the warning sheet (#h-a11y-sheet) when the user taps Go online. -->

</div>

<!-- Toast -->
<div id="host-toast" class="toast" role="status" aria-live="polite"></div>

<!-- ── Approval sheet ───────────────────────────────────────────────── -->
<div id="h-req-backdrop" class="sheet-backdrop"></div>
<div id="h-req-sheet" class="sheet" role="dialog"
     aria-modal="true" aria-labelledby="h-req-title">
  <div class="sheet-handle"></div>

  <!-- Header -->
  <div>
    <h3 id="h-req-title" style="font-family:var(--font-display);font-size:19px;
        font-weight:700;letter-spacing:-0.02em;margin:0 0 4px">
      ${t('m.host.reqTitle')}
    </h3>
    <p style="margin:0;font-size:13.5px;color:var(--text-muted)">
      ${t('approve.lead')}
    </p>
  </div>

  <!-- Peer identity block -->
  <div style="background:var(--accent-soft);border:1px solid var(--accent-soft-2);
      border-radius:var(--r-lg);padding:14px 16px;display:flex;align-items:center;gap:12px">
    <div style="width:40px;height:40px;border-radius:12px;background:var(--accent-soft-2);
        color:var(--accent);display:grid;place-items:center;flex:none">
      <svg viewBox="0 0 24 24" fill="none" width="22" height="22">
        <rect x="6" y="3" width="12" height="18" rx="2.5"
          stroke="currentColor" stroke-width="2"/>
        <path d="M11 18h2" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>
      </svg>
    </div>
    <div style="flex:1;min-width:0">
      <div style="font-size:11px;font-weight:700;letter-spacing:0.05em;
          text-transform:uppercase;color:var(--text-faint);margin-bottom:2px">
        ${t('approve.deviceId')}
      </div>
      <div id="h-req-name"
        style="font-size:15px;font-weight:600;color:var(--text);word-break:break-word">
        —
      </div>
      <div id="h-req-detail" class="mono"
        style="font-size:12.5px;color:var(--text-muted);word-break:break-all;margin-top:2px"></div>
    </div>
  </div>

  <!-- Password status — plain text (no chip background), wraps on small screens -->
  <div id="h-req-pw-status"
    style="align-self:stretch;font-size:12.5px;color:var(--text-muted);line-height:1.4">
    ${t('approve.pwNone')}
  </div>

  <!-- Action buttons — stacked for thumb reach -->
  <div style="display:flex;flex-direction:column;gap:10px">
    <button id="h-req-allow" class="btn btn-primary"
      style="background:var(--ok);box-shadow:none;font-size:17px;padding:16px">
      <svg viewBox="0 0 24 24" fill="none" width="20" height="20">
        <path d="M20 6 9 17l-5-5" stroke="currentColor"
          stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>
      </svg>
      ${t('m.host.allow')}
    </button>
    <button id="h-req-deny" class="btn btn-ghost full"
      style="border-color:var(--danger);color:var(--danger);font-size:17px;padding:16px">
      <svg viewBox="0 0 24 24" fill="none" width="20" height="20">
        <path d="M18 6 6 18M6 6l12 12" stroke="currentColor"
          stroke-width="2.2" stroke-linecap="round"/>
      </svg>
      ${t('m.host.deny')} <span id="h-req-deny-cnt" style="opacity:0.8;font-weight:600">(30s)</span>
    </button>
  </div>
</div>

<!-- ── Accessibility (remote-control) warning sheet — shown on Go online ── -->
<div id="h-a11y-backdrop" class="sheet-backdrop"></div>
<div id="h-a11y-sheet" class="sheet" role="dialog" aria-modal="true" aria-labelledby="h-a11y-title">
  <div class="sheet-handle"></div>
  <div>
    <h3 id="h-a11y-title" style="font-family:var(--font-display);font-size:19px;
        font-weight:700;letter-spacing:-0.02em;margin:0 0 6px">
      ${t('a11y.warnTitle')}
    </h3>
    <p class="msg" style="text-align:left;margin:0 0 12px">${t('a11y.warnLead')}</p>
    <div style="display:flex;flex-direction:column;gap:8px;margin-bottom:4px">
      <div style="display:flex;align-items:center;gap:10px;font-size:14.5px">
        <span id="h-a11y-ctl-mark" style="font-weight:700;font-size:17px;min-width:18px;text-align:center;color:var(--danger)">✕</span>
        <span>${t('a11y.rowControl')}</span>
      </div>
      <div id="h-a11y-cur-row" style="display:none;align-items:center;gap:10px;font-size:14.5px">
        <span id="h-a11y-cur-mark" style="font-weight:700;font-size:16px;min-width:18px;text-align:center">🖱️</span>
        <span>${t('a11y.rowCursor')}</span>
      </div>
    </div>
  </div>
  <div style="display:flex;flex-direction:column;gap:10px">
    <button id="h-a11y-grant" class="btn btn-primary" style="font-size:16px;padding:15px">
      ${t('a11y.grant')}
    </button>
    <button id="h-a11y-cursor" class="btn btn-ghost full" style="font-size:15px;padding:13px">
      ${t('a11y.grantCursor')}
    </button>
    <button id="h-a11y-continue" class="btn btn-ghost full" style="font-size:16px;padding:15px">
      ${t('a11y.goAnyway')}
    </button>
  </div>
</div>
`;
}

// ── Wire up events ────────────────────────────────────────────────────────────

function wireButtons() {
	// Online / offline toggle.
	$('h-online-btn')?.addEventListener('click', () => {
		if (isOnline) {
			goOffline();
		} else {
			goOnline();
		}
	});

	// Copy + share ID.
	$('h-copy-id-btn')?.addEventListener('click', copyId);
	$('h-share-id-btn')?.addEventListener('click', shareId);

	// Copy + rotate OTP.
	$('h-copy-otp-btn')?.addEventListener('click', copyOtp);
	$('h-rotate-otp-btn')?.addEventListener('click', rotatePassword);

	// Accessibility warning sheet (shown on Go online when a permission is off).
	// Keep the sheet OPEN while the user is away in system settings: on return the
	// visibilitychange handler re-queries + refreshes the ✓/✕ marks and hides this
	// button once granted, then naturally surfaces the cursor-overlay grant next —
	// instead of the old behaviour where pressing grant dismissed the sheet and the
	// user slipped online having granted only accessibility.
	$('h-a11y-grant')?.addEventListener('click', async () => {
		try { await invoke('open_a11y_settings', {}); } catch (_) {}
	});
	// Optional: grant the overlay ("draw over other apps") permission so the remote
	// operator SEES a pointer on the phone (the injected cursor). Control still works
	// without it — this only makes the cursor visible in the stream.
	$('h-a11y-cursor')?.addEventListener('click', async () => {
		try { await invoke('request_overlay', {}); } catch (_) {}
	});
	// Returning from a system settings screen (accessibility / draw-over): if the sheet is
	// still open, re-query both permissions and refresh its ✓/✕ marks + grant buttons live,
	// so the user sees what they just granted without re-opening the sheet.
	if (!wiredA11yResume) {
		wiredA11yResume = true;
		document.addEventListener('visibilitychange', async () => {
			if (document.visibilityState !== 'visible') return;
			if (!$('h-a11y-sheet')?.classList.contains('open')) return;
			let a11y = false, overlay = false;
			if (hasTauri) {
				try { a11y = await invoke('a11y_enabled', {}); } catch (_) {}
				try { overlay = await invoke('overlay_granted', {}); } catch (_) {}
			}
			// Both granted (accessibility usually auto-grants the overlay) → done, go online.
			if (a11y && overlay) { closeA11ySheet(); doGoOnline(); return; }
			openA11ySheet(a11y, overlay);
		});
	}
	$('h-a11y-continue')?.addEventListener('click', () => {
		// "Go online anyway" = the user opted out of the cursor overlay; stop nagging for it
		// on future go-onlines (accessibility is still re-checked each time — it's required).
		try { localStorage.setItem('pulsar.cursorPromptDismissed', '1'); } catch (_) {}
		closeA11ySheet();
		doGoOnline();
	});
	$('h-a11y-backdrop')?.addEventListener('click', closeA11ySheet);

	// "Stop sharing" action on the Android foreground-service notification: the
	// Kotlin layer dispatches this window event (via evaluateJavascript) — kick every
	// active session (ends the capture + service notification with it) but STAY online,
	// so the device remains reachable for new requests.
	if (!wiredNotifStop) {
		wiredNotifStop = true;
		window.addEventListener('pulsar-stop-host', () => {
			for (const p of [...activePeers]) kickPeer(p.sid);
		});
	}

	// Approval sheet.
	$('h-req-allow')?.addEventListener('click', () => respondRequest(true));
	$('h-req-deny')?.addEventListener('click',  () => respondRequest(false));
	$('h-req-backdrop')?.addEventListener('click', () => respondRequest(false));
}

// ── Tauri event subscriptions ─────────────────────────────────────────────────

const unlisteners = [];

async function subscribeEvents() {
	if (!hasTauri) return;

	unlisteners.push(
		await listen('host-password', (payload) => {
			if (payload && payload.password) {
				currentOtp = payload.password;
				renderOtp();
			}
		})
	);

	unlisteners.push(
		await listen('session-request', (payload) => {
			if (!payload) return;
			openApprovalSheet({
				reqId:       payload.reqId || payload.req_id,
				peer:        payload.peer || '?',
				hasPassword: !!(payload.hasPassword || payload.has_password),
			});
		})
	);

	unlisteners.push(
		await listen('host-peer-connected', (payload) => {
			if (!payload) return;
			const sid = payload.sid;
			if (!activePeers.find(p => p.sid === sid)) {
				activePeers.push({ sid, peer: payload.peer || '?', elapsed: 0 });
			}
			renderPeers();
		})
	);

	unlisteners.push(
		await listen('host-peer-disconnected', (payload) => {
			if (!payload) return;
			activePeers = activePeers.filter(p => p.sid !== payload.sid);
			renderPeers();
			// If the approval sheet was for this peer (auto-dismiss).
			if (pendingRequest) closeApprovalSheet();
		})
	);
}

// ── Visibility-change refresh (a11y, etc.) ────────────────────────────────────

function onVisible() {
	if (!document.hidden) refreshA11y();
}

// ── Screen registration ───────────────────────────────────────────────────────

let mounted = false;

// Sheets that must live on <body> (see relocation note below), so they're removed +
// recreated together on every (re)build.
const HOST_SHEET_IDS = ['h-req-backdrop', 'h-req-sheet', 'h-a11y-backdrop', 'h-a11y-sheet'];

// Build (or REBUILD) the tab's DOM + wire it + render current state. Re-runnable: the
// template uses inline t(), so applyI18n (which only touches data-i18n nodes) can't
// re-translate it on a language change — we rebuild the whole tab instead. Event
// SUBSCRIPTIONS (Tauri events, visibilitychange, langchange) stay in mount() so they're
// wired exactly once; this only rebuilds the view + re-attaches DOM click handlers to
// the fresh elements (the old ones are discarded with the cleared container).
function buildContent() {
	const container = document.getElementById('t-host');
	if (!container) return;
	// Drop any previously-relocated sheets so a rebuild doesn't leave duplicates on <body>.
	HOST_SHEET_IDS.forEach((id) => document.getElementById(id)?.remove());
	// Clear the static HTML stub (index.html) / the prior build, then rebuild.
	container.innerHTML = '';
	buildDOM(container);
	// The approval + accessibility sheets are position:fixed viewport overlays. #t-host is
	// a .tab carrying `animation: rise …` (a transform), which makes it a containing block
	// for fixed descendants — left inside the tab, translateY(100%) doesn't hide them and
	// they render inline at the bottom. Relocate to <body> so they're true full-screen modals.
	HOST_SHEET_IDS.forEach((id) => {
		const el = document.getElementById(id);
		if (el) document.body.appendChild(el);
	});
	wireButtons();
	renderStatus();
	renderId();
	renderOtp();
	renderPeers();
	applyUnattendedHost();
}

function mount() {
	if (mounted) return;
	mounted = true;

	buildContent();

	// Subscribe to Tauri events (once).
	subscribeEvents();

	// Refresh a11y status on first mount and on re-focus.
	refreshA11y();
	document.addEventListener('visibilitychange', onVisible);

	// Re-translate on language change: the template is rendered with inline t(), so the
	// only way to switch its language is to rebuild it (router re-runs onShow, not mount).
	window.addEventListener('langchange', () => {
		if (mounted) { buildContent(); refreshA11y(); }
	});

	// Game mode is a CLIENT-ONLY personality: switching into it must stop hosting —
	// end every active session and go fully offline (unregister from the relay) so the
	// device neither streams nor accepts new connection requests while gaming.
	// (host.js is premounted at boot, so this listener is live from app start.)
	window.__pulsarBus?.addEventListener('mode-changed', (e) => {
		if (e.detail === 'game' && isOnline) goOffline();
	});
}

function onShow() {
	// Re-check a11y whenever the user navigates to this tab.
	refreshA11y();
	// The unattended toggle lives in Settings — re-sync the OTP/warning on return.
	applyUnattendedHost();
}

registerScreen({
	id:          'host',
	navIcon: `<svg viewBox="0 0 24 24" fill="none">
		<rect x="6" y="3" width="12" height="18" rx="2.5"
			stroke="currentColor" stroke-width="2"/>
		<path d="M11 18h2" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>
	</svg>`,
	navLabel:    t('nav.host'),
	navLabelKey: 'nav.host',
	mount,
	onShow,
});
