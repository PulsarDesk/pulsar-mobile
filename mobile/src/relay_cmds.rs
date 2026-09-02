//! Local relay server — run the rendezvous/relay on THIS device.
//!
//! The relay crate (`pulsar_relay`) is portable (tokio UDP, no host-only deps),
//! so it embeds fine on Android. Starting it here lets the phone act as a relay
//! for the whole LAN: other devices set their relay to this phone's `ip:port` and
//! get IDs + fall-back transport from it (self-hosted rendezvous, per the product
//! design — "identity distribution + fallback transport are fully yours").
//!
//! Commands (app-level, invoked by bare name from JS — NOT plugin commands, so no
//! ACL/build.rs entry needed):
//!   - `start_local_relay { port? }` → RelayStatus   (idempotent; default port 21116)
//!   - `stop_local_relay`           → RelayStatus
//!   - `local_relay_status`         → RelayStatus
//!   - `generate_relay_totp`        → TotpEnrollment (2FA enrollment: QR + manual code)
//!
//! The running task is held in [`LocalRelay`] managed state and stopped by
//! aborting it (which drops the UDP socket).

use std::net::SocketAddr;

use serde::Serialize;
use tauri::async_runtime::{self, JoinHandle};
use tauri::{AppHandle, Manager, Runtime};
use tokio::sync::Mutex;

/// Default relay UDP port — mirrors the design's `:21116`
/// (`pulsar_core::proto::DEFAULT_RELAY_PORT`).
const DEFAULT_RELAY_PORT: u16 = 21116;

/// The running local-relay task (if any) + its bound port. `None` = stopped.
#[derive(Default)]
pub struct LocalRelay(pub Mutex<Option<RelayTask>>);

pub struct RelayTask {
    handle: JoinHandle<()>,
    port: u16,
}

/// Reported to the UI: whether the relay is up + the address other devices use.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayStatus {
    pub running: bool,
    /// LAN IP peers use to reach this relay (`""` if unknown / not running).
    pub ip: String,
    /// Bound UDP port (`0` when not running).
    pub port: u16,
}

impl RelayStatus {
    fn stopped() -> Self {
        Self { running: false, ip: String::new(), port: 0 }
    }
}

/// Best-effort LAN IP of this device (same trick as `client::local_ip`): open a
/// UDP socket "toward" a public address — no packets are sent — and read the
/// kernel-chosen source IP.
async fn lan_ip() -> String {
    let sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    if sock.connect("8.8.8.8:80").await.is_err() {
        return String::new();
    }
    sock.local_addr().map(|a| a.ip().to_string()).unwrap_or_default()
}

/// Start the local relay. Idempotent: if already running, returns the live status.
/// `port` of `0`/`None` → [`DEFAULT_RELAY_PORT`].
/// JS: `invoke('start_local_relay', { port })` → [`RelayStatus`].
#[tauri::command]
pub async fn start_local_relay<R: Runtime>(
    app: AppHandle<R>,
    port: Option<u16>,
    // v4: optional auth policy for the relay THIS phone hosts. All default to none/off, so
    // an unconfigured local relay stays open exactly as before.
    password: Option<String>,
    totp: Option<String>,
    require_e2e: Option<bool>,
) -> Result<RelayStatus, String> {
    let state = app.state::<LocalRelay>();
    let mut guard = state.0.lock().await;

    // Already running → just report it (don't double-bind the port).
    if let Some(t) = guard.as_ref() {
        return Ok(RelayStatus { running: true, ip: lan_ip().await, port: t.port });
    }

    let p = port.filter(|p| *p != 0).unwrap_or(DEFAULT_RELAY_PORT);
    let addr = SocketAddr::from(([0, 0, 0, 0], p));
    // Build the optional auth policy. A base32 2FA secret is decoded up front so a typo
    // fails here instead of silently rejecting every login.
    let totp_secret = match totp.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(
            pulsar_relay::base32_decode(s)
                .filter(|b| !b.is_empty())
                .ok_or_else(|| "2FA secret geçersiz (base32 olmalı)".to_string())?,
        ),
        None => None,
    };
    let auth = pulsar_relay::RelayAuth {
        password: password.filter(|s| !s.is_empty()),
        totp_secret,
        require_e2e: require_e2e.unwrap_or(false),
    };
    let relay = pulsar_relay::Relay::bind(addr)
        .await
        .map_err(|e| format!("relay bind {addr} failed: {e}"))?
        .with_auth(auth);
    // Use the actually-bound port (matters if the caller passed 0 explicitly via
    // a future "ephemeral" path; with our default it equals `p`).
    let bound = relay.local_addr().map(|a| a.port()).unwrap_or(p);

    let handle = async_runtime::spawn(async move {
        if let Err(e) = relay.run().await {
            log::warn!("[relay] local relay loop exited: {e}");
        }
    });

    *guard = Some(RelayTask { handle, port: bound });
    log::info!("[relay] local relay listening on 0.0.0.0:{bound}");
    Ok(RelayStatus { running: true, ip: lan_ip().await, port: bound })
}

