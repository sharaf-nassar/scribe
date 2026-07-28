//! Feature 013/014/015 remote-connect surface, ported into the GPUI rebuild.
//!
//! Owns the transport-free picker state machine that lets a user reach a Scribe
//! window on another of their machines over either transport — the 013 tailnet
//! path or the 014 direct-LAN path — plus the 013 auto-reconnect overlay. The
//! module is the rendering-independent core of the winit client's
//! [`remote_connect.rs`](../../scribe-client/src/remote_connect.rs): the merge /
//! dedup / step-transition logic and the typed [`RemoteConnectAction`] intents
//! are ported byte-for-byte, while the winit GPU painting is dropped
//! in favour of the flattened [`PickerView`] the GPUI chrome will consume.
//!
//! The picker only produces intents; the app layer turns each into a
//! `ListRemotePeers` request, a window-list probe, or a spawned remote-control
//! client process (via the [`crate::remote_handshake`] dial-env spawn).

use scribe_common::config::{RemoteConfig, SharingMode};
use scribe_common::ids::WindowId;
use scribe_common::protocol::{
    LanPeerInfo, LanRefusal, REMOTE_PROTOCOL_VERSION, RemotePeerInfo, RemoteRefusal, WindowInfo,
};

/// Which transport a merged peer row dials over (feature 014, T024). A UX label
/// and a routing tag — NOT a trust identity: the LAN (`device_id = SHA-256(SPKI)`)
/// and tailnet (`MagicDNS`) namespaces are distinct and are matched only
/// heuristically by machine name (a best-effort convenience, analysis C3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTransport {
    /// The feature 013 tailnet path (`MagicDNS` / IP over plain TCP).
    Tailnet,
    /// The feature 014 LAN path (mDNS-discovered, mutual TLS + device approval).
    Lan,
}

impl PeerTransport {
    /// Short human label shown on the picker row, the controlled-window transport
    /// indicator (T025), and failure copy (FR-009).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tailnet => "Tailscale",
            Self::Lan => "Local network",
        }
    }

    /// Stable sort rank so the preferred (LAN) path leads a same-named pair.
    fn rank(self) -> u8 {
        match self {
            Self::Lan => 0,
            Self::Tailnet => 1,
        }
    }
}

/// Feature 013 (T009): typed outcome of a remote (tailnet) dial + preamble
/// handshake, folded into the picker as the dial settles. `Accepted` keeps the
/// window step loading until the list arrives; `Refused` / `ConnectionFailure`
/// map to distinct UX-002 failure copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteConnectOutcome {
    /// Preamble accepted; the connection now behaves exactly like a local one.
    Accepted,
    /// The server answered with a typed refusal.
    Refused(RemoteRefusal),
    /// Connect refused/timed out, or the link closed before a reply arrived.
    ConnectionFailure,
}

/// Feature 014 (T015): typed outcome of a LAN dial — the mutual-TLS handshake,
/// the `LanHello` preamble, and the owning side's device-approval gate. The LAN
/// analogue of [`RemoteConnectOutcome`], differing only in that a refusal carries
/// a [`LanRefusal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanConnectOutcome {
    /// Approved (or an already-trusted device); the link behaves like a remote one.
    Accepted,
    /// The peer refused with a typed [`LanRefusal`].
    Refused(LanRefusal),
    /// The TCP connect, the TLS handshake, or the framed exchange failed.
    ConnectionFailure,
}

/// The intent a key press produces on the picker. Feature 015: a default connect
/// dials `Hello { window_id, takeover: false }`; the server joins the share
/// additively in a shared mode or returns the legacy `LostControl` (with
/// explicit-reclaim banner) in `SingleController`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteConnectAction {
    /// Nothing changed; no redraw required.
    None,
    /// Visible state changed; the caller should request a redraw.
    Redraw,
    /// The picker was dismissed; the caller should close it and redraw.
    Close,
    /// Re-request both peer lists (tailnet + LAN) from the local server.
    RequestPeers,
    /// Dial `host:port` over `transport` only to enumerate its windows.
    ProbeWindows { host: String, port: u16, transport: PeerTransport },
    /// Attach to an existing window on the peer over `transport`.
    Attach { host: String, port: u16, window_id: WindowId, transport: PeerTransport },
    /// Create a fresh window on the peer over `transport` (`Hello { window_id: None }`).
    NewWindow { host: String, port: u16, transport: PeerTransport },
    /// Paste the host clipboard into the manual `host:port` entry.
    PasteManual,
}

