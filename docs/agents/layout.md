# Layout — where things live

Part of the pulsar-mobile agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Layout

- `mobile/` — the Tauri app crate (`pulsar-mobile`). `src/`: client.rs (2k LOC),
  host.rs, net.rs, rtp.rs, datachan.rs, session_cmds.rs, relay_cmds.rs
  (on-device local relay: `start/stop/local_relay_status`), config, identity.
  decode_caps.rs: connect-time codec + fps choice from the phone's decoders
  (hardware-only auto codec with AV1 last, `decode_codecs` pinned to the armed codec,
  decoder fps cap, refresh vote).
  clipsync.rs: two-way clipboard sync (desktop `clipboard_sync.rs` port, `clipboardSync` pref,
  on by default) for viewed AND hosted sessions. Peer → phone is written at once (background
  writes are allowed); phone → peer comes from a 500 ms poll of the plugin's `clipboardPoll`,
  which reads only while Pulsar's window is focused (Android 10+), so a copy in another app goes
  out when the user returns to Pulsar. Baseline uses the clip timestamp (no Android 12+ "pasted"
  toast on connect). Untested on a device.
  call.rs: voice calls (protocol: pulsar-core `call`, same as desktop), as client (key = slot)
  and host (key = sid). Ringing, the peer's voice, an "incoming call" notification and a
  `microphone` foreground service while a call is up are the plugin's `CallAudio.kt` /
  `CallService.kt` (`ringStart` built-in tone or the phone ringtone, `voiceFeed` on a
  VOICE_COMMUNICATION track so host playback capture never echoes it back); this phone's mic is
  call.rs's own pump over the plugin AudioRecord. UI: `ui/js/call.js` (call card on every
  screen, the overlay's Voice call card, `callFor` for the host screen's peer rows, ringtone
  pref → `set_ringtone`). The microphone service starts when WE ring (Pulsar is on screen
  then), so the mic works if the peer answers after the user left Pulsar; answering happens
  in-app (the notification opens it). Calling is offered only to a peer that said `Hello`.
  Echo: Android's own canceller (VOICE_COMMUNICATION mic + voice-call track).
  Untested on a device (2026-11-17).
  Received files (datachan.rs `FileEnd`): reassembled in private storage, then the plugin's
  `ReceivedFiles.kt` copies them to public Downloads/Pulsar Received (MediaStore on 10+, the
  public dir with WRITE_EXTERNAL_STORAGE ≤ 9) and posts a notification with Open / Share; the
  session's files card shows the same actions (`open_received` / `share_received`).
  trust.rs: the desktop's trust files, same names + JSON shapes — `known_peers.json` (TOFU
  pin of each relay id's host key; `connect_pinned` refuses a changed key with
  `IDENTITY_CHANGED:<id>`, only the user's "trust the new key" → `forget_peer` clears it;
  UI: `ui/js/trust-guard.js`), `paired_devices.json` (clients this phone trusts as host: no
  OTP, no approval sheet; pairing asks on the approval sheet unless the session proved a
  password; Settings → Paired devices lists / revokes) and `device_creds.json` (client: the
  host accepted our `pair_device`; Devices → ⋮ → Pair / Unpair).
  pad_feed.rs: physical gamepads — Kotlin `PadInput` sends pad state over a loopback
  UDP socket (token-prefixed datagrams), forwarded to the active pane's host as
  `GamepadSlot`; neutral state on focus loss / pane change / session end; rumble back.
  ds5.rs + ds5_usb.rs (Android) + plugin `Ds5Usb.kt`: DualSense raw passthrough (desktop
  `ds5/client.rs` parity) for hosts advertising `FEAT_DS5` — USB only: Kotlin claims the HID
  interface (USB permission dialog), Rust does report I/O on the usbfs fd (`Ds5Input` with
  gyro/touchpad, `Ds5Caps`/`Ds5Feature`, `Ds5Output` → triggers/lightbar/LEDs/rumble, HD
  haptics → classic rumble) and hands the pad back to `hid-playstation` (`USBDEVFS_CONNECT`)
  when no FEAT_DS5 session is left. Bluetooth stays a plain `GamepadSlot` (no raw HID for
  apps on stock Android); no pad speaker/mic. Untested on a device.
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
  is skipped), steps to a lower-fps rung and caps the ladder there (`decoder_sheds_step` →
  `Controller::client_decode_bound` fed the frame budget, since MediaCodec reports no decode
  time; retried upward later, doubling; the UI shows `play-decode-limit`). Backlog drops alone
  are no verdict: the reorder buffer's release of a held hole overflows the 16-AU backlog on any
  decoder. `last_req` keeps the session's size + fps (the ladder's top), not the current rung.
  Resolution/fps ladder ON (`apply_ladder`, desktop play.rs parity): top rung = requested size at
  the decoder-capped fps (`decode_caps::effective_fps`), so no rung exceeds the decoder; floors
  remote 720p (desktop 1080p) / game 720p60, last resort 720p30; a session-menu restream rebuilds
  the ladder and asks the host for its start rung. Auto start = probe, else per-path memory, else
  the controller's cautious no-memory start (was a fixed 8 Mbit). `acfg.mobile` (gentler start +
  climb) from `mobile_link.rs` → plugin `networkKind` (ConnectivityManager: cellular, or metered
  non-VPN Wi-Fi = a phone hotspot), once per session start like the desktop (no mid-session
  re-detect there either).
  RFI (reference-frame invalidation, desktop parity, untested on a device): asked for when the
  host advertises `FEAT_RFI` (H.264 / HEVC, not AV1). `rtp.rs` `ReorderBuffer` reports a hole
  early (`poll_rfi`: 3 ms, longer on a reordering path; at once when given up on) → the read loop
  sends `DataMsg::MediaRfi` while NACK repair still runs; a held recovery-tag packet (the host's
  `PULSAR-RFI-RECOV` SEI) releases past a reported hole at once; `RfiGate` drops AUs until a whole
  recovery frame or keyframe, and after `rfi_timeout` (150 ms, 1.5 RTT + 50 ms, ≤ 1 s) asks for
  the keyframe after all. A written-off packet sends no IDR request while RFI is on. Still off:
  intra refresh (`ir_capable = false`) — it needs per-decoder MediaCodec verification.
