//! `mDNS` / `DNS-SD` peer discovery for LAN remote control (research D1).
//!
//! This module owns a single pure-Rust `mdns-sd` [`ServiceDaemon`] used for BOTH
//! advertising this machine and browsing for peers, so the interface filtering
//! is configured once and applies to both roles. It exposes:
//!
//! - **Advertising** ([`LanDiscovery::start_advertising`] /
//!   [`LanDiscovery::stop_advertising`]) — publish `_scribe._tcp.local.` with the
//!   control port in the `SRV` record and `TXT` records `txtvers`, `id` (the hex
//!   Device ID from [`crate::lan::identity`]), `protovers`
//!   ([`REMOTE_PROTOCOL_VERSION`](scribe_common::protocol::REMOTE_PROTOCOL_VERSION)),
//!   and `host` (the machine hostname — the LAN↔tailnet name-dedup key). Stopping
//!   sends an `mDNS` goodbye.
//! - **Browsing** ([`LanDiscovery::start_browsing`]) — a background `tokio` task
//!   drains the daemon's event channel with [`recv_async`](mdns_sd::Receiver),
//!   maintaining a deduped table of [`LanPeerInfo`] read back via
//!   [`LanDiscovery::peers`] (the source for `ListLanPeers`). Peers are keyed and
//!   deduped by `TXT` `id`; the machine's own advert is filtered out; resolved
//!   addresses are filtered to the current physical-LAN subnet; and entries are
//!   evicted on `ServiceRemoved` or a failed `verify()`.
//!
//! **Multi-interface / `VPN` gotcha (research D1, the #1 pitfall)**: tailnet and
//! other tunnel/point-to-point interfaces are `disable_interface`-d on the shared
//! daemon at construction, so a peer never advertises on nor resolves to a
//! `100.x` tailnet address. Address filtering to the physical-LAN subnet is a
//! second line of defence.
//!
//! The daemon is a self-contained worker thread; every call here only enqueues a
//! command, so these methods are cheap to invoke from the async `RemoteControl`
//! supervisor without blocking. Dropping a [`LanDiscovery`] (or the supervisor
//! going dormant on an untrusted network) tears the daemon down completely.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mdns_sd::{
    IfKind, Receiver, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo,
};
use netdev::Interface;
use tokio::task::JoinHandle;

use scribe_common::protocol::LanPeerInfo;

/// The `DNS-SD` service type Scribe advertises and browses.
const SERVICE_TYPE: &str = "_scribe._tcp.local.";
/// `TXT` key: record format version, always [`TXTVERS_VALUE`].
const TXT_KEY_TXTVERS: &str = "txtvers";
/// `TXT` key: hex Device ID (`SHA-256`(`SPKI`)) — the dedupe + pin key.
const TXT_KEY_ID: &str = "id";
/// `TXT` key: the peer's `REMOTE_PROTOCOL_VERSION`.
const TXT_KEY_PROTOVERS: &str = "protovers";
/// `TXT` key: machine hostname, the LAN↔tailnet name-match key.
const TXT_KEY_HOST: &str = "host";
/// `TXT` `txtvers` value (`DNS-SD` convention, RFC 6763 §6.4).
const TXTVERS_VALUE: &str = "1";
/// Hostname fallback when the OS lookup fails.
const DEFAULT_HOST: &str = "localhost";
/// `mDNS` instance-name fallback when the hostname is empty.
const DEFAULT_INSTANCE: &str = "scribe";

/// How often the browse task re-`verify()`s known peers and prunes stale ones.
/// Low frequency keeps the enabled-but-idle cost negligible (spec PR-003).
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// Per-peer `verify()` deadline; a peer that fails to answer is evicted.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an offline (post-`ServiceRemoved`) entry lingers before pruning, so
/// the picker can briefly grey a peer that just vanished.
const EVICT_AFTER: Duration = Duration::from_secs(30);