/// Stop the local relay (aborts the task → drops the socket). No-op if not running.
/// JS: `invoke('stop_local_relay')` → [`RelayStatus`].
#[tauri::command]
pub async fn stop_local_relay<R: Runtime>(app: AppHandle<R>) -> Result<RelayStatus, String> {
    let state = app.state::<LocalRelay>();
    let mut guard = state.0.lock().await;
    if let Some(t) = guard.take() {
        t.handle.abort();
        log::info!("[relay] local relay stopped");
    }
    Ok(RelayStatus::stopped())
}

/// A 2FA enrollment for the relay this phone hosts: what to scan, what to type when a
/// QR can't be scanned, and the secret the relay must run with.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpEnrollment {
    /// Base32 shared secret — what `start_local_relay`'s `totp` argument takes.
    pub secret: String,
    /// The same secret in 4-character groups, for manual entry into an authenticator.
    pub secret_grouped: String,
    /// `otpauth://totp/…` URI (what the QR encodes).
    pub uri: String,
    /// The URI rendered as a scannable QR — a complete inline SVG document.
    pub qr_svg: String,
}

/// Mint (or re-display) a TOTP enrollment for the phone-hosted relay.
///
/// With a non-empty `existing` base32 secret this re-renders THAT secret's QR, so a user
/// who lost the code can recover it without invalidating devices already enrolled;
/// otherwise a fresh 160-bit secret is minted. Nothing is stored here — the caller decides
/// whether to start the relay with the returned secret.
///
/// JS: `invoke('generate_relay_totp', { label, existing })` → [`TotpEnrollment`].
#[tauri::command]
pub async fn generate_relay_totp(
    label: Option<String>,
    existing: Option<String>,
) -> Result<TotpEnrollment, String> {
    // Label the authenticator entry with the address peers actually dial, so a user
    // hosting relays on several devices can tell the entries apart.
    let label = match label.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(l) => l,
        None => match lan_ip().await {
            ip if !ip.is_empty() => format!("{ip}:{DEFAULT_RELAY_PORT}"),
            _ => "Pulsar relay".to_string(),
        },
    };
    let e = match existing.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            pulsar_relay::base32_decode(s)
                .filter(|b| !b.is_empty())
                .ok_or_else(|| "2FA secret geçersiz (base32 olmalı)".to_string())?;
            pulsar_relay::enrollment_from_secret(s, &label, "Pulsar")
        }
        None => pulsar_relay::generate_totp(&label, "Pulsar"),
    };
    Ok(TotpEnrollment {
        secret: e.secret,
        secret_grouped: e.secret_grouped,
        uri: e.uri,
        qr_svg: e.qr_svg,
    })
}

/// Current local-relay status (for the Settings toggle to reflect on mount).
/// JS: `invoke('local_relay_status')` → [`RelayStatus`].
#[tauri::command]
pub async fn local_relay_status<R: Runtime>(app: AppHandle<R>) -> Result<RelayStatus, String> {
    let state = app.state::<LocalRelay>();
    let guard = state.0.lock().await;
    match guard.as_ref() {
        Some(t) => Ok(RelayStatus { running: true, ip: lan_ip().await, port: t.port }),
        None => Ok(RelayStatus::stopped()),
    }
}
