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

/// A slot's decoder health (`decoder_status`, polled by the read loop on its keepalive tick).
/// All counters are cumulative since the slot's decoder was (re)configured; a decrease means
/// it was rebuilt. Older plugin builds answer only `ok`/`detail` (the rest defaults).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DecoderStatus {
  pub ok: bool,
  /// `"failed"` = the decode path threw and the codec was rebuilt since the last poll.
  pub detail: String,
  /// An access unit was thrown away since the last poll (input backlog over its cap, or no
  /// input buffer in time): the reference chain is broken until a keyframe.
  pub need_idr: bool,
  /// Pictures the decoder put out.
  pub decoded: u64,
  /// Access units dropped before decode (backlog guard + input-buffer timeout).
  pub dropped: u64,
  /// Of those, the ones that found no free decoder input buffer in time: the decoder still
  /// held every buffer, it is behind. (Backlog-guard drops also follow a network burst.)
  pub stalled: u64,
}

impl DecoderStatus {
  /// The decode path threw and the codec was rebuilt since the last poll.
  pub fn failed(&self) -> bool {
    self.detail == "failed"
  }
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

/// Mobile host encode capabilities (`hostEncoderCaps`): `codecs` = what this phone can
/// encode, best first (`av1` / `h265` only from hardware encoders, `h264` always), and the
/// shared screen's current real size and refresh rate (0 = unknown). `ok` false = no probe
/// (desktop builds, an old plugin): the host then advertises the H.264-only fallback.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HostEncoderCaps {
  pub ok: bool,
  pub codecs: Vec<String>,
  pub width: u32,
  pub height: u32,
  pub refresh_hz: u32,
}