/// What to advertise for this machine. `protocol_version` should be
/// [`REMOTE_PROTOCOL_VERSION`](scribe_common::protocol::REMOTE_PROTOCOL_VERSION);
/// `addrs` are the physical-LAN address(es) the listener is bound to (when empty,
/// the daemon auto-fills from its enabled, non-tunnel interfaces).
#[derive(Debug, Clone)]
pub struct AdvertiseConfig {
    /// Hex Device ID for the `TXT` `id` record (from [`crate::lan::identity`]).
    pub device_id_hex: String,
    /// Machine hostname for the `TXT` `host` record and the `mDNS` instance name.
    pub host: String,
    /// Physical-LAN address(es) to advertise in the `A`/`AAAA` records.
    pub addrs: Vec<IpAddr>,
    /// Control port for the `SRV` record.
    pub port: u16,
    /// Remote protocol version for the `TXT` `protovers` record.
    pub protocol_version: u32,
}

/// Errors from the `mDNS` daemon.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The underlying `mdns-sd` daemon could not create a socket, enqueue a
    /// command, or build the service record.
    #[error("mDNS discovery error: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

/// One discovered peer plus the bookkeeping the browse task needs to evict it.
struct PeerEntry {
    /// The `mDNS` fullname, used to correlate a later `ServiceRemoved`.
    fullname: String,
    /// The wire-facing peer description returned by [`LanDiscovery::peers`].
    info: LanPeerInfo,
    /// When the peer went offline (`ServiceRemoved`), for stale pruning; `None`
    /// while the peer is online.
    offline_since: Option<Instant>,
}

/// The discovered-peer table, shared between [`LanDiscovery`] and its browse task.
type SharedLanPeers = Arc<Mutex<HashMap<String, PeerEntry>>>;

/// The LAN discovery surface: one `mdns-sd` daemon driving advertise + browse.
pub struct LanDiscovery {
    /// The shared daemon (cheap to clone; clones share one worker thread).
    daemon: ServiceDaemon,
    /// This machine's hex Device ID, used to filter its own advert out of browse.
    self_device_id_hex: String,
    /// Deduped table of discovered peers, keyed by hex Device ID.
    peers: SharedLanPeers,
    /// The currently advertised `mDNS` fullname, if advertising.
    advertised: Mutex<Option<String>>,
    /// The background browse task handle, if browsing.
    browse_task: Mutex<Option<JoinHandle<()>>>,
}

impl LanDiscovery {
    /// Create the `mDNS` daemon and disable tunnel/`VPN` interfaces on it, so
    /// neither advertising nor browsing ever touches a tailnet/tunnel link
    /// (research D1). Does not advertise or browse until asked.
    pub fn new(self_device_id_hex: String) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let tunnel_kinds = tunnel_if_kinds(&netdev::get_interfaces());
        if !tunnel_kinds.is_empty() {
            daemon.disable_interface(tunnel_kinds)?;
        }
        Ok(Self {
            daemon,
            self_device_id_hex,
            peers: Arc::new(Mutex::new(HashMap::new())),
            advertised: Mutex::new(None),
            browse_task: Mutex::new(None),
        })
    }

    /// Advertise this machine, replacing any prior advert. Idempotent in effect:
    /// re-registers with the supplied addresses/port/`TXT` records.
    pub fn start_advertising(&self, config: &AdvertiseConfig) -> Result<(), DiscoveryError> {
        self.stop_advertising();
        let info = build_service_info(config)?;
        let fullname = info.get_fullname().to_owned();
        self.daemon.register(info)?;
        if let Ok(mut slot) = self.advertised.lock() {
            *slot = Some(fullname.clone());
        }
        tracing::info!(%fullname, port = config.port, "advertising Scribe on the LAN");
        Ok(())
    }

    /// Stop advertising and send an `mDNS` goodbye so peers evict us promptly.
    pub fn stop_advertising(&self) {
        let fullname = match self.advertised.lock() {
            Ok(mut slot) => slot.take(),
            Err(_poisoned) => None,
        };
        if let Some(fullname) = fullname {
            if let Err(error) = self.daemon.unregister(&fullname) {
                tracing::debug!(%error, %fullname, "mDNS unregister failed");
            }
        }
    }

    /// Start the background browse task (no-op if already browsing). The task
    /// maintains the peer table until [`LanDiscovery`] is dropped.
    pub fn start_browsing(&self) -> Result<(), DiscoveryError> {
        let mut slot = match self.browse_task.lock() {
            Ok(slot) => slot,
            Err(_poisoned) => return Ok(()),
        };
        if slot.is_some() {
            return Ok(());
        }
        let receiver = self.daemon.browse(SERVICE_TYPE)?;
        let task = spawn_browse_task(
            self.daemon.clone(),
            Arc::clone(&self.peers),
            self.self_device_id_hex.clone(),
            receiver,
        );
        *slot = Some(task);
        Ok(())
    }

    /// A snapshot of the currently known LAN peers (self excluded), for the
    /// `ListLanPeers` handler and the connect picker. Offline-but-not-yet-evicted
    /// peers carry `online = false`. Delegates to a [`LanPeerHandle`] so the
    /// snapshot logic is shared with the dispatch-path read handle.
    #[must_use]
    pub fn peers(&self) -> Vec<LanPeerInfo> {
        self.peer_handle().peers()
    }

    /// A cheap, cloneable read handle over this discovery's live peer table,
    /// shareable with the dispatch path (the `ListLanPeers` handler) independently
    /// of this `LanDiscovery`'s lifetime. The `RemoteControl` supervisor publishes
    /// one while the LAN transport is active; cloning shares the one underlying
    /// table, so the handle keeps reflecting browse updates until this
    /// `LanDiscovery` is dropped on dormancy.
    #[must_use]
    pub fn peer_handle(&self) -> LanPeerHandle {
        LanPeerHandle { peers: Arc::clone(&self.peers) }
    }

    /// Whether this machine is currently advertising.
    #[must_use]
    pub fn is_advertising(&self) -> bool {
        self.advertised.lock().is_ok_and(|slot| slot.is_some())
    }
}

