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

    /// Fold an authoritative `SessionList` into the strip, keeping the order the
    /// strip already has.
    ///
    /// The list says which sessions exist and what they are called; it does not
    /// say what order the user put them in. Overwriting the strip with the
    /// list's order — which is what this did — threw away every drag-reorder on
    /// the next list and, because the server's list is grouped by a `HashMap` of
    /// workspaces, reshuffled a multi-region window's tabs on every reconnect.
    /// Order is client state now: it is restored from the server's workspace
    /// tree (see [`Self::order_by`]) and reported back on it.
    ///
    /// Surviving tabs keep their position and take the list's fresh metadata,
    /// sessions the list no longer names are dropped, and genuinely new ones are
    /// appended in list order. Returns the session that should be attached.
    pub fn reconcile(&mut self, incoming: Vec<TabEntry>) -> Option<SessionId> {
        let previous = self.active_session();
        let mut merged: Vec<TabEntry> = Vec::with_capacity(incoming.len());
        for existing in &self.tabs {
            if let Some(fresh) = incoming.iter().find(|tab| tab.session_id == existing.session_id) {
                merged.push(fresh.clone());
            }
        }
        for fresh in incoming {
            if !merged.iter().any(|tab| tab.session_id == fresh.session_id) {
                merged.push(fresh);
            }
        }
        self.tabs = merged;
        self.active = previous
            .and_then(|id| self.tabs.iter().position(|tab| tab.session_id == id))
            .unwrap_or(0);
        self.active_session()
    }

    /// Make `session_id` the active tab, if it is open.
    ///
    /// Used after a reconnect adoption to restore the tab the region was showing
    /// when the client last reported its tree.
    pub fn activate(&mut self, session_id: SessionId) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.session_id == session_id) else {
            return false;
        };
        self.active = index;
        true
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
    ///
    /// The refocus is workspace-scoped: an exit hands the selection to the
    /// nearest surviving tab of the *same* workspace, so a strip-adjacent tab
    /// from another region can never be pulled across a workspace boundary by
    /// the reconcile pass that adopts the newly active session. Only when the
    /// exited tab was its workspace's last does the selection fall back to the
    /// strip-adjacent neighbour — the region is collapsing, and the reconcile
    /// pass re-points the selection at whichever region inherits focus.
    pub fn remove(&mut self, session_id: SessionId) -> Option<SessionId> {
        let index = self.tabs.iter().position(|tab| tab.session_id == session_id)?;
        let workspace_id = self.tabs.get(index)?.workspace_id;
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
        if !was_active {
            return None;
        }
        if let Some(next) = self.nearest_in_workspace(index, workspace_id) {
            self.active = next;
        }
        self.active_session()
    }

    /// The surviving tab of `workspace_id` closest to removal point `index`,
    /// preferring the tab that slid into the removed slot over the one before
    /// it — the same "next tab wins" rule the strip-global clamp follows.
    fn nearest_in_workspace(&self, index: usize, workspace_id: WorkspaceId) -> Option<usize> {
        let distance = |i: usize| {
            // The slot at `index` now holds the old `index + 1` tab: the
            // successor. Rank it closest, then earlier tabs by proximity.
            if i >= index { (i - index) * 2 } else { (index - i) * 2 - 1 }
        };
        self.tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.workspace_id == workspace_id)
            .min_by_key(|(i, _)| distance(*i))
            .map(|(i, _)| i)
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

    /// Focus the `index`-th tab (0-based) of the active workspace.
    ///
    /// The strip is window-global, so `select_tab_N` shortcuts must not index
    /// it directly: with two workspaces the low digits would always land in
    /// the first region's tabs. This counts only tabs filed under the active
    /// workspace — the one holding the focused pane, kept current by every
    /// focus move — and ignores out-of-range digits like [`Self::select`].
    pub fn select_in_workspace(&mut self, index: usize) -> Option<SessionId> {
        let workspace_id = self.active_workspace()?;
        let strip_index = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.workspace_id == workspace_id)
            .nth(index)
            .map(|(i, _)| i)?;
        self.select(strip_index)
    }

    /// Move a tab within the strip while keeping the same session active.
    ///
    /// Returns `false` for an out-of-range or unchanged move so callers can
    /// skip a redundant redraw.
    /// Reorder the strip so sessions named in `order` come first, in that
    /// order; everything else keeps its relative order after them.
    ///
    /// Used after a reconnect adoption: `order` is the adopted layout's
    /// left-to-right region order, so each workspace's tab group sits in the
    /// strip where its region sits in the window.
    pub fn order_by(&mut self, order: &[SessionId]) {
        let active_session = self.active_session();
        let position = |tab: &TabEntry| {
            order.iter().position(|id| *id == tab.session_id).unwrap_or(usize::MAX)
        };
        self.tabs.sort_by_key(position);
        self.active = active_session
            .and_then(|id| self.tabs.iter().position(|tab| tab.session_id == id))
            .unwrap_or(0);
    }

    /// Keep every workspace's tabs in one contiguous run.
    ///
    /// New tabs are appended globally, so returning to an earlier workspace
    /// and opening one can otherwise produce `left, right, left`. The titlebar
    /// cannot anchor two separate runs at the same region edge without
    /// overlapping them, so normalize the strip while preserving both
    /// workspace order and the order of tabs inside each workspace.
    pub fn group_by_workspace(&mut self) {
        let active_session = self.active_session();
        let mut workspaces = Vec::new();
        for tab in &self.tabs {
            if !workspaces.contains(&tab.workspace_id) {
                workspaces.push(tab.workspace_id);
            }
        }
        self.tabs.sort_by_key(|tab| {
            workspaces.iter().position(|id| *id == tab.workspace_id).unwrap_or(usize::MAX)
        });
        self.active = active_session
            .and_then(|id| self.tabs.iter().position(|tab| tab.session_id == id))
            .unwrap_or(0);
    }

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

    /// The workspace a session's tab is filed under, if the session is open.
    #[must_use]
    pub fn workspace_of(&self, session_id: SessionId) -> Option<WorkspaceId> {
        self.tabs.iter().find(|tab| tab.session_id == session_id).map(|tab| tab.workspace_id)
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
