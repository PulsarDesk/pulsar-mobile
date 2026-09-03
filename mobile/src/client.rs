//! M4 live client: connect to a host via the relay (or direct IP) and stream its
//! video onto the native surface.
//!
//! Drives pulsar-core (register → connect → authenticate → request_stream with
//! media-over-session), reads RTP video out of the encrypted session, runs it
//! through the [`crate::rtp`] depacketizer, and hands Annex-B access units to the
//! native decoder plugin (`PulsarVideo::feed_au`). The webview UI sits on top of
//! the surface the decoder renders into (Path A).
//!
//! ## W2-rust additions
//! - `conn-phase` events emitted at each milestone (transport, auth, preparing).
//! - 45-second overall timeout + 30-second post-auth timeout → `connect-timed-out`.
//! - `target` param (renamed from `id`) branches on `DeviceId::parse` vs
//!   `SocketAddr`/`connect_direct` for IP-direct LAN connections.
//! - `mode`, `codec`, `fps`, `bitrate_kbps`, `width`, `height`, `quality` params
//!   are threaded into `StreamReq` (game mode → `QualityPref::Latency`).
//! - Read loop `select!`s on cancel `Notify` (from `session_cmds::SessionRegistry`)
//!   and a restream `mpsc` channel.
//! - Emits `play-ended { slot, reason }` on all read-loop exit paths.
//!
//! ## W3-rust additions
//! - Auth race loop: on `HostAuth::NeedPassword`, emits `auth-prompt { slot, peer }`
//!   and awaits a `submit_password` oneshot from `PwPending` (30-second timeout).
//!   On timeout emits `auth-prompt` with reason "timeout" and aborts the connect.
//! - Read loop now tracks fps/mbps over a ~1-second window and emits `play-stats`.
//! - Emits `play-firstframe { slot }` on the first decoded access unit.
//! - Emits `play-stall { slot, stalled: true }` after ~2 seconds without a
//!   `TAG_VIDEO` packet; emits `play-stall { slot, stalled: false }` on resume.
//! - Restream path: on a new `StreamReq` from the quality commands, re-calls
//!   `request_stream` + re-arms `start_stream` for the new SPS. Stores the req
//!   back into `SessionRegistry::last_req` via `update_last_req`.
//! - Calls `pulsar_video().set_aspect` on stream entry (no-op stub until
//!   W3-media-native adds the `set_aspect` method to the plugin — see cross-lane
//!   note at the call site).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_pulsar_video::PulsarVideoExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio::time::timeout;

use pulsar_core::proto::DeviceId;
use pulsar_core::adapt::{self, Controller};
use pulsar_core::service::{
    self, query_stream_caps, recv_host_auth, request_games, request_launch, request_stream,
    send_auth, send_bye, send_data_via, send_input_via, AuthOutcome, ClientStats, DataMsg,
    GameInfo, HostAuth, InputEvent, LossRecovery, StreamReq,
};
use pulsar_core::{NetworkMode, QualityPref, SessionSender, Transport};

use crate::config::load_config;
use crate::rtp::{Codec, Depacketizer};
use crate::session_cmds::{ActivePane, ClaimedDisplays, PwPending, SessionRegistry};

// ── Connect / connection-phase timeouts ───────────────────────────────────────

/// Total time budget from `node.connect()` through `request_stream` (connect +
/// auth + stream-setup). Mirrors the desktop's `CONNECT_TIMEOUT_MS = 45_000`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);

/// Additional time allowed AFTER authentication succeeds for `request_stream` to
/// complete. Mirrors the desktop's `POST_AUTH_TIMEOUT_MS = 30_000`.
const POST_AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// How long we wait for the user to enter a password after an `auth-prompt`.
const PW_SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Reserved slot for the game-mode games FETCH (`list_remote_games`). Distinct from
/// the real stream slots (0/1) so a fetch's auth-prompt/OTP + cancel never collide
/// with a live or upcoming stream session. The auth sheet keys its DOM off the slot,
/// so any value works; `submit_password(FETCH_SLOT, …)` routes to the right pending.
const FETCH_SLOT: u8 = 200;

/// Interval for fps/mbps stats emission.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// After this many consecutive milliseconds without a TAG_VIDEO packet, emit
/// `play-stall { stalled: true }`.
const STALL_THRESHOLD: Duration = Duration::from_millis(2000);

// ── InputSenders managed state ────────────────────────────────────────────────

/// Live per-slot input senders: each connected session's [`SessionSender`] so the
/// `send_pointer`/`send_button`/`send_scroll`/… commands can forward touch from
/// the webview to the remote host concurrently with the read loop.
#[derive(Default)]
pub struct InputSenders(pub Mutex<HashMap<u8, SessionSender>>);

// ── Per-slot reconnect generation (start_stream/register race guard) ──────────

/// Monotonic per-slot counter bumped by `connect_host` right BEFORE it arms the
/// native decoder (`start_stream`), and captured by that connect's read loop. A
/// superseded old read loop for the same slot captured an earlier value, so it
/// fails the generation check at teardown and skips its slot-keyed cleanup
/// (`stop_stream`/`remove`/`play-ended`) even in the window BEFORE the new loop's
/// `register()` installs its `is_owner` token. Without this, a reconnect whose old
/// loop finished teardown inside that window tore down the freshly-armed new
/// session (reconnect appears to succeed, then instantly goes black/ends).
/// Module-global (not Tauri managed state) so the fix stays self-contained here;
/// keyed by slot so one slot's reconnect never supersedes another's.
static SLOT_GEN: LazyLock<Mutex<HashMap<u8, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bump `slot`'s reconnect generation and return the new value.
fn bump_slot_gen(slot: u8) -> u64 {
    let mut g = SLOT_GEN.lock().unwrap();
    let n = g.entry(slot).or_insert(0);
    *n += 1;
    *n
}

/// The current reconnect generation for `slot` (0 if never connected).
fn current_slot_gen(slot: u8) -> u64 {
    *SLOT_GEN.lock().unwrap().get(&slot).unwrap_or(&0)
}

#[cfg(test)]
mod slot_gen_tests {
    use super::{bump_slot_gen, current_slot_gen};

    #[test]
    fn generation_bumps_monotonically_per_slot() {
        // High, otherwise-unused slots so this can't collide with real connect logic
        // or other tests sharing the process-global SLOT_GEN map.
        let slot = 240u8;
        let g1 = bump_slot_gen(slot);
        let g2 = bump_slot_gen(slot);
        assert_eq!(g2, g1 + 1, "same slot must advance by exactly one");
        assert_eq!(current_slot_gen(slot), g2, "current reflects the last bump");
        // A never-bumped slot reads 0 and is independent of `slot`.
        assert_eq!(current_slot_gen(241u8), 0);
    }
}

// ── conn-phase event ─────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ConnPhasePayload {
    slot: u8,
    phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
}

// ── relay health probe (home-screen indicator) ───────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayHealth {
    pub healthy: bool,
    pub latency_ms: u32,
}

/// Lightweight relay reachability probe for the Connect-screen status indicator.
/// Sends a single bogus-id `Heartbeat` to the relay and waits up to 2s for ANY
/// reply: the relay always answers a heartbeat (a `HeartbeatAck` when authed, or a
/// `NotRegistered` error otherwise), so any datagram back means the relay is up.
/// No registration and no device entry — unlike `register()` it has no side
/// effects and isn't rate-limited (the relay only throttles `Register`).
///
/// JS: `invoke('relay_health', { relay })` → `{ healthy, latencyMs }`.
#[tauri::command]
pub async fn relay_health<R: Runtime>(
    app: AppHandle<R>,
    relay: String,
) -> Result<RelayHealth, String> {
    use pulsar_core::proto::{encode, ClientMsg, DeviceId, Token};
    let relay_str = if relay.is_empty() { load_config(&app).relay } else { relay };
    let addr: SocketAddr = crate::net::parse_relay(&relay_str).await?;
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;
    sock.connect(addr).await.map_err(|e| e.to_string())?;
    let probe = encode(&ClientMsg::Heartbeat {
        id: DeviceId(0),
        token: Token([0u8; 16]),
    });
    let start = std::time::Instant::now();
    sock.send(&probe).await.map_err(|e| e.to_string())?;
    let mut buf = [0u8; 256];
    let ok = tokio::time::timeout(std::time::Duration::from_millis(2000), sock.recv(&mut buf))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
    Ok(RelayHealth {
        healthy: ok,
        latency_ms: if ok { start.elapsed().as_millis() as u32 } else { 0 },
    })
}

/// Best-effort local LAN IP of this device, shown on the host screen so peers on
/// the same network can connect by address. Opens a UDP socket toward a public
/// address (no packets are actually sent) and reads the chosen source IP.
/// JS: `invoke('local_ip')` → "192.168.x.y" (or "" if unavailable).
#[tauri::command]
pub async fn local_ip() -> Result<String, String> {
    let sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return Ok(String::new()),
    };
    if sock.connect("8.8.8.8:80").await.is_err() {
        return Ok(String::new());
    }
    Ok(sock
        .local_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default())
}

/// The local UDP port this device's relay node is currently bound to, or `0` if the
/// node hasn't been created yet (no connection/host session this run). The Settings
/// "local port" field shows it as the live placeholder when no port is pinned.
/// JS: `invoke('node_port')` → u16.
#[tauri::command]
pub async fn node_port<R: Runtime>(app: AppHandle<R>) -> Result<u16, String> {
    let shared = app.state::<crate::net::SharedNode>();
    let g = shared.0.lock().await;
    Ok(g.as_ref()
        .and_then(|i| i.node.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(0))
}

/// App version + whether this is a local/debug build (`cargo`/`tauri … dev`) vs a
/// CI release. The Settings "About" card shows it so a dev build is clearly marked.
/// JS: `invoke('app_build_info')` → `{ version, local }`.
#[derive(Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    pub local: bool,
}

#[tauri::command]
pub async fn app_build_info() -> Result<BuildInfo, String> {
    Ok(BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        local: cfg!(debug_assertions),
    })
}

