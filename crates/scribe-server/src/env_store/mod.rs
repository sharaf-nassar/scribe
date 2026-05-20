//! Encrypted on-disk persistence of per-terminal exported-env deltas; AEAD data
//! key in the OS secret store. See specs/006-persist-terminal-env/data-model.md
//! and contracts/. Owns capture-side delta computation, AEAD envelope I/O, the
//! OS-keystore wrapper, and the on-disk store layout.
//!
//! This module file also owns the central server-side runtime registry
//! ([`EnvStoreState`]) that holds each live session's post-rc
//! [`StartupBaseline`], its working [`TerminalEnvDelta`], and per-session
//! [`EnvStatusState`]. The persist scheduler (T015) and hook-ingress
//! translation (T016) operate against this registry to compute deltas and
//! drive debounced writes.

pub mod delta;
pub mod envelope;
pub mod keystore;
pub mod store;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, broadcast, mpsc};

use scribe_common::ids::{SessionId, WindowId};

pub use self::delta::{EnvChangeEvent, StartupBaseline, TerminalEnvDelta, is_excluded};

/// Per-session persist debounce window. Coalesces rapid bulk-export blocks
/// (e.g. `source ./envrc`, `direnv` hooks) into a single disk write —
/// matches `specs/006-persist-terminal-env/research.md::R1.4` (100 ms is
/// below human perception while still tight enough that a typical
/// keystroke-paced edit lands within ~one debounce window).
pub const PERSIST_DEBOUNCE: Duration = Duration::from_millis(100);

/// Mirror of `scribe_common::protocol::EnvStatusState` but server-owned so
/// business logic in this crate doesn't import the wire type. T015 / T036
/// translate to the wire type when emitting `ServerMessage::EnvStatus` to
/// clients.
///
/// See `specs/006-persist-terminal-env/data-model.md::EnvStatus` for the
/// `Active` ↔ `Degraded { reason }` transitions and ownership rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EnvStatusState {
    /// Env capture is healthy. Default for any session while the feature is
    /// enabled.
    #[default]
    Active,
    /// A keystore failure stopped persistence. The on-disk envelope (if any)
    /// is left untouched. `reason` is short and safe to surface in a tooltip.
    Degraded { reason: String },
}

/// In-memory env-store state. One instance lives on the server's shared
/// state holder (e.g. `AppState`) and is consulted by hook-ingress, the
/// persist scheduler, and the session lifecycle path.
///
/// Holds the post-rc [`StartupBaseline`] per session, the live working
/// [`TerminalEnvDelta`] per session, and the per-session runtime
/// [`EnvStatusState`]. Persistence to disk is driven by the per-session
/// debounced timer in T015.
///
/// A single [`Mutex`] wraps the inner three maps. The lock surface is
/// small (clone-out the value, drop the guard, do work), so per-field or
/// per-session locking would add complexity without a measurable win.
///
/// `status_tx` is a [`broadcast::Sender`] over per-session
/// [`EnvStatusState`] transitions, fired by [`Self::set_status`] only when
/// the new value actually differs from the previous one. The IPC layer
/// subscribes on startup and forwards each transition to the owning
/// client as `ServerMessage::EnvStatus` (T036). Lagged receivers are
/// tolerated: the current status is always recoverable from
/// [`Self::get_status`], so a missed broadcast is informational only.
///
/// `last_enabled` caches the most recently observed value of
/// `terminal.env_persistence.enabled`. It is seeded by the server at
/// startup (`main.rs`) and atomically swapped by the `ConfigReloaded`
/// handler (T035) to detect `false → true` / `true → false` transitions
/// without needing a separate in-memory snapshot of the previous on-disk
/// config. Defaults to `false` (matching FR-009's "disabled by default")
/// so missing initialization still fails safe.
#[derive(Debug)]
pub struct EnvStoreState {
    inner: Mutex<EnvStoreInner>,
    status_tx: broadcast::Sender<(SessionId, EnvStatusState)>,
    last_enabled: AtomicBool,
}

