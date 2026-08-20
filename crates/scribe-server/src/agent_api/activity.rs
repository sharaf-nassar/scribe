//! Reference-counted per-session agent activity leases (spec 027).
//!
//! A bare boolean broadcast races: the first of two overlapping agent calls
//! would clear the second call's indicator. Each call instead holds an
//! [`AgentActivityLease`] for its duration. The tracker emits one
//! `(session_id, true)` transition when a session's first lease is taken and
//! one `(session_id, false)` transition only after the *last* lease is
//! released and the configured dwell has elapsed with no new lease arriving.
//! The IPC layer forwards each transition as `ServerMessage::AgentActivity`
//! to participants that advertised `agent_api`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use scribe_common::ids::SessionId;
use tokio::sync::mpsc;

/// One emitted indicator transition: the session and its new visible state.
pub type ActivityTransition = (SessionId, bool);

// @lat: [[server#Server#Agent API#Activity leases]]
/// Shared lease registry for the server process.
///
/// Leases are keyed per session *and* per caller connection, so a caller's
/// disconnect can release exactly the references it still holds
/// ([`Self::release_caller`]) and a policy disable can release everything
/// ([`Self::release_all`]) without disturbing the dwell state machine.
#[derive(Clone)]
pub struct AgentActivityTracker {
    dwell: Duration,
    transitions: mpsc::UnboundedSender<ActivityTransition>,
    inner: Arc<Mutex<TrackerState>>,
}

struct TrackerState {
    next_lease_id: u64,
    sessions: HashMap<SessionId, SessionActivity>,
}

/// Spec 027 `AgentActivityLease` state: one indicated session's live leases.
///
/// The entry exists exactly while the indicator shows active — from the first
/// acquire until a dwell timer clears it — so "entry present" needs no
/// separate `indicated` flag.
struct SessionActivity {
    /// Live leases: lease id → holding caller's connection key. The refcount
    /// is the map's size; ids make a stale guard's release a no-op after
    /// [`AgentActivityTracker::release_caller`] already dropped it.
    leases: HashMap<u64, usize>,
    /// Bumped on every acquire. An armed dwell clear fires only if its
    /// captured epoch is still current, so a re-acquire during dwell cancels
    /// the clear and the next release restarts the full dwell.
    epoch: u64,
}

impl AgentActivityTracker {
    /// Build a tracker that emits transitions into `transitions`.
    #[must_use]
    pub fn new(dwell: Duration, transitions: mpsc::UnboundedSender<ActivityTransition>) -> Self {
        Self {
            dwell,
            transitions,
            inner: Arc::new(Mutex::new(TrackerState {
                next_lease_id: 0,
                sessions: HashMap::new(),
            })),
        }
    }

    /// Take one activity reference on `session_id` for `caller`.
    ///
    /// Emits the `true` transition only on the session's first lease; taking
    /// a lease while the session is still dwelling re-arms it silently — the
    /// indicator never flickered off, so there is nothing to re-announce.
    pub fn acquire(&self, session_id: SessionId, caller: usize) -> AgentActivityLease {
        let mut state = self.lock();
        let lease_id = state.next_lease_id;
        state.next_lease_id = state.next_lease_id.wrapping_add(1);
        match state.sessions.entry(session_id) {
            Entry::Occupied(mut occupied) => {
                let activity = occupied.get_mut();
                activity.epoch = activity.epoch.wrapping_add(1);
                activity.leases.insert(lease_id, caller);
            }
            Entry::Vacant(vacant) => {
                vacant.insert(SessionActivity {
                    leases: HashMap::from([(lease_id, caller)]),
                    epoch: 0,
                });
                self.transitions.send((session_id, true)).ok();
            }
        }
        drop(state);
        AgentActivityLease { tracker: self.clone(), session_id, lease_id }
    }

    /// Release every lease still held by one caller connection (disconnect).
    pub fn release_caller(&self, caller: usize) {
        self.release_where(|holder| holder == caller);
    }

