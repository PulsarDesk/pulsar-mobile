//! Physical game controllers -> host (audit 3.1).
//!
//! The video plugin's `PadInput` (Kotlin) reads Android gamepads natively while a stream is
//! on screen and sends each pad's state to a loopback UDP socket owned here; this task
//! forwards it to the ACTIVE pane's host as `InputEvent::GamepadSlot` (the same event the
//! desktop client sends). No WebView and no per-event plugin call in the path. Before this
//! the only sender (`session/gamepad.js`) had been deleted and nothing ever called
//! `send_gamepad`, so controllers never reached the host.
//!
//! Each pad's last state is re-sent every [`RESEND`] for [`RESEND_WINDOW`] after its last
//! datagram, so a lost release datagram on the session can't leave a button held on the
//! host. The host emulation per pad comes from the Settings choice (`set_gamepad_targets`,
//! controller name -> "auto" | "xbox" | "ds4").
//!
//! Stale input: pad state belongs to ONE session. When the target changes (another active
//! pane, a reconnect, or no session at all) the old host gets a neutral state and every pad
//! is zeroed and silent until its next datagram, so a held button is never replayed into
//! the next session. PadInput itself sends neutral states on focus loss and stream stop.
//!
//! Loopback is shared by every app on the phone, so each datagram starts with a random
//! [`TOKEN_LEN`]-byte token handed to PadInput through the plugin call (only this process
//! sees it); anything without it is dropped. Otherwise any local app could drive a virtual
//! controller on the user's remote PC.
//!
//! Wire from Kotlin (little endian), one datagram each, after the token:
//! * `1` state:     `[1, pad, buttons u32, lx i16, ly i16, rx i16, ry i16, lt u8, rt u8]`
//! * `2` connected: `[2, pad, vendor u16, product u16, name UTF-8…]`
//! * `3` removed:   `[3, pad]`

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pulsar_core::SessionSender;
use pulsar_core::input::{EmulationTarget, GamepadKind, GamepadState};
use pulsar_core::service::InputEvent;
use tauri::{AppHandle, Manager, Runtime};

/// Re-send period of every connected pad's last state.
pub const RESEND: Duration = Duration::from_millis(500);
/// How long after a pad's last datagram its state keeps being re-sent. The host keeps the
/// last state anyway; this only covers a lost datagram.
pub const RESEND_WINDOW: Duration = Duration::from_secs(5);
/// Length of the per-process datagram token.
pub const TOKEN_LEN: usize = 16;

/// Controller name -> host emulation ("auto" | "xbox" | "ds4"), pushed by the UI.
#[derive(Default)]
pub struct PadTargets(pub Mutex<HashMap<String, String>>);

/// JS: `invoke('set_gamepad_targets', { targets: { "Xbox Wireless Controller": "ds4" } })`
#[tauri::command]
pub fn set_gamepad_targets<R: Runtime>(app: AppHandle<R>, targets: HashMap<String, String>) {
    *app.state::<PadTargets>().0.lock().unwrap() = targets;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadMsg {
    State { pad: u8, state: GamepadState },
    Connected { pad: u8, vendor: u16, product: u16, name: String },
    Removed { pad: u8 },
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

/// [`parse`] after checking and stripping the token. `None` = foreign or malformed.
pub fn parse_authed(b: &[u8], token: &[u8; TOKEN_LEN]) -> Option<PadMsg> {
    if b.len() < TOKEN_LEN {
        return None;
    }
    let (t, rest) = b.split_at(TOKEN_LEN);
    // Constant-time compare: no early exit on the first wrong byte.
    if t.iter().zip(token).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return None;
    }
    parse(rest)
}

#[cfg_attr(not(mobile), allow(dead_code))]
pub fn token_hex(token: &[u8; TOKEN_LEN]) -> String {
    token.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse(b: &[u8]) -> Option<PadMsg> {
    match (b.first()?, b.len()) {
        (1, 16..) => Some(PadMsg::State {
            pad: b[1],
            state: GamepadState {
                buttons: u32::from_le_bytes([b[2], b[3], b[4], b[5]]),
                left_x: u16_at(b, 6) as i16,
                left_y: u16_at(b, 8) as i16,
                right_x: u16_at(b, 10) as i16,
                right_y: u16_at(b, 12) as i16,
                left_trigger: b[14],
                right_trigger: b[15],
            },
        }),
        (2, 6..) => Some(PadMsg::Connected {
            pad: b[1],
            vendor: u16_at(b, 2),
            product: u16_at(b, 4),
            name: String::from_utf8_lossy(&b[6..]).into_owned(),
        }),
        (3, 2..) => Some(PadMsg::Removed { pad: b[1] }),
        _ => None,
    }
}

pub fn target_for(choice: Option<&str>) -> EmulationTarget {
    match choice {
        Some("xbox") => EmulationTarget::Xbox360,
        Some("ds4") => EmulationTarget::Ds4,
        _ => EmulationTarget::Auto,
    }
}

struct Pad {
    kind: GamepadKind,
    name: String,
    state: GamepadState,
    /// Got a state datagram since the last retarget (only these are (re-)sent).
    live: bool,
    last_rx: Instant,
}

impl Pad {
    fn new(kind: GamepadKind) -> Self {
        Pad { kind, name: String::new(), state: GamepadState::default(), live: false, last_rx: Instant::now() }
    }
}

/// The running feed: its port and token. `None` inside = the bind failed (not retried).
#[cfg_attr(not(mobile), allow(dead_code))]
static FEED: std::sync::OnceLock<Option<(u16, [u8; TOKEN_LEN])>> = std::sync::OnceLock::new();

/// Start the feed (once per process) and point the plugin at it (every call; idempotent on
/// the Kotlin side). Called at each connect. Android only; a no-op elsewhere.
pub fn ensure_started<R: Runtime>(app: &AppHandle<R>) {
    #[cfg(mobile)]
    {
        use tauri_plugin_pulsar_video::{PulsarVideoExt as _, PulsarVideoW5Ext as _};
        // Bind inside get_or_init: two concurrent connects (split view) create ONE socket
        // and ONE task, never a leaked second pair.
        let Some((port, token)) = *FEED.get_or_init(|| bind(app)) else { return };
        if let Err(e) = app.pulsar_video().pad_feed_start(port, &token_hex(&token)) {
            log::warn!("pulsar: pad feed start failed: {e:?}");
        }
    }
    let _ = app;
}

#[cfg_attr(not(mobile), allow(dead_code))]
fn bind<R: Runtime>(app: &AppHandle<R>) -> Option<(u16, [u8; TOKEN_LEN])> {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").ok()?;
    sock.set_nonblocking(true).ok()?;
    let port = sock.local_addr().ok()?.port();
    let mut token = [0u8; TOKEN_LEN];
    token.copy_from_slice(&pulsar_core::crypto::random_salt()[..TOKEN_LEN]);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let sock = match tokio::net::UdpSocket::from_std(sock) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("pulsar: pad feed socket: {e}");
                return;
            }
        };
        run(app, sock, token).await;
    });
    log::info!("pulsar: pad feed on 127.0.0.1:{port}");
    Some((port, token))
}

