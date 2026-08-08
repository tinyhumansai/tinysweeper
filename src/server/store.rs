//! The server's database, on MongoDB.
//!
//! This is the thing a stateless workflow could not have: a contributor is a
//! fact about a *person over time*, not about one pull request, and a whitelist
//! that resets on every run is not a whitelist.
//!
//! What lives here is deliberately narrow — identity, trust, and delivery
//! bookkeeping. Review state stays on GitHub in the durable comment's markers,
//! so losing this database costs the trust decisions and nothing else. That is
//! a design choice worth keeping: it means the database is never the thing
//! standing between a pull request and its review.
//!
//! Two collections carry unique indexes and are used for *claiming* rather than
//! storing: `deliveries` makes a redelivered webhook a no-op, and `leases` stops
//! two workers reviewing the same push. Both rely on the duplicate-key error
//! being the answer rather than an exception.

use bson::{Document, doc};
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, IndexModel};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Environment variable naming the test database. Absent means the store tests
/// skip, which is what keeps `cargo test` offline by default.
pub const TEST_URI_ENV: &str = "TINYSWEEPER_TEST_MONGODB_URI";

/// How much a contributor is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// Never seen before. Reviewed, but nothing is auto-applied.
    Unknown,
    /// Explicitly allowed. Reviews run normally.
    Allowed,
    /// Explicitly refused. No review runs, and nothing is posted.
    Blocked,
}

/// A person whose pull requests we have seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contributor {
    /// GitHub login. The document `_id`.
    #[serde(rename = "_id")]
    pub login: String,
    /// How much they are trusted.
    #[serde(default = "unknown_trust")]
    pub trust: Trust,
    /// How many of their pull requests have been reviewed.
    #[serde(default)]
    pub reviews: u64,
    /// How many findings were raised against them.
    #[serde(default)]
    pub findings: u64,
    /// How many of those findings they dismissed with a 👎.
    ///
    /// A high ratio here says the bot is being noisy *at this person*, which is
    /// far more actionable than a global average.
    #[serde(default)]
    pub dismissed: u64,
    /// Why a manual trust decision was made.
    #[serde(default)]
    pub note: Option<String>,
}

fn unknown_trust() -> Trust {
    Trust::Unknown
}

impl Contributor {
    /// A contributor we have never seen.
    pub fn unseen(login: &str) -> Self {
        Self {
            login: login.to_string(),
            trust: Trust::Unknown,
            reviews: 0,
            findings: 0,
            dismissed: 0,
            note: None,
        }
    }

    /// The share of findings this person waved away.
    ///
    /// `None` when there is nothing to divide by — which matters, because
    /// reporting 0% for someone who has never had a finding would read as
    /// "never disagrees" rather than "no data".
    pub fn dismissal_rate(&self) -> Option<f64> {
        (self.findings > 0).then(|| self.dismissed as f64 / self.findings as f64)
    }
}

/// The server's persistent state.
#[derive(Clone, Debug)]
pub struct Store {
    contributors: Collection<Contributor>,
    deliveries: Collection<Document>,
    leases: Collection<Document>,
    installations: Collection<Document>,
}

impl Store {
    /// Connect to `uri` and use database `name`.
    pub async fn connect(uri: &str, name: &str) -> Result<Self> {
        let client = Client::with_uri_str(uri)
            .await
            .map_err(|err| Error::Forge(format!("could not reach MongoDB: {err}")))?;
        let database = client.database(name);

        let store = Self {
            contributors: database.collection("contributors"),
            deliveries: database.collection("deliveries"),
            leases: database.collection("leases"),
            installations: database.collection("installations"),
        };
        store.ensure_indexes().await?;
        Ok(store)
    }

    /// Connect using `TINYSWEEPER_MONGODB_URI`.
    pub async fn from_env() -> Result<Self> {
        let uri = std::env::var("TINYSWEEPER_MONGODB_URI")
            .map_err(|_| Error::Forge("TINYSWEEPER_MONGODB_URI is not set".into()))?;
        let name =
            std::env::var("TINYSWEEPER_MONGODB_DB").unwrap_or_else(|_| "tinysweeper".to_string());
        Self::connect(&uri, &name).await
    }

