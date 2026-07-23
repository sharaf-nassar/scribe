//! LAN remote window control (feature 014).
//!
//! A Tailscale-free local-network transport that layers onto feature 013's
//! remote window control. Each concern is a focused submodule so discovery,
//! device identity, mutual TLS, and the two trust stores stay separable and
//! independently testable. The listener itself is driven by the
//! `RemoteControl` supervisor in [`crate::ipc_server`], not a parallel accept
//! stack.
//!
//! The focused submodules: [`discovery`] — mDNS advertise/browse (task T005),
//! [`identity`] — the per-install device keypair/cert (task T004), [`network`]
//! — the physical-network trust gate (task T006), [`tls`] — the async
//! mutual-TLS layer with the SPKI-pinning verifiers (task T007), and [`trust`]
//! — the trusted-device store plus the approval state machine (task T008).

pub mod discovery;
pub mod identity;
pub mod network;
pub mod tls;
pub mod trust;
