//! Keeping the server's memory engine fed. Requires `serve`.
//!
//! The counterpart of [`super::indexing`] for the memory layer: it is what
//! turns [`crate::memory`] on. Everything under it is complete and inert
//! without this seam — nothing implements [`Memory`] against a real engine
//! except [`CortexMemory`], and nothing feeds it a checkout except this.
//!
//! # Ingest reads the *base* branch
//!
//! The index is built from the pull request's head, because retrieval wants
//! the code the change lives in. Memory is built from the pull request's
//! base, deliberately: the conventions it holds are the policy the repository
//! has committed to, not the one a pull request proposes, and a memory a pull
//! request could write to before it merged would be a memory a contributor
//! could poison. A pull request that edits `AGENTS.md` is remembered once it
//! lands, on the next review of anything.
//!
//! # Ingest does not block the review
//!
//! For the same reason indexing does not: a full ingest of a large repository
//! is thousands of engine writes, each held until indexed, and a review is
//! expected in seconds. The review recalls whatever the engine holds right
//! now, and `crate::memory::recall` says so when that is nothing.
//!
//! # Freshness is per process
//!
//! One ingest per base tip per process, tracked in memory. The engine
//! deduplicates on content, so a second process ingesting the same tip pays
//! the network and writes nothing; a manifest for memory would save that cost
//! and nothing else, and is not worth a collection yet.
//!
//! # Conversations are remembered live, debounced
//!
//! Every delivery that touches a conversation — a comment, a review, an issue
//! closing — asks for that conversation to be re-read and remembered, through
//! [`crate::memory::Discussions`]. The re-read is debounced per conversation
//! by `memory.discussion_debounce_secs`: a review bot posting twenty inline
//! comments produces twenty deliveries in a few seconds, and one re-read
//! after the burst remembers all twenty. Replays are free at the engine, so
//! the debounce saves GitHub reads, not correctness.
//!
//! The history from before the server was listening comes from a backfill,
//! started from the admin API and run here in the background; its progress
//! is kept per repository so the operator can poll for it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::error::Result;
use crate::forge::RepoId;
use crate::indexer::fetch::Checkout;
use crate::memory::cortex::CortexMemory;
use crate::memory::{DiscussionReport, Discussions, IngestReport, Ingestor, Recaller};
use crate::ports::memory::Memory;
use crate::server::auth::AppAuth;
use crate::server::webhook::Conversation;

/// Where one repository's backfill stands.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct BackfillStatus {
    /// Whether it is still walking.
    pub running: bool,
    /// The `since` it was started with.
    pub since: Option<String>,
    /// How many conversations it was allowed to walk.
    pub limit: usize,
    /// When it started, RFC 3339.
    pub started_at: String,
    /// When it finished, RFC 3339, once it has.
    pub finished_at: Option<String>,
    /// What it did, once it has finished without a fatal error.
    pub report: Option<DiscussionReport>,
    /// Why it stopped early, when it did. The listing itself failing is the
    /// one fatal error; a single conversation failing is in the report.
    pub error: Option<String>,
}

/// What asking for a backfill produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackfillStart {
    /// A walk was recorded as started; the caller runs it.
    Started(BackfillStatus),
    /// One is already walking this repository; here is where it stands.
    AlreadyRunning(BackfillStatus),
}

/// Conversations one `run_backfill` chunk walks before the installation
/// token backing it is re-minted (from cache, unless it needs renewing).
const BACKFILL_CHUNK: usize = 200;

