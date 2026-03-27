//! Driver task registry: manages Claude Code task lifecycles.
//!
//! Each task is a PTY session running `claude` CLI in a dedicated git
//! worktree. The registry creates worktrees, spawns processes, reads output,
//! and forwards AI state changes to attached clients.

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;
use vte::Parser as VteParser;

use scribe_common::ai_state::AiProcessState;
use scribe_common::driver::{DriverTaskInfo, DriverTaskState};
use scribe_common::framing::write_message;
use scribe_common::ids::SessionId;
use scribe_common::protocol::ServerMessage;
use scribe_pty::async_fd::AsyncPtyFd;
use scribe_pty::metadata::MetadataParser;
use scribe_pty::osc_interceptor::OscInterceptor;

/// Buffer size for driver task PTY reads.
const DRIVER_PTY_BUF_SIZE: usize = 64 * 1024;

/// Maximum number of simultaneous driver tasks.
const MAX_DRIVER_TASKS: usize = 32;

/// Shared write half of a client connection — same type as `ipc_server.rs`.
pub type SharedWriter = Arc<Mutex<tokio::io::WriteHalf<tokio::net::UnixStream>>>;

/// Shared list of client writers currently receiving output for a task.
type AttachedWriters = Arc<Mutex<Vec<SharedWriter>>>;

/// Arc-wrapped registry handle used by spawned reader tasks.
pub type DriverTaskRegistryHandle = Arc<Mutex<DriverTaskRegistry>>;

/// A live driver task managed by the registry.
struct DriverTask {
    task_id: Uuid,
    project_path: PathBuf,
    description: String,
    worktree_path: PathBuf,
    branch_name: String,
    state: DriverTaskState,
    created_at: i64,
    /// Raw fd of the PTY master — kept for handoff serialisation.
    pty_master_raw_fd: RawFd,
    /// Write half for sending input to the task's PTY.
    pty_write: Arc<Mutex<tokio::io::WriteHalf<AsyncPtyFd>>>,
    child_pid: Pid,
    /// Clients currently subscribed to this task's output.
    attached_writers: AttachedWriters,
    /// Most-recent AI process state observed from OSC 1337.
    ai_state: Option<AiProcessState>,
}

/// Registry of all active driver tasks.
pub struct DriverTaskRegistry {
    tasks: HashMap<Uuid, DriverTask>,
}

