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
