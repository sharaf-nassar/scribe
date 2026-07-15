//! Physical-network trust gate for feature 014 (research D5, FR-018/SC-007).
//!
//! LAN discovery and the LAN listener are active only while this machine is on
//! a network the user has explicitly marked trusted. That gate is defense in
//! depth — its job is to stop accidental exposure on café Wi-Fi, not to
//! authenticate peers (real security is the device-approval + pinned-TLS layer).
//!
//! This module provides three things:
//!
//! 1. **Network fingerprinting** ([`current_network_fingerprint`]) — reads the
//!    default-gateway `MAC` address (primary anchor) plus the local subnet/CIDR
//!    (corroborator) of the **physical** LAN interface via `netdev`, never a
//!    VPN/tunnel default route. It fails closed (a typed
//!    [`NetworkFingerprintError`]) on a zero/unresolved gateway `MAC`, no
//!    default route, or a VPN-only default route, so the LAN surface stays
//!    dormant when the network cannot be confidently identified.
//! 2. **The trusted-networks store** ([`TrustedNetworksStore`]) — an
//!    add/remove/list store of structured [`TrustedNetwork`] records persisted
//!    under the server's per-user state directory, plus the pure match rule
//!    (equal non-zero gateway `MAC` **and** subnet) and
//!    [`TrustedNetworksStore::is_current_network_trusted`].
//! 3. **A network-change watcher** ([`spawn_network_watcher`]) — a background
//!    task that periodically re-evaluates trust and pokes a supplied callback
//!    when the trust status flips, so the `RemoteControl` supervisor can go
//!    dormant/active on a roam without waiting for a config reload (analysis
//!    C5).
//!
//! `SSID` is display-only and best-effort; this module never calls `SSID` APIs
//! (only `netdev` gateway/interface reads), so it cannot trip the macOS
//! Location permission.

use std::io::Write as _;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use netdev::{Interface, MacAddr, NetworkDevice};
use serde::{Deserialize, Serialize};

use scribe_common::app::current_state_dir;
use scribe_common::protocol::TrustedNetworkInfo;

/// On-disk store format version. Bumped only on an incompatible layout change.
const STORE_VERSION: u32 = 1;
/// Marks the store as server-owned; a mismatch is rejected on load.
const STORE_OWNER: &str = "server";
/// File name of the trusted-networks store under the state directory.
const TRUSTED_NETWORKS_FILE: &str = "lan_trusted_networks.toml";
/// The canonical zero `MAC`, which never counts as a match anchor.
const ZERO_MAC: &str = "00:00:00:00:00:00";

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Default interval between network-fingerprint re-evaluations in the watcher.
/// Low frequency keeps enabled-but-idle cost negligible while still detecting a
/// roam to an untrusted network promptly (SC-007).
pub const DEFAULT_NETWORK_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// The fingerprint of the physical LAN this machine is currently attached to.
///
/// Built only when the network can be confidently identified; otherwise
/// [`current_network_fingerprint`] fails closed with a
/// [`NetworkFingerprintError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkFingerprint {
    /// Normalized lowercase default-gateway `MAC` (`xx:xx:xx:xx:xx:xx`); the
    /// primary, never-zero match anchor.
    pub gateway_mac: String,
    /// Local subnet in CIDR form (e.g. `192.168.1.0/24`); the secondary
    /// corroborator.
    pub subnet_cidr: String,
    /// Default-gateway IP; a weak corroborator, not used by the match rule.
    pub gateway_ip: Option<IpAddr>,
    /// `SSID` display hint. Always `None` here (never read, to avoid tripping
    /// macOS Location); retained for the data model and future display.
    pub ssid: Option<String>,
}

/// Why the current network could not be confidently fingerprinted. Every
/// variant means "fail closed — treat as untrusted / dormant".
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NetworkFingerprintError {
    /// No default route to a physical LAN gateway was found.
    #[error("no default route to a physical LAN gateway")]
    NoDefaultRoute,
    /// A gateway exists but its link-layer address is zero or unresolved (e.g.
    /// a cold neighbor cache), so it cannot anchor trust.
    #[error("default gateway MAC address is zero or unresolved")]
    ZeroGatewayMac,
    /// The only default route is a VPN/tunnel interface; the physical LAN
    /// gateway must be fingerprinted instead, and none is reachable.
    #[error("the only default route is a VPN or tunnel interface")]
    VpnOnly,
    /// A physical gateway was found but no usable IPv4 subnet accompanies it.
    #[error("no usable IPv4 subnet on the physical LAN interface")]
    NoUsableSubnet,
}

