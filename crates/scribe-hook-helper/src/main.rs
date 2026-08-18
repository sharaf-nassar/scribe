//! `scribe-hook-helper` — emit one `HookEvent` to `scribe-server` and exit 0.
//!
//! Invoked by AI tool hook adapter scripts in `dist/ai-hook-*.sh` and by the
//! shell-integration scripts in `dist/shell-integration/`. Reads
//! `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID` from env, builds a `HookEvent`
//! from the CLI args plus an optional stdin payload document, sends it over
//! the existing length-prefixed msgpack IPC, and exits 0 in **every** path
//! (FR-007, FR-008, FR-009, FR-010, FR-011).
//!
//! Argv carries only fixed, non-secret selectors (`--provider`, `--event`,
//! `--state`, `--fill-percent`, `--conversation-id`, `--baseline-ready`).
//! Every value-bearing field — prompt text, task label, assistant message,
//! env baseline/delta — arrives as a JSON document on stdin under
//! `--payload-stdin`, because `/proc/<pid>/cmdline` is world-readable and a
//! single argument is capped at `MAX_ARG_STRLEN` (128 KiB), which silently
//! turned oversized payloads into `E2BIG` exec failures. The pre-transport
//! argv flags stay accepted for one release: shells started before an
//! upgrade keep the old integration functions in memory and go on calling
//! the old contract until they restart.
//!
//! See `specs/003-ai-hook-channel/contracts/helper-cli.md` for the full
//! invocation contract and exhaustive failure-mode table.

use std::collections::BTreeMap;
use std::env;
use std::io::Read as _;
use std::sync::mpsc;
use std::time::Duration;

use clap::Parser;
use scribe_common::ai_state::{AiProvider, AiState};
use scribe_common::framing::write_message;
use scribe_common::hook::{HookEvent, HookEventKind};
use scribe_common::ids::SessionId;
use scribe_common::protocol::ClientMessage;
use tokio::io::AsyncReadExt as _;
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Total wall-clock budget covering connect + write + server close. Spec FR-012.
/// Comfortably above warm-cache loopback Unix-socket round-trip (sub-ms) and
/// well below the SC-002 200 ms p95 end-to-end UI budget.
const EMIT_BUDGET: Duration = Duration::from_millis(100);

/// Upper bound on a `--payload-stdin` document. An order of magnitude above
/// the server's 512 KiB per-terminal env cap (`env_store::delta`), so nothing
/// the server would have kept is lost here, and far below the 64 MiB IPC
/// frame limit, so a runaway writer cannot balloon helper memory.
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Wall-clock bound on draining the payload document. The writer is always a
/// short-lived `printf`/`python3` on the other end of a pipe, so this only
/// fires when that writer is wedged — and then the caller is a shell prompt
/// hook, which must never block forever. On expiry the event is dropped
/// (silent exit 0) rather than emitted with a truncated payload.
const PAYLOAD_BUDGET: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(name = "scribe-hook-helper", disable_help_flag = true, disable_version_flag = true)]
struct Cli {
    /// AI provider id, one of `claude_code`, `codex_code`, `pi`, or the
    /// synthetic `system` value used for non-AI events
    /// (`--event=env_delta`). Unknown values cause exit 0 silently per
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

    /// Read the event's value-bearing fields as a JSON object from stdin
    /// instead of from argv. See [`Payload`] for the accepted keys. Stdin
    /// is drained before anything else so the writing shell can never block
    /// on a full pipe, and a document that is absent, oversized, non-UTF-8,
    /// or malformed drops the event silently per FR-007.
    #[arg(long = "payload-stdin", default_value_t = false)]
    payload_stdin: bool,
}

