use crate::ids::SessionId;

#[derive(Debug, thiserror::Error)]
pub enum ScribeError {
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    #[error("workspace not found: {workspace_id}")]
    WorkspaceNotFound { workspace_id: String },

    #[error("PTY spawn failed: {reason}")]
    PtySpawnFailed { reason: String },

    #[error("IPC error: {reason}")]
    IpcError { reason: String },

    #[error("protocol error: {reason}")]
    ProtocolError { reason: String },

    #[error("config error: {reason}")]
    ConfigError { reason: String },

    #[error("theme parse error: {reason}")]
    ThemeParse { reason: String },

    #[error("IO error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    #[error("serialization error: {source}")]
    Serialization {
        #[from]
        source: rmp_serde::encode::Error,
    },

    #[error("deserialization error: {source}")]
    Deserialization {
        #[from]
        source: rmp_serde::decode::Error,
    },
}