/// A framework-neutral key event the picker understands. The GPUI view lowers a
/// `KeyDownEvent` (and modifier state) into this shape at the call site, so the
/// state machine stays testable without a display server. The paste shortcut
/// (Ctrl/Super+V) resolves to [`PickerKey::Paste`] before it reaches the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKey {
    Escape,
    Enter,
    Up,
    Down,
    Tab,
    Backspace,
    Paste,
    Char(char),
}

/// One selectable window on the chosen peer, or the trailing "New window" row.
struct WindowRow {
    /// `Some` for an existing window to claim; `None` for the "New window" row.
    window_id: Option<WindowId>,
    /// Row label (short window id today; workspace names arrive with T011).
    label: String,
    session_count: usize,
    /// Whether the window currently has a connected controller (in-use marker).
    in_use: bool,
    /// Feature 015 (T025): total attached participants (owner + remotes).
    participant_count: usize,
    /// Feature 015 (T025): the window's sharing mode, `None` from a pre-sharing server.
    mode: Option<SharingMode>,
}

/// One selectable device in the merged peer step (feature 014, T024).
struct PeerRow {
    /// Display name (device / machine name).
    name: String,
    /// Dial host for `transport` (tailnet address or LAN subnet address).
    host: String,
    /// Dial port for `transport` (tailnet 46061 / LAN 46062).
    port: u16,
    transport: PeerTransport,
    /// Whether the peer is currently reachable on `transport`.
    online: bool,
    /// `Some(other)` when the SAME machine is also reachable on `other` but was
    /// deduped into this single row (dual-reachable, LAN preferred).
    also_other: Option<PeerTransport>,
}

/// Which step the picker is on.
enum Stage {
    /// Choosing a peer or typing a manual `host:port` target.
    Peers,
    /// A peer was chosen; its window list is loading or shown.
    Windows {
        host: String,
        port: u16,
        transport: PeerTransport,
        label: String,
        loading: bool,
        awaiting_approval: bool,
        rows: Vec<WindowRow>,
    },
    /// Terminal failure with distinct UX-002 copy.
    Failed { lines: Vec<String> },
}

/// State for the remote-connect overlay picker.
pub struct RemoteConnect {
    active: bool,
    stage: Stage,
    /// Same-account tailnet peers from the local server (013).
    peers: Vec<RemotePeerInfo>,
    /// mDNS-discovered LAN peers from the local server (feature 014, T014).
    lan_peers: Vec<LanPeerInfo>,
    /// Merged, deduped, sorted device rows built from `peers` + `lan_peers`.
    rows: Vec<PeerRow>,
    /// Manual `host:port` entry buffer, used when no peer row is chosen.
    manual: String,
    /// Highlighted row within the current stage's list.
    selected: usize,
}