/// The engine, and what it has been fed.
pub struct MemoryBackend {
    /// The engine. `Arc` so a background ingest can hold it past the request.
    pub memory: Arc<CortexMemory>,
    /// The base revision each repository was last ingested at, this process.
    fresh: Mutex<HashMap<String, String>>,
    /// One async lock per repository, so that concurrent deliveries for the
    /// same base tip serialize on the checkout and ingest instead of each
    /// racing the freshness check and cloning independently.
    ///
    /// A plain `Mutex` will not do: the section it guards awaits a clone and
    /// an ingest, and holding a sync lock across an `.await` blocks the
    /// runtime thread rather than yielding it.
    ingesting: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Conversations with a re-read already scheduled, as `repo#number`. A
    /// delivery that finds its conversation here has nothing to do: the
    /// scheduled re-read will see its comment too.
    pending: Mutex<HashSet<String>>,
    /// The last backfill per repository, running or finished.
    backfills: Mutex<HashMap<String, BackfillStatus>>,
}

impl std::fmt::Debug for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBackend")
            .field("engine", &self.memory.name())
            .finish_non_exhaustive()
    }
}

impl MemoryBackend {
    /// Open the engine described by `[memory]`, if there is one.
    ///
    /// `Ok(None)` means the operator did not turn memory on, which is a
    /// supported deployment. An error means one *was* configured and could
    /// not be reached, which is a mistake rather than a choice and must not
    /// be papered over as "memory off": a silently forgetful reviewer still
    /// posts reviews, just ones that repeat themselves.
    pub async fn open(config: &Config) -> Result<Option<Self>> {
        if !config.memory.enabled {
            return Ok(None);
        }
        let memory = CortexMemory::from_config(&config.memory)?;
        memory.health().await?;
        Ok(Some(Self::over(memory)))
    }

    /// A backend over an already-built engine, with nothing ingested.
    fn over(memory: CortexMemory) -> Self {
        Self {
            memory: Arc::new(memory),
            fresh: Mutex::new(HashMap::new()),
            ingesting: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            backfills: Mutex::new(HashMap::new()),
        }
    }

    /// Claim the debounce slot for `conversation`.
    ///
    /// `true` means the caller owns the re-read and must call
    /// [`Self::release`] when it is done; `false` means one is already
    /// scheduled and this delivery rides along with it.
    pub fn claim(&self, conversation: &Conversation) -> bool {
        self.pending
            .lock()
            .expect("pending lock")
            .insert(conversation_key(conversation))
    }

    /// Give the debounce slot back once the re-read has started reading.
    ///
    /// Released *before* the read rather than after it, deliberately: a
    /// comment that lands while the read is in flight may or may not be in
    /// the page GitHub serves, and letting its delivery schedule a fresh
    /// re-read is what makes sure it is remembered either way.
    pub fn release(&self, conversation: &Conversation) {
        self.pending
            .lock()
            .expect("pending lock")
            .remove(&conversation_key(conversation));
    }

    /// Remember one conversation now, through the shared pipeline.
    pub async fn remember_conversation(
        &self,
        config: &Config,
        repo: &RepoId,
        number: u64,
        pull_request: bool,
        token: &str,
    ) -> Result<DiscussionReport> {
        let forge = crate::forge::github::GitHubRead::new(token)?;
        Discussions::new(self.memory.as_ref(), &forge, &config.memory)
            .remember_number(repo, number, pull_request)
            .await
    }

    /// The last backfill started for `repo`, if any.
    pub fn backfill_status(&self, repo: &RepoId) -> Option<BackfillStatus> {
        self.backfills
            .lock()
            .expect("backfill lock")
            .get(&repo.to_string())
            .cloned()
    }

    /// Record that a backfill of `repo` is starting, unless one is running.
    ///
    /// Two walks over one repository would read every conversation twice
    /// for nothing, so a running one is reported instead of doubled.
    pub fn start_backfill(
        &self,
        repo: &RepoId,
        since: Option<String>,
        limit: usize,
    ) -> BackfillStart {
        let mut backfills = self.backfills.lock().expect("backfill lock");
        if let Some(running) = backfills.get(&repo.to_string()).filter(|s| s.running) {
            return BackfillStart::AlreadyRunning(running.clone());
        }
        let status = BackfillStatus {
            running: true,
            since,
            limit,
            started_at: now(),
            finished_at: None,
            report: None,
            error: None,
        };
        backfills.insert(repo.to_string(), status.clone());
        BackfillStart::Started(status)
    }

