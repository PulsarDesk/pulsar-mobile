//! What this phone can decode, and the codec / frame-rate choices that follow from it.
//!
//! * Codec (audit 1.1): "auto" picks the host's best codec that has a HARDWARE decoder here
//!   — not simply the host's first, which on an AV1-capable host meant software AV1 on most
//!   phones (lag, heat, or a black screen without any AV1 decoder). The request carries the
//!   whole decodable list as `decode_codecs`, so the host's codec fallback stays inside it.
//! * Frame rate (audit 1.3): capped by what the decoder declares for the stream size
//!   (`areSizeAndRateSupported`), not a fixed 60; "unlimited" (0) means the panel's highest
//!   refresh.
//!
//! The probe is Android-only (`decoderCaps` in the video plugin). Elsewhere (desktop
//! compile checks, iOS which has no native plugin yet) [`DecoderCaps::default`] stands for
//! "unknown" and the choices fall back to the previous behavior (minus software AV1).

use tauri::{AppHandle, Runtime};
pub use tauri_plugin_pulsar_video::DecoderCaps;

/// The fps used when neither the request nor the panel says anything.
pub const DEFAULT_FPS: u32 = 60;

/// Probe the decoders for a `width`x`height` stream (0x0 = 1080p). Never fails: an error
/// yields the "unknown" default.
pub fn query<R: Runtime>(app: &AppHandle<R>, width: u32, height: u32) -> DecoderCaps {
    #[cfg(mobile)]
    {
        use tauri_plugin_pulsar_video::{PulsarVideoExt as _, PulsarVideoW5Ext as _};
        match app.pulsar_video().decoder_caps(width, height) {
            Ok(c) => {
                log::info!(
                    "pulsar: decoder caps hw={:?} sw={:?} max_fps={:?} panel={}Hz @{}x{}",
                    c.hw, c.sw, c.max_fps, c.panel_hz, width, height
                );
                return c;
            }
            Err(e) => log::warn!("pulsar: decoder caps probe failed: {e:?}"),
        }
    }
    let _ = (app, width, height);
    DecoderCaps::default()
}

/// Wire codec name, normalized (`"hevc"` -> `"h265"`, `"avc"` -> `"h264"`).
fn norm(c: &str) -> String {
    match c.to_ascii_lowercase().as_str() {
        "hevc" | "h265" => "h265".into(),
        "avc" | "h264" => "h264".into(),
        "av01" | "av1" => "av1".into(),
        other => other.into(),
    }
}

/// The codecs this phone should be streamed in, best first: hardware decoders when any are
/// known, else software ones without AV1 (software AV1 is too slow for a live stream on a
/// phone). Empty = unknown.
fn decodable(caps: &DecoderCaps) -> Vec<String> {
    if !caps.hw.is_empty() {
        return caps.hw.iter().map(|c| norm(c)).collect();
    }
    caps.sw.iter().map(|c| norm(c)).filter(|c| c != "av1").collect()
}

/// Resolve the codec for a stream request and the `decode_codecs` list to send with it.
///
/// * An explicit pick (`pref` other than `"auto"`/empty) the host offers is honored even
///   without a hardware decoder (the user asked for it); it is then appended to the list.
/// * `"auto"`: the host's first codec this phone decodes in hardware (host order = its
///   preference); without any overlap, H.264 when the host has it (a software H.264 decoder
///   exists nearly everywhere), else the host's first.
/// * `decode_codecs` = the decodable codecs in host-preference order, then the remaining
///   decodable ones, then the resolved codec if missing — stable across codec switches, so
///   a later "auto" re-resolves to its first entry.
/// * Unknown caps: the old choice (`pref`, else the host's first codec) except that a
///   host-first AV1 is skipped for its next codec; `decode_codecs` = just that codec.
pub fn pick_codec(host: &[String], pref: &str, caps: &DecoderCaps) -> (String, Vec<String>) {
    let host: Vec<String> = host.iter().map(|c| norm(c)).collect();
    let pref = norm(pref);
    let explicit = !pref.is_empty() && pref != "auto";
    let dec = decodable(caps);

    if dec.is_empty() {
        let resolved = if explicit && host.contains(&pref) {
            pref
        } else {
            host.iter()
                .find(|c| c.as_str() != "av1")
                .or_else(|| host.first())
                .cloned()
                .unwrap_or_else(|| "h264".into())
        };
        return (resolved.clone(), vec![resolved]);
    }

    let mut list: Vec<String> = host.iter().filter(|c| dec.contains(c)).cloned().collect();
    for c in &dec {
        if !list.contains(c) {
            list.push(c.clone());
        }
    }
    let resolved = if explicit && host.contains(&pref) {
        pref
    } else if let Some(c) = host.iter().find(|c| dec.contains(c)) {
        c.clone()
    } else if host.iter().any(|c| c == "h264") || host.is_empty() {
        "h264".into()
    } else {
        host[0].clone()
    };
    if !list.contains(&resolved) {
        list.push(resolved.clone());
    }
    (resolved, list)
}

/// The fps to request: `requested` (0 = "unlimited" -> the panel's highest refresh), capped
/// by the decoder's declared limit for `codec` at the stream size. Unknown decoder limit:
/// capped at the panel refresh (never below 60), the old conservative bound.
pub fn effective_fps(requested: u32, codec: &str, caps: &DecoderCaps) -> u32 {
    let panel = if (24..=500).contains(&caps.panel_hz) { caps.panel_hz } else { DEFAULT_FPS };
    let want = if requested == 0 { panel } else { requested };
    match caps.max_fps.get(&norm(codec)).copied().filter(|&m| m > 0) {
        Some(max) => want.min(max),
        None => want.min(panel.max(DEFAULT_FPS)),
    }
    .max(1)
}

