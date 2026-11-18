<p align="center">
  <a href="https://pulsardesk.com"><img src=".github/assets/hero.png" alt="Pulsar Mobile: your PC, in your pocket" width="100%"></a>
</p>

<h3 align="center">Your computer and your games, on your phone. Free.</h3>

<p align="center">
  A free, open-source alternative to TeamViewer, AnyDesk and Parsec. <b>No account, no subscription, no tracking.</b>
</p>

<p align="center">
  <a href="https://github.com/PulsarDesk/pulsar-mobile/releases/latest/download/Pulsar-android.apk"><img src="https://img.shields.io/badge/Android-Download_APK-13B3D4?style=for-the-badge&logo=android&logoColor=white" alt="Download for Android"></a>
  <a href="https://github.com/PulsarDesk/pulsar-desktop/releases/latest"><img src="https://img.shields.io/badge/Desktop_app-Windows_·_macOS_·_Linux-5B57E0?style=for-the-badge" alt="Get the desktop app"></a>
</p>

<p align="center">
  <a href="https://github.com/PulsarDesk/pulsar-mobile/releases"><img src="https://img.shields.io/github/v/release/PulsarDesk/pulsar-mobile?include_prereleases&style=flat-square&color=13B3D4&label=version" alt="Version"></a>
  <a href="https://github.com/PulsarDesk/pulsar-mobile/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/PulsarDesk/pulsar-mobile/ci.yml?branch=main&style=flat-square&label=CI" alt="CI"></a>
  <a href="https://github.com/PulsarDesk/pulsar-mobile/releases"><img src="https://img.shields.io/github/downloads/PulsarDesk/pulsar-mobile/total?style=flat-square&color=5B57E0" alt="Downloads"></a>
  <a href="https://github.com/PulsarDesk/pulsar-mobile/stargazers"><img src="https://img.shields.io/github/stars/PulsarDesk/pulsar-mobile?style=flat-square&color=13B3D4" alt="Stars"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPLv3-5B57E0?style=flat-square" alt="License: GPLv3"></a>
</p>

<p align="center"><img src=".github/assets/steps.png" alt="1. Install Pulsar on both devices. 2. Share the ID and one-time password. 3. You're connected, encrypted end to end." width="100%"></p>

<p align="center"><img src=".github/assets/feat-m-control.png" alt="Control a computer: mouse or touch mode, keyboard, files and clipboard, sound and mic, any screen, several sessions" width="100%"></p>

<p align="center"><img src=".github/assets/feat-m-gaming.png" alt="Game streaming: your game library, real controllers, battery shown, quality presets, live stats, Android TV" width="100%"></p>

<p align="center"><img src=".github/assets/feat-m-share.png" alt="Share and stay safe: share your screen, you approve, always encrypted, passwords and 2FA, relay on the phone, 4 languages" width="100%"></p>

<p align="center">
  <img src="docs/screenshots/connect.png" width="30%" alt="Connect">
  &nbsp;
  <img src="docs/screenshots/game-mode.png" width="30%" alt="Game mode">
  &nbsp;
  <img src="docs/screenshots/my-device.png" width="30%" alt="My Device">
</p>

## 📥 Get Pulsar Mobile