    /// Record how `repo`'s backfill ended.
    fn finish_backfill(&self, repo: &RepoId, outcome: Result<DiscussionReport>) {
        let mut backfills = self.backfills.lock().expect("backfill lock");
        if let Some(status) = backfills.get_mut(&repo.to_string()) {
            status.running = false;
            status.finished_at = Some(now());
            match outcome {
                Ok(report) => status.report = Some(report),
                Err(err) => status.error = Some(err.to_string()),
            }
        }
    }

    /// Walk `repo`'s history since `since` and remember it, recording the
    /// outcome under [`Self::backfill_status`]. Call only after
    /// [`Self::start_backfill`] said yes.
    ///
    /// Walked in chunks of [`BACKFILL_CHUNK`] rather than as one pass over
    /// `limit`: the default walk is thousands of GitHub reads plus a memory
    /// write per subject, easily long enough to outlast an installation
    /// token's hour, and a single `GitHubRead` built once up front would
    /// carry that one token for the whole thing. Re-minting between chunks
    /// costs nothing extra — `AppAuth::installation_token` answers from
    /// cache while the token is still good — and renews it before it expires
    /// when the walk runs long.
    pub async fn run_backfill(
        &self,
        config: &Config,
        repo: &RepoId,
        since: Option<&str>,
        limit: usize,
        auth: &AppAuth,
        installation: u64,
    ) {
        let mut cursor = since.map(str::to_string);
        let mut combined = DiscussionReport::default();
        let mut walked = 0usize;
        let outcome: Result<DiscussionReport> = async {
            while walked < limit {
                let chunk = (limit - walked).min(BACKFILL_CHUNK);
                let token = auth.installation_token(installation).await?;
                let forge = crate::forge::github::GitHubRead::new(&token)?;
                let report = Discussions::new(self.memory.as_ref(), &forge, &config.memory)
                    .backfill(repo, cursor.as_deref(), chunk)
                    .await?;
                let processed = report.subjects + report.failed.len();
                // `last_seen` is where this chunk actually got to whether or
                // not everything in it succeeded, unlike `resume_from` (which
                // this walk does not read per chunk): a failure partway
                // through must not stop later chunks from ever being walked,
                // only stop the *externally reported* cursor from advancing
                // past it. See `DiscussionReport::last_seen`.
                let advanced = report.last_seen.clone();
                let stalled = advanced == cursor;
                combined.absorb(report);
                walked += chunk;
                if stalled || advanced.is_none() || processed < chunk {
                    break;
                }
                cursor = advanced;
            }
            // Safe to hand back only once nothing anywhere in the walk
            // failed: a resume must never skip past a failure, even one an
            // internal cursor already made progress beyond.
            combined.resume_from = if combined.failed.is_empty() {
                cursor
            } else {
                None
            };
            Ok(combined)
        }
        .await;
        match &outcome {
            Ok(report) => tracing::info!(%repo, "memory backfill finished: {}", report.summary()),
            Err(err) => tracing::warn!(%repo, %err, "memory backfill failed"),
        }
        self.finish_backfill(repo, outcome);
    }

