//! Feature 013 (T014/T018) + feature 014 (T014/T024): the "Connect to remote
//! machine…" flow.
//!
//! Owns the command-palette-launched GPU overlay picker that lets a user reach
//! a Scribe window on another of their machines over either transport — the 013
//! tailnet path or the 014 direct-LAN path. The flow has two forward steps plus a
//! terminal failure step:
//!
//! 1. **Peers** — the merged device list: same-account online tailnet peers
//!    ([`ClientMessage::ListRemotePeers`], 013) and mDNS-discovered LAN peers
//!    ([`ClientMessage::ListLanPeers`], 014), deduped by machine name so a
//!    dual-reachable machine appears once with the direct LAN path preferred
//!    (T024, a best-effort UX heuristic — NOT a trust identity), each row
//!    labeled with its transport, plus a manual `host:port` entry field.
//! 2. **Windows** — the chosen peer's windows (session counts + in-use marker)
//!    plus a synthetic "New window" entry. Attaching sends
//!    `Hello { window_id, takeover: false }` (feature 015: additive share join in
//!    a shared mode, legacy `LostControl`/reclaim in `SingleController`); "New
//!    window" sends `Hello { window_id: None }` (T018).
//! 3. **Failed** — a terminal outcome rendered with the distinct UX-002 copy
//!    per [`RemoteRefusal`] variant, plus the combined connection-failure
//!    wording (contracts/settings-and-config.md failure-copy table).
//!
//! Rendering mirrors the existing overlay chrome (`command_palette.rs`,
//! `close_dialog.rs`): a full-viewport backdrop, a centered bordered box, and a
//! selectable row list drawn as [`CellInstance`] quads in the terminal GPU pass.
//! The module is intentionally transport-free: it only produces a
//! [`RemoteConnectAction`] intent, which the app layer turns into a
//! `ListRemotePeers` request, a window-list probe, or a spawned remote-control
//! client process.

use scribe_common::config::{RemoteConfig, SharingMode};
use scribe_common::ids::WindowId;
use scribe_common::protocol::{
    LanPeerInfo, LanRefusal, REMOTE_PROTOCOL_VERSION, RemotePeerInfo, RemoteRefusal, WindowInfo,
};
use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::ipc_client::{LanConnectOutcome, RemoteConnectOutcome};
use crate::layout::Rect;

/// Minimum / maximum overlay width in grid columns; clamps the box to a
/// readable size regardless of the longest peer or window label.
const MIN_COLS: usize = 44;
const MAX_COLS: usize = 78;
/// Longest run of list rows drawn before the list is truncated; keeps the
/// overlay bounded on machines with many peers or windows.
const MAX_ROWS: usize = 10;
/// Overlay layout never needs more than this many grid units, which keeps the
/// integer-to-float conversion exact for pixel placement (mirrors the sibling
/// overlays).
const MAX_GRID_UNITS: usize = 65_535;

type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

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

/// Everything the app layer must act on after routing a key into the picker.
///
/// The picker never touches the network itself; it hands one of these back and
/// the caller performs the side effect (send `ListRemotePeers`/`ListLanPeers`,
/// dial a probe, or spawn a remote-control client process over the chosen
/// [`PeerTransport`]).
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
    /// Dial `host:port` over `transport` only to enumerate its windows (no window
    /// claim yet). The LAN transport runs the full TLS + device-approval gate.
    ProbeWindows { host: String, port: u16, transport: PeerTransport },
    /// Attach to an existing window on the peer over `transport`. Feature 015: a
    /// default connect dials `Hello { window_id, takeover: false }` — the server
    /// joins the share additively in a shared mode, or returns the legacy
    /// `LostControl` (with explicit-reclaim banner) in `SingleController`.
    Attach { host: String, port: u16, window_id: WindowId, transport: PeerTransport },
    /// Create a fresh window on the peer over `transport` (`Hello { window_id:
    /// None }`, T018).
    NewWindow { host: String, port: u16, transport: PeerTransport },
    /// Paste the host clipboard into the manual `host:port` entry. The app layer
    /// owns the clipboard handle, so it reads the text and calls
    /// [`RemoteConnect::append_manual`].
    PasteManual,
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
    /// Feature 015 (T025): total attached participants (owner + remotes) from the
    /// enriched `WindowInfo`. `>= 2` means the window is actively shared, driving
    /// the "shared · N attached" occupancy text instead of the binary in-use flag.
    participant_count: usize,
    /// Feature 015 (T025): the window's sharing mode, `None` from a pre-sharing
    /// server. Reserved for mode-specific occupancy copy.
    mode: Option<SharingMode>,
}

