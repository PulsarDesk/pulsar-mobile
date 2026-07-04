# pulsar-mobile

The **Android + iOS** app for [Pulsar](https://github.com/PulsarDesk) — the free,
open-source remote-desktop + game-streaming platform.

A thin, client-first **Tauri 2** shell (Path A: a transparent webview over a native
video surface) that reuses the shared engine. Desktop-only pieces (SDL3, ffmpeg,
gtk/x11, libmpv) deliberately stay out so the crate cross-compiles to mobile.

## Layout

```
pulsar-mobile/
  mobile/                         # the Tauri mobile app (pulsar-mobile crate + ui/)
    src/                          #   Rust: client/host/transport glue
    ui/                           #   static webview UI (touch-first)
    tauri.conf.json               #   mobile app config
  crates/
    tauri-plugin-pulsar-video/    # native video-surface plugin (Android MediaCodec
                                  # host/encode, surface transform/orientation)
```

Depends on (git dependencies):
[`pulsar-core`](https://github.com/PulsarDesk/pulsar-core) (shared engine),
[`relay`](https://github.com/PulsarDesk/relay) (on-device local relay), and
transitively [`pulsar-proto`](https://github.com/PulsarDesk/pulsar-proto).

## Develop

```bash
cd mobile
bun install
bun run tauri android dev      # Android (needs Android SDK + NDK + an emulator/device)
bun run tauri ios dev          # iOS (needs macOS + Xcode)
```

Host compile-check (no device toolchain):

```bash
cargo check -p pulsar-mobile
```

## Releases

CI compile-checks every push/PR. `release.yml` cuts a **tag + GitHub Release** from
Conventional Commits (no commit-back). Attaching a built **APK** is opt-in — set repo
variable `ENABLE_MOBILE_RELEASE=true` and provide an Android signing keystore
(`ANDROID_KEYSTORE_BASE64`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`,
`ANDROID_STORE_PASSWORD`). **iOS** store builds need a macOS runner + Apple signing and
are done manually for now.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