impl Default for RemoteConnect {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteConnect {
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: false,
            stage: Stage::Peers,
            peers: Vec::new(),
            lan_peers: Vec::new(),
            rows: Vec::new(),
            manual: String::new(),
            selected: 0,
        }
    }

    /// Open the picker on the peer step. Both peer sources are cleared so the
    /// caller's fresh `ListRemotePeers` + `ListLanPeers` responses repopulate them.
    pub fn open(&mut self) {
        self.reset();
        self.active = true;
    }

    pub fn close(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.active = false;
        self.stage = Stage::Peers;
        self.peers.clear();
        self.lan_peers.clear();
        self.rows.clear();
        self.manual.clear();
        self.selected = 0;
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Apply the local server's `RemotePeerList` reply (013 tailnet peers).
    /// Ignored unless the picker is still on the peer step.
    pub fn set_peers(&mut self, peers: Vec<RemotePeerInfo>) {
        if !matches!(self.stage, Stage::Peers) {
            return;
        }
        self.peers = peers;
        self.rebuild_rows();
    }

    /// Apply the local server's `LanPeerList` reply (feature 014, T014), merged
    /// with the tailnet peers by machine name (T024). Ignored off the peer step.
    pub fn set_lan_peers(&mut self, peers: Vec<LanPeerInfo>) {
        if !matches!(self.stage, Stage::Peers) {
            return;
        }
        self.lan_peers = peers;
        self.rebuild_rows();
    }

    /// Rebuild the merged, deduped, sorted peer list from the tailnet + LAN
    /// sources (feature 014, T024). Dual-reachable machines collapse to a single
    /// LAN-preferred row; incompatible-version LAN peers are dropped. Online peers
    /// sort first, then by name, then LAN before tailnet.
    fn rebuild_rows(&mut self) {
        let tailnet_port = RemoteConfig::default().port;
        let mut tailnet: Vec<(&RemotePeerInfo, bool)> =
            self.peers.iter().map(|peer| (peer, false)).collect();

        let mut rows: Vec<PeerRow> = Vec::new();
        for lan in &self.lan_peers {
            if lan.protovers != REMOTE_PROTOCOL_VERSION {
                continue;
            }
            let also_other = claim_tailnet_match(&mut tailnet, &lan.host);
            rows.push(PeerRow {
                name: lan.name.clone(),
                host: lan.addr.clone(),
                port: lan.port,
                transport: PeerTransport::Lan,
                online: lan.online,
                also_other,
            });
        }

        for (peer, claimed) in &tailnet {
            if *claimed {
                continue;
            }
            rows.push(PeerRow {
                name: peer.name.clone(),
                host: peer.addr.clone(),
                port: tailnet_port,
                transport: PeerTransport::Tailnet,
                online: peer.online,
                also_other: None,
            });
        }

        rows.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.transport.rank().cmp(&b.transport.rank()))
        });
        self.rows = rows;
        self.clamp_selection();
    }

    /// Apply a window list probed from the chosen peer. Ignored unless the picker
    /// is on the matching peer's window step.
    pub fn set_windows(&mut self, host: &str, port: u16, windows: Vec<WindowInfo>) {
        let Stage::Windows {
            host: cur_host, port: cur_port, loading, awaiting_approval, rows, ..
        } = &mut self.stage
        else {
            return;
        };
        if cur_host != host || *cur_port != port {
            return;
        }
        let mut built: Vec<WindowRow> = windows
            .into_iter()
            .map(|info| WindowRow {
                window_id: Some(info.window_id),
                label: info.window_id.to_string(),
                session_count: info.session_count,
                in_use: info.connected,
                participant_count: info.participant_count,
                mode: info.mode,
            })
            .collect();
        built.push(WindowRow {
            window_id: None,
            label: String::from("New window"),
            session_count: 0,
            in_use: false,
            participant_count: 0,
            mode: None,
        });
        *rows = built;
        *loading = false;
        *awaiting_approval = false;
        self.selected = 0;
    }

    /// Fold a tailnet dial outcome from the window-list probe into the picker.
    /// Only failures matter here; an accepted probe keeps the step loading.
    pub fn on_dial_outcome(&mut self, outcome: RemoteConnectOutcome) {
        if !self.active {
            return;
        }
        match outcome {
            RemoteConnectOutcome::Accepted => {}
            RemoteConnectOutcome::Refused(reason) => {
                self.fail(refusal_lines(reason, &self.peer_label()));
            }
            RemoteConnectOutcome::ConnectionFailure => {
                self.fail(connection_failure_lines(&self.peer_label()));
            }
        }
    }

    /// Fold a LAN dial outcome (TLS + device-approval gate) into the picker
    /// (feature 014, T014) — the LAN analogue of [`Self::on_dial_outcome`].
    pub fn on_lan_dial_outcome(&mut self, outcome: LanConnectOutcome) {
        if !self.active {
            return;
        }
        match outcome {
            LanConnectOutcome::Accepted => {}
            LanConnectOutcome::Refused(reason) => {
                self.fail(lan_refusal_lines(reason, &self.peer_label()));
            }
            LanConnectOutcome::ConnectionFailure => {
                self.fail(lan_connection_failure_lines(&self.peer_label()));
            }
        }
    }

    /// The owning LAN peer is holding this connection pending the user's device
    /// approval (feature 014, T019). Swap the window step's loading note for the
    /// cancelable "Waiting for approval…" overlay. Ignored once the list arrived.
    pub fn on_awaiting_approval(&mut self) {
        if !self.active {
            return;
        }
        if let Stage::Windows { loading: true, awaiting_approval, .. } = &mut self.stage {
            *awaiting_approval = true;
        }
    }

    /// A peer delivered a `RemoteDisconnect` sever notice. Lets the picker state
    /// the disable as fact rather than inferring it from a cold connect failure.
    pub fn on_severed(&mut self, reason: RemoteRefusal) {
        if !self.active {
            return;
        }
        let label = self.peer_label();
        match reason {
            RemoteRefusal::Disabled => self.fail(vec![format!(
                "Remote access was turned off on {label} — connection closed."
            )]),
            other => self.fail(refusal_lines(other, &label)),
        }
    }

    /// Route a key press into the picker, returning the intent the caller must act
    /// on. Callers invoke this only while [`Self::is_active`].
    pub fn handle_key(&mut self, key: PickerKey) -> RemoteConnectAction {
        match &self.stage {
            Stage::Peers => self.handle_peers_key(key),
            Stage::Windows { .. } => self.handle_windows_key(key),
            Stage::Failed { .. } => self.handle_failed_key(key),
        }
    }

    fn handle_peers_key(&mut self, key: PickerKey) -> RemoteConnectAction {
        match key {
            PickerKey::Escape => RemoteConnectAction::Close,
            PickerKey::Enter => self.confirm_peer(),
            PickerKey::Down | PickerKey::Tab => {
                self.move_selection(true, self.rows.len());
                RemoteConnectAction::Redraw
            }
            PickerKey::Up => {
                self.move_selection(false, self.rows.len());
                RemoteConnectAction::Redraw
            }
            PickerKey::Backspace => {
                if self.manual.pop().is_some() {
                    RemoteConnectAction::Redraw
                } else {
                    RemoteConnectAction::None
                }
            }
            PickerKey::Paste => RemoteConnectAction::PasteManual,
            PickerKey::Char(ch) if !ch.is_control() => {
                self.manual.push(ch);
                RemoteConnectAction::Redraw
            }
            PickerKey::Char(_) => RemoteConnectAction::None,
        }
    }

    /// Enter on the peer step: a typed manual target wins over the highlighted
    /// device. Manual entry keeps the 013 tailnet transport; a picked row dials
    /// over the transport its merge chose (feature 014, T024).
    fn confirm_peer(&mut self) -> RemoteConnectAction {
        if let Some((host, port)) = parse_host_port(&self.manual) {
            self.enter_windows_stage(host.clone(), port, PeerTransport::Tailnet);
            return RemoteConnectAction::ProbeWindows {
                host,
                port,
                transport: PeerTransport::Tailnet,
            };
        }
        let Some(row) = self.rows.get(self.selected).filter(|row| row.online) else {
            return RemoteConnectAction::None;
        };
        let host = row.host.clone();
        let port = row.port;
        let transport = row.transport;
        self.enter_windows_stage(host.clone(), port, transport);
        RemoteConnectAction::ProbeWindows { host, port, transport }
    }

    /// Append pasted text to the manual `host:port` entry, dropping control and
    /// whitespace characters (a host target never contains them).
    pub fn append_manual(&mut self, text: &str) {
        self.manual.extend(text.chars().filter(|ch| !ch.is_control() && !ch.is_whitespace()));
    }

    fn handle_windows_key(&mut self, key: PickerKey) -> RemoteConnectAction {
        let row_count = match &self.stage {
            Stage::Windows { rows, .. } => rows.len(),
            _ => 0,
        };
        match key {
            PickerKey::Escape => {
                self.stage = Stage::Peers;
                self.selected = 0;
                RemoteConnectAction::RequestPeers
            }
            PickerKey::Down | PickerKey::Tab => {
                self.move_selection(true, row_count);
                RemoteConnectAction::Redraw
            }
            PickerKey::Up => {
                self.move_selection(false, row_count);
                RemoteConnectAction::Redraw
            }
            PickerKey::Enter => self.confirm_window(),
            _ => RemoteConnectAction::None,
        }
    }

    fn confirm_window(&mut self) -> RemoteConnectAction {
        let Stage::Windows { host, port, transport, loading, rows, .. } = &self.stage else {
            return RemoteConnectAction::None;
        };
        if *loading {
            return RemoteConnectAction::None;
        }
        let Some(row) = rows.get(self.selected) else {
            return RemoteConnectAction::None;
        };
        let host = host.clone();
        let port = *port;
        let transport = *transport;
        match row.window_id {
            Some(window_id) => RemoteConnectAction::Attach { host, port, window_id, transport },
            None => RemoteConnectAction::NewWindow { host, port, transport },
        }
    }

    fn handle_failed_key(&mut self, key: PickerKey) -> RemoteConnectAction {
        match key {
            PickerKey::Escape => RemoteConnectAction::Close,
            PickerKey::Enter => {
                self.stage = Stage::Peers;
                self.selected = 0;
                RemoteConnectAction::RequestPeers
            }
            _ => RemoteConnectAction::None,
        }
    }

    fn enter_windows_stage(&mut self, host: String, port: u16, transport: PeerTransport) {
        let label = self.dial_label(&host);
        self.stage = Stage::Windows {
            host,
            port,
            transport,
            label,
            loading: true,
            awaiting_approval: false,
            rows: Vec::new(),
        };
        self.selected = 0;
    }

    /// Display label for the peer currently being reached — its device name when a
    /// listed peer matches the dial address, else the raw host.
    fn dial_label(&self, host: &str) -> String {
        self.rows
            .iter()
            .find(|row| row.host == host)
            .map_or_else(|| host.to_owned(), |row| row.name.clone())
    }

    /// Best label for the peer in failure copy: the windows-stage label if we got
    /// that far, otherwise the highlighted/typed peer.
    fn peer_label(&self) -> String {
        if let Stage::Windows { label, .. } = &self.stage {
            return label.clone();
        }
        if let Some((host, _)) = parse_host_port(&self.manual) {
            self.dial_label(&host)
        } else if let Some(row) = self.rows.get(self.selected) {
            row.name.clone()
        } else {
            String::from("the remote machine")
        }
    }

    fn fail(&mut self, lines: Vec<String>) {
        self.stage = Stage::Failed { lines };
        self.selected = 0;
    }

    fn move_selection(&mut self, forward: bool, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else if forward {
            self.selected = (self.selected + 1) % count;
        } else {
            self.selected = (self.selected + count - 1) % count;
        }
    }

    fn clamp_selection(&mut self) {
        let count = match &self.stage {
            Stage::Peers => self.rows.len(),
            Stage::Windows { rows, .. } => rows.len(),
            Stage::Failed { .. } => 0,
        };
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Flatten the current stage into a renderer-friendly view (title, an optional
    /// editable/status subtitle, the row list, and a footer hint). No-op-safe when
    /// inactive: the caller checks [`Self::is_active`] before painting.
    #[must_use]
    pub fn view(&self) -> PickerView {
        match &self.stage {
            Stage::Peers => self.peers_view(),
            Stage::Windows { label, loading, awaiting_approval, rows, .. } => {
                let phase = match (*loading, *awaiting_approval) {
                    (true, true) => WindowsPhase::AwaitingApproval,
                    (true, false) => WindowsPhase::Loading,
                    (false, _) => WindowsPhase::Loaded,
                };
                windows_view(label, phase, rows)
            }
            Stage::Failed { lines } => failed_view(lines),
        }
    }

    fn peers_view(&self) -> PickerView {
        let subtitle = if self.manual.is_empty() {
            String::from("Type host:port, or pick a device below")
        } else {
            format!("> {}", self.manual)
        };
        let mut rows: Vec<PickerRow> = self.rows.iter().map(peer_row).collect();
        if rows.is_empty() {
            rows.push(PickerRow {
                text: String::from("No devices found - type a host:port above"),
                dim: true,
            });
        }
        PickerView {
            title: String::from("Connect to remote machine"),
            subtitle: Some(subtitle),
            rows,
            selectable: !self.rows.is_empty(),
            selected: self.selected,
            footer: Some(String::from("Enter connect  Up/Down pick  Esc cancel")),
        }
    }
}

/// Which of the three window-step displays is showing, derived from the stage's
/// `loading` / `awaiting_approval` flags. Collapsing the two bools into one enum
/// keeps [`windows_view`] within the boolean-parameter budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsPhase {
    /// Held pending device approval on the owning LAN peer (feature 014, T019).
    AwaitingApproval,
    /// Dialing / loading the window list.
    Loading,
    /// The window list has arrived.
    Loaded,
}

