use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachRequest {
  /// Solid color to paint the test surface (hex, e.g. "#00E5FF"). M2 spike only —
  /// later this is replaced by the decoder's output surface.
  pub color: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachResponse {
  pub ok: bool,
  // Some Android @Command handlers (e.g. setStatusBar) resolve with just `{ok}`,
  // so accept a missing `detail` instead of failing deserialization.
  #[serde(default)]
  pub detail: String,
}

/// In-app update: `update_status` result. `downloaded` = this version's APK is complete on
/// disk; `active`/`received`/`total`/`bps` = live download progress (the UI polls this —
/// plugin `trigger()` events don't reach JS in this build); `error` = last failure.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateStatus {
  pub downloaded: bool,
  pub bytes: u64,
  pub active: bool,
  pub received: u64,
  pub total: u64,
  pub bps: u64,
  pub error: String,
}

/// In-app update: `update_install` result — exactly one of the flags is set (or `error`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateInstall {
  pub started: bool,
  pub needs_permission: bool,
  pub signature_mismatch: bool,
  pub error: String,
}

/// `decoderCaps` (native-only): what this device decodes. `hw` = codecs (`"h265"`, `"h264"`,
/// `"av1"`) with a HARDWARE decoder; `sw` = every decodable codec, hardware or software;
/// `max_fps` = codec -> highest rate its decoder declares for the asked size (0 / missing =
/// unknown); `panel_hz` = the display's highest refresh at its current resolution. The
/// default (all empty / 0) means "unknown" (desktop builds, iOS, a failed probe).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DecoderCaps {
  pub ok: bool,
  pub hw: Vec<String>,
  pub sw: Vec<String>,
  pub max_fps: std::collections::HashMap<String, u32>,
  pub panel_hz: u32,
}

/// `hostStatus` (native-only): `stop_gen` moves each time the screen-share projection is
/// stopped outside Pulsar; `message` is the end reason for the viewer (app language).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HostStatus {
  pub ok: bool,
  pub stop_gen: u32,
  pub message: String,
}
