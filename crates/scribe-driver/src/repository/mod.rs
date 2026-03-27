//! Task persistence types and the repository trait.

pub mod sqlite;

/// A persisted driver task record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskRecord {
    /// Unique task identifier (UUID string).
    pub id: String,
    /// Absolute path to the project directory.
    pub project_path: String,
    /// Human-readable task description.
    pub description: String,
    /// Current lifecycle state as a string (e.g. "Running", "Completed").
    pub state: String,
    /// Absolute path to the git worktree created for this task, if any.
    pub worktree_path: Option<String>,
    /// Unix timestamp (seconds) when the task was created.
    pub created_at: i64,
    /// Unix timestamp (seconds) when the task completed, if applicable.
    pub completed_at: Option<i64>,
    /// Exit code of the claude process, if applicable.
    pub exit_code: Option<i32>,
}

/// Aggregated metrics collected during a task run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskMetrics {
    /// Task this record belongs to.
    pub task_id: String,
    /// Total tokens consumed by the AI model.
    pub tokens_used: u64,
    /// Number of files the agent changed.
    pub files_changed: u64,
    /// Approximate cost in USD.
    pub cost_usd: f64,
    /// Number of agentic waves (tool-use rounds) completed.
    pub waves_completed: u64,
}

/// A single chunk of raw PTY output stored for a task.
#[derive(Debug, Clone)]
pub struct TaskOutputChunk {
    /// Task this chunk belongs to.
    pub task_id: String,
    /// Monotonically increasing sequence number within the task.
    pub seq: i64,
    /// Raw text chunk.
    pub chunk: String,
    /// Unix timestamp (milliseconds) when the chunk was written.
    pub timestamp: i64,
}

/// Aggregate statistics for the driver dashboard.
pub struct DriverStats {
    /// Number of currently running tasks.
    pub running: usize,
    /// Number of successfully completed tasks.
    pub completed: usize,
    /// Number of failed or stopped tasks.
    pub failed: usize,
    /// Total tokens used across all tasks.
    pub total_tokens: u64,
}

/// A persisted project record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProjectRecord {
    /// Absolute path to the project directory.
    pub path: String,
    /// Human-readable project name.
    pub name: String,
}

/// Repository interface for driver task persistence.
pub trait TaskRepository: Send + Sync {
    /// Persist a new task record.
    fn create_task(&self, record: &TaskRecord) -> Result<(), String>;

    /// Update the lifecycle state of an existing task.
    fn update_task_state(&self, task_id: &str, state: &str) -> Result<(), String>;

    /// Mark a task as completed, recording the exit code and timestamp.
    fn complete_task(&self, task_id: &str, exit_code: Option<i32>) -> Result<(), String>;

    /// List all tasks, most recent first.
    fn list_tasks(&self) -> Result<Vec<TaskRecord>, String>;

    /// Fetch a single task by ID.
    fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, String>;

    /// Append a chunk of PTY output for a task.
    fn append_output(&self, task_id: &str, chunk: &str) -> Result<(), String>;

    /// Retrieve all PTY output for a task as a single concatenated string.
    fn get_output(&self, task_id: &str) -> Result<String, String>;

    /// Insert or replace the metrics record for a task.
    fn update_metrics(&self, task_id: &str, metrics: &TaskMetrics) -> Result<(), String>;

    /// Fetch the metrics for a task, if recorded.
    fn get_metrics(&self, task_id: &str) -> Result<Option<TaskMetrics>, String>;

    /// Return aggregate statistics for the dashboard.
    fn get_stats(&self) -> Result<DriverStats, String>;

    /// Add a project to the list (no-op if already present).
    fn add_project(&self, path: &str, name: &str) -> Result<(), String>;

    /// Remove a project from the list by path.
    fn remove_project(&self, path: &str) -> Result<(), String>;

    /// List all projects, ordered alphabetically by name.
    fn list_projects(&self) -> Result<Vec<ProjectRecord>, String>;
}