- Client audio: the feeder thread (`client.rs`) repairs the RTP stream (`AudioRepair`) and
  hands each payload with its payload type to `feed_audio` — Opus (97) through MediaCodec, raw
  L16 (98) straight through (a lost L16 packet is replayed halved, like the desktop). Playout is
  `AudioPlayout.kt`: the desktop's `native_audio.rs` steering — queue low point held at 15 ms
  (10 ms more per dry queue, ≤ 120 ms, eases back after 20 s), ±2 % rate via an in-app linear
  resampler (FAST tracks reject `setPlaybackRate`), flush + re-buffer past target + 150 ms,
  `audio: player stats` logged every 5 s. The start threshold is lowered (API 31+:
  `setStartThresholdInFrames`; API 24–30: usable size shrunk for the start, reopened once the
  head moves), else AudioFlinger waits for the whole 400 ms capacity before mixing. Audio quality (Settings → Session, `audioQuality` pref,
  next connect) resolves "auto" with the desktop's rules (`auto_audio_quality`). Untested on a
  device.
- `crates/tauri-plugin-pulsar-video/` — the native surface plugin.
  `PulsarVideoPlugin.kt` (~2.8k lines) is the center of gravity.
  `DecoderProbe.kt`: hardware-decoder detection + Moonlight's vendor gates;
  `PadInput.kt`: native gamepad reader (Window.Callback proxy, in-stream only).
- Deps: `pulsar-core` + `pulsar-relay` as **git deps**.

## Picture-in-picture (Android client stream)