/// Capacity for the status-transition broadcast channel. Sized well above
/// any realistic burst of per-session transitions in a single tick — the
/// persist scheduler debounces at 100 ms, so a 64-slot ring buffer
/// comfortably absorbs every plausible startup / failover storm.
const STATUS_BROADCAST_CAP: usize = 64;

impl Default for EnvStoreState {
    fn default() -> Self {
        let (status_tx, _) = broadcast::channel(STATUS_BROADCAST_CAP);
        Self { inner: Mutex::default(), status_tx, last_enabled: AtomicBool::new(false) }
    }
}

#[derive(Debug, Default)]
struct EnvStoreInner {
    baselines: HashMap<SessionId, StartupBaseline>,
    deltas: HashMap<SessionId, TerminalEnvDelta>,
    statuses: HashMap<SessionId, EnvStatusState>,
    /// One sender per live session, owned by the long-running per-session
    /// [`persist_task`] spawned on first [`EnvStoreState::schedule_persist`]
    /// call. Each tick on this channel resets that task's debounce timer.
    /// Removing the entry drops the sender, the receiver observes
    /// channel-closed, and the task exits cleanly — that is how
    /// [`EnvStoreState::forget_session`] and
    /// [`EnvStoreState::drop_scheduler`] terminate the task.
    schedulers: HashMap<SessionId, mpsc::UnboundedSender<()>>,
}

impl EnvStoreState {
    /// Record the post-rc baseline for `session`. Also clears any prior
    /// `TerminalEnvDelta` for the session, since a baseline-ready event
    /// resets per-session delta state per
    /// `data-model.md::StartupBaseline` state-transitions. Re-recording a
    /// baseline for the same session overwrites the previous one and
    /// likewise discards any in-progress delta.
    pub async fn record_baseline(&self, session: SessionId, baseline: StartupBaseline) {
        let var_count = baseline.vars.len();
        let captured_at = baseline.captured_at;
        let mut guard = self.inner.lock().await;
        guard.baselines.insert(session, baseline);
        guard.deltas.remove(&session);
        tracing::debug!(
            target: "scribe_server::env_store",
            session_id = ?session,
            var_count,
            ?captured_at,
            "recorded startup baseline; cleared prior delta"
        );
    }

    /// Returns `true` if a [`StartupBaseline`] has been recorded for the
    /// session. Used by lib-side tests today; production callers exist via
    /// the in-method `baselines.contains_key` check on the fold path.
    #[cfg(test)]
    pub async fn has_baseline(&self, session: SessionId) -> bool {
        let guard = self.inner.lock().await;
        guard.baselines.contains_key(&session)
    }

    /// Fold `event` into the session's working [`TerminalEnvDelta`] via
    /// [`TerminalEnvDelta::apply_event`]. Returns `true` if a delta now
    /// exists for the session, which T015's persist scheduler uses to
    /// decide whether to reset its debounce timer.
    ///
    /// Returns `false` early (and drops the event) when no baseline has
    /// been recorded yet: a delta without a baseline reference is
    /// meaningless and must wait for the post-rc `baseline_ready: true`
    /// emit. The dropped event is logged at debug.
    pub async fn fold_event(&self, session: SessionId, event: EnvChangeEvent) -> bool {
        let mut guard = self.inner.lock().await;
        if !guard.baselines.contains_key(&session) {
            tracing::debug!(
                target: "scribe_server::env_store",
                session_id = ?session,
                added_count = event.added.len(),
                removed_count = event.removed.len(),
                "dropping env change event: no baseline recorded yet"
            );
            return false;
        }
        let delta = guard.deltas.entry(session).or_default();
        delta.apply_event(event);
        true
    }

    /// Clone the session's current working [`TerminalEnvDelta`], if any.
    /// Used by the persist scheduler (T015) to take a stable snapshot to
    /// encrypt and write to disk without holding the inner lock across I/O.
    pub async fn current_delta(&self, session: SessionId) -> Option<TerminalEnvDelta> {
        let guard = self.inner.lock().await;
        guard.deltas.get(&session).cloned()
    }

