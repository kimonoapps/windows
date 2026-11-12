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
    /// No answer at all: no connection, the computer asleep or offline. Worth waiting out for hours.
    pub fn unanswered(&self) -> bool {
        self.status == 0
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

/// How long to keep trying after something goes wrong: a week while there's no connection at all
/// (people start a backup and walk away; the Wi-Fi drops, the laptop sleeps), a few minutes for a
/// server that answers with errors, two hours of "busy" (Kimono holding room for other uploads). Start again after any success.
#[derive(Debug)]
pub struct Patience {
    since: Option<std::time::Instant>,
    tries: u32,
}

impl Default for Patience {
    fn default() -> Self { Self::new() }
}

impl Patience {
    pub const OFFLINE_FOR: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

    pub fn new() -> Self { Self { since: None, tries: 0 } }

    pub fn reset(&mut self) { self.since = None; self.tries = 0; }

    /// Waits before the next try, or says to give up (`false`) on this problem.
    pub async fn wait(&mut self, p: &Problem) -> bool {
        if !(p.offline() || p.busy()) { return false; }
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        self.tries += 1;
        let enough = if p.unanswered() { since.elapsed() > Self::OFFLINE_FOR }
            else if p.busy() { since.elapsed() > std::time::Duration::from_secs(2 * 3600) }
            else { self.tries > 8 };
        if enough { return false; }
        let pause = std::time::Duration::from_millis(if p.busy() { 30_000 } else { (500u64 << self.tries.min(7)).min(60_000) });
        /* With no connection, the network coming back ends the wait at once. */
        if p.unanswered() { crate::net::wait_for_network(pause).await; } else { tokio::time::sleep(pause).await; }
        true
    }
}
