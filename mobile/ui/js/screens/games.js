/**
 * games.js — Game-mode host library picker (desktop parity).
 *
 * In GAME mode, connecting doesn't stream straight away: it first FETCHES the
 * host's published game/app library (`list_remote_games` — the same
 * `request_games` the desktop uses), shows it as a full-screen picker (a pinned
 * "Desktop" card + one card per game), and only streams once the user picks one.
 * Picking Desktop streams the whole screen; picking a game asks the host to
 * launch it first (threaded into `connect_host` as `gameId`).
 *
 * Flow: connect.js's doConnect(target, slot, 'game') with NO gameId diverts here;
 * on pick we call doConnect(target, slot, 'game', pickedId) to actually stream.
 *
 * The overlay is appended to <body> (like the connecting overlay) and torn down
 * on pick / back / error-dismiss. Cyan-accented to match the game personality.
 */

import { invoke } from '../tauri.js';
import { t } from '../i18n.js';
import { relay, netmode, deviceName } from '../store/config.js';

let _overlay = null; // live overlay root (null when closed)

/** Remove the overlay if present. */
function close() {
  if (_overlay) {
    _overlay.classList.remove('open');
    const el = _overlay;
    _overlay = null;
    setTimeout(() => el.remove(), 220);
  }
}

/** Initials fallback for a card with no cover image. */
function initials(name) {
  return (name || '?')
    .split(' ')
    .map((w) => w[0])
    .slice(0, 2)
    .join('')
    .toUpperCase();
}

function fmtId(id) {
  const d = String(id).replace(/\D/g, '');
  return d.length === 9 ? `${d.slice(0, 3)} ${d.slice(3, 6)} ${d.slice(6)}` : id;
}

/**
 * Open the game-mode picker for `target` and stream the picked entry into `slot`.
 * @param {string} target  9-digit relay id (digits) or ip[:port]
 * @param {number} slot
 */
export function openGamesFetch(target, slot = 0) {
  close();

  const el = document.createElement('div');
  el.id = 'games-overlay';
  el.setAttribute('role', 'dialog');
  el.setAttribute('aria-modal', 'true');
  el.innerHTML = `
    <div class="games-backdrop"></div>
    <div class="games-body">
      <div class="games-head">
        <button class="games-back" aria-label="${t('gaming.back')}">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 18l-6-6 6-6"/></svg>
        </button>
        <span class="games-title mono">${fmtId(target)}</span>
      </div>
      <div class="games-state" id="games-state">
        <span class="games-spinner"></span>
        <span>${t('gaming.fetching')}</span>
      </div>
      <div class="games-grid" id="games-grid" hidden></div>
    </div>
  `;
  const style = document.createElement('style');
  style.textContent = _css();
  el.appendChild(style);
  document.body.appendChild(el);
  _overlay = el;
  requestAnimationFrame(() => el.classList.add('open'));

  el.querySelector('.games-back').addEventListener('click', close);
  el.querySelector('.games-backdrop').addEventListener('click', close);

  const pick = (gameId) => {
    close();
    // Dynamic import breaks the connect.js ↔ games.js cycle (connect.js imports
    // openGamesFetch statically; we call its doConnect back here).
    import('./connect.js').then(({ doConnect }) => doConnect(target, slot, 'game', gameId));
  };

  // Fetch the host library. Password '' — if the host needs an OTP the auth-prompt
  // sheet fires (session.js, keyed on the reserved fetch slot) exactly like a
  // normal connect.
  invoke('list_remote_games', {
    relay: relay(),
    target,
    password: '',
    netmode: netmode(),
    name: deviceName(),
  })
    .then((games) => renderGames(el, games || [], pick))
    .catch((e) => renderError(el, String(e && e.message ? e.message : e)));
}

/** Render the fetched library: pinned Desktop card + one card per game. */
function renderGames(el, games, pick) {
  if (!_overlay || el !== _overlay) return; // closed while fetching
  const state = el.querySelector('#games-state');
  const grid = el.querySelector('#games-grid');
  if (state) state.hidden = true;
  if (!grid) return;

  // The host always publishes a built-in "desktop" entry (with a live screenshot);
  // use it for the pinned Desktop card and drop it from the list so it isn't twice.
  const desktopImg = (games.find((g) => g.id === 'desktop') || {}).image || '';
  const rest = games.filter((g) => g.id !== 'desktop');

  const cardHtml = (id, title, kind, img) => `
    <button class="gcard" data-id="${id}">
      <span class="gcard-ico${img ? ' img' : ''}">
        ${img
          ? `<img src="${img}" alt="" />`
          : `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="13" rx="2"/><path d="M8 21h8M12 17v4"/></svg>`}
      </span>
      <span class="gcard-meta">
        <span class="gcard-name">${title}</span>
        <span class="gcard-kind mono">${kind}</span>
      </span>
    </button>`;

  let html = cardHtml('desktop', t('gaming.desktop'), t('gaming.wholeScreen'), desktopImg);
  for (const g of rest) {
    html += cardHtml(g.id, g.title || g.id, g.kind || t('gaming.game'), g.image || '');
  }
  grid.innerHTML = html;
  grid.hidden = false;
  grid.querySelectorAll('.gcard').forEach((btn) => {
    btn.addEventListener('click', () => {
      const id = btn.dataset.id;
      // Desktop = stream the whole screen (no host-side launch): send '' as gameId.
      pick(id === 'desktop' ? '' : id);
    });
  });
}

