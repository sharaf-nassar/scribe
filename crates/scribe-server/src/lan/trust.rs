//! Trusted-device pin store and LAN approval state machine (research D4,
//! data-model `TrustedDevice` / `ApprovalRequest`).
//!
//! This module owns the LAN trust boundary's persistent and runtime halves; it
//! is deliberately the store + state machine ONLY — the accept path, the
//! owning-side prompt, and the local-socket handlers wire it in (tasks T011,
//! T018, T027). It provides two cooperating pieces:
//!
//! 1. **The trusted-device store** ([`TrustedDevicesStore`]) — an
//!    approve/list/revoke store of [`TrustedDevice`] records persisted under the
//!    server's per-user state directory, keyed by the peer's pinned
//!    `device_id = SHA-256(SPKI)`. Each record carries the full presented
//!    certificate, the peer's advertised label, the first-approval time, and the
//!    trusted network it was approved on. [`TrustedDevicesStore::is_trusted`] is
//!    the strict pin check the approval gate short-circuits on; a device whose
//!    presented `device_id` matches no record is unknown and needs approval —
//!    including a reinstalled peer that regenerated its key (a new `device_id`,
//!    simply a new unknown device, never silently trusted, FR-005). The store
//!    also answers [`TrustedDevicesStore::name_collision`]: whether an
//!    already-trusted device already uses an advertised name — an INFORMATIONAL
//!    prompt hint only, never a trust key.
//!
//! 2. **The approval state machine** ([`PendingApprovals`]) — the runtime
//!    `ApprovalRequest` lifecycle (`Pending` -> `Approved` / `Declined` /
//!    timed-out). A first-time connection is held pending the owning user's
//!    decision while NO window or session data flows (SEC-001). Holds are bounded
//!    two ways (analysis S1): a cap on concurrent pending holds
//!    ([`MAX_PENDING_APPROVALS`]) so unapproved dialers cannot accumulate an
//!    unbounded backlog of human-decision holds, and a per-hold timeout
//!    ([`APPROVAL_TIMEOUT`]) so a single dialer cannot occupy a scarce slot
//!    across an unbounded decision window. The cap and timeout are separate from
//!    the tailnet and LAN connection/handshake caps, so LAN approval activity can
//!    never starve another transport's admission.
//!
//! On approve the accept path persists a [`TrustedDevice`] via
//! [`TrustedDevicesStore::approve`] and proceeds into the 013 attach flow; on
//! decline or timeout it refuses (`LanRefusal::Declined`), reveals nothing, and
//! remembers nothing.

use std::collections::HashMap;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use scribe_common::app::current_state_dir;
use scribe_common::protocol::TrustedDeviceInfo;

use crate::lan::identity::{DEVICE_ID_LEN, fingerprint_words_for};

/// On-disk store format version. Bumped only on an incompatible layout change.
const STORE_VERSION: u32 = 1;
/// Marks the store as server-owned; a mismatch is rejected on load.
const STORE_OWNER: &str = "server";
/// File name of the trusted-devices store under the state directory. Shares the
/// `lan_` prefix with the sibling identity/network stores.
const TRUSTED_DEVICES_FILE: &str = "lan_trusted_devices.toml";

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// A 32-byte LAN Device ID: `SHA-256` of the peer's `SubjectPublicKeyInfo`, the
/// exact-match pin key for LAN trust (same shape as
/// [`crate::lan::identity::DeviceIdentity::device_id`]).
pub type DeviceId = [u8; DEVICE_ID_LEN];

/// A [`TrustedDevicesStore`] shared between the accept path, the approval-decision
/// handler, and the Settings list/revoke handlers.
pub type SharedTrustedDevices = Arc<Mutex<TrustedDevicesStore>>;

// ── Trusted-device store ────────────────────────────────────────────────────