impl Drop for LanDiscovery {
    fn drop(&mut self) {
        self.stop_advertising();
        if let Ok(mut slot) = self.browse_task.lock() {
            if let Some(task) = slot.take() {
                task.abort();
            }
        }
        if let Err(error) = self.daemon.shutdown() {
            tracing::debug!(%error, "mDNS daemon shutdown failed");
        }
    }
}

/// A cheap, cloneable read handle over a [`LanDiscovery`]'s discovered-peer table,
/// decoupled from that `LanDiscovery`'s lifetime. The `RemoteControl` supervisor
/// publishes one into its shared state while the LAN transport is active so the
/// local-only `ListLanPeers` dispatch handler can read the live peer snapshot
/// without reaching into the supervisor task. Cloning shares the one underlying
/// table; the supervisor clears the published handle when the transport goes
/// dormant, so a stale table is never observed after teardown.
#[derive(Clone)]
pub struct LanPeerHandle {
    /// The discovered-peer table shared with the owning [`LanDiscovery`] and its
    /// browse task.
    peers: SharedLanPeers,
}

impl LanPeerHandle {
    /// A snapshot of the currently known LAN peers (self excluded). Mirrors
    /// [`LanDiscovery::peers`]; offline-but-not-yet-evicted peers carry
    /// `online = false`. A poisoned lock yields an empty list (fail-closed).
    #[must_use]
    pub fn peers(&self) -> Vec<LanPeerInfo> {
        match self.peers.lock() {
            Ok(map) => map.values().map(|entry| entry.info.clone()).collect(),
            Err(_poisoned) => Vec::new(),
        }
    }
}

/// Read the system hostname via `gethostname(2)`, falling back to `localhost`.
/// The `RemoteControl` supervisor uses this to fill [`AdvertiseConfig::host`].
#[must_use]
pub fn local_hostname() -> String {
    nix::unistd::gethostname()
        .map_or_else(|_| String::from(DEFAULT_HOST), |name| name.to_string_lossy().into_owned())
}

/// Build the `mDNS` service record from an [`AdvertiseConfig`]. When no explicit
/// addresses are supplied, the daemon auto-selects from its enabled interfaces.
fn build_service_info(config: &AdvertiseConfig) -> Result<ServiceInfo, DiscoveryError> {
    let instance = instance_label(&config.host);
    let host_name = format!("{instance}.local.");
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &host_name,
        config.addrs.as_slice(),
        config.port,
        txt_properties(config),
    )?;
    Ok(if config.addrs.is_empty() { info.enable_addr_auto() } else { info })
}

