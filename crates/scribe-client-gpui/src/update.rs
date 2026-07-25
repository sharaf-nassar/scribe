//! Update availability and progress for the terminal window.
//!
//! The server owns the whole update lifecycle: it broadcasts
//! `ServerMessage::UpdateAvailable` when a newer release appears and
//! `ServerMessage::UpdateProgress` for every download / verify / install
//! transition. The client's job is to hold the latest of each, render the
//! centred status-bar CTA from them, and send `ClientMessage::TriggerUpdate` or
//! `ClientMessage::DismissUpdate` back when the user acts on that CTA.
//!
//! [`UpdateState`] is that holder, kept display-independent so the shell can
//! share one behind a mutex between the IPC reader thread (which writes it) and
//! the GPUI view (which reads it every redraw). The transitions mirror the
//! winit client's `handle_update_available` / `handle_update_progress` /
//! `open_update_dialog` trio verbatim so the CTA and the confirmation modal
//! behave identically across the cutover.

use scribe_common::protocol::UpdateProgressState;

use crate::dialog::{AnyDialog, UpdateDialog};

/// The latest update announcement and the latest install progress the server
/// has broadcast, as the terminal window knows them.
///
/// Both halves are independent: `UpdateAvailable` arms the CTA, `UpdateProgress`
/// overrides its label while an install runs. A progress state is deliberately
/// *not* cleared by a later `UpdateAvailable`, matching the winit client — the
/// server only re-announces a version it still considers installable.
#[derive(Debug, Clone, Default)]
pub struct UpdateState {
    version: Option<String>,
    release_url: Option<String>,
    progress: Option<UpdateProgressState>,
}

impl UpdateState {
    /// Record a `ServerMessage::UpdateAvailable` broadcast.
    pub fn on_available(&mut self, version: String, release_url: String) {
        self.version = Some(version);
        self.release_url = Some(release_url);
    }

    /// Record a `ServerMessage::UpdateProgress` broadcast.
    pub fn on_progress(&mut self, state: UpdateProgressState) {
        self.progress = Some(state);
    }

    /// Version string for the pending update, feeding
    /// [`StatusBarData::update_available`](crate::status_bar::StatusBarData).
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Release page for the pending update, retained for the "what's new" link
    /// the settings window already renders from `ReleaseList`.
    #[must_use]
    pub fn release_url(&self) -> Option<&str> {
        self.release_url.as_deref()
    }

    /// In-flight install progress, feeding
    /// [`StatusBarData::update_progress`](crate::status_bar::StatusBarData).
    #[must_use]
    pub fn progress(&self) -> Option<&UpdateProgressState> {
        self.progress.as_ref()
    }

    /// The confirmation modal the centred CTA opens, or `None` when there is
    /// nothing to confirm.
    ///
    /// A completed-but-restart-required install outranks a pending version, so
    /// the user is asked about the cold restart they already paid for rather
    /// than about downloading again — the same precedence the winit
    /// `open_update_dialog` applies.
    #[must_use]
    pub fn confirmation(&self) -> Option<AnyDialog> {
        match (&self.progress, &self.version) {
            (Some(UpdateProgressState::CompletedRestartRequired { version }), _) => {
                Some(AnyDialog::Update(UpdateDialog::new_restart_required(version.clone())))
            }
            (_, Some(version)) => {
                Some(AnyDialog::Update(UpdateDialog::new_install(version.clone())))
            }
            _ => None,
        }
    }

    /// Clear the pending announcement because the user confirmed the install.
    ///
    /// The CTA immediately stops offering the update; the server's first
    /// `UpdateProgress` takes over the label a moment later.
    pub fn on_triggered(&mut self) {
        self.version = None;
        self.release_url = None;
    }

    /// Clear everything because the user dismissed the notification. The server
    /// suppresses re-notification for this version, so the CTA must not come
    /// back on its own.
    pub fn on_dismissed(&mut self) {
        self.version = None;
        self.release_url = None;
        self.progress = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog::{DialogOutcome, UpdateAction, UpdateDialogKind};

    fn kind(dialog: &AnyDialog) -> UpdateDialogKind {
        match dialog {
            AnyDialog::Update(update) => update.kind(),
            _ => panic!("expected an update dialog"),
        }
    }

    // @lat: [[test#GPUI Client Headless Suites#GPUI Update Surfaces#Server broadcasts arm the status bar CTA]]
    #[test]
    fn server_broadcasts_arm_the_status_bar_cta() {
        let mut state = UpdateState::default();
        assert_eq!(state.version(), None);
        assert!(state.progress().is_none());
        assert!(state.confirmation().is_none());

        state.on_available("2.0.0".to_owned(), "https://example.test/v2".to_owned());
        assert_eq!(state.version(), Some("2.0.0"));
        assert_eq!(state.release_url(), Some("https://example.test/v2"));
        assert_eq!(kind(&state.confirmation().unwrap()), UpdateDialogKind::InstallAvailable);

        state.on_progress(UpdateProgressState::Downloading);
        assert!(matches!(state.progress(), Some(UpdateProgressState::Downloading)));
        // A later announcement never clears in-flight progress.
        state.on_available("2.0.1".to_owned(), "https://example.test/v201".to_owned());
        assert!(matches!(state.progress(), Some(UpdateProgressState::Downloading)));
    }

    // @lat: [[test#GPUI Client Headless Suites#GPUI Update Surfaces#Restart-required outranks a pending version]]
    #[test]
    fn restart_required_outranks_a_pending_version() {
        let mut state = UpdateState::default();
        state.on_available("2.0.0".to_owned(), "https://example.test/v2".to_owned());
        state
            .on_progress(UpdateProgressState::CompletedRestartRequired { version: "2.0.0".into() });

        let dialog = state.confirmation().unwrap();
        assert_eq!(kind(&dialog), UpdateDialogKind::RestartRequired);
        // The safe action is still "Cancel", so Esc never cold-restarts.
        assert_eq!(dialog.cancel(), DialogOutcome::Update(UpdateAction::Secondary));
    }

    // @lat: [[test#GPUI Client Headless Suites#GPUI Update Surfaces#Trigger and dismiss clear the CTA]]
    #[test]
    fn trigger_and_dismiss_clear_the_cta() {
        let mut triggered = UpdateState::default();
        triggered.on_available("2.0.0".to_owned(), "https://example.test/v2".to_owned());
        triggered.on_triggered();
        assert_eq!(triggered.version(), None);
        assert_eq!(triggered.release_url(), None);
        assert!(triggered.confirmation().is_none());

        let mut dismissed = UpdateState::default();
        dismissed.on_available("2.0.0".to_owned(), "https://example.test/v2".to_owned());
        dismissed.on_progress(UpdateProgressState::Failed { reason: "network".to_owned() });
        dismissed.on_dismissed();
        assert_eq!(dismissed.version(), None);
        assert!(dismissed.progress().is_none());
        assert!(dismissed.confirmation().is_none());
    }
}