/// One approved LAN peer — the pin record (data-model `TrustedDevice`).
/// Persisted; converted to [`TrustedDeviceInfo`] for the Settings surface. Both
/// byte fields are stored as lowercase hex so the whole record round-trips
/// through `TOML` (which has no native binary type).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustedDevice {
    /// Lowercase hex of the 32-byte `device_id = SHA-256(SPKI)`; the pin key and
    /// the `device_id_hex` echoed to the Settings surface.
    device_id: String,
    /// Lowercase hex of the peer's self-signed certificate DER — the full public
    /// identity, retained for later re-verify/display.
    cert_der: String,
    /// Human label: the peer's advertised name at approval time. Display only —
    /// never a trust key (FR-005).
    label: String,
    /// First approval time, Unix epoch milliseconds.
    first_seen: u64,
    /// The [`crate::lan::network`] `TrustedNetwork` id this device was approved
    /// on; the context where trust was granted. Absent for a record written
    /// without a resolvable network id.
    #[serde(default)]
    approved_on_network: Option<String>,
}

impl TrustedDevice {
    /// Project to the Settings/wire shape, deriving the fingerprint words from the
    /// stored `device_id` so the list shows the same read-aloud fingerprint as the
    /// approval prompt. A record whose `device_id` is not decodable (a corrupt
    /// file) falls back to an empty fingerprint rather than dropping the row.
    fn to_info(&self) -> TrustedDeviceInfo {
        let fingerprint_words = decode_device_id_hex(&self.device_id)
            .map_or_else(String::new, |id| fingerprint_words_for(&id));
        TrustedDeviceInfo {
            device_id_hex: self.device_id.clone(),
            label: self.label.clone(),
            fingerprint_words,
            approved_at: self.first_seen,
        }
    }
}

/// The persisted store document.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTrustedDevices {
    version: u32,
    #[serde(default)]
    owner: String,
    updated_at_ms: u64,
    #[serde(default)]
    devices: Vec<TrustedDevice>,
}

impl Default for PersistedTrustedDevices {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            owner: STORE_OWNER.to_owned(),
            updated_at_ms: 0,
            devices: Vec::new(),
        }
    }
}

/// Errors from the trusted-devices store.
#[derive(Debug, thiserror::Error)]
pub enum TrustedDevicesError {
    #[error("could not determine the state directory")]
    NoStateDir,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML serialize error: {0}")]
    Serialize(String),
    #[error("TOML parse error: {0}")]
    Parse(String),
    #[error("unsupported trusted-devices store version {0}")]
    UnsupportedVersion(u32),
    #[error("trusted-devices store owner is not server-owned")]
    NonServerOwnedStore,
}

/// The trusted-devices pin store. Loaded once; every mutation rebuilds the whole
/// document (never mutating the loaded copy in place) and writes it back
/// atomically with owner-private permissions.
pub struct TrustedDevicesStore {
    path: Option<PathBuf>,
    data: PersistedTrustedDevices,
}

