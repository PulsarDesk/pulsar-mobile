// Relay authentication prompt (v5).
//
// The design is deliberately NOT "fields in Settings". A relay's credentials are asked
// for ONCE, in a sheet, at the moment the relay actually demands them — and the answer is
// never stored. What IS stored (by the Rust side, in the core config) is the durable
// ACCESS KEY the relay issues in return, bound to this device's public key. So every
// later connect registers silently, and the prompt reappears only if the relay server rotates
// the relay's credentials, which invalidates the key.
//
// `withRelayAuth` wraps any invoke that registers with the relay: it runs the call, and
// when the backend answers `RELAY_AUTH_REQUIRED:<pw>:<totp>` it opens the sheet, retries
// with what the user typed, and keeps the sheet open (with an error line) on
// `RELAY_AUTH_FAILED`.

import { t } from './i18n.js';

/** Parse `RELAY_AUTH_REQUIRED:<bool>:<bool>` out of a backend error string. */
function parseAuthRequired(msg) {
  const m = /RELAY_AUTH_REQUIRED:(\w+):(\w+)/.exec(msg);
  if (!m) return null;
  return { needPassword: m[1] === 'true', needTotp: m[2] === 'true' };
}

/**
 * Show the relay-auth sheet. Resolves `{ password, totp }` on submit, or `null` when the
 * user cancels. Only the factors the relay asked for are rendered.
 */
export function promptRelayAuth({ needPassword, needTotp, relay, failed }) {
  // Callers that pass '' mean "whatever the config says" — show something readable.
  const relayLabel = relay && relay.trim() ? relay.trim() : t('settings.relay');
  return new Promise((resolve) => {
    const backdrop = document.createElement('div');
    backdrop.className = 'sheet-backdrop';
    const sheet = document.createElement('div');
    sheet.className = 'sheet';
    sheet.setAttribute('role', 'dialog');
    sheet.setAttribute('aria-modal', 'true');

    const pwField = needPassword
      ? `<div class="field">
           <label for="ra-pw">${t('m.relayAuth.password')}</label>
           <input id="ra-pw" class="input" type="password" autocomplete="off" autocorrect="off"
                  autocapitalize="none" spellcheck="false"
                  aria-label="${t('m.relayAuth.password')}" />
         </div>`
      : '';
    const totpField = needTotp
      ? `<div class="field">
           <label for="ra-totp">${t('m.relayAuth.totp')}</label>
           <input id="ra-totp" class="input mono" type="text" inputmode="numeric" maxlength="8"
                  autocomplete="one-time-code" placeholder="000000" aria-label="${t('m.relayAuth.totp')}" />
           <span class="settings-field-hint">${t('m.relayAuth.totpHint')}</span>
         </div>`
      : '';
    const errLine = failed
      ? `<div style="font-size:13px;color:var(--danger);margin:0 2px 12px;line-height:1.4;">${t('m.relayAuth.failed')}</div>`
      : '';

    sheet.innerHTML = `
      <div class="sheet-handle"></div>
      <div style="font-size:17px;font-weight:700;margin:4px 2px 4px;">${t('m.relayAuth.title')}</div>
      <div class="mono" style="font-size:13px;color:var(--text);margin:0 2px 6px;word-break:break-all;">${relayLabel}</div>
      <div style="font-size:14px;color:var(--text-muted);line-height:1.45;margin:0 2px 14px;">${t('m.relayAuth.desc')}</div>
      ${errLine}
      ${pwField}
      ${totpField}
      <div style="font-size:12.5px;color:var(--text-muted);line-height:1.45;margin:6px 2px 18px;padding:9px 11px;background:var(--surface-2);border-radius:12px;border-left:3px solid var(--accent);">${t('m.relayAuth.keyNote')}</div>
      <div style="display:flex;gap:10px;">
        <button class="btn btn-ghost" style="flex:1" id="ra-cancel">${t('m.relayAuth.cancel')}</button>
        <button class="btn" style="flex:1" id="ra-ok">${t('m.relayAuth.connect')}</button>
      </div>`;

    document.body.appendChild(backdrop);
    document.body.appendChild(sheet);
    requestAnimationFrame(() => {
      backdrop.classList.add('open');
      sheet.classList.add('open');
      sheet.querySelector('#ra-pw')?.focus();
    });

    let done = false;
    const close = (val) => {
      if (done) return;
      done = true;
      backdrop.classList.remove('open');
      sheet.classList.remove('open');
      setTimeout(() => {
        backdrop.remove();
        sheet.remove();
      }, 280);
      resolve(val);
    };
    const submit = () => {
      const password = sheet.querySelector('#ra-pw')?.value.trim() || '';
      const totp = sheet.querySelector('#ra-totp')?.value.trim() || '';
      // Don't retry with a factor the relay asked for still blank — it would just bounce.
      if ((needPassword && !password) || (needTotp && !totp)) return;
      close({ password, totp });
    };
    sheet.querySelector('#ra-cancel').addEventListener('click', () => close(null));
    sheet.querySelector('#ra-ok').addEventListener('click', submit);
    backdrop.addEventListener('click', () => close(null));
    sheet.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') submit();
    });
  });
}

/**
 * Run a relay-registering operation, prompting for credentials if the relay demands them.
 *
 * `run(creds)` is called with `{ relayPassword, relayTotp }` — `null`/`null` on the first
 * attempt (the stored access key alone normally suffices), then whatever the user typed.
 * Rethrows any non-auth error unchanged, and throws a cancellation marker if the user
 * dismisses the sheet, so callers can stay quiet instead of toasting a scary message.
 */
export async function withRelayAuth(relayLabel, run) {
  let creds = { relayPassword: null, relayTotp: null };
  let failed = false;
  // Which factors the relay asked for, remembered across retries: a REJECTED attempt
  // replies BadAuth without restating them, and re-prompting for the wrong set (e.g.
  // dropping the 2FA field on a relay that wants MFA) would make it unanswerable.
  let want = { needPassword: true, needTotp: false };
  // First attempt + at most a few prompted retries, so a relay that keeps rejecting
  // can't spin an endless sheet loop.
  for (let attempt = 0; attempt < 4; attempt++) {
    try {
      return await run(creds);
    } catch (e) {
      const msg = String(e);
      const required = parseAuthRequired(msg);
      const rejected = msg.includes('RELAY_AUTH_FAILED');
      if (!required && !rejected) throw e;
      if (rejected) failed = true;
      if (required) want = required;
      const answer = await promptRelayAuth({ ...want, relay: relayLabel, failed });
      if (!answer) {
        const err = new Error('RELAY_AUTH_CANCELLED');
        err.cancelled = true;
        throw err;
      }
      creds = { relayPassword: answer.password || null, relayTotp: answer.totp || null };
    }
  }
  throw new Error('RELAY_AUTH_FAILED');
}