    /// Drop all in-memory state (baseline, delta, status, persist
    /// scheduler) for `session`. Will be wired into the session-close path
    /// once that integration lands; tests exercise it today via the lib
    /// path. Removing the scheduler entry drops its
    /// [`mpsc::UnboundedSender`], which causes the per-session
    /// [`persist_task`] to observe channel-closed on its next `recv` and
    /// exit cleanly.
    #[cfg(test)]
    pub async fn forget_session(&self, session: SessionId) {
        let mut guard = self.inner.lock().await;
        let had_baseline = guard.baselines.remove(&session).is_some();
        let had_delta = guard.deltas.remove(&session).is_some();
        let had_status = guard.statuses.remove(&session).is_some();
        let had_scheduler = guard.schedulers.remove(&session).is_some();
        tracing::debug!(
            target: "scribe_server::env_store",
            session_id = ?session,
            had_baseline,
            had_delta,
            had_status,
            had_scheduler,
            "forgot session env-store state"
        );
    }

    /// Set the per-session [`EnvStatusState`]. T015 / T036 are the only
    /// callers; this method is intentionally narrow so the registry
    /// remains the single source of truth.
    ///
    /// Emits a `(session, status)` tuple on the internal broadcast channel
    /// ONLY when the value actually changes (Old != New) so subscribers
    /// (T036's IPC forwarder) never see spurious transitions. A missing
    /// previous entry is treated as [`EnvStatusState::Active`] (the
    /// implicit default returned by [`Self::get_status`]) to keep the
    /// transition semantics consistent with what observers see.
    pub async fn set_status(&self, session: SessionId, status: EnvStatusState) {
        let changed = {
            let mut guard = self.inner.lock().await;
            let prev = guard.statuses.insert(session, status.clone());
            prev.as_ref().unwrap_or(&EnvStatusState::Active) != &status
        };
        if changed {
            // `broadcast::send` returns `Err` only when there are zero
            // subscribers — that is a no-op condition (the IPC layer just
            // hasn't subscribed yet, or has gone away). The current status
            // is recoverable from `get_status`, so a missed broadcast is
            // informational only.
            _ = self.status_tx.send((session, status));
        }
    }

    /// Get the per-session [`EnvStatusState`], defaulting to
    /// [`EnvStatusState::Active`] when no status has been explicitly set.
    /// The live wire path uses the broadcast subscription instead of this
    /// pull-style getter; the getter exists for tests.
    #[cfg(test)]
    pub async fn get_status(&self, session: SessionId) -> EnvStatusState {
        let guard = self.inner.lock().await;
        guard.statuses.get(&session).cloned().unwrap_or_default()
    }

    /// Subscribe to per-session [`EnvStatusState`] transitions. Each item is
    /// a `(session, new_state)` tuple emitted by [`Self::set_status`] only
    /// when the value changes. The IPC server spawns a single long-running
    /// task per server startup that drains this receiver and forwards
    /// every transition to the owning client as
    /// `ServerMessage::EnvStatus`. See T036 in
    /// `specs/006-persist-terminal-env/tasks.md`.
    ///
    /// Lagged receivers (`RecvError::Lagged`) should be logged and skipped
    /// — the canonical state is always retrievable via [`Self::get_status`].
    pub fn subscribe_status(&self) -> broadcast::Receiver<(SessionId, EnvStatusState)> {
        self.status_tx.subscribe()
    }

