// ABOUTME: Error types for seren-acp-claude
// ABOUTME: Unified error handling for Claude and ACP operations

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Claude connection error: {0}")]
    ClaudeConnection(String),

    #[error("Claude query error: {0}")]
    ClaudeQuery(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SDK error: {0}")]
    Sdk(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<claude_agent_sdk_rs::ClaudeError> for Error {
    fn from(e: claude_agent_sdk_rs::ClaudeError) -> Self {
        Error::Sdk(e.to_string())
    }
}
