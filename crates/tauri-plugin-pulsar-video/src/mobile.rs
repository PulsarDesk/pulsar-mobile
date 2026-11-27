use base64::Engine;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tauri::{
  plugin::{PluginApi, PluginHandle},
  AppHandle, Runtime,
};

use crate::models::*;

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_pulsar_video);

// initializes the Kotlin or Swift plugin classes
pub fn init<R: Runtime, C: DeserializeOwned>(
  _app: &AppHandle<R>,
  api: PluginApi<R, C>,
) -> crate::Result<PulsarVideo<R>> {
  #[cfg(target_os = "android")]
  let handle = api.register_android_plugin("dev.pulsar.video", "PulsarVideoPlugin")?;
  #[cfg(target_os = "ios")]
  let handle = api.register_ios_plugin(init_plugin_pulsar_video)?;
  Ok(PulsarVideo(handle))
}

#[derive(Serialize)]
struct MimeArgs<'a> {
  mime: &'a str,
  slot: u8,
}

#[derive(Serialize)]
struct DataArgs {
  data: String,
  slot: u8,
}

#[derive(Serialize)]
struct AudioArgs {
  data: String,
  /// RTP payload type: 97 = Opus, 98 = uncompressed L16 (pulsar-core `audio::*_PAYLOAD_TYPE`).
  pt: u8,
}

#[derive(Serialize)]
struct SlotArgs {
  slot: u8,
}

#[derive(Serialize)]
struct UpdateArgs<'a> {
  url: &'a str,
  version: &'a str,
  size: u64,
}

