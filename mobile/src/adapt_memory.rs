//! Per-peer adaptive-streaming memory (Phase 1.3, mobile twin of the desktop's): the last
//! rate a session to a peer ran cleanly at, so the next session starts near it instead of
//! probing from scratch. A tiny JSON map in the app data dir (`adapt-memory.json`).
//!
//! Keyed by peer AND path class (`lan` / `wan` / `relay`) like the desktop: the map, its keys
//! and the classification are shared (`pulsar_core::adapt::memory`). A rate learned on the home
//! Wi-Fi no longer seeds a cellular session to the same host. Entries of the old peer-only
//! format are ignored and dropped on the next write.

use std::path::PathBuf;

use tauri::{AppHandle, Manager, Runtime};

pub(crate) use pulsar_core::adapt::memory::PathClass;

fn path<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("adapt-memory.json")
}

/// The peer's last known-good rate (kbit/s) on this kind of path, if remembered within the
/// last 30 days.
pub(crate) fn hint<R: Runtime>(app: &AppHandle<R>, peer: &str, class: PathClass) -> Option<u32> {
    pulsar_core::adapt::memory::hint(&path(app), peer, class)
}

/// Remember `kbps` as the peer's last known-good rate on this kind of path (overwrites; best
/// effort).
pub(crate) fn remember<R: Runtime>(app: &AppHandle<R>, peer: &str, class: PathClass, kbps: u32) {
    pulsar_core::adapt::memory::remember(&path(app), peer, class, kbps)
}