fn windows_view(label: &str, phase: WindowsPhase, rows: &[WindowRow]) -> PickerView {
    if phase == WindowsPhase::AwaitingApproval {
        return PickerView {
            title: format!("Waiting for approval on {label}…"),
            subtitle: None,
            rows: vec![PickerRow {
                text: String::from("Approve this device on that machine to continue."),
                dim: true,
            }],
            selectable: false,
            selected: 0,
            footer: Some(String::from("Esc cancel")),
        };
    }
    if phase == WindowsPhase::Loading {
        return PickerView {
            title: format!("Connecting to {label}…"),
            subtitle: None,
            rows: vec![PickerRow { text: String::from("Loading windows…"), dim: true }],
            selectable: false,
            selected: 0,
            footer: Some(String::from("Esc cancel")),
        };
    }
    PickerView {
        title: format!("Windows on {label}"),
        subtitle: None,
        rows: rows.iter().map(window_row).collect(),
        selectable: true,
        selected: 0,
        footer: Some(String::from("Enter open  Up/Down pick  Esc back")),
    }
}

fn failed_view(lines: &[String]) -> PickerView {
    let rows: Vec<PickerRow> =
        lines.iter().map(|line| PickerRow { text: line.clone(), dim: false }).collect();
    PickerView {
        title: String::from("Couldn't connect"),
        subtitle: None,
        rows,
        selectable: false,
        selected: 0,
        footer: Some(String::from("Enter retry  Esc close")),
    }
}

