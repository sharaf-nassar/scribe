mod assert;
mod capture;
mod cmd_socket;
mod daemon;
mod input;
mod ipc;
mod render;
mod server;
mod session;
mod wait;

use std::fmt;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "stderr write failure is non-recoverable in an exit path"
                )]
                let _ = writeln!(stderr, "FAIL: {msg}");
                Self::from(1)
            }
            TestError::InfraError(ref msg) => {
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "stderr write failure is non-recoverable in an exit path"
                )]
                let _ = writeln!(stderr, "ERROR: {msg}");
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
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the scribe-server process.
    Start,
    /// Stop the scribe-server process.
    Stop,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the test daemon in the background.
    Start,
    /// Stop a running test daemon.
    Stop,
    /// Internal: run the daemon in the foreground (not user-facing).
    Run,
}

#[derive(Subcommand)]
enum SessionAction {
    /// Create a new terminal session.
    Create,
    /// Attach to an existing (detached) session.
    Attach {
        /// Session ID to attach to.
        session_id: String,
    },
    /// Close an existing terminal session.
    Close {
        /// Session ID to close.
        session_id: String,
    },
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
            }
        }
        Command::Daemon { action } => {
            let rt =
                tokio::runtime::Runtime::new().map_err(|e| TestError::InfraError(e.to_string()))?;
            match action {
                DaemonAction::Start => {
                    rt.block_on(daemon::start()).map_err(|e| TestError::InfraError(e.to_string()))
                }
                DaemonAction::Stop => {
                    rt.block_on(daemon::stop()).map_err(|e| TestError::InfraError(e.to_string()))
                }
                DaemonAction::Run => {
                    rt.block_on(daemon::run()).map_err(|e| TestError::InfraError(e.to_string()))
                }
            }
        }
        Command::Session { action } => match action {
            SessionAction::Create => session::create(),
            SessionAction::Attach { session_id } => session::attach(&session_id),
            SessionAction::Close { session_id } => session::close(&session_id),
        },
        Command::Send { session_id, data } => input::send(&session_id, &data),
        Command::Resize { session_id, cols, rows } => input::resize(&session_id, cols, rows),
        Command::Screenshot { session_id, path } => capture::screenshot(&session_id, &path),
        Command::Snapshot { session_id, path } => capture::snapshot(&session_id, &path),
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
        Command::AssertExit { session_id, code, timeout } => {
            assert::assert_exit(&session_id, code, timeout)
        }
    }
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