impl DriverTaskRegistry {
    /// Create a new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { tasks: HashMap::new() }
    }

    /// Create a new driver task: open a git worktree, spawn `claude`, and start
    /// the PTY read loop. Returns the created task's info on success.
    ///
    /// # Errors
    ///
    /// Returns a string error if the task limit is hit, the worktree cannot be
    /// created, or the `claude` process fails to spawn.
    pub async fn create_task(
        registry: &DriverTaskRegistryHandle,
        task_id: Uuid,
        project_path: PathBuf,
        description: String,
    ) -> Result<DriverTaskInfo, String> {
        {
            let reg = registry.lock().await;
            if reg.tasks.len() >= MAX_DRIVER_TASKS {
                return Err(format!("driver task limit ({MAX_DRIVER_TASKS}) reached"));
            }
        }

        let short_id = &task_id.to_string()[..8];
        let branch_name = format!("driver/{short_id}");
        let worktree_name = format!(
            "{}-driver-{short_id}",
            project_path.file_name().and_then(|n| n.to_str()).unwrap_or("project")
        );
        let parent = project_path.parent().ok_or_else(|| {
            format!("project path {} has no parent directory", project_path.display())
        })?;
        let worktree_path = parent.join(&worktree_name);

        // Create git worktree.
        create_git_worktree(&project_path, &worktree_path, &branch_name)?;

        // Open a PTY pair.
        let pty_pair = nix::pty::openpty(None, None).map_err(|e| format!("openpty failed: {e}"))?;
        let master_fd: OwnedFd = pty_pair.master;
        let slave_fd: OwnedFd = pty_pair.slave;

        // Spawn the `claude` process with the slave PTY as its stdio.
        let child_pid_raw = spawn_claude(&worktree_path, &description, slave_fd)?;
        let child_pid =
            Pid::from_raw(i32::try_from(child_pid_raw).map_err(|e| format!("PID overflow: {e}"))?);

        let raw_fd = master_fd.as_raw_fd();
        let pty_fd =
            AsyncPtyFd::new(master_fd).map_err(|e| format!("AsyncPtyFd::new failed: {e}"))?;

        let (pty_read, pty_write) = tokio::io::split(pty_fd);
        let pty_write = Arc::new(Mutex::new(pty_write));
        let attached_writers: AttachedWriters = Arc::new(Mutex::new(Vec::new()));

        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        #[allow(clippy::cast_possible_wrap, reason = "Unix timestamps fit in i64 until year 2554")]
        let created_at_i64 = created_at as i64;

        let info = DriverTaskInfo {
            task_id,
            project_path: project_path.clone(),
            description: description.clone(),
            state: DriverTaskState::Starting,
            worktree_path: Some(worktree_path.clone()),
            created_at: created_at_i64,
        };

        let fake_sid = SessionId::new();
        let reader_state = DriverPtyReaderState {
            task_id,
            pty_read,
            osc_parser: VteParser::new(),
            metadata_parser: MetadataParser::new(fake_sid),
            osc_events: Vec::new(),
            attached_writers: Arc::clone(&attached_writers),
            registry: Arc::clone(registry),
        };

        {
            let mut reg = registry.lock().await;
            reg.tasks.insert(
                task_id,
                DriverTask {
                    task_id,
                    project_path,
                    description,
                    worktree_path,
                    branch_name,
                    state: DriverTaskState::Starting,
                    created_at: created_at_i64,
                    pty_master_raw_fd: raw_fd,
                    pty_write,
                    child_pid,
                    attached_writers,
                    ai_state: None,
                },
            );
        }

        tokio::spawn(driver_pty_reader_task(reader_state));

        info!(%task_id, "driver task created");
        Ok(info)
    }

    /// Stop a running driver task: send SIGTERM, clean up worktree + branch.
    pub fn stop_task(&mut self, task_id: Uuid) {
        let Some(task) = self.tasks.remove(&task_id) else {
            warn!(%task_id, "stop_task: task not found");
            return;
        };

        if let Err(e) = signal::kill(task.child_pid, Signal::SIGTERM) {
            warn!(%task_id, "SIGTERM failed: {e}");
        }

        let worktree_path = task.worktree_path.clone();
        let branch = task.branch_name.clone();
        let project_path = task.project_path.clone();
        tokio::spawn(async move {
            cleanup_worktree(&project_path, &worktree_path, &branch);
        });

        info!(%task_id, "driver task stopped");
    }

    /// Write input bytes to a task's PTY master.
    pub async fn send_input(&self, task_id: Uuid, data: &[u8]) {
        let Some(task) = self.tasks.get(&task_id) else {
            warn!(%task_id, "send_input: task not found");
            return;
        };

        let mut pty_write = task.pty_write.lock().await;
        if let Err(e) = pty_write.write_all(data).await {
            warn!(%task_id, "PTY write failed: {e}");
        }
    }

    /// Return info for all live tasks.
    #[must_use]
    pub fn list_tasks(&self) -> Vec<DriverTaskInfo> {
        self.tasks
            .values()
            .map(|t| DriverTaskInfo {
                task_id: t.task_id,
                project_path: t.project_path.clone(),
                description: t.description.clone(),
                state: t.state.clone(),
                worktree_path: Some(t.worktree_path.clone()),
                created_at: t.created_at,
            })
            .collect()
    }

    /// Attach a client writer to receive output from the given task.
    ///
    /// Returns `Err` if the task is not found (e.g. already exited).
    pub async fn attach_task(&self, task_id: Uuid, writer: SharedWriter) -> Result<(), String> {
        let Some(task) = self.tasks.get(&task_id) else {
            warn!(%task_id, "attach_task: task not found");
            return Err(format!("driver task {task_id} not found"));
        };
        task.attached_writers.lock().await.push(writer);
        info!(%task_id, "client attached to driver task");
        Ok(())
    }

    /// Collect PTY master fds and task metadata for a hot-reload handoff.
    pub fn serialize_for_handoff(&self) -> (Vec<HandoffDriverTask>, Vec<RawFd>) {
        let mut tasks = Vec::with_capacity(self.tasks.len());
        let mut fds = Vec::with_capacity(self.tasks.len());

        for task in self.tasks.values() {
            tasks.push(HandoffDriverTask {
                task_id: task.task_id,
                project_path: task.project_path.clone(),
                description: task.description.clone(),
                worktree_path: task.worktree_path.clone(),
                branch_name: task.branch_name.clone(),
                state: task.state.clone(),
                created_at: task.created_at,
                child_pid: task.child_pid.as_raw(),
            });
            fds.push(task.pty_master_raw_fd);
        }

        (tasks, fds)
    }

    /// Reconstruct tasks from handoff state and restart their PTY read loops.
    pub async fn restore_from_handoff(
        registry: &DriverTaskRegistryHandle,
        handoff_tasks: Vec<HandoffDriverTask>,
        fds: Vec<OwnedFd>,
    ) -> Result<(), String> {
        for (handoff, owned_fd) in handoff_tasks.into_iter().zip(fds) {
            let task_id = handoff.task_id;
            let raw_fd = owned_fd.as_raw_fd();
            let pty_fd = AsyncPtyFd::new(owned_fd)
                .map_err(|e| format!("AsyncPtyFd::new failed for {task_id}: {e}"))?;

            let (pty_read, pty_write) = tokio::io::split(pty_fd);
            let pty_write = Arc::new(Mutex::new(pty_write));
            let attached_writers: AttachedWriters = Arc::new(Mutex::new(Vec::new()));
            let child_pid = Pid::from_raw(handoff.child_pid);

            let reader_state = DriverPtyReaderState {
                task_id,
                pty_read,
                osc_parser: VteParser::new(),
                metadata_parser: MetadataParser::new(SessionId::new()),
                osc_events: Vec::new(),
                attached_writers: Arc::clone(&attached_writers),
                registry: Arc::clone(registry),
            };

            registry.lock().await.tasks.insert(
                task_id,
                DriverTask {
                    task_id,
                    project_path: handoff.project_path,
                    description: handoff.description,
                    worktree_path: handoff.worktree_path,
                    branch_name: handoff.branch_name,
                    state: handoff.state,
                    created_at: handoff.created_at,
                    pty_master_raw_fd: raw_fd,
                    pty_write,
                    child_pid,
                    attached_writers,
                    ai_state: None,
                },
            );

            tokio::spawn(driver_pty_reader_task(reader_state));
            info!(%task_id, "driver task restored from handoff");
        }
        Ok(())
    }
}