/// What a key/click on the [`ReconnectOverlay`] asks the app to do (feature 013,
/// T030). Transport-free: it returns an intent and the app owns the side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectAction {
    /// No affordance for this input in the current stage.
    None,
    /// Cancel the in-progress auto-reconnect and settle.
    Cancel,
    /// One-action reconnect from a settled state (a fresh attach).
    Reconnect,
    /// Close the window from a settled state.
    Close,
}

/// Feature 013 (T030): the auto-reconnect overlay for a controlling-side window
/// whose remote link dropped. Distinct from the [`RemoteConnect`] picker (which
/// runs only during the initial connect): this appears on an already-attached
/// window and is a full-window modal while active.
pub struct ReconnectOverlay {
    /// Display label for the peer being reached (host / `MagicDNS` name).
    peer: String,
    stage: ReconnectStage,
}

enum ReconnectStage {
    /// Actively retrying with capped backoff; cancelable.
    Reconnecting { attempt: u32 },
    /// Settled (cancel / disabled / gave up); the copy explains why.
    Settled { lines: Vec<String> },
}

impl ReconnectOverlay {
    /// Enter the reconnecting state for `peer` at `attempt` (1-based).
    #[must_use]
    pub fn reconnecting(peer: String, attempt: u32) -> Self {
        Self { peer, stage: ReconnectStage::Reconnecting { attempt } }
    }

