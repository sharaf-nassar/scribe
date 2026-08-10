//! Per-region tab model backing the GPUI shell's tab strip.
//!
//! A window is a tree of workspace regions, and each region has its own ordered
//! tabs and its own tab on screen. This module stores exactly that: regions in
//! strip order, each owning its tabs and the index of the one it is showing.
//!
//! It deliberately holds no window-wide "active tab". Which region owns the
//! window's focus belongs to the shell (`PaneShell::focused_workspace_id`), and
//! the session the window is attached to is `Shared::active_session` — keeping a
//! third copy here is what let a selection name a tab in one region and a pane
//! in another, so every method that needs a region takes it as an argument
//! instead. The previous shape was one flat `Vec` plus one cursor, written when
//! the GPUI shell owned a single workspace; everything that had to be layered on
//! top of it afterwards (strip-wide regrouping, titlebar-position translation,
//! two selection APIs, a cross-workspace adoption guard) existed to simulate the
//! partition this now stores directly.
//!
//! The IPC reader mutates it from server traffic (`SessionList`,
//! `SessionCreated`, `SessionExited`) and the key-dispatch path mutates it from
//! [`crate::keybindings::LayoutAction`] tab commands. Both share it behind a
//! mutex — which is why it stays a plain data structure rather than a GPUI
//! entity, the reader thread having no `App` to hold one in — so every mutator
//! returns the session that should now be attached (or `None` when nothing
//! changed) rather than reaching for the IPC sink itself.

use scribe_common::ids::{SessionId, WorkspaceId};

use crate::tab_bar::TabData;

/// One tab in the shell's strip: the session it renders and its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabEntry {
    /// The server session this tab is attached to.
    pub session_id: SessionId,
    /// The workspace the session belongs to, used when spawning sibling tabs.
    pub workspace_id: WorkspaceId,
    /// Basename of the shell or command entrypoint.
    pub shell_name: String,
    /// Native application title from OSC 0/2, when one is active.
    pub terminal_title: Option<String>,
    /// Provider task label, set while an AI tool is working on a named task.
    /// It is a fallback when the application has not set its own title and is
    /// dropped again when the provider clears it.
    pub task_label: Option<String>,
}

impl TabEntry {
    /// A tab with no provider task label yet.
    #[must_use]
    pub fn new(session_id: SessionId, workspace_id: WorkspaceId, shell_name: String) -> Self {
        Self { session_id, workspace_id, shell_name, terminal_title: None, task_label: None }
    }

    /// The label the strip renders: native title, AI fallback, then shell.
    #[must_use]
    pub fn display_title(&self) -> &str {
        self.terminal_title.as_deref().or(self.task_label.as_deref()).unwrap_or(&self.shell_name)
    }
}

/// One region's ordered tabs plus the index of the tab it is showing.
///
/// The active index is always a valid position while the region is non-empty;
/// an empty region is dropped rather than kept with a dangling cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTabs {
    /// The region these tabs belong to.
    pub workspace_id: WorkspaceId,
    tabs: Vec<TabEntry>,
    active: usize,
}

impl WorkspaceTabs {
    /// A region showing its only tab.
    fn seeded(entry: TabEntry) -> Self {
        Self { workspace_id: entry.workspace_id, tabs: vec![entry], active: 0 }
    }

    /// Borrow this region's ordered tabs.
    #[must_use]
    pub fn tabs(&self) -> &[TabEntry] {
        &self.tabs
    }

    /// The session this region is showing.
    #[must_use]
    pub fn active_session(&self) -> Option<SessionId> {
        self.tabs.get(self.active).map(|tab| tab.session_id)
    }

    /// Number of tabs in this region.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// `true` while this region holds no tabs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Re-point `active` at `session_id`, falling back to the first tab.
    ///
    /// Every reorder runs through this rather than adjusting the index by hand:
    /// the tab on screen must not change because its neighbours moved.
    fn keep_showing(&mut self, session_id: Option<SessionId>) {
        self.active = session_id
            .and_then(|id| self.tabs.iter().position(|tab| tab.session_id == id))
            .unwrap_or(0);
    }
}