/// Tell the video plugin the stream's fps so the display runs at a matching refresh
/// (Android; a no-op elsewhere).
pub fn vote_refresh<R: Runtime>(app: &AppHandle<R>, slot: u8, fps: u32) {
    #[cfg(mobile)]
    {
        use tauri_plugin_pulsar_video::{PulsarVideoExt as _, PulsarVideoW5Ext as _};
        if let Err(e) = app.pulsar_video().set_stream_fps(slot, fps) {
            log::warn!("pulsar: set_stream_fps({slot}, {fps}) failed: {e:?}");
        }
    }
    let _ = (app, slot, fps);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(hw: &[&str], sw: &[&str], max: &[(&str, u32)], panel: u32) -> DecoderCaps {
        DecoderCaps {
            ok: true,
            hw: hw.iter().map(|s| s.to_string()).collect(),
            sw: sw.iter().map(|s| s.to_string()).collect(),
            max_fps: max.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            panel_hz: panel,
        }
    }
    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn auto_skips_software_av1() {
        // AV1-first host, phone with HW HEVC/H.264 only (software AV1 present).
        let c = caps(&["h265", "h264"], &["h265", "h264", "av1"], &[], 120);
        let (codec, list) = pick_codec(&v(&["av1", "h265", "h264"]), "auto", &c);
        assert_eq!(codec, "h265");
        assert_eq!(list, v(&["h265", "h264"]));
    }

    #[test]
    fn auto_takes_hw_av1_in_host_order() {
        let c = caps(&["h265", "h264", "av1"], &["h265", "h264", "av1"], &[], 120);
        let (codec, list) = pick_codec(&v(&["av1", "h265", "h264"]), "auto", &c);
        assert_eq!(codec, "av1");
        assert_eq!(list, v(&["av1", "h265", "h264"]));
    }

    #[test]
    fn explicit_pick_is_honored_and_listed() {
        let c = caps(&["h264"], &["h264", "h265"], &[], 60);
        let (codec, list) = pick_codec(&v(&["h265", "h264"]), "h265", &c);
        assert_eq!(codec, "h265");
        assert_eq!(list, v(&["h264", "h265"]));
        // An explicit pick the host lacks falls back to auto.
        let (codec, _) = pick_codec(&v(&["h264"]), "h265", &c);
        assert_eq!(codec, "h264");
    }

    #[test]
    fn no_overlap_prefers_h264() {
        let c = caps(&["h264"], &["h264"], &[], 60);
        let (codec, list) = pick_codec(&v(&["av1", "h265"]), "auto", &c);
        assert_eq!(codec, "av1"); // host has no h264: its first codec
        assert_eq!(list, v(&["h264", "av1"]));
        let (codec, _) = pick_codec(&v(&["av1", "h265", "h264"]), "auto", &caps(&["vp9"], &[], &[], 60));
        assert_eq!(codec, "h264");
    }

    #[test]
    fn software_only_device_avoids_av1() {
        let c = caps(&[], &["h264", "av1"], &[], 60);
        let (codec, list) = pick_codec(&v(&["av1", "h264"]), "auto", &c);
        assert_eq!(codec, "h264");
        assert_eq!(list, v(&["h264"]));
    }

    #[test]
    fn unknown_caps_keep_old_choice_minus_av1() {
        let c = DecoderCaps::default();
        assert_eq!(pick_codec(&v(&["av1", "h265"]), "auto", &c), ("h265".into(), v(&["h265"])));
        assert_eq!(pick_codec(&v(&["h265", "h264"]), "h264", &c), ("h264".into(), v(&["h264"])));
        assert_eq!(pick_codec(&v(&["av1"]), "auto", &c), ("av1".into(), v(&["av1"])));
        assert_eq!(pick_codec(&[], "auto", &c), ("h264".into(), v(&["h264"])));
    }

    #[test]
    fn names_are_normalized() {
        let c = caps(&["h265"], &["h265"], &[], 60);
        assert_eq!(pick_codec(&v(&["hevc"]), "auto", &c).0, "h265");
    }

    #[test]
    fn fps_follows_decoder_not_a_fixed_60() {
        let c = caps(&["h265"], &["h265"], &[("h265", 120)], 144);
        assert_eq!(effective_fps(120, "h265", &c), 120);
        assert_eq!(effective_fps(144, "h265", &c), 120);
        assert_eq!(effective_fps(0, "h265", &c), 120); // unlimited -> panel 144, decoder 120
        assert_eq!(effective_fps(30, "h265", &c), 30);
    }

    #[test]
    fn fps_unknown_decoder_caps_at_panel() {
        let c = caps(&["h264"], &["h264"], &[], 120);
        assert_eq!(effective_fps(120, "h264", &c), 120);
        assert_eq!(effective_fps(240, "h264", &c), 120);
        let none = DecoderCaps::default();
        assert_eq!(effective_fps(0, "h264", &none), 60);
        assert_eq!(effective_fps(120, "h264", &none), 60);
        assert_eq!(effective_fps(30, "h264", &none), 30);
    }
}
