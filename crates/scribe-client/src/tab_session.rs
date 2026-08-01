//! Ordered tab/session model backing the GPUI shell's tab bar.
//!
//! The winit client kept its tab order inside the window-layout tree; the GPUI
//! rebuild's shell owns a single workspace, so the tab strip needs only an
//! ordered session list plus a selection cursor. This module is that model,
//! kept pure (no GPUI, no IPC) so the selection, insertion, and removal rules
//! are unit-testable headlessly.
//!
//! The IPC reader mutates it from server traffic (`SessionList`,
//! `SessionCreated`, `SessionExited`) and the key-dispatch path mutates it from
//! [`crate::keybindings::LayoutAction`] tab commands. Both share it behind a
//! mutex, so every mutator returns the session that should now be attached (or
//! `None` when nothing changed) rather than reaching for the IPC sink itself.

use scribe_common::ids::{SessionId, WorkspaceId};

use crate::tab_bar::TabData;

/// One tab in the shell's strip: the session it renders and its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabEntry {
    /// The server session this tab is attached to.
    pub session_id: SessionId,
    /// The workspace the session belongs to, used when spawning sibling tabs.
    pub workspace_id: WorkspaceId,
    /// Tab label (shell basename, or the session title once one arrives).
    pub title: String,
    /// Provider task label, set while an AI tool is working on a named task.
    /// It outranks `title` in the rendered strip and is dropped again when the
    /// provider clears it.
    pub task_label: Option<String>,
}

impl TabEntry {
    /// A tab with no provider task label yet.
    #[must_use]
    pub fn new(session_id: SessionId, workspace_id: WorkspaceId, title: String) -> Self {
        Self { session_id, workspace_id, title, task_label: None }
    }

    /// The label the strip renders: the provider task label while one is
    /// active, otherwise the session title.
    ///
    /// Mirrors the winit client's `Pane::preferred_tab_title` so a pane's label
    /// does not change meaning across the cutover.
    #[must_use]
    pub fn display_title(&self) -> &str {
        self.task_label.as_deref().unwrap_or(&self.title)
    }
}

/// Ordered tab strip plus the index of the active tab.
///
/// The active index is always a valid position while the strip is non-empty;
/// removal clamps it so the selection can never dangle past the end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabSessions {
    tabs: Vec<TabEntry>,
    active: usize,
}

impl TabSessions {
    /// An empty strip (before the first `SessionList` arrives).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the ordered tabs.
    #[must_use]
    pub fn tabs(&self) -> &[TabEntry] {
        &self.tabs
    }

    /// Number of open tabs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// `true` while no session is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// The active tab, if any.
    #[must_use]
    pub fn active(&self) -> Option<&TabEntry> {
        self.tabs.get(self.active)
    }

    /// The active tab's session id, if any.
    #[must_use]
    pub fn active_session(&self) -> Option<SessionId> {
        self.active().map(|tab| tab.session_id)
    }

    /// The workspace new tabs should be spawned into.
    ///
    /// Falls back to the first tab's workspace so a `new_tab` issued while the
    /// selection is momentarily empty still targets the shell's workspace.
    #[must_use]
    pub fn active_workspace(&self) -> Option<WorkspaceId> {
        self.active().or_else(|| self.tabs.first()).map(|tab| tab.workspace_id)
    }

    /// Replace the whole strip (an authoritative `SessionList`), preserving the
    /// active session when it survived the rebuild.
    ///
    /// Returns the session that should be attached after the rebuild.
    pub fn replace_all(&mut self, tabs: Vec<TabEntry>) -> Option<SessionId> {
        let previous = self.active_session();
        self.tabs = tabs;
        self.active = previous
            .and_then(|id| self.tabs.iter().position(|tab| tab.session_id == id))
            .unwrap_or(0);
        self.active_session()
    }

    /// Append a session and focus it, matching the legacy client's behaviour of
    /// switching to a freshly created tab.
    ///
    /// Returns `false` — leaving the strip untouched — when the session is
    /// already open. The server re-announces `SessionCreated` as its
    /// acknowledgement of every `AttachSessions`, so a caller that attached on
    /// each announcement would loop forever; this makes the insert the only
    /// event that means "a new tab appeared".
    pub fn insert_active(&mut self, entry: TabEntry) -> bool {
        if self.tabs.iter().any(|tab| tab.session_id == entry.session_id) {
            return false;
        }
        self.tabs.push(entry);
        self.active = self.tabs.len() - 1;
        true
    }