/// Where one rendered tab lives: its region, its position inside that region,
/// and the session it renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabAddress {
    /// The region the tab belongs to.
    pub workspace_id: WorkspaceId,
    /// The tab's position within that region — the index [`TabSessions::select`]
    /// and [`TabSessions::reorder`] take, never a window-global one.
    pub index: usize,
    /// The session the tab renders.
    pub session_id: SessionId,
}

/// The window's tabs, partitioned by region and ordered as the strip draws
/// them: regions left to right, tabs left to right inside each region.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabSessions {
    regions: Vec<WorkspaceTabs>,
}

impl TabSessions {
    /// An empty strip (before the first `SessionList` arrives).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the regions in strip order.
    #[must_use]
    pub fn regions(&self) -> &[WorkspaceTabs] {
        &self.regions
    }

    /// Every tab in the window, in strip order.
    ///
    /// Region grouping is structural now, so this is simply the concatenation —
    /// there is no longer a regrouping pass that has to run before the strip can
    /// be drawn or reported.
    pub fn entries(&self) -> impl Iterator<Item = &TabEntry> {
        self.regions.iter().flat_map(|region| region.tabs.iter())
    }

    /// Total number of open tabs across every region.
    #[must_use]
    pub fn len(&self) -> usize {
        self.regions.iter().map(WorkspaceTabs::len).sum()
    }

    /// `true` while no session is open in any region.
    ///
    /// A region that runs out of tabs is dropped ([`Self::prune_empty`]), so an
    /// empty strip is an empty region list.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Borrow one region's tabs.
    #[must_use]
    pub fn region(&self, workspace_id: WorkspaceId) -> Option<&WorkspaceTabs> {
        self.regions.iter().find(|region| region.workspace_id == workspace_id)
    }

    /// The tab `workspace_id`'s region is showing.
    #[must_use]
    pub fn active_session_in(&self, workspace_id: WorkspaceId) -> Option<SessionId> {
        self.region(workspace_id).and_then(WorkspaceTabs::active_session)
    }

    /// The workspace a session's tab is filed under, if the session is open.
    #[must_use]
    pub fn workspace_of(&self, session_id: SessionId) -> Option<WorkspaceId> {
        self.entries().find(|tab| tab.session_id == session_id).map(|tab| tab.workspace_id)
    }

    /// Fold an authoritative `SessionList` into the strip, keeping the order the
    /// strip already has.
    ///
    /// The list says which sessions exist and what they are called; it does not
    /// say what order the user put them in. Overwriting the strip with the
    /// list's order threw away every drag-reorder on the next list and, because
    /// the server's list is grouped by a `HashMap` of workspaces, reshuffled a
    /// multi-region window's tabs on every reconnect. Order is client state: it
    /// is restored from the server's workspace tree (see [`Self::order_by`]) and
    /// reported back on it.
    ///
    /// Surviving tabs keep their position and take the list's fresh metadata,
    /// sessions the list no longer names are dropped, genuinely new ones are
    /// appended to their own region in list order, and each region keeps
    /// showing the tab it was showing. Returns the session the window should be
    /// attached to: `attached` when it survived the list, else the first
    /// region's shown tab.
    pub fn reconcile(
        &mut self,
        incoming: Vec<TabEntry>,
        attached: Option<SessionId>,
    ) -> Option<SessionId> {
        let showing: Vec<(WorkspaceId, Option<SessionId>)> = self
            .regions
            .iter()
            .map(|region| (region.workspace_id, region.active_session()))
            .collect();
        // Regions keep their strip order and lose only the tabs the list
        // dropped; a tab that moved workspace is re-filed by the append pass
        // below, so it is removed here too.
        // A tab the list still names keeps its slot and takes the list's fresh
        // metadata. One the list re-files under another workspace is dropped
        // here and re-appended to its new region by the pass below, so the
        // server's filing always wins over the strip's.
        let refreshed = |tab: &TabEntry| {
            incoming
                .iter()
                .find(|fresh| {
                    fresh.session_id == tab.session_id && fresh.workspace_id == tab.workspace_id
                })
                .cloned()
        };
        for region in &mut self.regions {
            region.tabs = region.tabs.iter().filter_map(refreshed).collect();
        }
        for fresh in incoming {
            if self.entries().any(|tab| tab.session_id == fresh.session_id) {
                continue;
            }
            self.push(fresh);
        }
        // Every region is re-pointed, not just the ones that already existed:
        // the append pass shows each tab it pushes (correct for a user opening
        // one, wrong for a rebuild), so a region the list introduces has to fall
        // back to its first tab rather than keep whichever tab landed last.
        for region in &mut self.regions {
            let previous = showing
                .iter()
                .find(|(workspace_id, _)| *workspace_id == region.workspace_id)
                .and_then(|(_, session_id)| *session_id);
            region.keep_showing(previous);
        }
        self.prune_empty();
        attached
            .filter(|id| self.entries().any(|tab| tab.session_id == *id))
            .or_else(|| self.regions.first().and_then(WorkspaceTabs::active_session))
    }