    /// Advance the visible attempt counter while still retrying. Ignored once
    /// settled, so a late attempt event cannot revive the spinner.
    pub fn set_attempt(&mut self, attempt: u32) {
        if let ReconnectStage::Reconnecting { attempt: current } = &mut self.stage {
            *current = attempt;
        }
    }

    /// Build the settled state after the user cancelled the auto-reconnect.
    #[must_use]
    pub fn settled_cancelled(peer: String) -> Self {
        let lines = vec![format!("Disconnected from {peer}.")];
        Self { peer, stage: ReconnectStage::Settled { lines } }
    }

    /// Build the settled state after the capped backoff was exhausted — the
    /// combined connection-failure copy (offline / not running / disabled).
    #[must_use]
    pub fn settled_unreachable(peer: String) -> Self {
        let lines = connection_failure_lines(&peer);
        Self { peer, stage: ReconnectStage::Settled { lines } }
    }

    /// Build the settled state after an authoritative refusal / sever notice.
    #[must_use]
    pub fn settled_refused(peer: String, reason: RemoteRefusal) -> Self {
        let lines = match reason {
            RemoteRefusal::Disabled => {
                vec![format!("Remote access was turned off on {peer} — connection closed.")]
            }
            other => refusal_lines(other, &peer),
        };
        Self { peer, stage: ReconnectStage::Settled { lines } }
    }