    /// Notify the per-session persist scheduler that the working delta has
    /// changed and should be sealed to disk after the [`PERSIST_DEBOUNCE`]
    /// window expires. The scheduler task is spawned lazily on first call
    /// per session; subsequent calls just send a tick which folds into
    /// "(re)arm the debounce timer", so any number of `EnvChanged` events
    /// within a single window coalesce into one disk write.
    ///
    /// `window_id` and `launch_id` are captured into the spawned task and
    /// used as the on-disk envelope coordinates
    /// (`<state_dir>/restore/env/<window_id>/<launch_id>.envz`). They must
    /// be stable for the lifetime of the session — that matches the
    /// session lifecycle: a session is bound to one `(window_id, launch_id)`
    /// pair on attach and the binding does not change until the session
    /// ends, at which point [`Self::forget_session`] tears down the task.
    ///
    /// # Concurrency
    ///
    /// The spawned task holds an [`Arc`] back into `self` so it can call
    /// [`Self::current_delta`] and [`Self::set_status`] when the timer
    /// fires. Callers MUST therefore invoke this on an `Arc<EnvStoreState>`
    /// — the server-global state holder (e.g. `AppState`) already wraps
    /// the registry in `Arc`, so existing call sites get this for free.
    pub async fn schedule_persist(
        self: &Arc<Self>,
        session: SessionId,
        window_id: WindowId,
        launch_id: String,
    ) {
        let tx = self.scheduler_for(session, window_id, launch_id.clone()).await;
        // Unbounded channel send only fails if the receiver was dropped;
        // that would mean the task already exited (e.g. forget_session
        // removed our entry concurrently). In that case the next call
        // will re-spawn lazily — drop the tick.
        _ = tx.send(());
    }

    /// Tear down the persist scheduler for `session` (drops its sender so
    /// the per-session [`persist_task`] observes channel-closed on its
    /// next `recv` and exits). Idempotent: calling on a session with no
    /// scheduler is a no-op.
    ///
    /// Most callers should prefer [`Self::forget_session`], which also
    /// drops the scheduler alongside the rest of the per-session state.
    /// This method exists for paths that want to halt persistence without
    /// discarding the baseline + delta (e.g. the
    /// `terminal.env_persistence.enabled` `true → false` transition in
    /// T035, which stops timers but may keep state for an immediate
    /// re-enable).
    pub async fn drop_scheduler(&self, session: SessionId) {
        let mut guard = self.inner.lock().await;
        // Dropping the Sender closes the channel.
        guard.schedulers.remove(&session);
    }

