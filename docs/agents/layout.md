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
  (IDR requests back off in a loss streak), `pong::PongMatcher`, `caps::mobile_auto_cap_kbps`
  (≤ 50 Mbit; was a flat 12) + `path_cap_kbps` (relay cap while relayed, `path_changed` on a
  relay ↔ direct switch), `memory` keyed by peer × path class. `rtp.rs` `ReorderBuffer` only
  orders; its wait is the repair window + one sweep (≤ 250 ms). The Kotlin decoder reports
  dropped AUs (`needIdr` → IDR request) and its counters (`decoder_status`): a decoder that
  sheds frames 2 windows running steps the fps down (`Controller::client_decode_bound`).
  Still off on mobile: RFI and intra refresh (`rfi: false`, `ir_capable = false`) — they need
  the depacketizer to resume on a recovery frame and per-decoder MediaCodec verification.
- `crates/tauri-plugin-pulsar-video/` — the native surface plugin.
  `PulsarVideoPlugin.kt` (~2.8k lines) is the center of gravity.
- Deps: `pulsar-core` + `pulsar-relay` as **git deps**.