    #[must_use]
    pub fn is_settled(&self) -> bool {
        matches!(self.stage, ReconnectStage::Settled { .. })
    }

    /// Map a key press to the app action for the current stage.
    #[must_use]
    pub fn key_action(&self, key: PickerKey) -> ReconnectAction {
        match self.stage {
            ReconnectStage::Reconnecting { .. } => match key {
                PickerKey::Escape => ReconnectAction::Cancel,
                _ => ReconnectAction::None,
            },
            ReconnectStage::Settled { .. } => match key {
                PickerKey::Enter => ReconnectAction::Reconnect,
                PickerKey::Escape => ReconnectAction::Close,
                _ => ReconnectAction::None,
            },
        }
    }

    /// The action a mouse click performs: a settled overlay reconnects; a retrying
    /// one ignores it so a stray click cannot cancel.
    #[must_use]
    pub fn click_action(&self) -> ReconnectAction {
        if self.is_settled() { ReconnectAction::Reconnect } else { ReconnectAction::None }
    }

    /// Flatten the current stage into a renderer-friendly [`PickerView`].
    #[must_use]
    pub fn view(&self) -> PickerView {
        match &self.stage {
            ReconnectStage::Reconnecting { attempt } => PickerView {
                title: format!("Reconnecting to {}…", self.peer),
                subtitle: None,
                rows: vec![PickerRow { text: format!("Attempt {attempt}"), dim: true }],
                selectable: false,
                selected: 0,
                footer: Some(String::from("Esc cancel")),
            },
            ReconnectStage::Settled { lines } => PickerView {
                title: String::from("Disconnected"),
                subtitle: None,
                rows: lines
                    .iter()
                    .map(|line| PickerRow { text: line.clone(), dim: false })
                    .collect(),
                selectable: false,
                selected: 0,
                footer: Some(String::from("Enter reconnect  Esc close")),
            },
        }
    }
}

/// A flattened, renderer-ready row.
pub struct PickerRow {
    pub text: String,
    /// Rendered greyed (offline peer, placeholder, loading note).
    pub dim: bool,
}

/// A flattened, renderer-ready snapshot of the current stage, consumed by the
/// GPUI chrome in place of the winit client's GPU quad list.
pub struct PickerView {
    pub title: String,
    pub subtitle: Option<String>,
    pub rows: Vec<PickerRow>,
    /// Whether the row list responds to selection (false for status-only rows).
    pub selectable: bool,
    pub selected: usize,
    pub footer: Option<String>,
}

/// Build a merged peer-step row: an online marker, the device name, its transport
/// label, and — for a dual-reachable machine shown once — an "also on …" hint.
fn peer_row(row: &PeerRow) -> PickerRow {
    let marker = if row.online { "* " } else { "  " };
    let label = row.transport.label();
    let text = row.also_other.map_or_else(
        || format!("{marker}{}  {label}", row.name),
        |other| format!("{marker}{}  {label}  (also {})", row.name, other.label()),
    );
    PickerRow { text, dim: !row.online }
}

/// Best-effort machine-name match key for LAN↔tailnet dedup (feature 014, T024):
/// the first dot-delimited label, trimmed and lowercased. `None` for an empty
/// name. A UX heuristic, never a trust key (analysis C3).
fn name_match_key(name: &str) -> Option<String> {
    let label = name.trim().split('.').next().unwrap_or("").trim();
    if label.is_empty() { None } else { Some(label.to_ascii_lowercase()) }
}

/// Claim the first not-yet-claimed tailnet peer whose machine name matches
/// `lan_host` (feature 014, T024 dedup), marking it consumed and returning the
/// "also reachable on Tailscale" hint for the LAN row.
fn claim_tailnet_match(
    tailnet: &mut [(&RemotePeerInfo, bool)],
    lan_host: &str,
) -> Option<PeerTransport> {
    let key = name_match_key(lan_host)?;
    let entry = tailnet.iter_mut().find(|(peer, claimed)| {
        !*claimed && name_match_key(&peer.name).as_deref() == Some(key.as_str())
    })?;
    entry.1 = true;
    Some(PeerTransport::Tailnet)
}