    /// Release every lease held by anyone (policy disable). Each cleared
    /// session's indicator still waits out the dwell before turning off.
    pub fn release_all(&self) {
        self.release_where(|_| true);
    }

    fn release_where(&self, evict: impl Fn(usize) -> bool) {
        let cleared = {
            let mut state = self.lock();
            state
                .sessions
                .iter_mut()
                .filter_map(|(&session_id, activity)| {
                    // An already-dwelling session (no leases) keeps its armed
                    // clear; only a held session that empties here arms one.
                    let held = !activity.leases.is_empty();
                    activity.leases.retain(|_, holder| !evict(*holder));
                    (held && activity.leases.is_empty()).then_some((session_id, activity.epoch))
                })
                .collect::<Vec<_>>()
        };
        for (session_id, epoch) in cleared {
            self.arm_dwell(session_id, epoch);
        }
    }

    /// Drop one lease by id; arms the dwell clear when it was the last one.
    /// A lease already evicted by [`Self::release_caller`] /
    /// [`Self::release_all`] is absent and releases nothing.
    fn release(&self, session_id: SessionId, lease_id: u64) {
        let armed = {
            let mut state = self.lock();
            let Some(activity) = state.sessions.get_mut(&session_id) else { return };
            if activity.leases.remove(&lease_id).is_none() || !activity.leases.is_empty() {
                return;
            }
            activity.epoch
        };
        self.arm_dwell(session_id, armed);
    }

    fn arm_dwell(&self, session_id: SessionId, epoch: u64) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // Outside a runtime (server teardown) no timer can run; clear
            // immediately instead of leaking the armed entry.
            self.finish_dwell(session_id, epoch);
            return;
        };
        let tracker = self.clone();
        // Anchor the deadline at release time, not at the spawned task's
        // first poll, so the dwell is measured from the actual release.
        let deadline = tokio::time::Instant::now() + self.dwell;
        handle.spawn(async move {
            tokio::time::sleep_until(deadline).await;
            tracker.finish_dwell(session_id, epoch);
        });
    }

    fn finish_dwell(&self, session_id: SessionId, epoch: u64) {
        let mut state = self.lock();
        // A mismatched epoch means a lease arrived after this clear was
        // armed; that acquire (or the release after it) owns the next clear.
        if state.sessions.get(&session_id).is_none_or(|activity| activity.epoch != epoch) {
            return;
        }
        state.sessions.remove(&session_id);
        self.transitions.send((session_id, false)).ok();
    }

    fn lock(&self) -> MutexGuard<'_, TrackerState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One held activity reference (spec 027 `AgentActivityLease`).
///
/// Dropping it releases the reference; the session's indicator clears only
/// after the last reference drops and the dwell elapses.
#[must_use = "dropping the lease releases the activity reference"]
pub struct AgentActivityLease {
    tracker: AgentActivityTracker,
    session_id: SessionId,
    lease_id: u64,
}

impl Drop for AgentActivityLease {
    fn drop(&mut self) {
        self.tracker.release(self.session_id, self.lease_id);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn tracker(
        dwell_ms: u64,
    ) -> (AgentActivityTracker, mpsc::UnboundedReceiver<ActivityTransition>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (AgentActivityTracker::new(Duration::from_millis(dwell_ms), sender), receiver)
    }

    /// Poll armed dwell tasks at the current paused instant. Plain yields —
    /// never an idle await — so auto-advance cannot skip a dwell boundary.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_leases_do_not_clear_early_and_dwell_gates_the_clear() {
        let (tracker, mut transitions) = tracker(1_500);
        let session = SessionId::new();

        let first = tracker.acquire(session, 1);
        assert_eq!(transitions.try_recv().ok(), Some((session, true)));
        let second = tracker.acquire(session, 2);
        assert!(transitions.try_recv().is_err(), "second lease must not re-announce");

        drop(first);
        tokio::time::advance(Duration::from_millis(1_500)).await;
        settle().await;
        assert!(
            transitions.try_recv().is_err(),
            "first release must not clear while the second lease is held"
        );

        drop(second);
        tokio::time::advance(Duration::from_millis(1_499)).await;
        settle().await;
        assert!(transitions.try_recv().is_err(), "clear must wait out the full dwell");
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(transitions.try_recv().ok(), Some((session, false)));
    }