/// Access to the pulsar-video APIs.
pub struct PulsarVideo<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> PulsarVideo<R> {
  /// Force the webview transparent and insert a native video surface beneath it.
  pub fn attach(&self, payload: AttachRequest) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("attach", payload).map_err(Into::into)
  }

  /// Remove the native surface and make the webview opaque again.
  pub fn detach(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("detach", ()).map_err(Into::into)
  }

  /// Decode the bundled H.265 test clip onto the native surface (M3 proof).
  pub fn play_test(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("playTest", ()).map_err(Into::into)
  }

  /// Begin a live stream: attach the surface and arm the decoder for `mime`
  /// (`video/avc` or `video/hevc`). The decoder configures itself from the first
  /// access unit's parameter sets. (M4)
  pub fn start_stream(&self, mime: &str, slot: u8) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("startStream", MimeArgs { mime, slot })
      .map_err(Into::into)
  }

  /// Feed one Annex-B access unit to a slot's live decoder. Called per-frame from
  /// the session read loop; the bytes are base64'd for the JSON bridge. (M4/M8)
  pub fn feed_au(&self, au: &[u8], slot: u8) -> crate::Result<AttachResponse> {
    let data = base64::engine::general_purpose::STANDARD.encode(au);
    self
      .0
      .run_mobile_plugin("feedAu", DataArgs { data, slot })
      .map_err(Into::into)
  }

  /// Feed one audio packet to the (shared) live audio player (M5): an Opus packet
  /// (`pt` 97, decoded by MediaCodec) or an uncompressed L16 payload (`pt` 98, 16-bit
  /// big-endian interleaved stereo, played as is).
  pub fn feed_audio(&self, packet: &[u8], pt: u8) -> crate::Result<AttachResponse> {
    let data = base64::engine::general_purpose::STANDARD.encode(packet);
    self
      .0
      .run_mobile_plugin("feedAudio", AudioArgs { data, pt })
      .map_err(Into::into)
  }

  /// Stop + release a slot's live decoder. (M4/M8)
  pub fn stop_stream(&self, slot: u8) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("stopStream", SlotArgs { slot })
      .map_err(Into::into)
  }

  /// Mobile HOST (M16): capture this device's screen (MediaProjection), encode
  /// (MediaCodec), packetize to RTP, and send it to `port` on loopback.
  /// `loss_recovery`: `normal` / `short_gop` / `intra_refresh` — applied when the encoder
  /// is created (a running MediaCodec keeps its GOP; recovery there is the on-demand IDR).
  /// `epoch` = [`crate::host_epoch`] read when the stream request arrived: the plugin skips
  /// the start if a `stop_host` came after it.
  #[allow(clippy::too_many_arguments)]
  pub fn start_host(&self, port: u16, audio_port: u16, codec: &str, width: u32, height: u32, fps: u32, bitrate_kbps: u32, loss_recovery: &str, epoch: u64) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("startHost", HostArgs { port, audio_port, codec, width, height, fps, bitrate_kbps, loss_recovery, epoch })
      .map_err(Into::into)
  }

  pub fn stop_host(&self) -> crate::Result<AttachResponse> {
    let epoch = crate::next_host_epoch();
    self.0.run_mobile_plugin("stopHost", StopHostArgs { epoch }).map_err(Into::into)
  }

  /// Inject a tap/swipe into this device (normalized 0..1 coords) via the
  /// AccessibilityService — used by the mobile host's `on_input` (M16 polish).
  pub fn host_gesture(&self, x1: f64, y1: f64, x2: f64, y2: f64) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("hostGesture", GestureArgs { x1, y1, x2, y2 })
      .map_err(Into::into)
  }

  /// Force an immediate IDR on the running host encoder (client keyframe request —
  /// MediaNack([0]) — after an unrepairable packet loss).
  pub fn host_request_sync(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("hostRequestSync", ()).map_err(Into::into)
  }

  /// Move the remote-control virtual cursor to an absolute normalized (0..1) point.
  pub fn host_pointer_abs(&self, x: f64, y: f64) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("hostPointerAbs", PointerAbsArgs { x, y })
      .map_err(Into::into)
  }

  /// Move the remote-control virtual cursor by a relative (evdev px) delta.
  pub fn host_pointer_rel(&self, dx: f64, dy: f64) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("hostPointerRel", PointerRelArgs { dx, dy })
      .map_err(Into::into)
  }

  /// Press/release a remote-control pointer button (`pulsar_core::input::mouse_button`
  /// ids): left / middle drive the tap / drag finger, right is a long-press, back is the
  /// Android Back action; the plugin ignores the others.
  pub fn host_pointer_button(&self, button: u8, down: bool) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("hostPointerButton", PointerBtnArgs { button, down })
      .map_err(Into::into)
  }

  /// Mobile host: the video codecs this phone can ENCODE (hardware AV1 / HEVC, H.264
  /// always, best first) and the screen it shares (real size + refresh rate).
  pub fn host_encoder_caps(&self) -> crate::Result<HostEncoderCaps> {
    self.0.run_mobile_plugin("hostEncoderCaps", ()).map_err(Into::into)
  }

  /// Mouse wheel → a swipe at the remote-control cursor.
  pub fn host_scroll(&self, dx: f64, dy: f64) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("hostScroll", ScrollArgs { dx, dy })
      .map_err(Into::into)
  }

  /// Keyboard key (evdev code) from the remote client. Special keys map to Android
  /// navigation (Win→Home, Alt+Tab→Recents, Esc→Back); editing keys act on the
  /// focused text field via the accessibility service.
  pub fn host_key(&self, code: u32, down: bool) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("hostKey", KeyArgs { code, down })
      .map_err(Into::into)
  }

  /// A resolved Unicode string to type verbatim into the focused text field.
  pub fn host_char(&self, text: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("hostChar", CharArgs { text })
      .map_err(Into::into)
  }

  /// `ok` reflects whether the "draw over other apps" overlay permission is granted
  /// (gates the visible remote-control cursor).
  pub fn overlay_granted(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("overlayGranted", ()).map_err(Into::into)
  }

  /// Open the system settings to grant the "draw over other apps" overlay permission.
  pub fn request_overlay(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("requestOverlay", ()).map_err(Into::into)
  }

  /// Open Android's Accessibility settings so the user can enable Pulsar control.
  pub fn open_a11y_settings(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("openA11ySettings", ()).map_err(Into::into)
  }

  /// `ok` reflects whether the control AccessibilityService is currently enabled.
  pub fn a11y_enabled(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("a11yEnabled", ()).map_err(Into::into)
  }

  /// `ok` reflects whether the control AccessibilityService is DECLARED in this build at
  /// all. The default release APK strips it (Play Protect blocks browser installs of apps
  /// that declare one); only the "host control" APK carries it.
  pub fn a11y_available(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("a11yAvailable", ()).map_err(Into::into)
  }

  /// `ok` when Android 13+ will show "Restricted setting" for the accessibility service
  /// until the user allows restricted settings in App info (non-store install).
  pub fn a11y_restricted(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("a11yRestricted", ()).map_err(Into::into)
  }

  /// Open this app's App info page (where "Allow restricted settings" lives).
  pub fn open_app_info(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("openAppInfo", ()).map_err(Into::into)
  }

  /// W6-host-notify: post a high-priority heads-up notification for an incoming
  /// connection request, so the user is alerted even when the app is backgrounded
  /// or the screen is off (the in-app approval sheet is invisible then). Called
  /// from the host `approval_race` alongside the `session-request` event.
  pub fn notify_request(&self, peer: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("notifyRequest", NotifyRequestArgs { peer })
      .map_err(Into::into)
  }

  /// Ensure the POST_NOTIFICATIONS runtime permission (Android 13+); fires the
  /// system dialog when missing. `ok` = already granted.
  pub fn notif_permission(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("notifPermission", ()).map_err(Into::into)
  }

  /// Dismiss the incoming-request notification (request answered or expired).
  pub fn cancel_notify_request(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("cancelNotifyRequest", ()).map_err(Into::into)
  }

  /// Set the app-UI language used for notification strings (tr/en/ru/kk).
  pub fn set_notif_lang(&self, lang: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("setNotifLang", NotifLangArgs { lang })
      .map_err(Into::into)
  }

  // ---- W3-media-native additions ----

  /// Mute (`muted: true`) or unmute (`false`) the remote audio AudioTrack.
  pub fn set_audio_muted(&self, muted: bool) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("setAudioMuted", AudioMutedArgs { muted })
      .map_err(Into::into)
  }

  /// Set video surface aspect mode for `slot`: `"fit"` | `"fill"` | `"stretch"`.
  pub fn set_aspect(&self, slot: u8, mode: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("setAspect", AspectArgs { slot, mode: mode.to_string() })
      .map_err(Into::into)
  }

  /// Apply a pinch-zoom/pan transform: the video's destination rect on screen,
  /// normalized to the surface [0..1] (`w`/`h` > 1 = zoomed in). DPR-independent.
  pub fn set_video_transform(&self, slot: u8, x: f32, y: f32, w: f32, h: f32) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("setVideoTransform", VideoTransformArgs { slot, x, y, w, h })
      .map_err(Into::into)
  }

  /// Lock screen orientation: `landscape: true` → landscape, `false` → portrait.
  pub fn set_orientation(&self, landscape: bool) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("setOrientation", OrientationArgs { landscape })
      .map_err(Into::into)
  }

  /// Status / nav bar icon colour vs the app theme: `light_theme: true` → dark
  /// icons (light bg); `false` → light icons (dark bg). `setting` (the app theme
  /// setting) becomes the app's night mode, for the launch splash.
  pub fn set_status_bar(&self, light_theme: bool, setting: Option<String>) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("setStatusBar", StatusBarArgs { light_theme, setting })
      .map_err(Into::into)
  }

  /// Read the system clipboard text via the native `ClipboardManager`
  /// (returned in `AttachResponse.detail`).
  pub fn read_clipboard(&self) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("readClipboard", ())
      .map_err(Into::into)
  }

  /// Clipboard sync: has the clipboard changed since `seq` (-1 = baseline)? Returns its text
  /// when it did and the app may read it now (focused). `since` (epoch ms, baseline only) =
  /// when the session opened: a clip set after it is reported as `fresh`. Rust-only.
  pub fn clipboard_poll(&self, seq: i64, since: i64) -> crate::Result<ClipPoll> {
    self
      .0
      .run_mobile_plugin("clipboardPoll", ClipPollArgs { seq, since })
      .map_err(Into::into)
  }

  /// Clipboard sync: write a peer's text to the system clipboard. Rust-only.
  pub fn write_clipboard(&self, text: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("writeClipboard", ClipWriteArgs { text })
      .map_err(Into::into)
  }

  /// Copy a received file (`path`, private app storage) into the public Downloads/Pulsar
  /// Received folder and post the "file received" notification (Open / Share). Rust-only.
  pub fn publish_download(&self, path: &str, name: &str) -> crate::Result<PublishedFile> {
    self
      .0
      .run_mobile_plugin("publishDownload", PublishArgs { path, name })
      .map_err(Into::into)
  }

  /// Open a received file (`content://` URI from `publish_download`) in another app.
  pub fn open_download(&self, uri: &str, mime: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("openDownload", UriArgs { uri, mime })
      .map_err(Into::into)
  }

  /// Share a received file (`content://` URI) through the system share sheet.
  pub fn share_download(&self, uri: &str, mime: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("shareDownload", UriArgs { uri, mime })
      .map_err(Into::into)
  }

  /// Display refresh rate in Hz (returned in `AttachResponse.detail`).
  pub fn screen_refresh_rate(&self) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("screenRefreshRate", ())
      .map_err(Into::into)
  }

  /// Connected gamepads + battery (JSON in `AttachResponse.detail`).
  /// Last decoded video size for `slot` (`detail = "<vw>x<vh>"`, "0x0" until first frame).
  pub fn get_video_size(&self, slot: u8) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("getVideoSize", SlotArgs { slot })
      .map_err(Into::into)
  }

  /// Capture (`on: true`) or release a physical mouse (Android pointer capture).
  pub fn set_pointer_capture(&self, on: bool) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("setPointerCapture", PointerCaptureArgs { on })
      .map_err(Into::into)
  }

  pub fn pip_supported(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("pipSupported", ()).map_err(Into::into)
  }

  pub fn pip_enter(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("pipEnter", ()).map_err(Into::into)
  }

  pub fn pip_set_auto(&self, on: bool) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("pipSetAuto", PipAutoArgs { on })
      .map_err(Into::into)
  }

  pub fn gamepad_battery(&self) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("gamepadBattery", ())
      .map_err(Into::into)
  }

  /// Poll+clear the slot's decoder-failed flag. `true` = the decode path threw and the
  /// codec was rebuilt since the last poll — the caller should nudge the host for an IDR
  /// (`MediaNack([0])`) so the fresh decoder gets a reference frame immediately.
  pub fn decoder_failed(&self, slot: u8) -> crate::Result<bool> {
    self
      .0
      .run_mobile_plugin::<AttachResponse>("decoderStatus", SlotArgs { slot })
      .map(|r| r.detail == "failed")
      .map_err(Into::into)
  }

  /// Poll the slot's decoder health and clear its one-shot flags (`failed`, `need_idr`): see
  /// [`DecoderStatus`]. The caller nudges the host for an IDR on either flag and watches the
  /// counters for a decoder that sheds what it is fed.
  pub fn decoder_status(&self, slot: u8) -> crate::Result<DecoderStatus> {
    self
      .0
      .run_mobile_plugin::<DecoderStatus>("decoderStatus", SlotArgs { slot })
      .map_err(Into::into)
  }

  /// The home-screen wallpaper as a base64 JPEG (≤14 KB) in
  /// `AttachResponse.detail`; `ok:false` when it can't be read. Used as this
  /// device's identity image pushed to peers (mobile analogue of the desktop
  /// `avatar_mode = "wallpaper"`).
  pub fn get_wallpaper_avatar(&self) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("getWallpaperAvatar", ())
      .map_err(Into::into)
  }

  // ---- W4-mic additions -------------------------------------------------------

  /// Start capturing microphone audio via Android AudioRecord (VOICE_COMMUNICATION,
  /// 48 kHz mono s16le). PCM is buffered internally; W4-rust-client's `mic_start`
  /// Tauri command drains it and forwards `DataMsg::Audio` frames to the host.
  pub fn mic_start(&self) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("micStart", ())
      .map_err(Into::into)
  }

  /// Stop the AudioRecord capture. Any remaining buffered PCM is still readable
  /// by the caller before sending `DataMsg::AudioEnd`.
  pub fn mic_stop(&self) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("micStop", ())
      .map_err(Into::into)
  }

  // ---- In-app update (Updater.kt) --------------------------------------------

  /// Start the background APK download (resolves immediately).
  pub fn update_download(&self, url: &str, version: &str, size: u64) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("updateDownload", UpdateArgs { url, version, size })
      .map_err(Into::into)
  }

  /// Stop the running download + delete its `.part`.
  pub fn update_cancel(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("updateCancel", ()).map_err(Into::into)
  }

  /// On-disk + live-progress state for `version`.
  pub fn update_status(&self, version: &str, size: u64) -> crate::Result<UpdateStatus> {
    self
      .0
      .run_mobile_plugin("updateStatus", UpdateArgs { url: "", version, size })
      .map_err(Into::into)
  }

  /// Launch the system installer for `version`'s downloaded APK.
  pub fn update_install(&self, version: &str) -> crate::Result<UpdateInstall> {
    self
      .0
      .run_mobile_plugin("updateInstall", UpdateArgs { url: "", version, size: 0 })
      .map_err(Into::into)
  }

  /// Poll one ~20 ms PCM frame (48 kHz mono s16le, 1920 bytes) from the native mic
  /// buffer, returning the raw decoded s16le bytes — or an empty `Vec` when the
  /// buffer has no frame ready. The Kotlin side base64-encodes the PCM into the
  /// response `detail` (`{ok:false, detail:""}` when empty); we decode it here so
  /// callers can forward the bytes straight into `DataMsg::Audio`. Called in a tight
  /// loop by the W4-rust-client `mic_start` Tauri command task.
  ///
  /// This is a **native-only** bridge method — it is NOT registered as a JS
  /// command in `lib.rs`/`default.toml`. Only Rust code calls it.
  /// Voice call: play one chunk of the peer's mic (s16le 48 kHz mono) on a voice-call
  /// AudioTrack of its own (not the session audio's playout).
  pub fn voice_feed(&self, pcm: &[u8]) -> crate::Result<AttachResponse> {
    let data = base64::engine::general_purpose::STANDARD.encode(pcm);
    self
      .0
      .run_mobile_plugin("voiceFeed", VoiceArgs { data })
      .map_err(Into::into)
  }

  /// Voice call: close the peer-voice track.
  pub fn voice_stop(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("voiceStop", ()).map_err(Into::into)
  }

  /// Ring: loop `pcm` (one cycle, s16le 48 kHz mono) on the ringtone stream, or the
  /// phone's own ringtone when `system`; vibrates too when the ringer mode allows it.
  pub fn ring_start(&self, pcm: &[u8], system: bool) -> crate::Result<AttachResponse> {
    let data = base64::engine::general_purpose::STANDARD.encode(pcm);
    self
      .0
      .run_mobile_plugin("ringStart", RingArgs { data, system })
      .map_err(Into::into)
  }

  pub fn ring_stop(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("ringStop", ()).map_err(Into::into)
  }

  /// A high-priority "incoming call" notification (tap = open Pulsar), for a call that rings
  /// while the app is in the background.
  pub fn call_notify(&self, title: &str, body: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("callNotify", NotifyArgs { title, body })
      .map_err(Into::into)
  }

  /// The call's `microphone` foreground service (the mic keeps working in the background).
  pub fn call_service(&self, on: bool) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("callService", CallServiceArgs { on })
      .map_err(Into::into)
  }

  pub fn call_notify_clear(&self) -> crate::Result<AttachResponse> {
    self.0.run_mobile_plugin("callNotifyClear", ()).map_err(Into::into)
  }

  pub fn poll_mic_frame(&self) -> crate::Result<Vec<u8>> {
    let resp = self
      .0
      .run_mobile_plugin::<AttachResponse>("pollMicFrame", ())?;
    if resp.detail.is_empty() {
      return Ok(Vec::new());
    }
    Ok(
      base64::engine::general_purpose::STANDARD
        .decode(resp.detail)
        .unwrap_or_default(),
    )
  }
}

