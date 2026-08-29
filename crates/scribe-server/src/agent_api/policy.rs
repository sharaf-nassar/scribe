//! Default-safe capability policy lifecycle for the local agent API.
//!
//! Prompt-mode requests are correlated by [`PromptId`], parked per
//! `(agent_label, capability, target)` key, and resolved together. The first
//! request occupies one of the 64 bounded slots; request 65 is denied.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use scribe_common::agent::{AgentCapability, AgentError, AgentPolicyMode};
use scribe_common::config::AgentApiConfig;
use scribe_common::protocol::{ClipboardDecision, PromptId};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// Maximum simultaneous requests parked behind one prompt key.
pub const MAX_PENDING_PER_KEY: usize = 64;

/// How long a withdrawn prompt id keeps answering `Always*` decisions.
///
/// A dialog is dismissed and clicked concurrently: the click that was already
/// in flight when the prompt stopped being answerable still carries an explicit
/// user preference, so the key survives long enough to apply it instead of
/// dropping it silently. Once-only decisions on a tombstone stay no-ops — the
/// request they would have answered is already gone.
const PROMPT_TOMBSTONE_TTL: Duration = Duration::from_secs(30);

/// Prompt payload to route to an agent-API-capable client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPolicyPrompt {
    pub prompt_id: PromptId,
    pub agent_label: String,
    pub capability: AgentCapability,
    pub target: String,
}

/// Immediate or deferred result of a capability policy check.
pub enum PolicyResolution {
    Allow,
    Deny,
    Prompt { prompt: AgentPolicyPrompt, pending: PendingAuthorization },
    Parked(PendingAuthorization),
}

/// Awaitable authorization parked behind an issued prompt.
pub struct PendingAuthorization {
    engine: AgentPolicyEngine,
    prompt_id: PromptId,
    waiter_id: u64,
    deadline: Instant,
    receiver: Option<oneshot::Receiver<Result<(), AgentError>>>,
}

impl PendingAuthorization {
    /// Prompt shared by this request and any same-key parked requests.
    #[must_use]
    pub fn prompt_id(&self) -> PromptId {
        self.prompt_id
    }

    /// Await the user's correlated response or the configured prompt timeout.
    pub async fn wait(mut self) -> Result<(), AgentError> {
        let Some(receiver) = self.receiver.take() else {
            return Err(denied("agent capability prompt was cancelled"));
        };
        match tokio::time::timeout_at(self.deadline, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_sender_dropped)) => Err(denied("agent capability prompt was cancelled")),
            Err(_elapsed) => {
                self.engine.expire(self.prompt_id);
                Err(prompt_timeout())
            }
        }
    }
}

impl Drop for PendingAuthorization {
    fn drop(&mut self) {
        self.engine.cancel_waiter(self.prompt_id, self.waiter_id);
    }
}

// @lat: [[server#Server#Agent API#Admission and capability policy]]
/// Shared, in-memory policy and prompt state for the server process.
#[derive(Clone)]
pub struct AgentPolicyEngine {
    inner: Arc<Mutex<PolicyState>>,
    /// Prompt ids that stopped being answerable. The engine holds no writer
    /// references, so the transport takes this stream once and turns each id
    /// into a `ServerMessage::AgentPromptDismiss`, exactly as it does for
    /// activity transitions.
    dismissals: mpsc::UnboundedSender<PromptId>,
}

struct PolicyState {
    config: AgentApiConfig,
    next_prompt_id: u64,
    next_waiter_id: u64,
    pending: HashMap<PromptKey, PendingPrompt>,
    prompt_keys: HashMap<PromptId, PromptKey>,
    last_decisions: HashMap<PromptKey, CachedDecision>,
    /// Keys of prompts that stopped being answerable, kept for
    /// [`PROMPT_TOMBSTONE_TTL`] so a racing click still persists `Always*`.
    tombstones: HashMap<PromptId, Tombstone>,
    dismissals: Option<mpsc::UnboundedReceiver<PromptId>>,
}

struct PendingPrompt {
    prompt_id: PromptId,
    deadline: Instant,
    waiters: HashMap<u64, oneshot::Sender<Result<(), AgentError>>>,
}

