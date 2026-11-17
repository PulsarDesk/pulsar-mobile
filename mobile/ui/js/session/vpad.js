/**
 * vpad.js — on-screen (touch) game controller, game mode only.
 *
 * A Moonlight-style translucent layer over the video: two analog sticks (+ L3/R3), a
 * D-pad, A/B/X/Y, LB/RB, LT/RT (digital, full pull), Select / Guide / Start. Toggled
 * from the overlay's Controllers section ("On-screen controller"; remembered per phone).
 *
 * Wire: `send_gamepad` (input_cmds.rs) → `InputEvent::GamepadSlot` on the ACTIVE pane's
 * host, as an Xbox-family pad at host slot VPAD_IDX. Physical pads (pad_feed.rs) take
 * slots from 0 up, so the on-screen pad sits at the last of the host's 4 slots and
 * never collides with them. State is sent on change (coalesced per frame) and re-sent
 * every KEEPALIVE_MS (a lost datagram must not leave a button held). Hidden, a pane
 * switch, or the session's end → a neutral state + `send_gamepad_disconnect`.
 *
 * Buttons use pulsar-core's bits (input::types::button), the same as PadInput.kt.
 * Touches on the layer are kept away from the touch→host engine (app.js uiBlocked)
 * and from the 3-finger overlay gesture (overlay.js).
 */

import { invoke, hasTauri } from '../tauri.js';
import { t } from '../i18n.js';
import { activeSlot } from './session.js';

const B = {
	A: 1 << 0, B: 1 << 1, X: 1 << 2, Y: 1 << 3, LB: 1 << 4, RB: 1 << 5,
	BACK: 1 << 6, START: 1 << 7, GUIDE: 1 << 8, L3: 1 << 9, R3: 1 << 10,
	UP: 1 << 11, DOWN: 1 << 12, LEFT: 1 << 13, RIGHT: 1 << 14,
};
/** Host-side pad slot of the on-screen controller (the host emulates 4). */
const VPAD_IDX = 3;
const KEEPALIVE_MS = 1000;
const DEADZONE = 0.08;
const LS_ON = 'pulsar.vpad.on.v1';

let _on = false;          // the user's toggle
let _shown = false;       // layer live (on + game mode + in session)
let _layer = null;
let _slot = null;         // session slot the current state went to
const _st = { buttons: 0, lx: 0, ly: 0, rx: 0, ry: 0, lt: 0, rt: 0 };
let _dirty = false;
let _raf = 0;
let _keepalive = null;

function _loadPref() {
	try { return localStorage.getItem(LS_ON) === '1'; } catch (_) { return false; }
}
function _savePref(on) {
	try { localStorage.setItem(LS_ON, on ? '1' : '0'); } catch (_) {}
}

// ── Sending ───────────────────────────────────────────────────────────────────

function _curSlot() {
	try { return activeSlot(); } catch (_) { return 0; }
}

function _send() {
	if (!hasTauri) return;
	const slot = _curSlot();
	if (_slot !== null && _slot !== slot) _release(_slot); // pane switch: free the old host's pad
	_slot = slot;
	invoke('send_gamepad', {
		slot, padIdx: VPAD_IDX,
		buttons: _st.buttons, lx: _st.lx, ly: _st.ly, rx: _st.rx, ry: _st.ry, lt: _st.lt, rt: _st.rt,
		target: 'auto',
	}).catch(() => {});
}

/** Neutral + disconnect on `slot`'s host. */
function _release(slot) {
	if (!hasTauri || slot === null) return;
	invoke('send_gamepad', { slot, padIdx: VPAD_IDX, buttons: 0, lx: 0, ly: 0, rx: 0, ry: 0, lt: 0, rt: 0, target: 'auto' })
		.catch(() => {})
		.finally(() => invoke('send_gamepad_disconnect', { slot, padIdx: VPAD_IDX }).catch(() => {}));
}

function _changed() {
	_dirty = true;
	if (_raf) return;
	_raf = requestAnimationFrame(() => {
		_raf = 0;
		if (_dirty && _shown) { _dirty = false; _send(); }
	});
}

function _setBit(bit, down) {
	const was = _st.buttons;
	_st.buttons = down ? (was | bit) : (was & ~bit);
	if (_st.buttons !== was) _changed();
}