impl TrustedDevicesStore {
    /// Load the store from the state directory, falling back to an empty store
    /// (with a warning) on any read/parse error so a corrupt file never wedges
    /// startup or silently trusts a stranger.
    pub fn load() -> Self {
        let path = current_state_dir().map(|dir| dir.join(TRUSTED_DEVICES_FILE));
        let data = match Self::read(path.as_deref()) {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(%error, "failed to load trusted devices; using empty store");
                PersistedTrustedDevices::default()
            }
        };
        Self { path, data }
    }

    fn read(path: Option<&Path>) -> Result<PersistedTrustedDevices, TrustedDevicesError> {
        let Some(path) = path else {
            return Ok(PersistedTrustedDevices::default());
        };
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedTrustedDevices::default());
            }
            Err(error) => return Err(error.into()),
        };
        let data: PersistedTrustedDevices = toml::from_str(&content)
            .map_err(|error| TrustedDevicesError::Parse(error.to_string()))?;
        if data.version != STORE_VERSION {
            return Err(TrustedDevicesError::UnsupportedVersion(data.version));
        }
        if data.owner != STORE_OWNER {
            return Err(TrustedDevicesError::NonServerOwnedStore);
        }
        Ok(data)
    }

    /// All trusted devices in the Settings/wire shape.
    pub fn list(&self) -> Vec<TrustedDeviceInfo> {
        self.data.devices.iter().map(TrustedDevice::to_info).collect()
    }

    /// Whether `device_id` matches a pinned record — the strict pin check the
    /// approval gate short-circuits on. An unpinned id is simply unknown.
    pub fn is_trusted(&self, device_id: &DeviceId) -> bool {
        let hex = encode_hex(device_id);
        self.data.devices.iter().any(|device| device.device_id == hex)
    }

    /// Whether an already-trusted device already uses `name` as its label — an
    /// informational hint for the approval prompt (FR-005), NEVER a trust key.
    /// Because a pending device is not yet in the store, any match is by
    /// definition a different, already-trusted device.
    pub fn name_collision(&self, name: &str) -> bool {
        self.data.devices.iter().any(|device| device.label == name)
    }

    /// The stored label for `device_id`, or `None` when no record matches. Used to
    /// name the device in the `lan: revoked` audit line before its pin is removed
    /// (feature 014 T027); display-only, never a trust key.
    pub fn label_for(&self, device_id: &DeviceId) -> Option<String> {
        let hex = encode_hex(device_id);
        self.data
            .devices
            .iter()
            .find(|device| device.device_id == hex)
            .map(|device| device.label.clone())
    }

    /// Persist the approval of `request` as a [`TrustedDevice`] and return the
    /// stored record in its Settings/wire shape. Idempotent: re-approving an
    /// already-trusted `device_id` refreshes its certificate, label, and network
    /// context while preserving the original `first_seen`.
    pub fn approve(
        &mut self,
        request: &ApprovalRequest,
    ) -> Result<TrustedDeviceInfo, TrustedDevicesError> {
        let device_id = encode_hex(&request.device_id);
        let now = unix_time_ms();
        let first_seen = self
            .data
            .devices
            .iter()
            .find(|device| device.device_id == device_id)
            .map_or(now, |existing| existing.first_seen);
        let record = TrustedDevice {
            device_id: device_id.clone(),
            cert_der: encode_hex(&request.cert_der),
            label: request.device_name.clone(),
            first_seen,
            approved_on_network: request.network_id.clone(),
        };
        let info = record.to_info();
        let mut next = self.data.clone();
        next.devices.retain(|device| device.device_id != device_id);
        next.devices.push(record);
        next.updated_at_ms = now;
        self.persist(next)?;
        Ok(info)
    }

    /// Remove the pin for `device_id`, returning whether a record was removed.
    /// After a revoke the device is unknown again and must be re-approved on its
    /// next connection (FR-010); severing any live connection is the caller's job
    /// (via the `device_id -> connection-id` index).
    pub fn revoke(&mut self, device_id: &DeviceId) -> Result<bool, TrustedDevicesError> {
        let hex = encode_hex(device_id);
        let mut next = self.data.clone();
        let before = next.devices.len();
        next.devices.retain(|device| device.device_id != hex);
        if next.devices.len() == before {
            return Ok(false);
        }
        next.updated_at_ms = unix_time_ms();
        self.persist(next)?;
        Ok(true)
    }

    fn persist(&mut self, next: PersistedTrustedDevices) -> Result<(), TrustedDevicesError> {
        let path = self.path.as_deref().ok_or(TrustedDevicesError::NoStateDir)?;
        write_toml_atomic(path, &next)?;
        self.data = next;
        Ok(())
    }
}

// ── Approval state machine ──────────────────────────────────────────────────

/// Maximum concurrent LAN connections held **pending device approval** (analysis
/// S1). Each hold blocks on an owning-user decision, so this is deliberately
/// small — a handful of the user's own devices, never a crowd — and is separate
/// from the tailnet and LAN connection/handshake caps so unapproved dialers can
/// neither exhaust another transport's admission nor accumulate an unbounded
/// backlog of human-decision holds. A dialer arriving with the cap full is
/// refused `Busy`.
pub const MAX_PENDING_APPROVALS: usize = 4;