/// One selectable device in the merged peer step (feature 014, T024). Tailnet
/// peers (013 [`RemotePeerInfo`]) and LAN peers ([`LanPeerInfo`]) are merged by
/// machine name into these rows; each records the [`PeerTransport`] it dials over
/// and whether the same machine is also reachable on the other transport
/// (deduped away, LAN preferred).
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
    /// deduped into this single row (dual-reachable, LAN preferred) — drives an
    /// "also on …" hint so the user sees the fallback exists (FR-008).
    also_other: Option<PeerTransport>,
}

/// Which step the picker is on. Peer-step data (`peers`, `lan_peers`, `rows`,
/// `manual`) lives on the parent so it survives a round-trip into the windows
/// step and back.
enum Stage {
    /// Choosing a peer or typing a manual `host:port` target.
    Peers,
    /// A peer was chosen; its window list is loading or shown. `transport` records
    /// which path the probe/attach dials over (feature 014, T024). While `loading`
    /// over the LAN transport, `awaiting_approval` flips true once the owning peer
    /// reports the connection is held pending device approval
    /// ([`LanApprovalPending`](scribe_common::protocol::ServerMessage::LanApprovalPending)),
    /// swapping the "Loading windows…" note for the cancelable "Waiting for
    /// approval on <peer>…" overlay (feature 014, T019, FR-014); it clears when the
    /// window list arrives (approved) and is irrelevant once the stage leaves
    /// loading.
    Windows {
        host: String,
        port: u16,
        transport: PeerTransport,
        label: String,
        loading: bool,
        awaiting_approval: bool,
        rows: Vec<WindowRow>,
    },
    /// Terminal failure with distinct UX-002 copy. `Enter` returns to the peer
    /// step; `Esc` closes.
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
    /// Merged, deduped, sorted device rows built from `peers` + `lan_peers`
    /// (feature 014, T024); the selectable peer-step list and the `selected`
    /// index target. Rebuilt on every `set_peers` / `set_lan_peers`.
    rows: Vec<PeerRow>,
    /// Manual `host:port` entry buffer, used when no peer row is chosen.
    manual: String,
    /// Highlighted row within the current stage's list.
    selected: usize,
}

