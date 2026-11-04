use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("Invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    AuthorizationRejected(String),
}

impl Error {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
