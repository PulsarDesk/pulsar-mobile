# Layout — where things live

Part of the pulsar-mobile agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Layout

- `mobile/` — the Tauri app crate (`pulsar-mobile`). `src/`: client.rs (2k LOC),
  host.rs, net.rs, rtp.rs, datachan.rs, session_cmds.rs, relay_cmds.rs
  (on-device local relay: `start/stop/local_relay_status`), config, identity.
  decode_caps.rs: connect-time codec + fps choice from the phone's decoders
  (hardware-only auto codec with AV1 last, `decode_codecs` pinned to the armed codec,
  decoder fps cap, refresh vote).
  pad_feed.rs: physical gamepads — Kotlin `PadInput` sends pad state over a loopback
  UDP socket (token-prefixed datagrams), forwarded to the active pane's host as
  `GamepadSlot`; neutral state on focus loss / pane change / session end; rumble back.
  Desktop-parity pad controls live there too: Select+L1+R1+X → `pad-overlay-toggle`,
  Start+Select+L1+R1 → `pad-quit` (combo buttons latched off the host until released);
  while the overlay is open (`set_pad_overlay_open`) the host gets a neutral pad and the
  pad drives the menu via `pad-nav` → `gamepad-nav.js` `padNav` (buttons still held when it
  closes stay off the host until released; the quit combo ends the ACTIVE pane in a split). The on-screen touch
  controller is `ui/js/session/vpad.js` (game mode, `send_gamepad` at host pad slot 3).
  Client streams run immersive (system bars hidden, swipe = transient) — plugin
  `setImmersive` in `startStreamService` / `stopStreamService`.
- `mobile/ui/` — **static** webview UI, plain ES modules, no bundler
  (`frontendDist: "ui"`). i18n: `ui/js/i18n/{en,tr,ru,kk}.js`.
- Client streaming loop (`client.rs` read loop): loss repair and ABR plumbing are the
  desktop client's, shared via pulsar-core (2026-11-15) — `adapt::repair::RepairTracker`
  (reorder / FEC waits before a NACK, one re-NACK, write-off after the RTT-derived window;
  window counters incl. goodput; FEC share released when parity stops), `KeyframePacer`
  (IDR requests back off in a loss streak, capped at 1 s here: `KF_GAP_MAX_FROZEN`, the
  depacketizer freezes until the keyframe), `pong::PongMatcher`, `caps::mobile_auto_cap_kbps`
  (≤ 50 Mbit; was a flat 12) + `path_cap_kbps` (relay cap while relayed, `path_changed` on a
  relay ↔ direct switch), `memory` keyed by peer × path class. `rtp.rs` `ReorderBuffer` only
  orders; its wait is the repair window + one sweep (≤ 250 ms), bounded by time (the 4096-packet
  cap is a memory guard; 128 flushed every hole at 30-50 Mbit before its NACK could help). The
  Kotlin decoder reports dropped AUs (`needIdr` → IDR request) and its counters
  (`decoder_status`: decoded / dropped / stalled): a decoder that puts out < 85 % of what it is
  fed, or finds no input buffer for ≥ 5 %, 3 windows running (a window with a network burst
  is skipped), steps the fps down ONE rung (`Controller::client_decoder_sheds`, retried upward after 5 min, doubling; the UI
  shows `play-decode-limit`). Backlog drops alone are no verdict: the reorder buffer's release
  of a held hole overflows the 16-AU backlog on any decoder. `last_req` keeps the session's fps,
  not the decoder-forced one.
  Still off on mobile: RFI and intra refresh (`rfi: false`, `ir_capable = false`) — they need
  the depacketizer to resume on a recovery frame and per-decoder MediaCodec verification.
- `crates/tauri-plugin-pulsar-video/` — the native surface plugin.
  `PulsarVideoPlugin.kt` (~2.8k lines) is the center of gravity.
  `DecoderProbe.kt`: hardware-decoder detection + Moonlight's vendor gates;
  `PadInput.kt`: native gamepad reader (Window.Callback proxy, in-stream only).
- Deps: `pulsar-core` + `pulsar-relay` as **git deps**.

## Keep-awake (Android, `PulsarVideoPlugin.kt`)

Untested on a device. `WAKE_LOCK` is declared in the plugin manifest; before that, every
WakeLock / WifiLock acquire threw, so these locks are new on every device:

- **Client** (`acquirePerfLocks`, from attach to detach): `FLAG_KEEP_SCREEN_ON` (screen never
  times out during a stream), sustained-performance mode, and a Wi-Fi lock
  (`WIFI_MODE_FULL_LOW_LATENCY` on Android 10+, `FULL_HIGH_PERF` before) — the Wi-Fi lock is
  newly active. No PARTIAL wake lock: with the screen turned off by the power button it would
  keep the stream running with no limit.
- **Host** (`startHost` .. `stopHost`): a `SCREEN_DIM_WAKE_LOCK`, so the shared screen does not
  time out and lock mid-session (the app is in the background, the window flag does not
  apply). The power button still turns it off. Start/stop are ordered by a host epoch
  (`tauri_plugin_pulsar_video::host_epoch`): a start older than the last stop — a late
  re-stream request, or a permission dialog answered after the client left — is skipped, so
  the lock cannot outlive its session.
- **Gap vs the desktop client** (`pulsar-desktop/src-tauri/src/keep_awake.rs`,
  `power_watch.rs`): the Android client has no input-idle expiry (the desktop drops its
  display hold 30 min after the last input) and does not end a stream when its screen turns
  off or the phone locks (FGS + audio playback keep it running). A phone left connected on a
  stand therefore keeps itself and the HOST awake (the host holds its display + system awake
  while any stream runs) until the session ends some other way. Follow-up: a mobile
  power-watch (end on `ACTION_SCREEN_OFF`, reconnect on unlock) plus the desktop's idle expiry
  for `FLAG_KEEP_SCREEN_ON`.