// ── Controls ──────────────────────────────────────────────────────────────────

/** Press-and-hold button: `apply(down)` on press / release. */
function _wireButton(el, apply) {
	let pid = null;
	el.addEventListener('pointerdown', (e) => {
		e.preventDefault();
		pid = e.pointerId;
		try { el.setPointerCapture(pid); } catch (_) {}
		el.classList.add('on');
		apply(true);
		if (navigator.vibrate) { try { navigator.vibrate(8); } catch (_) {} }
	});
	const up = (e) => {
		if (e.pointerId !== pid) return;
		pid = null;
		el.classList.remove('on');
		apply(false);
	};
	el.addEventListener('pointerup', up);
	el.addEventListener('pointercancel', up);
}

/** Analog stick: drag inside the base → axes (up-positive Y, the wire convention). */
function _wireStick(base, setAxes) {
	const knob = base.querySelector('.vp-knob');
	let pid = null;
	const move = (e) => {
		const r = base.getBoundingClientRect();
		const rad = r.width / 2;
		let dx = (e.clientX - (r.left + rad)) / rad;
		let dy = (e.clientY - (r.top + rad)) / rad;
		const m = Math.hypot(dx, dy);
		if (m > 1) { dx /= m; dy /= m; }
		knob.style.transform = 'translate(' + (dx * rad * 0.55) + 'px,' + (dy * rad * 0.55) + 'px)';
		const ax = (v) => (Math.abs(v) < DEADZONE ? 0 : Math.round(v * 32767));
		setAxes(ax(dx), ax(-dy));
		_changed();
	};
	base.addEventListener('pointerdown', (e) => {
		e.preventDefault();
		pid = e.pointerId;
		try { base.setPointerCapture(pid); } catch (_) {}
		base.classList.add('on');
		move(e);
	});
	base.addEventListener('pointermove', (e) => { if (e.pointerId === pid) move(e); });
	const up = (e) => {
		if (e.pointerId !== pid) return;
		pid = null;
		base.classList.remove('on');
		knob.style.transform = '';
		setAxes(0, 0);
		_changed();
	};
	base.addEventListener('pointerup', up);
	base.addEventListener('pointercancel', up);
}

function _build() {
	if (_layer) return;
	_injectStyles();
	_layer = document.querySelector('.gamepad-layer') || document.createElement('div');
	_layer.className = 'gamepad-layer';
	_layer.setAttribute('aria-hidden', 'true');
	_layer.innerHTML = [
		'<div class="vp-ctl vp-pill vp-lt" data-trig="l">LT</div>',
		'<div class="vp-ctl vp-pill vp-lb" data-btn="LB">LB</div>',
		'<div class="vp-ctl vp-pill vp-rb" data-btn="RB">RB</div>',
		'<div class="vp-ctl vp-pill vp-rt" data-trig="r">RT</div>',
		'<div class="vp-ctl vp-stick vp-ls" data-stick="l"><div class="vp-knob"></div></div>',
		'<div class="vp-ctl vp-small vp-l3" data-btn="L3">L3</div>',
		'<div class="vp-dpad">',
		'<div class="vp-ctl vp-dir vp-up" data-btn="UP">▲</div>',
		'<div class="vp-ctl vp-dir vp-left" data-btn="LEFT">◀</div>',
		'<div class="vp-ctl vp-dir vp-right" data-btn="RIGHT">▶</div>',
		'<div class="vp-ctl vp-dir vp-down" data-btn="DOWN">▼</div>',
		'</div>',
		'<div class="vp-abxy">',
		'<div class="vp-ctl vp-face vp-y" data-btn="Y">Y</div>',
		'<div class="vp-ctl vp-face vp-x" data-btn="X">X</div>',
		'<div class="vp-ctl vp-face vp-b" data-btn="B">B</div>',
		'<div class="vp-ctl vp-face vp-a" data-btn="A">A</div>',
		'</div>',
		'<div class="vp-ctl vp-stick vp-rs" data-stick="r"><div class="vp-knob"></div></div>',
		'<div class="vp-ctl vp-small vp-r3" data-btn="R3">R3</div>',
		'<div class="vp-mid">',
		'<div class="vp-ctl vp-small" data-btn="BACK">⧉</div>',
		'<div class="vp-ctl vp-small" data-btn="GUIDE">◉</div>',
		'<div class="vp-ctl vp-small" data-btn="START">≡</div>',
		'</div>',
	].join('');
	_layer.querySelectorAll('[data-btn]').forEach((el) => {
		const bit = B[el.dataset.btn];
		_wireButton(el, (down) => _setBit(bit, down));
	});
	_layer.querySelectorAll('[data-trig]').forEach((el) => {
		const key = el.dataset.trig === 'l' ? 'lt' : 'rt';
		_wireButton(el, (down) => { _st[key] = down ? 255 : 0; _changed(); });
	});
	_layer.querySelectorAll('[data-stick]').forEach((el) => {
		const left = el.dataset.stick === 'l';
		_wireStick(el, (x, y) => {
			if (left) { _st.lx = x; _st.ly = y; } else { _st.rx = x; _st.ry = y; }
		});
	});
	// No context menu / text selection from a long press on a control.
	_layer.addEventListener('contextmenu', (e) => e.preventDefault());
	if (!_layer.parentNode) document.body.appendChild(_layer);
}

