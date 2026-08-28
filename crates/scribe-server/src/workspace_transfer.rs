//! Workspace-transfer gate and idempotency ledger (spec 029, US2/C4).
//!
//! The gate is one async mutex serializing workspace transfer and move
//! transactions against the two readers that must never observe a half commit:
//! the handoff/state-dump snapshotter (`handoff::serialize_state`) and agent
//! world capture. Registry `RwLock`s alone cannot give that atomicity because a
//! commit mutates the live-session, share, and workspace registries separately.
//!
//! The state behind the gate is the bounded transaction ledger: the most recent
//! [`TRANSFER_LEDGER_CAP`] transfer/move results, serialized into handoff state
//! so a retry after a lost ACK — even across an upgrade — replays the recorded
//! result. Entries expire only by capacity, never by time.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use scribe_common::protocol::{WorkspaceMoveResult, WorkspaceTransferResult};

/// Most-recent workspace transaction results retained for idempotent retries.
pub const TRANSFER_LEDGER_CAP: usize = 64;

/// One recorded workspace outcome, carried in handoff state.
///
/// `untagged` preserves the phase-1 transfer wire map (`transfer_id`, `result`)
/// while adding the disjoint move map (`move_id`, `result`, `source_closed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransferLedgerEntry {
    Transfer { transfer_id: u64, result: WorkspaceTransferResult },
    Move { move_id: u64, result: WorkspaceMoveResult, source_closed: bool },
}

/// The replayable outcome of one workspace move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedWorkspaceMove {
    pub result: WorkspaceMoveResult,
    pub source_closed: bool,
}

/// State guarded by the transfer gate: the bounded result ledger plus the
/// handoff latch that turns a post-snapshot transaction into a typed
/// `HandoffInProgress` refusal instead of a silently lost commit.
#[derive(Default)]
pub struct TransferState {
    entries: VecDeque<TransferLedgerEntry>,
    handoff_in_progress: bool,
}

impl TransferState {
    /// Rebuild the ledger from a handoff payload (order preserved, capped).
    #[must_use]
    pub fn restore(entries: Vec<TransferLedgerEntry>) -> Self {
        let mut state = Self::default();
        for entry in entries {
            state.push(entry);
        }
        state
    }

    /// The recorded result for a retried phase-1 `transfer_id`, if any.
    #[must_use]
    pub fn recorded(&self, transfer_id: u64) -> Option<WorkspaceTransferResult> {
        self.entries.iter().find_map(|entry| match entry {
            TransferLedgerEntry::Transfer { transfer_id: id, result } if *id == transfer_id => {
                Some(*result)
            }
            TransferLedgerEntry::Transfer { .. } | TransferLedgerEntry::Move { .. } => None,
        })
    }

    /// Record one phase-1 transfer result.
    pub fn record(&mut self, transfer_id: u64, result: WorkspaceTransferResult) {
        self.push(TransferLedgerEntry::Transfer { transfer_id, result });
    }

    /// The recorded result for a retried `move_id`, if any.
    #[must_use]
    pub fn recorded_move(&self, move_id: u64) -> Option<RecordedWorkspaceMove> {
        self.entries.iter().find_map(|entry| match entry {
            TransferLedgerEntry::Move { move_id: id, result, source_closed } if *id == move_id => {
                Some(RecordedWorkspaceMove { result: *result, source_closed: *source_closed })
            }
            TransferLedgerEntry::Transfer { .. } | TransferLedgerEntry::Move { .. } => None,
        })
    }

    /// Record one workspace-move result.
    pub fn record_move(&mut self, move_id: u64, result: RecordedWorkspaceMove) {
        self.push(TransferLedgerEntry::Move {
            move_id,
            result: result.result,
            source_closed: result.source_closed,
        });
    }

    fn push(&mut self, entry: TransferLedgerEntry) {
        if self.entries.len() == TRANSFER_LEDGER_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Latch the handoff refusal: set under the gate before the snapshot is
    /// taken, so no transaction can commit state the payload no longer carries.
    pub fn begin_handoff(&mut self) {
        self.handoff_in_progress = true;
    }

    /// Clear the latch after a failed handoff — the old server keeps serving.
    pub fn abort_handoff(&mut self) {
        self.handoff_in_progress = false;
    }

    /// Whether a handoff snapshot has been taken and not aborted.
    #[must_use]
    pub fn handoff_in_progress(&self) -> bool {
        self.handoff_in_progress
    }

    /// Ledger entries in recording order, for handoff serialization.
    #[must_use]
    pub fn entries(&self) -> Vec<TransferLedgerEntry> {
        self.entries.iter().copied().collect()
    }
}

/// The workspace transaction gate. Lock order: this gate strictly BEFORE any
/// registry guard (live sessions → window shares → workspace manager).
pub type TransferGate = Arc<tokio::sync::Mutex<TransferState>>;

/// Fresh gate with an empty ledger (normal startup).
#[must_use]
pub fn new_transfer_gate() -> TransferGate {
    Arc::new(tokio::sync::Mutex::new(TransferState::default()))
}

/// Gate seeded from a handoff payload's ledger (upgrade startup).
#[must_use]
pub fn restored_transfer_gate(entries: Vec<TransferLedgerEntry>) -> TransferGate {
    Arc::new(tokio::sync::Mutex::new(TransferState::restore(entries)))
}

#[cfg(test)]
mod tests {
    use scribe_common::protocol::{
        WorkspaceMoveRefusal, WorkspaceTransferRefusal, WorkspaceTransferResult,
    };

