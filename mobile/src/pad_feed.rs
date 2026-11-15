//! Physical game controllers -> host (audit 3.1).
//!
//! The video plugin's `PadInput` (Kotlin) reads Android gamepads natively while a stream is
//! on screen and sends each pad's state to a loopback UDP socket owned here; this task
//! forwards it to the ACTIVE pane's host as `InputEvent::GamepadSlot` (the same event the
//! desktop client sends). No WebView and no per-event plugin call in the path. Before this
//! the only sender (`session/gamepad.js`) had been deleted and nothing ever called
//! `send_gamepad`, so controllers never reached the host.
//!
//! Each pad's last state is re-sent every [`RESEND`] so a lost release datagram on the
//! session can't leave a button held on the host. The host emulation per pad comes from the
//! Settings choice (`set_gamepad_targets`, controller name -> "auto" | "xbox" | "ds4").
//!
//! Wire from Kotlin (little endian), one datagram each:
//! * `1` state:     `[1, pad, buttons u32, lx i16, ly i16, rx i16, ry i16, lt u8, rt u8]`
//! * `2` connected: `[2, pad, vendor u16, product u16, name UTF-8…]`
//! * `3` removed:   `[3, pad]`

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use pulsar_core::input::{EmulationTarget, GamepadKind, GamepadState};
use pulsar_core::service::InputEvent;
use tauri::{AppHandle, Manager, Runtime};

/// Re-send period of every connected pad's last state.
pub const RESEND: Duration = Duration::from_millis(500);

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
}

#[cfg_attr(not(mobile), allow(dead_code))]
static PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

/// Start the feed (once per process) and point the plugin at it (every call; idempotent on
/// the Kotlin side). Called at each connect. Android only; a no-op elsewhere.
pub fn ensure_started<R: Runtime>(app: &AppHandle<R>) {
    #[cfg(mobile)]
    {
        use tauri_plugin_pulsar_video::{PulsarVideoExt as _, PulsarVideoW5Ext as _};
        let port = match PORT.get() {
            Some(p) => *p,
            None => match bind(app) {
                Some(p) => *PORT.get_or_init(|| p),
                None => return,
            },
        };
        if let Err(e) = app.pulsar_video().pad_feed_start(port) {
            log::warn!("pulsar: pad feed start failed: {e:?}");
        }
    }
    let _ = app;
}

#[cfg_attr(not(mobile), allow(dead_code))]
fn bind<R: Runtime>(app: &AppHandle<R>) -> Option<u16> {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").ok()?;
    sock.set_nonblocking(true).ok()?;
    let port = sock.local_addr().ok()?.port();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let sock = match tokio::net::UdpSocket::from_std(sock) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("pulsar: pad feed socket: {e}");
                return;
            }
        };
        run(app, sock).await;
    });
    log::info!("pulsar: pad feed on 127.0.0.1:{port}");
    Some(port)
}

async fn send<R: Runtime>(app: &AppHandle<R>, ev: &InputEvent) {
    let slot = app.state::<crate::session_cmds::ActivePane>().get();
    let sender = app.state::<crate::client::InputSenders>().0.lock().unwrap().get(&slot).cloned();
    if let Some(s) = sender {
        let _ = pulsar_core::service::send_input_via(&s, ev).await;
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

async fn run<R: Runtime>(app: AppHandle<R>, sock: tokio::net::UdpSocket) {
    let mut pads: HashMap<u8, Pad> = HashMap::new();
    let mut buf = [0u8; 512];
    let mut tick = tokio::time::interval(RESEND);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = sock.recv(&mut buf) => {
                let Ok(n) = r else { continue };
                match parse(&buf[..n]) {
                    Some(PadMsg::Connected { pad, vendor, product, name }) => {
                        let kind = GamepadKind::from_vid_pid(vendor, product);
                        log::info!("pulsar: pad {pad} = {name} ({vendor:04x}:{product:04x}, {})", kind.label());
                        let e = pads.entry(pad).or_insert(Pad { kind, name: String::new(), state: GamepadState::default() });
                        e.kind = kind;
                        e.name = name;
                    }
                    Some(PadMsg::State { pad, state }) => {
                        let p = pads.entry(pad).or_insert(Pad { kind: GamepadKind::Unknown, name: String::new(), state });
                        p.state = state;
                        let ev = slot_event(&app, pad, p);
                        send(&app, &ev).await;
                    }
                    Some(PadMsg::Removed { pad }) => {
                        pads.remove(&pad);
                        send(&app, &InputEvent::GamepadDisconnect { slot: pad }).await;
                    }
                    None => {}
                }
            }
            _ = tick.tick() => {
                for (idx, p) in pads.iter() {
                    let ev = slot_event(&app, *idx, p);
                    send(&app, &ev).await;
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