#[derive(Clone, PartialEq, Eq)]
struct PromptKey {
    agent_label: String,
    capability: AgentCapability,
    target: String,
}

impl Hash for PromptKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.agent_label.hash(state);
        capability_index(self.capability).hash(state);
        self.target.hash(state);
    }
}

#[derive(Clone, Copy)]
struct CachedDecision {
    decision: ClipboardDecision,
    resolved_at: Instant,
}

struct Tombstone {
    key: PromptKey,
    expires_at: Instant,
}

impl AgentPolicyEngine {
    #[must_use]
    pub fn new(config: AgentApiConfig) -> Self {
        let (dismissals, receiver) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Mutex::new(PolicyState {
                config,
                next_prompt_id: 1,
                next_waiter_id: 1,
                pending: HashMap::new(),
                prompt_keys: HashMap::new(),
                last_decisions: HashMap::new(),
                tombstones: HashMap::new(),
                dismissals: Some(receiver),
            })),
            dismissals,
        }
    }

    /// Take the withdrawn-prompt stream. The server consumes it once at startup.
    pub fn take_dismissals(&self) -> Option<mpsc::UnboundedReceiver<PromptId>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).dismissals.take()
    }

    /// Resolve one request without looking up its target first.
    ///
    /// `has_capable_client` is consulted only for `Prompt`: explicit `Allow`
    /// remains usable by a headless server, while an unrenderable prompt denies.
    #[must_use]
    pub fn authorize(
        &self,
        agent_label: impl Into<String>,
        capability: AgentCapability,
        target: impl Into<String>,
        has_capable_client: bool,
    ) -> PolicyResolution {
        self.authorize_at(
            PromptKey { agent_label: agent_label.into(), capability, target: target.into() },
            has_capable_client,
            Instant::now(),
        )
    }

    fn authorize_at(
        &self,
        key: PromptKey,
        has_capable_client: bool,
        now: Instant,
    ) -> PolicyResolution {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match mode_for(&state.config, key.capability) {
            AgentPolicyMode::Allow => return PolicyResolution::Allow,
            AgentPolicyMode::Deny => return PolicyResolution::Deny,
            AgentPolicyMode::Prompt if !has_capable_client => return PolicyResolution::Deny,
            AgentPolicyMode::Prompt => {}
        }

        if let Some(cached) = state.last_decisions.get(&key).copied() {
            let window = Duration::from_millis(state.config.burst_window_ms);
            if !window.is_zero() && now.duration_since(cached.resolved_at) < window {
                return resolution_for(cached.decision);
            }
            state.last_decisions.remove(&key);
        }

        if let Some((prompt_id, deadline)) =
            state.pending.get(&key).map(|pending| (pending.prompt_id, pending.deadline))
        {
            if state
                .pending
                .get(&key)
                .is_some_and(|pending| pending.waiters.len() >= MAX_PENDING_PER_KEY)
            {
                return PolicyResolution::Deny;
            }
            let (sender, receiver) = oneshot::channel();
            let waiter_id = allocate_waiter_id(&mut state);
            if let Some(pending) = state.pending.get_mut(&key) {
                pending.waiters.insert(waiter_id, sender);
            }
            return PolicyResolution::Parked(PendingAuthorization {
                engine: self.clone(),
                prompt_id,
                waiter_id,
                deadline,
                receiver: Some(receiver),
            });
        }

        let prompt_id = allocate_prompt_id(&mut state);
        let waiter_id = allocate_waiter_id(&mut state);
        let deadline = now + Duration::from_millis(state.config.prompt_timeout_ms);
        let (sender, receiver) = oneshot::channel();
        state.pending.insert(
            key.clone(),
            PendingPrompt { prompt_id, deadline, waiters: HashMap::from([(waiter_id, sender)]) },
        );
        state.prompt_keys.insert(prompt_id, key.clone());

        PolicyResolution::Prompt {
            prompt: AgentPolicyPrompt {
                prompt_id,
                agent_label: key.agent_label,
                capability: key.capability,
                target: key.target,
            },
            pending: PendingAuthorization {
                engine: self.clone(),
                prompt_id,
                waiter_id,
                deadline,
                receiver: Some(receiver),
            },
        }
    }

    /// Apply a correlated client decision. Mismatched ids are no-ops.
    ///
    /// `AlwaysAllow` and `AlwaysDeny` immediately mutate the matching
    /// capability's in-memory mode; the config-write round trip can persist the
    /// same mode later without delaying current requests. A decision that
    /// raced its prompt's withdrawal lands on the tombstone instead, where
    /// `Always*` still persists and once-only choices are no-ops.
    pub fn resolve(&self, prompt_id: PromptId, decision: ClipboardDecision) -> bool {
        self.resolve_at(prompt_id, decision, Instant::now())
    }

    fn resolve_at(&self, prompt_id: PromptId, decision: ClipboardDecision, now: Instant) -> bool {
        let (waiters, result) = {
            let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(key) = state.prompt_keys.remove(&prompt_id) else {
                return resolve_tombstoned(&mut state, prompt_id, decision, now);
            };
            let Some(pending) = state.pending.remove(&key) else {
                return false;
            };
            apply_always_decision(&mut state.config, key.capability, decision);
            state.last_decisions.insert(key, CachedDecision { decision, resolved_at: now });
            (pending.waiters, result_for(decision))
        };
        for sender in waiters.into_values() {
            drop(sender.send(result.clone()));
        }
        true
    }

    /// Replace live policy and cancel every prompt issued under the old one.
    ///
    /// Existing tombstones die with the old policy: the reloaded config is the
    /// user's own current answer, so a late `Always*` from a prompt raised
    /// before the reload must not overwrite it.
    pub fn refresh(&self, config: AgentApiConfig) {
        let (waiters, dismissed) = {
            let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            state.config = config;
            state.last_decisions.clear();
            state.tombstones.clear();
            let dismissed = std::mem::take(&mut state.prompt_keys).into_keys().collect::<Vec<_>>();
            let waiters = std::mem::take(&mut state.pending)
                .into_values()
                .flat_map(|pending| pending.waiters.into_values())
                .collect::<Vec<_>>();
            (waiters, dismissed)
        };
        for prompt_id in dismissed {
            self.dismissals.send(prompt_id).ok();
        }
        let result = Err(denied("agent capability prompt cancelled by policy refresh"));
        for sender in waiters {
            drop(sender.send(result.clone()));
        }
    }

    /// Current in-memory policy, including any applied `Always*` decision.
    #[must_use]
    pub fn config(&self) -> AgentApiConfig {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).config.clone()
    }

    fn expire(&self, prompt_id: PromptId) {
        let waiters = {
            let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(key) = state.prompt_keys.remove(&prompt_id) else {
                return;
            };
            let waiters = state.pending.remove(&key).map(|pending| pending.waiters);
            entomb(&mut state, prompt_id, key);
            waiters
        };
        self.dismissals.send(prompt_id).ok();
        if let Some(waiters) = waiters {
            let result = Err(prompt_timeout());
            for sender in waiters.into_values() {
                drop(sender.send(result.clone()));
            }
        }
    }

    fn cancel_waiter(&self, prompt_id: PromptId, waiter_id: u64) {
        {
            let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(key) = state.prompt_keys.get(&prompt_id).cloned() else {
                return;
            };
            let empty = state.pending.get_mut(&key).is_some_and(|pending| {
                pending.waiters.remove(&waiter_id);
                pending.waiters.is_empty()
            });
            if !empty {
                return;
            }
            state.pending.remove(&key);
            state.prompt_keys.remove(&prompt_id);
            entomb(&mut state, prompt_id, key);
        }
        self.dismissals.send(prompt_id).ok();
    }
}