#[derive(Serialize)]
struct PointerCaptureArgs {
  on: bool,
}

#[derive(Serialize)]
struct PipAutoArgs {
  on: bool,
}

#[derive(Serialize)]
struct AudioMutedArgs {
  muted: bool,
}

#[derive(Serialize)]
struct AspectArgs {
  slot: u8,
  mode: String,
}

#[derive(Serialize)]
struct VideoTransformArgs {
  slot: u8,
  x: f32,
  y: f32,
  w: f32,
  h: f32,
}

#[derive(Serialize)]
struct OrientationArgs {
  landscape: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusBarArgs {
  light_theme: bool,
  setting: Option<String>,
}

#[derive(Serialize)]
struct GestureArgs {
  x1: f64,
  y1: f64,
  x2: f64,
  y2: f64,
}

#[derive(Serialize)]
struct PointerAbsArgs {
  x: f64,
  y: f64,
}

#[derive(Serialize)]
struct PointerRelArgs {
  dx: f64,
  dy: f64,
}

#[derive(Serialize)]
struct PointerBtnArgs {
  button: u8,
  down: bool,
}

#[derive(Serialize)]
struct ScrollArgs {
  dx: f64,
  dy: f64,
}

#[derive(Serialize)]
struct KeyArgs {
  code: u32,
  down: bool,
}

#[derive(Serialize)]
struct CharArgs<'a> {
  text: &'a str,
}