/// Build a window-step row: the "New window" entry, or an existing window with its
/// session count and (feature 015) live share occupancy or in-use marker.
fn window_row(row: &WindowRow) -> PickerRow {
    let text = if row.window_id.is_none() {
        String::from("+ New window")
    } else {
        let occupancy = if row.participant_count >= 2 {
            format!("  {} \u{00B7} {} attached", share_kind_label(row.mode), row.participant_count)
        } else if row.in_use {
            String::from("  (in use)")
        } else {
            String::new()
        };
        format!("{}  {} session(s){occupancy}", row.label, row.session_count)
    };
    PickerRow { text, dim: false }
}

/// Feature 015 (T025): the occupancy prefix for a shared window's picker row.
fn share_kind_label(mode: Option<SharingMode>) -> &'static str {
    match mode {
        Some(SharingMode::FreeForAll) => "free-for-all",
        Some(SharingMode::SharedSingleTypist | SharingMode::SingleController) | None => "shared",
    }
}

/// Map a typed [`RemoteRefusal`] to its distinct UX-002 failure copy.
fn refusal_lines(reason: RemoteRefusal, peer: &str) -> Vec<String> {
    match reason {
        RemoteRefusal::Disabled => vec![
            format!("Remote access is turned off on {peer}."),
            String::from("Enable it in Scribe Settings on that machine."),
        ],
        RemoteRefusal::Unauthorized => vec![format!(
            "{peer} refused: this device isn't signed in as the same Tailscale account."
        )],
        RemoteRefusal::IdentityUnavailable => vec![format!(
            "{peer} can't verify device identity right now (Tailscale unavailable there)."
        )],
        RemoteRefusal::IncompatibleVersion => vec![format!(
            "Scribe versions don't match between this machine ({}) and {peer}. Update the older one.",
            env!("CARGO_PKG_VERSION")
        )],
        RemoteRefusal::Busy => {
            vec![format!("{peer} has too many remote connections right now.")]
        }
    }
}

/// The combined FR-004 connection-failure copy: a disabled peer has no listener,
/// so offline / not-running / disabled are deliberately indistinguishable here.
fn connection_failure_lines(peer: &str) -> Vec<String> {
    vec![format!(
        "Can't reach {peer} — it may be offline, Scribe may not be running, or remote access may be turned off there."
    )]
}

/// Map a typed [`LanRefusal`] to its distinct UX-002 failure copy — the LAN
/// analogue of [`refusal_lines`].
fn lan_refusal_lines(reason: LanRefusal, peer: &str) -> Vec<String> {
    match reason {
        LanRefusal::Declined => vec![format!("{peer} declined this device.")],
        LanRefusal::NotTrustedNetwork => {
            vec![format!("{peer} isn't accepting local connections on this network.")]
        }
        LanRefusal::Disabled => vec![format!("Local remote access is turned off on {peer}.")],
        LanRefusal::IncompatibleVersion => vec![format!(
            "Scribe versions don't match between this machine ({}) and {peer}. Update the older one.",
            env!("CARGO_PKG_VERSION")
        )],
        LanRefusal::Busy => {
            vec![format!("{peer} has too many remote connections right now.")]
        }
    }
}

/// The LAN connection-failure copy: a dormant or absent peer leaves nothing
/// listening, so offline / asleep / off-network are indistinguishable (FR-004).
fn lan_connection_failure_lines(peer: &str) -> Vec<String> {
    vec![format!(
        "Can't reach {peer} on the local network — it may be offline, asleep, or not on this network."
    )]
}

/// Parse a manual `host` or `host:port` entry, defaulting the port to the
/// configured remote port. Mirrors the `SCRIBE_REMOTE_DIAL` env parser so a bare
/// IPv6 literal falls through to the default port and is dialed verbatim.
fn parse_host_port(input: &str) -> Option<(String, u16)> {
    crate::remote_handshake::parse_dial_target(input, RemoteConfig::default().port)
}

#[cfg(test)]
mod tests;
