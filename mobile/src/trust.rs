//! Peer-identity trust for the mobile app — the desktop app's three trust files, same
//! names, same JSON shapes, same semantics (`pulsar-desktop/src-tauri/src/{util,pairing,
//! device_creds}.rs`):
//!
//! - `known_peers.json` (client, TOFU): `{"<relay>/<id>": "<hex pubkey>"}` — the static
//!   X25519 key first seen behind a relay id. The relay maps pubkey → id but never proves
//!   WHICH key owns an id to the requester, so a malicious / compromised relay (or a peer that
//!   won the id registration race) could answer in a known device's place with its own key.
//!   A later connect passes the pinned key to `Node::connect_pinned`, which refuses an answer
//!   with a different key (`ConnError::IdentityChanged`). A pin is NEVER overwritten
//!   silently: only the user's explicit "trust the new key" (`forget_peer`) removes it, and
//!   the next connect pins again.
//! - `paired_devices.json` (host): the clients this phone trusts by their static key — a
//!   paired key skips the one-time password and the Allow/Deny sheet.
//! - `device_creds.json` (client): per target, whether a host accepted our `PairRequest`.
//!   The desktop also keeps a sealed connect password there; the mobile app saves none, so
//!   `enc` / `pw` stay empty (kept so the file reads the same on both apps).
//!
//! All three live in `<app_data_dir>` next to `identity.key` and `config.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};

use pulsar_core::proto::{DeviceId, PublicKey};

use crate::config::load_config;