/// Per-task state serialised during a hot-reload handoff.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct HandoffDriverTask {
    pub task_id: Uuid,
    pub project_path: PathBuf,
    pub description: String,
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub state: DriverTaskState,
    pub created_at: i64,
    pub child_pid: i32,
}

// ── PTY reader task ──────────────────────────────────────────────────

/// State for a driver task's PTY reader task.
struct DriverPtyReaderState {
    task_id: Uuid,
    pty_read: tokio::io::ReadHalf<AsyncPtyFd>,
    osc_parser: VteParser,
    metadata_parser: MetadataParser,
    osc_events: Vec<scribe_pty::metadata::MetadataEvent>,
    attached_writers: AttachedWriters,
    registry: DriverTaskRegistryHandle,
}

/// Read PTY output from a driver task, forward to attached clients, and
/// detect AI state changes via OSC 1337.
async fn driver_pty_reader_task(mut state: DriverPtyReaderState) {
    use tokio::io::AsyncReadExt as _;

    let mut buf = vec![0u8; DRIVER_PTY_BUF_SIZE];

    loop {
        let n = match state.pty_read.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                warn!(task_id = %state.task_id, "PTY read error: {e}");
                break;
            }
        };

        let Some(bytes) = buf.get(..n) else { break };

        broadcast_driver_output(&state.attached_writers, state.task_id, bytes).await;

        run_driver_osc_interceptor(
            &mut state.osc_parser,
            &state.metadata_parser,
            bytes,
            &mut state.osc_events,
        );

        let events: Vec<_> = std::mem::take(&mut state.osc_events);
        for event in events {
            handle_driver_metadata_event(
                event,
                state.task_id,
                &state.attached_writers,
                &state.registry,
            )
            .await;
        }
    }

    let exit_msg = ServerMessage::DriverTaskExited { task_id: state.task_id, exit_code: None };
    broadcast_server_msg(&state.attached_writers, &exit_msg).await;

    state.registry.lock().await.tasks.remove(&state.task_id);
    info!(task_id = %state.task_id, "driver PTY reader task exited");
}

