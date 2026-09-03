//! Per-peer adaptive-streaming memory (Phase 1.3, mobile twin of the desktop's): the last
//! rate a session to a peer ran cleanly at, so the next session starts near it instead of
//! probing from scratch. A tiny JSON map in the app data dir (`adapt-memory.json`).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};

#[derive(Serialize, Deserialize, Default, Clone)]
struct Entry {
    kbps: u32,
    at: u64,
}

fn path<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("adapt-memory.json")
}

fn load<R: Runtime>(app: &AppHandle<R>) -> HashMap<String, Entry> {
    std::fs::read(path(app))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The peer's last known-good rate (kbit/s), if remembered within the last 30 days.
pub(crate) fn hint<R: Runtime>(app: &AppHandle<R>, peer: &str) -> Option<u32> {
    let e = load(app).remove(peer)?;
    (e.kbps > 0 && now_secs().saturating_sub(e.at) < 30 * 24 * 3600).then_some(e.kbps)
}

/// Remember `kbps` as the peer's last known-good rate (overwrites; best effort).
pub(crate) fn remember<R: Runtime>(app: &AppHandle<R>, peer: &str, kbps: u32) {
    if kbps == 0 || peer.is_empty() {
        return;
    }
    let mut m = load(app);
    m.insert(peer.to_string(), Entry { kbps, at: now_secs() });
    let cutoff = now_secs().saturating_sub(90 * 24 * 3600);
    m.retain(|_, e| e.at >= cutoff);
    let p = path(app);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(&m) {
        let _ = std::fs::write(p, json);
    }
}
