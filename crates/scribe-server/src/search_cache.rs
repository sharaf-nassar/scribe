//! One-snapshot-per-query-burst cache behind `SearchRequest` (spec 017 US8-2).
//!
//! Find-in-scrollback answers every query edit from a full `ScreenSnapshot` of
//! the session's grid — 27.6 MiB at 120x36 and 46.0 MiB at 200x50 with the
//! default 10,000-line scrollback, taken under the `Term` mutex the PTY reader
//! needs for every chunk it feeds. Taking one per keystroke made a 10-character
//! query cost ten of them.
//!
//! The overlay searches a still picture: the scrollback cannot change unless
//! the session produces output or is resized. So the first request of a burst
//! stores its snapshot here and every later edit reuses it, holding the `Term`
//! only long enough to confirm the picture still matches. New output drops the
//! entry from the reader side, and the client releases it when the overlay
//! closes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use scribe_common::screen::ScreenSnapshot;
use std::sync::Arc;

/// Identity of the `Term` state a cached snapshot was taken from.
///
/// `commit` is the session's [`crate::ipc_server::TermCommit`] cursor, which
/// advances past every fed chunk, so an unchanged cursor means no output landed
/// since. The grid shape is carried alongside because a resize rewrites the
/// coordinate space a match is reported in without touching the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotKey {
    /// `TermCommit` value at the moment the snapshot was taken.
    pub commit: u64,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_rows: u32,
}

/// The scrollback snapshot the find overlay's current query burst is reading.
///
/// Every accessor is a short, non-blocking critical section over a
/// `std::sync::Mutex`; callers hold the session's `Term` lock across `get` and
/// `store` so the key they validate against cannot change underneath them.
#[derive(Default)]
pub struct SearchSnapshotCache {
    /// Mirrors "`slot` is `Some`" so the PTY reader can skip the mutex entirely
    /// on the overwhelmingly common path where no overlay is open.
    populated: AtomicBool,
    slot: Mutex<Option<(SnapshotKey, Arc<ScreenSnapshot>)>>,
}

impl SearchSnapshotCache {
    /// The cached snapshot, if it was taken from exactly this `Term` state.
    ///
    /// A key mismatch discards the entry: the picture it holds is stale and no
    /// later request can want it back.
    pub fn get(&self, key: SnapshotKey) -> Option<Arc<ScreenSnapshot>> {
        if !self.populated.load(Ordering::Relaxed) {
            return None;
        }
        let mut slot = self.slot.lock().ok()?;
        match slot.as_ref() {
            Some((cached, snapshot)) if *cached == key => Some(Arc::clone(snapshot)),
            Some(_) => {
                *slot = None;
                self.populated.store(false, Ordering::Relaxed);
                None
            }
            None => None,
        }
    }

    /// Keep `snapshot` for the rest of this query burst.
    pub fn store(&self, key: SnapshotKey, snapshot: Arc<ScreenSnapshot>) {
        let Ok(mut slot) = self.slot.lock() else { return };
        *slot = Some((key, snapshot));
        self.populated.store(true, Ordering::Relaxed);
    }

    /// Drop the cached snapshot and its allocation.
    ///
    /// Called from the PTY reader on every fed chunk and from the client's
    /// `SearchClosed`, so the memory is released as soon as the picture goes
    /// stale rather than lingering until the next query.
    pub fn invalidate(&self) {
        if !self.populated.load(Ordering::Relaxed) {
            return;
        }
        self.populated.store(false, Ordering::Relaxed);
        if let Ok(mut slot) = self.slot.lock() {
            *slot = None;
        }
    }
}

/// Shared handle on a session's [`SearchSnapshotCache`], held by the registry
/// entry, the PTY reader (which invalidates it), and the search handler.
pub type SessionSearchCache = Arc<SearchSnapshotCache>;