/// Send raw driver output to all attached writers.
async fn broadcast_driver_output(writers: &AttachedWriters, task_id: Uuid, bytes: &[u8]) {
    let msg = ServerMessage::DriverTaskOutput { task_id, data: bytes.to_vec() };
    broadcast_server_msg(writers, &msg).await;
}

/// Broadcast a `ServerMessage` to all attached writers, pruning dead ones.
async fn broadcast_server_msg(writers: &AttachedWriters, msg: &ServerMessage) {
    let mut guard = writers.lock().await;
    let mut dead = Vec::new();
    for (i, writer) in guard.iter().enumerate() {
        let mut w = writer.lock().await;
        if write_message(&mut *w, msg).await.is_err() {
            dead.push(i);
        }
    }
    for i in dead.into_iter().rev() {
        guard.remove(i);
    }
}

/// Run OSC interception on a byte slice.
fn run_driver_osc_interceptor(
    osc_parser: &mut VteParser,
    metadata_parser: &MetadataParser,
    bytes: &[u8],
    out: &mut Vec<scribe_pty::metadata::MetadataEvent>,
) {
    let mut interceptor = OscInterceptor::new(metadata_parser, out);
    osc_parser.advance(&mut interceptor, bytes);
}

/// Handle a metadata event from the driver task PTY.
async fn handle_driver_metadata_event(
    event: scribe_pty::metadata::MetadataEvent,
    task_id: Uuid,
    writers: &AttachedWriters,
    registry: &DriverTaskRegistryHandle,
) {
    use scribe_pty::metadata::MetadataEvent;

    match event {
        MetadataEvent::AiStateChanged(ai_state) => {
            let driver_state = ai_state_to_driver_state(&ai_state);
            {
                let mut reg = registry.lock().await;
                if let Some(task) = reg.tasks.get_mut(&task_id) {
                    task.ai_state = Some(ai_state.clone());
                    task.state = driver_state.clone();
                }
            }
            let msg = ServerMessage::DriverTaskStateChanged {
                task_id,
                state: driver_state,
                ai_state: Some(ai_state),
            };
            broadcast_server_msg(writers, &msg).await;
        }
        MetadataEvent::AiStateCleared => {
            {
                let mut reg = registry.lock().await;
                if let Some(task) = reg.tasks.get_mut(&task_id) {
                    task.ai_state = None;
                    task.state = DriverTaskState::Running;
                }
            }
            let msg = ServerMessage::DriverTaskStateChanged {
                task_id,
                state: DriverTaskState::Running,
                ai_state: None,
            };
            broadcast_server_msg(writers, &msg).await;
        }
        _ => {}
    }
}