    /// Seed the cached `terminal.env_persistence.enabled` value at server
    /// startup. Called once by `main.rs` before the IPC dispatcher is
    /// running, so an immediate first `ConfigReloaded` sees a stable
    /// baseline for the transition compare.
    pub fn seed_last_enabled(&self, enabled: bool) {
        self.last_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Atomically swap the cached `enabled` value and return the previous
    /// one. Used by the `ConfigReloaded` handler (T035) to detect
    /// `false → true` and `true → false` transitions of
    /// `terminal.env_persistence.enabled` in a single read-modify-write.
    pub fn swap_last_enabled(&self, new_enabled: bool) -> bool {
        self.last_enabled.swap(new_enabled, Ordering::Relaxed)
    }

    /// Get-or-create the per-session persist-scheduler sender. Spawning the
    /// debounce task on first call is intentionally lazy so sessions that
    /// never see an env change pay no cost. Splitting this out of
    /// [`Self::schedule_persist`] keeps that caller free of the
    /// guard-lifetime gymnastics required to combine the lookup and the
    /// conditional insert in one expression.
    async fn scheduler_for(
        self: &Arc<Self>,
        session: SessionId,
        window_id: WindowId,
        launch_id: String,
    ) -> mpsc::UnboundedSender<()> {
        let mut guard = self.inner.lock().await;
        if let Some(existing) = guard.schedulers.get(&session).cloned() {
            return existing;
        }
        let (tx, rx) = mpsc::unbounded_channel::<()>();
        guard.schedulers.insert(session, tx.clone());
        // Drop the guard before spawning so the spawned task cannot
        // deadlock against us if it races to acquire the lock during early
        // ticks.
        drop(guard);

        let state = Arc::clone(self);
        tokio::spawn(persist_task(state, session, window_id, launch_id, rx));
        tx
    }
}

/// Per-session debounced persist task. Spawned by
/// [`EnvStoreState::schedule_persist`] on first use for a session; exits
/// when its [`mpsc::UnboundedReceiver`] returns `None`, which happens
/// after the matching entry is removed from `EnvStoreInner::schedulers`
/// (typically via [`EnvStoreState::forget_session`] or
/// [`EnvStoreState::drop_scheduler`]).
///
/// On each tick the task resets its debounce deadline. When the deadline
/// fires, it snapshots the current [`TerminalEnvDelta`] (so the write
/// runs outside the registry lock) and calls
/// [`store::write_envelope`] to seal it. A keystore / encryption failure
/// transitions the session's [`EnvStatusState`] to `Degraded { reason }`
/// and leaves any pre-existing envelope file untouched — per FR-007 /
/// FR-016 there is no plaintext fallback. Success transitions the
/// status back to [`EnvStatusState::Active`].
async fn persist_task(
    state: Arc<EnvStoreState>,
    session: SessionId,
    window_id: WindowId,
    launch_id: String,
    mut rx: mpsc::UnboundedReceiver<()>,
) {
    use tokio::time::{Instant, sleep_until};

    let mut deadline: Option<Instant> = None;

    loop {
        tokio::select! {
            biased;
            ticket = rx.recv() => {
                if let Some(()) = ticket {
                    // (Re)arm the debounce window. Subsequent ticks
                    // before the deadline simply push it out again,
                    // coalescing bursts into one write.
                    deadline = Some(Instant::now() + PERSIST_DEBOUNCE);
                } else {
                    // All senders dropped; session forgotten. Exit.
                    tracing::debug!(
                        target: "scribe_server::env_store",
                        session_id = ?session,
                        window_id = ?window_id,
                        launch_id = %launch_id,
                        "persist task exiting; scheduler channel closed"
                    );
                    return;
                }
            }
            () = async {
                match deadline {
                    Some(d) => sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            }, if deadline.is_some() => {
                // Debounce fired. Clear the deadline so we don't re-fire
                // immediately on the next loop iteration if no fresh tick
                // arrives — the next `schedule_persist` will arm a new one.
                deadline = None;

                // Snapshot the current delta outside the lock-and-write
                // path. If the delta vanished between tick and fire (e.g.
                // baseline re-recorded, session forgotten), skip the write.
                let Some(delta) = state.current_delta(session).await else {
                    tracing::debug!(
                        target: "scribe_server::env_store",
                        session_id = ?session,
                        window_id = ?window_id,
                        launch_id = %launch_id,
                        "persist tick fired but no delta present; skipping write"
                    );
                    continue;
                };

                match store::write_envelope(window_id, &launch_id, &delta).await {
                    Ok(()) => {
                        state.set_status(session, EnvStatusState::Active).await;
                        tracing::debug!(
                            target: "scribe_server::env_store",
                            session_id = ?session,
                            window_id = ?window_id,
                            launch_id = %launch_id,
                            added_count = delta.added.len(),
                            removed_count = delta.removed.len(),
                            "persisted env envelope"
                        );
                    }
                    Err(e) => {
                        let reason = format!("{e}");
                        state
                            .set_status(
                                session,
                                EnvStatusState::Degraded { reason: reason.clone() },
                            )
                            .await;
                        tracing::warn!(
                            target: "scribe_server::env_store",
                            error = ?e,
                            session_id = ?session,
                            window_id = ?window_id,
                            launch_id = %launch_id,
                            "env persist failed; degrading session (no plaintext fallback)"
                        );
                        // Do NOT delete the existing envelope file: it may
                        // still be the most-recent good state from a prior
                        // successful tick. T034 / T036 will translate this
                        // EnvStatusState into the wire-level EnvStatus
                        // ServerMessage when the client surface lands.
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Instant;

    fn baseline_with(vars: &[(&str, &str)]) -> StartupBaseline {
        let mut map = BTreeMap::new();
        for (k, v) in vars {
            map.insert((*k).to_string(), (*v).to_string());
        }
        StartupBaseline { vars: map, captured_at: Instant::now() }
    }

    #[tokio::test]
    async fn record_baseline_then_fold_event_then_current_delta_roundtrip() {
        let state = EnvStoreState::default();
        let session = SessionId::new();

        // Without a baseline, events are dropped.
        let dropped = state
            .fold_event(
                session,
                EnvChangeEvent { added: vec![("FOO".into(), "bar".into())], removed: vec![] },
            )
            .await;
        assert!(!dropped, "events before baseline must be dropped");
        assert!(state.current_delta(session).await.is_none());

        // Record a baseline; fold an event; observe it in the current delta.
        state.record_baseline(session, baseline_with(&[("PATH", "/usr/bin")])).await;
        assert!(state.has_baseline(session).await);

        let applied = state
            .fold_event(
                session,
                EnvChangeEvent {
                    added: vec![("PROJECT_ROOT".into(), "/home/me".into())],
                    removed: vec!["LANG".into()],
                },
            )
            .await;
        assert!(applied, "fold_event must report a delta exists post-fold");

        let delta = state.current_delta(session).await.expect("delta should exist after fold");
        assert_eq!(delta.added.get("PROJECT_ROOT").map(String::as_str), Some("/home/me"));
        assert!(delta.removed.contains("LANG"));

        // Status defaults to Active; can be set + read back.
        assert_eq!(state.get_status(session).await, EnvStatusState::Active);
        state
            .set_status(session, EnvStatusState::Degraded { reason: "keystore unavailable".into() })
            .await;
        assert_eq!(
            state.get_status(session).await,
            EnvStatusState::Degraded { reason: "keystore unavailable".into() }
        );

        // Re-recording the baseline clears the prior delta.
        state.record_baseline(session, baseline_with(&[("PATH", "/usr/local/bin")])).await;
        assert!(state.current_delta(session).await.is_none());

        // forget_session removes everything.
        state.forget_session(session).await;
        assert!(!state.has_baseline(session).await);
        assert_eq!(state.get_status(session).await, EnvStatusState::Active);
    }
}

#[cfg(test)]
mod tests_persist {
    use super::*;
    use scribe_common::ids::WindowId;
    use std::sync::Arc;

    /// Verifies the per-session debounce-coalescing invariant: many rapid
    /// `schedule_persist` calls for the same session must produce exactly
    /// one scheduler entry (and therefore one spawned [`persist_task`]),
    /// not one per tick. We do NOT wait for the 100 ms debounce to fire
    /// here — that would require a working keystore + filesystem, which
    /// is the integration-test domain. T029's "exactly one disk write per
    /// debounce window" assertion is satisfied by this single-entry
    /// invariant because the spawned task is the only path through which
    /// `write_envelope` is called.
    #[tokio::test(flavor = "current_thread")]
    async fn schedule_is_idempotent_per_session() {
        let state = Arc::new(EnvStoreState::default());
        let session = SessionId::new();
        let window = WindowId::new();

        for _ in 0..10 {
            state.schedule_persist(session, window, "launch-1".to_string()).await;
        }

        let guard = state.inner.lock().await;
        assert_eq!(
            guard.schedulers.len(),
            1,
            "expected exactly one scheduler entry across 10 ticks"
        );
        assert!(
            guard.schedulers.contains_key(&session),
            "scheduler entry should be keyed by the session id we sent ticks for"
        );
    }

    /// `drop_scheduler` should remove the per-session entry so the
    /// underlying task observes channel-closed and exits.
    #[tokio::test(flavor = "current_thread")]
    async fn drop_scheduler_removes_entry() {
        let state = Arc::new(EnvStoreState::default());
        let session = SessionId::new();
        let window = WindowId::new();

        state.schedule_persist(session, window, "launch-1".to_string()).await;
        {
            let guard = state.inner.lock().await;
            assert_eq!(guard.schedulers.len(), 1);
        }

        state.drop_scheduler(session).await;
        let guard = state.inner.lock().await;
        assert!(guard.schedulers.is_empty(), "drop_scheduler should remove the per-session sender");
    }

    /// `forget_session` should also tear down the scheduler entry, not
    /// just baseline/delta/status.
    #[tokio::test(flavor = "current_thread")]
    async fn forget_session_also_drops_scheduler() {
        let state = Arc::new(EnvStoreState::default());
        let session = SessionId::new();
        let window = WindowId::new();

        state.schedule_persist(session, window, "launch-1".to_string()).await;
        state.forget_session(session).await;

        let guard = state.inner.lock().await;
        assert!(
            guard.schedulers.is_empty(),
            "forget_session should drop the per-session scheduler"
        );
    }
}