/// The `TXT` record map: `txtvers`, `id`, `protovers`, `host`.
fn txt_properties(config: &AdvertiseConfig) -> HashMap<String, String> {
    let mut props = HashMap::new();
    props.insert(TXT_KEY_TXTVERS.to_owned(), TXTVERS_VALUE.to_owned());
    props.insert(TXT_KEY_ID.to_owned(), config.device_id_hex.clone());
    props.insert(TXT_KEY_PROTOVERS.to_owned(), config.protocol_version.to_string());
    props.insert(TXT_KEY_HOST.to_owned(), config.host.clone());
    props
}

/// The first hostname label, used as the `mDNS` instance name and host record.
fn instance_label(host: &str) -> String {
    let label = host.split('.').next().unwrap_or(host);
    if label.is_empty() { DEFAULT_INSTANCE.to_owned() } else { label.to_owned() }
}

/// The set of [`IfKind`]s naming every tunnel/`VPN` interface — by name and by
/// each of its addresses — so `disable_interface` excludes tailnet/tunnels on
/// both advertise and browse.
fn tunnel_if_kinds(ifaces: &[Interface]) -> Vec<IfKind> {
    let mut kinds = Vec::new();
    for iface in ifaces.iter().filter(|iface| is_tunnel(iface)) {
        kinds.push(IfKind::Name(iface.name.clone()));
        for net in &iface.ipv4 {
            kinds.push(IfKind::Addr(IpAddr::V4(net.addr())));
        }
        for net in &iface.ipv6 {
            kinds.push(IfKind::Addr(IpAddr::V6(net.addr())));
        }
    }
    kinds
}

/// Spawn the browse task: drain `mDNS` events into the peer table and, on a slow
/// timer, refresh the local-subnet view, prune stale peers, and re-`verify()`
/// live ones (so a crashed peer is evicted before its record TTL expires).
fn spawn_browse_task(
    daemon: ServiceDaemon,
    peers: SharedLanPeers,
    self_id: String,
    receiver: Receiver<ServiceEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ifaces = load_interfaces().await;
        let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = receiver.recv_async() => match event {
                    Ok(event) => handle_browse_event(event, &peers, &self_id, &ifaces),
                    Err(_disconnected) => break,
                },
                _ = sweep.tick() => {
                    ifaces = load_interfaces().await;
                    prune_stale(&peers);
                    verify_online_peers(&daemon, &peers);
                }
            }
        }
    })
}

/// Snapshot the host's network interfaces off the async worker thread.
async fn load_interfaces() -> Vec<Interface> {
    tokio::task::spawn_blocking(netdev::get_interfaces).await.unwrap_or_default()
}

/// Route one browse event to the peer table. Non-resolve/remove events (search
/// lifecycle) are ignored.
fn handle_browse_event(
    event: ServiceEvent,
    peers: &SharedLanPeers,
    self_id: &str,
    ifaces: &[Interface],
) {
    match event {
        ServiceEvent::ServiceResolved(service) => {
            handle_resolved(peers, self_id, service.as_ref(), ifaces);
        }
        ServiceEvent::ServiceRemoved(_ty_domain, fullname) => mark_offline(peers, &fullname),
        _other => {}
    }
}

/// Upsert a resolved peer into the table, deduped by `TXT` `id`. Silently drops
/// this machine's own advert, records missing `id`, and off-subnet peers.
fn handle_resolved(
    peers: &SharedLanPeers,
    self_id: &str,
    service: &ResolvedService,
    ifaces: &[Interface],
) {
    let Some((device_id_hex, info)) = resolved_to_peer(service, self_id, ifaces) else {
        return;
    };
    let Ok(mut map) = peers.lock() else {
        return;
    };
    map.insert(
        device_id_hex,
        PeerEntry { fullname: service.fullname.clone(), info, offline_since: None },
    );
}

/// Convert a resolved service into `(device_id_hex, LanPeerInfo)`, applying the
/// self-filter, the `TXT` `id` requirement, and the current-subnet address
/// filter. Returns `None` when the record is not a dialable LAN Scribe peer.
fn resolved_to_peer(
    service: &ResolvedService,
    self_id: &str,
    ifaces: &[Interface],
) -> Option<(String, LanPeerInfo)> {
    let props = &service.txt_properties;
    let device_id_hex = props.get_property_val_str(TXT_KEY_ID)?.to_owned();
    if device_id_hex.eq_ignore_ascii_case(self_id) {
        return None;
    }
    let name = instance_name(&service.fullname, &service.ty_domain);
    let host = props.get_property_val_str(TXT_KEY_HOST).map_or_else(|| name.clone(), str::to_owned);
    let protovers =
        props.get_property_val_str(TXT_KEY_PROTOVERS).and_then(|value| value.parse().ok())?;
    let addr = pick_lan_addr(&service.addresses, ifaces)?;
    Some((
        device_id_hex,
        LanPeerInfo { name, host, addr, port: service.port, protovers, online: true },
    ))
}