impl RemoteConnect {
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
    /// caller's fresh `ListRemotePeers` + `ListLanPeers` responses repopulate them
    /// (no stale device list).
    pub fn open(&mut self) {
        self.active = true;
        self.stage = Stage::Peers;
        self.peers.clear();
        self.lan_peers.clear();
        self.rows.clear();
        self.manual.clear();
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.stage = Stage::Peers;
        self.peers.clear();
        self.lan_peers.clear();
        self.rows.clear();
        self.manual.clear();
        self.selected = 0;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Apply the local server's [`RemotePeerList`](scribe_common::protocol::ServerMessage::RemotePeerList)
    /// reply (013 tailnet peers). Ignored unless the picker is still on the peer
    /// step; the merged row list is rebuilt (feature 014, T024).
    pub fn set_peers(&mut self, peers: Vec<RemotePeerInfo>) {
        if !matches!(self.stage, Stage::Peers) {
            return;
        }
        self.peers = peers;
        self.rebuild_rows();
    }

    /// Apply the local server's [`LanPeerList`](scribe_common::protocol::ServerMessage::LanPeerList)
    /// reply (feature 014, T014): the mDNS-discovered LAN peers, merged with the
    /// tailnet peers by machine name (T024). Ignored unless the picker is still on
    /// the peer step.
    pub fn set_lan_peers(&mut self, peers: Vec<LanPeerInfo>) {
        if !matches!(self.stage, Stage::Peers) {
            return;
        }
        self.lan_peers = peers;
        self.rebuild_rows();
    }

    /// Rebuild the merged, deduped, sorted peer list from the tailnet + LAN
    /// sources (feature 014, T024). A LAN peer and a tailnet peer whose machine
    /// names match confidently collapse to a single LAN-preferred row
    /// (dual-reachable, FR-008); every other peer keeps its own transport-labeled
    /// row. Incompatible-version LAN peers are dropped before offering them (the
    /// exact-match policy would refuse the dial anyway, data-model `LanPeer`).
    /// Online peers sort first, then by name, then LAN before tailnet.
    fn rebuild_rows(&mut self) {
        let tailnet_port = RemoteConfig::default().port;
        // Pair each tailnet peer with a "claimed by a LAN name match" flag so a
        // dual-reachable machine is emitted once, LAN preferred (FR-008). A flag
        // Vec walked with iterators keeps this index-free (repo denies indexing).
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

        // Remaining (unclaimed) tailnet peers keep their own labeled rows.
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

        // Online first (easiest to pick; offline stay greyed), then by name, then
        // LAN before tailnet so the preferred path leads any same-named pair.
        rows.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.transport.rank().cmp(&b.transport.rank()))
        });
        self.rows = rows;
        self.clamp_selection();
    }

    /// Apply a window list probed from the chosen peer. Ignored unless the
    /// picker is on the matching peer's window step.
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
        // Trailing synthetic entry — always available even when the peer has no
        // existing windows (remote create, clarification #3 / T018).
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
        // The window list only arrives after the owning peer approved this device,
        // so any "Waiting for approval…" overlay has settled (feature 014, T019).
        *awaiting_approval = false;
        self.selected = 0;
    }

    /// Fold a dial outcome from the window-list probe into the picker. Only
    /// failures matter here: an accepted probe keeps the window step loading
    /// until [`set_windows`](Self::set_windows) arrives.
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
    /// (feature 014, T014) — the LAN analogue of [`on_dial_outcome`](Self::on_dial_outcome).
    /// Only failures matter here: an accepted LAN window probe keeps the window
    /// step loading until [`set_windows`](Self::set_windows) arrives, and an
    /// accepted attach spawns its own client-window process. Each [`LanRefusal`]
    /// maps to its distinct UX-002 copy; the merged connection failure is the
    /// LAN-specific "can't reach on the local network" wording.
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
    /// approval (feature 014, T019): the window probe received
    /// [`LanApprovalPending`](scribe_common::protocol::ServerMessage::LanApprovalPending).
    /// Swap the window step's "Loading windows…" note for the cancelable "Waiting
    /// for approval on <peer>…" overlay (FR-014, US2.5). Only meaningful while the
    /// window step is still loading; ignored otherwise (a stray pending after the
    /// list already arrived, or when the picker has moved on / closed), so a late
    /// event never resurrects the overlay. Settles when the list arrives (approved,
    /// [`set_windows`](Self::set_windows)) or the dial is refused
    /// ([`on_lan_dial_outcome`](Self::on_lan_dial_outcome)); Esc cancels by
    /// stepping back to the peer list.
    pub fn on_awaiting_approval(&mut self) {
        if !self.active {
            return;
        }
        if let Stage::Windows { loading: true, awaiting_approval, .. } = &mut self.stage {
            *awaiting_approval = true;
        }
    }

    /// A peer delivered a [`RemoteDisconnect`](scribe_common::protocol::ServerMessage::RemoteDisconnect)
    /// sever notice. Lets the picker state the disable as fact rather than
    /// inferring it from a cold connect failure (contracts Disable semantics).
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

    /// Route a key press into the picker, returning the intent the caller must
    /// act on. Callers invoke this only while [`is_active`](Self::is_active).
    pub fn handle_key(
        &mut self,
        event: &KeyEvent,
        modifiers: ModifiersState,
    ) -> RemoteConnectAction {
        match &self.stage {
            Stage::Peers => self.handle_peers_key(event, modifiers),
            Stage::Windows { .. } => self.handle_windows_key(event),
            Stage::Failed { .. } => self.handle_failed_key(event),
        }
    }

    fn handle_peers_key(
        &mut self,
        event: &KeyEvent,
        modifiers: ModifiersState,
    ) -> RemoteConnectAction {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => RemoteConnectAction::Close,
            Key::Named(NamedKey::Enter) => self.confirm_peer(),
            Key::Named(NamedKey::ArrowDown | NamedKey::Tab) => {
                self.move_selection(true, self.rows.len());
                RemoteConnectAction::Redraw
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.move_selection(false, self.rows.len());
                RemoteConnectAction::Redraw
            }
            Key::Named(NamedKey::Backspace) => {
                if self.manual.pop().is_some() {
                    RemoteConnectAction::Redraw
                } else {
                    RemoteConnectAction::None
                }
            }
            Key::Character(text)
                if (modifiers.control_key() || modifiers.super_key())
                    && !modifiers.alt_key()
                    && text.eq_ignore_ascii_case("v") =>
            {
                RemoteConnectAction::PasteManual
            }
            Key::Character(text)
                if !modifiers.control_key() && !modifiers.alt_key() && !modifiers.super_key() =>
            {
                let mut changed = false;
                for ch in text.chars().filter(|ch| !ch.is_control()) {
                    self.manual.push(ch);
                    changed = true;
                }
                if changed { RemoteConnectAction::Redraw } else { RemoteConnectAction::None }
            }
            _ => RemoteConnectAction::None,
        }
    }

    /// Enter on the peer step: a typed manual target wins over the highlighted
    /// device, so a user who started typing always reaches what they typed. Manual
    /// entry keeps the 013 tailnet transport (the LAN source is the discovered-peer
    /// path); a picked row dials over the transport its merge chose (feature 014,
    /// T024).
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
    /// whitespace characters (a host target never contains them, and this also
    /// strips the trailing newline a clipboard copy usually carries).
    pub fn append_manual(&mut self, text: &str) {
        self.manual.extend(text.chars().filter(|ch| !ch.is_control() && !ch.is_whitespace()));
    }

    fn handle_windows_key(&mut self, event: &KeyEvent) -> RemoteConnectAction {
        let row_count = match &self.stage {
            Stage::Windows { rows, .. } => rows.len(),
            _ => 0,
        };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.stage = Stage::Peers;
                self.selected = 0;
                RemoteConnectAction::RequestPeers
            }
            Key::Named(NamedKey::ArrowDown | NamedKey::Tab) => {
                self.move_selection(true, row_count);
                RemoteConnectAction::Redraw
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.move_selection(false, row_count);
                RemoteConnectAction::Redraw
            }
            Key::Named(NamedKey::Enter) => self.confirm_window(),
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

    fn handle_failed_key(&mut self, event: &KeyEvent) -> RemoteConnectAction {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => RemoteConnectAction::Close,
            Key::Named(NamedKey::Enter) => {
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

    /// Display label for the peer currently being reached — its device name
    /// when a listed peer matches the dial address, else the raw host.
    fn dial_label(&self, host: &str) -> String {
        self.rows
            .iter()
            .find(|row| row.host == host)
            .map_or_else(|| host.to_owned(), |row| row.name.clone())
    }

    /// Best label for the peer in failure copy: the windows-stage label if we
    /// got that far, otherwise the highlighted/typed peer.
    fn peer_label(&self) -> String {
        match &self.stage {
            Stage::Windows { label, .. } => label.clone(),
            _ => {
                if let Some((host, _)) = parse_host_port(&self.manual) {
                    self.dial_label(&host)
                } else if let Some(row) = self.rows.get(self.selected) {
                    row.name.clone()
                } else {
                    String::from("the remote machine")
                }
            }
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

    /// Append the picker's GPU instances (backdrop + bordered box + rows) to
    /// `out`. No-op while inactive.
    pub fn build_instances(&self, ctx: RemoteConnectBuildContext<'_>) {
        if !self.active {
            return;
        }
        let view = self.view(clamp_cols(ctx.viewport.width, ctx.cell_size.0));
        render_picker_view(&view, ctx);
    }

    /// Flatten the current stage into a renderer-friendly view (title, an
    /// optional editable/status subtitle, the row list, and a footer hint).
    fn view(&self, content_cols: usize) -> PickerView {
        match &self.stage {
            Stage::Peers => {
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
            Stage::Windows { label, loading, awaiting_approval, rows, .. } => {
                // Held pending device approval on the owning peer (feature 014,
                // T019): a distinct cancelable overlay rather than the loading note,
                // shown until the window list arrives (approved) or the dial is
                // refused. Nothing is selectable while waiting; Esc steps back.
                if *loading && *awaiting_approval {
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
                let mut view_rows: Vec<PickerRow> = if *loading {
                    vec![PickerRow { text: String::from("Loading windows…"), dim: true }]
                } else {
                    rows.iter().map(window_row).collect()
                };
                if view_rows.is_empty() {
                    view_rows.push(PickerRow {
                        text: String::from("No windows on this machine"),
                        dim: true,
                    });
                }
                PickerView {
                    title: format!("Windows on {label}"),
                    subtitle: None,
                    rows: view_rows,
                    selectable: !*loading,
                    selected: self.selected,
                    footer: Some(String::from("Enter open  Up/Down pick  Esc back")),
                }
            }
            Stage::Failed { lines } => {
                let rows: Vec<PickerRow> = lines
                    .iter()
                    .flat_map(|line| wrap_text(line, content_cols))
                    .map(|text| PickerRow { text, dim: false })
                    .collect();
                PickerView {
                    title: String::from("Couldn't connect"),
                    subtitle: None,
                    rows,
                    selectable: false,
                    selected: 0,
                    footer: Some(String::from("Enter retry  Esc close")),
                }
            }
        }
    }
}

/// Build-time context handed to [`RemoteConnect::build_instances`], mirroring
/// the sibling overlay build contexts.
pub struct RemoteConnectBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

/// Render a flattened [`PickerView`] as the shared overlay chrome (backdrop +
/// centered bordered box + rows + footer). Extracted so both the connect picker
/// and the [`ReconnectOverlay`] (T030) draw an identical box. No-op on a
/// degenerate cell size or when the box does not fit.
fn render_picker_view(view: &PickerView, ctx: RemoteConnectBuildContext<'_>) {
    let RemoteConnectBuildContext { out, viewport, cell_size, chrome, resolve_glyph } = ctx;
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }

    let colors = PickerColors::from_chrome(chrome);
    let Some(layout) = PickerLayout::new(view, viewport, cell_size) else {
        return;
    };
    let mut renderer = PickerRenderer::new(out, cell_size, resolve_glyph);

    renderer.push_solid_rect(viewport, colors.backdrop);
    renderer.push_solid_rect(layout.box_rect, colors.bg);
    renderer.draw_border(layout.box_rect, colors.border);

    let text_x = layout.box_rect.x + cell_w;
    let mut row_y = layout.box_rect.y + cell_h;

    renderer.emit_line(
        &view.title,
        text_x,
        row_y,
        TextColors { fg: colors.header_fg, bg: colors.bg },
    );
    row_y += cell_h * 2.0;

    if let Some(subtitle) = &view.subtitle {
        renderer.emit_line(
            subtitle,
            text_x,
            row_y,
            TextColors { fg: colors.subtitle_fg, bg: colors.bg },
        );
        row_y += cell_h * 2.0;
    }

    for (index, row) in view.rows.iter().take(MAX_ROWS).enumerate() {
        let selected = view.selectable && index == view.selected;
        if selected {
            renderer.push_solid_rect(
                Rect {
                    x: layout.box_rect.x + 1.0,
                    y: row_y,
                    width: (layout.box_rect.width - 2.0).max(0.0),
                    height: cell_h,
                },
                colors.selection_bg,
            );
        }
        let fg = if selected {
            colors.selection_fg
        } else if row.dim {
            colors.dim_fg
        } else {
            colors.item_fg
        };
        let bg = if selected { colors.selection_bg } else { colors.bg };
        renderer.emit_line(&row.text, text_x, row_y, TextColors { fg, bg });
        row_y += cell_h;
    }

    if let Some(footer) = &view.footer {
        row_y += cell_h;
        renderer.emit_line(
            footer,
            text_x,
            row_y,
            TextColors { fg: colors.subtitle_fg, bg: colors.bg },
        );
    }
}

/// What a key/click on the [`ReconnectOverlay`] asks the app to do (feature 013,
/// T030). The overlay is transport-free: it returns an intent and the app owns
/// the side effect (cancel the loop, spawn a fresh remote client, close).
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
/// whose remote link dropped.
///
/// Distinct from the [`RemoteConnect`] picker (which runs only during the
/// initial connect): this appears on an already-attached window and is a
/// full-window modal while active. The IPC thread drives its
/// [`set_attempt`](Self::set_attempt) updates; the app settles it on cancel, an
/// authoritative sever, or an exhausted backoff, each offering a one-action
/// reconnect. Reuses [`render_picker_view`] so it shares the picker's box chrome.
pub struct ReconnectOverlay {
    /// Display label for the peer being reached (host / `MagicDNS` name).
    peer: String,
    stage: ReconnectStage,
}

enum ReconnectStage {
    /// Actively retrying with capped backoff; cancelable.
    Reconnecting { attempt: u32 },
    /// Settled (cancel / disabled / gave up); the copy explains why and the
    /// window waits for the one-action reconnect.
    Settled { lines: Vec<String> },
}

impl ReconnectOverlay {
    /// Enter the reconnecting state for `peer` at `attempt` (1-based).
    #[must_use]
    pub fn reconnecting(peer: String, attempt: u32) -> Self {
        Self { peer, stage: ReconnectStage::Reconnecting { attempt } }
    }

    /// Advance the visible attempt counter while still retrying. Ignored once
    /// settled, so a late in-flight attempt event cannot revive the spinner over
    /// a terminal message.
    pub fn set_attempt(&mut self, attempt: u32) {
        if let ReconnectStage::Reconnecting { attempt: current } = &mut self.stage {
            *current = attempt;
        }
    }

    /// Build the settled state after the user cancelled the auto-reconnect
    /// (offers a one-action reconnect).
    #[must_use]
    pub fn settled_cancelled(peer: String) -> Self {
        let lines = vec![format!("Disconnected from {peer}.")];
        Self { peer, stage: ReconnectStage::Settled { lines } }
    }

    /// Build the settled state after the capped backoff was exhausted — the
    /// combined connection-failure copy (offline / not running / disabled), since
    /// a disabled peer has no listener (contracts Disable semantics, FR-004).
    #[must_use]
    pub fn settled_unreachable(peer: String) -> Self {
        let lines = connection_failure_lines(&peer);
        Self { peer, stage: ReconnectStage::Settled { lines } }
    }

    /// Build the settled state after an authoritative refusal / delivered sever
    /// notice, using the typed reason's copy — `Disabled` states the fact rather
    /// than inferring it.
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
    pub fn key_action(&self, key: &Key) -> ReconnectAction {
        match self.stage {
            ReconnectStage::Reconnecting { .. } => match key {
                Key::Named(NamedKey::Escape) => ReconnectAction::Cancel,
                _ => ReconnectAction::None,
            },
            ReconnectStage::Settled { .. } => match key {
                Key::Named(NamedKey::Enter) => ReconnectAction::Reconnect,
                Key::Named(NamedKey::Escape) => ReconnectAction::Close,
                _ => ReconnectAction::None,
            },
        }
    }

    /// The action a mouse click performs: a settled overlay reconnects (mirrors
    /// the lost-control banner's click-to-reclaim); a retrying one ignores it so
    /// a stray click cannot cancel.
    #[must_use]
    pub fn click_action(&self) -> ReconnectAction {
        if self.is_settled() { ReconnectAction::Reconnect } else { ReconnectAction::None }
    }

    /// Append the overlay (dim backdrop + centered box) to `out`, reusing the
    /// picker's box renderer.
    pub fn build_instances(&self, ctx: RemoteConnectBuildContext<'_>) {
        let content_cols = clamp_cols(ctx.viewport.width, ctx.cell_size.0);
        let view = self.view(content_cols);
        render_picker_view(&view, ctx);
    }

    /// Flatten the current stage into a renderer-friendly [`PickerView`].
    fn view(&self, content_cols: usize) -> PickerView {
        match &self.stage {
            ReconnectStage::Reconnecting { attempt } => PickerView {
                title: format!("Reconnecting to {}…", self.peer),
                subtitle: None,
                rows: vec![PickerRow { text: format!("Attempt {attempt}"), dim: true }],
                selectable: false,
                selected: 0,
                footer: Some(String::from("Esc cancel")),
            },
            ReconnectStage::Settled { lines } => {
                let rows: Vec<PickerRow> = lines
                    .iter()
                    .flat_map(|line| wrap_text(line, content_cols))
                    .map(|text| PickerRow { text, dim: false })
                    .collect();
                PickerView {
                    title: String::from("Disconnected"),
                    subtitle: None,
                    rows,
                    selectable: false,
                    selected: 0,
                    footer: Some(String::from("Enter reconnect  Esc close")),
                }
            }
        }
    }
}

/// A flattened, renderer-ready row.
struct PickerRow {
    text: String,
    /// Rendered greyed (offline peer, placeholder, loading note).
    dim: bool,
}

/// A flattened, renderer-ready snapshot of the current stage.
struct PickerView {
    title: String,
    subtitle: Option<String>,
    rows: Vec<PickerRow>,
    /// Whether the row list responds to selection (false for status-only rows).
    selectable: bool,
    selected: usize,
    footer: Option<String>,
}

struct PickerLayout {
    box_rect: Rect,
}

impl PickerLayout {
    fn new(view: &PickerView, viewport: Rect, cell_size: (f32, f32)) -> Option<Self> {
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let cols = view_cols(view, viewport.width, cell_w);
        let rows = view_rows(view);
        let box_w = grid_width(cols, cell_w);
        let box_h = grid_height(rows, cell_h);
        let box_rect = Rect {
            x: viewport.x + ((viewport.width - box_w) / 2.0).max(0.0),
            y: viewport.y + ((viewport.height - box_h) / 4.0).max(0.0),
            width: box_w,
            height: box_h,
        };
        Some(Self { box_rect })
    }
}

/// Total grid rows the box needs: top pad + title + blank + optional subtitle +
/// rows + optional footer + bottom pad.
fn view_rows(view: &PickerView) -> usize {
    let mut rows = 2; // title + trailing blank
    if view.subtitle.is_some() {
        rows += 2;
    }
    rows += view.rows.len().min(MAX_ROWS);
    if view.footer.is_some() {
        rows += 2;
    }
    rows + 2 // top + bottom padding
}

fn view_cols(view: &PickerView, viewport_width: f32, cell_w: f32) -> usize {
    let longest = std::iter::once(view.title.chars().count())
        .chain(view.subtitle.iter().map(|s| s.chars().count() + 2))
        .chain(view.rows.iter().take(MAX_ROWS).map(|r| r.text.chars().count() + 2))
        .chain(view.footer.iter().map(|s| s.chars().count()))
        .max()
        .unwrap_or(MIN_COLS)
        + 2; // side padding
    longest.clamp(MIN_COLS, clamp_cols(viewport_width, cell_w))
}

/// Usable content columns inside the box, used for failure-copy word wrapping.
fn clamp_cols(viewport_width: f32, cell_w: f32) -> usize {
    let max = grid_units_in_extent(viewport_width, cell_w);
    MAX_COLS.min(max.max(MIN_COLS))
}

struct PickerRenderer<'a> {
    out: &'a mut Vec<CellInstance>,
    cell_w: f32,
    resolve_glyph: &'a mut GlyphResolver<'a>,
}

impl<'a> PickerRenderer<'a> {
    fn new(
        out: &'a mut Vec<CellInstance>,
        cell_size: (f32, f32),
        resolve_glyph: &'a mut GlyphResolver<'a>,
    ) -> Self {
        Self { out, cell_w: cell_size.0, resolve_glyph }
    }

    fn push_solid_rect(&mut self, rect: Rect, color: [f32; 4]) {
        self.out.push(scribe_renderer::chrome::solid_quad(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            color,
        ));
    }

    fn draw_border(&mut self, rect: Rect, color: [f32; 4]) {
        self.push_solid_rect(Rect { x: rect.x, y: rect.y, width: rect.width, height: 1.0 }, color);
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y + rect.height - 1.0, width: rect.width, height: 1.0 },
            color,
        );
        self.push_solid_rect(Rect { x: rect.x, y: rect.y, width: 1.0, height: rect.height }, color);
        self.push_solid_rect(
            Rect { x: rect.x + rect.width - 1.0, y: rect.y, width: 1.0, height: rect.height },
            color,
        );
    }

    fn emit_line(&mut self, text: &str, start_x: f32, y: f32, colors: TextColors) {
        for (idx, ch) in text.chars().enumerate() {
            let (uv_min, uv_max) = (self.resolve_glyph)(ch);
            self.out.push(CellInstance {
                pos: [start_x + grid_width(idx, self.cell_w), y],
                size: [0.0, 0.0],
                uv_min,
                uv_max,
                fg_color: colors.fg,
                bg_color: colors.bg,
                corner_radius: 0.0,
            });
        }
    }
}

/// Foreground/background pair for one rendered text line.
#[derive(Clone, Copy)]
struct TextColors {
    fg: [f32; 4],
    bg: [f32; 4],
}

struct PickerColors {
    backdrop: [f32; 4],
    bg: [f32; 4],
    border: [f32; 4],
    header_fg: [f32; 4],
    subtitle_fg: [f32; 4],
    item_fg: [f32; 4],
    dim_fg: [f32; 4],
    selection_bg: [f32; 4],
    selection_fg: [f32; 4],
}

impl PickerColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        let mut bg = srgb_to_linear_rgba(chrome.tab_bar_active_bg);
        bg[3] = 0.96;
        let border = srgb_to_linear_rgba(chrome.accent);
        let header_fg = srgb_to_linear_rgba(chrome.tab_text_active);
        let item_fg = srgb_to_linear_rgba(chrome.tab_text_active);
        let mut subtitle_fg = srgb_to_linear_rgba(chrome.status_bar_text);
        subtitle_fg[3] *= 0.85;
        let mut dim_fg = item_fg;
        dim_fg[3] *= 0.55;
        let mut active_row_bg = srgb_to_linear_rgba(chrome.status_bar_bg);
        active_row_bg[3] = 1.0;
        let active_text = srgb_to_linear_rgba(chrome.tab_text_active);

        Self {
            backdrop: [0.0, 0.0, 0.0, 0.20],
            bg,
            border,
            header_fg,
            subtitle_fg,
            item_fg,
            dim_fg,
            selection_bg: active_row_bg,
            selection_fg: active_text,
        }
    }
}