    /// Drop a session (it exited or its tab was closed), clamping the selection.
    ///
    /// Returns the session that should now be attached: `None` when the strip
    /// is empty or when the removal did not disturb the active tab.
    pub fn remove(&mut self, session_id: SessionId) -> Option<SessionId> {
        let index = self.tabs.iter().position(|tab| tab.session_id == session_id)?;
        let was_active = index == self.active;
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
            return None;
        }
        if index < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len() - 1);
        was_active.then(|| self.active_session()).flatten()
    }

    /// Focus the next tab, wrapping at the end.
    ///
    /// Returns the newly active session, or `None` when the selection did not
    /// move (fewer than two tabs) so the caller skips a redundant attach.
    pub fn focus_next(&mut self) -> Option<SessionId> {
        self.step(1)
    }

    /// Focus the previous tab, wrapping at the start.
    pub fn focus_prev(&mut self) -> Option<SessionId> {
        self.step(-1)
    }

    /// Focus the tab at `index` (0-based), ignoring out-of-range positions the
    /// way the legacy `select_tab_N` shortcuts did.
    pub fn select(&mut self, index: usize) -> Option<SessionId> {
        if index >= self.tabs.len() || index == self.active {
            return None;
        }
        self.active = index;
        self.active_session()
    }

    /// Move a tab within the strip while keeping the same session active.
    ///
    /// Returns `false` for an out-of-range or unchanged move so callers can
    /// skip a redundant redraw.
    pub fn reorder(&mut self, from: usize, to: usize) -> bool {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return false;
        }
        let active_session = self.active_session();
        let moved = self.tabs.remove(from);
        self.tabs.insert(to, moved);
        self.active = active_session
            .and_then(|id| self.tabs.iter().position(|tab| tab.session_id == id))
            .unwrap_or(0);
        true
    }

    /// Retitle a session's tab, returning `true` when the label changed.
    pub fn set_title(&mut self, session_id: SessionId, title: String) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.session_id == session_id) else {
            return false;
        };
        if tab.title == title {
            return false;
        }
        tab.title = title;
        true
    }

    /// Re-file a session under another workspace, returning `true` when the
    /// entry moved.
    ///
    /// A session's workspace is the server's to own, but the client learns of a
    /// move first: a pane in a freshly split region adopts a session that was
    /// created through the previous region's workspace, and the client sends
    /// `ClientMessage::MoveSession` to say so. Recording it here keeps the strip
    /// (and therefore the workspace a later `new_tab` targets) in step with the
    /// region the user is actually in, instead of pinning every later session to
    /// the window's first workspace.
    pub fn set_workspace(&mut self, session_id: SessionId, workspace_id: WorkspaceId) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.session_id == session_id) else {
            return false;
        };
        if tab.workspace_id == workspace_id {
            return false;
        }
        tab.workspace_id = workspace_id;
        true
    }

    /// Set or clear a session's provider task label, returning `true` when the
    /// strip changed.
    ///
    /// A blank label is treated as no label at all, matching the winit client's
    /// rule that a provider must not be able to blank a tab down to nothing.
    pub fn set_task_label(&mut self, session_id: SessionId, label: Option<&str>) -> bool {
        let label = label.map(str::trim).filter(|label| !label.is_empty());
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.session_id == session_id) else {
            return false;
        };
        if tab.task_label.as_deref() == label {
            return false;
        }
        tab.task_label = label.map(ToOwned::to_owned);
        true
    }

    /// Lower the strip into the titlebar's render model.
    #[must_use]
    pub fn to_tab_data(&self) -> Vec<TabData> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let mut data = TabData::new(tab.display_title().to_owned());
                data.accessibility_id = tab.session_id.to_string();
                data.is_active = index == self.active;
                data
            })
            .collect()
    }

    /// Move the selection by `delta` with wraparound.
    fn step(&mut self, delta: isize) -> Option<SessionId> {
        let len = self.tabs.len();
        if len < 2 {
            return None;
        }
        let len_i = isize::try_from(len).unwrap_or(isize::MAX);
        let current = isize::try_from(self.active).unwrap_or(0);
        self.active = usize::try_from((current + delta).rem_euclid(len_i)).unwrap_or(0);
        self.active_session()
    }
}

#[cfg(test)]
mod tests;
