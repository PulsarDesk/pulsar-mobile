# Develop, debug APKs, releases

Part of the pulsar-mobile agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Develop

```bash
cd mobile && bun install
bun run tauri android dev        # emulator/device (SDK+NDK; see project memory)
cargo check -p pulsar-mobile     # host compile-check, no device toolchain
```

Standalone debug APK: `tauri android build --debug`, install the **universal**
variant (`arm64`-only can carry a stale cached devUrl). `tauri android dev`
always bakes a live devUrl — the APK breaks when you kill the dev process.

Releases: build-first — semantic-release dry-runs the version, the APK is built with
it (`--config '{"version":…}'`, tauri.conf.json is never rewritten) and verified, THEN
the tag + release is cut with `Pulsar-android.apk` attached. **`PulsarA11yService` is
declared `android:enabled="false"` in the plugin manifest — load-bearing:** Play
Protect blocks browser installs of apps that declare an accessibility service, but
not a disabled one (verified on a phone 2026-09-03); `openA11ySettings` enables the
component at runtime. CI's verify step fails the release if the attribute goes. The
launcher icon must be re-branded after `tauri android init` (`tauri icon
app-icon.png`) or the APK ships Tauri's icon.