/// Map an `AiProcessState` to the corresponding `DriverTaskState`.
fn ai_state_to_driver_state(ai_state: &AiProcessState) -> DriverTaskState {
    use scribe_common::ai_state::AiState;

    match ai_state.state {
        AiState::IdlePrompt | AiState::Processing => DriverTaskState::Running,
        AiState::WaitingForInput => DriverTaskState::WaitingForInput,
        AiState::PermissionPrompt => DriverTaskState::PermissionPrompt,
        AiState::Error => DriverTaskState::Failed,
    }
}

// ── Git worktree helpers ─────────────────────────────────────────────

fn create_git_worktree(
    project_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
) -> Result<(), String> {
    let status = std::process::Command::new("git")
        .args(["worktree", "add", &worktree_path.to_string_lossy(), "-b", branch_name])
        .current_dir(project_path)
        .status()
        .map_err(|e| format!("git worktree add failed: {e}"))?;

    if !status.success() {
        return Err(format!("git worktree add exited with {status} for branch {branch_name}"));
    }
    Ok(())
}

fn cleanup_worktree(project_path: &Path, worktree_path: &Path, branch_name: &str) {
    let rm = std::process::Command::new("git")
        .args(["worktree", "remove", "--force", &worktree_path.to_string_lossy()])
        .current_dir(project_path)
        .status();

    match rm {
        Ok(s) if s.success() => {}
        Ok(s) => warn!(?worktree_path, "git worktree remove exited with {s}"),
        Err(e) => warn!(?worktree_path, "git worktree remove failed: {e}"),
    }

    let bd = std::process::Command::new("git")
        .args(["branch", "-D", branch_name])
        .current_dir(project_path)
        .status();

    match bd {
        Ok(s) if s.success() => {}
        Ok(s) => warn!(branch = branch_name, "git branch -D exited with {s}"),
        Err(e) => warn!(branch = branch_name, "git branch -D failed: {e}"),
    }
}

// ── Process spawn ────────────────────────────────────────────────────

/// Spawn the `claude` CLI in the given worktree with the description as
/// the initial prompt. The slave PTY fd becomes the process's stdio.
///
/// Returns the child PID.
fn spawn_claude(
    worktree_path: &PathBuf,
    description: &str,
    slave_fd: OwnedFd,
) -> Result<u32, String> {
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::process::CommandExt as _;

    let slave_raw = slave_fd.into_raw_fd();

    let mut cmd = std::process::Command::new("claude");
    cmd.arg("--print")
        .arg(description)
        .current_dir(worktree_path)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor");

    // SAFETY: The closure runs in the child process (after fork, before exec).
    // We redirect stdio to the slave PTY and create a new session. This is the
    // standard POSIX pattern for attaching a process to a PTY.
    #[allow(unsafe_code, reason = "PTY stdio redirection requires unsafe pre_exec with libc")]
    unsafe {
        cmd.pre_exec(move || {
            // New session so the child gets its own controlling terminal.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }

            // Set slave as controlling terminal.
            libc::ioctl(slave_raw, libc::TIOCSCTTY as libc::c_ulong, 0i32);

            // Redirect stdin/stdout/stderr.
            for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                if libc::dup2(slave_raw, fd) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            // Close the original slave fd (dup2 copies are sufficient).
            if slave_raw > libc::STDERR_FILENO {
                libc::close(slave_raw);
            }

            Ok(())
        });
    }

    let child = cmd.spawn().map_err(|e| format!("failed to spawn claude: {e}"))?;
    let pid = child.id();

    // Leak the Child so we don't wait — the PTY read loop detects EOF on exit.
    // We kill via SIGTERM in stop_task.
    let _child = std::mem::ManuallyDrop::new(child);

    Ok(pid)
}
