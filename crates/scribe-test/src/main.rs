mod assert;
mod capture;
mod cmd_socket;
mod daemon;
mod input;
mod ipc;
mod lan_peer;
mod remote_peer;
mod render;
mod replay;
mod server;
mod session;
mod share_tap;
mod wait;

use std::fmt;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use scribe_common::ai_state::AiProvider;
use scribe_common::protocol::AiResumeMode;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Exit-code-aware error for the test harness.
///
/// - `TestFailure` -> exit 1 (assertion / check didn't pass)
/// - `InfraError`  -> exit 2 (harness infrastructure problem)
enum TestError {
    /// A test assertion failed.
    TestFailure(String),
    /// An infrastructure error (socket, spawn, timeout, …).
    InfraError(String),
}

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TestFailure(msg) => write!(f, "FAIL: {msg}"),
            Self::InfraError(msg) => write!(f, "ERROR: {msg}"),
        }
    }
}

impl From<TestError> for ExitCode {
    fn from(err: TestError) -> Self {
        let mut stderr = io::stderr().lock();
        match err {
            TestError::TestFailure(ref msg) => {
                drop(writeln!(stderr, "FAIL: {msg}"));
                Self::from(1)
            }
            TestError::InfraError(ref msg) => {
                drop(writeln!(stderr, "ERROR: {msg}"));
                Self::from(2)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// E2E test harness for the Scribe terminal emulator.
#[derive(Parser)]
#[command(name = "scribe-test", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the scribe-server process.
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Manage the test daemon (long-lived helper process).
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Manage terminal sessions.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Send data (keystrokes) to a session.
    Send {
        /// Target session ID.
        session_id: String,
        /// Data to send (interpreted as UTF-8).
        data: String,
    },
    /// Resize a session's terminal.
    Resize {
        /// Target session ID.
        session_id: String,
        /// Number of columns.
        cols: u16,
        /// Number of rows.
        rows: u16,
    },
    /// Capture a PNG screenshot of a session.
    Screenshot {
        /// Target session ID.
        session_id: String,
        /// Output file path.
        path: PathBuf,
    },
    /// Capture a text snapshot of a session's screen contents.
    Snapshot {
        /// Target session ID.
        session_id: String,
        /// Output file path.
        path: PathBuf,
    },
    /// Print the AI chrome (prompt-bar context meter, tab context suffix) a
    /// client would draw for a session.
    AiChrome {
        /// Target session ID.
        session_id: String,
    },
    /// Inspect the `SessionReplay` frames the daemon received and the screen it
    /// rebuilt from them.
    Replay {
        #[command(subcommand)]
        action: ReplayAction,
    },
    /// Wait until output matching a regex pattern appears.
    WaitOutput {
        /// Target session ID.
        session_id: String,
        /// Regex pattern to match against.
        pattern: String,
        /// Timeout in milliseconds.
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    /// Wait until the session's CWD matches the given path.
    WaitCwd {
        /// Target session ID.
        session_id: String,
        /// Expected working directory path.
        path: PathBuf,
        /// Timeout in milliseconds.
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    /// Wait until the session has been idle for a specified duration.
    WaitIdle {
        /// Target session ID.
        session_id: String,
        /// Idle duration in milliseconds.
        #[arg(long, default_value_t = 500)]
        ms: u64,
        /// Timeout in milliseconds.
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    /// Assert that a specific cell contains an expected character.
    AssertCell {
        /// Target session ID.
        session_id: String,
        /// Row (0-indexed).
        row: u16,
        /// Column (0-indexed).
        col: u16,
        /// Expected character / string at that cell.
        expected: String,
    },
    /// Assert that the cursor is at a specific position.
    AssertCursor {
        /// Target session ID.
        session_id: String,
        /// Expected cursor row (0-indexed).
        row: u16,
        /// Expected cursor column (0-indexed).
        col: u16,
    },
    /// Assert that the current screen matches a reference snapshot (JSON file).
    AssertSnapshotMatch {
        /// Target session ID.
        session_id: String,
        /// Path to a reference snapshot JSON file (from `snapshot` command).
        reference: PathBuf,
    },
    /// Assert that no zero-byte `PtyOutput` frame arrived for a session.
    AssertNoEmptyOutput {
        /// Target session ID.
        session_id: String,
    },
    /// Relay the client socket through a wire tap that records every outbound
    /// `ClientMessage` and can inject `ServerMessage`s toward the client.
    ///
    /// Stands in for the second machine a feature-015 share needs, without
    /// faking either end: the client under test still handshakes with the real
    /// server over the real framed protocol.
    ShareTap {
        /// Socket path the client connects to (normally the server socket).
        #[arg(long)]
        listen: PathBuf,
        /// The real server socket the tap relays to.
        #[arg(long)]
        upstream: PathBuf,
        /// JSONL file every outbound `ClientMessage` is appended to.
        #[arg(long)]
        record: PathBuf,
        /// Injection socket served for `share-inject`.
        #[arg(long)]
        control: PathBuf,
    },
    /// Stand in for a second machine's feature-014 LAN listener: terminate a
    /// real mutual-TLS handshake, record the client's `LanHello`, run the
    /// device-approval gate, and splice an approved connection to the local
    /// server.
    LanPeer(LanPeerArgs),
    /// Stand in for a second machine's feature-013 tailnet listener: read the
    /// client's `RemoteHandshake`, record it, answer the mandatory
    /// `RemoteHandshakeReply`, and splice an accepted connection to the local
    /// server.
    RemotePeer(RemotePeerArgs),
    /// Send one JSON-encoded `ServerMessage` to a running `share-tap`, which
    /// frames it to the client as if the server had sent it.
    ShareInject {
        /// Injection socket of the running tap.
        #[arg(long)]
        control: PathBuf,
        /// JSON-encoded `ServerMessage` (serde-tagged by `type`).
        message: String,
    },
    /// Assert that a session exits with a specific exit code.
    AssertExit {
        /// Target session ID.
        session_id: String,
        /// Expected exit code.
        code: i32,
        /// Timeout in milliseconds.
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    /// Assert that a session's child was killed by a specific signal.
    AssertSignal {
        /// Target session ID.
        session_id: String,
        /// Expected terminating signal number.
        signal: i32,
        /// Timeout in milliseconds.
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the scribe-server process.
    Start,
    /// Stop the scribe-server process.
    Stop,
    /// Trigger a hot-reload upgrade (launch new server with --upgrade).
    Upgrade,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the test daemon in the background.
    Start,
    /// Stop a running test daemon.
    Stop,
    /// Print the window ID the server assigned the daemon. Pass it to a client
    /// as `SCRIBE_JOIN_WINDOW` so it joins the daemon's window share.
    WindowId,
    /// Print the most recent automation action, or `none` when empty.
    LastAction,
    /// Reset the recorded automation action to empty.
    ClearAction,
    /// Internal: run the daemon in the foreground (not user-facing).
    Run,
}

#[derive(Subcommand)]
enum ReplayAction {
    /// Print how many replay frames arrived, the running live-output byte
    /// count, and the most recent frame's geometry and position in that stream.
    Status {
        /// Target session ID.
        session_id: String,
        /// Block until at least this many frames have been applied.
        #[arg(long, default_value_t = 0)]
        min_frames: u32,
        /// Fail unless exactly this many frames have been applied. Pass `0` to
        /// assert that a session was never sent a replay.
        #[arg(long)]
        expect_frames: Option<u32>,
        /// Timeout in milliseconds for `--min-frames`.
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    /// Print the replayed screen as text, or write it out as snapshot JSON.
    Screen {
        /// Target session ID.
        session_id: String,
        /// Write the snapshot as JSON to this path instead of printing text.
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Assert that the replayed screen matches the server's own screen.
    AssertMatches {
        /// Target session ID.
        session_id: String,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    /// Create a new terminal session.
    Create {
        /// Columns the PTY is spawned at (needs `--rows`; default 80x24).
        #[arg(long)]
        cols: Option<u16>,
        /// Rows the PTY is spawned at (needs `--cols`; default 80x24).
        #[arg(long)]
        rows: Option<u16>,
        /// AI provider to launch through the server-owned shell path.
        #[arg(long, value_enum)]
        ai_provider: Option<AiProviderArg>,
        /// Start a new AI conversation or resume an existing one.
        #[arg(long, value_enum, requires = "ai_provider")]
        ai_resume_mode: Option<AiResumeModeArg>,
        /// Conversation identifier passed to a resumed AI launch.
        #[arg(long, requires = "ai_provider")]
        ai_conversation_id: Option<String>,
        /// Working directory for the new session.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Existing environment-envelope identifier to restore.
        #[arg(long)]
        env_envelope_id: Option<String>,
    },
    /// Attach to an existing (detached) session.
    Attach {
        /// Session ID to attach to.
        session_id: String,
        /// Columns to attach at (needs `--rows`; default: send no dimensions).
        #[arg(long)]
        cols: Option<u16>,
        /// Rows to attach at (needs `--cols`; default: send no dimensions).
        #[arg(long)]
        rows: Option<u16>,
    },
    /// Close an existing terminal session.
    Close {
        /// Session ID to close.
        session_id: String,
    },
    /// Print the launch (env-envelope) id the harness minted for a session it
    /// created, so a test can assert env persistence has something to key on.
    EnvelopeId {
        /// Session ID to look up.
        session_id: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum AiProviderArg {
    Claude,
    Codex,
}

impl From<AiProviderArg> for AiProvider {
    fn from(provider: AiProviderArg) -> Self {
        match provider {
            AiProviderArg::Claude => Self::ClaudeCode,
            AiProviderArg::Codex => Self::CodexCode,
        }
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum AiResumeModeArg {
    New,
    Resume,
}

impl From<AiResumeModeArg> for AiResumeMode {
    fn from(mode: AiResumeModeArg) -> Self {
        match mode {
            AiResumeModeArg::New => Self::New,
            AiResumeModeArg::Resume => Self::Resume,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => ExitCode::from(e),
    }
}

/// Execute the parsed CLI command.
fn run(cli: Cli) -> Result<(), TestError> {
    match cli.command {
        Command::Server { action } => {
            let rt =
                tokio::runtime::Runtime::new().map_err(|e| TestError::InfraError(e.to_string()))?;
            match action {
                ServerAction::Start => {
                    rt.block_on(server::start()).map_err(|e| TestError::InfraError(e.to_string()))
                }
                ServerAction::Stop => {
                    rt.block_on(server::stop()).map_err(|e| TestError::InfraError(e.to_string()))
                }
                ServerAction::Upgrade => {
                    rt.block_on(server::upgrade()).map_err(|e| TestError::InfraError(e.to_string()))
                }
            }
        }
        Command::Daemon { action } => run_daemon(&action),
        Command::Session { action } => run_session(action),
        Command::Send { session_id, data } => input::send(&session_id, &data),
        Command::Resize { session_id, cols, rows } => input::resize(&session_id, cols, rows),
        Command::Screenshot { session_id, path } => capture::screenshot(&session_id, &path),
        Command::Snapshot { session_id, path } => capture::snapshot(&session_id, &path),
        Command::AiChrome { session_id } => capture::ai_chrome(&session_id),
        Command::Replay { action } => run_replay(action),
        Command::WaitOutput { session_id, pattern, timeout } => {
            wait::wait_output(&session_id, &pattern, timeout)
        }
        Command::WaitCwd { session_id, path, timeout } => {
            let path_str = path.to_string_lossy();
            wait::wait_cwd(&session_id, &path_str, timeout)
        }
        Command::WaitIdle { session_id, ms, timeout } => wait::wait_idle(&session_id, ms, timeout),
        Command::AssertCell { session_id, row, col, expected } => {
            let ch = extract_char(&expected)?;
            assert::assert_cell(&session_id, row, col, ch)
        }
        Command::AssertCursor { session_id, row, col } => {
            assert::assert_cursor(&session_id, row, col)
        }
        Command::AssertSnapshotMatch { session_id, reference } => {
            assert::assert_snapshot_match(&session_id, &reference)
        }
        Command::AssertNoEmptyOutput { session_id } => assert::assert_no_empty_output(&session_id),
        Command::AssertExit { session_id, code, timeout } => {
            assert::assert_exit(&session_id, code, timeout)
        }
        Command::AssertSignal { session_id, signal, timeout } => {
            assert::assert_signal(&session_id, signal, timeout)
        }
        Command::ShareTap { listen, upstream, record, control } => {
            let rt =
                tokio::runtime::Runtime::new().map_err(|e| TestError::InfraError(e.to_string()))?;
            rt.block_on(share_tap::run(&listen, &upstream, &record, &control))
                .map_err(|e| TestError::InfraError(e.to_string()))
        }
        Command::LanPeer(args) => run_lan_peer(args),
        Command::RemotePeer(args) => run_remote_peer(args),
        Command::ShareInject { control, message } => run_share_inject(&control, &message),
    }
}

fn run_daemon(action: &DaemonAction) -> Result<(), TestError> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| TestError::InfraError(e.to_string()))?;
    match action {
        DaemonAction::Start => {
            rt.block_on(daemon::start()).map_err(|e| TestError::InfraError(e.to_string()))
        }
        DaemonAction::Stop => {
            rt.block_on(daemon::stop()).map_err(|e| TestError::InfraError(e.to_string()))
        }
        DaemonAction::WindowId => {
            drop(rt);
            session::print_window_id()
        }
        DaemonAction::LastAction => {
            drop(rt);
            daemon::print_last_action().map_err(|e| TestError::InfraError(e.to_string()))
        }
        DaemonAction::ClearAction => {
            drop(rt);
            daemon::clear_last_action().map_err(|e| TestError::InfraError(e.to_string()))
        }
        DaemonAction::Run => {
            rt.block_on(daemon::run()).map_err(|e| TestError::InfraError(e.to_string()))
        }
    }
}

/// Route a `session` subcommand. Split out of [`run`] so the dispatcher stays a
/// table of one-line routes.
fn run_session(action: SessionAction) -> Result<(), TestError> {
    match action {
        SessionAction::Create {
            cols,
            rows,
            ai_provider,
            ai_resume_mode,
            ai_conversation_id,
            cwd,
            env_envelope_id,
        } => session::create(session::CreateOptions {
            cols,
            rows,
            ai_provider: ai_provider.map(Into::into),
            ai_resume_mode: ai_resume_mode.map(Into::into),
            ai_conversation_id,
            cwd,
            env_envelope_id,
        }),
        SessionAction::Attach { session_id, cols, rows } => {
            session::attach(&session_id, cols, rows)
        }
        SessionAction::Close { session_id } => session::close(&session_id),
        SessionAction::EnvelopeId { session_id } => session::print_envelope_id(&session_id),
    }
}

/// Route a `replay` subcommand. Split out of [`run`] so the dispatcher stays a
/// table of one-line routes.
fn run_replay(action: ReplayAction) -> Result<(), TestError> {
    match action {
        ReplayAction::Status { session_id, min_frames, expect_frames, timeout } => {
            replay::status(&session_id, min_frames, expect_frames, timeout)
        }
        ReplayAction::Screen { session_id, json } => replay::screen(&session_id, json.as_deref()),
        ReplayAction::AssertMatches { session_id } => replay::assert_matches(&session_id),
    }
}

/// Command-line shape of the LAN peer stand-in, grouped so the dispatcher passes
/// one value rather than six positional flags.
#[derive(clap::Args)]
struct LanPeerArgs {
    /// `host:port` to bind the LAN listener on.
    #[arg(long, default_value = "127.0.0.1:46062")]
    listen: String,
    /// Local server socket the device identity is borrowed from and an approved
    /// connection is spliced to.
    #[arg(long)]
    upstream: PathBuf,
    /// JSONL file every framed message in both directions is appended to.
    #[arg(long)]
    record: PathBuf,
    /// Decline instead of approving the device.
    #[arg(long)]
    decline: bool,
    /// Answer `LanApprovalPending` first, as an unknown device would see.
    #[arg(long)]
    pending: bool,
    /// Milliseconds to hold before the terminal approval result.
    #[arg(long, default_value_t = 0)]
    hold_ms: u64,
}

/// Run the feature-014 LAN peer stand-in until it is killed.
///
/// Split out of [`run`] so the dispatcher stays a table of one-line routes; the
/// stand-in owns its own Tokio runtime exactly as `share-tap` does.
fn run_lan_peer(args: LanPeerArgs) -> Result<(), TestError> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| TestError::InfraError(e.to_string()))?;
    let verdict =
        if args.decline { lan_peer::Verdict::Decline } else { lan_peer::Verdict::Approve };
    rt.block_on(lan_peer::run(lan_peer::LanPeerConfig {
        listen: args.listen,
        upstream: args.upstream,
        record: args.record,
        verdict,
        pending: args.pending,
        hold: std::time::Duration::from_millis(args.hold_ms),
    }))
    .map_err(|e| TestError::InfraError(e.to_string()))
}

/// Command-line shape of the tailnet peer stand-in, grouped for the same reason
/// [`LanPeerArgs`] is: the dispatcher passes one value rather than five
/// positional flags.
#[derive(clap::Args)]
struct RemotePeerArgs {
    /// `host:port` to bind the tailnet listener on.
    #[arg(long, default_value = "127.0.0.1:46061")]
    listen: String,
    /// Local server socket an accepted connection is spliced to.
    #[arg(long)]
    upstream: PathBuf,
    /// JSONL file every framed message in both directions is appended to.
    #[arg(long)]
    record: PathBuf,
    /// Refuse the handshake with this typed reason instead of accepting it.
    /// One of `disabled`, `unauthorized`, `identity_unavailable`,
    /// `incompatible_version`, `busy`.
    #[arg(long)]
    refuse: Option<String>,
    /// Milliseconds to hold before the mandatory handshake reply.
    #[arg(long, default_value_t = 0)]
    hold_ms: u64,
}

/// Run the feature-013 tailnet peer stand-in until it is killed.
///
/// Split out of [`run`] so the dispatcher stays a table of one-line routes; the
/// stand-in owns its own Tokio runtime exactly as `lan-peer` does.
fn run_remote_peer(args: RemotePeerArgs) -> Result<(), TestError> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| TestError::InfraError(e.to_string()))?;
    let verdict = match args.refuse.as_deref() {
        None => remote_peer::Verdict::Accept,
        Some(reason) => remote_peer::Verdict::Refuse(parse_refusal(reason)?),
    };
    rt.block_on(remote_peer::run(remote_peer::RemotePeerConfig {
        listen: args.listen,
        upstream: args.upstream,
        record: args.record,
        verdict,
        hold: std::time::Duration::from_millis(args.hold_ms),
    }))
    .map_err(|e| TestError::InfraError(e.to_string()))
}

/// Parse a `--refuse` value into the wire's typed refusal taxonomy. Spelled with
/// the same `snake_case` the protocol serializes, so a script names the reason
/// exactly as it will appear in the wire record.
fn parse_refusal(reason: &str) -> Result<scribe_common::protocol::RemoteRefusal, TestError> {
    use scribe_common::protocol::RemoteRefusal;
    match reason {
        "disabled" => Ok(RemoteRefusal::Disabled),
        "unauthorized" => Ok(RemoteRefusal::Unauthorized),
        "identity_unavailable" => Ok(RemoteRefusal::IdentityUnavailable),
        "incompatible_version" => Ok(RemoteRefusal::IncompatibleVersion),
        "busy" => Ok(RemoteRefusal::Busy),
        other => Err(TestError::InfraError(format!("unknown refusal reason: {other}"))),
    }
}

/// Frame one JSON-encoded `ServerMessage` to a running tap's control socket.
///
/// Split out of [`run`] for the same reason the two peer stand-ins are: the
/// dispatcher stays a table of one-line routes.
fn run_share_inject(control: &std::path::Path, message: &str) -> Result<(), TestError> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| TestError::InfraError(e.to_string()))?;
    rt.block_on(share_tap::inject(control, message))
        .map_err(|e| TestError::InfraError(e.to_string()))
}

/// Extract a single character from the expected string.
fn extract_char(s: &str) -> Result<char, TestError> {
    let mut chars = s.chars();
    let c = chars
        .next()
        .ok_or_else(|| TestError::InfraError("expected character string is empty".to_owned()))?;
    if chars.next().is_some() {
        return Err(TestError::InfraError(format!("expected a single character but got \"{s}\"")));
    }
    Ok(c)
}
