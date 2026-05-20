//! Per-terminal env delta computation against `StartupBaseline`, `ExclusionSet`
//! filtering, size caps. Owns `TerminalEnvDelta`, `StartupBaseline`, and the
//! server-internal `EnvChangeEvent` view used to fold hook-channel updates.
//!
//! See `specs/006-persist-terminal-env/data-model.md::TerminalEnvDelta` and
//! `specs/006-persist-terminal-env/research.md::R1.4` for the byte caps below.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Maximum size of a single environment variable's value. Oversized values
/// are skipped (logged at debug) rather than truncated, per FR-014 / SC-007.
pub const PER_VALUE_CAP_BYTES: usize = 64 * 1024;

/// Maximum total serialized size of one terminal's delta. Excess entries
/// are dropped FIFO with a warning log.
pub const PER_TERMINAL_CAP_BYTES: usize = 512 * 1024;

/// Variable names that are never captured or restored.
///
/// Three categories:
/// * Scribe-internal (terminal identification and hook-channel discovery).
/// * Display / desktop session vars that are minted fresh per session.
/// * Auth sockets, terminal-multiplexer markers, and host/process-specific
///   identifiers that would be invalid in a restored process.
///
/// The list is intentionally conservative: it's safer to omit a variable
/// from restore than to re-inject a stale value. Static at compile time;
/// user-overridable allow/deny lists are out of scope for the MVP.
pub const EXCLUSION_SET: &[&str] = &[
    // Scribe-injected
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "SCRIBE_HOOK_SOCK",
    "SCRIBE_SESSION_ID",
    "SCRIBE_RESTORE_ENV_DELTA_FILE",
    // Terminal identification (Scribe injects fresh values)
    "TERM",
    "COLORTERM",
    // Display / desktop session
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_ID",
    "XDG_SESSION_CLASS",
    "DESKTOP_SESSION",
    // Auth sockets and multiplexer markers
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_CLIENT",
    "SSH_CONNECTION",
    "SSH_TTY",
    "TMUX",
    "TMUX_PANE",
    "STY", // GNU screen
    // Process-id-bearing / shell-managed
    "WINDOWID",
    "_",
    "OLDPWD",
    "SHLVL",
    // Locale/auth tickets with host-specific lifetime
    "KRB5CCNAME",
    "GPG_TTY",
];

/// Returns true when `name` is in the `EXCLUSION_SET`. This is the single
/// lookup point used by `TerminalEnvDelta` to filter both adds and removes
/// before persistence.
#[must_use]
pub fn is_excluded(name: &str) -> bool {
    EXCLUSION_SET.contains(&name)
}

/// Per-terminal exported-env delta against the session's `StartupBaseline`.
/// `added`: name → current value for added/modified vars. `removed`:
/// names unset relative to the baseline. Names in the `ExclusionSet` are
/// filtered out by `apply_event`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalEnvDelta {
    #[serde(default)]
    pub added: BTreeMap<String, String>,
    #[serde(default)]
    pub removed: BTreeSet<String>,
}

/// Server-internal view of one observed hook-channel env change.
///
/// Note: the wire-level `EnvDeltaInput` carries a `baseline_ready` flag, but
/// `hook_ingress` short-circuits the baseline-ready case before constructing
/// this event, so `EnvChangeEvent` itself only ever represents a delta
/// (`baseline_ready: false`). The field is therefore omitted here.
#[derive(Debug, Clone)]
pub struct EnvChangeEvent {
    pub added: Vec<(String, String)>,
    pub removed: Vec<String>,
}

/// Per-session snapshot of exported env after shell startup/rc completes.
/// Captured exactly once per session, in-memory only, never on disk.
#[derive(Debug, Clone)]
pub struct StartupBaseline {
    pub vars: BTreeMap<String, String>,
    pub captured_at: Instant,
}

/// Approximate per-entry overhead for MessagePack-named encoding of
/// (String, String): name length byte(s) + value length byte(s) + a few
/// framing bytes. Used only for cap enforcement, not for byte-perfect
/// sizing.
const SERIALIZED_ENTRY_OVERHEAD: usize = 8;

impl TerminalEnvDelta {
    /// Compute the delta between `current` and `baseline`. Returns entries in
    /// `current` whose value differs from the baseline as `added`, and names
    /// present in the baseline but absent from `current` as `removed`.
    /// Names in the `ExclusionSet` are filtered out of both. Values larger
    /// than `PER_VALUE_CAP_BYTES` are skipped with a debug log. After
    /// assembly, the total serialized-size estimate is capped to
    /// `PER_TERMINAL_CAP_BYTES` by dropping `added` entries in `BTreeMap`
    /// iteration order (deterministic, sorted by name) with a warning log.
    /// Currently only the lib-side tests call this; the planned cold-restore
    /// diff path will adopt it once the restore flow lands.
    #[cfg(test)]
    #[must_use]
    pub fn compute_against_baseline(
        current: &BTreeMap<String, String>,
        baseline: &BTreeMap<String, String>,
    ) -> Self {
        let mut delta = Self::default();

        for (name, value) in current {
            if is_excluded(name) {
                continue;
            }
            if value.len() > PER_VALUE_CAP_BYTES {
                tracing::debug!(
                    target: "scribe_server::env_store",
                    name = %name,
                    value_bytes = value.len(),
                    cap_bytes = PER_VALUE_CAP_BYTES,
                    "skipping oversized env value during baseline diff"
                );
                continue;
            }
            match baseline.get(name) {
                Some(prev) if prev == value => {} // unchanged
                _ => {
                    delta.added.insert(name.clone(), value.clone());
                }
            }
        }

        for name in baseline.keys() {
            if is_excluded(name) {
                continue;
            }
            if !current.contains_key(name) {
                delta.removed.insert(name.clone());
            }
        }

        delta.enforce_terminal_cap();
        delta
    }