/// Fingerprint the physical LAN interface this machine currently routes
/// through. Reads `netdev` interface/gateway tables only.
pub fn current_network_fingerprint() -> Result<NetworkFingerprint, NetworkFingerprintError> {
    fingerprint_from_interfaces(&netdev::get_interfaces())
}

/// Pure core of [`current_network_fingerprint`] over an interface snapshot.
///
/// Prefers a physical (non-tunnel) interface that carries a resolvable default
/// gateway, choosing the system default route when it qualifies so a coexisting
/// VPN never hijacks the fingerprint (FR-008/009). Fails closed with a precise
/// reason when no such interface exists.
fn fingerprint_from_interfaces(
    ifaces: &[Interface],
) -> Result<NetworkFingerprint, NetworkFingerprintError> {
    let Some(iface) = choose_lan_iface(ifaces) else {
        return Err(classify_unidentifiable(ifaces));
    };
    // Guaranteed `Some` with a non-zero MAC by `is_lan_gateway_iface`; handled
    // defensively rather than with an indexing/unwrap panic.
    let Some(gateway) = iface.gateway.as_ref() else {
        return Err(NetworkFingerprintError::NoDefaultRoute);
    };
    let subnet_cidr =
        lan_subnet_cidr(iface, gateway).ok_or(NetworkFingerprintError::NoUsableSubnet)?;
    Ok(NetworkFingerprint {
        gateway_mac: gateway.mac_addr.address(),
        subnet_cidr,
        gateway_ip: gateway.ipv4.first().copied().map(IpAddr::V4),
        ssid: None,
    })
}

/// Select the physical LAN interface that anchors both network trust and the
/// LAN listener bind: the system default-route interface when it is a qualifying
/// physical LAN gateway link ([`is_lan_gateway_iface`]), else the first
/// qualifying candidate. Shared by [`fingerprint_from_interfaces`] and
/// [`physical_lan_addrs_from`] so the LAN listener always binds on the very
/// interface whose gateway made the network trusted (feature 014 T010).
fn choose_lan_iface(ifaces: &[Interface]) -> Option<&Interface> {
    let candidates: Vec<&Interface> =
        ifaces.iter().filter(|iface| is_lan_gateway_iface(iface)).collect();
    candidates.iter().copied().find(|iface| iface.default).or_else(|| candidates.first().copied())
}

/// The physical-LAN IPv4 address(es) the LAN listener binds and advertises on:
/// the IPv4 addresses configured on the same physical interface
/// [`current_network_fingerprint`] anchors trust to. Empty when no physical LAN
/// gateway interface qualifies, so the supervisor fails closed and binds nothing.
fn physical_lan_addrs_from(ifaces: &[Interface]) -> Vec<IpAddr> {
    choose_lan_iface(ifaces)
        .map(|iface| iface.ipv4.iter().map(|net| IpAddr::V4(net.addr())).collect())
        .unwrap_or_default()
}

/// One consistent activation snapshot for the LAN remote-control supervisor
/// (feature 014 T010): whether the current physical network is trusted per
/// `store` **and** the physical-LAN address(es) to bind/advertise on — both
/// derived from a single `netdev` interface read so a roam can never leave them
/// disagreeing. Fails closed (untrusted, empty addresses) when the network is
/// unidentifiable. Performs a blocking `netdev` read, so call it off the async
/// runtime (e.g. via `spawn_blocking`).
pub fn lan_activation_snapshot(store: &TrustedNetworksStore) -> (bool, Vec<IpAddr>) {
    let ifaces = netdev::get_interfaces();
    let addrs = physical_lan_addrs_from(&ifaces);
    let trusted = fingerprint_from_interfaces(&ifaces)
        .is_ok_and(|fingerprint| store.matches_any(&fingerprint).is_some());
    (trusted, addrs)
}

