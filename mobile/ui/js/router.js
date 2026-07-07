/**
 * router.js — bottom-nav router + mode/session body-class writer
 *
 * Exports:
 *   registerScreen(spec)     — called by each screen module at import time
 *   show(id)                 — navigate to a screen tab
 *   setMode(m)               — SOLE writer of body[data-mode] ('remote'|'game')
 *   enterSession()           — SOLE writer of body.in-session (adds class)
 *   exitSession()            — SOLE writer of body.in-session (removes class)
 *   setNetPill(transport)    — update the net-pill text from real transport
 *   initRouter(bus)          — boot, called by app.js after DOM is ready
 *
 * The bottom-nav is rendered from the registry; adding a screen only requires
 * a screen module to call registerScreen() + app.js to import that module.
 *
 * NOTE: router.js does NOT import app.js to avoid circular dependencies.
 * app.js passes the bus into initRouter().
 *
 * W2-shell-glue additions:
 *   - setNetPill(transport) updates #net-pill from real connect_host result
 *   - initRouter now subscribes to bus 'session-started' → enterSession()
 *     and bus 'session-ended' → exitSession()
 *   - initRouter subscribes to Tauri 'conn-phase' events to update the pill
 *     when the transport is known (before auth)
 */

import { t } from './i18n.js';

/** @type {Map<string, ScreenSpec>} */
const _screens = new Map();

let _activeId = null;

/**
 * @typedef {Object} ScreenSpec
 * @property {string}   id           — unique tab id (matches the <section id="t-{id}"> convention)
 * @property {string}   navIcon      — SVG string for the bottom-nav icon
 * @property {string}   navLabelKey  — i18n key for the tab label (rendered via t())
 * @property {string}   [navLabel]   — fallback label if i18n not yet loaded
 * @property {Function} [mount]      — called once after the tab section is shown the first time
 * @property {Function} [onShow]     — called each time this tab becomes active
 */

/**
 * Register a screen with the router. Called at module import time by each
 * screens/*.js module. Safe to call before DOMContentLoaded.
 * @param {ScreenSpec} spec
 */
export function registerScreen(spec) {
	_screens.set(spec.id, spec);
}

/**
 * Run a screen's one-time mount() WITHOUT switching the visible tab. Used to
 * pre-mount the host screen at boot so its incoming-connection listener
 * (`session-request`) + approval sheet exist globally — otherwise a peer
 * connecting while the user is on another tab would pop no modal.
 * @param {string} id
 */
export function premount(id) {
	const spec = _screens.get(id);
	if (!spec) return;
	if (spec.mount && !spec._mounted) {
		spec._mounted = true;
		spec.mount();
	}
}

/**
 * Navigate to screen by id.
 * @param {string} id
 */
export function show(id) {
	const spec = _screens.get(id);
	if (!spec) return;

	// Hide all tab sections
	document.querySelectorAll('main.scroll .tab').forEach((el) => {
		el.classList.remove('on');
	});
	// Show the target section
	const target = document.getElementById('t-' + id);
	if (target) target.classList.add('on');

	// Update nav buttons
	document.querySelectorAll('nav.bottom button[data-tab]').forEach((btn) => {
		btn.classList.toggle('on', btn.dataset.tab === id);
	});

	// First-mount hook
	if (spec.mount && !spec._mounted) {
		spec._mounted = true;
		spec.mount();
	}
	// onShow hook
	if (spec.onShow) spec.onShow();

	_activeId = id;
}

/**
 * Set a bottom-nav button's text label. The nav markup in index.html is
 * `<button><svg/><span data-i18n="…">Label</span></button>` — write into the
 * span when present (it's also translated by applyI18n) so the two mechanisms
 * never produce a duplicate label; fall back to the legacy bare text node.
 * @param {Element} btn
 * @param {string} text
 */
function setNavLabel(btn, text) {
	const span = btn.querySelector('span[data-i18n]');
	if (span) { span.textContent = text; return; }
	const svg = btn.querySelector('svg');
	let node = svg ? svg.nextSibling : btn.firstChild;
	while (node && node.nodeType !== 3) node = node.nextSibling; // 3 = TEXT_NODE
	if (node) node.textContent = text;
	else btn.appendChild(document.createTextNode(text));
}

/**
 * Re-label every bottom-nav button from its screen's navLabelKey via t().
 * Called on boot and on 'langchange' so the nav follows the active language
 * (the static labels in index.html are the pre-i18n fallback).
 */
export function refreshNavLabels() {
	document.querySelectorAll('nav.bottom button[data-tab]').forEach((btn) => {
		const spec = _screens.get(btn.dataset.tab);
		if (spec && spec.navLabelKey) setNavLabel(btn, t(spec.navLabelKey));
	});
}

/**
 * SOLE writer of body[data-mode]. Drives the whole app's personality:
 *   - syncs the top-bar game toggle (#mode-toggle) pressed state,
 *   - swaps the connect-tab copy (remote vs. game),
 *   - bounces off the host tab when entering game mode (game = pure client, no
 *     hosting — the "Cihazım" tab is hidden by CSS in game mode).
 * @param {'remote'|'game'} m
 */