/// Retire one prompt id that can no longer be answered, keeping its key
/// answerable for `Always*` until [`PROMPT_TOMBSTONE_TTL`] elapses.
fn entomb(state: &mut PolicyState, prompt_id: PromptId, key: PromptKey) {
    let now = Instant::now();
    state.tombstones.retain(|_, tombstone| tombstone.expires_at > now);
    state.tombstones.insert(prompt_id, Tombstone { key, expires_at: now + PROMPT_TOMBSTONE_TTL });
}

/// Apply a decision that arrived after its prompt was withdrawn.
///
/// Only `Always*` survives — it is a durable preference the user typed into a
/// dialog that was still on screen, so dropping it would lose real intent. No
/// waiter is signalled and no burst decision is cached: the request this would
/// have answered already failed.
fn resolve_tombstoned(
    state: &mut PolicyState,
    prompt_id: PromptId,
    decision: ClipboardDecision,
    now: Instant,
) -> bool {
    state.tombstones.retain(|_, tombstone| tombstone.expires_at > now);
    let Some(capability) =
        state.tombstones.get(&prompt_id).map(|tombstone| tombstone.key.capability)
    else {
        return false;
    };
    if !apply_always_decision(&mut state.config, capability, decision) {
        return false;
    }
    state.tombstones.remove(&prompt_id);
    true
}