    /// Append a session to its own region and show it there, matching the
    /// legacy client's behaviour of switching to a freshly created tab.
    ///
    /// Returns `false` — leaving the strip untouched — when the session is
    /// already open. The server re-announces `SessionCreated` as its
    /// acknowledgement of every `AttachSessions`, so a caller that attached on
    /// each announcement would loop forever; this makes the insert the only
    /// event that means "a new tab appeared".
    pub fn insert_active(&mut self, entry: TabEntry) -> bool {
        if self.entries().any(|tab| tab.session_id == entry.session_id) {
            return false;
        }
        self.push(entry);
        true
    }

    /// Append `entry` to its region, creating the region at the end of the
    /// strip when this is its first tab, and show it.
    fn push(&mut self, entry: TabEntry) {
        match self.regions.iter_mut().find(|region| region.workspace_id == entry.workspace_id) {
            Some(region) => {
                region.tabs.push(entry);
                region.active = region.tabs.len() - 1;
            }
            None => self.regions.push(WorkspaceTabs::seeded(entry)),
        }
    }

    /// Drop a session (it exited or its tab was closed).
    ///
    /// Returns the tab its region should now show, or `None` when the removed
    /// tab was not the one on screen or its region emptied out. The refocus
    /// cannot leave the region: a region's tabs are its own, so the "next tab
    /// wins, else the previous one" rule is just a clamp inside that region and
    /// no strip-adjacent tab from another region is reachable.
    pub fn remove(&mut self, session_id: SessionId) -> Option<SessionId> {
        let region = self
            .regions
            .iter_mut()
            .find(|region| region.tabs.iter().any(|tab| tab.session_id == session_id))?;
        let index = region.tabs.iter().position(|tab| tab.session_id == session_id)?;
        let was_active = index == region.active;
        region.tabs.remove(index);
        if region.tabs.is_empty() {
            self.prune_empty();
            return None;
        }
        // The slot at `index` now holds what was its successor, so clamping is
        // already "next tab wins"; only a removal before the shown tab shifts it.
        if index < region.active {
            region.active -= 1;
        }
        region.active = region.active.min(region.tabs.len() - 1);
        was_active.then(|| region.active_session()).flatten()
    }

    /// Show the next tab of `workspace_id`'s region, wrapping at the end.
    ///
    /// Returns the newly shown session, or `None` when the selection did not
    /// move (fewer than two tabs in the region) so the caller skips a redundant
    /// attach. The walk stays inside the region — with a window-global strip it
    /// stepped off the end into a neighbouring region's tabs, which is a switch
    /// no pane could honour without dragging the session across the boundary.
    pub fn focus_next(&mut self, workspace_id: WorkspaceId) -> Option<SessionId> {
        self.step(workspace_id, 1)
    }

    /// Show the previous tab of `workspace_id`'s region, wrapping at the start.
    pub fn focus_prev(&mut self, workspace_id: WorkspaceId) -> Option<SessionId> {
        self.step(workspace_id, -1)
    }

