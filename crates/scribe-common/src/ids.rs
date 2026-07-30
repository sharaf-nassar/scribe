use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Generate a UUID-based newtype ID with `new`, `as_uuid`, `to_full_string`,
/// `Default`, `Display` (with a short prefix), and `FromStr`.
macro_rules! define_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            #[must_use]
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }

            /// Returns the full UUID string (for CLI serialization).
            #[must_use]
            pub fn to_full_string(self) -> String {
                self.0.to_string()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let s = self.0.to_string();
                let prefix = s.get(..8).unwrap_or("unknown");
                write!(f, concat!($prefix, "-{}"), prefix)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let uuid = Uuid::parse_str(s)?;
                Ok(Self(uuid))
            }
        }
    };
}

define_id!(SessionId, "session");
define_id!(WorkspaceId, "ws");
define_id!(WindowId, "win");

/// Mint a launch id — the identifier a client sends as
/// `ClientMessage::CreateSession::env_envelope_id` and persists as its
/// `LaunchRecord.launch_id`.
///
/// Every create path mints one, because it is what names the session's
/// `<state_dir>/restore/env/<window_id>/<launch_id>.envz` envelope: a create
/// that omits it can never persist its environment. Launch ids stay bare
/// strings rather than a newtype because the wire, the on-disk snapshot, and
/// the envelope filename all carry them as text.
#[must_use]
pub fn new_launch_id() -> String {
    Uuid::new_v4().to_string()
}