// ── Visibility ────────────────────────────────────────────────────────────────

function _wanted() {
	return _on
		&& document.body.dataset.mode === 'game'
		&& document.body.classList.contains('in-session');
}

function _neutral() {
	_st.buttons = 0; _st.lx = 0; _st.ly = 0; _st.rx = 0; _st.ry = 0; _st.lt = 0; _st.rt = 0;
	_layer?.querySelectorAll('.on').forEach((el) => el.classList.remove('on'));
	_layer?.querySelectorAll('.vp-knob').forEach((k) => { k.style.transform = ''; });
}

function _sync() {
	const want = _wanted();
	if (want === _shown) return;
	_shown = want;
	if (want) {
		_build();
		document.body.classList.add('gamepad');
		_neutral();
		_send(); // plug the pad in on the host now, not at the first press
		_keepalive = setInterval(() => { if (_shown) _send(); }, KEEPALIVE_MS);
	} else {
		document.body.classList.remove('gamepad');
		clearInterval(_keepalive);
		_keepalive = null;
		_neutral();
		_release(_slot);
		_slot = null;
	}
}

// ── Overlay card ──────────────────────────────────────────────────────────────

function _mountCard(el) {
	el.innerHTML = [
		'<div class="card-row">',
		'<span class="lbl">', t('m.gamepad.onscreen'), '</span>',
		'<label class="toggle">',
		'<input type="checkbox" id="vpad-toggle" aria-label="', t('m.gamepad.onscreen'), '" />',
		'<span class="track"></span><span class="thumb"></span>',
		'</label>',
		'</div>',
		'<div class="vp-hint">', t('m.gamepad.combos'), '</div>',
	].join('');
	const cb = el.querySelector('#vpad-toggle');
	cb.checked = _on;
	cb.addEventListener('change', () => {
		_on = cb.checked;
		_savePref(_on);
		_sync();
	});
}

function _register() {
	const reg = window.__pulsarRegisterCard;
	if (typeof reg !== 'function') return false;
	reg({ id: 'onscreen-pad', modes: ['game'], section: 'controllers', order: 8, mount: _mountCard });
	return true;
}

// ── Styles ────────────────────────────────────────────────────────────────────