function renderError(el, msg) {
  if (!_overlay || el !== _overlay) return;
  const state = el.querySelector('#games-state');
  if (state) {
    state.hidden = false;
    state.classList.add('err');
    state.innerHTML = `<span>${msg}</span><button class="btn btn-ghost games-retry">${t('gaming.back')}</button>`;
    state.querySelector('.games-retry')?.addEventListener('click', close);
  }
}

function _css() {
  return `
#games-overlay {
  position: fixed; inset: 0; z-index: 60;
  opacity: 0; transition: opacity 0.22s var(--ease, ease);
  padding-top: var(--safe-top); padding-bottom: var(--safe-bottom);
}
#games-overlay.open { opacity: 1; }
/* The .games-state / .games-grid rules below set display:flex, which would
 * otherwise override the [hidden] attribute — force it to win so toggling
 * state/grid visibility actually works. */
#games-overlay [hidden] { display: none !important; }
#games-overlay .games-backdrop {
  position: absolute; inset: 0;
  background:
    radial-gradient(130% 80% at 50% -10%, var(--cyan-soft), transparent 58%),
    var(--bg);
  backdrop-filter: blur(24px) saturate(1.3);
  -webkit-backdrop-filter: blur(24px) saturate(1.3);
}
#games-overlay .games-body {
  position: relative; height: 100%;
  display: flex; flex-direction: column;
  padding: 14px 20px 24px; overflow-y: auto; scrollbar-width: none;
}
#games-overlay .games-body::-webkit-scrollbar { display: none; }
#games-overlay .games-head {
  display: flex; align-items: center; gap: 12px; margin-bottom: 22px;
}
#games-overlay .games-back {
  flex: none; width: 40px; height: 40px; display: grid; place-items: center;
  border: 1px solid var(--border); border-radius: var(--r-pill);
  background: var(--surface-2); color: var(--text); cursor: pointer; padding: 0;
}
#games-overlay .games-back svg { width: 20px; height: 20px; }
#games-overlay .games-title { font-size: 15px; color: var(--text-muted); font-weight: 600; }
#games-overlay .games-state {
  flex: 1; display: flex; flex-direction: column; align-items: center; justify-content: center;
  gap: 16px; color: var(--text-muted); font-size: 14.5px; text-align: center;
}
#games-overlay .games-state.err { gap: 20px; color: var(--danger); padding: 0 24px; }
#games-overlay .games-spinner {
  width: 34px; height: 34px; border-radius: 50%;
  border: 3px solid var(--surface-3); border-top-color: var(--cyan);
  animation: games-spin 0.8s linear infinite;
}
@keyframes games-spin { to { transform: rotate(360deg); } }
#games-overlay .games-grid { display: flex; flex-direction: column; gap: 10px; }
#games-overlay .gcard {
  display: flex; align-items: center; gap: 14px; width: 100%;
  padding: 14px 16px; text-align: left; cursor: pointer;
  background: var(--surface); border: 1px solid var(--border);
  border-radius: var(--r-xl); color: var(--text);
  min-height: var(--touch-min); transition: border-color 0.2s var(--ease, ease), background 0.2s var(--ease, ease);
}
#games-overlay .gcard:active { background: var(--surface-2); border-color: var(--cyan); }
#games-overlay .gcard-ico {
  flex: none; width: 46px; height: 46px; border-radius: 12px;
  display: grid; place-items: center; overflow: hidden;
  background: var(--cyan-soft); color: var(--cyan);
  font-weight: 700; font-size: 15px; font-family: var(--font-display);
}
#games-overlay .gcard-ico.img { background: var(--surface-3); }
#games-overlay .gcard-ico svg { width: 22px; height: 22px; }
#games-overlay .gcard-ico img { width: 100%; height: 100%; object-fit: cover; }
#games-overlay .gcard-meta { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
#games-overlay .gcard-name { font-size: 15.5px; font-weight: 600; }
#games-overlay .gcard-kind { font-size: 12px; color: var(--text-faint); }
`;
}
