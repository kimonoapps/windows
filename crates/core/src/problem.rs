/// A refusal or failure a person can read. `status` is the HTTP status, or 0 when Kimono couldn't be reached.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct Problem {
    pub message: String,
    pub status: u16,
}

impl Problem {
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self { message: message.into(), status }
    }
    pub fn say(message: impl Into<String>) -> Self {
        Self::new(message, 400)
    }
    pub fn unreachable() -> Self {
        Self::new("Kimono isn't answering. Try again in a moment.", 0)
    }
    /// Not reached at all (no connection, a gateway's error, a 5xx): worth trying again later.
    pub fn offline(&self) -> bool {
        self.status == 0 || self.status >= 500
    }
    /// Kimono is holding room for this person's other uploads, or rate limiting: wait and try again.
    pub fn busy(&self) -> bool {
        self.status == 429
    }
}

impl From<reqwest::Error> for Problem {
    fn from(_: reqwest::Error) -> Self {
        Self::unreachable()
    }
}

impl From<std::io::Error> for Problem {
    fn from(e: std::io::Error) -> Self {
        Self::new(format!("It couldn't be read: {e}"), 400)
    }
}

pub type Result<T, E = Problem> = std::result::Result<T, E>;
