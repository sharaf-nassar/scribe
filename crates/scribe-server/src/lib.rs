//! Library entry point for scribe-server integration tests.
//!
//! Re-exports the internal modules that integration tests rely on. The binary
//! entry point remains `main.rs`; this `lib.rs` exists alongside it so tests
//! under `tests/` can link against the crate's internals without going
//! through the binary.

pub mod attach_flow;
pub mod config;
pub mod env_store;
pub mod handoff;
pub mod hook_ingress;
pub mod ipc_server;
pub mod macos_proc;
pub mod releases;
pub mod session_manager;
pub mod shell_integration;
pub mod stop_classifier;
pub mod updater;
pub mod workspace_manager;
pub mod workspace_notes;