fn emit_phase<R: Runtime>(app: &AppHandle<R>, slot: u8, phase: &str, transport: Option<&str>) {
    let _ = app.emit(
        "conn-phase",
        ConnPhasePayload {
            slot,
            phase: phase.to_string(),
            transport: transport.map(str::to_string),
        },
    );
}

// ── play-ended event ──────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlayEndedPayload {
    slot: u8,
    reason: String,
}

fn emit_play_ended<R: Runtime>(app: &AppHandle<R>, slot: u8, reason: &str) {
    let _ = app.emit("play-ended", PlayEndedPayload { slot, reason: reason.to_string() });
}

// ── play-stats event ──────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlayStatsPayload {
    slot: u8,
    fps: f32,
    mbps: f32,
    transport: String,
}

fn emit_play_stats<R: Runtime>(
    app: &AppHandle<R>,
    slot: u8,
    fps: f32,
    mbps: f32,
    transport: &str,
) {
    let _ = app.emit(
        "play-stats",
        PlayStatsPayload { slot, fps, mbps, transport: transport.to_string() },
    );
}

// ── play-firstframe event ─────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlayFirstFramePayload {
    slot: u8,
}

fn emit_play_firstframe<R: Runtime>(app: &AppHandle<R>, slot: u8) {
    let _ = app.emit("play-firstframe", PlayFirstFramePayload { slot });
}

// ── play-rtt event (network round-trip latency for the perf HUD) ──────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlayRttPayload {
    slot: u8,
    rtt: f64, // milliseconds
}

fn emit_play_rtt<R: Runtime>(app: &AppHandle<R>, slot: u8, rtt: f64) {
    let _ = app.emit("play-rtt", PlayRttPayload { slot, rtt });
}

/// Connect to the native decoder's loopback AU socket (the plugin returns `port=N`
/// from `start_stream`). Streaming raw length-prefixed access units over this socket
/// bypasses the per-frame base64+JSON IPC, which marshalled at ~12 ms/call and capped
/// throughput near 84 fps (→ unbounded backlog ≈ 1 s latency at 120 fps). `None` falls
/// the read loop back to the IPC `feed_au` path (still fine up to ~60 fps).
async fn connect_au_socket(detail: &str) -> Option<tokio::net::TcpStream> {
    let port: u16 = detail.strip_prefix("port=").and_then(|s| s.trim().parse().ok())?;
    match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            log::info!("pulsar: au-socket connected :{port}");
            Some(s)
        }
        Err(e) => {
            log::warn!("pulsar: au-socket connect :{port} failed: {e:?} — falling back to IPC feed");
            None
        }
    }
}

// ── play-stall event ──────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlayStallPayload {
    slot: u8,
    stalled: bool,
}

fn emit_play_stall<R: Runtime>(app: &AppHandle<R>, slot: u8, stalled: bool) {
    let _ = app.emit("play-stall", PlayStallPayload { slot, stalled });
}

// ── auth-prompt event ─────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AuthPromptPayload {
    slot: u8,
    peer: String,
}

fn emit_auth_prompt<R: Runtime>(app: &AppHandle<R>, slot: u8, peer: &str) {
    let _ = app.emit("auth-prompt", AuthPromptPayload { slot, peer: peer.to_string() });
}

// ── Adaptive streaming ──────────────────────────────────────────────────────
// The controller lives in `pulsar_core::adapt` (shared with the desktop app, Phase 1 of
// pulsar-core/docs/adaptive-streaming.md): loss + keepalive RTT + a per-frame delay-gradient
// estimator → target rate, operating point (resolution × fps ladder), encoder bitrate net
// of FEC, and the host's loss-recovery mode. This file measures and actuates only.

/// Decision cadence (the controller works on 2 s windows).
const ABR_TICK: Duration = Duration::from_millis(2000);
/// Auto-rate cap / start (kbit/s) when the user did not pick a bitrate.
const AUTO_CAP_KBPS: u32 = 12_000;
const AUTO_START_KBPS: u32 = 8_000;

#[tauri::command]
pub async fn send_pointer<R: Runtime>(
    app: AppHandle<R>,
    slot: u8,
    x: f64,
    y: f64,
) -> Result<(), String> {
    let s = app.state::<InputSenders>().0.lock().unwrap().get(&slot).cloned();
    if let Some(s) = s {
        let _ = send_input_via(&s, &InputEvent::PointerMotion { x, y }).await;
    }
    Ok(())
}

/// Forward a pointer button (0=left/tap, 1=right, 2=middle) press/release to
/// session `slot`'s host.
#[tauri::command]
pub async fn send_button<R: Runtime>(
    app: AppHandle<R>,
    slot: u8,
    button: u8,
    down: bool,
) -> Result<(), String> {
    let s = app.state::<InputSenders>().0.lock().unwrap().get(&slot).cloned();
    if let Some(s) = s {
        let _ = send_input_via(&s, &InputEvent::PointerButton { button, down }).await;
    }
    Ok(())
}

/// Apply a local pinch-zoom/pan transform to slot `slot`'s native video surface.
/// `x,y,w,h` are the video's destination rect on screen, NORMALIZED to the surface
/// [0..1] (`w`/`h` > 1 = zoomed in). Local-only: the host stream is untouched; the
/// client magnifies the decoded surface and maps touches back to frame coords. The
/// JS layer computes the rect aspect-correctly from the `video-size` event.
///
/// JS: `invoke('set_video_transform', { slot, x, y, w, h })`
#[tauri::command]
pub async fn set_video_transform<R: Runtime>(
    app: AppHandle<R>,
    slot: u8,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Result<(), String> {
    #[cfg(mobile)]
    {
        let _ = app.pulsar_video().set_video_transform(slot, x, y, w, h);
    }
    #[cfg(not(mobile))]
    {
        let _ = (&app, slot, x, y, w, h);
    }
    Ok(())
}

// ── W4-rust-client commands ───────────────────────────────────────────────────

/// Ask the host to reverse the connection direction: the host will connect back
/// to *us* as a client (sending our relay-assigned ID so the host can find us).
///
/// Sends `DataMsg::ReverseRequest(myId)` over the active session for `slot`.
/// The local device ID is read from [`SessionRegistry`] where it was stored by
/// `connect_host` after registration.
///
/// JS: `invoke('reverse_play', { slot: 0 })`
#[tauri::command]
pub async fn reverse_play<R: Runtime>(app: AppHandle<R>, slot: u8) -> Result<(), String> {
    let sender = app.state::<InputSenders>().0.lock().unwrap().get(&slot).cloned();
    let my_id = app
        .state::<crate::session_cmds::SessionRegistry>()
        .my_id(slot)
        .ok_or_else(|| format!("no active session on slot {slot}"))?;

    if let Some(s) = sender {
        // Format the device ID as a 9-digit string (the relay canonical form).
        let id_str = format!("{:09}", my_id);
        send_data_via(&s, &DataMsg::ReverseRequest(id_str))
            .await
            .map_err(|e| format!("reverse_play send failed: {e:?}"))?;
    }
    Ok(())
}

/// Begin capturing this device's microphone and streaming raw s16le PCM to the
/// host as `DataMsg::Audio` frames (~20 ms per frame = 960 samples at 48 kHz).
///
/// The mic capture is performed by the native plugin (`micStart` Kotlin command,
/// added by the W4-mic lane). This command spawns a background task that:
/// 1. Calls `plugin:pulsar-video|micStart` (via `app.pulsar_video().mic_start()`).
/// 2. Polls `app.pulsar_video().mic_pcm()` every ~20 ms, reading raw s16le PCM.
/// 3. Wraps each chunk in `DataMsg::Audio(pcm)` and forwards via `send_data_via`.
/// 4. Stops when `mic_stop` is called (which notifies the per-slot `mic_cancel`).
///
/// # Cross-lane note
/// The plugin methods `mic_start()`, `mic_stop()`, and `mic_pcm()` on
/// `PulsarVideo<R>` are added by the W4-mic lane (in `crates/tauri-plugin-pulsar-
/// video/src/mobile.rs` and `desktop.rs`). Until W4-mic lands, those calls are
/// stubbed below with a `TODO` comment and the function degrades gracefully
/// (returns `Ok(())` without capturing anything).
///
/// JS: `invoke('mic_start', { slot: 0 })`
#[tauri::command]
pub async fn mic_start<R: Runtime>(app: AppHandle<R>, slot: u8) -> Result<(), String> {
    // Guard: slot must have an active session (we need its sender).
    let sender = {
        let senders = app.state::<InputSenders>();
        let guard = senders.0.lock().unwrap();
        guard.get(&slot).cloned()
    };
    let sender = match sender {
        Some(s) => s,
        None => return Err(format!("no active session on slot {slot}")),
    };

    // Retrieve the mic_cancel Notify so mic_stop can abort the loop.
    let mic_cancel = app
        .state::<crate::session_cmds::SessionRegistry>()
        .mic_cancel(slot)
        .ok_or_else(|| format!("no active session on slot {slot}"))?;

    // Start the native AudioRecord capture (Kotlin fills an internal PCM buffer that
    // the pump loop below drains via `poll_mic_frame`). No-op on desktop.
    if let Err(e) = app.pulsar_video().mic_start() {
        log::warn!("pulsar: mic_start plugin call failed slot={slot}: {e:?}");
    }
    log::info!("pulsar: mic_start slot={slot}");

    // Spawn the PCM-pump loop.
    let app_mic = app.clone();
    tauri::async_runtime::spawn(async move {
        // The native buffer fills at ~20 ms per frame; poll on the same cadence but
        // drain every ready frame each tick so mic latency can't accumulate if a tick
        // slips (Kotlin drops the oldest frame at its buffer cap either way).
        const FRAME_INTERVAL: Duration = Duration::from_millis(20);

        'pump: loop {
            tokio::select! {
                biased;
                _ = mic_cancel.notified() => {
                    // mic_stop was called — send AudioEnd to signal end of mic to host.
                    log::info!("pulsar: mic loop cancelled slot={slot}");
                    let _ = send_data_via(&sender, &DataMsg::AudioEnd).await;
                    break 'pump;
                }
                _ = tokio::time::sleep(FRAME_INTERVAL) => {
                    // Drain every PCM frame the native mic buffer has ready this tick
                    // and forward each as real s16le audio to the host.
                    loop {
                        let pcm = match app_mic.pulsar_video().poll_mic_frame() {
                            Ok(pcm) => pcm,
                            Err(_) => break, // transient plugin/JNI error — retry next tick
                        };
                        if pcm.is_empty() {
                            break; // buffer drained for this tick
                        }
                        if let Err(e) = send_data_via(&sender, &DataMsg::Audio(pcm)).await {
                            log::warn!("pulsar: mic_start send failed slot={slot}: {e:?}");
                            break 'pump;
                        }
                    }
                }
            }
        }
    });

    Ok(())
}

