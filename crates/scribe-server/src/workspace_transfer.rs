//! Workspace-transfer gate and idempotency ledger (spec 029, US2/C4).
//!
//! The gate is one async mutex serializing the transfer transaction against
//! the two other readers that must never observe a half-committed move: the
//! handoff/state-dump snapshotter (`handoff::serialize_state`) and the agent
//! world capture. Registry `RwLock`s alone cannot give that atomicity — the
//! commit mutates the live-session and workspace registries under separate
//! guards, so a captor without the gate could read between them.
//!
//! The state behind the gate is the bounded transfer ledger: the most recent
//! [`TRANSFER_LEDGER_CAP`] `transfer_id → result` records, serialized into
//! handoff state so a retry after a lost ACK — even across an upgrade — gets
//! the recorded result instead of a spurious `NotWorkspaceOwner` refusal.
//! Entries expire only by capacity, never by time.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use scribe_common::protocol::WorkspaceTransferResult;

/// Most-recent transfer results retained for idempotent retries.
pub const TRANSFER_LEDGER_CAP: usize = 64;

/// One recorded transfer outcome, carried in handoff state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferLedgerEntry {
    pub transfer_id: u64,
    pub result: WorkspaceTransferResult,
}

/// State guarded by the transfer gate: the bounded result ledger plus the
/// handoff latch that turns a post-snapshot transfer into a typed
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
            state.record(entry.transfer_id, entry.result);
        }
        state
    }

    /// The recorded result for a retried `transfer_id`, if any.
    #[must_use]
    pub fn recorded(&self, transfer_id: u64) -> Option<WorkspaceTransferResult> {
        self.entries.iter().find(|entry| entry.transfer_id == transfer_id).map(|entry| entry.result)
    }

    /// Record one result, evicting the oldest entry past the cap.
    pub fn record(&mut self, transfer_id: u64, result: WorkspaceTransferResult) {
        if self.entries.len() == TRANSFER_LEDGER_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(TransferLedgerEntry { transfer_id, result });
    }

    /// Latch the handoff refusal: set under the gate before the snapshot is
    /// taken, so no transfer can commit state the payload no longer carries.
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

/// The transfer gate. Lock order: this gate strictly BEFORE any registry
/// guard (live sessions → window shares → workspace manager).
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
    use scribe_common::protocol::{WorkspaceTransferRefusal, WorkspaceTransferResult};

    use super::*;

    // @lat: [[server#Workspace Transfer#Transfer gate and ledger]]
    #[test]
    fn ledger_replays_recorded_results_and_evicts_only_by_capacity() {
        let mut state = TransferState::default();
        state.record(1, WorkspaceTransferResult::Transferred);
        state.record(
            2,
            WorkspaceTransferResult::Refused { reason: WorkspaceTransferRefusal::SoleWorkspace },
        );
        assert_eq!(state.recorded(1), Some(WorkspaceTransferResult::Transferred));
        assert_eq!(
            state.recorded(2),
            Some(WorkspaceTransferResult::Refused {
                reason: WorkspaceTransferRefusal::SoleWorkspace
            })
        );
        assert_eq!(state.recorded(3), None);

        for id in 3..(3 + TRANSFER_LEDGER_CAP as u64) {
            state.record(id, WorkspaceTransferResult::Transferred);
        }
        assert_eq!(state.entries().len(), TRANSFER_LEDGER_CAP);
        assert_eq!(state.recorded(1), None, "oldest entries expire by capacity");
        assert_eq!(state.recorded(2), None);
        assert_eq!(state.recorded(3), Some(WorkspaceTransferResult::Transferred));
    }

    // @lat: [[server#Workspace Transfer#Transfer gate and ledger]]
    #[test]
    fn ledger_restore_round_trips_through_serialized_entries() {
        let mut state = TransferState::default();
        state.record(7, WorkspaceTransferResult::Transferred);
        state.record(
            8,
            WorkspaceTransferResult::Refused {
                reason: WorkspaceTransferRefusal::EnvironmentRebindFailed,
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