#[derive(Serialize)]
struct HostArgs<'a> {
  port: u16,
  audio_port: u16,
  codec: &'a str,
  width: u32,
  height: u32,
  fps: u32,
  bitrate_kbps: u32,
  loss_recovery: &'a str,
  epoch: u64,
}

#[derive(Serialize)]
struct StopHostArgs {
  epoch: u64,
}

#[derive(Serialize)]
struct NotifyRequestArgs<'a> {
  peer: &'a str,
}

#[derive(Serialize)]
struct DecoderCapsArgs {
  width: u32,
  height: u32,
}

#[derive(Serialize)]
struct StreamFpsArgs {
  slot: u8,
  fps: u32,
}

#[derive(Serialize)]
struct PadFeedArgs<'a> {
  port: u16,
  token: &'a str,
}

#[derive(Serialize)]
struct PadRumbleArgs {
  pad: u8,
  large: u8,
  small: u8,
}

#[derive(Serialize)]
struct Ds5UsbPollArgs {
  fresh: bool,
}

#[derive(Serialize)]
struct Ds5UsbCloseArgs {
  id: u32,
  failed: bool,
}

#[derive(Serialize)]
struct ClipPollArgs {
  seq: i64,
  since: i64,
}

#[derive(Serialize)]
struct ClipWriteArgs<'a> {
  text: &'a str,
}

