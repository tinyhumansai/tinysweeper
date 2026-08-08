//! The server's database.
//!
//! Embedded SQLite, one file. This is the thing a stateless workflow could not
//! have: a contributor is a fact about a *person over time*, not about one pull
//! request, and a whitelist that resets on every run is not a whitelist.
//!
//! What lives here is deliberately narrow — identity, trust and delivery
//! bookkeeping. Review state stays on GitHub in the durable comment's markers,
//! so losing this database costs the trust decisions and nothing else.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

/// How much a contributor is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Never seen before. Reviewed, but nothing is auto-applied.
    Unknown,
    /// Explicitly allowed. Reviews run normally.
    Allowed,
    /// Explicitly refused. No review runs, and nothing is posted.
    Blocked,
}

impl Trust {
    fn as_str(self) -> &'static str {
        match self {
            Trust::Unknown => "unknown",
            Trust::Allowed => "allowed",
            Trust::Blocked => "blocked",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "allowed" => Trust::Allowed,
            "blocked" => Trust::Blocked,
            _ => Trust::Unknown,
        }
    }
}

/// A person whose pull requests we have seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributor {
    /// GitHub login.
    pub login: String,
    /// How much they are trusted.
    pub trust: Trust,
    /// How many pull requests of theirs have been reviewed.
    pub reviews: u64,
    /// How many findings were raised against them.
    pub findings: u64,
    /// How many of those findings they dismissed with a 👎.
    ///
    /// A high ratio here is the signal that the bot is being noisy *at this
    /// person*, which is more actionable than a global average.
    pub dismissed: u64,
    /// Free-text note explaining a manual trust decision.
    pub note: Option<String>,
}

/// The server's persistent state.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

