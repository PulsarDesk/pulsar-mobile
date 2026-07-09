//! Shared relay [`Node`] used by BOTH roles: client (outgoing connects, see
//! [`crate::client::connect_host`]) and host (incoming accepts, see
//! [`crate::host::go_online`]).
//!
//! The phone is simultaneously a client and a host, and both roles derive the
//! SAME relay identity. Binding a SEPARATE `Node` per role (the previous design)
//! registered TWO UDP sockets under one device id, so the relay's `dev.addr`
//! flapped between them on every heartbeat and an INBOUND connection (another
//! device → this phone) was routed to whichever node registered last — usually a
//! client node, which has no accept loop. The host therefore never saw the
//! request: no approval popup, no session. (Verified live: the relay logged two
//! ports for one id and forwarded `Incoming` to the client node's port.)
//!
//! One shared `Arc<Node>` fixes this: a SINGLE registration → a single heartbeat
//! → a single relay address, so `next_incoming()` (host) and `connect()` (client)
//! share it and inbound connections always reach the accept loop. `Node::register`
//! spawns the heartbeat keyed on a weak ref, so the node stays alive exactly as
//! long as this `Arc` is held in managed state.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tauri::{AppHandle, Manager, Runtime};
use tokio::sync::Mutex;

use pulsar_core::proto::DeviceId;
use pulsar_core::{Discovery, NetworkMode, Node};

use crate::identity::load_identity;

/// Parse a user-entered relay address, defaulting to the standard relay port when
/// the input has none (e.g. just a hostname/IP) — so typing "192.168.1.50" works
/// without also typing ":21116". The stored/displayed config value is untouched;
/// only the address actually used to connect gets the default port.
///
/// Resolves DNS names, not just numeric IPs: `str::parse::<SocketAddr>()` only
/// accepts a literal IP, so a hostname like `vps.example.com:21121` would `Err`
/// and the node never reached the relay (IP worked, name didn't). We fall back to
/// `lookup_host` and prefer IPv4 — the relay binds `0.0.0.0`, and a name that
/// resolves to `::1`/AAAA first would never reach an IPv4-only relay.
pub(crate) async fn parse_relay(s: &str) -> Result<SocketAddr, String> {
    let s = s.trim();
    let with_port = if s.contains(':') {
        s.to_string()
    } else {
        format!("{s}:{}", pulsar_core::proto::DEFAULT_RELAY_PORT)
    };
    // Numeric IP:port — no resolver needed.
    if let Ok(parsed) = with_port.parse::<SocketAddr>() {
        return Ok(parsed);
    }
    // Hostname → DNS.
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host(&with_port)
        .await
        .map_err(|e| format!("bad relay: {e}"))?
        .collect();
    resolved
        .iter()
        .copied()
        .find(SocketAddr::is_ipv4)
        .or_else(|| resolved.first().copied())
        .ok_or_else(|| format!("bad relay: {with_port} çözülemedi"))
}

/// Resolve a direct-connect target — an `IP`, `IP:port`, `host`, or `host:port`
/// (a bare address gets the default node port) — to a socket address. Mirrors the
/// desktop `connect_target` flow: a bare LAN IP works WITHOUT a port, and DNS names
/// resolve (IPv4 first — the host binds an IPv4 socket). Returns `None` for a
/// non-address (e.g. a 9-digit relay ID), so the caller falls through to
/// `DeviceId::parse`. Previously the callers used `target.parse::<SocketAddr>()`,
/// which requires a literal `IP:port` — a bare IP or a hostname both `Err`'d and
/// the connect died as "bad target".
pub(crate) async fn resolve_target(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    // Numeric fast paths: IP:port, then bare IP (default node port).
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return Some(sa);
    }
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, pulsar_core::proto::DEFAULT_NODE_PORT));
    }
    // A bare relay ID (9 digits) has neither a dot nor a colon — skip DNS so a
    // relay-ID connect never blocks on a doomed lookup.
    if !s.contains('.') && !s.contains(':') {
        return None;
    }
    let with_port = if s.contains(':') {
        s.to_string()
    } else {
        format!("{s}:{}", pulsar_core::proto::DEFAULT_NODE_PORT)
    };
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host(&with_port).await.ok()?.collect();
    resolved
        .iter()
        .copied()
        .find(SocketAddr::is_ipv4)
        .or_else(|| resolved.first().copied())
}

/// The app's single relay `Node`, created on first use (a client connect or
/// `go_online`) and reused by both roles thereafter. `None` until first use.
#[derive(Default)]
pub struct SharedNode(pub Mutex<Option<SharedNodeInner>>);

pub struct SharedNodeInner {
    pub node: Arc<Node>,
    pub my_id: DeviceId,
    /// The relay + network mode this node was bound to. A later caller requesting a
    /// DIFFERENT relay/mode (user edited Settings → Ağ) rebinds the node so the new
    /// setting takes effect this connect instead of only after an app restart. The
    /// core `Node`'s relay address is immutable once bound, so a change means a rebuild.
    pub relay_addr: SocketAddr,
    pub mode: NetworkMode,
}

