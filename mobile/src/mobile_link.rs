//! Does this phone's route to the host run over a cellular (or other phone-shared) link?
//!
//! The mobile twin of the desktop's `mobile_link.rs`: a phone link reads its burst capacity in
//! the 1 s session-start probe and stalls for seconds on a handover, so the adaptive controller
//! starts it lower and climbs it more gently (`pulsar_core::adapt::Config::mobile`).
//!
//! Android asks `ConnectivityManager` for the active network (`networkKind` in the video
//! plugin): a cellular transport, or a metered Wi-Fi (another phone's hotspot — the desktop's
//! "Wi-Fi tether" case), is a phone link. Behind a VPN (ZeroTier, WireGuard, ...) the metered
//! flag describes the VPN, not the link under it, so only the cellular transport counts there.
//! Best effort, once per session start (the desktop does the same), never an error: an
//! unknown network is not a phone link.

use tauri::{AppHandle, Runtime};

/// What the route runs over. `Mobile` carries the reason (for the log).
#[cfg_attr(not(mobile), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Mobile(&'static str),
    Fixed,
    Unknown,
}

impl LinkKind {
    pub fn is_mobile(&self) -> bool {
        matches!(self, LinkKind::Mobile(_))
    }

    pub fn label(&self) -> &'static str {
        match self {
            LinkKind::Mobile(why) => why,
            LinkKind::Fixed => "fixed",
            LinkKind::Unknown => "unknown",
        }
    }
}

/// Classify the plugin's answer (split out for tests).
#[cfg_attr(not(mobile), allow(dead_code))]
pub fn classify(k: &tauri_plugin_pulsar_video::NetworkKind) -> LinkKind {
    if !k.known {
        LinkKind::Unknown
    } else if k.cellular {
        LinkKind::Mobile("cellular")
    } else if k.metered && !k.vpn {
        LinkKind::Mobile("metered")
    } else {
        LinkKind::Fixed
    }
}

/// Ask the platform (a blocking JNI hop on Android: call it off the async workers).
pub fn detect<R: Runtime>(app: &AppHandle<R>) -> LinkKind {
    #[cfg(mobile)]
    {
        use tauri_plugin_pulsar_video::{PulsarVideoExt as _, PulsarVideoW5Ext as _};
        match app.pulsar_video().network_kind() {
            Ok(k) => return classify(&k),
            Err(e) => log::warn!("pulsar: network kind probe failed: {e:?}"),
        }
    }
    let _ = app;
    LinkKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri_plugin_pulsar_video::NetworkKind;

    fn k(cellular: bool, wifi: bool, vpn: bool, metered: bool) -> NetworkKind {
        NetworkKind { known: true, cellular, wifi, vpn, metered }
    }

    #[test]
    fn cellular_is_mobile_even_behind_a_vpn() {
        assert!(classify(&k(true, false, false, true)).is_mobile());
        assert!(classify(&k(true, false, true, true)).is_mobile());
    }

    #[test]
    fn metered_wifi_is_a_hotspot() {
        assert_eq!(classify(&k(false, true, false, true)), LinkKind::Mobile("metered"));
        assert_eq!(classify(&k(false, true, false, false)), LinkKind::Fixed);
    }

    #[test]
    fn a_metered_vpn_alone_says_nothing() {
        assert_eq!(classify(&k(false, true, true, true)), LinkKind::Fixed);
    }

    #[test]
    fn unknown_is_not_mobile() {
        assert_eq!(classify(&NetworkKind::default()), LinkKind::Unknown);
    }
}