/// Where pad input goes now: the active pane's session (slot, its reconnect generation,
/// its sender). `None` = no session on the active pane.
type Target = (u8, u64, SessionSender);

fn current_target<R: Runtime>(app: &AppHandle<R>) -> Option<Target> {
    let slot = app.state::<crate::session_cmds::ActivePane>().get();
    let sender = app.state::<crate::client::InputSenders>().0.lock().unwrap().get(&slot).cloned()?;
    Some((slot, crate::client::current_slot_gen(slot), sender))
}

/// Follow the active pane. On a change the old session gets a neutral state for each pad
/// it saw, and every pad is zeroed and stays silent until its next datagram.
async fn retarget<R: Runtime>(app: &AppHandle<R>, cur: &mut Option<Target>, pads: &mut HashMap<u8, Pad>) {
    let now = current_target(app);
    let same = match (cur.as_ref(), now.as_ref()) {
        (Some(a), Some(b)) => a.0 == b.0 && a.1 == b.1,
        (None, None) => true,
        _ => false,
    };
    if same {
        return;
    }
    if let Some((_, _, old)) = cur.take() {
        for (idx, p) in pads.iter_mut().filter(|(_, p)| p.live) {
            p.state = GamepadState::default();
            let ev = slot_event(app, *idx, p);
            // The old session may be gone already: then this fails harmlessly.
            let _ = pulsar_core::service::send_input_via(&old, &ev).await;
        }
    }
    for p in pads.values_mut() {
        p.state = GamepadState::default();
        p.live = false;
    }
    *cur = now;
}

async fn send(cur: &Option<Target>, ev: &InputEvent) {
    if let Some((_, _, s)) = cur {
        let _ = pulsar_core::service::send_input_via(s, ev).await;
    }
}

/// The controller name as Settings shows (and keys the emulation choice by): the native
/// name without a trailing "(…)" suffix — the same rule as gamepad-monitor.js `shortName`.
pub fn short_name(name: &str) -> &str {
    let t = name.trim();
    if t.ends_with(')') {
        if let Some(i) = t.find('(') {
            return t[..i].trim();
        }
    }
    t
}

fn slot_event<R: Runtime>(app: &AppHandle<R>, idx: u8, p: &Pad) -> InputEvent {
    let target = {
        let t = app.state::<PadTargets>();
        let m = t.0.lock().unwrap();
        target_for(m.get(short_name(&p.name)).map(String::as_str))
    };
    InputEvent::GamepadSlot { slot: idx, kind: p.kind, target, state: p.state }
}

