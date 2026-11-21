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

/// A status code as a standalone token (not inside a hex key such as `.../ab429f...`).
fn has_code(t: &str, code: &str) -> bool {
    let b = t.as_bytes();
    t.match_indices(code).any(|(i, m)| {
        let before = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let j = i + m.len();
        let after = j >= b.len() || !b[j].is_ascii_alphanumeric();
        before && after
    })
}

/// True when an error text from rclone or a store signals throttling (429, 503, B2 too_busy).
pub fn looks_throttled(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    has_code(&t, "429")
        || has_code(&t, "503")
        || t.contains("too many requests")
        || t.contains("too_busy")
        || t.contains("slowdown")
        || t.contains("slow down")
        || t.contains("service unavailable")
        || t.contains("service_unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn throttle_text_is_matched_on_tokens_not_inside_keys() {
        assert!(looks_throttled("HTTP 429: too many"));
        assert!(looks_throttled("status 503"));
        assert!(looks_throttled("b2: too_busy (429)"));
        assert!(looks_throttled("SlowDown: please reduce your request rate"));
        assert!(!looks_throttled(
            "failed to upload v1/packs/ab/ab429f03c5031e.pack: corrupted"
        ));
        assert!(!looks_throttled("object 5034290 not found"));
    }
}
