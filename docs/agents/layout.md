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
