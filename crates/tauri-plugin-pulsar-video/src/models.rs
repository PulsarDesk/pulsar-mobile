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
