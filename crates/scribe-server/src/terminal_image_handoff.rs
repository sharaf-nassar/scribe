//! Carry committed image state and paused framing across a server upgrade.
//!
//! A hot reload pauses PTY reads wherever the last one ended and hands the
//! successor everything it needs to resume without the application noticing.
//! For images that is three distinct things, and dropping any one of them is a
//! visible regression:
//!
//! * the committed scene — definitions and placements under one generation,
//!   shipped as the same bounded `Begin`/chunk/`Commit` burst a late attacher
//!   receives, so the successor stages a whole scene or none of it;
//! * the paused framer's held prefix — a read can end mid-APC or mid-DCS, and
//!   a successor that starts in Ground replays that prefix as visible text;
//! * an in-flight chunked Kitty transfer — accumulation spans many complete
//!   commands, so its normalized bytes have no raw prefix left to replay.
//!
//! The whole payload rides inside `HandoffState`, which is capped at 256 MiB
//! for every session's text replay put together. One session may legitimately
//! retain 128 MiB of canonical pixels, so image bytes get their own ceiling
//! across the payload: a scene that does not fit is exported as an *empty*
//! scene rather than a truncated one. The successor then shows that session
//! with no images instead of half a scene, and its text is untouched.

use scribe_common::kitty_decode::KittyTransferState;
use scribe_common::terminal_images::{
    TerminalImageGeneration, TerminalImageReplayMessage, TerminalOutputSequence, TerminalScreenKind,
};
use scribe_pty::graphics_framing::{
    KittyCommandControls, KittyControlPresence, PartialFramingState, RawByteRange,
};
use serde::{Deserialize, Serialize};

use crate::terminal_image_publication::DefinitionPayload;
use crate::terminal_image_state::SessionTerminal;

/// Total canonical image bytes one handoff payload may carry.
///
/// Half of the 256 MiB `HandoffState` ceiling, which leaves the other half for
/// every session's compressed text replay. One maximum v1 scene is exactly
/// this size, so the first max-scene session fits and later ones downgrade to
/// an empty scene instead of pushing the payload past the state cap.
pub const MAX_HANDOFF_IMAGE_BYTES: u64 = 134_217_728;

/// One session's image state as it crosses the upgrade socket.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionImageHandoff {
    pub generation: TerminalImageGeneration,
    pub sequence: TerminalOutputSequence,
    pub active_screen: TerminalScreenKind,
    /// Screen the last published burst left viewers on, which can lag the
    /// canonical screen when a read committed but has not published yet.
    pub published_screen: TerminalScreenKind,
    pub next_assigned_image_id: u64,
    /// Bounded `Begin`/definition/chunk/placement/`Commit` burst.
    pub records: Vec<TerminalImageReplayMessage>,
    pub framing: PartialFramingState,
    pub pending_kitty: Option<PendingKittyHandoff>,
}

/// A chunked Kitty transfer that was mid-accumulation when reads paused.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingKittyHandoff {
    pub controls: KittyCommandControls,
    pub presence: KittyControlPresence,
    pub range: RawByteRange,
    pub transfer: KittyTransferState,
}

/// Payload-free facts about one whole handoff's image state.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct HandoffImageCounters {
    /// Sessions that exported image state at all.
    pub sessions: u32,
    /// Definitions carried with complete canonical pixels.
    pub definitions: u32,
    pub placements: u32,
    pub chunks: u32,
    pub total_rgba_bytes: u64,
    /// Largest single chunk record, which must stay inside the replay ceiling.
    pub max_chunk_bytes: u64,
    /// Sessions whose scene did not fit the payload ceiling and were exported
    /// empty. A dropped scene is never a partial scene.
    pub dropped_scenes: u32,
    /// Sessions that paused mid-APC or mid-DCS.
    pub partial_framers: u32,
    /// Sessions that paused mid-chunk-accumulation.
    pub pending_transfers: u32,
}

/// Ordered, bounded image export for one handoff payload.
///
/// The caller walks its sessions once, in payload order, and this accumulates
/// the shared byte ceiling and the payload-free counters that end up in
/// upgrade evidence.
pub struct HandoffImageExport {
    enabled: bool,
    remaining_bytes: u64,
    counters: HandoffImageCounters,
}

impl HandoffImageExport {
    /// Start an export. `enabled` is the master image switch: with images off,
    /// nothing is exported and the payload stays readable by a server that
    /// predates image state, which is what makes rollback a config change
    /// rather than a cold restart.
    // @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self::with_ceiling_for_validation(enabled, MAX_HANDOFF_IMAGE_BYTES)
    }

    /// Start an export against a smaller ceiling, so the drop path can be
    /// driven from a real committed scene instead of 128 MiB of pixels.
    #[must_use]
    pub const fn with_ceiling_for_validation(enabled: bool, ceiling_bytes: u64) -> Self {
        Self {
            enabled,
            remaining_bytes: ceiling_bytes,
            counters: HandoffImageCounters {
                sessions: 0,
                definitions: 0,
                placements: 0,
                chunks: 0,
                total_rgba_bytes: 0,
                max_chunk_bytes: 0,
                dropped_scenes: 0,
                partial_framers: 0,
                pending_transfers: 0,
            },
        }
    }

    /// Payload-free facts accumulated so far.
    #[must_use]
    pub const fn counters(&self) -> HandoffImageCounters {
        self.counters
    }

    /// Export one session, charging its canonical pixels to the shared ceiling.
    ///
    /// Returns `None` only when images are disabled, so an enabled session
    /// always carries its cursor, generation, and framing even if its scene
    /// was too large to fit.
    // @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
    pub fn session(
        &mut self,
        terminal: &SessionTerminal,
        payload: DefinitionPayload<'_>,
    ) -> Option<SessionImageHandoff> {
        if !self.enabled {
            return None;
        }
        let mut exported = terminal.export_handoff(payload);
        let scene_bytes = exported.scene_bytes;
        if scene_bytes > self.remaining_bytes {
            self.counters.dropped_scenes = self.counters.dropped_scenes.saturating_add(1);
            exported = terminal.export_handoff(&mut |_| None);
        } else {
            self.remaining_bytes = self.remaining_bytes.saturating_sub(scene_bytes);
            self.counters.definitions =
                self.counters.definitions.saturating_add(exported.definitions);
            self.counters.placements = self.counters.placements.saturating_add(exported.placements);
            self.counters.chunks = self.counters.chunks.saturating_add(exported.chunks);
            self.counters.total_rgba_bytes =
                self.counters.total_rgba_bytes.saturating_add(scene_bytes);
            self.counters.max_chunk_bytes =
                self.counters.max_chunk_bytes.max(exported.max_chunk_bytes);
        }
        self.counters.sessions = self.counters.sessions.saturating_add(1);
        if exported.state.framing.is_partial() {
            self.counters.partial_framers = self.counters.partial_framers.saturating_add(1);
        }
        if exported.state.pending_kitty.is_some() {
            self.counters.pending_transfers = self.counters.pending_transfers.saturating_add(1);
        }
        Some(exported.state)
    }
}

/// One session's export plus the payload-free cost of its scene.
pub struct ExportedSessionImages {
    pub state: SessionImageHandoff,
    pub definitions: u32,
    pub placements: u32,
    pub chunks: u32,
    pub scene_bytes: u64,
    pub max_chunk_bytes: u64,
}