async fn run<R: Runtime>(app: AppHandle<R>, sock: tokio::net::UdpSocket, token: [u8; TOKEN_LEN]) {
    let mut pads: HashMap<u8, Pad> = HashMap::new();
    let mut cur: Option<Target> = None;
    let mut buf = [0u8; 512];
    let mut tick = tokio::time::interval(RESEND);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = sock.recv(&mut buf) => {
                let Ok(n) = r else { continue };
                let Some(msg) = parse_authed(&buf[..n], &token) else { continue };
                retarget(&app, &mut cur, &mut pads).await;
                match msg {
                    PadMsg::Connected { pad, vendor, product, name } => {
                        let kind = GamepadKind::from_vid_pid(vendor, product);
                        log::info!("pulsar: pad {pad} = {name} ({vendor:04x}:{product:04x}, {})", kind.label());
                        let e = pads.entry(pad).or_insert_with(|| Pad::new(kind));
                        e.kind = kind;
                        e.name = name;
                    }
                    PadMsg::State { pad, state } => {
                        let p = pads.entry(pad).or_insert_with(|| Pad::new(GamepadKind::Unknown));
                        p.state = state;
                        p.live = true;
                        p.last_rx = Instant::now();
                        let ev = slot_event(&app, pad, p);
                        send(&cur, &ev).await;
                    }
                    PadMsg::Removed { pad } => {
                        // Only a pad this session saw has a virtual pad on its host.
                        if pads.remove(&pad).is_some_and(|p| p.live) {
                            send(&cur, &InputEvent::GamepadDisconnect { slot: pad }).await;
                        }
                    }
                }
            }
            _ = tick.tick() => {
                retarget(&app, &mut cur, &mut pads).await;
                for (idx, p) in pads.iter().filter(|(_, p)| p.live && p.last_rx.elapsed() < RESEND_WINDOW) {
                    let ev = slot_event(&app, *idx, p);
                    send(&cur, &ev).await;
                }
            }
        }
    }
}

/// Host rumble (`DataMsg::Rumble`) -> the physical pad's motors. Android only.
pub fn rumble<R: Runtime>(app: &AppHandle<R>, pad: u8, large: u8, small: u8) {
    #[cfg(mobile)]
    {
        let app = app.clone();
        // A plugin call is a blocking JNI hop: keep it off the session read loop.
        tauri::async_runtime::spawn_blocking(move || {
            use tauri_plugin_pulsar_video::{PulsarVideoExt as _, PulsarVideoW5Ext as _};
            let _ = app.pulsar_video().pad_rumble(pad, large, small);
        });
    }
    let _ = (app, pad, large, small);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state() {
        let mut b = vec![1u8, 2];
        b.extend_from_slice(&0x0000_4801u32.to_le_bytes()); // A + DPAD_UP + DPAD_RIGHT
        for v in [32767i16, -32767, 0, 100] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&[255, 7]);
        assert_eq!(
            parse(&b),
            Some(PadMsg::State {
                pad: 2,
                state: GamepadState {
                    buttons: 0x4801,
                    left_x: 32767,
                    left_y: -32767,
                    right_x: 0,
                    right_y: 100,
                    left_trigger: 255,
                    right_trigger: 7,
                },
            })
        );
        assert_eq!(parse(&b[..15]), None); // short
    }

    #[test]
    fn parses_connect_and_remove() {
        let mut b = vec![2u8, 0, 0x4C, 0x05, 0xE6, 0x0C];
        b.extend_from_slice("DualSense Wireless Controller".as_bytes());
        assert_eq!(
            parse(&b),
            Some(PadMsg::Connected { pad: 0, vendor: 0x054C, product: 0x0CE6, name: "DualSense Wireless Controller".into() })
        );
        assert_eq!(GamepadKind::from_vid_pid(0x054C, 0x0CE6), GamepadKind::Ds5);
        assert_eq!(parse(&[3, 1]), Some(PadMsg::Removed { pad: 1 }));
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[9, 0, 0]), None);
    }

    #[test]
    fn foreign_datagrams_are_dropped() {
        let token = [7u8; TOKEN_LEN];
        let mut b = token.to_vec();
        b.extend_from_slice(&[3, 1]);
        assert_eq!(parse_authed(&b, &token), Some(PadMsg::Removed { pad: 1 }));
        b[0] ^= 1;
        assert_eq!(parse_authed(&b, &token), None);
        assert_eq!(parse_authed(&[3, 1], &token), None); // no token
        assert_eq!(parse_authed(&token, &token), None); // token only
        let mut t = [0u8; TOKEN_LEN];
        t[0] = 0x0f;
        t[1] = 0xa0;
        let hex = token_hex(&t);
        assert_eq!(hex.len(), 2 * TOKEN_LEN);
        assert!(hex.starts_with("0fa000"));
    }

    #[test]
    fn short_names_match_the_settings_list() {
        assert_eq!(short_name("Xbox Wireless Controller"), "Xbox Wireless Controller");
        assert_eq!(short_name("Pad (STANDARD GAMEPAD Vendor: 054c Product: 0ce6)"), "Pad");
        assert_eq!(short_name("  Pad  "), "Pad");
        assert_eq!(short_name("Pad (x"), "Pad (x");
    }

    #[test]
    fn targets() {
        assert_eq!(target_for(Some("xbox")), EmulationTarget::Xbox360);
        assert_eq!(target_for(Some("ds4")), EmulationTarget::Ds4);
        assert_eq!(target_for(Some("auto")), EmulationTarget::Auto);
        assert_eq!(target_for(None), EmulationTarget::Auto);
    }
}