    /// Show the `index`-th tab (0-based) of `workspace_id`'s region, ignoring
    /// out-of-range positions the way the legacy `select_tab_N` shortcuts did.
    ///
    /// This is the only selection entry point. The window-global variant it
    /// replaces is what let a `select_tab_N` digit, a titlebar click, or a strip
    /// fallback name a tab outside the region it was aimed at.
    pub fn select(&mut self, workspace_id: WorkspaceId, index: usize) -> Option<SessionId> {
        let region = self.region_mut(workspace_id)?;
        if index >= region.tabs.len() || index == region.active {
            return None;
        }
        region.active = index;
        region.active_session()
    }

    /// Show `session_id` in its own region, whichever region that is.
    ///
    /// Returns the session when this changed what its region shows, and `None`
    /// when it was already showing (or is not open) so the caller skips a
    /// redundant attach — the same contract as [`Self::select`], which is what
    /// lets both feed `switch_tab`. Callers name a session rather than a
    /// position because a click, a notification, a pane focus, a reconnect
    /// adoption, and a strip fallback all know *which session*; with a flat
    /// strip each had to look up a window-global index first, and that lookup
    /// is what carried the selection out of the region it was aimed at.
    pub fn show(&mut self, session_id: SessionId) -> Option<SessionId> {
        let region = self
            .regions
            .iter_mut()
            .find(|region| region.tabs.iter().any(|tab| tab.session_id == session_id))?;
        let index = region.tabs.iter().position(|tab| tab.session_id == session_id)?;
        if index == region.active {
            return None;
        }
        region.active = index;
        Some(session_id)
    }

    /// Move a tab inside its own region, keeping the same tab on screen.
    ///
    /// Both positions are region-local, so a drag cannot carry a tab out of its
    /// region — the strip offset arithmetic that used to translate them is gone
    /// with the flat model. Returns `false` for an out-of-range or unchanged
    /// move so callers can skip a redundant redraw.
    pub fn reorder(&mut self, workspace_id: WorkspaceId, from: usize, to: usize) -> bool {
        let Some(region) = self.region_mut(workspace_id) else { return false };
        if from >= region.tabs.len() || to >= region.tabs.len() || from == to {
            return false;
        }
        let showing = region.active_session();
        let moved = region.tabs.remove(from);
        region.tabs.insert(to, moved);
        region.keep_showing(showing);
        true
    }

    /// Order regions, and the tabs inside each, to match `order`.
    ///
    /// Used after a reconnect adoption: `order` is the adopted layout's
    /// left-to-right, top-to-bottom session order, so each region lands where
    /// its own region sits in the window and its tabs come back in the order the
    /// user left them. Sessions the order does not name keep their relative
    /// position at the end.
    pub fn order_by(&mut self, order: &[SessionId]) {
        let rank = |session_id: SessionId| {
            order.iter().position(|id| *id == session_id).unwrap_or(usize::MAX)
        };
        for region in &mut self.regions {
            let showing = region.active_session();
            region.tabs.sort_by_key(|tab| rank(tab.session_id));
            region.keep_showing(showing);
        }
        self.regions.sort_by_key(|region| {
            region.tabs.iter().map(|tab| rank(tab.session_id)).min().unwrap_or(usize::MAX)
        });
    }

    /// Set or reset a session's native title, returning `true` on change.
    pub fn set_title(&mut self, session_id: SessionId, title: Option<String>) -> bool {
        let title = title.filter(|title| !title.trim().is_empty());
        let Some(tab) = self.entry_mut(session_id) else { return false };
        if tab.terminal_title == title {
            return false;
        }
        tab.terminal_title = title;
        true
    }