/// Whether an interface is a physical LAN link with an ARP-resolved default
/// gateway: up, not loopback, not a tunnel/point-to-point link, with its own
/// non-zero `MAC`, an IPv4 subnet, and a gateway carrying a non-zero `MAC` and
/// an IPv4 address. A VPN tunnel's gateway has no link-layer `MAC`, so this
/// naturally excludes tunnels.
fn is_lan_gateway_iface(iface: &Interface) -> bool {
    iface.is_up()
        && !iface.is_loopback()
        && !iface.is_tun()
        && !iface.is_point_to_point()
        && !iface.ipv4.is_empty()
        && iface.mac_addr.is_some_and(|mac| mac != MacAddr::zero())
        && iface
            .gateway
            .as_ref()
            .is_some_and(|gateway| gateway.mac_addr != MacAddr::zero() && !gateway.ipv4.is_empty())
}

/// Precise fail-closed reason when no physical LAN gateway interface qualifies.
fn classify_unidentifiable(ifaces: &[Interface]) -> NetworkFingerprintError {
    let route_bearing = || ifaces.iter().filter(|iface| iface.default || iface.gateway.is_some());
    if route_bearing().next().is_none() {
        return NetworkFingerprintError::NoDefaultRoute;
    }
    let only_tunnels = route_bearing()
        .all(|iface| iface.is_tun() || iface.is_point_to_point() || iface.is_loopback());
    if only_tunnels {
        return NetworkFingerprintError::VpnOnly;
    }
    NetworkFingerprintError::ZeroGatewayMac
}

/// Derive the CIDR subnet on `iface` that contains the gateway IP, falling back
/// to the interface's first IPv4 network. Returns the canonical network form
/// (host bits zeroed, e.g. `192.168.1.0/24`).
fn lan_subnet_cidr(iface: &Interface, gateway: &NetworkDevice) -> Option<String> {
    let gateway_v4 = gateway.ipv4.first().copied();
    let net = gateway_v4
        .and_then(|gw| iface.ipv4.iter().find(|candidate| candidate.contains(&gw)).copied())
        .or_else(|| iface.ipv4.first().copied())?;
    Some(net.trunc().to_string())
}

/// Default user-facing label for a newly trusted network: the `SSID` when known
/// (never here), else a gateway-`MAC`-derived name (data-model `TrustedNetwork`).
fn default_network_label(fingerprint: &NetworkFingerprint) -> String {
    fingerprint.ssid.clone().unwrap_or_else(|| format!("Gateway {}", fingerprint.gateway_mac))
}

/// Whether `current` matches a stored trusted network: equal, non-zero gateway
/// `MAC` **and** equal subnet (data-model `TrustedNetwork` match rule).
fn network_matches(current: &NetworkFingerprint, stored: &TrustedNetwork) -> bool {
    !is_zero_mac(&current.gateway_mac)
        && current.gateway_mac == stored.gateway_mac
        && current.subnet_cidr == stored.subnet_cidr
}

fn is_zero_mac(mac: &str) -> bool {
    mac.is_empty() || mac == ZERO_MAC
}

/// One trusted network — the activation gate record (data-model
/// `TrustedNetwork`). Persisted; converted to [`TrustedNetworkInfo`] for the
/// wire/settings surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustedNetwork {
    /// Record key.
    id: String,
    /// User-facing label (`SSID` when known, else a gateway-derived default).
    label: String,
    /// Normalized lowercase gateway `MAC`; the primary match anchor.
    gateway_mac: String,
    /// Local subnet in CIDR form; the secondary corroborator.
    subnet_cidr: String,
    /// Gateway IP; a weak corroborator only, not part of the match rule.
    #[serde(default)]
    gateway_ip: Option<IpAddr>,
    /// `SSID` display hint; may be absent on wired links or modern macOS.
    #[serde(default)]
    ssid: Option<String>,
    /// When the network was trusted, Unix epoch milliseconds.
    added_at: u64,
}

impl TrustedNetwork {
    fn to_info(&self) -> TrustedNetworkInfo {
        TrustedNetworkInfo {
            id: self.id.clone(),
            label: self.label.clone(),
            gateway_mac: self.gateway_mac.clone(),
            subnet_cidr: self.subnet_cidr.clone(),
            ssid: self.ssid.clone(),
            added_at: self.added_at,
        }
    }
}