/// The `mDNS` instance name (the fullname with the trailing service type and dot
/// removed), falling back to the raw fullname.
fn instance_name(fullname: &str, ty_domain: &str) -> String {
    fullname
        .strip_suffix(ty_domain)
        .and_then(|head| head.strip_suffix('.'))
        .unwrap_or(fullname)
        .to_owned()
}

/// Choose a single dialable address on the current physical-LAN subnet, IPv4
/// preferred over IPv6, skipping loopback and off-subnet (incl. tunnel) addresses.
fn pick_lan_addr(addrs: &HashSet<ScopedIp>, ifaces: &[Interface]) -> Option<String> {
    let mut v4: Option<IpAddr> = None;
    let mut v6: Option<IpAddr> = None;
    for scoped in addrs {
        let ip = scoped.to_ip_addr();
        if ip.is_loopback() || !on_local_subnet(ip, ifaces) {
            continue;
        }
        if ip.is_ipv4() {
            v4.get_or_insert(ip);
        } else {
            v6.get_or_insert(ip);
        }
    }
    v4.or(v6).map(|ip| ip.to_string())
}

/// Whether `ip` falls inside a subnet of one of this machine's physical-LAN
/// interfaces (tunnels excluded), i.e. it is directly reachable on the LAN.
fn on_local_subnet(ip: IpAddr, ifaces: &[Interface]) -> bool {
    ifaces.iter().filter(|iface| is_physical_lan(iface)).any(|iface| iface_contains(iface, ip))
}

/// Whether one of `iface`'s configured subnets contains `ip`.
fn iface_contains(iface: &Interface, ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => iface.ipv4.iter().any(|net| net.contains(&v4)),
        IpAddr::V6(v6) => iface.ipv6.iter().any(|net| net.contains(&v6)),
    }
}

/// A physical LAN link: up, not loopback, not a tunnel/point-to-point interface.
fn is_physical_lan(iface: &Interface) -> bool {
    iface.is_up() && !iface.is_loopback() && !iface.is_tun() && !iface.is_point_to_point()
}

/// A tunnel/`VPN` link (tailnet, `WireGuard`, generic tun/point-to-point).
fn is_tunnel(iface: &Interface) -> bool {
    iface.is_tun() || iface.is_point_to_point()
}

/// Mark every entry with a matching fullname offline (evicted lazily by
/// [`prune_stale`]), so the picker can grey a peer that just disappeared.
fn mark_offline(peers: &SharedLanPeers, fullname: &str) {
    let Ok(mut map) = peers.lock() else {
        return;
    };
    for entry in map.values_mut().filter(|entry| entry.fullname == fullname) {
        entry.info.online = false;
        entry.offline_since = Some(Instant::now());
    }
}

/// Drop entries that have been offline longer than [`EVICT_AFTER`].
fn prune_stale(peers: &SharedLanPeers) {
    let Ok(mut map) = peers.lock() else {
        return;
    };
    let now = Instant::now();
    map.retain(|_id, entry| {
        entry.info.online
            || entry.offline_since.is_none_or(|since| now.duration_since(since) < EVICT_AFTER)
    });
}

/// Re-`verify()` each online peer; a peer that no longer answers produces a
/// `ServiceRemoved` on the browse channel, evicting it (research D1).
fn verify_online_peers(daemon: &ServiceDaemon, peers: &SharedLanPeers) {
    let fullnames: Vec<String> = match peers.lock() {
        Ok(map) => map
            .values()
            .filter(|entry| entry.info.online)
            .map(|entry| entry.fullname.clone())
            .collect(),
        Err(_poisoned) => return,
    };
    for fullname in fullnames {
        if let Err(error) = daemon.verify(fullname, VERIFY_TIMEOUT) {
            tracing::debug!(%error, "mDNS verify failed");
        }
    }
}
