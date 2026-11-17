/**
 * trust-guard.js — the blocking "device identity changed" warning.
 *
 * The Rust connect path pins each host's static key on the first connect (TOFU,
 * `known_peers.json`, see mobile/src/trust.rs). When a later connect is answered with a
 * DIFFERENT key, Rust refuses the session and rejects with `IDENTITY_CHANGED:<id>`.
 * This module turns that into a blocking sheet: the connection stays refused unless the
 * user explicitly picks "Trust the new key", which drops the pin (`forget_peer`) so the
 * caller can retry and the next connect pins the new key. Dismissing the sheet any other
 * way (backdrop, Escape, the safe button) keeps the old pin and does not connect.
 */

import { invoke } from './tauri.js';
import { t } from './i18n.js';

const MARKER_RE = /IDENTITY_CHANGED:\s*([\d ]+)/;

/**
 * The grouped device id from an `IDENTITY_CHANGED:<id>` error, or null for any other error.
 * @param {unknown} err
 * @returns {string|null}
 */
export function identityChangedId(err) {
	const raw = err && typeof err === 'object' && 'message' in err ? err.message : err;
	const m = MARKER_RE.exec(String(raw ?? ''));
	return m ? m[1].trim() : null;
}

const esc = (s) => String(s).replace(/[<>&"]/g, (c) => ({ '<': '&lt;', '>': '&gt;', '&': '&amp;', '"': '&quot;' }[c]));

let _open = null; // the pending promise while a sheet is up (one at a time)

/**
 * Show the blocking warning for `id`. Resolves true ONLY when the user chose to trust the
 * new key AND the old pin was removed; false otherwise (connect stays blocked).
 * @param {string} id      grouped relay id from the error
 * @param {string} relay   the relay the connect used ('' = the saved one)
 * @returns {Promise<boolean>}
 */
export function confirmIdentityChange(id, relay) {
	if (_open) return _open;
	_open = new Promise((resolve) => {
		const backdrop = document.createElement('div');
		backdrop.className = 'sheet-backdrop';
		backdrop.style.zIndex = '300';
		const sheet = document.createElement('div');
		sheet.className = 'sheet';
		sheet.style.zIndex = '301';
		sheet.setAttribute('role', 'alertdialog');
		sheet.setAttribute('aria-modal', 'true');
		sheet.setAttribute('aria-labelledby', 'tg-title');
		sheet.innerHTML = `
			<div class="sheet-handle"></div>
			<div style="display:flex;align-items:center;gap:10px;margin:4px 2px 8px;">
				<svg viewBox="0 0 24 24" fill="none" width="26" height="26" style="color:var(--danger);flex:none" aria-hidden="true">
					<path d="M12 3 2 20h20L12 3Z" stroke="currentColor" stroke-width="2" stroke-linejoin="round"/>
					<path d="M12 10v4M12 17h.01" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"/>
				</svg>
				<div id="tg-title" style="font-size:17px;font-weight:700;">${esc(t('m.trust.title'))}</div>
			</div>
			<div style="font-size:14px;color:var(--text-muted);line-height:1.45;margin:0 2px 10px;">${esc(t('m.trust.lead', { id }))}</div>
			<div style="font-size:14px;color:var(--text);line-height:1.45;margin:0 2px 18px;font-weight:600;">${esc(t('m.trust.advice'))}</div>
			<div style="display:flex;flex-direction:column;gap:10px;">
				<button class="btn btn-primary" id="tg-keep" style="font-size:16px;padding:14px">${esc(t('m.trust.keep'))}</button>
				<button class="btn btn-ghost" id="tg-accept"
					style="border-color:var(--danger);color:var(--danger);font-size:15px;padding:13px">${esc(t('m.trust.accept'))}</button>
			</div>`;
		document.body.appendChild(backdrop);
		document.body.appendChild(sheet);
		requestAnimationFrame(() => { backdrop.classList.add('open'); sheet.classList.add('open'); });

		let done = false;
		const escFn = (e) => { if (e.key === 'Escape') close(false); };
		const close = async (trust) => {
			if (done) return;
			done = true;
			document.removeEventListener('keydown', escFn);
			backdrop.classList.remove('open');
			sheet.classList.remove('open');
			setTimeout(() => { backdrop.remove(); sheet.remove(); }, 280);
			let ok = false;
			if (trust) {
				try { await invoke('forget_peer', { relay: relay || '', id }); ok = true; }
				catch (e) { console.warn('[trust] forget_peer failed', e); }
			}
			_open = null;
			resolve(ok);
		};
		document.addEventListener('keydown', escFn);
		sheet.querySelector('#tg-keep').addEventListener('click', () => close(false));
		sheet.querySelector('#tg-accept').addEventListener('click', () => close(true));
		backdrop.addEventListener('click', () => close(false));
		sheet.querySelector('#tg-keep').focus?.();
	});
	return _open;
}
