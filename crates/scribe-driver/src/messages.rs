//! Typed IPC messages between the driver webview JS and Rust.

/// Messages sent from the webview JS to the Rust backend via the IPC handler.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
pub enum DriverIpcMessage {
    /// Create a new driver task for the given project.
    CreateTask { project_path: String, description: String },
    /// Stop a running task by ID.
    StopTask { task_id: String },
    /// Send input data to a running task's PTY.
    SendInput { task_id: String, data: String },
    /// Request the current full task state (for initial load / refresh).
    RequestState,
    /// Switch the active view in the UI.
    SwitchView { view: String },
    /// Add a project to the project list.
    AddProject { path: String },
    /// Remove a project from the project list by path.
    RemoveProject { path: String },
    /// Request the full project list.
    ListProjects,
}