    use super::*;

    // @lat: [[server#Workspace Transfer#Transfer gate and ledger]]
    #[test]
    fn ledger_replays_both_result_kinds_and_evicts_only_by_shared_capacity() {
        let mut state = TransferState::default();
        state.record(1, WorkspaceTransferResult::Transferred);
        state.record(
            2,
            WorkspaceTransferResult::Refused { reason: WorkspaceTransferRefusal::SoleWorkspace },
        );
        let moved =
            RecordedWorkspaceMove { result: WorkspaceMoveResult::Moved, source_closed: true };
        state.record_move(1, moved);
        assert_eq!(state.recorded(1), Some(WorkspaceTransferResult::Transferred));
        assert_eq!(
            state.recorded(2),
            Some(WorkspaceTransferResult::Refused {
                reason: WorkspaceTransferRefusal::SoleWorkspace
            })
        );
        assert_eq!(state.recorded_move(1), Some(moved));
        assert_eq!(state.recorded_move(2), None);

        for id in 3..(3 + TRANSFER_LEDGER_CAP as u64) {
            state.record_move(
                id,
                RecordedWorkspaceMove {
                    result: WorkspaceMoveResult::Refused {
                        reason: WorkspaceMoveRefusal::TargetWindowUnavailable,
                    },
                    source_closed: false,
                },
            );
        }
        assert_eq!(state.entries().len(), TRANSFER_LEDGER_CAP);
        assert_eq!(state.recorded(1), None, "oldest entries expire by shared capacity");
        assert_eq!(state.recorded(2), None);
        assert_eq!(state.recorded_move(1), None);
        assert!(state.recorded_move(3).is_some());
    }

    #[test]
    fn phase_one_transfer_entry_wire_still_decodes() {
        #[derive(Serialize)]
        struct PhaseOneEntry {
            transfer_id: u64,
            result: WorkspaceTransferResult,
        }

        let bytes = rmp_serde::to_vec_named(&PhaseOneEntry {
            transfer_id: 7,
            result: WorkspaceTransferResult::Transferred,
        })
        .expect("serialize phase-one entry");
        assert_eq!(
            rmp_serde::from_slice::<TransferLedgerEntry>(&bytes).expect("decode current entry"),
            TransferLedgerEntry::Transfer {
                transfer_id: 7,
                result: WorkspaceTransferResult::Transferred,
            }
        );
    }

    // @lat: [[server#Workspace Transfer#Transfer gate and ledger]]
    #[test]
    fn ledger_restore_round_trips_transfer_and_move_entries() {
        let mut state = TransferState::default();
        state.record(7, WorkspaceTransferResult::Transferred);
        state.record(
            8,
            WorkspaceTransferResult::Refused {
                reason: WorkspaceTransferRefusal::EnvironmentRebindFailed,
            },
        );
        state.record_move(
            9,
            RecordedWorkspaceMove {
                result: WorkspaceMoveResult::Refused {
                    reason: WorkspaceMoveRefusal::EnvironmentRebindFailed,
                },
                source_closed: false,
            },
        );

        let bytes = rmp_serde::to_vec_named(&state.entries()).expect("serialize ledger");
        let entries: Vec<TransferLedgerEntry> =
            rmp_serde::from_slice(&bytes).expect("deserialize ledger");
        let restored = TransferState::restore(entries);
        assert_eq!(restored.recorded(7), Some(WorkspaceTransferResult::Transferred));
        assert_eq!(
            restored.recorded(8),
            Some(WorkspaceTransferResult::Refused {
                reason: WorkspaceTransferRefusal::EnvironmentRebindFailed
            })
        );
        assert_eq!(
            restored.recorded_move(9),
            Some(RecordedWorkspaceMove {
                result: WorkspaceMoveResult::Refused {
                    reason: WorkspaceMoveRefusal::EnvironmentRebindFailed,
                },
                source_closed: false,
            })
        );
        assert!(!restored.handoff_in_progress(), "the latch never crosses a handoff");
    }

    #[test]
    fn handoff_latch_sets_and_clears() {
        let mut state = TransferState::default();
        assert!(!state.handoff_in_progress());
        state.begin_handoff();
        assert!(state.handoff_in_progress());
        state.abort_handoff();
        assert!(!state.handoff_in_progress());
    }
}