impl Default for AgentPolicyEngine {
    fn default() -> Self {
        Self::new(AgentApiConfig::default())
    }
}

/// Return the configured mode for one capability.
#[must_use]
pub fn mode_for(config: &AgentApiConfig, capability: AgentCapability) -> AgentPolicyMode {
    match capability {
        AgentCapability::ReadMetadata => config.read_metadata,
        AgentCapability::ReadContent => config.read_content,
        AgentCapability::DispatchAction => config.dispatch_action,
        AgentCapability::DispatchDestructiveAction => config.dispatch_destructive_action,
        AgentCapability::WriteInput => config.write_input,
    }
}

fn allocate_prompt_id(state: &mut PolicyState) -> PromptId {
    let id = PromptId(state.next_prompt_id);
    state.next_prompt_id = state.next_prompt_id.wrapping_add(1);
    id
}

fn allocate_waiter_id(state: &mut PolicyState) -> u64 {
    let id = state.next_waiter_id;
    state.next_waiter_id = state.next_waiter_id.wrapping_add(1);
    id
}

const fn capability_index(capability: AgentCapability) -> u8 {
    match capability {
        AgentCapability::ReadMetadata => 0,
        AgentCapability::ReadContent => 1,
        AgentCapability::DispatchAction => 2,
        AgentCapability::DispatchDestructiveAction => 3,
        AgentCapability::WriteInput => 4,
    }
}

/// Persist an `Always*` choice onto its capability axis, reporting whether the
/// decision was one that changes policy at all.
fn apply_always_decision(
    config: &mut AgentApiConfig,
    capability: AgentCapability,
    decision: ClipboardDecision,
) -> bool {
    let mode = match decision {
        ClipboardDecision::AlwaysAllow => AgentPolicyMode::Allow,
        ClipboardDecision::AlwaysDeny => AgentPolicyMode::Deny,
        ClipboardDecision::AllowOnce | ClipboardDecision::DenyOnce => return false,
    };
    match capability {
        AgentCapability::ReadMetadata => config.read_metadata = mode,
        AgentCapability::ReadContent => config.read_content = mode,
        AgentCapability::DispatchAction => config.dispatch_action = mode,
        AgentCapability::DispatchDestructiveAction => config.dispatch_destructive_action = mode,
        AgentCapability::WriteInput => config.write_input = mode,
    }
    true
}

const fn is_allowed(decision: ClipboardDecision) -> bool {
    matches!(decision, ClipboardDecision::AllowOnce | ClipboardDecision::AlwaysAllow)
}

fn resolution_for(decision: ClipboardDecision) -> PolicyResolution {
    if is_allowed(decision) { PolicyResolution::Allow } else { PolicyResolution::Deny }
}

fn result_for(decision: ClipboardDecision) -> Result<(), AgentError> {
    if is_allowed(decision) { Ok(()) } else { Err(denied("agent capability denied by user")) }
}

fn denied(message: &str) -> AgentError {
    AgentError::Denied { message: message.into() }
}