/// Upper bound on how long a LAN connection may sit **pending approval** before
/// its slot is released and the dialer refused (`Declined`). A backstop against
/// an unapproved dialer holding a scarce pending-approval slot across an
/// unbounded human-decision window, yet generous enough for the owning user to
/// notice the prompt and decide.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// A first-time LAN connection awaiting the owning user's decision (data-model
/// `ApprovalRequest`). Assembled by the accept path from the completed
/// mutual-TLS handshake — the pinned `device_id` and the presented certificate —
/// plus the peer's advertised name, then persisted as a [`TrustedDevice`] on
/// approve or discarded on decline/timeout. It carries no window or session
/// data: nothing is revealed before approval (SEC-001).
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// The peer's pinned `device_id = SHA-256(SPKI)`, taken from the TLS
    /// handshake by the pinning verifier.
    pub device_id: DeviceId,
    /// The peer's presented certificate DER (public identity), persisted for
    /// later re-verify/display on approve.
    pub cert_der: Vec<u8>,
    /// The peer's advertised display name. Becomes the trusted-device label on
    /// approve; display only, never a trust key.
    pub device_name: String,
    /// The trusted network the request arrived on — the label shown on the
    /// prompt.
    pub network_label: String,
    /// The trusted network's record id, persisted as `approved_on_network`.
    pub network_id: Option<String>,
    /// `true` when an already-trusted DIFFERENT device shares this advertised
    /// name — an informational prompt hint (FR-005), never a trust key.
    pub name_collision: bool,
}

impl ApprovalRequest {
    /// The peer's read-aloud fingerprint words for the approval prompt and the
    /// pushed `LanApprovalRequest` (research D8).
    pub fn fingerprint_words(&self) -> String {
        fingerprint_words_for(&self.device_id)
    }
}

/// How a pending [`ApprovalRequest`] resolved (data-model `ApprovalRequest`
/// `state`, past `Pending`). `Declined` and `TimedOut` both refuse the dialer
/// and remember nothing; only `Approved` writes a [`TrustedDevice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// The owning user approved the prompt.
    Approved,
    /// The owning user declined the prompt.
    Declined,
    /// No decision arrived within [`APPROVAL_TIMEOUT`]; the slot was released.
    TimedOut,
}

/// Returned by [`PendingApprovals::begin`] when the concurrent pending-approval
/// cap ([`MAX_PENDING_APPROVALS`]) is already reached; the caller refuses the
/// dialer with `Busy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the LAN pending-approval cap is reached")]
pub struct PendingApprovalCapReached;

/// The runtime registry of LAN connections held pending approval. Correlates a
/// `request_id` (pushed to the owning client in `ServerMessage::LanApprovalRequest`)
/// with the eventual decision (`ClientMessage::LanApprovalDecision`), enforces the
/// concurrent-hold cap, and — via [`ApprovalTicket`] — the per-hold timeout.
/// Shared as an `Arc` between the accept path (which begins and awaits holds) and
/// the decision handler (which resolves them).
pub struct PendingApprovals {
    /// Live pending holds keyed by `request_id`, each carrying the channel that
    /// delivers the decision to the awaiting accept task. Guarded by a std mutex:
    /// every critical section is a couple of non-blocking map operations and
    /// never spans an `.await`.
    pending: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    /// Monotonic allocator for `request_id`s. Never reused within a process.
    next_id: AtomicU64,
}

impl PendingApprovals {
    /// Create an empty registry.
    pub fn new() -> Arc<Self> {
        Arc::new(Self { pending: Mutex::new(HashMap::new()), next_id: AtomicU64::new(0) })
    }

    /// Reserve a pending-approval hold, or fail with [`PendingApprovalCapReached`]
    /// when [`MAX_PENDING_APPROVALS`] are already outstanding. On success the
    /// returned [`ApprovalTicket`] carries the allocated `request_id` (to push in
    /// the prompt) and awaits the decision; dropping it releases the hold.
    pub fn begin(self: &Arc<Self>) -> Result<ApprovalTicket, PendingApprovalCapReached> {
        let (decision_tx, decision_rx) = oneshot::channel();
        let request_id = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if pending.len() >= MAX_PENDING_APPROVALS {
                return Err(PendingApprovalCapReached);
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            pending.insert(id, decision_tx);
            id
        };
        Ok(ApprovalTicket { manager: Arc::clone(self), request_id, decision: Some(decision_rx) })
    }

    /// Deliver the owning user's decision for `request_id`, returning whether a
    /// matching pending hold existed. Called by the approval-decision handler on
    /// `ClientMessage::LanApprovalDecision`. A stale or duplicate id is a no-op
    /// (`false`).
    pub fn resolve(&self, request_id: u64, approve: bool) -> bool {
        let sender = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            pending.remove(&request_id)
        };
        sender.is_some_and(|decision_tx| {
            // The receiver may already be gone (the hold timed out and was
            // dropped); delivering to it then fails harmlessly.
            if decision_tx.send(approve).is_err() {
                tracing::debug!("LAN approval decision target already gone");
            }
            true
        })
    }

    /// Remove a hold once its ticket is done, releasing its slot. Idempotent with
    /// [`resolve`](Self::resolve), which also removes on delivery.
    fn release(&self, request_id: u64) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.remove(&request_id);
    }
}

