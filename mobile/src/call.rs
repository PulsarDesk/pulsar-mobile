//! Voice calls over a session, like a phone — the same protocol as the desktop app
//! (`pulsar_core::call`): either side rings, the other answers or declines. This phone may be
//! the client (key: pane `slot`) or the host (key: session `sid`).
//!
//! This module carries out the core's [`Step`]s: the signal goes out over the session, the
//! ringtone plays (`ringStart`: a built-in tone synthesized by the core, or the phone's own
//! ringtone) with a "incoming call" notification for when the app is in the background, and
//! while the call is up this phone's mic streams to the peer (`DataMsg::Audio`, its own pump
//! over the plugin's AudioRecord) and the peer's mic plays on a voice-call AudioTrack
//! (`voiceFeed`, fed from `datachan::route` / the host's `on_audio`). Every change is the
//! `call-state` event `{side, id, phase, peer, since}`, which `ui/js/call.js` shows.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pulsar_core::call::{Action, Call, Phase, Step};
use pulsar_core::service::{send_data_via, CallSignal, DataMsg};
use pulsar_core::SessionSender;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_pulsar_video::PulsarVideoExt;
use tokio::sync::Notify;

/// Which session a call belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "side", content = "id", rename_all = "lowercase")]
pub enum CallKey {
    /// This phone hosts: the session id.
    Host(u64),
    /// This phone views: the pane slot.
    Client(u64),
}

impl CallKey {
    fn parse(side: &str, id: u64) -> Option<CallKey> {
        match side {
            "host" => Some(CallKey::Host(id)),
            "client" => Some(CallKey::Client(id)),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Entry {
    call: Call,
    /// Wall-clock ms the phase began (the talk timer in the UI).
    since_ms: u64,
    /// The other side's name ("" = unknown).
    peer: String,
    /// Stops this call's mic pump.
    mic_stop: Option<Arc<Notify>>,
}

static CALLS: Mutex<Option<HashMap<CallKey, Entry>>> = Mutex::new(None);
/// Host side: each session's sender (the client side uses `InputSenders`).
static HOST_SENDERS: Mutex<Option<HashMap<u64, SessionSender>>> = Mutex::new(None);
/// The ringtone setting: `classic` / `chime` / `pulse` / `none` / `system`.
static TONE: Mutex<String> = Mutex::new(String::new());
/// Bumped per preview, so an older preview's timer does not stop a newer one.
static PREVIEW_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn with_calls<T>(f: impl FnOnce(&mut HashMap<CallKey, Entry>) -> T) -> T {
    let mut g = CALLS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Serialize)]
pub struct CallState {
    #[serde(flatten)]
    key: CallKey,
    phase: Phase,
    peer: String,
    since: u64,
    /// The peer takes calls (said `Hello`); an older Pulsar never answers one.
    supported: bool,
}

/// The session came up: tell the peer we take calls (`Hello`, repeated on the core's schedule
/// until it answered or the session ended).
pub fn session_up<R: Runtime>(app: &AppHandle<R>, key: CallKey) {
    with_calls(|m| {
        m.entry(key).or_default();
    });
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let t0 = tokio::time::Instant::now();
        for at in pulsar_core::call::HELLO_AT {
            tokio::time::sleep_until(t0 + *at).await;
            let known = with_calls(|m| m.get(&key).map(|e| e.call.supported()));
            if known != Some(false) {
                return; // answered, or the session is gone
            }
            if let Some(tx) = sender(&app, key) {
                let _ = send_data_via(&tx, &DataMsg::Call(CallSignal::Hello)).await;
            }
        }
    });
}

// ── Session lifecycle hooks ───────────────────────────────────────────────────

/// Host: a session is up (`peer` = its name or id, for the UI).
pub fn register_host(sid: u64, sender: SessionSender, peer: &str) {
    HOST_SENDERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(sid, sender);
    set_peer_name(CallKey::Host(sid), peer);
}

/// Host: the session's Hello schedule (after `register_host`).
pub fn host_up<R: Runtime>(app: &AppHandle<R>, sid: u64) {
    session_up(app, CallKey::Host(sid));
}

/// The peer's display name arrived (`DataMsg::PeerName`, or the host's beacon name).
pub fn set_peer_name(key: CallKey, name: &str) {
    let name = name.trim();
    if !name.is_empty() {
        with_calls(|m| m.entry(key).or_default().peer = name.to_string());
    }
}

/// The session ended: drop its call without signalling.
pub fn session_gone<R: Runtime>(app: &AppHandle<R>, key: CallKey) {
    let step = with_calls(|m| {
        m.get_mut(&key)
            .map(|e| e.call.gone(Instant::now()))
            .unwrap_or_default()
    });
    apply(app, key, step);
    with_calls(|m| m.remove(&key));
    if let CallKey::Host(sid) = key {
        if let Some(m) = HOST_SENDERS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            m.remove(&sid);
        }
    }
}

