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
//!
//! A move entry also records the request it answered, and only a retry (the
//! same id *and* the same request) replays it. The ledger is shared by every
//! client and outlives them all, and a move id is only as unique as the client
//! that minted it: clients that numbered moves from 1 per launch had every
//! post-restart move answered with an earlier client's `Moved`, and nothing
//! moved. A reused id with a different request is new work, so it runs.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use scribe_common::ids::{WindowId, WorkspaceId};
use scribe_common::protocol::{
    WorkspaceMoveOperation, WorkspaceMoveResult, WorkspaceTransferResult,
};

/// Everything a `MoveWorkspace` asks for, with the window it came from: the
/// identity a retry repeats exactly and an unrelated move never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMoveRequest {
    pub source_window: WindowId,
    pub workspace_id: WorkspaceId,
    pub target_window: WindowId,
    pub target_workspace_id: WorkspaceId,
    pub operation: WorkspaceMoveOperation,
}

/// Most-recent workspace transaction results retained for idempotent retries.
pub const TRANSFER_LEDGER_CAP: usize = 64;

/// One recorded workspace outcome, carried in handoff state.
///
/// `untagged` preserves the phase-1 transfer wire map (`transfer_id`, `result`)
/// while adding the disjoint move map (`move_id`, `result`, `source_closed`,
/// `request`). `request` is absent from entries an older server recorded; those
/// still replay by id alone, the contract they were recorded under, until
/// capacity evicts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransferLedgerEntry {
    Transfer {
        transfer_id: u64,
        result: WorkspaceTransferResult,
    },
    Move {
        move_id: u64,
        result: WorkspaceMoveResult,
        source_closed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<WorkspaceMoveRequest>,
    },
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

    /// The recorded result if `request` under `move_id` is a retry, or `None`
    /// when it is new work.
    ///
    /// The newest matching entry wins, so a reused id that ran fresh replays
    /// its own result on its own retry. An entry without a request (recorded
    /// by an older server) matches on the id alone.
    #[must_use]
    pub fn recorded_move(
        &self,
        move_id: u64,
        request: WorkspaceMoveRequest,
    ) -> Option<RecordedWorkspaceMove> {
        self.entries.iter().rev().find_map(|entry| match entry {
            TransferLedgerEntry::Move { move_id: id, result, source_closed, request: recorded }
                if *id == move_id && recorded.is_none_or(|recorded| recorded == request) =>
            {
                Some(RecordedWorkspaceMove { result: *result, source_closed: *source_closed })
            }
            TransferLedgerEntry::Transfer { .. } | TransferLedgerEntry::Move { .. } => None,
        })
    }

    /// Whether `move_id` was already recorded for some other request: the
    /// client reused an id, and the move about to run is not a retry.
    #[must_use]
    pub fn move_id_reused(&self, move_id: u64, request: WorkspaceMoveRequest) -> bool {
        self.entries.iter().any(|entry| {
            matches!(entry, TransferLedgerEntry::Move { move_id: id, request: Some(recorded), .. }
                if *id == move_id && *recorded != request)
        })
    }

    /// Record one workspace-move result with the request it answered.
    pub fn record_move(
        &mut self,
        move_id: u64,
        request: WorkspaceMoveRequest,
        result: RecordedWorkspaceMove,
    ) {
        self.push(TransferLedgerEntry::Move {
            move_id,
            result: result.result,
            source_closed: result.source_closed,
            request: Some(request),
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

    fn request(operation: WorkspaceMoveOperation) -> WorkspaceMoveRequest {
        WorkspaceMoveRequest {
            source_window: WindowId::new(),
            workspace_id: WorkspaceId::new(),
            target_window: WindowId::new(),
            target_workspace_id: WorkspaceId::new(),
            operation,
        }
    }

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
        let swap = request(WorkspaceMoveOperation::Swap);
        state.record_move(1, swap, moved);
        assert_eq!(state.recorded(1), Some(WorkspaceTransferResult::Transferred));
        assert_eq!(
            state.recorded(2),
            Some(WorkspaceTransferResult::Refused {
                reason: WorkspaceTransferRefusal::SoleWorkspace
            })
        );
        assert_eq!(state.recorded_move(1, swap), Some(moved));
        assert_eq!(state.recorded_move(2, swap), None);

        for id in 3..(3 + TRANSFER_LEDGER_CAP as u64) {
            state.record_move(
                id,
                swap,
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
        assert_eq!(state.recorded_move(1, swap), None);
        assert!(state.recorded_move(3, swap).is_some());
    }

    // @lat: [[server#Workspace Transfer#Transfer gate and ledger]]
    #[test]
    fn a_reused_move_id_replays_only_for_the_request_it_recorded() {
        let mut state = TransferState::default();
        let first = request(WorkspaceMoveOperation::Swap);
        let other = WorkspaceMoveRequest {
            operation: WorkspaceMoveOperation::MoveTab {
                tab_session_id: scribe_common::ids::SessionId::new(),
                target_index: 0,
            },
            ..request(WorkspaceMoveOperation::Swap)
        };
        let moved =
            RecordedWorkspaceMove { result: WorkspaceMoveResult::Moved, source_closed: false };
        state.record_move(1, first, moved);
        assert_eq!(state.recorded_move(1, first), Some(moved), "a retry replays");
        assert_eq!(state.recorded_move(1, other), None, "another request reusing the id runs");
        assert!(state.move_id_reused(1, other));
        assert!(!state.move_id_reused(1, first));

        // The reused id's own fresh result is what its own retry replays.
        let refused = RecordedWorkspaceMove {
            result: WorkspaceMoveResult::Refused {
                reason: WorkspaceMoveRefusal::TargetWorkspaceUnavailable,
            },
            source_closed: false,
        };
        state.record_move(1, other, refused);
        assert_eq!(state.recorded_move(1, other), Some(refused));
        assert_eq!(state.recorded_move(1, first), Some(moved));
    }

    #[test]
    fn a_move_entry_from_an_older_server_still_replays_by_id() {
        #[derive(Serialize)]
        struct OlderMoveEntry {
            move_id: u64,
            result: WorkspaceMoveResult,
            source_closed: bool,
        }

        let bytes = rmp_serde::to_vec_named(&vec![OlderMoveEntry {
            move_id: 4,
            result: WorkspaceMoveResult::Moved,
            source_closed: true,
        }])
        .expect("serialize older entry");
        let entries: Vec<TransferLedgerEntry> =
            rmp_serde::from_slice(&bytes).expect("decode older entry");
        let restored = TransferState::restore(entries);
        assert_eq!(
            restored.recorded_move(4, request(WorkspaceMoveOperation::Swap)),
            Some(RecordedWorkspaceMove { result: WorkspaceMoveResult::Moved, source_closed: true })
        );
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
        let swap = request(WorkspaceMoveOperation::Swap);
        state.record_move(
            9,
            swap,
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
            restored.recorded_move(9, swap),
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
