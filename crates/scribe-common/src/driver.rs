use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Lifecycle state of a driver task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DriverTaskState {
    Starting,
    Running,
    WaitingForInput,
    PermissionPrompt,
    Completed,
    Failed,
    Stopped,
}

/// Summary of a live driver task, sent in `DriverTaskList` responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverTaskInfo {
    pub task_id: Uuid,
    pub project_path: PathBuf,
    pub description: String,
    pub state: DriverTaskState,
    pub worktree_path: Option<PathBuf>,
    /// Unix timestamp (seconds since epoch) when the task was created.
    pub created_at: i64,
}