/// Value-bearing event fields delivered off argv by `--payload-stdin`.
///
/// Every key is optional and every one that is absent falls back to its
/// `--flag` counterpart, which is what makes one binary serve both the old
/// and the new transport during the dual-accept release. Unknown keys are
/// ignored so a newer script can add a field without a newer helper
/// rejecting the whole document.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct Payload {
    state: Option<String>,
    conversation_id: Option<String>,
    text: Option<String>,
    label: Option<String>,
    fill_percent: Option<u32>,
    last_message: Option<String>,
    added: Option<BTreeMap<String, String>>,
    removed: Option<Vec<String>>,
    baseline_ready: Option<bool>,
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

    // Drain the transport before any other check. The writer is blocked on
    // the pipe until this returns, and bailing out early on, say, a missing
    // `SCRIBE_HOOK_SOCK` would leave it holding a payload larger than the
    // pipe buffer with no reader.
    let payload = read_payload(&cli)?;

    let sock_path = env::var("SCRIBE_HOOK_SOCK").map_err(|_| ())?;
    if sock_path.is_empty() {
        return Err(());
    }

    let session_id_str = env::var("SCRIBE_SESSION_ID").map_err(|_| ())?;
    let session_id: SessionId = session_id_str.parse().map_err(|_| ())?;

    let provider = AiProvider::from_id(&cli.provider).ok_or(())?;
    let kind = build_kind(&cli, &payload)?;

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

    // Keep the connection established until the server consumes the transient
    // frame and closes its side. On macOS, `getpeereid` fails with ENOTCONN if
    // this process exits between the server's `accept` and `peer_cred` calls;
    // the complete frame remains buffered, but the server correctly rejects it
    // because it can no longer verify the sender. Linux's SO_PEERCRED does not
    // expose that race, which made every provider hook look healthy there.
    //
    // HookEvent has no reply by contract, so EOF is the acknowledgement. A
    // server that never closes cannot hold up the AI tool: the caller wraps
    // connect + write + this read in the fixed EMIT_BUDGET timeout.
    let mut unexpected_reply = [0_u8; 1];
    match stream.read(&mut unexpected_reply).await {
        Ok(0) => Ok(()),
        Ok(_) | Err(_) => Err(()),
    }
}

/// Read and parse the `--payload-stdin` document, or return an empty payload
/// when the caller used the argv contract.
///
/// The read runs on a detached thread so a wedged writer costs
/// [`PAYLOAD_BUDGET`] rather than hanging the shell prompt that spawned us
/// forever; returning from `main` tears the thread down with the process.
fn read_payload(cli: &Cli) -> Result<Payload, ()> {
    if !cli.payload_stdin {
        return Ok(Payload::default());
    }

    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("payload-read".to_owned())
        .spawn(move || {
            let mut buf = Vec::new();
            let capped = u64::try_from(MAX_PAYLOAD_BYTES).unwrap_or(u64::MAX).saturating_add(1);
            let outcome = std::io::stdin().lock().take(capped).read_to_end(&mut buf);
            _ = tx.send(outcome.map(|_| buf));
        })
        .map_err(|_| ())?;

    let raw = rx.recv_timeout(PAYLOAD_BUDGET).map_err(|_| ())?.map_err(|_| ())?;
    if raw.len() > MAX_PAYLOAD_BYTES {
        return Err(());
    }
    let text = String::from_utf8(raw).map_err(|_| ())?;
    serde_json::from_str(&text).map_err(|_| ())
}