    async fn ensure_indexes(&self) -> Result<()> {
        let unique = IndexOptions::builder().unique(true).build();

        // The whole claim mechanism rests on these being unique: without them a
        // redelivered webhook quietly reviews twice.
        self.deliveries
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "delivery": 1 })
                    .options(unique.clone())
                    .build(),
            )
            .await
            .map_err(|err| Error::Forge(format!("could not index deliveries: {err}")))?;

        self.leases
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "key": 1 })
                    .options(unique)
                    .build(),
            )
            .await
            .map_err(|err| Error::Forge(format!("could not index leases: {err}")))?;

        // Deliveries are bookkeeping, not history. Expiring them keeps the
        // collection from growing without bound, and a delivery old enough to
        // expire is old enough that a redelivery is a new event anyway.
        let ttl = IndexOptions::builder()
            .expire_after(std::time::Duration::from_secs(7 * 24 * 60 * 60))
            .build();
        self.deliveries
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "received": 1 })
                    .options(ttl)
                    .build(),
            )
            .await
            .map_err(|err| Error::Forge(format!("could not set the delivery TTL: {err}")))?;

        Ok(())
    }

    /// Look a contributor up, or return an unseen one.
    pub async fn contributor(&self, login: &str) -> Result<Contributor> {
        let found = self
            .contributors
            .find_one(doc! { "_id": login })
            .await
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(found.unwrap_or_else(|| Contributor::unseen(login)))
    }

    /// Set a contributor's trust, with a note explaining why.
    pub async fn set_trust(&self, login: &str, trust: Trust, note: Option<&str>) -> Result<()> {
        let trust = bson::to_bson(&trust).map_err(|err| Error::Forge(err.to_string()))?;
        self.contributors
            .update_one(
                doc! { "_id": login },
                doc! { "$set": { "trust": trust, "note": note } },
            )
            .upsert(true)
            .await
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// Record that a review happened.
    ///
    /// `$inc` with an upsert, so this never clobbers a trust decision made
    /// elsewhere — the counters and the trust are written by different code
    /// paths and must not race each other.
    pub async fn record_review(&self, login: &str, findings: u64) -> Result<()> {
        self.contributors
            .update_one(
                doc! { "_id": login },
                doc! { "$inc": { "reviews": 1_i64, "findings": findings as i64 } },
            )
            .upsert(true)
            .await
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// Record that a finding was dismissed.
    pub async fn record_dismissal(&self, login: &str) -> Result<()> {
        self.contributors
            .update_one(doc! { "_id": login }, doc! { "$inc": { "dismissed": 1_i64 } })
            .upsert(true)
            .await
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// Claim a delivery id, returning false if it was already seen.
    ///
    /// GitHub redelivers on timeout, and a redelivery must not cause a second
    /// review: that is duplicate comments and duplicate spend for no new
    /// information.
    pub async fn claim_delivery(&self, delivery: &str, event: &str) -> Result<bool> {
        let now = bson::DateTime::now();
        match self
            .deliveries
            .insert_one(doc! { "delivery": delivery, "event": event, "received": now })
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_duplicate_key(&err) => Ok(false),
            Err(err) => Err(Error::Forge(err.to_string())),
        }
    }

    /// Take a lease, returning false if someone else holds it.
    pub async fn claim_lease(&self, key: &str, holder: &str) -> Result<bool> {
        let now = bson::DateTime::now();
        match self
            .leases
            .insert_one(doc! { "key": key, "holder": holder, "taken": now })
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_duplicate_key(&err) => Ok(false),
            Err(err) => Err(Error::Forge(err.to_string())),
        }
    }

    /// Release a lease.
    pub async fn release_lease(&self, key: &str) -> Result<()> {
        self.leases
            .delete_one(doc! { "key": key })
            .await
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// Record an installation.
    pub async fn record_installation(&self, id: u64, account: &str) -> Result<()> {
        self.installations
            .update_one(
                doc! { "_id": id as i64 },
                doc! { "$set": { "account": account, "suspended": false } },
            )
            .upsert(true)
            .await
            .map_err(|err| Error::Forge(err.to_string()))?;
        Ok(())
    }

    /// How many installations are known.
    pub async fn installation_count(&self) -> Result<u64> {
        self.installations
            .count_documents(doc! {})
            .await
            .map_err(|err| Error::Forge(err.to_string()))
    }

    /// Whether the database is reachable.
    pub async fn healthy(&self) -> bool {
        self.installations.count_documents(doc! {}).await.is_ok()
    }
}

/// Whether an error is a unique-index violation.
///
/// This is the *expected* outcome of a claim, not a failure, which is why it is
/// matched rather than propagated.
fn is_duplicate_key(err: &mongodb::error::Error) -> bool {
    matches!(
        *err.kind,
        mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(
            mongodb::error::WriteError { code: 11000, .. }
        ))
    ) || err.to_string().contains("E11000")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connect to the test database, or skip.
    ///
    /// The offline-by-default invariant applies to the test suite too: without
    /// `TINYSWEEPER_TEST_MONGODB_URI` these skip rather than fail, so
    /// `cargo test` still needs no network. CI sets it against a service
    /// container.
    async fn store() -> Option<Store> {
        let uri = std::env::var(TEST_URI_ENV).ok()?;
        let name = format!("tinysweeper_test_{}", std::process::id());
        let store = Store::connect(&uri, &name).await.expect("connects");
        // Each run gets a fresh database name, so no cleanup is needed between
        // tests and a failed run leaves its state behind to inspect.
        Some(store)
    }

    macro_rules! store_test {
        ($name:ident, $body:expr) => {
            #[tokio::test]
            async fn $name() {
                let Some(store) = store().await else {
                    eprintln!("skipping: {} is not set", TEST_URI_ENV);
                    return;
                };
                let f: fn(Store) -> _ = $body;
                f(store).await;
            }
        };
    }

    store_test!(an_unseen_contributor_is_unknown_rather_than_missing, |store| async move {
        let who = store.contributor("newcomer-unseen").await.expect("reads");
        assert_eq!(who.trust, Trust::Unknown);
        assert_eq!(who.reviews, 0);
    });

    store_test!(trust_survives_and_carries_its_reason, |store| async move {
        store
            .set_trust("trust-subject", Trust::Blocked, Some("spam pull requests"))
            .await
            .expect("writes");
        let who = store.contributor("trust-subject").await.expect("reads");
        assert_eq!(who.trust, Trust::Blocked);
        assert_eq!(who.note.as_deref(), Some("spam pull requests"));
    });

    store_test!(review_counts_accumulate_across_pull_requests, |store| async move {
        // The whole point of the database: a fact about a person over time,
        // which a stateless workflow has nowhere to keep.
        store.record_review("counter", 2).await.expect("writes");
        store.record_review("counter", 3).await.expect("writes");
        let who = store.contributor("counter").await.expect("reads");
        assert_eq!(who.reviews, 2);
        assert_eq!(who.findings, 5);
    });

    store_test!(recording_a_review_does_not_clobber_trust, |store| async move {
        store
            .set_trust("keeper", Trust::Allowed, None)
            .await
            .expect("writes");
        store.record_review("keeper", 1).await.expect("writes");
        assert_eq!(
            store.contributor("keeper").await.expect("reads").trust,
            Trust::Allowed
        );
    });

    store_test!(a_redelivered_webhook_is_claimed_only_once, |store| async move {
        // GitHub redelivers on timeout. A second review is duplicate comments
        // and duplicate spend for no new information.
        assert!(store.claim_delivery("d-1", "pull_request").await.expect("claims"));
        assert!(!store.claim_delivery("d-1", "pull_request").await.expect("claims"));
    });

    store_test!(a_lease_is_held_by_one_worker_and_released, |store| async move {
        assert!(store.claim_lease("repo#7@abc", "worker-1").await.expect("claims"));
        assert!(!store.claim_lease("repo#7@abc", "worker-2").await.expect("claims"));
        store.release_lease("repo#7@abc").await.expect("releases");
        assert!(store.claim_lease("repo#7@abc", "worker-2").await.expect("claims"));
    });

    store_test!(installations_are_recorded_idempotently, |store| async move {
        store.record_installation(1, "tinyhumansai").await.expect("writes");
        store.record_installation(1, "tinyhumansai").await.expect("writes");
        assert_eq!(store.installation_count().await.expect("counts"), 1);
    });

    // Pure logic, no database needed — these always run.

    #[test]
    fn an_unseen_contributor_has_no_dismissal_rate_rather_than_zero() {
        // Reporting 0% for someone who has never had a finding would read as
        // "never disagrees" instead of "no data".
        assert_eq!(Contributor::unseen("nobody").dismissal_rate(), None);
    }

    #[test]
    fn the_dismissal_rate_is_findings_relative() {
        let mut who = Contributor::unseen("someone");
        who.findings = 8;
        who.dismissed = 2;
        assert_eq!(who.dismissal_rate(), Some(0.25));
    }

    #[test]
    fn trust_round_trips_through_bson() {
        for trust in [Trust::Unknown, Trust::Allowed, Trust::Blocked] {
            let encoded = bson::to_bson(&trust).expect("encodes");
            let decoded: Trust = bson::from_bson(encoded).expect("decodes");
            assert_eq!(decoded, trust);
        }
    }
}