export function setMode(m) {
	const game = m === 'game';
	document.body.dataset.mode = m;

	// Top-bar toggle pressed state (cyan when game).
	const btn = document.getElementById('mode-toggle');
	if (btn) {
		btn.classList.toggle('on', game);
		btn.setAttribute('aria-pressed', String(game));
	}

	// Game mode is a pure-client personality — no hosting. If the host tab is
	// active while entering game mode, its nav button is about to be hidden, so
	// move to connect first (else the user is stranded on an invisible tab).
	if (game && _activeId === 'host') show('connect');

	// Connect-tab copy follows the personality. Rewrite the data-i18n KEY (not just
	// the text) so a later applyI18n()/langchange re-render keeps the game copy
	// instead of reverting to the remote strings.
	const title = document.querySelector('#t-connect h2.title');
	const sub   = document.querySelector('#t-connect p.sub');
	if (title) {
		const k = game ? 'home.gameTitle' : 'home.title';
		title.setAttribute('data-i18n', k);
		title.textContent = t(k);
	}
	if (sub) {
		const k = game ? 'home.gameSub' : 'home.sub';
		sub.setAttribute('data-i18n', k);
		sub.textContent = t(k);
	}
}

/**
 * SOLE writer: adds body.in-session (hides nav, shows bar).
 */
export function enterSession() {
	document.body.classList.add('in-session');
}

/**
 * SOLE writer: removes body.in-session (shows nav again).
 */
export function exitSession() {
	document.body.classList.remove('in-session');
}

// Net-pill transport label keys (contract §2.6: "direct"|"relay")
const _transportKeys = {
	direct: 'm.netpill.transportP2p',
	relay:  'm.netpill.transportRelay',
};

/**
 * Update the net-pill (#net-pill) to show the real transport.
 * Called once connect_host resolves with a transport value, and also
 * driven by the 'conn-phase' Tauri event while connecting.
 * @param {string} transport — 'direct' | 'relay'
 */
export function setNetPill(transport) {
	const pill = document.getElementById('net-pill');
	if (!pill) return;
	const key = _transportKeys[transport];
	if (key) pill.textContent = t(key);
}

const _modeKeys = {
	'auto':       'm.netpill.modeAuto',
	'p2p-only':   'm.netpill.modeP2p',
	'relay-only': 'm.netpill.modeRelay',
};

/**
 * Show the configured NETWORK MODE on the net-pill (idle state). The pill used to
 * be hard-coded "P2P → relay", so switching the mode in Settings never showed.
 * During a live session setNetPill() overrides it with the real transport;
 * session end should call this again to fall back to the mode label.
 * @param {'auto'|'p2p-only'|'relay-only'} mode
 */
export function setNetPillMode(mode) {
	const pill = document.getElementById('net-pill');
	if (!pill) return;
	pill.textContent = t(_modeKeys[mode] || _modeKeys['auto']);
}

/**
 * Boot the router: wire the static nav buttons + listen to bus events.
 * Called by app.js after the DOM is ready.
 * @param {{ addEventListener: Function, on: Function }} bus  — the app.js EventTarget bus
 * @param {{ listen: Function }} [tauri]  — optional tauri.js module for Tauri events
 */
export function initRouter(bus, tauri) {
	// Wire bottom-nav buttons from the registry (for statically rendered tabs)
	// and from any data-tab buttons already in the HTML.
	document.querySelectorAll('nav.bottom button[data-tab]').forEach((btn) => {
		btn.addEventListener('click', () => {
			show(btn.dataset.tab);
		});
	});

	if (bus) {
		// Listen for mode-changed bus event (fired by connect screen mode toggle)
		bus.addEventListener('mode-changed', (e) => {
			setMode(e.detail);
		});

		// W2-shell-glue: session lifecycle — sole writes of body.in-session
		// Fired by session.js (W2-session) when a session starts/ends.
		bus.addEventListener('session-started', () => {
			enterSession();
		});
		bus.addEventListener('session-ended', () => {
			exitSession();
		});

		// W2-shell-glue: net-pill update from connect result
		// Fired by app.js connect flow after connect_host resolves.
		bus.addEventListener('net-transport', (e) => {
			setNetPill(e.detail);
		});
	}

	// W2-shell-glue: Tauri 'conn-phase' — update pill when transport is known
	// The event fires before auth completes, so the pill shows transport early.
	if (tauri && tauri.listen) {
		tauri.listen('conn-phase', (payload) => {
			// payload: {slot, phase, transport?}
			if (payload && payload.transport) {
				setNetPill(payload.transport);
			}
		}).catch(() => { /* Tauri not available — silently skip */ });
	}

	// Label the nav from the active language (replaces the static index.html
	// fallback labels), and keep it + the visible screen in sync on language
	// change. setLang() (i18n.js) dispatches 'langchange' on window. Re-running
	// show() re-runs the active screen's onShow (its idempotent re-render path);
	// mount() is guarded so it never double-wires.
	refreshNavLabels();
	window.addEventListener('langchange', () => {
		refreshNavLabels();
		if (_activeId) show(_activeId);
	});

	// Activate the first registered screen or the first on-class tab.
	const firstOn = document.querySelector('main.scroll .tab.on');
	if (firstOn) {
		const id = firstOn.id.replace(/^t-/, '');
		_activeId = id;
		// Ensure the matching nav button has .on
		const btn = document.querySelector('nav.bottom button[data-tab="' + id + '"]');
		if (btn) btn.classList.add('on');
	}
}