    #[tokio::test(start_paused = true)]
    async fn reacquire_during_dwell_cancels_the_clear_and_restarts_the_dwell() {
        let (tracker, mut transitions) = tracker(1_500);
        let session = SessionId::new();

        let first = tracker.acquire(session, 1);
        assert_eq!(transitions.try_recv().ok(), Some((session, true)));
        drop(first); // clear armed for t=1500
        tokio::time::advance(Duration::from_secs(1)).await;

        let second = tracker.acquire(session, 1); // t=1000: cancels the armed clear
        settle().await;
        assert!(transitions.try_recv().is_err(), "indicator never cleared, so no duplicate true");
        drop(second); // clear armed for t=2500

        tokio::time::advance(Duration::from_millis(500)).await; // t=1500: stale timer fires
        settle().await;
        assert!(transitions.try_recv().is_err(), "the cancelled clear must not fire");
        tokio::time::advance(Duration::from_millis(999)).await; // t=2499
        settle().await;
        assert!(transitions.try_recv().is_err(), "the restarted dwell runs in full");
        tokio::time::advance(Duration::from_millis(1)).await; // t=2500
        settle().await;
        assert_eq!(transitions.try_recv().ok(), Some((session, false)));
    }

    #[tokio::test(start_paused = true)]
    async fn caller_disconnect_releases_only_that_callers_leases() {
        let (tracker, mut transitions) = tracker(1_500);
        let shared = SessionId::new();
        let solo = SessionId::new();

        let _shared_first = tracker.acquire(shared, 1);
        let _shared_again = tracker.acquire(shared, 1);
        let _shared_other = tracker.acquire(shared, 2);
        let _solo_lease = tracker.acquire(solo, 1);
        assert_eq!(transitions.try_recv().ok(), Some((shared, true)));
        assert_eq!(transitions.try_recv().ok(), Some((solo, true)));

        tracker.release_caller(1);
        tokio::time::advance(Duration::from_millis(1_500)).await;
        settle().await;
        assert_eq!(transitions.try_recv().ok(), Some((solo, false)));
        assert!(transitions.try_recv().is_err(), "caller 2 still holds the shared session");

        tracker.release_caller(2);
        tokio::time::advance(Duration::from_millis(1_500)).await;
        settle().await;
        assert_eq!(transitions.try_recv().ok(), Some((shared, false)));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_lease_drop_after_caller_release_does_not_clear_a_successor() {
        let (tracker, mut transitions) = tracker(1_500);
        let session = SessionId::new();

        let stale = tracker.acquire(session, 1);
        assert_eq!(transitions.try_recv().ok(), Some((session, true)));
        tracker.release_caller(1);
        tokio::time::advance(Duration::from_millis(100)).await;

        let _live = tracker.acquire(session, 2); // re-arms during dwell
        drop(stale); // already evicted: must release nothing
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert!(
            transitions.try_recv().is_err(),
            "a stale guard drop must not clear the successor's lease"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn policy_disable_releases_every_lease_and_clears_after_dwell() {
        let (tracker, mut transitions) = tracker(1_500);
        let first = SessionId::new();
        let second = SessionId::new();

        let _first_lease = tracker.acquire(first, 1);
        let _second_lease = tracker.acquire(second, 2);
        assert_eq!(transitions.try_recv().ok(), Some((first, true)));
        assert_eq!(transitions.try_recv().ok(), Some((second, true)));

        tracker.release_all();
        tokio::time::advance(Duration::from_millis(1_499)).await;
        settle().await;
        assert!(transitions.try_recv().is_err(), "disable still waits out the dwell");

        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        let cleared: HashSet<ActivityTransition> =
            [transitions.try_recv(), transitions.try_recv()].into_iter().flatten().collect();
        assert_eq!(cleared, HashSet::from([(first, false), (second, false)]));
    }
}