fn data_file<R: Runtime>(app: &AppHandle<R>, name: &str) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(name)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write `json` to a sibling `.tmp` file, then rename it over `path`, so a crash mid-write
/// can't leave a truncated file that silently resets every pin / pairing.
fn atomic_write_json(path: &Path, json: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        if let Err(e) = std::fs::rename(&tmp, path) {
            log::warn!("pulsar trust: rename failed, falling back to direct write: {e}");
            let _ = std::fs::write(path, json);
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Hex form of a public key (lowercase, 64 chars).
pub(crate) fn key_hex(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_key(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(k)
}

/// Short grouped fingerprint for display: the first 16 hex chars in groups of 4.
pub(crate) fn fingerprint(hex: &str) -> String {
    hex.get(..16)
        .unwrap_or(hex)
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The relay a command acts on: the caller's, else the saved one (what `connect_host` uses).
pub(crate) fn relay_or_config<R: Runtime>(app: &AppHandle<R>, relay: &str) -> String {
    if relay.trim().is_empty() {
        load_config(app).relay
    } else {
        relay.to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// known_peers.json — client-side TOFU pins
// ─────────────────────────────────────────────────────────────────────────────

/// Serializes read-modify-write of `known_peers.json` (two first-connects at once must not
/// clobber each other's pin).
static PEERS_LOCK: Mutex<()> = Mutex::new(());

/// The map key: ids are minted per relay, so the same 9-digit id is a different device on
/// another relay — a pin is scoped to `(relay, id)` (desktop `peer_key_entry`).
fn peer_key_entry(relay: &str, id: &DeviceId) -> String {
    format!("{}/{}", relay.trim().to_ascii_lowercase(), id.0)
}

fn load_pins(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn pinned_in(path: &Path, relay: &str, id: &DeviceId) -> Option<PublicKey> {
    parse_key(load_pins(path).get(&peer_key_entry(relay, id))?)
}

/// Pin `key` for `(relay, id)` unless a pin already exists. Never overwrites: a changed
/// key is rejected by `connect_pinned` before this runs, and only `forget_peer` clears a pin.
fn pin_in(path: &Path, relay: &str, id: &DeviceId, key: &PublicKey) -> bool {
    let _g = PEERS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut map = load_pins(path);
    let entry = peer_key_entry(relay, id);
    if map.contains_key(&entry) {
        return false;
    }
    map.insert(entry, key_hex(key));
    if let Ok(json) = serde_json::to_string(&map) {
        atomic_write_json(path, &json);
    }
    true
}

fn forget_in(path: &Path, relay: &str, id: &DeviceId) -> bool {
    let _g = PEERS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut map = load_pins(path);
    if map.remove(&peer_key_entry(relay, id)).is_none() {
        return false;
    }
    if let Ok(json) = serde_json::to_string(&map) {
        atomic_write_json(path, &json);
    }
    true
}

fn known_peers_path<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    data_file(app, "known_peers.json")
}

/// The key pinned for `(relay, id)` on an earlier connect, if any.
pub(crate) fn known_peer_key<R: Runtime>(app: &AppHandle<R>, relay: &str, id: &DeviceId) -> Option<PublicKey> {
    let _g = PEERS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    pinned_in(&known_peers_path(app), relay, id)
}

/// Record the key first seen behind `(relay, id)` (TOFU). No-op when already pinned.
pub(crate) fn pin_peer_key<R: Runtime>(app: &AppHandle<R>, relay: &str, id: &DeviceId, key: &PublicKey) {
    if pin_in(&known_peers_path(app), relay, id, key) {
        log::info!("pulsar trust: pinned host key for {} ({})", id.grouped(), fingerprint(&key_hex(key)));
    }
}

/// The error string a connect returns for a pinned-key mismatch: `IDENTITY_CHANGED:` + the
/// grouped id (the desktop's sentinel), so the UI can show the blocking warning.
pub(crate) fn identity_changed_err(id: &DeviceId) -> String {
    format!("IDENTITY_CHANGED:{}", id.grouped())
}

/// The user explicitly chose "trust the new key" on the identity-changed warning: drop the
/// pin for `(relay, id)`, so the next connect pins whatever key answers. `relay` "" = the
/// saved relay. JS: `invoke('forget_peer', { relay, id })`.
#[tauri::command]
pub fn forget_peer<R: Runtime>(app: AppHandle<R>, relay: String, id: String) -> Result<bool, String> {
    let device_id = DeviceId::parse(&id).ok_or_else(|| format!("bad device id: {id}"))?;
    let relay = relay_or_config(&app, &relay);
    let removed = forget_in(&known_peers_path(&app), &relay, &device_id);
    log::warn!("pulsar trust: user accepted a NEW host key for {} (pin removed: {removed})", device_id.grouped());
    Ok(removed)
}

// ─────────────────────────────────────────────────────────────────────────────
// paired_devices.json — host-side trust store (desktop `pairing::TrustStore`)
// ─────────────────────────────────────────────────────────────────────────────

/// One trusted client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PairedDevice {
    /// The client's static X25519 public key, lowercase hex (64 chars).
    pub key: String,
    /// The client's display name at pairing time.
    #[serde(default)]
    pub name: String,
    /// The peer label at pairing time (relay id or a direct address).
    #[serde(default)]
    pub id: String,
    /// Epoch ms.
    #[serde(default)]
    pub paired_at: u64,
    /// Epoch ms of the last connection the pairing let in (0 = never since pairing).
    #[serde(default)]
    pub last_seen: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TrustStore {
    #[serde(default)]
    pub devices: Vec<PairedDevice>,
}

/// Longest name / label kept per entry (a client chooses its name).
const NAME_MAX: usize = 64;
/// Most paired devices kept; pairing one more drops the least recently seen.
const MAX_DEVICES: usize = 64;

fn clip(s: &str) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(NAME_MAX).collect()
}

impl TrustStore {
    /// A missing or corrupt file is an empty store (nobody trusted).
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            atomic_write_json(path, &json);
        }
    }

    pub fn is_trusted(&self, key: &[u8; 32]) -> bool {
        let k = key_hex(key);
        self.devices.iter().any(|d| d.key == k)
    }

    /// Trust `key` (or refresh its name / label if it already is). True when it was new.
    pub fn add(&mut self, key: &[u8; 32], name: &str, id: &str, now_ms: u64) -> bool {
        let k = key_hex(key);
        if let Some(d) = self.devices.iter_mut().find(|d| d.key == k) {
            if !name.trim().is_empty() {
                d.name = clip(name);
            }
            d.id = clip(id);
            return false;
        }
        if self.devices.len() >= MAX_DEVICES {
            if let Some(i) = self
                .devices
                .iter()
                .enumerate()
                .min_by_key(|(_, d)| d.last_seen.max(d.paired_at))
                .map(|(i, _)| i)
            {
                self.devices.remove(i);
            }
        }
        self.devices.push(PairedDevice { key: k, name: clip(name), id: clip(id), paired_at: now_ms, last_seen: 0 });
        true
    }

    /// Stop trusting the key given as hex (case-insensitive). True when an entry was removed.
    pub fn remove_hex(&mut self, key_hex: &str) -> bool {
        let k = key_hex.trim().to_ascii_lowercase();
        let before = self.devices.len();
        self.devices.retain(|d| d.key != k);
        self.devices.len() != before
    }

    pub fn touch(&mut self, key: &[u8; 32], now_ms: u64) -> bool {
        let k = key_hex(key);
        match self.devices.iter_mut().find(|d| d.key == k) {
            Some(d) => {
                d.last_seen = now_ms;
                true
            }
            None => false,
        }
    }
}

/// What let an accepted session in — drives the pairing decision (desktop `AuthVia`; the
/// phone host has no same-device pre-auth and no host 2FA).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthVia {
    Unattended,
    Paired,
    /// The one-time password or the standing connect password.
    Password,
    /// The phone's user tapped Allow on the approval sheet.
    Operator,
}

/// How the host answers a `PairRequest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairDecision {
    /// Trust the key now: this session proved a password (or is already paired).
    Accept,
    /// Ask the phone's user on a pairing sheet first.
    AskOperator,
}

/// Pairing is never silent: Allow on the CONNECT sheet did not consent to permanent trust,
/// and unattended access proved nothing in this session.
pub(crate) fn pair_decision(via: AuthVia) -> PairDecision {
    match via {
        AuthVia::Password | AuthVia::Paired => PairDecision::Accept,
        AuthVia::Operator | AuthVia::Unattended => PairDecision::AskOperator,
    }
}

static STORE_LOCK: Mutex<()> = Mutex::new(());

fn store_path<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    data_file(app, "paired_devices.json")
}

