/**
 * call.js — voice calls, like a phone (Rust: src/call.rs; protocol: pulsar-core `call`).
 *
 * Works the same whether this phone is the client (side 'client', id = pane slot) or the
 * host (side 'host', id = session sid):
 *   - A call card fixed at the top of the screen, on every screen and in-session:
 *     incoming → caller name + Answer / Decline; outgoing → "Calling…" + Cancel;
 *     active → talk time + Hang up.
 *   - The in-session overlay gets a "Voice call" card (Audio section) to ring the host.
 *   - The host screen's peer rows get a call button (`callFor`, used by screens/host.js).
 *   - The ringtone setting (`ringtone` pref) is pushed to Rust at start and on change.
 *
 * Events (Rust → JS):
 *   call-state   { side, id, phase: 'idle'|'outgoing'|'incoming'|'active', peer, since,
 *                  supported }  — supported: the peer takes calls (an older Pulsar never
 *                  answers, so calling is not offered to it)
 *   call-notice  { kind: 'call-missed'|'call-declined'|'call-noanswer'|'call-ended', key }
 * Commands: call_action { side, id, action }, call_states, set_ringtone { tone },
 *   ringtone_preview { tone, stop }.
 */

import { invoke, listen } from './tauri.js';
import { t } from './i18n.js';
import { toast } from './toast.js';
import { getPref, setPref } from './store/prefs.js';

/** Built-in tones + the phone's own ringtone (`system`, the default) + silent. */
export const RINGTONES = ['system', 'classic', 'chime', 'pulse', 'none'];

/** key "side:id" → { side, id, phase, peer, since } (non-idle calls only). */
const calls = new Map();
/** key → the peer takes calls. */
const support = new Map();
/** Subscribers (host.js re-renders its call buttons). */
const subs = new Set();
let _slot = 0;
let _timer = null;

const keyOf = (side, id) => side + ':' + id;

/** The call phase of a session ('idle' when none). */
export function callFor(side, id) {
	return calls.get(keyOf(side, id))?.phase || 'idle';
}

/** Whether the peer of a session takes calls (said so when the session came up). */
export function callable(side, id) {
	return support.get(keyOf(side, id)) === true || callFor(side, id) !== 'idle';
}

/** Re-run `cb` whenever any call changes. Returns an unsubscribe. */
export function onCalls(cb) {
	subs.add(cb);
	return () => subs.delete(cb);
}

/** Ring / answer / decline / hang up. */
export function callAction(side, id, action) {
	return invoke('call_action', { side, id, action }).catch(() => {});
}

/** The phone-button action for a session's current phase. */
export function primaryAction(phase) {
	return phase === 'idle' ? 'start' : phase === 'incoming' ? 'accept' : 'hangup';
}

/** The ringtone setting. */
export function ringtone() {
	const v = getPref('ringtone');
	return RINGTONES.includes(v) ? v : 'system';
}

export function setRingtone(tone) {
	setPref('ringtone', tone);
	invoke('set_ringtone', { tone }).catch(() => {});
	invoke('ringtone_preview', { tone, stop: tone === 'none' }).catch(() => {});
}

// ── Rendering ────────────────────────────────────────────────────────────────

function talk(since) {
	const s = Math.max(0, Math.floor((Date.now() - (since || 0)) / 1000));
	return Math.floor(s / 60) + ':' + String(s % 60).padStart(2, '0');
}

function button(cls, label, action, c) {
	const b = document.createElement('button');
	b.type = 'button';
	b.className = 'call-btn ' + cls;
	b.setAttribute('aria-label', label);
	b.textContent = label;
	b.addEventListener('click', (e) => {
		e.stopPropagation();
		callAction(c.side, c.id, action);
	});
	return b;
}

function render() {
	let root = document.getElementById('call-cards');
	if (!root) {
		root = document.createElement('div');
		root.id = 'call-cards';
		root.setAttribute('role', 'region');
		root.setAttribute('aria-live', 'assertive');
		document.body.appendChild(root);
	}
	root.textContent = '';
	for (const c of calls.values()) {
		const card = document.createElement('div');
		card.className = 'call-card ' + c.phase;
		const txt = document.createElement('div');
		txt.className = 'call-txt';
		const ttl = document.createElement('div');
		ttl.className = 'call-ttl';
		ttl.textContent =
			c.phase === 'incoming' ? t('call.incoming')
			: c.phase === 'outgoing' ? t('call.calling')
			: t('call.inCall') + ' · ' + talk(c.since);
		const who = document.createElement('div');
		who.className = 'call-who';
		who.textContent = c.peer || t('call.peer');
		txt.append(ttl, who);
		card.appendChild(txt);
		if (c.phase === 'incoming') {
			card.append(button('no', t('call.decline'), 'decline', c), button('ok', t('call.answer'), 'accept', c));
		} else {
			card.appendChild(button('no', c.phase === 'outgoing' ? t('call.cancel') : t('call.hangup'), 'hangup', c));
		}
		root.appendChild(card);
	}
	root.style.display = calls.size ? 'flex' : 'none';
	// The talk timer ticks while a call is up.
	const active = [...calls.values()].some((c) => c.phase === 'active');
	if (active && !_timer) _timer = setInterval(render, 1000);
	if (!active && _timer) {
		clearInterval(_timer);
		_timer = null;
	}
	renderSessionCard();
	for (const cb of subs) {
		try { cb(); } catch (_) {}
	}
}