/// Return the shared node, binding + registering it on the FIRST call. Later callers
/// reuse the existing node when they request the SAME relay + network mode (the hot
/// path — both roles read the same persisted `Config`, so they agree and the node is
/// shared); a caller requesting a DIFFERENT relay/mode rebinds a fresh node so a
/// Settings change applies without an app restart. The device name is still taken
/// from whoever binds. The async `Mutex` is held across `bind`/`register` so two
/// concurrent first-callers can't race two nodes into existence.
///
/// Note: rebinding replaces the strong `Arc` held here; the previous node then drops
/// (stopping its weak-ref-keyed heartbeat) once no live read/accept loop still holds
/// it. A device actively HOSTING should re-run `go_online` after a relay change so its
/// accept loop moves to the new node — a client connect alone rebinds only the shared
/// node, not the host's already-spawned accept loop.
pub(crate) async fn get_or_create_node<R: Runtime>(
    app: &AppHandle<R>,
    relay_addr: SocketAddr,
    mode: NetworkMode,
    dev_name: String,
) -> Result<(Arc<Node>, DeviceId), String> {
    let shared = app.state::<SharedNode>();
    let mut g = shared.0.lock().await;
    if let Some(inner) = g.as_mut() {
        if inner.relay_addr == relay_addr && inner.mode == mode {
            // Reusing a node that bound while the relay was down (offline, no id yet):
            // retry registration now — the relay may have come back — so a second
            // go_online upgrades offline → online WITHOUT a rebind.
            if inner.my_id.0 < DeviceId::MIN {
                if let Ok(id) = inner.node.register().await {
                    inner.my_id = id;
                }
            }
            return Ok((inner.node.clone(), inner.my_id));
        }
        log::info!(
            "pulsar: relay/mode changed ({} → {relay_addr}) — rebinding shared node",
            inner.relay_addr
        );
    }
    // Bind the user-configured node port (0 = ephemeral/random, the default).
    let port = crate::config::load_config(app).node_port;
    let local: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
    let identity = load_identity(app);
    let node = Node::bind_with_identity(local, relay_addr, mode, dev_name, identity)
        .await
        .map_err(|e| format!("bind failed: {e}"))?;
    // Register is BEST-EFFORT: if the relay is unreachable we still keep the node bound +
    // stored so the host accept loop serves LAN peers by IP:port, AND a client can reach
    // a DIRECT ip:port target (neither needs the relay). `DeviceId(0)` (< DeviceId::MIN)
    // marks "no relay id yet"; a later go_online retries registration (see the reuse path
    // above). Previously a register failure `?`-returned here and dropped the node, so a
    // relay outage killed even relay-less direct connects + hosting.
    let my_id = match node.register().await {
        Ok(id) => id,
        Err(e) => {
            log::info!("pulsar: relay unreachable ({e:?}) — offline mode, node still bound + serving");
            DeviceId(0)
        }
    };
    *g = Some(SharedNodeInner {
        node: node.clone(),
        my_id,
        relay_addr,
        mode,
    });
    Ok((node, my_id))
}

/// The app's single LAN-discovery beacon (multicast 239.255.71.21). Announces this
/// device and collects peers seen on the local network. Created on the first
/// `lan_devices` call and reused thereafter. `None` until first use.
///
/// Note: Android drops inbound multicast unless a `WifiManager.MulticastLock` is
/// held — `MainActivity` acquires one at startup so `recv_loop` actually sees
/// other devices' announces.
#[derive(Default)]
pub struct SharedDiscovery(pub Mutex<Option<Arc<Discovery>>>);

/// Start the discovery beacon on the first call, reuse it after. Started RECEIVE-ONLY
/// (announcing paused): the phone is a *discoverer* here (the connect screen's "on your
/// network" list + recents online dots), NOT a host. Merely opening the connect screen
/// must not advertise the phone on the LAN — without going online it can't accept a
/// connection, so announcing would make it show up (and look reachable) in other
/// devices' lists when it isn't. Receiving still runs, so the list keeps working. The
/// async `Mutex` serialises concurrent first-callers.
pub(crate) async fn get_or_create_discovery<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<Arc<Discovery>, String> {
    let shared = app.state::<SharedDiscovery>();
    let mut g = shared.0.lock().await;
    if let Some(d) = g.as_ref() {
        return Ok(d.clone());
    }
    let cfg = crate::config::load_config(app);
    let pubkey = load_identity(app).public_bytes();
    let disc = Discovery::start(cfg.device_name, 0, pubkey, None)
        .await
        .map_err(|e| format!("discovery start failed: {e}"))?;
    // Receive-only until (and unless) the phone actually goes online as a host.
    disc.set_paused(true);
    *g = Some(disc.clone());
    Ok(disc)
}

/// Advertise on the LAN as a HOST: (re)start the beacon ANNOUNCING (unpaused) with the
/// real bound `node_port` + our relay `id` (if registered), replacing the receive-only
/// list beacon. This is what makes the device discoverable + directly reachable — but
/// ONLY after it has gone online as a host, never merely from opening the connect screen.
pub(crate) async fn announce_as_host<R: Runtime>(
    app: &AppHandle<R>,
    node_port: u16,
    id: Option<DeviceId>,
) {
    let cfg = crate::config::load_config(app);
    let pubkey = load_identity(app).public_bytes();
    match Discovery::start(cfg.device_name, node_port, pubkey, id).await {
        Ok(disc) => {
            *app.state::<SharedDiscovery>().0.lock().await = Some(disc);
        }
        Err(e) => log::warn!("pulsar: host LAN beacon failed to start: {e}"),
    }
}

/// Go back to receive-only on the LAN (stop advertising) — called when the host goes
/// offline so the device stops appearing/being reachable to other devices.
pub(crate) async fn stop_announcing<R: Runtime>(app: &AppHandle<R>) {
    if let Some(d) = app.state::<SharedDiscovery>().0.lock().await.as_ref() {
        d.set_paused(true);
    }
}