/// Is this client key paired with this phone?
pub(crate) fn is_paired<R: Runtime>(app: &AppHandle<R>, key: &[u8; 32]) -> bool {
    let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    TrustStore::load(&store_path(app)).is_trusted(key)
}

/// Stamp `last_seen` for a connection the pairing let in.
pub(crate) fn touch_paired<R: Runtime>(app: &AppHandle<R>, key: &[u8; 32]) {
    let path = store_path(app);
    let changed = {
        let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = TrustStore::load(&path);
        let changed = s.touch(key, now_ms());
        if changed {
            s.save(&path);
        }
        changed
    };
    if changed {
        let _ = app.emit("paired-devices-changed", ());
    }
}

pub(crate) fn add_paired<R: Runtime>(app: &AppHandle<R>, key: &[u8; 32], name: &str, id: &str) {
    let path = store_path(app);
    {
        let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = TrustStore::load(&path);
        s.add(key, name, id, now_ms());
        s.save(&path);
    }
    log::info!("pulsar host: pairing — client key trusted ({id}, {})", fingerprint(&key_hex(key)));
    let _ = app.emit("paired-devices-changed", ());
}

pub(crate) fn remove_paired<R: Runtime>(app: &AppHandle<R>, key_hex: &str) -> bool {
    let path = store_path(app);
    let removed = {
        let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = TrustStore::load(&path);
        let removed = s.remove_hex(key_hex);
        if removed {
            s.save(&path);
        }
        removed
    };
    if removed {
        log::info!("pulsar host: pairing — client key revoked");
        let _ = app.emit("paired-devices-changed", ());
    }
    removed
}

/// Settings → Paired devices row.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedView {
    key: String,
    fingerprint: String,
    name: String,
    id: String,
    paired_at: u64,
    last_seen: u64,
}