impl Store {
    /// Open, or create, the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .map_err(|err| Error::path(path, format!("could not open the database: {err}")))?;
        Self::from_connection(connection)
    }

    /// An in-memory database, for tests.
    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()
            .map_err(|err| Error::Forge(format!("could not open an in-memory database: {err}")))?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.connection.lock().expect("store lock");
        connection
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA foreign_keys = ON;

                CREATE TABLE IF NOT EXISTS contributors (
                    login     TEXT PRIMARY KEY,
                    trust     TEXT NOT NULL DEFAULT 'unknown',
                    reviews   INTEGER NOT NULL DEFAULT 0,
                    findings  INTEGER NOT NULL DEFAULT 0,
                    dismissed INTEGER NOT NULL DEFAULT 0,
                    note      TEXT
                );

                CREATE TABLE IF NOT EXISTS installations (
                    id         INTEGER PRIMARY KEY,
                    account    TEXT NOT NULL,
                    suspended  INTEGER NOT NULL DEFAULT 0
                );

                -- Webhook deliveries are recorded so a redelivery — which GitHub
                -- does routinely on timeout — cannot cause a second review of
                -- the same event.
                CREATE TABLE IF NOT EXISTS deliveries (
                    id         TEXT PRIMARY KEY,
                    event      TEXT NOT NULL,
                    received   INTEGER NOT NULL
                );

                -- A lease per (repo, pr, head sha) so two concurrent deliveries
                -- for the same push cannot both review it.
                CREATE TABLE IF NOT EXISTS leases (
                    key        TEXT PRIMARY KEY,
                    holder     TEXT NOT NULL,
                    taken      INTEGER NOT NULL
                );
                "#,
            )
            .map_err(|err| Error::Forge(format!("migration failed: {err}")))?;
        Ok(())
    }

    /// Look a contributor up, or return an unknown one.
    pub fn contributor(&self, login: &str) -> Result<Contributor> {
        let connection = self.connection.lock().expect("store lock");
        let found = connection
            .query_row(
                "SELECT trust, reviews, findings, dismissed, note \
                 FROM contributors WHERE login = ?1",
                params![login],
                |row| {
                    Ok(Contributor {
                        login: login.to_string(),
                        trust: Trust::parse(&row.get::<_, String>(0)?),
                        reviews: row.get(1)?,
                        findings: row.get(2)?,
                        dismissed: row.get(3)?,
                        note: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(|err| Error::Forge(err.to_string()))?;

        Ok(found.unwrap_or(Contributor {
            login: login.to_string(),
            trust: Trust::Unknown,
            reviews: 0,
            findings: 0,
            dismissed: 0,
            note: None,
        }))
    }

    /// Set a contributor's trust, with a note explaining why.
    pub fn set_trust(&self, login: &str, trust: Trust, note: Option<&str>) -> Result<()> {
        let connection = self.connection.lock().expect("store lock");
        connection
            .execute(
                "INSERT INTO contributors (login, trust, note) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(login) DO UPDATE SET trust = ?2, note = ?3",
                params![login, trust.as_str(), note],
            )
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// Record that a review happened.
    pub fn record_review(&self, login: &str, findings: u64) -> Result<()> {
        let connection = self.connection.lock().expect("store lock");
        connection
            .execute(
                "INSERT INTO contributors (login, reviews, findings) VALUES (?1, 1, ?2) \
                 ON CONFLICT(login) DO UPDATE SET \
                   reviews = reviews + 1, findings = findings + ?2",
                params![login, findings],
            )
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// Record that a finding was dismissed.
    pub fn record_dismissal(&self, login: &str) -> Result<()> {
        let connection = self.connection.lock().expect("store lock");
        connection
            .execute(
                "INSERT INTO contributors (login, dismissed) VALUES (?1, 1) \
                 ON CONFLICT(login) DO UPDATE SET dismissed = dismissed + 1",
                params![login],
            )
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// Claim a delivery id, returning false if it was already seen.
    ///
    /// GitHub redelivers on timeout, and a redelivery must not cause a second
    /// review of the same event — that is duplicate comments and duplicate
    /// spend for no new information.
    pub fn claim_delivery(&self, id: &str, event: &str, now: i64) -> Result<bool> {
        let connection = self.connection.lock().expect("store lock");
        let inserted = connection
            .execute(
                "INSERT OR IGNORE INTO deliveries (id, event, received) VALUES (?1, ?2, ?3)",
                params![id, event, now],
            )
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(inserted == 1)
    }

    /// Take a lease, returning false if someone else holds it.
    pub fn claim_lease(&self, key: &str, holder: &str, now: i64) -> Result<bool> {
        let connection = self.connection.lock().expect("store lock");
        let inserted = connection
            .execute(
                "INSERT OR IGNORE INTO leases (key, holder, taken) VALUES (?1, ?2, ?3)",
                params![key, holder, now],
            )
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(inserted == 1)
    }

    /// Release a lease.
    pub fn release_lease(&self, key: &str) -> Result<()> {
        let connection = self.connection.lock().expect("store lock");
        connection
            .execute("DELETE FROM leases WHERE key = ?1", params![key])
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// Record an installation.
    pub fn record_installation(&self, id: u64, account: &str) -> Result<()> {
        let connection = self.connection.lock().expect("store lock");
        connection
            .execute(
                "INSERT INTO installations (id, account) VALUES (?1, ?2) \
                 ON CONFLICT(id) DO UPDATE SET account = ?2, suspended = 0",
                params![id, account],
            )
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// How many installations are known.
    pub fn installation_count(&self) -> Result<u64> {
        let connection = self.connection.lock().expect("store lock");
        connection
            .query_row("SELECT COUNT(*) FROM installations", [], |row| row.get(0))
            .map_err(|err| Error::Forge(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unseen_contributor_is_unknown_rather_than_missing() {
        let store = Store::in_memory().expect("opens");
        let who = store.contributor("newcomer").expect("reads");

        assert_eq!(who.trust, Trust::Unknown);
        assert_eq!(who.reviews, 0);
    }

    #[test]
    fn trust_survives_and_carries_its_reason() {
        let store = Store::in_memory().expect("opens");
        store
            .set_trust("someone", Trust::Blocked, Some("spam pull requests"))
            .expect("writes");

        let who = store.contributor("someone").expect("reads");
        assert_eq!(who.trust, Trust::Blocked);
        assert_eq!(who.note.as_deref(), Some("spam pull requests"));
    }

    #[test]
    fn review_counts_accumulate_across_pull_requests() {
        // The whole point of the database: this is a fact about a person over
        // time, which a stateless workflow has nowhere to keep.
        let store = Store::in_memory().expect("opens");
        store.record_review("someone", 2).expect("writes");
        store.record_review("someone", 3).expect("writes");

        let who = store.contributor("someone").expect("reads");
        assert_eq!(who.reviews, 2);
        assert_eq!(who.findings, 5);
    }

    #[test]
    fn recording_a_review_does_not_reset_trust() {
        let store = Store::in_memory().expect("opens");
        store
            .set_trust("someone", Trust::Allowed, None)
            .expect("writes");
        store.record_review("someone", 1).expect("writes");

        assert_eq!(store.contributor("someone").expect("reads").trust, Trust::Allowed);
    }

    #[test]
    fn dismissals_are_tracked_separately_from_findings() {
        let store = Store::in_memory().expect("opens");
        store.record_review("someone", 4).expect("writes");
        store.record_dismissal("someone").expect("writes");
        store.record_dismissal("someone").expect("writes");

        let who = store.contributor("someone").expect("reads");
        assert_eq!(who.findings, 4);
        assert_eq!(who.dismissed, 2);
    }

    #[test]
    fn a_redelivered_webhook_is_claimed_only_once() {
        // GitHub redelivers on timeout. A second review of the same event is
        // duplicate comments and duplicate spend for no new information.
        let store = Store::in_memory().expect("opens");
        assert!(store.claim_delivery("abc", "pull_request", 1).expect("claims"));
        assert!(!store.claim_delivery("abc", "pull_request", 2).expect("claims"));
    }

    #[test]
    fn a_lease_is_held_by_one_worker_and_released() {
        let store = Store::in_memory().expect("opens");
        assert!(store.claim_lease("repo#7@abc", "worker-1", 1).expect("claims"));
        assert!(!store.claim_lease("repo#7@abc", "worker-2", 1).expect("claims"));

        store.release_lease("repo#7@abc").expect("releases");
        assert!(store.claim_lease("repo#7@abc", "worker-2", 2).expect("claims"));
    }

    #[test]
    fn installations_are_recorded_idempotently() {
        let store = Store::in_memory().expect("opens");
        store.record_installation(152184043, "tinyhumansai").expect("writes");
        store.record_installation(152184043, "tinyhumansai").expect("writes");

        assert_eq!(store.installation_count().expect("counts"), 1);
    }
}