Untested on a device (2026-11-27, pulsar-desktop#18 parity with the desktop's session small
window). Manifest: the plugin's `AndroidManifest.xml` declares `dev.pulsar.app.MainActivity`
with `supportsPictureInPicture` + `resizeableActivity`; the manifest merger adds them to the
generated activity (gen/android is never edited; if the app identifier changes, update that
name). The generated `configChanges` already has orientation|screenSize|smallestScreenSize|
screenLayout. Plugin (`PulsarVideoPlugin.kt` "picture-in-picture"): `pip_supported` (API 26+
and `FEATURE_PICTURE_IN_PICTURE`), `pip_enter` (overlay "Small window" button), `pip_set_auto`
(JS arms it while a session is live and Settings → "Small window when leaving the app",
pref `pipOnLeave`, is on; `detach` disarms). Android 12+ auto-enters via
`setAutoEnterEnabled`, 8-11 via the androidx `OnUserLeaveHintProvider`. Aspect = the single
pane's video size (split: the screen), clamped to 2.39:1..1:2.39; source rect = the picture's
fit rect. Mode changes go to JS as a `pulsar-pip` window event `{ pip, closed }` (androidx
`OnPictureInPictureModeChangedProvider`; `closed` = left PiP while the activity is stopped =
the user closed the window). `ui/js/session/pip.js`: `body.pip-mode` hides every webview
element and swallows input events; `closed` ends every session like the End button.

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

## Mobile host (phone shares its screen)

Untested on a device (2026-11-17 parity pass with the desktop host):

- **Codecs** (`host.rs` `probe_host_caps` → plugin `hostEncoderCaps` → `HostEncoder.probeCodecs`):
  every session advertises what the phone can encode, desktop order: `av1` and `h265` only from
  a hardware encoder, `h264` always. The screen goes out as the one `DisplayInfo` (real size +
  refresh) so the client offers it as the native size and caps its fps to the panel. AV1 is
  packetized like the desktop host (`pulsar-capture` `send_av1_tu`: W=0, aggregation header
  0x00, one LEB128-prefixed chunk per packet); the sequence header from the codec-config
  buffer is re-sent with every keyframe that lacks one.
- **Size / fps / bit rate** (`HostEncoder.plan`): the client's request up to what the screen and
  the encoder take — the screen fitted into the requested box (orientation-agnostic, never
  upscaled), shrunk to `VideoCapabilities.isSizeSupported`, fps ≤ panel refresh and the
  encoder's rate at that size (`KEY_MAX_FPS_TO_ENCODER` drops the panel's extra frames, API 29+),
  bit rate inside `bitrateRange`. A software encoder (emulator) keeps the old 1280 px / 30 fps /
  8 Mbit/s limits. A re-stream with only a new bit rate (client ABR) is applied live; a new
  codec / size / fps rebuilds the encoder on the same VirtualDisplay (`HostEncoder.retune`).
- **Input** (`PulsarA11yService`, the AccessibilityService): left / middle = tap / live drag,
  right = long-press (context menu), mouse back button = Back (`FEAT_MOUSE_X` is advertised),
  wheel on both axes = swipe. Keys: Esc / Alt+Left / Browser Back → Back, Meta tapped alone /
  Meta+D → Home, Alt+Tab / Meta+Tab → Recents, Meta+N notifications, Meta+A quick settings,
  Meta+L lock (9+), PrintScreen screenshot (9+), volume / media keys (AudioManager), and on the
  focused text field: typing, Backspace, Delete, Enter, arrows (Shift extends), Home / End,
  Ctrl+Home / End, Ctrl+A / C / X / V.
- **What stock Android cannot do** (no `INJECT_EVENTS` for a normal app): no real KeyEvents, so
  F-keys, Tab focus moves, Ctrl+Z, game / IME keys and apps that read raw keys get nothing;
  ↑ / ↓ only step to line boundaries (a11y has no same-column move); no mouse hover or real
  secondary / middle button (touch gestures only); the forward side button has no action;
  scrolling is a swipe (it flings); the lock screen PIN pad cannot be typed into.