/// Build a merged peer-step row: an online marker, the device name, its transport
/// label, and — when the same machine is also reachable on the other transport
/// but shown once (LAN preferred) — an "also on …" hint (feature 014, T024,
/// FR-008/009).
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
/// name so blank hostnames never match. A UX heuristic, never a trust key
/// (analysis C3).
fn name_match_key(name: &str) -> Option<String> {
    let label = name.trim().split('.').next().unwrap_or("").trim();
    if label.is_empty() { None } else { Some(label.to_ascii_lowercase()) }
}

/// Claim the first not-yet-claimed tailnet peer whose machine name matches
/// `lan_host` (feature 014, T024 dedup), marking it consumed and returning the
/// "also reachable on Tailscale" hint for the LAN row. `None` when no confident
/// name match exists, so the tailnet peer keeps its own separate labeled row.
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

/// Build a window-step row: the "New window" entry, or an existing window with
/// its session count and in-use marker.
fn window_row(row: &WindowRow) -> PickerRow {
    let text = if row.window_id.is_none() {
        String::from("+ New window")
    } else {
        // Feature 015 (T025): show live share occupancy ("shared · N attached")
        // when more than one machine is attached; otherwise keep feature 013's
        // binary in-use marker for a single-controller window.
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

/// Feature 015 (T025): the occupancy prefix for a shared window's picker row,
/// derived from its sharing mode — "free-for-all" reads distinctly from the
/// single-typist "shared", and a pre-sharing server (`None`) falls back to
/// "shared".
fn share_kind_label(mode: Option<SharingMode>) -> &'static str {
    match mode {
        Some(SharingMode::FreeForAll) => "free-for-all",
        Some(SharingMode::SharedSingleTypist | SharingMode::SingleController) | None => "shared",
    }
}

/// Map a typed [`RemoteRefusal`] to its distinct UX-002 failure copy
/// (contracts/settings-and-config.md). Each line names the remedy.
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

/// Map a typed [`LanRefusal`] to its distinct UX-002 failure copy
/// (contracts/settings-and-config.md, feature 014) — the LAN analogue of
/// [`refusal_lines`]. `IncompatibleVersion` names this machine's version (the
/// refusal carries no peer version), mirroring the tailnet wording.
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

/// The LAN connection-failure copy (contracts/settings-and-config.md): a dormant
/// or absent peer leaves nothing listening, so offline / asleep / off-network are
/// deliberately indistinguishable on a cold LAN dial (FR-004).
fn lan_connection_failure_lines(peer: &str) -> Vec<String> {
    vec![format!(
        "Can't reach {peer} on the local network — it may be offline, asleep, or not on this network."
    )]
}

/// Parse a manual `host` or `host:port` entry, defaulting the port to the
/// configured remote port. Mirrors the `SCRIBE_REMOTE_DIAL` env parser so a
/// bare IPv6 literal falls through to the default port and is dialed verbatim.
fn parse_host_port(input: &str) -> Option<(String, u16)> {
    let target = input.trim();
    if target.is_empty() {
        return None;
    }
    let default_port = RemoteConfig::default().port;
    let (host, port) = match target.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') => {
            (host, port.parse::<u16>().unwrap_or(default_port))
        }
        _ => (target, default_port),
    };
    Some((host.to_owned(), port))
}

/// Greedy word-wrap for failure copy so long sentences fit the box width.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn grid_units(units: usize) -> u16 {
    u16::try_from(units.min(MAX_GRID_UNITS)).unwrap_or(u16::MAX)
}

fn grid_width(cols: usize, cell_w: f32) -> f32 {
    f32::from(grid_units(cols)) * cell_w
}

fn grid_height(rows: usize, cell_h: f32) -> f32 {
    f32::from(grid_units(rows)) * cell_h
}

/// Largest column count whose pixel width still fits `extent`. Uses a bounded
/// binary search over [`grid_width`] rather than float→int casts, matching the
/// sibling overlays' precision-lint-clean approach (`close_dialog.rs`).
fn grid_units_in_extent(extent: f32, unit: f32) -> usize {
    if unit <= 0.0 || !extent.is_finite() || extent <= 0.0 {
        return 0;
    }

    let mut low = 0usize;
    let mut high = 1usize;
    while high < MAX_GRID_UNITS && grid_width(high, unit) <= extent {
        low = high;
        high = high.saturating_mul(2).min(MAX_GRID_UNITS);
        if high == low {
            break;
        }
    }

    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if grid_width(mid, unit) <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low
}