/// JS: `invoke('list_paired_devices')` → `PairedView[]`.
#[tauri::command]
pub fn list_paired_devices<R: Runtime>(app: AppHandle<R>) -> Vec<PairedView> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    TrustStore::load(&store_path(&app))
        .devices
        .into_iter()
        .map(|d| PairedView {
            fingerprint: fingerprint(&d.key),
            key: d.key,
            name: d.name,
            id: d.id,
            paired_at: d.paired_at,
            last_seen: d.last_seen,
        })
        .collect()
}

/// JS: `invoke('revoke_paired_device', { key })` → whether an entry was removed.
#[tauri::command]
pub fn revoke_paired_device<R: Runtime>(app: AppHandle<R>, key: String) -> bool {
    remove_paired(&app, &key)
}

// ─────────────────────────────────────────────────────────────────────────────
// device_creds.json — client-side "the host accepted our pairing" flags
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CredEntry {
    /// Desktop: how `pw` is encoded. Always empty here (the mobile app saves no password).
    #[serde(default)]
    pub enc: String,
    #[serde(default)]
    pub pw: String,
    /// The host accepted our `PairRequest` (cleared on unpair).
    #[serde(default)]
    pub paired: bool,
}

static CREDS_LOCK: Mutex<()> = Mutex::new(());

/// Store key for a connect target: relay ids are scoped to the relay, addresses stand alone
/// (desktop `device_creds::entry_key`).
pub(crate) fn cred_entry_key(relay: &str, target: &str) -> String {
    let despaced: String = target.chars().filter(|c| !c.is_whitespace()).collect();
    if despaced.len() == 9 && despaced.chars().all(|c| c.is_ascii_digit()) {
        format!("{}/{}", relay.trim().to_ascii_lowercase(), despaced)
    } else {
        format!("addr/{}", target.trim().to_ascii_lowercase())
    }
}

fn creds_path<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    data_file(app, "device_creds.json")
}

fn load_creds(path: &Path) -> HashMap<String, CredEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Record whether the host behind `(relay, target)` has paired us. An entry with nothing
/// left in it is dropped.
pub(crate) fn set_paired_flag<R: Runtime>(app: &AppHandle<R>, relay: &str, target: &str, paired: bool) {
    let path = creds_path(app);
    let key = cred_entry_key(relay, target);
    let _g = CREDS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut map = load_creds(&path);
    let e = map.entry(key.clone()).or_default();
    e.paired = paired;
    if *e == CredEntry::default() {
        map.remove(&key);
    }
    if let Ok(json) = serde_json::to_string(&map) {
        atomic_write_json(&path, &json);
    }
}

/// What the Devices UI shows for one device (desktop `CredInfo`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredInfo {
    has_password: bool,
    paired: bool,
}

