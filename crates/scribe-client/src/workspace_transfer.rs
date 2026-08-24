//! Correlated client-side workspace transfer state.
//!
//! The server owns the atomic move. This module keeps the client from mutating
//! its source tree early, retries an unknown outcome with the same transfer id,
//! and turns one matching success into one target-window bootstrap.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use scribe_common::{
    ids::{WindowId, WorkspaceId},
    protocol::{WorkspaceTransferRefusal, WorkspaceTransferResult},
};

/// How long a sent request may wait before the client safely retries its id.
pub const WORKSPACE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

/// Size and optional cursor-anchored origin for the claimed target window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransferWindowSpec {
    pub width: f32,
    pub height: f32,
    /// `None` means compositor placement (Wayland).
    pub origin: Option<(f32, f32)>,
}

/// Ids-only request sent to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceTransferRequest {
    pub correlation: u64,
    pub workspace: WorkspaceId,
    pub target_window: WindowId,
}

/// Target-window work released only by one matching success.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransferredWindow {
    pub window_id: WindowId,
    pub spec: TransferWindowSpec,
}

/// User-facing recovery or refusal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTransferFeedback {
    CapabilityAbsent,
    SoleWorkspace,
    AlreadyPending,
    SendFailed,
    Disconnected,
    TimedOutRetrying,
    Refused(WorkspaceTransferRefusal),
}

impl WorkspaceTransferFeedback {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::CapabilityAbsent | Self::Refused(WorkspaceTransferRefusal::CapabilityAbsent) => {
                "Workspace tear-out is unavailable with this server"
            }
            Self::SoleWorkspace | Self::Refused(WorkspaceTransferRefusal::SoleWorkspace) => {
                "This is the window's only workspace; nothing to detach"
            }
            Self::AlreadyPending => "A workspace tear-out is already pending",
            Self::SendFailed => "Workspace was not detached because the request could not be sent",
            Self::Disconnected => {
                "Workspace tear-out was interrupted; retrying safely after reconnect"
            }
            Self::TimedOutRetrying => "Workspace tear-out has not answered; retrying safely",
            Self::Refused(WorkspaceTransferRefusal::UnknownWorkspace) => {
                "Workspace could not be detached because it no longer exists"
            }
            Self::Refused(WorkspaceTransferRefusal::NotWorkspaceOwner) => {
                "Workspace could not be detached because source ownership changed"
            }
            Self::Refused(WorkspaceTransferRefusal::NoWindowControl) => {
                "Workspace could not be detached without window control"
            }
            Self::Refused(WorkspaceTransferRefusal::TargetWindowIdCollision) => {
                "Workspace target collided with an existing window; try again"
            }
            Self::Refused(WorkspaceTransferRefusal::HandoffInProgress) => {
                "Workspace could not be detached during server handoff; try again"
            }
            Self::Refused(WorkspaceTransferRefusal::EnvironmentRebindFailed) => {
                "Workspace environment restore data could not be rebound; source was unchanged"
            }
        }
    }
}

/// Foreground work parked by the IPC thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkspaceTransferOutcome {
    Open(TransferredWindow),
    Feedback(WorkspaceTransferFeedback),
}

/// Whether a transfer result matched the pending request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTransferResultDisposition {
    Accepted,
    Ignored,
}

#[derive(Debug, Clone, Copy)]
struct PendingTransfer {
    request: WorkspaceTransferRequest,
    target: TransferredWindow,
    ever_sent: bool,
    deadline: Option<Instant>,
    retry_due: bool,
}

/// One source window's transfer coordinator.
#[derive(Debug, Default)]
pub struct WorkspaceTransfers {
    pending: Option<PendingTransfer>,
    outcomes: VecDeque<WorkspaceTransferOutcome>,
}

impl WorkspaceTransfers {
    /// Begin one transfer without touching the source layout.
    pub fn begin(
        &mut self,
        capable: bool,
        workspace_count: usize,
        request: WorkspaceTransferRequest,
        spec: TransferWindowSpec,
    ) -> Result<WorkspaceTransferRequest, WorkspaceTransferFeedback> {
        if !capable {
            return Err(WorkspaceTransferFeedback::CapabilityAbsent);
        }
        if workspace_count <= 1 {
            return Err(WorkspaceTransferFeedback::SoleWorkspace);
        }
        if self.pending.is_some() {
            return Err(WorkspaceTransferFeedback::AlreadyPending);
        }
        self.pending = Some(PendingTransfer {
            request,
            target: TransferredWindow { window_id: request.target_window, spec },
            ever_sent: false,
            deadline: None,
            retry_due: false,
        });
        Ok(request)
    }

