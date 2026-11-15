# Layout — where things live

Part of the pulsar-mobile agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Layout

- `mobile/` — the Tauri app crate (`pulsar-mobile`). `src/`: client.rs (2k LOC),
  host.rs, net.rs, rtp.rs, datachan.rs, session_cmds.rs, relay_cmds.rs
  (on-device local relay: `start/stop/local_relay_status`), config, identity.
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
- Deps: `pulsar-core` + `pulsar-relay` as **git deps**.