    /// A recaller over the engine, for one review.
    pub fn recaller(&self) -> Recaller<'_> {
        Recaller::new(self.memory.as_ref())
    }

    /// Whether `repo` has been ingested at `revision` by this process.
    pub fn is_fresh(&self, repo: &str, revision: &str, config: &Config) -> bool {
        self.fresh
            .lock()
            .expect("freshness lock")
            .get(repo)
            .is_some_and(|known| *known == freshness_key(revision, config))
    }

    /// The per-repository lock that serializes `ensure_ingested`, creating it
    /// on first use.
    fn ingest_lock(&self, repo_id: &str) -> Arc<AsyncMutex<()>> {
        self.ingesting
            .lock()
            .expect("ingest lock table")
            .entry(repo_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Feed the engine `repo`'s tree at `revision`, fetching it if it must.
    ///
    /// `Ok(None)` when this process already did. The freshness check comes
    /// before the fetch so a busy repository's deliveries cost neither a
    /// clone nor a walk. Concurrent calls for the same repository serialize
    /// on [`Self::ingest_lock`]: without it, a burst of deliveries sharing a
    /// base tip could all observe a miss before any of them recorded the
    /// revision, and each would clone and ingest it independently.
    pub async fn ensure_ingested(
        &self,
        config: &Config,
        repo: &RepoId,
        revision: &str,
        token: &str,
    ) -> Result<Option<IngestReport>> {
        let repo_id = repo.to_string();
        if self.is_fresh(&repo_id, revision, config) {
            return Ok(None);
        }
        if !config.memory.ingest_code && !config.memory.ingest_conventions {
            return Ok(None);
        }
        let lock = self.ingest_lock(&repo_id);
        let _guard = lock.lock().await;
        // Re-check now that this call holds the repository's lock: another
        // task may have ingested this exact revision while this one waited.
        if self.is_fresh(&repo_id, revision, config) {
            return Ok(None);
        }
        // Read-only, like the index's checkout: the same boundary the review
        // runs against, and no write token anywhere near it.
        let checkout =
            Checkout::fetch(&super::indexing::git_host(), &repo_id, revision, token).await?;
        let ingestor = Ingestor::new(self.memory.as_ref(), &config.memory, &config.paths.ignore)?;
        let report = ingestor.ingest_checkout(&repo_id, checkout.path()).await?;
        tracing::info!(repo = %repo_id, revision, "memory ingested: {}", report.summary());
        self.fresh
            .lock()
            .expect("freshness lock")
            .insert(repo_id, freshness_key(revision, config));
        Ok(Some(report))
    }
}

/// The freshness cache key for `revision` under `config`.
///
/// Keyed on the *effective ingestion policy*, not the revision alone: if the
/// first delivery for a base SHA races a transient failure to fetch the
/// repository's own configuration overlay and falls back to the
/// deployment's, ingesting under that fallback and marking the plain
/// revision fresh would make a later delivery — one that *did* load the
/// repository's real `paths.ignore` — skip re-ingesting, leaving paths the
/// repository explicitly excluded stored and recallable until the base
/// branch moves again. `paths.ignore`, `memory.ingest_code`,
/// `memory.ingest_conventions`, and `memory.convention_files` are exactly
/// what `Ingestor::new` and the selector it builds are constructed from.
fn freshness_key(revision: &str, config: &Config) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.paths.ignore.hash(&mut hasher);
    config.memory.ingest_code.hash(&mut hasher);
    config.memory.ingest_conventions.hash(&mut hasher);
    config.memory.convention_files.hash(&mut hasher);
    format!("{revision}#{:x}", hasher.finish())
}

/// Ingest `repo` in the background.
///
/// Spawned by the review path and deliberately not awaited by it: see the
/// module docs. Errors are logged rather than propagated, because a failed
/// ingest degrades a review and must not fail one. Shares the index permit
/// pool, so a delivery burst cannot turn into a burst of clones.
pub async fn ingest_in_background(
    backend: Arc<MemoryBackend>,
    config: Arc<Config>,
    permits: Arc<tokio::sync::Semaphore>,
    repo: RepoId,
    revision: String,
    token: String,
) {
    let Ok(_permit) = permits.acquire_owned().await else {
        return;
    };
    if let Err(err) = backend
        .ensure_ingested(&config, &repo, &revision, &token)
        .await
    {
        tracing::warn!(%err, repo = %repo, "memory ingest failed; the next review recalls what the engine already holds");
    }
}

/// The debounce key for a conversation.
fn conversation_key(conversation: &Conversation) -> String {
    format!("{}#{}", conversation.repo, conversation.number)
}

