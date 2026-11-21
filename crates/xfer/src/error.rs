use std::fmt;

/// Engine errors. `Unverifiable` and `Mismatch` are the verification failures: no receipt is ever
/// issued for data whose upload ended in either.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport: {0}")]
    Transport(String),
    #[error("throttled: {0}")]
    Throttled(String),
    #[error("unverifiable upload of {key}: {why}")]
    Unverifiable { key: String, why: String },
    #[error("checksum mismatch for {key}: {why}")]
    Mismatch { key: String, why: String },
    #[error("corrupt data: {0}")]
    Corrupt(String),
    #[error("refused: {0}")]
    Refused(String),
    #[error("over budget: {0}")]
    Budget(String),
    #[error("internal error: {0}")]
    Bug(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Errors worth another attempt of the same request.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Error::Transport(_) | Error::Throttled(_) | Error::Mismatch { .. } | Error::Io(_)
        )
    }

    pub fn is_throttle(&self) -> bool {
        matches!(self, Error::Throttled(_))
    }

    pub fn corrupt(what: impl fmt::Display) -> Error {
        Error::Corrupt(what.to_string())
    }
}

/// True when an error text from rclone or a store signals throttling (429, 503, B2 too_busy).
pub fn looks_throttled(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("429")
        || t.contains("503")
        || t.contains("too many requests")
        || t.contains("too_busy")
        || t.contains("slowdown")
        || t.contains("slow down")
        || t.contains("service unavailable")
        || t.contains("service_unavailable")
}
