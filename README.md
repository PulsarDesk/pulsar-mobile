# pulsar-mobile

The **Android + iOS** app for [Pulsar](https://github.com/PulsarDesk) — the free,
open-source remote-desktop + game-streaming platform.

A **Tauri 2** shell (Path A: a transparent webview over a native video surface)
that reuses the shared engine. The phone is both a **client** and a **host**:
it can share its own screen (MediaProjection capture + MediaCodec encode) and
accept remote control (accessibility-service gesture injection). Desktop-only
pieces (SDL3, ffmpeg, gtk/x11, libmpv) deliberately stay out so the crate
cross-compiles to mobile. Android is the working target; iOS is scaffolding
only for now. Android TV is supported (leanback launcher + D-pad navigation).

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
Conventional Commits (no commit-back) — but only **after** the Android APKs have built:
the version is computed in a dry run, both APKs are built with it stamped in and
verified (signature, version, manifest), and the release is created last with the
APKs attached. A failed build means no release, not an empty one.

Every release carries two APKs, named without a version so
`releases/latest/download/<name>` is a stable link:

| APK | What it is |
| --- | --- |
| `Pulsar-android.apk` | **The one to install.** Client + phone-as-host screen sharing. No accessibility service. |
| `Pulsar-android-hostcontrol.apk` | The same app **plus `PulsarA11yService`**, so a connected peer can *touch and control* this phone. |

Why two: Google Play Protect refuses to install an app downloaded through a browser
(or a messenger / file manager) when its manifest declares an **accessibility
service** — "this app can access sensitive information" — and offers no override. The
default APK therefore strips the service (`mobile/scripts/patch-android-no-a11y.py`)
and the host screen says so; the host-control APK keeps it and is installed with
`adb install Pulsar-android-hostcontrol.apk`.

Signing needs the repo secrets `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEY_ALIAS`,
`ANDROID_KEY_PASSWORD`, `ANDROID_STORE_PASSWORD`; the build fails loudly without
them. **iOS** store builds need a macOS runner + Apple signing and are done manually
for now.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