#[derive(Serialize)]
struct VoiceArgs {
  data: String,
}

#[derive(Serialize)]
struct RingArgs {
  data: String,
  system: bool,
}

#[derive(Serialize)]
struct CallServiceArgs {
  on: bool,
}

#[derive(Serialize)]
struct NotifyArgs<'a> {
  title: &'a str,
  body: &'a str,
}

#[derive(Serialize)]
struct PublishArgs<'a> {
  path: &'a str,
  name: &'a str,
}

#[derive(Serialize)]
struct UriArgs<'a> {
  uri: &'a str,
  mime: &'a str,
}

#[derive(Serialize)]
struct NotifLangArgs<'a> {
  lang: &'a str,
}

// ---- W5-native additions -------------------------------------------------------

/// Response from `enumerateDecoders` / `enumerateHostCodecs` — a list of codec
/// name strings (e.g. `["h265", "h264", "av1"]`).
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct CodecListResponse {
  pub ok: bool,
  pub codecs: Vec<String>,
}

/// Response from `positionPanes` — echoes the applied layout.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct LayoutResponse {
  pub ok: bool,
  pub layout: String,
}

/// Response from `setHdrMode` — echoes the applied mode.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct HdrResponse {
  pub ok: bool,
  pub slot: u8,
  pub mode: String,
}