    /// Re-file a session under another region, returning `true` when it moved.
    ///
    /// A session's workspace is the server's to own, but the client learns of a
    /// move first: a pane in a freshly split region adopts a session that was
    /// created through the previous region's workspace, and the client sends
    /// `ClientMessage::MoveSession` to say so. Recording it here keeps the strip
    /// in step with the region the user is actually in.
    ///
    /// The tab is appended to the target region and shown there, because the
    /// only thing that moves a session between regions is a pane in the target
    /// adopting it. A source region left empty is dropped.
    pub fn set_workspace(&mut self, session_id: SessionId, workspace_id: WorkspaceId) -> bool {
        let Some(source) = self
            .regions
            .iter_mut()
            .find(|region| region.tabs.iter().any(|tab| tab.session_id == session_id))
        else {
            return false;
        };
        if source.workspace_id == workspace_id {
            return false;
        }
        let Some(index) = source.tabs.iter().position(|tab| tab.session_id == session_id) else {
            return false;
        };
        let showing = source.active_session();
        let mut entry = source.tabs.remove(index);
        source.keep_showing(showing);
        entry.workspace_id = workspace_id;
        self.push(entry);
        self.prune_empty();
        true
    }

    /// Set or clear a session's provider task label, returning `true` when the
    /// strip changed.
    ///
    /// A blank label is treated as no label at all, matching the winit client's
    /// rule that a provider must not be able to blank a tab down to nothing.
    pub fn set_task_label(&mut self, session_id: SessionId, label: Option<&str>) -> bool {
        let label = label.map(str::trim).filter(|label| !label.is_empty());
        let Some(tab) = self.entry_mut(session_id) else { return false };
        if tab.task_label.as_deref() == label {
            return false;
        }
        tab.task_label = label.map(ToOwned::to_owned);
        true
    }

    /// Every tab's address, in the same order as [`Self::to_tab_data`].
    ///
    /// The titlebar and the in-region bars are flat rows of pixels, so a click
    /// arrives as a position in a row and has to be resolved back to a tab.
    /// Pairing the render model with these addresses is what makes that
    /// resolution total: a row position names a region and a position inside
    /// it, never a window-global index that a neighbouring region also answers
    /// to.
    pub fn addresses(&self) -> impl Iterator<Item = TabAddress> {
        self.regions.iter().flat_map(|region| {
            region.tabs.iter().enumerate().map(move |(index, tab)| TabAddress {
                workspace_id: region.workspace_id,
                index,
                session_id: tab.session_id,
            })
        })
    }

    /// Lower the strip into the titlebar's render model, in strip order.
    ///
    /// `is_active` marks the tab each region is *showing*, not one tab for the
    /// whole window: every region paints its own bar and every bar underlines
    /// its own tab, independent of which region holds the window's focus.
    #[must_use]
    pub fn to_tab_data(&self) -> Vec<TabData> {
        self.regions
            .iter()
            .flat_map(|region| {
                region.tabs.iter().enumerate().map(move |(index, tab)| {
                    let mut data = TabData::new(tab.display_title().to_owned());
                    data.accessibility_id = tab.session_id.to_string();
                    data.is_active = index == region.active;
                    data
                })
            })
            .collect()
    }

    /// Borrow one region mutably.
    fn region_mut(&mut self, workspace_id: WorkspaceId) -> Option<&mut WorkspaceTabs> {
        self.regions.iter_mut().find(|region| region.workspace_id == workspace_id)
    }

    /// Borrow one session's tab mutably, wherever it is filed.
    fn entry_mut(&mut self, session_id: SessionId) -> Option<&mut TabEntry> {
        self.regions
            .iter_mut()
            .flat_map(|region| region.tabs.iter_mut())
            .find(|tab| tab.session_id == session_id)
    }

    /// Drop regions that have run out of tabs, so an empty region can never be
    /// carried with a dangling cursor.
    fn prune_empty(&mut self) {
        self.regions.retain(|region| !region.tabs.is_empty());
    }

    /// Move a region's shown tab by `delta` with wraparound.
    fn step(&mut self, workspace_id: WorkspaceId, delta: isize) -> Option<SessionId> {
        let region = self.region_mut(workspace_id)?;
        let len = region.tabs.len();
        if len < 2 {
            return None;
        }
        let len_i = isize::try_from(len).unwrap_or(isize::MAX);
        let current = isize::try_from(region.active).unwrap_or(0);
        region.active = usize::try_from((current + delta).rem_euclid(len_i)).unwrap_or(0);
        region.active_session()
    }
}

#[cfg(test)]
mod tests;