/// Pairing badges for several devices at once. JS: `invoke('device_creds', { relay, targets })`.
#[tauri::command]
pub fn device_creds<R: Runtime>(app: AppHandle<R>, relay: String, targets: Vec<String>) -> HashMap<String, CredInfo> {
    let relay = relay_or_config(&app, &relay);
    let map = {
        let _g = CREDS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        load_creds(&creds_path(&app))
    };
    targets
        .into_iter()
        .map(|t| {
            let e = map.get(&cred_entry_key(&relay, &t));
            (t, CredInfo { has_password: e.is_some_and(|e| !e.pw.is_empty()), paired: e.is_some_and(|e| e.paired) })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pulsar-mobile-trust-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("f.json")
    }

    const A: [u8; 32] = [0xAB; 32];
    const B: [u8; 32] = [0x01; 32];

    #[test]
    fn pin_is_first_use_and_never_overwritten() {
        let p = tmp("pin");
        let id = DeviceId(482_913_056);
        assert_eq!(pinned_in(&p, "relay:1", &id), None, "no file = nothing pinned");
        assert!(pin_in(&p, "Relay:1 ", &id, &A));
        assert_eq!(pinned_in(&p, "relay:1", &id), Some(A), "relay key is trimmed + lowercased");
        assert!(!pin_in(&p, "relay:1", &id, &B), "a changed key must not replace the pin");
        assert_eq!(pinned_in(&p, "relay:1", &id), Some(A));
        assert_eq!(pinned_in(&p, "other:1", &id), None, "pins are scoped per relay");
        assert!(forget_in(&p, "relay:1", &id));
        assert!(!forget_in(&p, "relay:1", &id));
        assert!(pin_in(&p, "relay:1", &id, &B), "after an explicit forget the next key pins");
        assert_eq!(pinned_in(&p, "relay:1", &id), Some(B));
    }

    #[test]
    fn pin_file_matches_desktop_format() {
        let p = tmp("fmt");
        let id = DeviceId(641_724_395);
        pin_in(&p, "relay.pulsardesk.com:21116", &id, &A);
        let map: HashMap<String, String> = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(map.get("relay.pulsardesk.com:21116/641724395"), Some(&"ab".repeat(32)));
    }

    #[test]
    fn corrupt_pin_file_pins_nothing() {
        let p = tmp("corrupt-pin");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{nope").unwrap();
        assert_eq!(pinned_in(&p, "r", &DeviceId(123_456_789)), None);
    }

    #[test]
    fn store_add_persist_revoke() {
        let p = tmp("store");
        let mut s = TrustStore::load(&p);
        assert!(!s.is_trusted(&A));
        assert!(s.add(&A, "Laptop", "482 913 056", 10));
        assert!(!s.add(&A, "Laptop 2", "482 913 056", 20), "re-pair refreshes, not duplicates");
        assert_eq!(s.devices.len(), 1);
        assert_eq!(s.devices[0].paired_at, 10);
        s.save(&p);
        let mut back = TrustStore::load(&p);
        assert!(back.is_trusted(&A));
        assert!(!back.is_trusted(&B));
        assert!(back.touch(&A, 99));
        assert!(back.remove_hex(&key_hex(&A).to_uppercase()));
        assert!(!back.remove_hex(&key_hex(&A)));
        back.save(&p);
        assert!(!TrustStore::load(&p).is_trusted(&A), "revocation persists");
    }

    #[test]
    fn store_caps_and_sanitizes() {
        let mut s = TrustStore::default();
        s.add(&A, &format!("  x\u{7}{}", "n".repeat(200)), "id", 1);
        assert_eq!(s.devices[0].name.chars().count(), NAME_MAX);
        assert!(!s.devices[0].name.contains('\u{7}'));
        for i in 0..MAX_DEVICES as u8 + 3 {
            s.add(&[i.wrapping_add(2); 32], "d", "id", 100 + i as u64);
        }
        assert_eq!(s.devices.len(), MAX_DEVICES);
    }

    #[test]
    fn pairing_is_never_silent() {
        assert_eq!(pair_decision(AuthVia::Password), PairDecision::Accept);
        assert_eq!(pair_decision(AuthVia::Paired), PairDecision::Accept);
        assert_eq!(pair_decision(AuthVia::Operator), PairDecision::AskOperator);
        assert_eq!(pair_decision(AuthVia::Unattended), PairDecision::AskOperator);
    }

    #[test]
    fn cred_keys_scope_ids_per_relay() {
        assert_eq!(cred_entry_key("Relay.Example:21116 ", "482 913 056"), "relay.example:21116/482913056");
        assert_ne!(cred_entry_key("a:1", "482913056"), cred_entry_key("b:1", "482913056"));
        assert_eq!(cred_entry_key("a:1", "192.168.1.5:9000"), cred_entry_key("b:1", " 192.168.1.5:9000"));
    }

    #[test]
    fn parse_key_and_fingerprint() {
        assert_eq!(parse_key(&key_hex(&A)), Some(A));
        assert_eq!(parse_key("abc"), None);
        assert_eq!(fingerprint(&key_hex(&A)), "abab abab abab abab");
    }
}