/// Build the event, preferring the stdin payload and falling back to the
/// argv flag of the same name. Every field resolves the same way, so a
/// pre-upgrade caller that supplies nothing on stdin observes exactly the
/// behaviour it did before the transport change.
fn build_kind(cli: &Cli, payload: &Payload) -> Result<HookEventKind, ()> {
    let conversation_id = payload.conversation_id.clone().or_else(|| cli.conversation_id.clone());

    match cli.event {
        EventKind::StateChanged => {
            let state_str = payload.state.as_deref().or(cli.state.as_deref()).ok_or(())?;
            let state = parse_ai_state(state_str)?;
            Ok(HookEventKind::StateChanged { state, conversation_id })
        }
        EventKind::SessionStopped => {
            // `--last-message-file` was the pre-transport escape hatch for
            // multi-KiB assistant messages; it stays accepted, but the
            // shipped adapters now stream the text so it never lands on
            // disk at all.
            let last_message = if let Some(text) = payload.last_message.clone() {
                text
            } else {
                let path = cli.last_message_file.as_deref().ok_or(())?;
                let text = std::fs::read_to_string(path).map_err(|_| ())?;
                // Best-effort cleanup; ignore errors.
                drop(std::fs::remove_file(path));
                text
            };
            Ok(HookEventKind::SessionStopped { last_message, conversation_id })
        }
        EventKind::StateCleared => Ok(HookEventKind::StateCleared),
        EventKind::PromptReceived => {
            let text = payload.text.clone().or_else(|| cli.text.clone()).ok_or(())?;
            if text.is_empty() {
                return Err(());
            }
            Ok(HookEventKind::PromptReceived { text, conversation_id })
        }
        EventKind::TaskLabelChanged => {
            let label = payload.label.clone().or_else(|| cli.label.clone()).ok_or(())?;
            if label.is_empty() {
                return Err(());
            }
            Ok(HookEventKind::TaskLabelChanged { label })
        }
        EventKind::TaskLabelCleared => Ok(HookEventKind::TaskLabelCleared),
        EventKind::ContextChanged => {
            let pct = payload.fill_percent.or(cli.fill_percent).ok_or(())?;
            let pct: u8 = u8::try_from(pct).unwrap_or(100).min(100);
            Ok(HookEventKind::ContextChanged { fill_percent: pct })
        }
        EventKind::EnvDelta => {
            let added: Vec<(String, String)> = match payload.added.clone() {
                Some(map) => map.into_iter().collect(),
                None => match cli.added_json.as_deref() {
                    Some(s) if !s.is_empty() => {
                        let map: BTreeMap<String, String> =
                            serde_json::from_str(s).map_err(|_| ())?;
                        map.into_iter().collect()
                    }
                    _ => Vec::new(),
                },
            };
            let removed: Vec<String> = match payload.removed.clone() {
                Some(names) => names,
                None => match cli.removed_json.as_deref() {
                    Some(s) if !s.is_empty() => serde_json::from_str(s).map_err(|_| ())?,
                    _ => Vec::new(),
                },
            };
            let baseline_ready = payload.baseline_ready.unwrap_or(cli.baseline_ready);
            Ok(HookEventKind::EnvChanged { added, removed, baseline_ready })
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
    use scribe_common::framing::read_message;
    use tokio::net::UnixListener;

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

    // @lat: [[test#Test Harness#AI Hook Helper#Pi uses the generic hook schema]]
    #[test]
    fn pi_provider_uses_the_generic_hook_schema() {
        let cli = Cli::try_parse_from([
            "scribe-hook-helper",
            "--provider=pi",
            "--event=state_changed",
            "--state=processing",
        ])
        .expect("Pi provider arguments should parse");
        assert_eq!(AiProvider::from_id(&cli.provider), Some(AiProvider::Pi));
        assert!(matches!(
            build_kind(&cli, &Payload::default()),
            Ok(HookEventKind::StateChanged { state: AiState::Processing, conversation_id: None })
        ));
    }

    // @lat: [[test#Test Harness#AI Hook Helper#Sender lifetime protects macOS peer credentials]]
    #[tokio::test]
    async fn sender_waits_for_server_close_after_writing_frame() {
        let session_id = SessionId::new();
        let socket_path = std::env::temp_dir().join(format!(
            "sh-{}-{}.sock",
            std::process::id(),
            &session_id.to_full_string()[..8]
        ));
        let listener = UnixListener::bind(&socket_path).expect("test listener should bind");
        let message = ClientMessage::HookEvent(HookEvent {
            session_id,
            provider: AiProvider::ClaudeCode,
            kind: HookEventKind::StateChanged { state: AiState::Processing, conversation_id: None },
        });

        let sender_path = socket_path.to_string_lossy().into_owned();
        let sender = tokio::spawn(async move { try_send(&sender_path, &message).await });
        let (mut server_stream, _) = listener.accept().await.expect("sender should connect");

        let received: ClientMessage =
            read_message(&mut server_stream).await.expect("sender should write one complete frame");
        assert!(matches!(received, ClientMessage::HookEvent(_)));
        tokio::task::yield_now().await;
        assert!(
            !sender.is_finished(),
            "sender must remain connected while the server verifies and consumes the frame"
        );
        server_stream
            .peer_cred()
            .expect("peer credentials must remain queryable after the complete frame arrives");

        drop(server_stream);
        let result = tokio::time::timeout(Duration::from_secs(1), sender)
            .await
            .expect("sender should observe server close")
            .expect("sender task should not panic");
        assert_eq!(result, Ok(()));

        drop(listener);
        std::fs::remove_file(&socket_path).expect("test socket should be removable");
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
            payload_stdin: false,
        }
    }

    fn parse_payload(json: &str) -> Payload {
        serde_json::from_str(json).expect("payload should parse")
    }

    #[test]
    fn build_kind_state_changed_requires_state() {
        let cli = make_cli(EventKind::StateChanged);
        assert!(build_kind(&cli, &Payload::default()).is_err());
    }

    #[test]
    fn build_kind_state_changed_builds_with_state() {
        let mut cli = make_cli(EventKind::StateChanged);
        cli.state = Some("processing".to_owned());
        cli.conversation_id = Some("conv-abc".to_owned());
        let kind = build_kind(&cli, &Payload::default()).expect("should build");
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
        assert!(build_kind(&cli, &Payload::default()).is_err());
    }

    #[test]
    fn build_kind_task_label_cleared_takes_no_args() {
        let cli = make_cli(EventKind::TaskLabelCleared);
        let kind = build_kind(&cli, &Payload::default()).expect("should build");
        assert!(matches!(kind, HookEventKind::TaskLabelCleared));
    }

    #[test]
    fn build_kind_context_changed_clamps_above_100() {
        let mut cli = make_cli(EventKind::ContextChanged);
        cli.fill_percent = Some(500);
        let kind = build_kind(&cli, &Payload::default()).expect("should build");
        match kind {
            HookEventKind::ContextChanged { fill_percent } => assert_eq!(fill_percent, 100),
            _ => panic!("expected ContextChanged"),
        }
    }

    #[test]
    fn build_kind_context_changed_passes_through_valid() {
        let mut cli = make_cli(EventKind::ContextChanged);
        cli.fill_percent = Some(73);
        let kind = build_kind(&cli, &Payload::default()).expect("should build");
        match kind {
            HookEventKind::ContextChanged { fill_percent } => assert_eq!(fill_percent, 73),
            _ => panic!("expected ContextChanged"),
        }
    }

    /// The whole point of the transport change: the shipped shell scripts
    /// now pass nothing but selectors on argv, and the env payload — which
    /// carries every exported variable, secrets included — arrives on
    /// stdin.
    #[test]
    fn build_kind_env_delta_reads_payload_document() {
        let cli = make_cli(EventKind::EnvDelta);
        let payload = parse_payload(
            r#"{"added":{"TOKEN":"s3cret","B":""},"removed":["OLD"],"baseline_ready":true}"#,
        );
        let kind = build_kind(&cli, &payload).expect("should build");
        match kind {
            HookEventKind::EnvChanged { added, removed, baseline_ready } => {
                assert_eq!(
                    added,
                    vec![
                        ("B".to_owned(), String::new()),
                        ("TOKEN".to_owned(), "s3cret".to_owned())
                    ]
                );
                assert_eq!(removed, vec!["OLD".to_owned()]);
                assert!(baseline_ready);
            }
            _ => panic!("expected EnvChanged"),
        }
    }

    /// Dual-accept: a shell that was already running when the package was
    /// upgraded still holds the old integration functions in memory and
    /// keeps calling the argv contract until it restarts.
    #[test]
    fn build_kind_env_delta_still_accepts_argv_contract() {
        let mut cli = make_cli(EventKind::EnvDelta);
        cli.added_json = Some(r#"{"A":"1"}"#.to_owned());
        cli.removed_json = Some(r#"["B"]"#.to_owned());
        cli.baseline_ready = true;
        let kind = build_kind(&cli, &Payload::default()).expect("should build");
        match kind {
            HookEventKind::EnvChanged { added, removed, baseline_ready } => {
                assert_eq!(added, vec![("A".to_owned(), "1".to_owned())]);
                assert_eq!(removed, vec!["B".to_owned()]);
                assert!(baseline_ready);
            }
            _ => panic!("expected EnvChanged"),
        }
    }

    /// An empty `added` object is a real delta (every variable was unset),
    /// not "fall back to argv" — otherwise a stale argv value would win.
    #[test]
    fn build_kind_env_delta_payload_beats_argv() {
        let mut cli = make_cli(EventKind::EnvDelta);
        cli.added_json = Some(r#"{"STALE":"1"}"#.to_owned());
        let payload = parse_payload(r#"{"added":{},"removed":[]}"#);
        let kind = build_kind(&cli, &payload).expect("should build");
        match kind {
            HookEventKind::EnvChanged { added, removed, .. } => {
                assert!(added.is_empty(), "payload's empty object must win, got {added:?}");
                assert!(removed.is_empty());
            }
            _ => panic!("expected EnvChanged"),
        }
    }

    /// Prompt text, task labels, and assistant messages are the argv
    /// exposures #42/#44 named; all three now resolve from the document.
    #[test]
    fn build_kind_reads_text_label_and_last_message_from_payload() {
        let cli = make_cli(EventKind::PromptReceived);
        let payload = parse_payload(r#"{"text":"deploy with $API_KEY","conversation_id":"c-1"}"#);
        match build_kind(&cli, &payload).expect("should build") {
            HookEventKind::PromptReceived { text, conversation_id } => {
                assert_eq!(text, "deploy with $API_KEY");
                assert_eq!(conversation_id.as_deref(), Some("c-1"));
            }
            _ => panic!("expected PromptReceived"),
        }

        let label_cli = make_cli(EventKind::TaskLabelChanged);
        let label_payload = parse_payload(r#"{"label":"ship it"}"#);
        match build_kind(&label_cli, &label_payload).expect("should build") {
            HookEventKind::TaskLabelChanged { label } => assert_eq!(label, "ship it"),
            _ => panic!("expected TaskLabelChanged"),
        }

        // No `--last-message-file`, so this only builds if the document is
        // consulted — and it must not touch the filesystem to do it.
        let stop_cli = make_cli(EventKind::SessionStopped);
        let stop_payload = parse_payload(r#"{"last_message":"done"}"#);
        match build_kind(&stop_cli, &stop_payload).expect("should build") {
            HookEventKind::SessionStopped { last_message, .. } => {
                assert_eq!(last_message, "done");
            }
            _ => panic!("expected SessionStopped"),
        }
    }

    /// Forward compatibility: a newer script may add keys this build has
    /// never heard of, and dropping the whole event over one of them would
    /// be the same silent loss the transport change exists to remove.
    #[test]
    fn payload_ignores_unknown_keys() {
        let payload = parse_payload(r#"{"text":"hi","future_field":{"nested":1}}"#);
        assert_eq!(payload.text.as_deref(), Some("hi"));
    }

    /// Every literal `--flag=value` pair in the shipped scripts under
    /// `dist/`, keyed by flag, with the file that carries it.
    fn shipped_flag_values(flag: &str) -> Vec<(String, String)> {
        let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist");
        let mut files = Vec::new();
        collect_files(&dist, &mut files);
        assert!(!files.is_empty(), "no files found under {}", dist.display());

        let needle = format!("--{flag}=");
        let mut found = Vec::new();
        for path in files {
            let bytes = std::fs::read(&path).expect("read shipped script");
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let name = path.display().to_string();
            found.extend(literal_values(&text, &needle).map(|value| (name.clone(), value)));
        }
        found
    }

    /// Values immediately following `needle` in `text`, up to the first
    /// separator. Interpolated values are skipped — only literals pin the
    /// contract.
    fn literal_values<'a>(text: &'a str, needle: &'a str) -> impl Iterator<Item = String> + 'a {
        text.match_indices(needle).filter_map(|(idx, _)| {
            let value: String = text[idx + needle.len()..]
                .chars()
                .take_while(|c| !c.is_whitespace() && !matches!(c, '\'' | '"' | '\\' | '`'))
                .collect();
            (!value.is_empty() && !value.contains('$')).then_some(value)
        })
    }

    fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir).expect("read dist dir");
        for entry in entries {
            let path = entry.expect("dist dir entry").path();
            if path.is_dir() {
                collect_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    /// Pins the helper CLI contract against what the shipped scripts pass:
    /// a script spelling that `Cli::try_parse` rejects is dropped silently
    /// (FR-007), so the event would never reach the server.
    #[test]
    fn shipped_scripts_use_parseable_event_tokens() {
        use clap::ValueEnum;

        let events = shipped_flag_values("event");
        assert!(events.len() >= 10, "expected the shipped --event literals, got {events:?}");
        for (file, token) in &events {
            assert!(
                EventKind::from_str(token, false).is_ok(),
                "{file} passes --event={token}, which the helper rejects"
            );
        }
        assert!(
            events.iter().any(|(_, token)| token == "env_delta"),
            "shell integration's env-delta emit is missing from dist/"
        );
    }

    #[test]
    fn shipped_scripts_use_known_provider_ids() {
        let providers = shipped_flag_values("provider");
        assert!(!providers.is_empty(), "expected --provider literals under dist/");
        for (file, id) in &providers {
            assert!(
                AiProvider::from_id(id).is_some(),
                "{file} passes --provider={id}, which the helper rejects"
            );
        }
        assert!(
            providers.iter().any(|(_, id)| id == "system"),
            "shell integration's system-provider emit is missing from dist/"
        );
    }
}
