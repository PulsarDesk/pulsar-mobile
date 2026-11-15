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
- `mobile/ui/` — **static** webview UI, plain ES modules, no bundler
  (`frontendDist: "ui"`). i18n: `ui/js/i18n/{en,tr,ru,kk}.js`.
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