fn sender<R: Runtime>(app: &AppHandle<R>, key: CallKey) -> Option<SessionSender> {
    match key {
        CallKey::Host(sid) => HOST_SENDERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|m| m.get(&sid).cloned()),
        CallKey::Client(slot) => app
            .state::<crate::client::InputSenders>()
            .0
            .lock()
            .unwrap()
            .get(&(slot as u8))
            .cloned(),
    }
}

// ── Events from the peer / the user / the clock ───────────────────────────────

/// A signal from the peer.
pub fn on_signal<R: Runtime>(app: &AppHandle<R>, key: CallKey, sig: CallSignal) {
    let (step, learned) = with_calls(|m| {
        let e = m.entry(key).or_default();
        let before = e.call.supported();
        (e.call.signal(sig, Instant::now()), !before)
    });
    log::info!("pulsar call: {key:?} signal {sig:?} → {step:?}");
    apply(app, key, step);
    if learned && step.moved.is_none() {
        // The peer takes calls: the UI offers calling now.
        let (peer, since) = with_calls(|m| m.get(&key).map(|e| (e.peer.clone(), e.since_ms)).unwrap_or_default());
        let _ = app.emit("call-state", CallState { key, phase: Phase::Idle, peer, since, supported: true });
    }
}

/// The peer's mic (`Audio` / `AudioEnd`): played on the voice-call track.
pub fn on_voice<R: Runtime>(app: &AppHandle<R>, msg: &DataMsg) {
    match msg {
        DataMsg::Audio(pcm) => {
            let _ = app.pulsar_video().voice_feed(pcm);
        }
        DataMsg::AudioEnd => {
            let _ = app.pulsar_video().voice_stop();
        }
        _ => {}
    }
}

fn action<R: Runtime>(app: &AppHandle<R>, key: CallKey, what: &str) {
    let Some(a) = Action::parse(what) else { return };
    if sender(app, key).is_none() {
        return; // no session (any more)
    }
    // A peer that never said it takes calls would never answer.
    let supported = with_calls(|m| m.get(&key).is_some_and(|e| e.call.supported()));
    if a == Action::Start && !supported {
        log::info!("pulsar call: {key:?} peer does not take calls (older Pulsar)");
        return;
    }
    let step = with_calls(|m| m.entry(key).or_default().call.action(a, Instant::now()));
    log::info!("pulsar call: {key:?} action {what} → {step:?}");
    apply(app, key, step);
}

/// One task for the app: every 500 ms, the core's timers (ring repeat, ring-outs).
fn ensure_ticker<R: Runtime>(app: &AppHandle<R>) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = Instant::now();
            let steps: Vec<(CallKey, Step)> = with_calls(|m| {
                m.iter_mut()
                    .filter(|(_, e)| e.call.phase() != Phase::Idle)
                    .map(|(k, e)| (*k, e.call.tick(now)))
                    .collect()
            });
            for (key, step) in steps {
                apply(&app, key, step);
            }
        }
    });
}

// ── Carrying out a step ───────────────────────────────────────────────────────

fn apply<R: Runtime>(app: &AppHandle<R>, key: CallKey, step: Step) {
    if let Some((sig, times)) = step.send {
        if let Some(tx) = sender(app, key) {
            tauri::async_runtime::spawn(async move {
                for _ in 0..times {
                    let _ = send_data_via(&tx, &DataMsg::Call(sig)).await;
                }
            });
        }
    }
    let Some((from, to)) = step.moved else { return };
    ensure_ticker(app);
    let since = now_ms();
    let (peer, mic_stop, supported) = with_calls(|m| {
        let e = m.entry(key).or_default();
        e.since_ms = since;
        let stop = if to != Phase::Active { e.mic_stop.take() } else { None };
        (e.peer.clone(), stop, e.call.supported())
    });
    // Ringing (+ its notification) only while a call comes in.
    if to == Phase::Incoming {
        let tone = ringtone();
        let pcm = pulsar_core::call::tone_pcm(if tone == "system" { "classic" } else { &tone });
        if tone != "none" {
            let _ = app.pulsar_video().ring_start(&pcm, tone == "system");
        }
        let _ = app.pulsar_video().call_notify(&peer, "");
    } else if from == Phase::Incoming {
        let _ = app.pulsar_video().ring_stop();
        let _ = app.pulsar_video().call_notify_clear();
    }
    if let Some(stop) = mic_stop {
        stop.notify_one();
    }
    if to == Phase::Active && from != Phase::Active {
        start_mic(app, key);
    }
    if from == Phase::Active && to != Phase::Active {
        let _ = app.pulsar_video().voice_stop();
    }
    sync_service(app);
    log::info!("pulsar call: {key:?} {from:?} → {to:?} ({:?})", step.ended);
    let _ = app.emit("call-state", CallState { key, phase: to, peer, since, supported });
    if let Some(kind) = step.ended.and_then(|w| w.notice()) {
        let _ = app.emit("call-notice", serde_json::json!({ "kind": kind, "key": key }));
    }
}

