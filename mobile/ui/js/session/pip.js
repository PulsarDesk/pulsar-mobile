/**
 * pip.js — Android picture-in-picture for a client stream (pulsar-desktop#18 parity:
 * the desktop's session "small window").
 *
 * Native side: PulsarVideoPlugin.kt "picture-in-picture" (+ the plugin manifest's
 * supportsPictureInPicture merge onto MainActivity).
 *
 *   - Support: `pip_supported` once at load → html[data-pip="1"]. Every PiP control
 *     (the overlay's "Small window" button, the Settings toggle) carries `.pip-only`
 *     and is hidden without it (Android < 8, no PiP feature, browser dev).
 *   - Auto-enter: while a session is live and Settings → "Small window when leaving the
 *     app" (pref `pipOnLeave`, default on) is set, `pip_set_auto {on:true}` arms it
 *     (Android 12+: setAutoEnterEnabled; 8-11: onUserLeaveHint). Disarmed when the last
 *     session ends (the native detach disarms too).
 *   - In PiP (`pulsar-pip` window event from the plugin): body.pip-mode hides every
 *     webview element so only the native video shows, and input events are swallowed
 *     before the touch / key layers see them. Expanding the window restores the UI.
 *   - PiP window closed (`closed: true`): every session ends the way the End button
 *     ends it (end_session + detach).
 *
 * Exports: enterPip() — the overlay button's action.
 */

import { invoke, hasTauri } from '../tauri.js';
import { getPref } from '../store/prefs.js';
import { registry, endSession } from './session.js';
import { close as closeOverlay } from './overlay.js';

const getBus = () => window.__pulsarBus || null;

let _supported = false;

/** Enter PiP now (the in-session "Small window" button). Resolves true on success. */
export async function enterPip() {
	if (!hasTauri || !_supported || registry.length === 0) return false;
	try {
		const r = await invoke('plugin:pulsar-video|pip_enter');
		return !!(r && r.ok);
	} catch (e) {
		console.warn('[pip] enter failed:', e);
		return false;
	}
}

/** Arm or disarm auto-enter for the current state (a live session + the setting). */
function syncAuto() {
	if (!hasTauri || !_supported) return;
	const on = registry.length > 0 && getPref('pipOnLeave') !== false;
	invoke('plugin:pulsar-video|pip_set_auto', { on }).catch(() => {});
}

function setPipMode(on) {
	document.body.classList.toggle('pip-mode', on);
	if (on) {
		try { closeOverlay(); } catch (_) {}
		// Drop a focused text field (soft keyboard) so it is not up when the window expands.
		try { document.activeElement?.blur?.(); } catch (_) {}
	}
	const bus = getBus();
	if (bus) bus.emit('pip-changed', on);
}

function onPip(e) {
	const d = (e && e.detail) || {};
	if (d.pip) { setPipMode(true); return; }
	setPipMode(false);
	if (d.closed) {
		// The user closed the PiP window: end every session like the End button does.
		[...registry].forEach((s) => { endSession(s.slot).catch(() => {}); });
	}
}

// Input guard: nothing reaches the host while in PiP. Window capture listeners run before
// the touch layer (app.js, window bubble listeners) and the panels' element listeners.
function guard(e) {
	if (!document.body.classList.contains('pip-mode')) return;
	e.stopImmediatePropagation();
}

function injectStyles() {
	if (document.getElementById('pip-styles')) return;
	const style = document.createElement('style');
	style.id = 'pip-styles';
	style.textContent = `
html:not([data-pip="1"]) .pip-only { display: none !important; }
/* PiP: only the native video under the transparent webview shows. */
body.pip-mode > * { visibility: hidden !important; }
body.pip-mode { background: transparent !important; }
`;
	document.head.appendChild(style);
}

async function init() {
	injectStyles();
	for (const type of ['touchstart', 'touchmove', 'touchend', 'pointerdown', 'pointermove',
		'pointerup', 'mousedown', 'mousemove', 'mouseup', 'wheel', 'keydown', 'keyup', 'click']) {
		// Passive: never delays scrolling outside PiP (stopping propagation is enough).
		window.addEventListener(type, guard, { capture: true, passive: true });
	}
	window.addEventListener('pulsar-pip', onPip);

	if (hasTauri) {
		try {
			const r = await invoke('plugin:pulsar-video|pip_supported');
			_supported = !!(r && r.ok);
		} catch (_) { _supported = false; }
	}
	if (_supported) document.documentElement.dataset.pip = '1';

	const bus = getBus();
	if (bus) {
		bus.on('session-started', syncAuto);
		bus.on('session-ended', () => { setPipMode(false); syncAuto(); });
		bus.on('pip-pref-changed', syncAuto);
		bus.on('pip-enter', () => { enterPip(); });
	}
	// A session may already be live when this module finishes loading.
	if (registry.length > 0) syncAuto();
}

init();
