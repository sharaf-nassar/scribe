//! Find-in-scrollback overlay state.
//!
//! Tracks whether the search overlay is visible, the current query text,
//! match results received from the server, and which match is highlighted.
//! This module is pure state -- it does no rendering.

#![allow(
    dead_code,
    reason = "search overlay module is new; methods will be called once rendering is wired"
)]

use scribe_common::protocol::SearchMatch;

/// UI state for the find-in-scrollback overlay.
pub struct SearchOverlay {
    active: bool,
    query: String,
    matches: Vec<SearchMatch>,
    current_match: usize,
}

impl SearchOverlay {
    /// Creates a new inactive overlay with empty state.
    pub fn new() -> Self {
        Self { active: false, query: String::new(), matches: Vec::new(), current_match: 0 }
    }

    /// Opens the overlay, clearing any previous query and results.
    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.matches.clear();
        self.current_match = 0;
    }

    /// Closes the overlay and resets all state.
    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.current_match = 0;
    }

    /// Appends a character to the search query.
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
    }

    /// Removes the last character from the search query, if any.
    pub fn pop_char(&mut self) {
        self.query.pop();
    }

    /// Returns the current search query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Returns whether the overlay is currently visible.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Replaces the match results and resets the highlight to the first match.
    pub fn set_results(&mut self, matches: Vec<SearchMatch>) {
        self.matches = matches;
        self.current_match = 0;
    }

    /// Advances to the next match, wrapping around to the first.
    /// No-op when there are no matches.
    pub fn next_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current_match = (self.current_match + 1) % self.matches.len();
    }

    /// Goes back to the previous match, wrapping around to the last.
    /// No-op when there are no matches.
    pub fn prev_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let count = self.matches.len();
        self.current_match = (self.current_match + count - 1) % count;
    }

    /// Returns the index of the currently highlighted match.
    pub fn current_match_index(&self) -> usize {
        self.current_match
    }

    /// Returns the total number of matches.
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }
}