/// Stop the microphone capture started by `mic_start` for `slot`.
///
/// Notifies the per-slot `mic_cancel` so the PCM-pump loop sends
/// `DataMsg::AudioEnd` and exits.
///
/// JS: `invoke('mic_stop', { slot: 0 })`
#[tauri::command]
pub async fn mic_stop<R: Runtime>(app: AppHandle<R>, slot: u8) -> Result<(), String> {
    if let Some(cancel) = app
        .state::<crate::session_cmds::SessionRegistry>()
        .mic_cancel(slot)
    {
        cancel.notify_one();
    }
    // Stop the native AudioRecord capture. Any PCM still buffered is left for the
    // pump loop to drain before it exits on the cancel above. No-op on desktop.
    if let Err(e) = app.pulsar_video().mic_stop() {
        log::warn!("pulsar: mic_stop plugin call failed slot={slot}: {e:?}");
    }
    log::info!("pulsar: mic_stop slot={slot}");
    Ok(())
}

// ── W5: lan_devices command (best-effort stub) ────────────────────────────────

/// Return the list of Pulsar devices discovered on the local network via the
/// UDP multicast beacon ([`pulsar_core::Discovery`]).
///
/// **STATUS: STUB (returns `[]`) — FLAG W5-lan-presence**
///
/// `Discovery::start` requires an `async` context and several arguments
/// (device name, node port, public key, relay id) that are only available
/// after a Node has been bound and registered. On a cellular-only Android
/// device multicast may be unavailable entirely. Given these constraints and
/// the low priority of this feature (prio 5 in the roadmap), this command
/// returns an empty array. The JS layer (`devices.js`) gracefully degrades
/// by showing no "online" dots.
///
/// When this is fully implemented, the caller should:
/// 1. Start `Discovery::start(name, port, pubkey, id)` once after `go_online`
///    or the first `connect_host` registration and store the handle in managed
///    state (the W5-native or a dedicated W5-lan lane).
/// 2. Return `discovery.peers().await` mapped to `[{id, name, addr}]`.
///
/// JS: `invoke('lan_devices', {})` → `[{ id: string|null, name: string, addr: string }]`
#[derive(serde::Serialize)]
pub struct LanDevice {
    /// The beacon's *claimed* relay id string (e.g. `"123456789"`), or `null`.
    pub id: Option<String>,
    pub name: String,
    /// `IP:port` of the peer's node socket (as observed from our multicast receive).
    pub addr: String,
}

#[tauri::command]
pub async fn lan_devices<R: Runtime>(app: tauri::AppHandle<R>) -> Result<Vec<LanDevice>, String> {
    // Start (or reuse) the multicast discovery beacon and return the peers it has
    // heard. Requires the Android MulticastLock (acquired in the pulsar-video
    // plugin's load() — tracked code, not the gitignored gen/ MainActivity) to
    // receive announces; without it this returns an empty list (graceful degrade).
    let disc = crate::net::get_or_create_discovery(&app).await?;
    let our_pubkey = crate::identity::load_identity(&app).public_bytes();
    // Collapse a device seen under several beacon nonces (a restart/reconnect mints a
    // fresh nonce; the stale beacons linger until TTL) into ONE row per identity pubkey,
    // keeping the freshest — otherwise the same device shows as duplicate rows sharing
    // its relay id. Drop our own beacon in the same pass.
    let mut by_key: std::collections::HashMap<[u8; 32], pulsar_core::discovery::DiscoveredPeer> =
        std::collections::HashMap::new();
    for p in disc.peers().await {
        if p.pubkey == our_pubkey {
            continue;
        }
        match by_key.get(&p.pubkey) {
            Some(kept) if kept.last_seen >= p.last_seen => {}
            _ => {
                by_key.insert(p.pubkey, p);
            }
        }
    }
    let mut list: Vec<LanDevice> = by_key
        .into_values()
        .map(|p| LanDevice {
            id: p.id.map(|d| d.0.to_string()),
            name: p.name,
            addr: p.addr.to_string(),
        })
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(list)
}

// ── ConnectResult ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ConnectResult {
    pub ok: bool,
    pub my_id: u32,
    pub codec: String,
    pub mos: bool,
    /// `"direct"` or `"relay"` — the actual transport used for this session.
    pub transport: String,
    pub detail: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn mime_for(codec: &str) -> &'static str {
    match codec {
        "h265" | "hevc" => "video/hevc",
        "av1" => "video/av01",
        _ => "video/avc",
    }
}

/// Map the UI's network-mode string (Ayarlar) to a `NetworkMode`.
pub(crate) fn parse_mode(s: &str) -> NetworkMode {
    match s {
        "p2p-only" => NetworkMode::P2pOnly,
        "relay-only" => NetworkMode::RelayOnly,
        _ => NetworkMode::Auto,
    }
}

fn codec_enum(codec: &str) -> Codec {
    match codec {
        "h265" | "hevc" => Codec::H265,
        _ => Codec::H264,
    }
}

fn transport_str(t: Transport) -> &'static str {
    match t {
        Transport::Direct => "direct",
        Transport::Relay => "relay",
    }
}

/// Pick the best codec from host caps honoring the caller's preference.
/// Returns the negotiated codec string (`"h264"` / `"h265"` / etc.).
fn pick_codec(caps_codecs: &[String], pref: &str) -> String {
    if pref != "auto" && !pref.is_empty() {
        if caps_codecs.iter().any(|c| c == pref) {
            return pref.to_string();
        }
    }
    caps_codecs.first().cloned().unwrap_or_else(|| "h264".into())
}

fn parse_quality(s: &str) -> QualityPref {
    match s {
        "latency" => QualityPref::Latency,
        "quality" => QualityPref::Quality,
        _ => QualityPref::Balanced,
    }
}

// ── Auth race loop ────────────────────────────────────────────────────────────

/// Run the client-side auth race loop per §2 of the implementation plan.
///
/// 1. Send the initial `Auth(password)` to the host.
/// 2. Poll `recv_host_auth` in a loop:
///    - `Ok` → return `Accepted`.
///    - `Denied` → return `Denied`.
///    - `NeedPassword` → emit `auth-prompt { slot, peer }`, store a oneshot
///      sender in `PwPending`, await `submit_password` (up to 60 s), then
///      re-send `Auth(new_password)` and continue polling.
///    - `Gone` → return `Denied` (session closed during auth).
///    - `Other` → ignore and continue polling.
///
/// This replaces the old single-shot `authenticate()` call.
async fn auth_race<R: Runtime>(
    app: &AppHandle<R>,
    slot: u8,
    sess: &mut pulsar_core::Session,
    initial_password: &str,
    peer: &str,
) -> Result<AuthOutcome, String> {
    // Send the initial auth message (possibly empty if unattended).
    send_auth(sess, initial_password)
        .await
        .map_err(|e| format!("auth send failed: {e:?}"))?;

    loop {
        let verdict = timeout(POST_AUTH_TIMEOUT, recv_host_auth(sess))
            .await
            .map_err(|_| "connect-timed-out".to_string())?;

        match verdict {
            HostAuth::Ok => return Ok(AuthOutcome::Accepted),
            HostAuth::Denied => return Ok(AuthOutcome::Denied),
            HostAuth::Gone => return Ok(AuthOutcome::Denied),
            HostAuth::Other => {
                // Non-auth message (keepalive etc.) — keep waiting.
                continue;
            }
            HostAuth::NeedPassword => {
                // Prompt the user for the password.
                emit_auth_prompt(app, slot, peer);

                // Create a oneshot and register it in the pending map.
                let (tx, rx) = oneshot::channel::<String>();
                app.state::<PwPending>().0.lock().unwrap().insert(slot, tx);
                tokio::pin!(rx);
                let deadline = tokio::time::Instant::now() + PW_SUBMIT_TIMEOUT;

                // Wait for EITHER the user's submitted password OR the host approving
                // without one — the desktop's "Allow" sends `Ok`. Previously we only
                // awaited the password, so a host-side approval was missed and the OTP
                // prompt stayed open forever even though the host had accepted.
                let pw = 'wait: loop {
                    tokio::select! {
                        biased;
                        host = recv_host_auth(sess) => match host {
                            HostAuth::Ok => {
                                // Host approved (Allow). Tell the UI to dismiss the prompt.
                                let _ = app.emit("auth-ok", AuthPromptPayload {
                                    slot, peer: peer.to_string(),
                                });
                                return Ok(AuthOutcome::Accepted);
                            }
                            HostAuth::Denied | HostAuth::Gone => return Ok(AuthOutcome::Denied),
                            // NeedPassword / keepalive while we wait — keep waiting.
                            _ => continue 'wait,
                        },
                        r = tokio::time::timeout_at(deadline, &mut rx) => match r {
                            Ok(Ok(pw)) => break 'wait pw,
                            Ok(Err(_)) => return Err("auth cancelled".into()),
                            Err(_) => return Err("connect-timed-out".into()),
                        },
                    }
                };

                // Re-send the password and loop back to poll.
                send_auth(sess, &pw)
                    .await
                    .map_err(|e| format!("auth send failed: {e:?}"))?;
            }
        }
    }
}

