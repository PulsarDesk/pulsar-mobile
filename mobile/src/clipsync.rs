//! Two-way clipboard sync between this phone and every connected peer: the sessions it is
//! VIEWING (client) and the ones it is HOSTING. Port of the desktop's `clipboard_sync.rs`
//! (same default, size cap, poll rate, baseline and echo rules, same `DataMsg::Clipboard`
//! wire message), within Android's limits:
//!
//! - Peer → phone always works: writing the clipboard is allowed in the background.
//! - Phone → peer needs a READ, which Android 10+ grants only to the focused app. The watcher
//!   polls the native plugin (`clipboardPoll`), which reads only while Pulsar's window has
//!   focus: a copy inside the app is sent at once, a copy made in another app when the user
//!   comes back to Pulsar (the plugin marks every resume as a possible change).
//!
//! With sync off nothing is sent automatically and inbound text falls back to the manual
//! flow (the session's "Send clipboard" button / the "Clipboard received → Copy" toast).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use pulsar_core::service::{send_data_via, DataMsg};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_pulsar_video::PulsarVideoExt;
use tokio::sync::mpsc;

use crate::client::InputSenders;

/// Bigger clipboards are not synced (a stray multi-MB copy must not flood a session).
const MAX_BYTES: usize = 1 << 20;
const POLL: Duration = Duration::from_millis(500);

static ENABLED: AtomicBool = AtomicBool::new(true);
/// What the watcher last observed in the local clipboard (`None` = no baseline yet: the next
/// observation only sets it, so opening a session doesn't push whatever was copied before).
static SEEN: Mutex<Option<String>> = Mutex::new(None);
/// Hosted sessions' outbound side channel (`DataHandlers::outbound`), by host session id.
static HOST_OUT: Mutex<Option<HashMap<u32, mpsc::Sender<DataMsg>>>> = Mutex::new(None);

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// UI → app: the `clipboardSync` setting (Settings → Session). Pushed at boot and on change.
#[tauri::command]
pub fn set_clipboard_sync(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    *SEEN.lock().unwrap() = None;
}

/// A hosted session opened: its outbound sender receives local clipboard changes.
pub(crate) fn register_host(sid: u32, tx: mpsc::Sender<DataMsg>) {
    HOST_OUT.lock().unwrap().get_or_insert_with(HashMap::new).insert(sid, tx);
}

pub(crate) fn unregister_host(sid: u32) {
    if let Some(m) = HOST_OUT.lock().unwrap().as_mut() {
        m.remove(&sid);
    }
}

/// Text that came from a peer: write it to the local clipboard, then call `done(applied)`:
/// `true` when sync is on and the write succeeded (the caller then skips the manual-flow
/// toast), `false` otherwise (sync off, too big, or the write failed — e.g. no native
/// clipboard on this platform). The write runs off the calling task: it hops to the Android
/// UI thread, so `done` may run later on a blocking-pool thread.
pub(crate) fn apply_remote<R: Runtime>(
    app: &AppHandle<R>,
    text: &str,
    done: impl FnOnce(bool) + Send + 'static,
) {
    if !enabled() || text.is_empty() || text.len() > MAX_BYTES {
        done(false);
        return;
    }
    *SEEN.lock().unwrap() = Some(text.to_string());
    let app = app.clone();
    let text = text.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let ok = match app.pulsar_video().write_clipboard(&text) {
            Ok(r) if r.ok => true,
            Ok(r) => {
                log::warn!("clipboard sync: writing the remote clipboard failed: {}", r.detail);
                false
            }
            Err(e) => {
                log::warn!("clipboard sync: writing the remote clipboard failed: {e}");
                false
            }
        };
        done(ok);
    });
}

/// Wall-clock now in epoch milliseconds (the base of Android's clip timestamps).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Is any session open (viewed or hosted)?
fn any_session<R: Runtime>(app: &AppHandle<R>) -> bool {
    !app.state::<InputSenders>().0.lock().unwrap().is_empty()
        || HOST_OUT.lock().unwrap().as_ref().is_some_and(|m| !m.is_empty())
}

/// Send `text` to every open session.
async fn broadcast<R: Runtime>(app: &AppHandle<R>, text: String) {
    let clients: Vec<_> = app.state::<InputSenders>().0.lock().unwrap().values().cloned().collect();
    let hosts: Vec<_> = HOST_OUT
        .lock()
        .unwrap()
        .as_ref()
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default();
    let msg = DataMsg::Clipboard(text);
    for s in clients {
        let _ = send_data_via(&s, &msg).await;
    }
    for tx in hosts {
        let _ = tx.try_send(msg.clone());
    }
}

pub(crate) fn spawn_watcher<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        // The plugin's clipboard-change counter we last saw; -1 = no baseline yet.
        let mut last_seq: i64 = -1;
        // When the current run of open sessions started (epoch ms; 0 = idle). The baseline
        // can only be taken once Pulsar is focused; a clip set after this is a real copy.
        let mut since: i64 = 0;
        loop {
            tokio::time::sleep(POLL).await;
            if !enabled() || !any_session(&app) {
                *SEEN.lock().unwrap() = None;
                last_seq = -1;
                since = 0;
                continue;
            }
            if since == 0 {
                since = now_ms();
            }
            let app2 = app.clone();
            let seq = last_seq;
            let poll = match tauri::async_runtime::spawn_blocking(move || app2.pulsar_video().clipboard_poll(seq, since)).await {
                Ok(Ok(p)) if p.ok => p,
                _ => continue,
            };
            if poll.seq == last_seq {
                continue; // unchanged, or not readable now (app not focused)
            }
            let baseline = last_seq < 0;
            last_seq = poll.seq;
            if baseline && !(poll.changed && poll.fresh) {
                // The plugin may set the baseline from the clip's timestamp alone (no read,
                // so no Android 12+ "pasted" toast): any later change then counts.
                *SEEN.lock().unwrap() = Some(if poll.changed { poll.text } else { String::new() });
                continue;
            }
            if baseline {
                // A fresh clip (copied after the session opened, before Pulsar got focus):
                // send it, unless it is peer text we wrote ourselves (SEEN holds that).
                SEEN.lock().unwrap().get_or_insert_with(String::new);
            }
            if !poll.changed || poll.text.len() > MAX_BYTES {
                continue;
            }
            let text = poll.text;
            {
                let mut seen = SEEN.lock().unwrap();
                match seen.as_deref() {
                    None => {
                        *seen = Some(text);
                        continue;
                    }
                    Some(s) if s == text => continue, // our own write of peer text: no echo
                    _ => *seen = Some(text.clone()),
                }
            }
            if text.is_empty() {
                continue; // cleared / non-text now: nothing to send
            }
            broadcast(&app, text).await;
        }
    });
}
