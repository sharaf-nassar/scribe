//! Library entry point for scribe-server integration tests.
//!
//! Re-exports the internal modules that integration tests rely on. The binary
//! entry point remains `main.rs`; this `lib.rs` exists alongside it so tests
//! under `tests/` can link against the crate's internals without going
//! through the binary.

pub mod agent_api;
pub mod attach_flow;
pub mod beads_board;
pub mod child_identity;
pub mod child_watch;
pub mod clipboard_state;
pub mod config;
pub mod env_store;
pub mod git_ref_watcher;
pub mod github_ci;
pub mod handoff;
pub mod hook_ingress;
pub mod image_sharing_probe;
pub mod ipc_server;
pub mod lan;
pub mod macos_proc;
pub mod pty_guard;
pub mod releases;
pub mod search_cache;
pub mod session_exit;
pub mod session_manager;
pub mod shell_integration;
pub mod state_dump;
pub mod stop_classifier;
pub mod tailnet;
pub mod terminal_image_handoff;
pub mod terminal_image_mutations;
pub mod terminal_image_publication;
pub mod terminal_image_replay;
pub mod terminal_image_sharing;
pub mod terminal_image_state;
pub mod updater;
pub mod workspace_manager;