/// This phone's mic to the peer while the call is up: the plugin's AudioRecord, drained every
/// 20 ms into `DataMsg::Audio`. Asked again every 2 s while it gives nothing (the first ask may
/// only have raised the permission prompt).
fn start_mic<R: Runtime>(app: &AppHandle<R>, key: CallKey) {
    let Some(tx) = sender(app, key) else { return };
    let stop = Arc::new(Notify::new());
    with_calls(|m| m.entry(key).or_default().mic_stop = Some(stop.clone()));
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = app.pulsar_video().mic_start();
        let mut last_frame = Instant::now();
        let mut last_ask = Instant::now();
        'pump: loop {
            tokio::select! {
                biased;
                _ = stop.notified() => break 'pump,
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    loop {
                        let pcm = match app.pulsar_video().poll_mic_frame() {
                            Ok(p) if !p.is_empty() => p,
                            _ => break,
                        };
                        last_frame = Instant::now();
                        if send_data_via(&tx, &DataMsg::Audio(pcm)).await.is_err() {
                            break 'pump;
                        }
                    }
                    let now = Instant::now();
                    if now.duration_since(last_frame) > Duration::from_secs(2)
                        && now.duration_since(last_ask) > Duration::from_secs(2)
                    {
                        // The permission may have just been granted: the service can start now.
                        last_ask = now;
                        let _ = app.pulsar_video().call_service(true);
                        let _ = app.pulsar_video().mic_start();
                    }
                }
            }
        }
        let _ = app.pulsar_video().mic_stop();
        let _ = send_data_via(&tx, &DataMsg::AudioEnd).await;
        log::info!("pulsar call: {key:?} mic off");
    });
}

/// The `microphone` foreground service runs while any call rings out (we called: started then,
/// while Pulsar is surely on screen, so the mic still works if the peer answers after the user
/// left Pulsar — Android refuses to start it from the background) or is up.
fn sync_service<R: Runtime>(app: &AppHandle<R>) {
    static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let want = with_calls(|m| m.values().any(|e| matches!(e.call.phase(), Phase::Outgoing | Phase::Active)));
    if ON.swap(want, std::sync::atomic::Ordering::SeqCst) != want {
        let _ = app.pulsar_video().call_service(want);
    }
}

fn ringtone() -> String {
    let t = TONE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if t.is_empty() {
        "system".into()
    } else {
        t
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// JS: `invoke('call_action', { side: 'client'|'host', id, action: 'start'|'accept'|'decline'|'hangup' })`
#[tauri::command]
pub fn call_action<R: Runtime>(app: AppHandle<R>, side: String, id: u64, action: String) -> Result<(), String> {
    let key = CallKey::parse(&side, id).ok_or("bad side")?;
    self::action(&app, key, &action);
    Ok(())
}

/// JS: `invoke('call_states')` → every ongoing call (the UI seeds itself, then follows `call-state`).
#[tauri::command]
pub fn call_states() -> Vec<CallState> {
    with_calls(|m| {
        m.iter()
            .filter(|(_, e)| e.call.phase() != Phase::Idle || e.call.supported())
            .map(|(k, e)| CallState {
                key: *k,
                phase: e.call.phase(),
                peer: e.peer.clone(),
                since: e.since_ms,
                supported: e.call.supported(),
            })
            .collect()
    })
}

/// JS: `invoke('set_ringtone', { tone })` at start and on every Settings change.
#[tauri::command]
pub fn set_ringtone(tone: String) {
    *TONE.lock().unwrap_or_else(|e| e.into_inner()) = tone;
}

/// JS: `invoke('ringtone_preview', { tone, stop })` — Settings plays the pick for ~5 s.
#[tauri::command]
pub fn ringtone_preview<R: Runtime>(app: AppHandle<R>, tone: String, stop: bool) {
    let gen = PREVIEW_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    // Never cut a real call's ringing.
    let ringing = with_calls(|m| m.values().any(|e| e.call.phase() == Phase::Incoming));
    if ringing {
        return;
    }
    let _ = app.pulsar_video().ring_stop();
    if stop || tone == "none" {
        return;
    }
    let pcm = pulsar_core::call::tone_pcm(if tone == "system" { "classic" } else { &tone });
    let _ = app.pulsar_video().ring_start(&pcm, tone == "system");
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let ringing = with_calls(|m| m.values().any(|e| e.call.phase() == Phase::Incoming));
        if PREVIEW_GEN.load(std::sync::atomic::Ordering::SeqCst) == gen && !ringing {
            let _ = app.pulsar_video().ring_stop();
        }
    });
}