/// The persisted store document.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTrustedNetworks {
    version: u32,
    #[serde(default)]
    owner: String,
    updated_at_ms: u64,
    #[serde(default)]
    networks: Vec<TrustedNetwork>,
}

impl Default for PersistedTrustedNetworks {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            owner: STORE_OWNER.to_owned(),
            updated_at_ms: 0,
            networks: Vec::new(),
        }
    }
}

/// Errors from the trusted-networks store.
#[derive(Debug, thiserror::Error)]
pub enum TrustedNetworksError {
    #[error("could not determine the state directory")]
    NoStateDir,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML serialize error: {0}")]
    Serialize(String),
    #[error("TOML parse error: {0}")]
    Parse(String),
    #[error("unsupported trusted-networks store version {0}")]
    UnsupportedVersion(u32),
    #[error("trusted-networks store owner is not server-owned")]
    NonServerOwnedStore,
    #[error("current network is unidentifiable: {0}")]
    Unidentifiable(#[from] NetworkFingerprintError),
}

/// The trusted-networks activation store. Loaded once; every mutation writes
/// the whole document back atomically with owner-private permissions.
pub struct TrustedNetworksStore {
    path: Option<PathBuf>,
    data: PersistedTrustedNetworks,
}

/// A [`TrustedNetworksStore`] shared between the request handlers and the
/// background [`spawn_network_watcher`] task.
pub type SharedTrustedNetworks = Arc<Mutex<TrustedNetworksStore>>;

impl TrustedNetworksStore {
    /// Load the store from the state directory, falling back to an empty store
    /// (with a warning) on any read/parse error so a corrupt file never wedges
    /// startup.
    pub fn load() -> Self {
        let path = current_state_dir().map(|dir| dir.join(TRUSTED_NETWORKS_FILE));
        let data = match Self::read(path.as_deref()) {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(%error, "failed to load trusted networks; using empty store");
                PersistedTrustedNetworks::default()
            }
        };
        Self { path, data }
    }

    fn read(path: Option<&Path>) -> Result<PersistedTrustedNetworks, TrustedNetworksError> {
        let Some(path) = path else {
            return Ok(PersistedTrustedNetworks::default());
        };
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedTrustedNetworks::default());
            }
            Err(error) => return Err(error.into()),
        };
        let data: PersistedTrustedNetworks = toml::from_str(&content)
            .map_err(|error| TrustedNetworksError::Parse(error.to_string()))?;
        if data.version != STORE_VERSION {
            return Err(TrustedNetworksError::UnsupportedVersion(data.version));
        }
        if data.owner != STORE_OWNER {
            return Err(TrustedNetworksError::NonServerOwnedStore);
        }
        Ok(data)
    }

    /// All trusted networks in the wire/settings shape.
    pub fn list(&self) -> Vec<TrustedNetworkInfo> {
        self.data.networks.iter().map(TrustedNetwork::to_info).collect()
    }

    /// The stored network matching `fingerprint` under the match rule, if any.
    fn matches_any(&self, fingerprint: &NetworkFingerprint) -> Option<&TrustedNetwork> {
        self.data.networks.iter().find(|stored| network_matches(fingerprint, stored))
    }

    /// Whether this machine is currently on a trusted network. Fails closed
    /// (returns `false`) whenever the network is unidentifiable.
    pub fn is_current_network_trusted(&self) -> bool {
        current_network_fingerprint()
            .is_ok_and(|fingerprint| self.matches_any(&fingerprint).is_some())
    }

    /// The trusted network this machine currently matches, for status display.
    pub fn current_trusted_network(&self) -> Option<TrustedNetworkInfo> {
        let fingerprint = current_network_fingerprint().ok()?;
        self.matches_any(&fingerprint).map(TrustedNetwork::to_info)
    }

    /// Trust the network this machine is currently on. Fails with a typed
    /// [`NetworkFingerprintError`] (via [`TrustedNetworksError::Unidentifiable`])
    /// when the current network cannot be fingerprinted. Idempotent: an already
    /// trusted network returns its existing record without adding a duplicate.
    pub fn add_current(
        &mut self,
        label: Option<String>,
    ) -> Result<TrustedNetworkInfo, TrustedNetworksError> {
        let fingerprint = current_network_fingerprint()?;
        if let Some(existing) = self.matches_any(&fingerprint) {
            return Ok(existing.to_info());
        }
        let now = unix_time_ms();
        let record = TrustedNetwork {
            id: uuid::Uuid::new_v4().to_string(),
            label: label.unwrap_or_else(|| default_network_label(&fingerprint)),
            gateway_mac: fingerprint.gateway_mac,
            subnet_cidr: fingerprint.subnet_cidr,
            gateway_ip: fingerprint.gateway_ip,
            ssid: fingerprint.ssid,
            added_at: now,
        };
        let info = record.to_info();
        let mut next = self.data.clone();
        next.networks.push(record);
        next.updated_at_ms = now;
        self.persist(next)?;
        Ok(info)
    }

    /// Remove a trusted network by id. Returns whether a record was removed.
    pub fn remove(&mut self, id: &str) -> Result<bool, TrustedNetworksError> {
        let mut next = self.data.clone();
        let before = next.networks.len();
        next.networks.retain(|stored| stored.id != id);
        if next.networks.len() == before {
            return Ok(false);
        }
        next.updated_at_ms = unix_time_ms();
        self.persist(next)?;
        Ok(true)
    }

    fn persist(&mut self, next: PersistedTrustedNetworks) -> Result<(), TrustedNetworksError> {
        let path = self.path.as_deref().ok_or(TrustedNetworksError::NoStateDir)?;
        write_toml_atomic(path, &next)?;
        self.data = next;
        Ok(())
    }
}