Download **[`Pulsar-android.apk`](https://github.com/PulsarDesk/pulsar-mobile/releases/latest/download/Pulsar-android.apk)**
and install it; it updates itself after that. Needs **Android 7.0+**. On the computer, install
[Pulsar Desktop](https://github.com/PulsarDesk/pulsar-desktop). iPhone and iPad are not available yet.

> [!TIP]
> **Letting others control your phone** needs Pulsar's accessibility service. If Android blocks it: *App info* → ⋮ →
> **Allow restricted settings**, then turn it on.

## ❓ Questions

<details><summary><b>Does it work outside my home Wi-Fi?</b></summary><br>
Yes. Pulsar connects directly when it can and falls back to a relay automatically, so mobile data works too. The
<i>Data saver</i> preset keeps data use low.
</details>

<details><summary><b>Is it safe?</b></summary><br>
Every session is end-to-end encrypted. Nobody connects without the password or your approval, and you can
disconnect anyone at any time.
</details>

<details><summary><b>Is it finished?</b></summary><br>
Not yet: Pulsar is in <b>alpha</b> (0.x). Expect rough edges and
<a href="https://github.com/PulsarDesk/pulsar-mobile/issues/new/choose">tell us what you hit</a>.
</details>

## 📚 More

<details>
<summary><b>🧪 Where it has been tested</b></summary>

| Device | Connect to a computer | Share this phone |
|---|---|---|
| Samsung Galaxy Z Flip4 | ✅ | ✅ |
| Android emulator (Android 15) | ✅ | ❔ partly |
| Android TV | ❔ not tested on a real TV yet | ❔ |
| iPhone / iPad | ❌ not available yet | ❌ |

Tried it on another phone? A short note in an [issue](https://github.com/PulsarDesk/pulsar-mobile/issues) helps.

</details>

<details>
<summary><b>✨ Full feature list</b></summary>

- **Control:** mouse (trackpad) or touch mode; tap, double-tap, long-press, drag, two-finger scroll; on-screen
  keyboard with Ctrl+Alt+Del; external mice and keyboards.
- **Session:** file transfer both ways, clipboard, chat, the computer's sound, your phone's microphone; pick a
  monitor; fit / fill / stretch; portrait or landscape; several sessions with a quick switcher; saved devices with
  favourites, search and icons.
- **Game mode:** stream the desktop, one screen or a game from the computer's library; controllers over Bluetooth
  or USB with rumble and battery level; the PC sees an Xbox 360 pad or a DualShock 4 (your pick).
- **Picture:** H.264 / H.265, HDR, resolution, frame rate, bitrate, presets from *Data saver* to *Low latency*,
  live stats overlay (fps, ping, loss).
- **Share this phone:** share the screen, approve or deny, see and disconnect connected devices.
- **Security & network:** always end-to-end encrypted; one-time password, permanent password, unattended access,
  two-factor codes; direct first with relay fallback, or forced either way; your own relay, or a relay running on
  the phone.
- **Android TV** D-pad navigation; updates itself; English, Turkish, Russian and Kazakh.

</details>

<details>
<summary><b>🐞 Reporting a bug</b></summary>

[Open an issue](https://github.com/PulsarDesk/pulsar-mobile/issues/new/choose) and include:

- Pulsar version, phone model and Android version
- The other side: computer or phone, its system and Pulsar version
- What you did and what happened (black screen, no connection, touch not working, crash, ...)
- A screen recording, if you can

</details>

<details>
<summary><b>🛠️ Build from source</b></summary>

**Needs:** Rust stable, [bun](https://bun.sh), the Android SDK + NDK and JDK 17.

```bash
git clone https://github.com/PulsarDesk/pulsar-mobile.git
cd pulsar-mobile/mobile
bun install
bun run tauri android dev    # needs a device or emulator
```

More: [docs/development.md](docs/development.md). Issues and pull requests are welcome; commits follow
[Conventional Commits](https://www.conventionalcommits.org).

</details>

---

<p align="center">
  <a href="https://github.com/PulsarDesk/pulsar-desktop">🖥️ Pulsar Desktop</a> ·
  <a href="https://github.com/PulsarDesk/relay">🛰️ Pulsar Relay</a> ·
  <a href="https://github.com/PulsarDesk/pulsar-core">🧩 pulsar-core</a> ·
  <a href="https://pulsardesk.com">🌐 pulsardesk.com</a>
</p>

<p align="center">
  <sub>Free software under the <a href="LICENSE">GNU GPLv3 or later</a>. Like Pulsar? A ⭐ helps other people find it.</sub>
</p>