function _injectStyles() {
	if (document.getElementById('vpad-styles')) return;
	const style = document.createElement('style');
	style.id = 'vpad-styles';
	style.textContent = `
.gamepad-layer {
	--vp-l: max(16px, env(safe-area-inset-left, 0px));
	--vp-r: max(16px, env(safe-area-inset-right, 0px));
	--vp-t: max(10px, env(safe-area-inset-top, 0px));
	--vp-b: max(12px, env(safe-area-inset-bottom, 0px));
	user-select: none;
	-webkit-user-select: none;
}
.gamepad-layer .vp-ctl {
	position: absolute;
	pointer-events: auto;
	touch-action: none;
	display: flex; align-items: center; justify-content: center;
	background: oklch(0.16 0.01 268 / 0.34);
	border: 1.5px solid oklch(1 0 0 / 0.3);
	color: oklch(0.97 0 0 / 0.85);
	font: 700 15px var(--font-sans);
	-webkit-tap-highlight-color: transparent;
}
.gamepad-layer .vp-ctl.on { background: oklch(0.78 0.13 210 / 0.5); border-color: oklch(0.85 0.12 210 / 0.8); }
.gamepad-layer .vp-pill { width: 64px; height: 38px; border-radius: 12px; top: var(--vp-t); font-size: 13px; }
.gamepad-layer .vp-lt { left: var(--vp-l); }
.gamepad-layer .vp-lb { left: calc(var(--vp-l) + 74px); }
.gamepad-layer .vp-rb { right: calc(var(--vp-r) + 74px); }
.gamepad-layer .vp-rt { right: var(--vp-r); }
.gamepad-layer .vp-stick { width: 120px; height: 120px; border-radius: 50%; }
.gamepad-layer .vp-ls { left: calc(var(--vp-l) + 12px); bottom: calc(var(--vp-b) + 118px); }
.gamepad-layer .vp-rs { right: calc(var(--vp-r) + 150px); bottom: var(--vp-b); width: 110px; height: 110px; }
.gamepad-layer .vp-knob {
	width: 50px; height: 50px; border-radius: 50%;
	background: oklch(1 0 0 / 0.32);
	pointer-events: none;
}
.gamepad-layer .vp-small { width: 40px; height: 40px; border-radius: 50%; font-size: 13px; }
.gamepad-layer .vp-l3 { left: var(--vp-l); bottom: calc(var(--vp-b) + 64px); }
.gamepad-layer .vp-r3 { right: calc(var(--vp-r) + 270px); bottom: calc(var(--vp-b) + 4px); }
.gamepad-layer .vp-dpad {
	position: absolute; width: 120px; height: 120px;
	left: calc(var(--vp-l) + 132px); bottom: var(--vp-b);
}
.gamepad-layer .vp-dir { width: 40px; height: 40px; border-radius: 10px; font-size: 13px; }
.gamepad-layer .vp-up    { left: 40px; top: 0; }
.gamepad-layer .vp-down  { left: 40px; bottom: 0; }
.gamepad-layer .vp-left  { left: 0; top: 40px; }
.gamepad-layer .vp-right { right: 0; top: 40px; }
.gamepad-layer .vp-abxy {
	position: absolute; width: 144px; height: 144px;
	right: var(--vp-r); bottom: calc(var(--vp-b) + 100px);
}
.gamepad-layer .vp-face { width: 48px; height: 48px; border-radius: 50%; font-size: 17px; }
.gamepad-layer .vp-y { left: 48px; top: 0; }
.gamepad-layer .vp-a { left: 48px; bottom: 0; }
.gamepad-layer .vp-x { left: 0; top: 48px; }
.gamepad-layer .vp-b { right: 0; top: 48px; }
.gamepad-layer .vp-mid {
	position: absolute; left: 50%; bottom: var(--vp-b);
	transform: translateX(-50%);
	width: 156px; height: 40px;
}
.gamepad-layer .vp-mid .vp-ctl:nth-child(1) { left: 0; }
.gamepad-layer .vp-mid .vp-ctl:nth-child(2) { left: 58px; }
.gamepad-layer .vp-mid .vp-ctl:nth-child(3) { right: 0; }
.vp-hint { font-size: 12px; color: oklch(0.7 0 0); margin-top: 6px; line-height: 1.4; }
`;
	document.head.appendChild(style);
}

// ── Wiring ────────────────────────────────────────────────────────────────────

_on = _loadPref();
if (!_register()) {
	window.addEventListener('overlay-ready', _register);
	if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', _register);
	else setTimeout(_register, 0);
}
(function wireBus() {
	const bus = window.__pulsarBus;
	if (!bus || typeof bus.on !== 'function') { setTimeout(wireBus, 300); return; }
	// session.js sets body.in-session / data-mode around these: re-check right after.
	bus.on('session-started', () => setTimeout(_sync, 0));
	bus.on('session-ended', () => setTimeout(_sync, 0));
	bus.on('mode-changed', () => setTimeout(_sync, 0));
	// Game pane → game pane keeps the layer up (no mode change): move the pad to the new
	// host now (_send releases the old one) instead of at the next keepalive.
	bus.on('active-pane-changed', () => setTimeout(() => { if (_shown) _send(); }, 0));
})();
