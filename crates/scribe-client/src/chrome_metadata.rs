//! Terminal-chrome metadata the server pushes about live sessions.
//!
//! The server is the only source of a pane's working directory, git branch,
//! shell/session context (remote host, tmux session) and env-capture health,
//! and of a workspace's display name. Those arrive as their own inbound
//! messages (`CwdChanged`, `GitBranch`, `SessionContextChanged`, `EnvStatus`,
//! `WorkspaceNamed`) and as fields on the authoritative `SessionList`, all of
//! which the GPUI client dropped before this module existed — every matching
//! status-bar field was hardcoded `None`.
//!
//! The store is kept pure (no GPUI, no IPC) so the merge rules are unit
//! testable headlessly. The IPC reader owns the write side behind a mutex and
//! the GPUI view reads it once per frame while building the status-bar model.

use std::{collections::HashMap, path::PathBuf};

use scribe_common::{
    ids::{SessionId, WorkspaceId},
    protocol::{EnvStatusState, SessionContext, SessionInfo, WorkspaceListEntry},
};

/// Chrome metadata for one session, as last reported by the server.
///
/// Every field is independently optional because the server emits each on its
/// own transition: a pane can have a CWD long before shell integration reports
/// a session context, and a branch only exists inside a git worktree.
#[derive(Debug, Clone, Default)]
pub struct SessionChrome {
    /// Stable local launch/environment-envelope identity from `SessionList`.
    pub launch_id: Option<String>,
    /// Working directory from OSC 7 (`CwdChanged`).
    pub cwd: Option<PathBuf>,
    /// Git branch the server detected for `cwd` (`GitBranch`). `None` outside a
    /// repository — the server sends the clearing message explicitly.
    pub git_branch: Option<String>,
    /// Remote host / tmux session reported by shell integration
    /// (`SessionContextChanged`).
    pub context: Option<SessionContext>,
    /// Env-capture runtime health (`EnvStatus`), driving the `⚠` glyph.
    pub env_status: Option<EnvStatusState>,
    /// Basename of the session's shell (`SessionCreated` / `SessionList`).
    ///
    /// Unlike the tab title — which the server overwrites with the OSC 0/2
    /// terminal title as soon as one arrives — this stays the shell the pane is
    /// actually running, which is what a dropped path has to be quoted for.
    pub shell_name: Option<String>,
}

impl SessionChrome {
    /// The remote host label to show instead of this machine's own.
    ///
    /// Only a context explicitly flagged `remote` with a non-empty host
    /// overrides the local label; a local shell keeps the client's own host
    /// name, matching the legacy client's `frame_status_snapshot`.
    #[must_use]
    pub fn host_label(&self) -> Option<&str> {
        let context = self.context.as_ref()?;
        if !context.remote {
            return None;
        }
        context.host.as_deref().filter(|host| !host.is_empty())
    }

    /// The tmux session name for the status bar's `tmux:` segment.
    #[must_use]
    pub fn tmux_label(&self) -> Option<&str> {
        self.context.as_ref()?.tmux_session.as_deref().filter(|label| !label.is_empty())
    }
}

/// Per-session and per-workspace chrome metadata for the whole window.
///
/// Sessions are keyed by id rather than by the attached pane so a background
/// tab keeps its metadata warm and switching tabs repaints the right chrome
/// without a server round trip.
#[derive(Debug, Clone, Default)]
pub struct ChromeMetadata {
    sessions: HashMap<SessionId, SessionChrome>,
    workspaces: HashMap<WorkspaceId, String>,
}

impl ChromeMetadata {
    /// An empty store (before the first `SessionList` arrives).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow one session's metadata, if any has been reported.
    #[must_use]
    pub fn session(&self, session_id: SessionId) -> Option<&SessionChrome> {
        self.sessions.get(&session_id)
    }

    /// The display name of a workspace, if the server has named it.
    #[must_use]
    pub fn workspace_name(&self, workspace_id: WorkspaceId) -> Option<&str> {
        self.workspaces.get(&workspace_id).map(String::as_str)
    }