    /// Record successful admission to the ordered writer queue.
    pub fn mark_sent(&mut self, transfer_id: u64, now: Instant) -> bool {
        let Some(pending) =
            self.pending.as_mut().filter(|pending| pending.request.correlation == transfer_id)
        else {
            return false;
        };
        pending.ever_sent = true;
        pending.deadline = Some(now + WORKSPACE_TRANSFER_TIMEOUT);
        pending.retry_due = false;
        true
    }

    /// Recover from writer admission failure.
    ///
    /// A first-send failure is known not to have reached the server. A retry
    /// failure follows an earlier admitted send, so its outcome remains unknown
    /// and is retried after the connection recovers.
    pub fn send_failed(&mut self, transfer_id: u64) {
        let Some(pending) =
            self.pending.as_mut().filter(|pending| pending.request.correlation == transfer_id)
        else {
            return;
        };
        if pending.ever_sent {
            pending.deadline = None;
            // The writer's failure tears the stream. Wait for the connection
            // supervisor to observe that edge before attempting another send.
            pending.retry_due = false;
        } else {
            self.pending = None;
            self.outcomes.push_back(WorkspaceTransferOutcome::Feedback(
                WorkspaceTransferFeedback::SendFailed,
            ));
        }
    }

    /// Keep an admitted request recoverable across a stream loss.
    pub fn disconnected(&mut self) -> bool {
        let Some(pending) = self.pending.as_mut().filter(|pending| pending.ever_sent) else {
            return false;
        };
        if pending.retry_due && pending.deadline.is_none() {
            return false;
        }
        pending.deadline = None;
        pending.retry_due = true;
        self.outcomes
            .push_back(WorkspaceTransferOutcome::Feedback(WorkspaceTransferFeedback::Disconnected));
        true
    }

    /// Return a same-id retry after timeout or reconnect.
    pub fn retry_if_due(
        &mut self,
        now: Instant,
        can_retry: bool,
    ) -> Option<WorkspaceTransferRequest> {
        let pending = self.pending.as_mut()?;
        if pending.deadline.is_some_and(|deadline| deadline <= now) {
            pending.deadline = None;
            pending.retry_due = true;
            self.outcomes.push_back(WorkspaceTransferOutcome::Feedback(
                WorkspaceTransferFeedback::TimedOutRetrying,
            ));
        }
        if !pending.retry_due || !can_retry {
            return None;
        }
        Some(pending.request)
    }

    /// Correlate one result. Matching success parks exactly one window open;
    /// refusals park feedback; unmatched frames have no side effect.
    pub fn receive_result(
        &mut self,
        transfer_id: u64,
        result: WorkspaceTransferResult,
    ) -> WorkspaceTransferResultDisposition {
        let Some(pending) =
            self.pending.filter(|pending| pending.request.correlation == transfer_id)
        else {
            return WorkspaceTransferResultDisposition::Ignored;
        };
        self.pending = None;
        match result {
            WorkspaceTransferResult::Transferred => {
                self.outcomes.push_back(WorkspaceTransferOutcome::Open(pending.target));
            }
            WorkspaceTransferResult::Refused { reason } => {
                self.outcomes.push_back(WorkspaceTransferOutcome::Feedback(
                    WorkspaceTransferFeedback::Refused(reason),
                ));
            }
        }
        WorkspaceTransferResultDisposition::Accepted
    }

    /// Drain foreground work in protocol order.
    pub fn take_outcomes(&mut self) -> Vec<WorkspaceTransferOutcome> {
        self.outcomes.drain(..).collect()
    }

    #[must_use]
    pub const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(transfer_id: u64) -> WorkspaceTransferRequest {
        WorkspaceTransferRequest {
            correlation: transfer_id,
            workspace: WorkspaceId::new(),
            target_window: WindowId::new(),
        }
    }

    fn spec() -> TransferWindowSpec {
        TransferWindowSpec { width: 800.0, height: 600.0, origin: Some((120.0, 80.0)) }
    }

    fn begin_sent(transfers: &mut WorkspaceTransfers, transfer_id: u64, now: Instant) {
        let request = request(transfer_id);
        transfers.begin(true, 2, request, spec()).expect("transfer should begin");
        assert!(transfers.mark_sent(transfer_id, now));
    }

    #[test]
    fn sole_workspace_and_absent_capability_send_nothing() {
        let mut transfers = WorkspaceTransfers::default();
        assert_eq!(
            transfers.begin(false, 2, request(1), spec()),
            Err(WorkspaceTransferFeedback::CapabilityAbsent)
        );
        assert_eq!(
            transfers.begin(true, 1, request(2), spec()),
            Err(WorkspaceTransferFeedback::SoleWorkspace)
        );
        assert!(!transfers.has_pending());
    }