#[derive(Serialize)]
struct PositionPanesArgs {
  layout: String,
}

#[derive(Serialize)]
struct SetHdrModeArgs {
  slot: u8,
  mode: String,
}

/// W5-native extension trait for additional `PulsarVideo<R>` methods. Rust allows
/// multiple `impl` blocks for the same type, but a trait keeps the W5 additions
/// clearly namespaced and avoids confusion with existing methods in the same
/// file. Import `PulsarVideoW5Ext` to call these.
pub trait PulsarVideoW5Ext<R: Runtime> {
  /// W5-native: Arrange the active video panes in a named layout.
  ///
  /// `layout`: `"single"` (default/fullscreen), `"left-right"` (landscape split),
  /// `"top-bottom"` (portrait/stacked split), `"quad"` (2×2 grid, 4 slots).
  ///
  /// Native-only — not a JS command.
  fn position_panes(&self, layout: &str) -> crate::Result<LayoutResponse>;

  /// W5-native: Enumerate video decoder MIME types supported by this device.
  ///
  /// Returns a `CodecListResponse` with names like `["h265", "h264", "av1"]` in
  /// preference order. Used by the `host_codecs` probe (W5-rust-host lane) and by
  /// the connect-time codec negotiation to exclude codecs the device cannot decode.
  ///
  /// Native-only — not a JS command.
  fn enumerate_decoders(&self) -> crate::Result<CodecListResponse>;