/// Now, as RFC 3339 to the second.
fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    rfc3339(secs)
}

/// A Unix timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Hand-rolled for the same reason `auth::parse_expiry` is: one field is not
/// worth a date library. Howard Hinnant's civil-from-days, which is exact for
/// every date the process will ever see.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Remember `conversation` after the debounce window, in the background.
///
/// Spawned by the webhook path and never awaited by it. Errors are logged:
/// a conversation that could not be re-read is remembered by its next
/// delivery or by a backfill, and must not fail the delivery that mentioned
/// it. The installation token is minted *after* the wait, so a long window
/// cannot hand an expired one to the read.
///
/// Shares the index permit pool with `ensure_ingested` and a running
/// backfill: the debounce only coalesces repeat deliveries for the *same*
/// conversation, so a comment burst spread across many issues would
/// otherwise still spawn one unbounded task per conversation, each
/// paginating its own timeline concurrently. Bounding them here caps how
/// much of the installation's shared read budget live re-reads can spend at
/// once, the same way a backfill's own walk is capped.
pub async fn remember_in_background(
    backend: Arc<MemoryBackend>,
    config: Arc<Config>,
    auth: Arc<AppAuth>,
    permits: Arc<tokio::sync::Semaphore>,
    conversation: Conversation,
) {
    if !backend.claim(&conversation) {
        tracing::debug!(
            repo = %conversation.repo,
            number = conversation.number,
            "a re-read of this conversation is already scheduled"
        );
        return;
    }
    tokio::time::sleep(std::time::Duration::from_secs(
        config.memory.discussion_debounce_secs,
    ))
    .await;
    backend.release(&conversation);

    let Ok(_permit) = permits.acquire_owned().await else {
        return;
    };

    let outcome = async {
        let repo = RepoId::parse(&conversation.repo).ok_or_else(|| {
            crate::error::Error::Forge(format!("`{}` is not owner/name", conversation.repo))
        })?;
        let token = auth.installation_token(conversation.installation).await?;
        backend
            .remember_conversation(
                &config,
                &repo,
                conversation.number,
                conversation.pull_request,
                &token,
            )
            .await
    }
    .await;
    match outcome {
        Ok(report) => tracing::info!(
            repo = %conversation.repo,
            number = conversation.number,
            "remembered a conversation: {}",
            report.summary()
        ),
        Err(err) => tracing::warn!(
            repo = %conversation.repo,
            number = conversation.number,
            %err,
            "could not remember a conversation; the next delivery or a backfill will"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_disabled_memory_opens_to_nothing() {
        let config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        assert!(!config.memory.enabled);
        assert!(MemoryBackend::open(&config).await.unwrap().is_none());
    }

    #[test]
    fn freshness_key_changes_with_the_effective_ingestion_policy() {
        // Regression: freshness used to be keyed on the revision alone. If
        // the first delivery for a base SHA ingested under the deployment's
        // fallback `paths.ignore` (its own repository-overlay fetch having
        // failed transiently) and a later delivery for the *same* revision
        // then successfully loaded the repository's real overlay, keying on
        // revision alone would mark that later, differently-configured
        // ingest a no-op — leaving paths the repository explicitly excluded
        // stored and recallable.
        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        let fallback = freshness_key("sha1", &config);
        assert_eq!(fallback, freshness_key("sha1", &config), "deterministic");

        config.paths.ignore = vec!["vendor/**".into()];
        let with_overlay = freshness_key("sha1", &config);
        assert_ne!(
            fallback, with_overlay,
            "a different effective paths.ignore must not look fresh"
        );

        assert_ne!(
            freshness_key("sha1", &config),
            freshness_key("sha2", &config),
            "a different revision must not look fresh either"
        );
    }

    #[tokio::test]
    async fn ingest_lock_is_shared_per_repository_and_serializes_holders() {
        // Regression for the freshness race: two concurrent `ensure_ingested`
        // calls for the same repository must contend on the *same* lock, not
        // each get their own, or the check-then-ingest window stays racy.
        // `CortexMemory::new` performs no I/O — it only builds an HTTP client
        // — so it is safe to construct directly here without a real engine.
        let backend = MemoryBackend::over(
            CortexMemory::new("http://127.0.0.1:1", "test-key").expect("client builds"),
        );

        let a = backend.ingest_lock("o/same");
        let b = backend.ingest_lock("o/same");
        assert!(
            Arc::ptr_eq(&a, &b),
            "the same repository must contend on one lock"
        );

        let other = backend.ingest_lock("o/other");
        assert!(
            !Arc::ptr_eq(&a, &other),
            "a different repository must not share the lock"
        );

        // While `a` is held, a second acquisition on the same lock must wait
        // for it to be released rather than resolving immediately.
        let guard = a.lock().await;
        let held = Arc::clone(&b);
        let waiter = tokio::spawn(async move {
            let _second_guard = held.lock().await;
        });
        // Give the spawned task a chance to run and observe it is still
        // blocked on the held lock.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the second acquisition must block");
        drop(guard);
        waiter.await.expect("the waiter completes once released");
    }

    #[test]
    fn timestamps_render_as_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_755_167_400), "2025-08-14T10:30:00Z");
    }

    #[test]
    fn a_conversation_is_claimed_once_until_released() {
        let backend = MemoryBackend::over(
            CortexMemory::new("http://127.0.0.1:1", "test-key").expect("client builds"),
        );
        let conversation = Conversation {
            repo: "o/r".into(),
            number: 7,
            pull_request: true,
            installation: 1,
        };
        assert!(
            backend.claim(&conversation),
            "the first delivery owns the re-read"
        );
        assert!(!backend.claim(&conversation), "a burst rides along");
        let other = Conversation {
            number: 8,
            ..conversation.clone()
        };
        assert!(
            backend.claim(&other),
            "a different conversation is its own slot"
        );
        backend.release(&conversation);
        assert!(
            backend.claim(&conversation),
            "released, the next delivery owns it again"
        );
    }

    #[test]
    fn one_backfill_per_repository_at_a_time() {
        let backend = MemoryBackend::over(
            CortexMemory::new("http://127.0.0.1:1", "test-key").expect("client builds"),
        );
        let repo = RepoId::parse("o/r").unwrap();
        assert!(backend.backfill_status(&repo).is_none());
        let BackfillStart::Started(started) =
            backend.start_backfill(&repo, Some("2026-01-01T00:00:00Z".into()), 50)
        else {
            panic!("nothing was running");
        };
        assert!(started.running);
        let BackfillStart::AlreadyRunning(refused) = backend.start_backfill(&repo, None, 50) else {
            panic!("a second walk must be refused");
        };
        assert_eq!(refused.since.as_deref(), Some("2026-01-01T00:00:00Z"));

        backend.finish_backfill(
            &repo,
            Ok(DiscussionReport {
                subjects: 3,
                ..DiscussionReport::default()
            }),
        );
        let done = backend.backfill_status(&repo).expect("recorded");
        assert!(!done.running);
        assert!(done.finished_at.is_some());
        assert_eq!(done.report.as_ref().map(|r| r.subjects), Some(3));
        assert!(
            matches!(
                backend.start_backfill(&repo, None, 50),
                BackfillStart::Started(_)
            ),
            "finished, so a new one may start"
        );
    }

    #[tokio::test]
    async fn an_enabled_memory_with_no_key_refuses_to_open() {
        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.memory.enabled = true;
        config.memory.endpoint = "http://127.0.0.1:1".into();
        config.memory.api_key_env = "TINYSWEEPER_TEST_NO_SUCH_CORTEX_KEY".into();
        let err = MemoryBackend::open(&config).await.unwrap_err();
        assert!(err.to_string().contains("is not set"), "{err}");
    }
}