    /// Record a pane's new working directory (`CwdChanged`).
    pub fn set_cwd(&mut self, session_id: SessionId, cwd: PathBuf) {
        self.sessions.entry(session_id).or_default().cwd = Some(cwd);
    }

    /// Record a pane's git branch (`GitBranch`).
    ///
    /// `None` is a real value, not a no-op: the server clears the branch when
    /// the CWD leaves a repository, and the segment must disappear with it.
    pub fn set_git_branch(&mut self, session_id: SessionId, branch: Option<String>) {
        self.sessions.entry(session_id).or_default().git_branch = branch;
    }

    /// Record a pane's shell/session context (`SessionContextChanged`).
    pub fn set_context(&mut self, session_id: SessionId, context: SessionContext) {
        self.sessions.entry(session_id).or_default().context = Some(context);
    }

    /// Record a pane's env-capture health (`EnvStatus`).
    pub fn set_env_status(&mut self, session_id: SessionId, state: EnvStatusState) {
        self.sessions.entry(session_id).or_default().env_status = Some(state);
    }

    /// Record the shell a session is running (`SessionCreated`).
    pub fn set_shell_name(&mut self, session_id: SessionId, shell_name: String) {
        self.sessions.entry(session_id).or_default().shell_name = Some(shell_name);
    }

    /// The shell a session runs, as last reported by the server.
    #[must_use]
    pub fn shell_name(&self, session_id: SessionId) -> Option<&str> {
        self.sessions.get(&session_id)?.shell_name.as_deref()
    }

    /// Record a workspace's display name (`WorkspaceNamed`).
    ///
    /// An empty name clears the entry so the status bar drops the segment
    /// rather than rendering a blank one.
    pub fn name_workspace(&mut self, workspace_id: WorkspaceId, name: String) {
        if name.trim().is_empty() {
            self.workspaces.remove(&workspace_id);
        } else {
            self.workspaces.insert(workspace_id, name);
        }
    }

    /// Drop everything known about an exited session.
    pub fn forget_session(&mut self, session_id: SessionId) {
        self.sessions.remove(&session_id);
    }

    /// Seed the store from an authoritative `SessionList`.
    ///
    /// The server replays each session's last-known CWD, branch and context on
    /// the list, so a reattach shows the same chrome the pane had before the
    /// client restarted instead of waiting for the next shell prompt. Sessions
    /// absent from the list are dropped: the list is the full live set.
    pub fn seed_from_session_list(
        &mut self,
        sessions: &[SessionInfo],
        workspaces: &[WorkspaceListEntry],
    ) {
        self.sessions.retain(|id, _| sessions.iter().any(|info| info.session_id == *id));
        for info in sessions {
            let entry = self.sessions.entry(info.session_id).or_default();
            if let Some(launch_id) = info.launch_id.clone() {
                entry.launch_id = Some(launch_id);
            }
            // The list carries a snapshot, not a transition: only overwrite a
            // field the server actually knows a value for, so a live update
            // that raced ahead of the list is not rolled back to `None`.
            if let Some(cwd) = info.cwd.clone() {
                entry.cwd = Some(cwd);
            }
            if let Some(branch) = info.git_branch.clone() {
                entry.git_branch = Some(branch);
            }
            if let Some(context) = info.context.clone() {
                entry.context = Some(context);
            }
            // The shell name is not optional on the wire, so the list always
            // carries it and a reattached pane knows its shell before the first
            // dropped path arrives.
            entry.shell_name = Some(info.shell_name.clone());
        }
        // Workspace rows are authoritative too: rebuilding the map clears a
        // listed `None` name and prunes workspaces omitted after reconnect.
        self.workspaces.clear();
        for workspace in workspaces {
            if let Some(name) = workspace.name.clone() {
                self.name_workspace(workspace.workspace_id, name);
            }
        }
    }
}

#[cfg(test)]
mod tests;