// ── list_remote_games command (game-mode fetch) ───────────────────────────────

/// One game/app the host publishes, serialized to JS for the game-mode picker.
/// Mirrors the desktop `GameInfo` (id / title / kind / cover image).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GameDto {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub image: String,
}

impl From<GameInfo> for GameDto {
    fn from(g: GameInfo) -> Self {
        GameDto { id: g.id, title: g.title, kind: g.kind, image: g.image }
    }
}

/// Game-mode FETCH (desktop parity): connect + authenticate to `target`, ask the
/// host for its published game/app library, and return it — WITHOUT starting a
/// stream. The session is dropped on return; the caller then picks an entry and
/// calls `connect_host` with that `game_id` to actually stream (which launches it).
///
/// Auth runs on the reserved fetch slot ([`FETCH_SLOT`]); if the host needs a
/// password the same `auth-prompt`/`submit_password` OTP flow fires as for a normal
/// connect. The host's OTP is stable within a `go_online`, so the password the user
/// enters here also unlocks the follow-up stream connect (JS reuses it → no second
/// prompt).
///
/// JS: `invoke('list_remote_games', { relay, target, password, netmode, name })`
#[tauri::command]
pub async fn list_remote_games<R: Runtime>(
    app: AppHandle<R>,
    relay: String,
    target: String,
    password: String,
    netmode: String,
    name: String,
    relay_password: Option<String>,
    relay_totp: Option<String>,
) -> Result<Vec<GameDto>, String> {
    // Cancel any stale read loop on the fetch slot before binding.
    if let Some(old_cancel) = app.state::<SessionRegistry>().cancel_for(FETCH_SLOT) {
        old_cancel.notify_one();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let cfg = load_config(&app);
    let relay_str = if relay.is_empty() { cfg.relay.clone() } else { relay };
    let relay_addr: SocketAddr = crate::net::parse_relay(&relay_str).await?;
    let dev_name = if name.is_empty() {
        if cfg.device_name.is_empty() { "Pulsar Mobile".to_string() } else { cfg.device_name.clone() }
    } else {
        name
    };
    let net_mode = if netmode.is_empty() { cfg.network_mode } else { parse_mode(&netmode) };

    let creds = crate::net::relay_creds(&cfg, &relay_str, relay_password, relay_totp);
    let (node, _my_id) =
        crate::net::get_or_create_node(&app, relay_addr, &relay_str, net_mode, dev_name, creds).await?;

    // Connect (relay ID or IP:port), inside the overall connect budget.
    let mut sess = timeout(CONNECT_TIMEOUT, async {
        if let Some(addr) = crate::net::resolve_target(&target).await {
            node.connect_direct(addr, None)
                .await
                .map_err(|e| format!("connect-direct failed: {e:?}"))
        } else {
            let dev_id = DeviceId::parse(&target)
                .ok_or_else(|| format!("bad target (not a device id, IP, or host): {target}"))?;
            node.connect(dev_id).await.map_err(|e| format!("connect failed: {e:?}"))
        }
    })
    .await
    .map_err(|_| "connect-timed-out".to_string())??;

    match auth_race(&app, FETCH_SLOT, &mut sess, &password, &target).await? {
        AuthOutcome::Accepted => {}
        AuthOutcome::Denied => return Err("auth denied (wrong password?)".into()),
        AuthOutcome::NeedPassword => return Err("host requires a password".into()),
    }

    let games = timeout(POST_AUTH_TIMEOUT, request_games(&mut sess))
        .await
        .map_err(|_| "connect-timed-out".to_string())?
        .map_err(|e| format!("request_games failed: {e:?}"))?;

    // Politely close the fetch session (best-effort); the host frees the slot.
    let _ = send_bye(&mut sess).await;
    Ok(games.into_iter().map(GameDto::from).collect())
}

// ── connect_host command ──────────────────────────────────────────────────────

/// Connect to `target` (9-digit relay ID **or** `IP:port` for LAN direct) on
/// `relay`, request the video stream, and start feeding decoded frames to the
/// native surface.
///
/// Falls back to the persisted [`Config`] when `relay`, `name`, and `netmode`
/// are empty strings.  After this command returns the read loop runs in the
/// background; the caller should listen for `play-ended` to know when the
/// session ends.
///
/// JS: `invoke('connect_host', { relay, target, password, slot, netmode, name,
///       mode, codec, fps, bitrateKbps, width, height, quality })`
#[tauri::command]
pub async fn connect_host<R: Runtime>(
    app: AppHandle<R>,
    relay: String,
    target: String,
    password: String,
    slot: u8,
    netmode: String,
    name: String,
    // W2-gamemode additions
    mode: String,          // "remote" | "game"
    codec: String,         // "auto" | "h264" | "h265"
    fps: u32,              // 0 = host default
    bitrate_kbps: u32,     // 0 = host default
    width: u32,            // 0 = host default
    height: u32,           // 0 = host default
    quality: String,       // "latency" | "balanced" | "quality"
    hdr: bool,             // request HDR (HEVC Main10 / PQ) from the host
    // game-mode: host-side id to launch before streaming ("" / "desktop" = whole
    // screen). Option so an older JS caller omitting it still deserializes.
    game_id: Option<String>,
    // v5 relay auth: ONE-SHOT credentials from the connect-time prompt, used for this
    // registration only and never stored — the relay's durable access key (kept in the
    // core config) is what authenticates later connects. Option so an older JS caller
    // omitting them still decodes.
    relay_password: Option<String>,
    relay_totp: Option<String>,
) -> Result<ConnectResult, String> {
    let game_id = game_id.unwrap_or_default();
    // Tear down any still-running read loop on this slot BEFORE binding a new Node.
    // Each connect binds a FRESH Node that registers with the relay under our
    // (identity-derived, shared) device id; if the previous slot's loop is still
    // alive its Node keeps re-registering too, so the relay's device address flaps
    // between the two ports for the whole duration of THIS connect's setup —
    // disrupting auth/caps/keepalive and making the new session die at the host's
    // 6 s PEER_TIMEOUT. Cancelling HERE (not only in `register()`, which runs after
    // setup) lets the old Node drop first. Best-effort settle delay so its relay
    // de-registration lands before the new Node registers. (`register()` keeps its
    // own cancel as a backstop for two connects racing into the same slot.)
    if let Some(old_cancel) = app.state::<SessionRegistry>().cancel_for(slot) {
        old_cancel.notify_one();
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    // ── Config fallback ───────────────────────────────────────────────────────
    let cfg = load_config(&app);

    let relay_str = if relay.is_empty() { cfg.relay.clone() } else { relay };
    let relay_addr: SocketAddr = crate::net::parse_relay(&relay_str).await?;

    let dev_name = if name.is_empty() {
        if cfg.device_name.is_empty() { "Pulsar Mobile".to_string() } else { cfg.device_name.clone() }
    } else {
        name
    };

    let net_mode = if netmode.is_empty() { cfg.network_mode } else { parse_mode(&netmode) };

    let game_mode = mode == "game";

    // Quality: game mode always prefers latency (Moonlight-style).
    let quality_pref = if game_mode {
        QualityPref::Latency
    } else {
        parse_quality(&quality)
    };

    // ── Phase: "reaching" ────────────────────────────────────────────────────
    emit_phase(&app, slot, "reaching", None);

    // ── Shared node + connect (inside the 45-second overall budget) ──────────
    // Dial out on the ONE shared relay Node (see net.rs) — the SAME node go_online
    // accepts incoming on. Binding a fresh per-connect node here would register a
    // SECOND socket under our identity, so the relay's device address would flap
    // and route INBOUND (host) connections to a node with no accept loop → the
    // incoming-request popup never fired. `get_or_create_node` binds+registers once
    // and is reused by both roles.
    let (node, my_id) =
        crate::net::get_or_create_node(
            &app,
            relay_addr,
            &relay_str,
            net_mode,
            dev_name.clone(),
            crate::net::relay_creds(&cfg, &relay_str, relay_password, relay_totp),
        )
        .await?;

    let connect_result = timeout(CONNECT_TIMEOUT, async {
        // ── W2-ipconnect: branch on DeviceId vs SocketAddr ───────────────────
        let sess = if let Some(addr) = crate::net::resolve_target(&target).await {
            // IP / IP:port / host[:port] — connect directly without relay rendezvous.
            node.connect_direct(addr, None)
                .await
                .map_err(|e| format!("connect-direct failed: {e:?}"))?
        } else {
            let dev_id = DeviceId::parse(&target)
                .ok_or_else(|| format!("bad target (not a device id, IP, or host): {target}"))?;
            node.connect(dev_id)
                .await
                .map_err(|e| format!("connect failed: {e:?}"))?
        };
        Ok::<_, String>(sess)
    })
    .await
    .map_err(|_| "connect-timed-out".to_string())?;

    let mut sess = connect_result?;

    // ── Phase: "transport" (real P2P / relay feedback) ───────────────────────
    let transport = sess.transport();
    let transport_s = transport_str(transport);
    emit_phase(&app, slot, "transport", Some(transport_s));

    // ── Phase: "auth" ─────────────────────────────────────────────────────────
    emit_phase(&app, slot, "auth", Some(transport_s));

    // W3: use the auth race loop so we can handle NeedPassword via auth-prompt.
    // The peer string uses the target as the display name.
    let auth_outcome = auth_race(&app, slot, &mut sess, &password, &target).await?;

    match auth_outcome {
        AuthOutcome::Accepted => {}
        AuthOutcome::Denied => return Err("auth denied (wrong password?)".into()),
        AuthOutcome::NeedPassword => {
            // Should not reach here — auth_race only returns NeedPassword if the
            // loop above somehow exits (it loops internally on NeedPassword). Guard
            // for completeness.
            return Err("host requires a password".into());
        }
    }

    // ── Game-mode launch (desktop parity) ─────────────────────────────────────
    // If the game-mode picker chose a specific game/app (not the built-in Desktop
    // entry), ask the host to launch it BEFORE requesting the stream, so the
    // captured screen already shows the game. "" and "desktop" both mean "stream
    // the whole screen" (no launch). A launch failure is non-fatal — fall through
    // to streaming the desktop rather than aborting the whole connect.
    if !game_id.is_empty() && game_id != "desktop" {
        if let Err(e) = request_launch(&mut sess, &game_id).await {
            log::warn!("pulsar: request_launch({game_id}) failed, streaming desktop: {e:?}");
        }
    }

    // ── Query caps + pick codec ───────────────────────────────────────────────
    let caps = query_stream_caps(&mut sess).await.unwrap_or_default();
    let mos = caps.features.iter().any(|f| f == "mos");
    // W5: detect FEAT_NACK support so request_keyframe knows whether MediaNack works.
    let feat_nack = caps.features.iter().any(|f| f == service::media::FEAT_NACK);

    // Emit host displays for the multi-monitor picker (W4-multimonitor).
    if !caps.displays.is_empty() {
        #[derive(Serialize, Clone)]
        #[serde(rename_all = "camelCase")]
        struct HostDisplaysPayload {
            slot: u8,
            displays: Vec<DisplayDto>,
        }
        #[derive(Serialize, Clone)]
        #[serde(rename_all = "camelCase")]
        struct DisplayDto {
            idx: u32,
            name: String,
            width: u32,
            height: u32,
            primary: bool,
        }
        let displays: Vec<DisplayDto> = caps
            .displays
            .iter()
            .map(|d| DisplayDto {
                idx: d.idx,
                name: d.name.clone(),
                width: d.width,
                height: d.height,
                primary: d.primary,
            })
            .collect();
        let _ = app.emit("host-displays", HostDisplaysPayload { slot, displays });
    }

    // Honor the caller's codec preference; fall back to host's best.
    let resolved_codec = pick_codec(&caps.codecs, &codec);
    let mime = mime_for(&resolved_codec);
    let cenum = codec_enum(&resolved_codec);

    // ── Phase: "preparing" ────────────────────────────────────────────────────
    emit_phase(&app, slot, "preparing", Some(transport_s));

    // Claim a new session generation for this slot BEFORE arming the decoder below.
    // A superseded old read loop for this slot (cancelled at the top of this connect)
    // that hasn't finished tearing down yet will see this newer generation at its
    // own teardown and skip its slot-keyed cleanup — so it cannot `stop_stream` /
    // remove the decoder + input sender we arm here in the window before `register()`
    // (further down) installs this loop's `is_owner` token. See the teardown guard.
    let my_gen = bump_slot_gen(slot);

    // Arm the native decoder for this slot before asking the host for frames. The
    // plugin returns "port=N" — a loopback socket we stream raw AUs to (desktop-style),
    // bypassing the per-frame IPC. `None` → fall back to the IPC feed_au path.
    let au_stream = match app.pulsar_video().start_stream(mime, slot) {
        Ok(r) => connect_au_socket(&r.detail).await,
        Err(_) => None,
    };

    // HDR: set the SurfaceView color mode + MediaFormat HDR hints BEFORE the decoder
    // configures on the first AU (PQ/HDR10 is the common host case). No-op when the
    // host streams SDR — the hints only take effect if the bitstream is actually HDR.
    // set_hdr_mode lives on the mobile-only PulsarVideoW5Ext, so gate it to mobile.
    #[cfg(mobile)]
    if hdr {
        use tauri_plugin_pulsar_video::PulsarVideoW5Ext as _;
        let _ = app.pulsar_video().set_hdr_mode(slot, "hdr10");
    }
    #[cfg(not(mobile))]
    let _ = hdr;

    // W3: call setAspect on stream entry.
    // NOTE: `set_aspect` is added to the plugin by the W3-media-native lane; it is
    // not yet present in `mobile.rs` / `desktop.rs`. This call is intentionally
    // left as a no-op comment until that lane lands its changes. When W3-media-native
    // ships, add:
    //   let _ = app.pulsar_video().set_aspect(slot, "fit");
    // (cross-lane assumption: W3-media-native adds `set_aspect(&self, slot: u8, mode: &str)`
    // to both `mobile.rs` and `desktop.rs`.)

    // ── W5: claimed-display map — pick a free display_idx for this slot ──────
    // When two split panes target the same host, each must request a distinct
    // display. `ClaimedDisplays::claim` finds the lowest unclaimed index and
    // atomically reserves it for this slot. If this is the only pane to this
    // host (or it has no competing sibling), it gets idx 0 (the primary display).
    // The caller may still override via `set_play_monitor` after connect.
    let claimed_display = app
        .state::<ClaimedDisplays>()
        .claim(slot, &target);

    // ── W5: per-cell reduced resolution for secondary panes ─────────────────
    // A 2nd (or higher) slot in a split view streams 720p to save bandwidth and
    // decode load on the phone. The 1st slot (slot 0) keeps whatever the caller
    // asked for (or host default if 0×0). This mirrors the desktop
    // `paneResolution` logic: 2-up → 720p, 4-up → 540p.
    //
    // Only applied when the caller did NOT explicitly request a resolution
    // (width == 0 means "host default") — if the JS layer already specified
    // a size (e.g. from the quality card), we honour that instead.
    let (eff_width, eff_height) = if slot > 0 && width == 0 && height == 0 {
        (1280u32, 720u32)
    } else if width == 0 && height == 0 {
        // Mobile default: 1080p, NOT the host's native resolution. A 1440p+ desktop
        // encoded for a phone screen spends the (ABR-limited) bitrate on pixels the
        // phone can't show — at the same kbps, 1080p is visibly cleaner (more bits
        // per pixel). An explicit user pick (quality preset / restream) still wins.
        (1920u32, 1080u32)
    } else {
        (width, height)
    };

    // Auto bitrate: NEVER send 0 to the host — 0 means "host default" (30 Mbit CBR),
    // which blasts the wifi downlink for the first ~1-2 s until the ABR's first
    // request lands (observed live: 20% loss + RTT panic within 2 s of connect →
    // ABR pinned at the floor → blocky picture for the whole session). Open at the
    // ABR's own starting point instead; the auto ramp ceiling stays higher.
    let auto_rate = bitrate_kbps == 0;
    let bitrate_kbps = if auto_rate { 8_000 } else { bitrate_kbps };

    // ── W5: audio routing — transmit audio only on the active pane slot ──────
    // Previously, audio was always transmitted for slot 0 only. Now it follows
    // the `ActivePane` state: the active pane gets audio; all others are muted.
    // On the first connect (usually slot 0), `ActivePane` defaults to 0, so
    // behaviour is unchanged for single-session usage.
    let active_slot = app.state::<ActivePane>().get();
    let transmit_audio = slot == active_slot;

    // fps 0 = "host default" — but the host's default is its own config/panel Hz
    // (120+ on a gaming PC), with NO cap for mobile clients. The phone pipeline is
    // sized for ~60 fps (decode + touch UI share the SoC); 120+ fps just floods the
    // input queue. Cap BOTH modes at 60 on mobile: the JS "auto" fps pref resolves
    // to the panel Hz (120 on this phone), and 120 fps on a software-x264 / relay
    // host starves each frame of bits AND overloads the encoder — with any wire loss
    // the client then never gets a CLEAN keyframe (the host can't force one on the
    // gst-launch path), so it drops every AU waiting for an IDR and the screen stays
    // black. 60 fps halves the per-frame load and keeps frames robust; a 120 Hz
    // gaming feel isn't reachable through a phone decoder + relay anyway.
    let fps = if fps == 0 { 60 } else { fps.min(60) };

    // The shared adaptive controller: the phone's requested size is the ladder's top rung
    // (the host scales its capture down to it); the mobile depacketizer waits for an IDR
    // after a loss, so it asks for the short-GOP recovery (not intra refresh).
    let mut acfg = adapt::Config::new(
        adapt::codec_from_wire(&resolved_codec),
        if auto_rate { AUTO_CAP_KBPS } else { bitrate_kbps },
    );
    acfg.native = (eff_width, eff_height, fps);
    // Start near the peer's last known-good rate (60 % of it), else the auto start.
    let peer_key = target.clone();
    let hint = crate::adapt_memory::hint(&app, &peer_key).map(|k| (k * 3 / 5).max(adapt::FLOOR_KBPS));
    acfg.start_kbps = if auto_rate { hint.unwrap_or(AUTO_START_KBPS).min(AUTO_CAP_KBPS) } else { 0 };
    acfg.ir_capable = false;
    // Size/fps are fixed for the session (settings / screen); only the bitrate adapts.
    acfg.ladder = false;
    let ctl = Controller::new(acfg);
    let start_point = ctl.point();
    let req = StreamReq {
        port: 0, // unused with media-over-session
        codec: resolved_codec.clone(),
        encoder: "auto".into(),
        width: start_point.width,
        height: start_point.height,
        fps: start_point.fps,
        audio_port: if transmit_audio { 1 } else { 0 },
        transmit_audio,
        mute_host: false,
        game_mode,
        bitrate_kbps: ctl.encoder_kbps(),
        quality: quality_pref,
        hdr,
        yuv444: false,
        decode_codecs: vec![resolved_codec.clone()],
        media_over_session: mos,
        cursor_external: false,
        // W5: use the claimed display index (freeDisplayFor logic).
        display_idx: claimed_display,
        window_hwnd: None,
        adapt: None,
        audio_layout: pulsar_core::audio::ChannelLayout::Stereo,
        loss_recovery: LossRecovery::Normal,
        // FEC parity (Phase 2.1): the host sends it only once we report loss — Reed-Solomon
        // (several losses per frame) preferred, XOR for older hosts.
        fec: mos,
        fec_rs: mos,
    };

    timeout(POST_AUTH_TIMEOUT, request_stream(&mut sess, &req))
        .await
        .map_err(|_| "connect-timed-out".to_string())?
        .map_err(|e| format!("request_stream failed: {e:?}"))?;

    let detail = format!(
        "connected; codec={resolved_codec} mos={mos} transport={transport_s} game={game_mode} \
         display_idx={claimed_display} active_audio={transmit_audio}"
    );

    // ── Register cancel + restream handles ───────────────────────────────────
    // Must happen before the read loop spawns so `end_session` can cancel it.
    // W4: register also receives `my_id` (for `reverse_play`) and returns
    // `mic_cancel` (for `mic_stop`). The mic_cancel is stored in `SessionRegistry`
    // and retrieved by `mic_stop`; we don't need it locally here.
    // W5: also passes `target` (for ClaimedDisplays) and `feat_nack`.
    let (cancel_notify, mut restream_rx, _mic_cancel) = app
        .state::<SessionRegistry>()
        .register(slot, my_id.0, target.clone(), feat_nack);

    // Store the initial req so quality patch commands can build from it.
    app.state::<SessionRegistry>().update_last_req(slot, req.clone());
    // Baseline StreamReq for the adaptive-bitrate controller (read loop). Cloned
    // before the spawn; the loop steps its `bitrate_kbps` up/down on measured loss.
    let abr_req0 = req.clone();

    // ── Expose the input sender for this slot ────────────────────────────────
    app.state::<InputSenders>().0.lock().unwrap().insert(slot, sess.sender());

    // ── Push our identity to the host (name + id + avatar) ───────────────────
    // Mirrors desktop `play.rs`: right after the session is up, send our display
    // name, relay id, and identity image over the side-channel so the host's
    // connections list shows the phone (name + avatar) instead of just ip:port.
    // Best-effort on a detached task; honors `avatar_mode` (anonymous = no image).
    // The avatar resolve is a (blocking) plugin/JNI call, hence its own task.
    {
        let sender = sess.sender();
        let app_av = app.clone();
        let name_for_push = dev_name.clone();
        let id_str = format!("{:09}", my_id.0);
        let avatar_mode = load_config(&app).avatar_mode;
        tauri::async_runtime::spawn(async move {
            let _ = send_data_via(&sender, &DataMsg::PeerName(name_for_push)).await;
            let _ = send_data_via(&sender, &DataMsg::PeerId(id_str)).await;
            if avatar_mode != "anonymous" {
                if let Some(png) = crate::datachan::resolve_self_avatar_png(&app_av, &avatar_mode) {
                    let _ = send_data_via(&sender, &DataMsg::Avatar(png)).await;
                }
            }
        });
    }

    let app2 = app.clone();
    let mut transport_owned = transport_s.to_string();
    let codec_owned = resolved_codec.clone();

    tauri::async_runtime::spawn(async move {
        let _node = node; // keep the node (and its recv loop) alive
        let mut au_stream = au_stream; // raw AU socket (None = IPC fallback)
        // Active codec for depacketization AND the keyframe gate (au_is_keyframe). Made
        // mutable so a mid-session codec change (restream branch) re-seats it — the gate
        // must test the NEW codec's NAL types, not the connect-time codec's.
        let mut cenum = cenum;
        let mut depack = Depacketizer::new(cenum);
        // Audio feeder thread: `feed_audio` is a BLOCKING main-thread JNI hop (~ms per
        // call at ~100 opus packets/s). Inline in this read loop it stalled the loop
        // long enough to back up the UDP socket during video bursts — kernel-dropped
        // packets that looked like "wifi loss" (torn frames → IDR churn). The loop now
        // just hands packets to a bounded channel; a full channel drops the packet
        // (a stale-audio glitch beats stalling video).
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
        {
            let app3 = app2.clone();
            std::thread::spawn(move || {
                while let Ok(op) = audio_rx.recv() {
                    let _ = app3.pulsar_video().feed_audio(&op);
                }
            });
        }
        let _ = service::send_keepalive(&mut sess).await;
        let mut last_ka = Instant::now();
        // RTT: timestamp the outstanding keepalive Ping; the matching Pong (is_pong)
        // in the recv path measures the network round-trip for the perf HUD.
        let mut ping_at: Option<Instant> = Some(Instant::now());

        // ── Adaptive bitrate + loss tracking (ported from desktop play/hold.rs) ──
        // Measure video RTP sequence-gap loss; every ~2 s step the host's encode
        // bitrate DOWN on congestion (halve on >15% loss, ×0.7 on >3%) / UP after a
        // clean stretch, and NACK missing seqs for retransmit. Plus a burst-backlog
        // drop in the recv arm (feed only the newest frame + request an IDR) so a
        // queue spike can't make us decode stale frames forever.
        let data_sender = sess.sender();
        let host_nack = feat_nack;
        let mut cur_req = abr_req0;
        let adapt_enabled = cur_req.media_over_session;
        let mut ctl = ctl;
        let mut reorder = crate::rtp::ReorderBuffer::new();
        let mut nack_seqs: Vec<u16> = Vec::new();
        let mut loss_corrupt = false;
        let mut waiting_idr = false;
        let mut total_lost: u64 = 0;
        let mut last_idr_req = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let mut framed: Vec<u8> = Vec::new();
        let mut last_abr = Instant::now();
        let mut rtt_samples: Vec<f32> = Vec::with_capacity(8);
        // FEC parity decoding (Phase 2.1) + per-frame delay sampling (Phase 3).
        let mut fec_dec = service::media::FecDecoder::default();
        let mut fec_rs = service::media::RsFecDecoder::default();
        let mut win_fec_ok: u32 = 0;
        let mut remembered_kbps: u32 = 0;
        let epoch = Instant::now();
        let mono_ms = move || epoch.elapsed().as_secs_f64() * 1000.0;
        let mut frame_ts: Option<u32> = None;
        let mut frame_last_arrival_ms: f64 = 0.0;

        // ── W3: stats tracking ───────────────────────────────────────────────
        let mut frames: u64 = 0;
        let mut aus: u64 = 0;
        let mut audio_pkts: u64 = 0;
        // Visibility for audio decoder trouble on the Kotlin side (feedAudio returns
        // ok=false while its decoder is being rebuilt / is disabled).
        let mut audio_feed_fails: u64 = 0;
        let mut cancelled = false;
        // Host said Bye (kicked us / went offline). Over a relay `recv` never returns
        // `None` when the peer vanishes, so this explicit signal is what ends the session
        // AT ONCE instead of waiting out HOST_SILENCE_TIMEOUT (the "video froze, then it
        // disconnected seconds later" the user saw).
        let mut host_bye = false;

        // Stats window counters (reset each STATS_INTERVAL).
        let mut stats_window_frames: u64 = 0;
        let mut stats_window_bytes: u64 = 0;
        let mut last_stats = Instant::now();

        // Firstframe flag — emitted once on the first decoded AU.
        let mut first_frame_emitted = false;

        // Stall tracking.
        let mut last_video_pkt = Instant::now();
        let mut stalled = false;

        // Host-liveness watchdog. A relay session's `recv` never returns `None` when
        // the host vanishes — it only 400 ms-times-out — so without this the loop runs
        // forever as a zombie, keeping its `Node` registered and flapping the relay's
        // device address against any later reconnect (the reconnect then dies at the
        // host's 6 s PEER_TIMEOUT). ANY inbound frame (video/audio/side-channel, and
        // the Pong the host replies to every keepalive) refreshes this, so a live host
        // keeps it fresh even on a static screen. Silence past the host's own
        // PEER_TIMEOUT (6 s) means the host is gone → end the session.
        let mut last_inbound = Instant::now();
        const HOST_SILENCE_TIMEOUT: Duration = Duration::from_secs(8);

        'read: loop {
            tokio::select! {
                biased;

                // ── Cancel path: end_session was called ──────────────────────
                _ = cancel_notify.notified() => {
                    cancelled = true;
                    break 'read;
                }

                // ── Restream path: quality/codec change requested (W3-quality)
                Some(new_req) = restream_rx.recv() => {
                    // Re-arm the native decoder with the new mime type.
                    let new_mime = mime_for(&new_req.codec);
                    let new_cenum = codec_enum(&new_req.codec);

                    // Stop the current decoder slot then start with new params.
                    // stop_stream closes the old AU socket; reconnect to the new one.
                    let _ = app2.pulsar_video().stop_stream(slot);
                    au_stream = match app2.pulsar_video().start_stream(new_mime, slot) {
                        Ok(r) => connect_au_socket(&r.detail).await,
                        Err(_) => None,
                    };

                    // Re-request the stream from the host.
                    if let Err(e) = request_stream(&mut sess, &new_req).await {
                        log::warn!("pulsar: restream request_stream failed: {e:?}");
                    } else {
                        // Reset depacketizer for potentially new codec.
                        depack = Depacketizer::new(new_cenum);
                        // Re-seat the active codec so the keyframe gate (au_is_keyframe)
                        // tests the NEW codec's IDR NAL types after a codec switch
                        // (e.g. h264→h265) — otherwise HEVC keyframes never satisfy the
                        // stale H.264 predicate and every AU is dropped forever.
                        cenum = new_cenum;
                        // Start the new stream clean: the host restarts its GOP with a
                        // fresh IDR, so clear any pending wait-for-keyframe / torn-AU
                        // state left over from the old codec (matches a fresh connect).
                        waiting_idr = false;
                        loss_corrupt = false;
                        // The host restarts its RTP sequence base on a restream → reset the
                        // reorder buffer so the discontinuity isn't counted as loss.
                        reorder.reset();
                        nack_seqs.clear();

                        // Re-seat the adaptive-bitrate baseline to the user's new pick
                        // so ABR ramps relative to it (and doesn't fight a manual change).
                        cur_req = new_req.clone();
                        // A user pick is a new top rung + cap: rebuild the controller around it.
                        let mut acfg = adapt::Config::new(
                            adapt::codec_from_wire(&new_req.codec),
                            if new_req.bitrate_kbps > 0 { new_req.bitrate_kbps } else { AUTO_CAP_KBPS },
                        );
                        acfg.native = (new_req.width, new_req.height, new_req.fps);
                        acfg.ir_capable = false;
                        acfg.ladder = false;
                        ctl = Controller::new(acfg);
                        fec_dec.reset();
                        fec_rs.reset();
                        frame_ts = None;
                        last_abr = Instant::now();
                        let new_codec = new_req.codec.clone();
                        app2.state::<SessionRegistry>().update_last_req(slot, new_req);
                        log::info!("pulsar: restream applied slot={slot} codec={new_codec}");
                    }
                }

                // ── Normal recv path ─────────────────────────────────────────
                result = tokio::time::timeout(
                    Duration::from_millis(400),
                    sess.recv(),
                ) => {
                    // B13: read the clock ONCE per recv-arm iteration and thread it
                    // through the batch + maintenance below (was sprinkled per packet).
                    let now = Instant::now();
                    match result {
                        Ok(Some(first)) => {
                            last_inbound = now;
                            // Drain already-queued datagrams too so a burst can't build a FIFO
                            // backlog we'd decode stale forever. Every packet is processed (loss
                            // tracking / audio / side-channel); only the NEWEST video frame is fed,
                            // older queued frames dropped + an IDR requested so decode resyncs.
                            let mut batch: Vec<Vec<u8>> = Vec::with_capacity(8);
                            batch.push(first);
                            while batch.len() < 512 {
                                match sess.try_recv() { Some(m) => batch.push(m), None => break }
                            }
                            for bytes in &batch {
                                let byte_len = bytes.len() as u64;
                                if let Some((tag, rtp)) = service::media::parse(bytes) {
                                    if tag == service::media::TAG_FEC_RS {
                                        // Reed-Solomon parity (Phase 2.1): rebuild the frame's
                                        // missing packets with zero round-trips and feed them like
                                        // late arrivals; the parity share is deducted from the
                                        // encoder rate.
                                        for (seq, pkt) in fec_rs.on_parity(rtp) {
                                            reorder.push(seq, &pkt, &mut nack_seqs);
                                            win_fec_ok += 1;
                                        }
                                        let r = fec_rs.last_ratio();
                                        if (r - ctl.fec_overhead()).abs() > 1e-3 {
                                            ctl.set_fec_overhead(r);
                                        }
                                    } else if tag == service::media::TAG_FEC {
                                        // XOR parity (older hosts): one loss per group.
                                        if let Some((seq, pkt)) = fec_dec.on_parity(rtp) {
                                            reorder.push(seq, &pkt, &mut nack_seqs);
                                            win_fec_ok += 1;
                                        }
                                        let n = fec_dec.last_n();
                                        let r = if n == 0 { 0.0 } else { 1.0 / n as f32 };
                                        if (r - ctl.fec_overhead()).abs() > 1e-3 {
                                            ctl.set_fec_overhead(r);
                                        }
                                    } else if tag == service::media::TAG_VIDEO {
                                        frames += 1;
                                        stats_window_bytes += byte_len;
                                        last_video_pkt = now;
                                        if stalled { stalled = false; emit_play_stall(&app2, slot, false); }
                                        // Reorder buffer (re-enabled — this link REORDERS heavily).
                                        // Measured live: "late" backward arrivals EXCEEDED counted
                                        // gaps, i.e. the "lost" packets were arriving a few ms late
                                        // and every one was a fake gap → torn frames, IDR churn,
                                        // ABR crash. Packets are released in seq order after a
                                        // short RTT-sized hold; NACK retransmits usually land
                                        // WITHIN the hold window, so a real single-packet loss
                                        // heals with no gap at all. The buffer owns NACK generation
                                        // (once per hole) and the ABR's true-loss window. Draining
                                        // happens after the batch loop (and on idle ticks, so a
                                        // blocked head can't stall past its hold window).
                                        if let Some(seq) = service::media::rtp_seq(rtp) {
                                            fec_dec.on_packet(seq, rtp);
                                            fec_rs.on_packet(seq, rtp);
                                            if rtp.len() >= 8 {
                                                let ts = u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]);
                                                let now_ms = mono_ms();
                                                if frame_ts != Some(ts) {
                                                    if let Some(prev) = frame_ts {
                                                        ctl.on_frame(prev, frame_last_arrival_ms);
                                                    }
                                                    frame_ts = Some(ts);
                                                }
                                                frame_last_arrival_ms = now_ms;
                                            }
                                            reorder.push(seq, rtp, &mut nack_seqs);
                                        }
                                    } else if tag == service::media::TAG_AUDIO {
                                        audio_pkts += 1;
                                        let is_active = app2.state::<ActivePane>().get() == slot;
                                        if is_active {
                                            if let Some(op) = crate::rtp::rtp_payload(rtp) {
                                                // Hand off to the audio feeder thread — NEVER block
                                                // the read loop on the main-thread plugin invoke.
                                                if audio_tx.try_send(op.to_vec()).is_err() {
                                                    audio_feed_fails += 1;
                                                    if audio_feed_fails == 1 || audio_feed_fails % 500 == 0 {
                                                        log::warn!("pulsar: audio feeder backlogged ({audio_feed_fails} drops)");
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else if service::is_pong(bytes) {
                                    // Keepalive Pong → network round-trip latency. Feeds BOTH the
                                    // HUD and the delay-aware ABR (bufferbloat shows here as RTT
                                    // climbing over the baseline, with ~zero packet loss).
                                    if let Some(t0) = ping_at.take() {
                                        let sample = t0.elapsed().as_secs_f64() * 1000.0;
                                        rtt_samples.push(sample as f32);
                                        reorder.set_rtt_ms(sample);
                                        emit_play_rtt(&app2, slot, sample);
                                    }
                                } else if service::is_bye(bytes) {
                                    // Host kicked us / went offline — tear down immediately
                                    // (handled right after the batch so the loop's Node drops
                                    // now, not after the silence watchdog).
                                    host_bye = true;
                                } else {
                                    crate::datachan::route(&app2, slot, bytes);
                                }
                            }
                            if host_bye {
                                log::info!("pulsar: host sent Bye (slot={slot}) — ending session now");
                                break 'read;
                            }
                            // A23: ONE coalesced retransmit NACK for every hole found
                            // across the whole batch (was one send per gap).
                            if host_nack {
                                // Seq 0 is the host's keyframe-request sentinel
                                // (`MediaNack([0])`, sent deliberately below on real loss
                                // / decoder-failure). It must NEVER ride in a retransmit
                                // list, or a genuine loss/reorder on the ~1/65536 packet
                                // whose 16-bit RTP seq wrapped to 0 would double as a
                                // spurious full-IDR request. Drop it — the reorder buffer
                                // already flags that hole as a declared gap, so the
                                // drop-until-keyframe path pulls an IDR in when it's truly
                                // lost; only the single-packet retransmit is skipped.
                                nack_seqs.retain(|&s| s != 0);
                                if !nack_seqs.is_empty() {
                                    let _ = send_data_via(
                                        &data_sender,
                                        &DataMsg::MediaNack(std::mem::take(&mut nack_seqs)),
                                    )
                                    .await;
                                }
                            }
                            nack_seqs.clear(); // no FEAT_NACK → don't accumulate forever
                        }
                        Ok(None) => {
                            // Session closed by the peer.
                            log::info!(
                                "pulsar: session closed (rtp_frames={frames} aus={aus} audio={audio_pkts})"
                            );
                            break 'read;
                        }
                        Err(_) => {
                            // recv timeout — drain + keepalive tick + stall check below.
                        }
                    }

                    // ── Drain reorder buffer → depacketize → gate → feed ─────────
                    // Runs EVERY loop iteration (arrivals AND the 400 ms recv timeout)
                    // so a blocked head-of-line hole is flushed once its RTT-sized
                    // hold window expires even when no new packets arrive.
                    while let Some(pkt) = reorder.pop_ready(Instant::now()) {
                        if reorder.take_gap_declared() {
                            // A real (post-hold) gap: whatever AU it lands in is torn.
                            loss_corrupt = true;
                        }
                        if let Some(au) = depack.push(&pkt) {
                            aus += 1;
                            // Drop-until-keyframe after loss: never feed a torn AU or a
                            // P-frame whose reference is gone — the decoder would paint
                            // corruption until the next IDR (safety GOP: 10 s). Freeze on
                            // the last good picture and pull an IDR in instead.
                            //
                            // ORDER MATTERS: the AU the gap landed in is torn even when it
                            // contains SPS/IDR NALs — a torn IDR passed the keyframe check
                            // and got fed ("half the screen is corrupt"). A gap-flagged AU
                            // is NEVER fed; only a CLEAN keyframe clears the gate.
                            let torn = loss_corrupt;
                            loss_corrupt = false;
                            let feed = if torn {
                                waiting_idr = true;
                                false
                            } else if waiting_idr {
                                if crate::rtp::au_is_keyframe(cenum, &au) {
                                    waiting_idr = false;
                                    true
                                } else {
                                    false
                                }
                            } else {
                                true
                            };
                            if !feed {
                                if host_nack && last_idr_req.elapsed() >= Duration::from_millis(400) {
                                    last_idr_req = Instant::now();
                                    log::info!("pulsar: loss detected slot={slot} — requesting IDR");
                                    let _ = send_data_via(&data_sender, &DataMsg::MediaNack(vec![0])).await;
                                }
                                depack.recycle_au(au);
                            } else {
                                stats_window_frames += 1;
                                let mut sent = false;
                                if let Some(stream) = au_stream.as_mut() {
                                    framed.clear();
                                    framed.extend_from_slice(&(au.len() as u32).to_be_bytes());
                                    framed.extend_from_slice(&au);
                                    sent = stream.write_all(&framed).await.is_ok();
                                }
                                if !sent {
                                    au_stream = None;
                                    let _ = app2.pulsar_video().feed_au(&au, slot);
                                }
                                if !first_frame_emitted {
                                    first_frame_emitted = true;
                                    emit_play_firstframe(&app2, slot);
                                }
                                depack.recycle_au(au);
                            }
                        }
                        reorder.recycle(pkt);
                    }

                    // ── Stats emission every STATS_INTERVAL ──────────────────
                    let stats_elapsed = now.duration_since(last_stats);
                    if stats_elapsed >= STATS_INTERVAL {
                        let secs = stats_elapsed.as_secs_f32();
                        let fps_val = stats_window_frames as f32 / secs;
                        // bytes → megabits/s
                        let mbps_val = (stats_window_bytes as f32 * 8.0) / (secs * 1_000_000.0);
                        // The connect-time transport snapshot goes stale when a late
                        // hole-punch ack upgrades the session relay→direct mid-stream.
                        // Poll the live value once per stats tick (one mutex lock/s) so
                        // the HUD badge, bar meta and session cards track reality.
                        let live = transport_str(sess.live_transport().await);
                        if live != transport_owned {
                            log::info!("pulsar: transport changed {transport_owned} -> {live} slot={slot}");
                            transport_owned = live.to_string();
                            // NOT phase "transport": connecting.js maps that to a handshake
                            // step and would drag a still-open overlay's indicator backwards.
                            emit_phase(&app2, slot, "transport-changed", Some(live));
                        }
                        emit_play_stats(&app2, slot, fps_val, mbps_val, &transport_owned);

                        stats_window_frames = 0;
                        stats_window_bytes = 0;
                        last_stats = now;
                    }

                    // ── Stall detection ───────────────────────────────────────
                    if !stalled && now.saturating_duration_since(last_video_pkt) >= STALL_THRESHOLD {
                        stalled = true;
                        emit_play_stall(&app2, slot, true);
                    }

                    // ── FAST congestion reflex (sub-tick) ─────────────────────
                    // The AP wifi-downlink queue bloats in <1 s under motion; the 1 s AIMD
                    // tick reacts only after the spike is already felt. This runs every loop
                    // iteration off the freshest keepalive RTT (~350 ms cadence) and cuts the
                    // bitrate HARD the instant RTT jumps over baseline — straight to the floor
                    // on a big spike — so we stop feeding the queue before it deepens.
                    // Fast reflex (Phase 3): a queue that keeps filling cuts the rate between
                    // windows — the controller's delay-gradient estimator decides.
                    if adapt_enabled {
                        if let Some(d) = ctl.poll_fast(mono_ms()) {
                            let mut changed = false;
                            if let Some(kbps) = d.bitrate {
                                cur_req.bitrate_kbps = kbps;
                                changed = true;
                            }
                            if let Some(p) = d.point {
                                cur_req.width = p.width;
                                cur_req.height = p.height;
                                cur_req.fps = p.fps;
                                changed = true;
                            }
                            if changed {
                                log::info!(
                                    "pulsar: ABR fast slot={slot} {} → {} kbps {} ({})",
                                    d.reason, cur_req.bitrate_kbps, ctl.point().label(), ctl.target_kbps()
                                );
                                if request_stream(&mut sess, &cur_req).await.is_err() {
                                    break 'read;
                                }
                                app2.state::<SessionRegistry>().update_last_req(slot, cur_req.clone());
                            }
                        }
                    }

                    if adapt_enabled && now.saturating_duration_since(last_abr) >= ABR_TICK {
                        last_abr = now;
                        let (recv, lost) = reorder.take_loss_window();
                        total_lost += lost as u64;
                        let sample = adapt::Sample {
                            recv,
                            lost,
                            rtt_ms: std::mem::take(&mut rtt_samples),
                            nack_sent: 0,
                            nack_ok: 0,
                            fec_ok: win_fec_ok,
                        };
                        win_fec_ok = 0;
                        let d = ctl.tick(&sample);
                        let sig = ctl.last();
                        let mut changed = false;
                        if let Some(r) = d.recovery {
                            cur_req.loss_recovery = r;
                            changed = true;
                        }
                        if let Some(kbps) = d.bitrate {
                            cur_req.bitrate_kbps = kbps;
                            changed = true;
                        }
                        if let Some(p) = d.point {
                            cur_req.width = p.width;
                            cur_req.height = p.height;
                            cur_req.fps = p.fps;
                            changed = true;
                        }
                        if d.is_change() {
                            log::info!(
                                "pulsar: ABR slot={slot} loss={:.1}% rtt={:.0}ms(+{:.0}) delay={:?} target={} kbps={} point={} ceiling={} fec={:.0}% recovery={:?}: {}",
                                sig.loss * 100.0, sig.rtt_ms, sig.excess_ms, sig.delay, ctl.target_kbps(),
                                cur_req.bitrate_kbps, ctl.point().label(), ctl.ceiling(), ctl.fec_overhead() * 100.0,
                                cur_req.loss_recovery, d.reason
                            );
                        }
                        // Per-peer memory: the rate that stayed clean 30 s seeds the next session.
                        let good = ctl.last_good_kbps();
                        if good > 0 && good != remembered_kbps {
                            remembered_kbps = good;
                            crate::adapt_memory::remember(&app2, &peer_key, good);
                        }
                        // Client → host feedback (the host sizes its FEC parity from the loss).
                        let stats = ClientStats {
                            v: ClientStats::VERSION,
                            loss: sig.loss,
                            rtt_ms: sig.rtt_ms,
                            jitter_ms: sig.jitter_ms,
                            kbps: cur_req.bitrate_kbps,
                            recovery: cur_req.loss_recovery,
                            nack_sent: 0,
                            nack_ok: 0,
                            fec_ok: sample.fec_ok,
                            point: ctl.point().label(),
                        };
                        let _ = send_data_via(&data_sender, &DataMsg::Stats(stats.to_json())).await;
                        if changed {
                            if request_stream(&mut sess, &cur_req).await.is_err() {
                                break 'read;
                            }
                            app2.state::<SessionRegistry>().update_last_req(slot, cur_req.clone());
                        }
                    }

                    if now.saturating_duration_since(last_ka) >= Duration::from_millis(350) {
                        let _ = service::send_keepalive(&mut sess).await;
                        last_ka = now;
                        ping_at = Some(now);
                        log::info!(
                            "pulsar: keepalive slot={slot} rtp={frames} aus={aus} audio={audio_pkts} \
                             lost={total_lost} transport={transport_owned} codec={codec_owned}"
                        );
                        // Decoder-failure recovery: the Kotlin decode path sets a failed flag
                        // when it threw and rebuilt the codec (its trigger() events never reach
                        // JS, so we PULL here). The fresh decoder reconfigures from retained
                        // parameter sets but needs an IDR to resume — nudge the host now
                        // (MediaNack([0]) keyframe sentinel) instead of freezing until the
                        // 10 s safety GOP.
                        if host_nack && app2.pulsar_video().decoder_failed(slot).unwrap_or(false) {
                            log::warn!("pulsar: decoder failed slot={slot} — requesting IDR");
                            let _ = send_data_via(&data_sender, &DataMsg::MediaNack(vec![0])).await;
                        }
                    }

                    // Host-gone watchdog: no inbound (not even a keepalive Pong) for
                    // longer than the host's PEER_TIMEOUT means the session is dead.
                    // Break so this loop's `Node` drops instead of zombie-registering.
                    if now.saturating_duration_since(last_inbound) >= HOST_SILENCE_TIMEOUT {
                        log::warn!(
                            "pulsar: host silent >{}s (slot={slot}) — ending dead session",
                            HOST_SILENCE_TIMEOUT.as_secs()
                        );
                        break 'read;
                    }
                }
            }
        }

        // ── Graceful teardown ─────────────────────────────────────────────────
        let reason = if cancelled { "cancelled" } else { "closed" };
        if cancelled {
            // Send `Bye` so the host tears down immediately (not after 6s).
            let _ = send_bye(&mut sess).await;
        }
        // Ownership guard: if a reconnect already re-registered this slot under a NEW
        // read loop, THIS loop was superseded (the registry entry now holds a different
        // cancel token). Skip ALL slot-keyed cleanup so we don't tear down the LIVE
        // session that replaced us — removing its registry entry, stopping its decoder,
        // resetting its active pane, or emitting a spurious play-ended would clobber it.
        // Our own `sess`/`_node` still drop when this task ends, which is what stops the
        // relay re-registration that caused the address flap in the first place.
        //
        // The generation check covers the earlier window too: a reconnect bumps the
        // slot generation before it arms its decoder, well before its `register()`
        // swaps the cancel token — so `is_owner` alone would still say true and let a
        // slow-tearing-down old loop clobber the new decoder. A mismatch here means a
        // newer connect owns this slot, so we skip cleanup.
        let owns = app2
            .state::<SessionRegistry>()
            .is_owner(slot, &cancel_notify)
            && current_slot_gen(slot) == my_gen;
        if owns {
            // Clear any pending password oneshot so the UI doesn't hang.
            app2.state::<PwPending>().0.lock().unwrap().remove(&slot);

            let _ = app2.pulsar_video().stop_stream(slot);
            app2.state::<InputSenders>().0.lock().unwrap().remove(&slot);
            app2.state::<SessionRegistry>().remove(slot);
            // W5: release the claimed display so a future reconnect can reclaim it.
            app2.state::<ClaimedDisplays>().release(slot);
            // W5: if the ending slot was the active pane, reset to slot 0 so audio
            // does not stay orphaned.
            {
                let active = app2.state::<ActivePane>();
                if active.get() == slot {
                    active.set(0);
                    let _ = app2.emit(
                        "active-pane-changed",
                        crate::session_cmds::ActivePaneChangedPayload { slot: 0 },
                    );
                }
            }
            emit_play_ended(&app2, slot, reason);
        } else {
            log::info!(
                "pulsar: superseded read loop slot={slot} exited (a reconnect owns it) — skipping slot cleanup"
            );
        }
    });

    Ok(ConnectResult {
        ok: true,
        my_id: my_id.0,
        codec: resolved_codec,
        mos,
        transport: transport_s.to_string(),
        detail,
    })
}