  /// W5-native: Set the HDR rendering mode for one video pane.
  ///
  /// `mode`: `"sdr"` (default), `"hdr10"`, `"hlg"`.
  ///
  /// Updates the Kotlin `Pane.hdrMediaFormatHints` and the `SurfaceView` color
  /// mode / Surface data space so the display pipeline routes the surface to the
  /// correct HW path. Takes effect on the next decoder configure (i.e. after the
  /// next keyframe / `arm()`).
  ///
  /// Native-only — not a JS command.
  fn set_hdr_mode(&self, slot: u8, mode: &str) -> crate::Result<HdrResponse>;

  /// Hardware/software decodable codecs, per-codec decoder fps limit at `width`x`height`
  /// and the panel's highest refresh (connect-time codec + fps choice). Native-only.
  fn decoder_caps(&self, width: u32, height: u32) -> crate::Result<DecoderCaps>;

  /// The stream on `slot` runs at `fps`: vote for a matching display refresh
  /// (Surface.setFrameRate / preferredDisplayModeId). 0 drops the vote. Native-only.
  fn set_stream_fps(&self, slot: u8, fps: u32) -> crate::Result<AttachResponse>;

  /// Mobile host: projection-stop generation + end reason (see [`HostStatus`]). Native-only.
  fn host_status(&self) -> crate::Result<HostStatus>;

