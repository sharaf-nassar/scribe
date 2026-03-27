//! Orchestrates webview IPC messages → `SQLite` + server IPC commands.

use std::sync::Arc;

use crate::messages::DriverIpcMessage;
use crate::repository::{DriverStats, ProjectRecord, TaskRecord, TaskRepository};
use crate::server_client::DriverServerCommand;

/// Orchestrator that handles IPC messages from the webview.
pub struct DriverHandler {
    repo: Arc<dyn TaskRepository>,
    cmd_tx: std::sync::mpsc::Sender<DriverServerCommand>,
}

impl DriverHandler {
    /// Create a new handler.
    pub fn new(
        repo: Arc<dyn TaskRepository>,
        cmd_tx: std::sync::mpsc::Sender<DriverServerCommand>,
    ) -> Self {
        Self { repo, cmd_tx }
    }

    /// Handle a raw JSON string from the webview IPC handler.
    ///
    /// Returns a JSON string to push back to the webview, or an empty string
    /// if no response is required.
    pub fn handle_raw(&self, body: &str) -> String {
        let msg = match serde_json::from_str::<DriverIpcMessage>(body) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("failed to parse driver IPC message: {e} — body: {body}");
                return error_response(&format!("invalid message: {e}"));
            }
        };

        match self.handle_message(msg) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("driver handler error: {e}");
                error_response(&e)
            }
        }
    }

    /// Dispatch a parsed [`DriverIpcMessage`] and return a JSON response.
    fn handle_message(&self, msg: DriverIpcMessage) -> Result<String, String> {
        match msg {
            DriverIpcMessage::CreateTask { project_path, description } => {
                self.handle_create_task(project_path, description)
            }
            DriverIpcMessage::StopTask { task_id } => self.handle_stop_task(&task_id),
            DriverIpcMessage::SendInput { task_id, data } => self.handle_send_input(&task_id, data),
            DriverIpcMessage::RequestState => self.handle_request_state(),
            DriverIpcMessage::SwitchView { view } => {
                tracing::debug!("driver UI switched view to: {view}");
                Ok(String::new())
            }
            DriverIpcMessage::AddProject { path } => self.handle_add_project(&path),
            DriverIpcMessage::RemoveProject { path } => self.handle_remove_project(&path),
            DriverIpcMessage::ListProjects => self.handle_list_projects(),
        }
    }

    fn handle_create_task(
        &self,
        project_path: String,
        description: String,
    ) -> Result<String, String> {
        let task_id = uuid::Uuid::new_v4();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().cast_signed())
            .unwrap_or(0_i64);

        let record = TaskRecord {
            id: task_id.to_string(),
            project_path: project_path.clone(),
            description: description.clone(),
            state: String::from("Starting"),
            worktree_path: None,
            created_at: now,
            completed_at: None,
            exit_code: None,
        };

        self.repo.create_task(&record)?;

        let cmd = DriverServerCommand::CreateTask {
            task_id,
            project_path: std::path::PathBuf::from(project_path),
            description,
        };
        self.cmd_tx.send(cmd).map_err(|e| format!("failed to send CreateTask to server: {e}"))?;

        serde_json::to_string(&serde_json::json!({
            "type": "task_created",
            "task_id": task_id.to_string(),
        }))
        .map_err(|e| format!("serialization error: {e}"))
    }

    fn handle_stop_task(&self, task_id: &str) -> Result<String, String> {
        let id = uuid::Uuid::parse_str(task_id)
            .map_err(|e| format!("invalid task UUID '{task_id}': {e}"))?;
        let cmd = DriverServerCommand::StopTask { task_id: id };
        self.cmd_tx.send(cmd).map_err(|e| format!("failed to send StopTask to server: {e}"))?;
        Ok(String::new())
    }

    fn handle_send_input(&self, task_id: &str, data: String) -> Result<String, String> {
        let id = uuid::Uuid::parse_str(task_id)
            .map_err(|e| format!("invalid task UUID '{task_id}': {e}"))?;
        let cmd = DriverServerCommand::SendInput { task_id: id, data: data.into_bytes() };
        self.cmd_tx.send(cmd).map_err(|e| format!("failed to send SendInput to server: {e}"))?;
        Ok(String::new())
    }

    fn handle_request_state(&self) -> Result<String, String> {
        let tasks = self.repo.list_tasks()?;
        let stats = self.repo.get_stats()?;
        let projects = self.repo.list_projects()?;
        build_state_response(&tasks, &stats, &projects)
    }

    fn handle_add_project(&self, path: &str) -> Result<String, String> {
        let normalized = normalize_path(path);
        let name = extract_project_name(&normalized);
        self.repo.add_project(&normalized, &name)?;
        let projects = self.repo.list_projects()?;
        serde_json::to_string(&serde_json::json!({
            "type": "project_added",
            "projects": projects,
        }))
        .map_err(|e| format!("serialization error: {e}"))
    }

    fn handle_remove_project(&self, path: &str) -> Result<String, String> {
        let normalized = normalize_path(path);
        self.repo.remove_project(&normalized)?;
        let projects = self.repo.list_projects()?;
        serde_json::to_string(&serde_json::json!({
            "type": "project_removed",
            "projects": projects,
        }))
        .map_err(|e| format!("serialization error: {e}"))
    }

    fn handle_list_projects(&self) -> Result<String, String> {
        let projects = self.repo.list_projects()?;
        serde_json::to_string(&serde_json::json!({
            "type": "projects",
            "projects": projects,
        }))
        .map_err(|e| format!("serialization error: {e}"))
    }
}

/// Build the full-state JSON response for `RequestState`.
fn build_state_response(
    tasks: &[TaskRecord],
    stats: &DriverStats,
    projects: &[ProjectRecord],
) -> Result<String, String> {
    serde_json::to_string(&serde_json::json!({
        "type": "state",
        "tasks": tasks,
        "stats": {
            "running": stats.running,
            "completed": stats.completed,
            "failed": stats.failed,
            "total_tokens": stats.total_tokens,
        },
        "projects": projects,
    }))
    .map_err(|e| format!("serialization error: {e}"))
}

/// Normalize a project path by trimming trailing slashes (preserving root `/`).
fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return String::from(
            path.chars().next().map_or(".", |c| if c == '/' { "/" } else { path }),
        );
    }
    trimmed.to_owned()
}

/// Extract a human-readable project name from a filesystem path.
///
/// Returns the last non-empty path segment, or the full path as fallback.
fn extract_project_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return path.to_owned();
    }
    trimmed.split('/').rfind(|s| !s.is_empty()).unwrap_or(path).to_owned()
}

/// Build an error response JSON string.
fn error_response(msg: &str) -> String {
    serde_json::json!({ "type": "error", "message": msg }).to_string()
}