function apply(s) {
	const k = keyOf(s.side, s.id);
	if (typeof s.supported === 'boolean') support.set(k, s.supported);
	if (s.phase === 'idle') calls.delete(k);
	else calls.set(k, s);
	render();
}

// ── In-session overlay card (Audio section): ring the host ───────────────────

let _cardEl = null;

function renderSessionCard() {
	if (!_cardEl) return;
	const phase = callFor('client', _slot);
	const ok = callable('client', _slot);
	_cardEl.textContent = '';
	const row = document.createElement('div');
	row.className = 'call-ov-row';
	const lbl = document.createElement('div');
	lbl.className = 'call-ov-lbl';
	lbl.textContent =
		!ok ? t('call.na')
		: phase === 'idle' ? t('call.ovIdle')
		: phase === 'outgoing' ? t('call.calling')
		: phase === 'incoming' ? t('call.incoming')
		: t('call.inCall');
	const b = document.createElement('button');
	b.type = 'button';
	b.className = 'call-btn ' + (phase === 'idle' || phase === 'incoming' ? 'ok' : 'no');
	b.textContent =
		phase === 'idle' ? t('call.start')
		: phase === 'incoming' ? t('call.answer')
		: phase === 'outgoing' ? t('call.cancel')
		: t('call.hangup');
	b.addEventListener('click', () => callAction('client', _slot, primaryAction(phase)));
	b.disabled = !ok;
	if (!ok) b.style.opacity = '0.4';
	row.append(lbl, b);
	_cardEl.appendChild(row);
}

function registerSessionCard() {
	const tryReg = () => {
		const overlay = window.__pulsarOverlay;
		if (!overlay || typeof overlay.registerCard !== 'function') return false;
		overlay.registerCard({
			id: 'call',
			modes: ['remote', 'game'],
			section: 'audio',
			order: 5,
			label: () => t('call.cardLabel'),
			mount(container) {
				_cardEl = document.createElement('div');
				_cardEl.className = 'call-ov';
				container.appendChild(_cardEl);
				renderSessionCard();
			},
		});
		return true;
	};
	if (!tryReg()) window.addEventListener('load', () => tryReg(), { once: true });
	const tryBus = () => {
		const bus = window.__pulsarBus;
		if (!bus) return false;
		bus.on('session-started', ({ slot }) => {
			_slot = slot ?? 0;
			renderSessionCard();
		});
		return true;
	};
	if (!tryBus()) window.addEventListener('load', () => tryBus(), { once: true });
}

// ── Styles (plain string: no template literal, see CLAUDE.md) ────────────────

function injectStyles() {
	if (document.getElementById('call-styles')) return;
	const st = document.createElement('style');
	st.id = 'call-styles';
	st.textContent = [
		'#call-cards{position:fixed;left:0;right:0;top:calc(env(safe-area-inset-top,0px) + 8px);z-index:10000;display:none;flex-direction:column;align-items:center;gap:8px;pointer-events:none;padding:0 12px}',
		'.call-card{pointer-events:auto;width:100%;max-width:440px;box-sizing:border-box;display:flex;align-items:center;gap:10px;padding:10px 10px 10px 16px;border-radius:18px;background:oklch(0.21 0.014 265 / 0.97);color:oklch(0.95 0.008 265);border:1px solid oklch(0.62 0.17 280 / 0.45);box-shadow:0 12px 32px -8px oklch(0 0 0 / 0.5);font-family:var(--font-sans,system-ui)}',
		'.call-card.incoming{border-color:oklch(0.72 0.15 150 / 0.8);animation:call-pulse 1.4s ease-in-out infinite}',
		'.call-txt{flex:1;min-width:0}',
		'.call-ttl{font-size:12px;font-weight:700;color:oklch(0.8 0.1 280)}',
		'.call-who{font-size:15px;font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}',
		'.call-btn{min-height:44px;min-width:44px;padding:0 16px;border:0;border-radius:999px;font:inherit;font-size:14px;font-weight:700;color:white;cursor:pointer}',
		'.call-btn.ok{background:oklch(0.62 0.16 150)}',
		'.call-btn.no{background:oklch(0.58 0.19 25)}',
		'.call-ov-row{display:flex;align-items:center;gap:12px;justify-content:space-between}',
		'.call-ov-lbl{font-weight:600}',
		'@keyframes call-pulse{50%{box-shadow:0 0 0 6px oklch(0.72 0.15 150 / 0.25)}}',
	].join('\n');
	document.head.appendChild(st);
}

// ── Boot ─────────────────────────────────────────────────────────────────────

injectStyles();
registerSessionCard();
invoke('set_ringtone', { tone: ringtone() }).catch(() => {});
listen('call-state', apply).catch(() => {});
listen('call-notice', (n) => {
	if (n && n.kind) toast(t('call.' + n.kind));
}).catch(() => {});
invoke('call_states')
	.then((list) => {
		for (const s of list || []) apply(s);
	})
	.catch(() => {});
