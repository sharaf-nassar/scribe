//! `scribe-hook-helper` — emit one `HookEvent` to `scribe-server` and exit 0.
//!
//! Invoked by AI tool hook adapter scripts in `dist/ai-hook-*.sh`. Reads
//! `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID` from env, builds a `HookEvent`
//! from the CLI args, sends it over the existing length-prefixed msgpack IPC,
//! and exits 0 in **every** path (FR-007, FR-008, FR-009, FR-010, FR-011).
//!
//! See `specs/003-ai-hook-channel/contracts/helper-cli.md` for the full
//! invocation contract and exhaustive failure-mode table.

use std::env;
use std::time::Duration;

use clap::Parser;
use scribe_common::ai_state::{AiProvider, AiState};
use scribe_common::framing::write_message;
use scribe_common::hook::{HookEvent, HookEventKind};
use scribe_common::ids::SessionId;
use scribe_common::protocol::ClientMessage;
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Total wall-clock budget covering connect + write + close. Spec FR-012.
/// Comfortably above warm-cache loopback Unix-socket round-trip (sub-ms) and
/// well below the SC-002 200 ms p95 end-to-end UI budget.
const EMIT_BUDGET: Duration = Duration::from_millis(100);

#[derive(Parser, Debug)]
#[command(name = "scribe-hook-helper", disable_help_flag = true, disable_version_flag = true)]
struct Cli {
    /// AI provider id, one of `claude_code`, `codex_code`, or the
    /// synthetic `system` value used for non-AI events
    /// (`--event=env-delta`). Unknown values cause exit 0 silently per
    /// FR-014. `system` corresponds to [`AiProvider::System`] and is
    /// intentionally absent from the user-visible
    /// `AiProvider::all()` listing.
    #[arg(long)]
    provider: String,

    #[arg(long)]
    event: EventKind,

    #[arg(long)]
    state: Option<String>,

    #[arg(long = "last-message-file")]
    last_message_file: Option<String>,

    #[arg(long = "conversation-id")]
    conversation_id: Option<String>,

    #[arg(long)]
    text: Option<String>,

    #[arg(long)]
    label: Option<String>,

    #[arg(long = "fill-percent")]
    fill_percent: Option<u32>,

    #[arg(long = "added-json")]
    added_json: Option<String>,

    #[arg(long = "removed-json")]
    removed_json: Option<String>,

    #[arg(long = "baseline-ready", default_value_t = false)]
    baseline_ready: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
#[clap(rename_all = "snake_case")]
enum EventKind {
    StateChanged,
    SessionStopped,
    StateCleared,
    PromptReceived,
    TaskLabelChanged,
    TaskLabelCleared,
    ContextChanged,
    /// Env-delta variant emitted by shell integration's pre-exec hooks
    /// (feature 006). Canonical invocation uses `--provider=system` —
    /// see [`AiProvider::System`] for rationale. `--added-json` /
    /// `--removed-json` carry the delta; `--baseline-ready` flips the
    /// server into post-rc baseline-capture mode.
    EnvDelta,
}

fn main() {
    // Swallow any panic message — must not leak to stderr (FR-009).
    // `panic = "abort"` in the workspace release profile prevents unwind,
    // but the hook still controls the printed message on the way down.
    std::panic::set_hook(Box::new(|_| {}));

    // Discard the Result silently — every failure path is FR-007 silent
    // exit-0. Assignment to `_` (Rust 2024) avoids both
    // `let_underscore_must_use` (needs a `let` binding) and
    // `dropping_copy_types` (would fire on `drop()` here because
    // `Result<(), ()>` is `Copy`).
    _ = run();
    // Returning from main yields exit code 0; `std::process::exit` is on
    // the workspace's disallowed-methods list.
}

fn run() -> Result<(), ()> {
    let cli = Cli::try_parse().map_err(|_| ())?;

    let sock_path = env::var("SCRIBE_HOOK_SOCK").map_err(|_| ())?;
    if sock_path.is_empty() {
        return Err(());
    }

    let session_id_str = env::var("SCRIBE_SESSION_ID").map_err(|_| ())?;
    let session_id: SessionId = session_id_str.parse().map_err(|_| ())?;

    let provider = AiProvider::from_id(&cli.provider).ok_or(())?;
    let kind = build_kind(&cli)?;

    let msg = ClientMessage::HookEvent(HookEvent { session_id, provider, kind });

    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_| ())?;

    runtime.block_on(async {
        _ = timeout(EMIT_BUDGET, try_send(&sock_path, &msg)).await;
    });

    Ok(())
}

async fn try_send(sock_path: &str, msg: &ClientMessage) -> Result<(), ()> {
    let mut stream = UnixStream::connect(sock_path).await.map_err(|_| ())?;
    write_message(&mut stream, msg).await.map_err(|_| ())?;
    Ok(())
}