/// Spawn a background task that re-evaluates network trust every
/// `poll_interval` and invokes `on_change` with the new trust status whenever
/// it flips relative to `initial_trusted` (the baseline the supervisor already
/// applied). Lets the supervisor go dormant/active on a roam without a config
/// reload (analysis C5, FR-018). Returns the task handle so the caller can
/// abort it on shutdown or LAN-disable.
pub fn spawn_network_watcher<F>(
    networks: SharedTrustedNetworks,
    poll_interval: Duration,
    initial_trusted: bool,
    on_change: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(bool) + Send + 'static,
{
    tokio::spawn(async move {
        let mut last_trusted = initial_trusted;
        let mut ticker = tokio::time::interval(poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let trusted_now = evaluate_current_trust(&networks).await;
            if trusted_now != last_trusted {
                last_trusted = trusted_now;
                on_change(trusted_now);
            }
        }
    })
}

/// Compute the current trust status. The blocking `netdev` read runs on the
/// blocking pool; the store lock is held only for the synchronous match, never
/// across an await.
async fn evaluate_current_trust(networks: &SharedTrustedNetworks) -> bool {
    let fingerprint = match tokio::task::spawn_blocking(current_network_fingerprint).await {
        Ok(Ok(fingerprint)) => fingerprint,
        Ok(Err(_unidentifiable)) => return false,
        Err(_join_error) => return false,
    };
    match networks.lock() {
        Ok(store) => store.matches_any(&fingerprint).is_some(),
        Err(_poisoned) => false,
    }
}

fn write_toml_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), TrustedNetworksError> {
    ensure_private_parent(path)?;
    let content = toml::to_string_pretty(value)
        .map_err(|error| TrustedNetworksError::Serialize(error.to_string()))?;
    let tmp_path = private_temp_path(path);
    {
        let mut file = create_private_file(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&tmp_path, path) {
        drop(std::fs::remove_file(&tmp_path));
        return Err(error.into());
    }
    set_private_file_permissions(path)?;
    Ok(())
}

fn ensure_private_parent(path: &Path) -> Result<(), TrustedNetworksError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_private_dir_permissions(parent)?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        options.mode(PRIVATE_FILE_MODE);
    }
    let file = options.open(path)?;
    set_private_file_permissions(path)?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn private_temp_path(path: &Path) -> PathBuf {
    let file_name =
        path.file_name().and_then(|name| name.to_str()).unwrap_or("lan_trusted_networks");
    path.with_file_name(format!(".{file_name}.{}.{}.tmp", std::process::id(), unix_time_ms()))
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