    #[test]
    fn every_typed_refusal_settles_with_specific_feedback() {
        let reasons = [
            WorkspaceTransferRefusal::UnknownWorkspace,
            WorkspaceTransferRefusal::NotWorkspaceOwner,
            WorkspaceTransferRefusal::NoWindowControl,
            WorkspaceTransferRefusal::CapabilityAbsent,
            WorkspaceTransferRefusal::SoleWorkspace,
            WorkspaceTransferRefusal::TargetWindowIdCollision,
            WorkspaceTransferRefusal::HandoffInProgress,
            WorkspaceTransferRefusal::EnvironmentRebindFailed,
        ];
        for (index, reason) in reasons.into_iter().enumerate() {
            let mut transfers = WorkspaceTransfers::default();
            let transfer_id = index as u64 + 10;
            begin_sent(&mut transfers, transfer_id, Instant::now());
            assert_eq!(
                transfers.receive_result(transfer_id, WorkspaceTransferResult::Refused { reason },),
                WorkspaceTransferResultDisposition::Accepted
            );
            assert_eq!(
                transfers.take_outcomes(),
                vec![WorkspaceTransferOutcome::Feedback(WorkspaceTransferFeedback::Refused(
                    reason
                ))]
            );
            assert!(!WorkspaceTransferFeedback::Refused(reason).message().is_empty());
            assert!(!transfers.has_pending());
        }
    }

    #[test]
    fn timeout_and_disconnect_retry_the_same_correlated_request() {
        let now = Instant::now();
        let mut timed_out = WorkspaceTransfers::default();
        begin_sent(&mut timed_out, 41, now);
        let retry = timed_out
            .retry_if_due(now + WORKSPACE_TRANSFER_TIMEOUT, true)
            .expect("timeout should retry");
        assert_eq!(retry.correlation, 41);
        assert_eq!(
            timed_out.take_outcomes(),
            vec![WorkspaceTransferOutcome::Feedback(WorkspaceTransferFeedback::TimedOutRetrying)]
        );

        let mut disconnected = WorkspaceTransfers::default();
        begin_sent(&mut disconnected, 42, now);
        assert!(disconnected.disconnected());
        assert_eq!(disconnected.retry_if_due(now, false), None);
        assert_eq!(
            disconnected.retry_if_due(now, true).map(|request| request.correlation),
            Some(42)
        );
    }

    #[test]
    fn late_and_duplicate_results_open_the_target_once() {
        let now = Instant::now();
        let mut transfers = WorkspaceTransfers::default();
        let initial = request(51);
        let target = TransferredWindow { window_id: initial.target_window, spec: spec() };
        transfers.begin(true, 2, initial, spec()).unwrap();
        transfers.mark_sent(51, now);
        transfers.retry_if_due(now + WORKSPACE_TRANSFER_TIMEOUT, true);

        assert_eq!(
            transfers.receive_result(51, WorkspaceTransferResult::Transferred),
            WorkspaceTransferResultDisposition::Accepted
        );
        assert_eq!(
            transfers.receive_result(51, WorkspaceTransferResult::Transferred),
            WorkspaceTransferResultDisposition::Ignored
        );
        assert_eq!(
            transfers.receive_result(999, WorkspaceTransferResult::Transferred),
            WorkspaceTransferResultDisposition::Ignored
        );
        assert_eq!(
            transfers.take_outcomes(),
            vec![
                WorkspaceTransferOutcome::Feedback(WorkspaceTransferFeedback::TimedOutRetrying),
                WorkspaceTransferOutcome::Open(target),
            ]
        );

        let next = request(52);
        let next_target = TransferredWindow { window_id: next.target_window, spec: spec() };
        transfers.begin(true, 2, next, spec()).unwrap();
        assert_eq!(
            transfers.receive_result(51, WorkspaceTransferResult::Transferred),
            WorkspaceTransferResultDisposition::Ignored
        );
        assert!(transfers.has_pending());
        assert_eq!(
            transfers.receive_result(52, WorkspaceTransferResult::Transferred),
            WorkspaceTransferResultDisposition::Accepted
        );
        assert_eq!(transfers.take_outcomes(), vec![WorkspaceTransferOutcome::Open(next_target)]);
    }

    #[test]
    fn first_send_failure_is_safe_but_retry_failure_remains_recoverable() {
        let mut first = WorkspaceTransfers::default();
        first.begin(true, 2, request(61), spec()).unwrap();
        first.send_failed(61);
        assert!(!first.has_pending());
        assert_eq!(
            first.take_outcomes(),
            vec![WorkspaceTransferOutcome::Feedback(WorkspaceTransferFeedback::SendFailed)]
        );

        let now = Instant::now();
        let mut retry = WorkspaceTransfers::default();
        begin_sent(&mut retry, 62, now);
        retry.send_failed(62);
        assert!(retry.has_pending());
        assert_eq!(retry.retry_if_due(now, true), None);
        assert!(retry.disconnected());
        assert_eq!(retry.retry_if_due(now, true).map(|request| request.correlation), Some(62));
    }
}