fn build_kind(cli: &Cli) -> Result<HookEventKind, ()> {
    match cli.event {
        EventKind::StateChanged => {
            let state_str = cli.state.as_deref().ok_or(())?;
            let state = parse_ai_state(state_str)?;
            Ok(HookEventKind::StateChanged { state, conversation_id: cli.conversation_id.clone() })
        }
        EventKind::SessionStopped => {
            let path = cli.last_message_file.as_deref().ok_or(())?;
            let last_message = std::fs::read_to_string(path).map_err(|_| ())?;
            // Best-effort cleanup; ignore errors.
            drop(std::fs::remove_file(path));
            Ok(HookEventKind::SessionStopped {
                last_message,
                conversation_id: cli.conversation_id.clone(),
            })
        }
        EventKind::StateCleared => Ok(HookEventKind::StateCleared),
        EventKind::PromptReceived => {
            let text = cli.text.clone().ok_or(())?;
            if text.is_empty() {
                return Err(());
            }
            Ok(HookEventKind::PromptReceived { text, conversation_id: cli.conversation_id.clone() })
        }
        EventKind::TaskLabelChanged => {
            let label = cli.label.clone().ok_or(())?;
            if label.is_empty() {
                return Err(());
            }
            Ok(HookEventKind::TaskLabelChanged { label })
        }
        EventKind::TaskLabelCleared => Ok(HookEventKind::TaskLabelCleared),
        EventKind::ContextChanged => {
            let pct = cli.fill_percent.ok_or(())?;
            let pct: u8 = u8::try_from(pct).unwrap_or(100).min(100);
            Ok(HookEventKind::ContextChanged { fill_percent: pct })
        }
        EventKind::EnvDelta => {
            let added: Vec<(String, String)> = match cli.added_json.as_deref() {
                Some(s) if !s.is_empty() => {
                    let map: std::collections::BTreeMap<String, String> =
                        serde_json::from_str(s).map_err(|_| ())?;
                    map.into_iter().collect()
                }
                _ => Vec::new(),
            };
            let removed: Vec<String> = match cli.removed_json.as_deref() {
                Some(s) if !s.is_empty() => serde_json::from_str(s).map_err(|_| ())?,
                _ => Vec::new(),
            };
            Ok(HookEventKind::EnvChanged { added, removed, baseline_ready: cli.baseline_ready })
        }
    }
}

fn parse_ai_state(s: &str) -> Result<AiState, ()> {
    match s {
        "idle_prompt" => Ok(AiState::IdlePrompt),
        "processing" => Ok(AiState::Processing),
        "waiting_for_input" => Ok(AiState::WaitingForInput),
        "permission_prompt" => Ok(AiState::PermissionPrompt),
        "error" => Ok(AiState::Error),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ai_state_recognizes_canonical_values() {
        assert_eq!(parse_ai_state("idle_prompt"), Ok(AiState::IdlePrompt));
        assert_eq!(parse_ai_state("processing"), Ok(AiState::Processing));
        assert_eq!(parse_ai_state("waiting_for_input"), Ok(AiState::WaitingForInput));
        assert_eq!(parse_ai_state("permission_prompt"), Ok(AiState::PermissionPrompt));
        assert_eq!(parse_ai_state("error"), Ok(AiState::Error));
    }

    #[test]
    fn parse_ai_state_rejects_unknown() {
        assert_eq!(parse_ai_state("inactive"), Err(()));
        assert_eq!(parse_ai_state("IdlePrompt"), Err(()));
        assert_eq!(parse_ai_state(""), Err(()));
    }

    fn make_cli(event: EventKind) -> Cli {
        Cli {
            provider: "claude_code".to_owned(),
            event,
            state: None,
            last_message_file: None,
            conversation_id: None,
            text: None,
            label: None,
            fill_percent: None,
            added_json: None,
            removed_json: None,
            baseline_ready: false,
        }
    }

    #[test]
    fn build_kind_state_changed_requires_state() {
        let cli = make_cli(EventKind::StateChanged);
        assert!(build_kind(&cli).is_err());
    }

    #[test]
    fn build_kind_state_changed_builds_with_state() {
        let mut cli = make_cli(EventKind::StateChanged);
        cli.state = Some("processing".to_owned());
        cli.conversation_id = Some("conv-abc".to_owned());
        let kind = build_kind(&cli).expect("should build");
        match kind {
            HookEventKind::StateChanged { state, conversation_id } => {
                assert_eq!(state, AiState::Processing);
                assert_eq!(conversation_id.as_deref(), Some("conv-abc"));
            }
            _ => panic!("expected StateChanged"),
        }
    }

    #[test]
    fn build_kind_prompt_received_rejects_empty_text() {
        let mut cli = make_cli(EventKind::PromptReceived);
        cli.text = Some(String::new());
        assert!(build_kind(&cli).is_err());
    }

    #[test]
    fn build_kind_task_label_cleared_takes_no_args() {
        let cli = make_cli(EventKind::TaskLabelCleared);
        let kind = build_kind(&cli).expect("should build");
        assert!(matches!(kind, HookEventKind::TaskLabelCleared));
    }

    #[test]
    fn build_kind_context_changed_clamps_above_100() {
        let mut cli = make_cli(EventKind::ContextChanged);
        cli.fill_percent = Some(500);
        let kind = build_kind(&cli).expect("should build");
        match kind {
            HookEventKind::ContextChanged { fill_percent } => assert_eq!(fill_percent, 100),
            _ => panic!("expected ContextChanged"),
        }
    }

    #[test]
    fn build_kind_context_changed_passes_through_valid() {
        let mut cli = make_cli(EventKind::ContextChanged);
        cli.fill_percent = Some(73);
        let kind = build_kind(&cli).expect("should build");
        match kind {
            HookEventKind::ContextChanged { fill_percent } => assert_eq!(fill_percent, 73),
            _ => panic!("expected ContextChanged"),
        }
    }
}