  /// What the active network runs over (cellular / metered / VPN), see [`NetworkKind`].
  /// Native-only.
  fn network_kind(&self) -> crate::Result<NetworkKind>;

  /// Physical gamepads: send pad state to this loopback UDP port, each datagram prefixed
  /// with `token` (hex; the receiver drops datagrams without it). Native-only.
  fn pad_feed_start(&self, port: u16, token: &str) -> crate::Result<AttachResponse>;

  /// Host rumble for physical pad `pad` (0..255 per motor, both 0 = stop). Native-only.
  fn pad_rumble(&self, pad: u8, large: u8, small: u8) -> crate::Result<AttachResponse>;

  /// DualSense raw passthrough: open every USB DualSense the app may use (asks the user
  /// for USB permission once per device while `fresh`), claim its HID interface and
  /// return the newly opened ones, plus whether a stream is on screen with focus.
  /// Native-only.
  fn ds5_usb_poll(&self, fresh: bool) -> crate::Result<Ds5UsbPoll>;

  /// Close a pad opened by [`Self::ds5_usb_poll`] (after Rust released its interface and
  /// re-attached the kernel driver) and free its gamepad slot. `failed` = the pad could
  /// not be driven (init failed): it is not claimed again until the next passthrough
  /// period, instead of every poll. Native-only.
  fn ds5_usb_close(&self, id: u32, failed: bool) -> crate::Result<AttachResponse>;
}

impl<R: Runtime> PulsarVideoW5Ext<R> for PulsarVideo<R> {
  fn position_panes(&self, layout: &str) -> crate::Result<LayoutResponse> {
    self
      .0
      .run_mobile_plugin("positionPanes", PositionPanesArgs { layout: layout.to_string() })
      .map_err(Into::into)
  }

  fn enumerate_decoders(&self) -> crate::Result<CodecListResponse> {
    self
      .0
      .run_mobile_plugin("enumerateDecoders", ())
      .map_err(Into::into)
  }

  fn set_hdr_mode(&self, slot: u8, mode: &str) -> crate::Result<HdrResponse> {
    self
      .0
      .run_mobile_plugin(
        "setHdrMode",
        SetHdrModeArgs { slot, mode: mode.to_string() },
      )
      .map_err(Into::into)
  }

  fn decoder_caps(&self, width: u32, height: u32) -> crate::Result<DecoderCaps> {
    self
      .0
      .run_mobile_plugin("decoderCaps", DecoderCapsArgs { width, height })
      .map_err(Into::into)
  }

  fn set_stream_fps(&self, slot: u8, fps: u32) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("setStreamFps", StreamFpsArgs { slot, fps })
      .map_err(Into::into)
  }

  fn host_status(&self) -> crate::Result<HostStatus> {
    self.0.run_mobile_plugin("hostStatus", ()).map_err(Into::into)
  }

  fn network_kind(&self) -> crate::Result<NetworkKind> {
    self.0.run_mobile_plugin("networkKind", ()).map_err(Into::into)
  }

  fn pad_feed_start(&self, port: u16, token: &str) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("padFeedStart", PadFeedArgs { port, token })
      .map_err(Into::into)
  }

  fn pad_rumble(&self, pad: u8, large: u8, small: u8) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("padRumble", PadRumbleArgs { pad, large, small })
      .map_err(Into::into)
  }

  fn ds5_usb_poll(&self, fresh: bool) -> crate::Result<Ds5UsbPoll> {
    self
      .0
      .run_mobile_plugin("ds5UsbPoll", Ds5UsbPollArgs { fresh })
      .map_err(Into::into)
  }

  fn ds5_usb_close(&self, id: u32, failed: bool) -> crate::Result<AttachResponse> {
    self
      .0
      .run_mobile_plugin("ds5UsbClose", Ds5UsbCloseArgs { id, failed })
      .map_err(Into::into)
  }
}