/// A reserved pending-approval hold. Owns the registry slot for its lifetime:
/// [`wait`](Self::wait) resolves it against the per-hold timeout, and dropping it
/// (on any path — decided, timed out, or the accept task bailing early) releases
/// the slot, so a hold can never leak against [`MAX_PENDING_APPROVALS`].
pub struct ApprovalTicket {
    manager: Arc<PendingApprovals>,
    request_id: u64,
    /// The decision channel, taken by [`wait`](Self::wait). `Option` only so the
    /// value can move out of a type that also has a `Drop` impl.
    decision: Option<oneshot::Receiver<bool>>,
}

impl ApprovalTicket {
    /// The `request_id` to push in `ServerMessage::LanApprovalRequest` and match
    /// against the returning `ClientMessage::LanApprovalDecision`.
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Await the owning user's decision, bounding the wait by `timeout` (callers
    /// pass [`APPROVAL_TIMEOUT`]). A missing decision channel (a lost sender or a
    /// second call) resolves as [`ApprovalOutcome::Declined`] — the fail-safe
    /// direction, revealing nothing. The hold's slot is released when the ticket
    /// drops at the end of this call.
    pub async fn wait(mut self, timeout: Duration) -> ApprovalOutcome {
        let Some(decision) = self.decision.take() else {
            return ApprovalOutcome::Declined;
        };
        match tokio::time::timeout(timeout, decision).await {
            Ok(Ok(true)) => ApprovalOutcome::Approved,
            Ok(Ok(false)) => ApprovalOutcome::Declined,
            Ok(Err(_sender_dropped)) => ApprovalOutcome::Declined,
            Err(_elapsed) => ApprovalOutcome::TimedOut,
        }
    }
}

impl Drop for ApprovalTicket {
    fn drop(&mut self) {
        self.manager.release(self.request_id);
    }
}

// ── Hex + time helpers ──────────────────────────────────────────────────────

/// Lowercase hex of `bytes`, no separators — the on-disk encoding for the
/// `device_id` and certificate byte fields (`TOML` has no native binary type).
fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().flat_map(|&byte| [hex_digit(byte >> 4), hex_digit(byte & 0x0f)]).collect()
}

/// Map a 4-bit nibble to its lowercase hex digit.
fn hex_digit(nibble: u8) -> char {
    let value = nibble & 0x0f;
    char::from(if value < 10 { b'0' + value } else { b'a' + value - 10 })
}

/// Parse one lowercase/uppercase hex digit to its 4-bit value, or `None` for a
/// non-hex byte.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode a hex string back to a 32-byte [`DeviceId`], or `None` if it is not
/// exactly 64 hex digits. Used to render the fingerprint for the Settings list
/// and to turn the wire `RevokeTrustedDevice` hex back into the pin key.
pub fn decode_device_id_hex(hex: &str) -> Option<DeviceId> {
    let raw = hex.as_bytes();
    if raw.len() != DEVICE_ID_LEN * 2 {
        return None;
    }
    let mut out = [0u8; DEVICE_ID_LEN];
    for (slot, pair) in out.iter_mut().zip(raw.chunks_exact(2)) {
        match pair {
            [high, low] => *slot = (hex_nibble(*high)? << 4) | hex_nibble(*low)?,
            _ => return None,
        }
    }
    Some(out)
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

// ── Atomic owner-private persistence ────────────────────────────────────────

fn write_toml_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), TrustedDevicesError> {
    ensure_private_parent(path)?;
    let content = toml::to_string_pretty(value)
        .map_err(|error| TrustedDevicesError::Serialize(error.to_string()))?;
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

fn ensure_private_parent(path: &Path) -> Result<(), TrustedDevicesError> {
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
        path.file_name().and_then(|name| name.to_str()).unwrap_or("lan_trusted_devices");
    path.with_file_name(format!(".{file_name}.{}.{}.tmp", std::process::id(), unix_time_ms()))
}