fn prompt_timeout() -> AgentError {
    AgentError::PromptTimeout { message: "agent capability prompt timed out".into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_config() -> AgentApiConfig {
        AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            write_input: AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        }
    }

    fn issue_prompt(
        engine: &AgentPolicyEngine,
        now: Instant,
    ) -> (AgentPolicyPrompt, PendingAuthorization) {
        match engine.authorize_at(
            PromptKey {
                agent_label: "agent-a".into(),
                capability: AgentCapability::ReadContent,
                target: "session-1".into(),
            },
            true,
            now,
        ) {
            PolicyResolution::Prompt { prompt, pending } => (prompt, pending),
            _ => panic!("expected prompt"),
        }
    }

    #[test]
    fn modes_resolve_allow_and_default_safe_deny() {
        let denied_engine = AgentPolicyEngine::default();
        assert!(matches!(
            denied_engine.authorize("agent", AgentCapability::ReadMetadata, "server", false),
            PolicyResolution::Deny
        ));

        let allowed_engine = AgentPolicyEngine::new(AgentApiConfig {
            read_metadata: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });
        assert!(matches!(
            allowed_engine.authorize("agent", AgentCapability::ReadMetadata, "server", false),
            PolicyResolution::Allow
        ));
    }

    #[test]
    fn prompt_issues_and_same_key_request_parks() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let now = Instant::now();
        let (prompt, first) = issue_prompt(&engine, now);
        assert_eq!(prompt.agent_label, "agent-a");
        assert_eq!(prompt.capability, AgentCapability::ReadContent);
        assert_eq!(prompt.target, "session-1");
        assert_eq!(first.prompt_id(), prompt.prompt_id);

        let second = engine.authorize_at(
            PromptKey {
                agent_label: "agent-a".into(),
                capability: AgentCapability::ReadContent,
                target: "session-1".into(),
            },
            true,
            now,
        );
        assert!(matches!(second, PolicyResolution::Parked(_)));
    }

    #[tokio::test]
    async fn prompt_response_correlates_and_resolves_parked_requests() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let now = Instant::now();
        let (prompt, first) = issue_prompt(&engine, now);
        let PolicyResolution::Parked(second) = engine.authorize_at(
            PromptKey {
                agent_label: "agent-a".into(),
                capability: AgentCapability::ReadContent,
                target: "session-1".into(),
            },
            true,
            now,
        ) else {
            panic!("expected parked request");
        };

        assert!(!engine.resolve(PromptId(prompt.prompt_id.0 + 1), ClipboardDecision::AllowOnce));
        assert!(engine.resolve(prompt.prompt_id, ClipboardDecision::AllowOnce));
        assert!(first.wait().await.is_ok());
        assert!(second.wait().await.is_ok());
        assert!(!engine.resolve(prompt.prompt_id, ClipboardDecision::DenyOnce));
    }

    #[test]
    fn prompt_mode_denies_without_capable_client() {
        let engine = AgentPolicyEngine::new(prompt_config());
        assert!(matches!(
            engine.authorize("agent-a", AgentCapability::ReadContent, "session-1", false),
            PolicyResolution::Deny
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn prompt_uses_configured_timeout_and_denies_every_waiter() {
        let engine = AgentPolicyEngine::new(AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            prompt_timeout_ms: 37,
            ..AgentApiConfig::default()
        });
        let now = Instant::now();
        let (_prompt, first) = issue_prompt(&engine, now);
        let PolicyResolution::Parked(second) = engine.authorize_at(
            PromptKey {
                agent_label: "agent-a".into(),
                capability: AgentCapability::ReadContent,
                target: "session-1".into(),
            },
            true,
            now,
        ) else {
            panic!("expected parked request");
        };
        let first_wait = tokio::spawn(first.wait());
        let second_wait = tokio::spawn(second.wait());

        tokio::time::advance(Duration::from_millis(37)).await;
        assert!(matches!(first_wait.await, Ok(Err(AgentError::PromptTimeout { .. }))));
        assert!(matches!(second_wait.await, Ok(Err(AgentError::PromptTimeout { .. }))));
    }

    #[test]
    fn burst_reuse_requires_exact_key_and_stays_inside_window() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let now = Instant::now();
        let (prompt, pending) = issue_prompt(&engine, now);
        assert!(engine.resolve_at(prompt.prompt_id, ClipboardDecision::AllowOnce, now));
        drop(pending);

        let exact = PromptKey {
            agent_label: "agent-a".into(),
            capability: AgentCapability::ReadContent,
            target: "session-1".into(),
        };
        assert!(matches!(
            engine.authorize_at(exact.clone(), true, now + Duration::from_millis(499)),
            PolicyResolution::Allow
        ));
        assert!(matches!(
            engine.authorize_at(
                PromptKey { target: "session-2".into(), ..exact.clone() },
                true,
                now + Duration::from_millis(499)
            ),
            PolicyResolution::Prompt { .. }
        ));
        assert!(matches!(
            engine.authorize_at(
                PromptKey { agent_label: "agent-b".into(), ..exact.clone() },
                true,
                now + Duration::from_millis(499)
            ),
            PolicyResolution::Prompt { .. }
        ));
        assert!(matches!(
            engine.authorize_at(
                PromptKey { capability: AgentCapability::WriteInput, ..exact },
                true,
                now + Duration::from_millis(499)
            ),
            PolicyResolution::Prompt { .. }
        ));
    }

    #[test]
    fn burst_window_boundary_requires_a_fresh_prompt() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let now = Instant::now();
        let (prompt, pending) = issue_prompt(&engine, now);
        assert!(engine.resolve_at(prompt.prompt_id, ClipboardDecision::DenyOnce, now));
        drop(pending);

        assert!(matches!(
            engine.authorize_at(
                PromptKey {
                    agent_label: "agent-a".into(),
                    capability: AgentCapability::ReadContent,
                    target: "session-1".into(),
                },
                true,
                now + Duration::from_millis(500)
            ),
            PolicyResolution::Prompt { .. }
        ));
    }

    #[test]
    fn request_65_for_the_same_key_is_denied() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let now = Instant::now();
        let (_prompt, first) = issue_prompt(&engine, now);
        let mut pending = vec![first];
        for _ in 1..MAX_PENDING_PER_KEY {
            match engine.authorize_at(
                PromptKey {
                    agent_label: "agent-a".into(),
                    capability: AgentCapability::ReadContent,
                    target: "session-1".into(),
                },
                true,
                now,
            ) {
                PolicyResolution::Parked(request) => pending.push(request),
                _ => panic!("requests 2 through 64 must park"),
            }
        }
        assert_eq!(pending.len(), MAX_PENDING_PER_KEY);
        assert!(matches!(
            engine.authorize_at(
                PromptKey {
                    agent_label: "agent-a".into(),
                    capability: AgentCapability::ReadContent,
                    target: "session-1".into(),
                },
                true,
                now
            ),
            PolicyResolution::Deny
        ));
    }

    #[tokio::test]
    async fn always_decisions_mutate_only_the_matching_capability() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let now = Instant::now();
        let (allow_prompt, allow_pending) = issue_prompt(&engine, now);
        assert!(engine.resolve(allow_prompt.prompt_id, ClipboardDecision::AlwaysAllow));
        assert!(allow_pending.wait().await.is_ok());
        assert_eq!(engine.config().read_content, AgentPolicyMode::Allow);
        assert_eq!(engine.config().write_input, AgentPolicyMode::Prompt);
        assert!(matches!(
            engine.authorize("agent-a", AgentCapability::ReadContent, "session-1", false),
            PolicyResolution::Allow
        ));

        engine.refresh(AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        });
        let (deny_prompt, deny_pending) = issue_prompt(&engine, Instant::now());
        assert!(engine.resolve(deny_prompt.prompt_id, ClipboardDecision::AlwaysDeny));
        assert!(matches!(deny_pending.wait().await, Err(AgentError::Denied { .. })));
        assert_eq!(engine.config().read_content, AgentPolicyMode::Deny);
        assert!(matches!(
            engine.authorize("agent-a", AgentCapability::ReadContent, "session-1", true),
            PolicyResolution::Deny
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn late_always_decision_still_persists_after_prompt_expiry() {
        let engine = AgentPolicyEngine::new(AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            prompt_timeout_ms: 37,
            ..AgentApiConfig::default()
        });
        let now = Instant::now();
        let (prompt, pending) = issue_prompt(&engine, now);
        let wait = tokio::spawn(pending.wait());
        tokio::time::advance(Duration::from_millis(37)).await;
        assert!(matches!(wait.await, Ok(Err(AgentError::PromptTimeout { .. }))));
        // The dialog is still visible; the user clicks "Always allow".
        let applied = engine.resolve(prompt.prompt_id, ClipboardDecision::AlwaysAllow);
        assert!(applied, "a late Always decision was silently dropped");
        assert_eq!(
            mode_for(&engine.config(), AgentCapability::ReadContent),
            AgentPolicyMode::Allow,
            "Always allow did not persist after the prompt expired"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_withdrawn_prompt_is_dismissed_once_and_forgets_once_only_decisions() {
        let engine = AgentPolicyEngine::new(AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            prompt_timeout_ms: 37,
            ..AgentApiConfig::default()
        });
        let mut dismissals = engine.take_dismissals().expect("dismissal stream");
        assert!(engine.take_dismissals().is_none(), "the stream is taken exactly once");

        let (prompt, pending) = issue_prompt(&engine, Instant::now());
        let wait = tokio::spawn(pending.wait());
        tokio::time::advance(Duration::from_millis(37)).await;
        assert!(matches!(wait.await, Ok(Err(AgentError::PromptTimeout { .. }))));

        assert_eq!(dismissals.recv().await, Some(prompt.prompt_id));
        assert!(dismissals.try_recv().is_err(), "the dropped waiter must not dismiss twice");

        // A once-only click on the tombstone changes nothing.
        assert!(!engine.resolve(prompt.prompt_id, ClipboardDecision::AllowOnce));
        assert_eq!(engine.config().read_content, AgentPolicyMode::Prompt);

        // The tombstone expires, so a much later click is dropped again.
        tokio::time::advance(super::PROMPT_TOMBSTONE_TTL + Duration::from_secs(1)).await;
        assert!(!engine.resolve(prompt.prompt_id, ClipboardDecision::AlwaysAllow));
        assert_eq!(engine.config().read_content, AgentPolicyMode::Prompt);
    }

    #[tokio::test]
    async fn dropping_the_last_waiter_dismisses_its_prompt() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let mut dismissals = engine.take_dismissals().expect("dismissal stream");
        let now = Instant::now();
        let (prompt, first) = issue_prompt(&engine, now);
        let PolicyResolution::Parked(second) = engine.authorize_at(
            PromptKey {
                agent_label: "agent-a".into(),
                capability: AgentCapability::ReadContent,
                target: "session-1".into(),
            },
            true,
            now,
        ) else {
            panic!("expected parked request");
        };

        drop(first);
        assert!(dismissals.try_recv().is_err(), "a prompt with waiters left is still answerable");
        drop(second);
        assert_eq!(dismissals.recv().await, Some(prompt.prompt_id));
    }

    #[tokio::test]
    async fn policy_refresh_dismisses_every_prompt_it_cancels() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let mut dismissals = engine.take_dismissals().expect("dismissal stream");
        let now = Instant::now();
        let (prompt, pending) = issue_prompt(&engine, now);

        engine.refresh(AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        });

        assert_eq!(dismissals.recv().await, Some(prompt.prompt_id));
        assert!(matches!(pending.wait().await, Err(AgentError::Denied { .. })));
        // The reloaded config outranks a decision from before it landed.
        assert!(!engine.resolve(prompt.prompt_id, ClipboardDecision::AlwaysAllow));
        assert_eq!(engine.config().read_content, AgentPolicyMode::Prompt);
    }

    #[tokio::test]
    async fn live_refresh_cancels_pending_and_applies_new_config() {
        let engine = AgentPolicyEngine::new(prompt_config());
        let now = Instant::now();
        let (prompt, pending) = issue_prompt(&engine, now);
        engine.refresh(AgentApiConfig {
            read_content: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });

        assert!(matches!(pending.wait().await, Err(AgentError::Denied { .. })));
        assert!(!engine.resolve(prompt.prompt_id, ClipboardDecision::AllowOnce));
        assert!(matches!(
            engine.authorize("agent-a", AgentCapability::ReadContent, "session-1", false),
            PolicyResolution::Allow
        ));
    }
}