    /// Fold a single `EnvChangeEvent` into this delta. `added` entries
    /// overwrite any prior value for the same name and clear any prior
    /// removal; `removed` entries clear any prior add and record the removal.
    /// `ExclusionSet` filtering is applied to both directions. Per-value cap
    /// is enforced on each add; per-terminal cap is re-enforced once after
    /// the whole event has been applied.
    pub fn apply_event(&mut self, event: EnvChangeEvent) {
        for (name, value) in event.added {
            if is_excluded(&name) {
                continue;
            }
            if value.len() > PER_VALUE_CAP_BYTES {
                tracing::debug!(
                    target: "scribe_server::env_store",
                    name = %name,
                    value_bytes = value.len(),
                    cap_bytes = PER_VALUE_CAP_BYTES,
                    "skipping oversized env value in apply_event"
                );
                continue;
            }
            self.removed.remove(&name);
            self.added.insert(name, value);
        }

        for name in event.removed {
            if is_excluded(&name) {
                continue;
            }
            self.added.remove(&name);
            self.removed.insert(name);
        }

        self.enforce_terminal_cap();
    }

    /// Cheap approximation of the MessagePack-named serialized size in bytes.
    /// Sums per-entry name + value bytes plus a fixed overhead per entry.
    /// Used internally for cap enforcement; not expected to match
    /// `rmp_serde` byte-for-byte.
    #[must_use]
    pub fn serialized_size_hint(&self) -> usize {
        let mut total: usize = 0;
        for (name, value) in &self.added {
            total = total.saturating_add(
                name.len().saturating_add(value.len()).saturating_add(SERIALIZED_ENTRY_OVERHEAD),
            );
        }
        for name in &self.removed {
            total = total.saturating_add(name.len().saturating_add(SERIALIZED_ENTRY_OVERHEAD));
        }
        total
    }

    /// Drop `added` entries in `BTreeMap` iteration order (sorted by name —
    /// deterministic FIFO once the map is fully built) until the serialized
    /// size estimate fits within `PER_TERMINAL_CAP_BYTES`. Emits one warning
    /// log per overflow event. Does not drop from `removed` because removals
    /// are tombstones that must persist to override the baseline.
    fn enforce_terminal_cap(&mut self) {
        let mut size = self.serialized_size_hint();
        if size <= PER_TERMINAL_CAP_BYTES {
            return;
        }

        let original_size = size;
        let mut dropped: usize = 0;
        while size > PER_TERMINAL_CAP_BYTES {
            let Some((name, value)) =
                self.added.keys().next().cloned().and_then(|k| self.added.remove_entry(&k))
            else {
                break;
            };
            let entry_size =
                name.len().saturating_add(value.len()).saturating_add(SERIALIZED_ENTRY_OVERHEAD);
            size = size.saturating_sub(entry_size);
            dropped = dropped.saturating_add(1);
        }

        tracing::warn!(
            target: "scribe_server::env_store",
            original_bytes = original_size,
            new_bytes = size,
            dropped_entries = dropped,
            cap_bytes = PER_TERMINAL_CAP_BYTES,
            "per-terminal env delta exceeded cap; dropped entries FIFO"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_excludes_are_present() {
        // Spot-check a representative entry from each category.
        for name in
            ["SCRIBE_HOOK_SOCK", "TERM", "DISPLAY", "SSH_AUTH_SOCK", "TMUX", "SHLVL", "GPG_TTY"]
        {
            assert!(is_excluded(name), "expected {name} to be in ExclusionSet");
        }
    }

    #[test]
    fn ordinary_user_vars_are_not_excluded() {
        for name in ["PROJECT_ROOT", "API_TOKEN", "MY_VAR", "FOO_BAR_BAZ"] {
            assert!(!is_excluded(name), "did not expect {name} in ExclusionSet");
        }
    }

    #[test]
    fn compute_against_baseline_happy_path() {
        let mut baseline = BTreeMap::new();
        baseline.insert("PATH".to_string(), "/usr/bin".to_string());
        baseline.insert("LANG".to_string(), "C".to_string());
        baseline.insert("OLD".to_string(), "kept".to_string());

        let mut current = BTreeMap::new();
        // Unchanged.
        current.insert("PATH".to_string(), "/usr/bin".to_string());
        // Modified.
        current.insert("LANG".to_string(), "en_US.UTF-8".to_string());
        // Newly added.
        current.insert("PROJECT_ROOT".to_string(), "/home/me/x".to_string());
        // Excluded name on the add side — must be filtered.
        current.insert("SHLVL".to_string(), "2".to_string());
        // "OLD" present in baseline, missing here -> removed.

        let delta = TerminalEnvDelta::compute_against_baseline(&current, &baseline);

        assert_eq!(
            delta.added.get("LANG").map(String::as_str),
            Some("en_US.UTF-8"),
            "modified var should be in added"
        );
        assert_eq!(
            delta.added.get("PROJECT_ROOT").map(String::as_str),
            Some("/home/me/x"),
            "newly added var should be in added"
        );
        assert!(!delta.added.contains_key("PATH"), "unchanged var must not appear in delta.added");
        assert!(
            !delta.added.contains_key("SHLVL"),
            "excluded var must be filtered out of delta.added"
        );
        assert!(
            delta.removed.contains("OLD"),
            "var absent from current should be in delta.removed"
        );
    }
}
