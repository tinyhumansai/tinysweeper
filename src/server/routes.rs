//! The axum application: routes, and the worker that does the actual work.
//!
//! A delivery is acknowledged and queued, never handled inline. GitHub gives a
//! webhook ten seconds and a review takes minutes, so handling one in the
//! request would guarantee a timeout — and a timeout means GitHub redelivers,
//! which means a second review of the same event. Acknowledge fast, work later.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::FutureExt;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::automerge::types::Outcome;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::forge::RepoId;
use crate::index::mongo::MongoIndex;
use crate::ports::knowledge::KnowledgeStore;
use crate::server::admin::{self, AdminAuth};
use crate::server::auth::AppAuth;
use crate::server::indexing::{IndexBackend, index_in_background};
use crate::server::manual::{self, FullReviews};
use crate::server::store::{Store, Trust};
use crate::server::webhook::{self, Action, Payload};

/// How many reviews may run at once.
///
/// Each one holds a model call open for minutes, so the limit is about spend
/// and rate limits rather than CPU. Keep it low: a repository-wide force-push
/// delivers a burst, and an unbounded worker pool turns that into an unbounded
/// bill.
const MAX_CONCURRENT_REVIEWS: usize = 4;

/// How many repositories may be indexed at once.
///
/// Lower than the review cap and for a different reason. A review is mostly
/// waiting on one model; a full index is a fetch, a tree in memory and
/// thousands of embedding calls, so two of them concurrently is already the
/// provider's rate limit and a good deal of the machine.
const MAX_CONCURRENT_INDEXES: usize = 2;

/// How many pull requests one manual, repository-wide review may queue.
///
/// The button is an escape hatch, not a way to spend an afternoon's budget in
/// one request: a repository with sixty open pull requests would otherwise be
/// sixty full reviews, each of them deliberately ignoring the dedupe that keeps
/// the second review of a pull request cheap.
const MAX_MANUAL_REVIEWS: usize = 20;

/// How the server was configured.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind.
    pub bind: String,
    /// Shared secret GitHub signs deliveries with.
    pub webhook_secret: String,
    /// Review configuration used when a repository has no file of its own.
    pub config: Config,
    /// Credential guarding `/admin`. `None` leaves the admin API unmounted —
    /// see `crate::server::admin` for why that is the fail-closed choice.
    pub admin_auth: Option<AdminAuth>,
}

/// Everything a handler needs.
#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
    store: Store,
    /// Curated knowledge documents. `None` when no retrieval database is
    /// reachable: the review still runs, without pinned context.
    knowledge: Option<Arc<dyn KnowledgeStore>>,
    auth: Arc<AppAuth>,
    permits: Arc<Semaphore>,
    /// The embedder and the retrieval stores, when `[embeddings]` names a
    /// provider. `None` runs every review diff-only, which is what tinysweeper
    /// did before an index existed.
    index: Option<Arc<IndexBackend>>,
    /// Bounds concurrent indexing separately from concurrent reviewing: a
    /// delivery burst must not turn into a burst of full indexes.
    index_permits: Arc<Semaphore>,
}

/// Run the server until the process is stopped.
pub async fn serve(config: ServerConfig, store: Store, auth: AppAuth) -> Result<()> {
    let bind = config.bind.clone();

    // The boot assertion. `$vectorSearch` and `$rankFusion` are stages a stock
    // `mongo:` image does not have, and an unsupported stage fails when the
    // query runs — which is to say on a contributor's pull request, hours after
    // the deploy, as a red check run nobody can explain. Proving it here turns
    // that into a refusal to start, which is the failure an operator can act
    // on. It must not degrade to "retrieval off": a silently unindexed reviewer
    // still posts reviews, just worse ones.
    // The knowledge store is opened whether or not retrieval is on: curated
    // documents are looked up by scope, not by vector, so they work on a
    // deployment with no embedding provider at all. A database that cannot be
    // opened costs pinned context and the admin knowledge routes; it does not
    // stop the server, because reviews are the thing that must keep running.
    let knowledge: Option<Arc<dyn KnowledgeStore>> = match MongoIndex::from_env().await {
        Ok(index) => {
            index.knowledge.prepare().await?;
            Some(Arc::new(index.knowledge))
        }
        Err(err) => {
            tracing::warn!(%err, "no knowledge store: curated documents are unavailable");
            None
        }
    };

    // Opening the backend runs the same boot assertion, and it now also proves
    // the *embedder* is usable: a provider whose key is missing or whose model
    // reports a different width than `[embeddings]` declares is a startup
    // failure rather than a partial index discovered later. An operator who
    // configured no provider gets `None` and a log line saying so.
    let index = match IndexBackend::open(&config.config).await? {
        Some(backend) => {
            tracing::info!(
                signature = %backend.signature,
                "retrieval is on; MongoDB hybrid search is available"
            );
            Some(Arc::new(backend))
        }
        None => {
            tracing::info!("retrieval is disabled: no embedding provider configured");
            None
        }
    };

    let admin_auth = config.admin_auth.clone();
    let state = AppState {
        config: Arc::new(config),
        store: store.clone(),
        knowledge: knowledge.clone(),
        auth: Arc::new(auth),
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS)),
        index: index.clone(),
        index_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_INDEXES)),
    };

    let manual_state = state.clone();
    let manual_auth = admin_auth.clone();

    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/webhook", post(receive))
        .with_state(state);

    // Mounted only when a token is configured. An admin router without a
    // credential would be an unauthenticated write endpoint on the public
    // internet, so its absence is the safe failure.
    match admin::router(store, knowledge, index, admin_auth) {
        Some(admin) => {
            app = app.merge(admin);
            tracing::info!("the admin API is mounted under /admin");
        }
        None => tracing::info!(
            "{} is not set; the admin API is not mounted",
            admin::TOKEN_ENV
        ),
    }

    // The manual full-review button, behind the same credential and absent for
    // the same reason when there is none. It is mounted separately from the
    // admin router because it needs the worker, not the trust database.
    let dispatch: Arc<dyn FullReviews> = Arc::new(ManualDispatch {
        state: manual_state,
    });
    if let Some(routes) = manual::router(manual_auth, manual::allowed_org(), dispatch) {
        app = app.merge(routes);
        tracing::info!(
            organisation = %manual::allowed_org(),
            "manual full reviews are available under /admin/reviews"
        );
    }

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|err| Error::Forge(format!("could not bind {bind}: {err}")))?;

    tracing::info!(%bind, "tinysweeper is listening");
    axum::serve(listener, app)
        .await
        .map_err(|err| Error::Forge(format!("server stopped: {err}")))
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let database = state.store.healthy().await;
    let status = if database {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(json!({
            "ok": database,
            "version": crate::VERSION,
            "database": if database { "up" } else { "down" },
            "reviews_available": state.permits.available_permits(),
        })),
    )
}

async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Signature first, before the body is parsed. Anyone on the internet can
    // POST here; the HMAC is the only thing separating a real delivery from a
    // forged one, and parsing attacker-controlled JSON before checking it is
    // work done on behalf of an attacker.
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    if let Err(err) = webhook::verify(&state.config.webhook_secret, &body, signature) {
        tracing::warn!(%err, "rejected a webhook delivery");
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let delivery = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let payload: Payload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(%err, %event, "could not parse a delivery");
            return (StatusCode::BAD_REQUEST, "unparseable payload").into_response();
        }
    };

    // GitHub redelivers on timeout. Claiming the id means a redelivery is a
    // no-op rather than a second review.
    match state.store.claim_delivery(&delivery, &event).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(%delivery, "already handled");
            return (StatusCode::OK, "duplicate").into_response();
        }
        Err(err) => {
            // A database that is down must not silently drop deliveries: fail
            // the request so GitHub retries rather than pretending it worked.
            tracing::error!(%err, "could not claim the delivery");
            return (StatusCode::SERVICE_UNAVAILABLE, "store unavailable").into_response();
        }
    }

    match webhook::route(&event, &payload) {
        Action::Ignore(reason) => {
            tracing::debug!(%event, reason, "ignoring");
            (StatusCode::OK, "ignored").into_response()
        }
        Action::Review {
            repo,
            number,
            author,
            installation,
        } => {
            tokio::spawn(handle_review(
                state,
                repo,
                number,
                author,
                installation,
                Mode::Incremental,
            ));
            (StatusCode::ACCEPTED, "queued").into_response()
        }
        Action::TriageIssue {
            repo,
            number,
            author,
            installation,
        } => {
            tokio::spawn(handle_triage(state, repo, number, author, installation));
            (StatusCode::ACCEPTED, "queued").into_response()
        }
        Action::AutoMerge {
            repo,
            numbers,
            installation,
        } => {
            for number in numbers {
                tokio::spawn(handle_automerge(
                    state.clone(),
                    repo.clone(),
                    number,
                    installation,
                ));
            }
            (StatusCode::ACCEPTED, "queued").into_response()
        }
    }
}

/// Re-evaluate one pull request against the auto-merge policy, off the request
/// path.
///
/// Errors are logged and dropped. Auto-merge failing is the safe direction by
/// construction — the pull request stays exactly where it was — so a forge
/// hiccup here is a log line, never a failed delivery that GitHub retries.
async fn handle_automerge(state: AppState, repo: String, number: u64, installation: u64) {
    if let Err(err) = automerge_inner(&state, &repo, number, installation).await {
        tracing::error!(%err, %repo, number, "auto-merge evaluation failed");
    }
}

async fn automerge_inner(
    state: &AppState,
    repo: &str,
    number: u64,
    installation: u64,
) -> Result<()> {
    // Checked before anything is read. With the feature off this path fires on
    // every check run in every repository the app is installed on, and an
    // API call per delivery to prove a disabled feature is disabled is a rate
    // limit spent on nothing.
    if !state.config.config.automerge.enabled {
        tracing::debug!(%repo, number, "auto-merge is off");
        return Ok(());
    }

    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    // Not held against `permits`: that semaphore bounds concurrent *reviews*,
    // which are minutes of model calls. This is four reads and possibly one
    // merge, and queueing it behind a review would mean a merge waiting on
    // work it has nothing to do with.
    //
    // The lease is what keeps it honest instead. Several checks finishing at
    // once is the normal case, and every one of them is a delivery: without
    // this, five deliveries would evaluate the same pull request concurrently
    // and race to merge it.
    let lease = format!("{repo}#automerge-{number}");
    if !state.store.claim_lease(&lease, "server").await? {
        tracing::debug!(%lease, "another worker is already evaluating this merge");
        return Ok(());
    }

    let outcome = evaluate_and_merge(state, &repo_id, number, installation).await;

    if let Err(err) = state.store.release_lease(&lease).await {
        tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
    }

    match outcome? {
        Outcome::Merged { method } => {
            tracing::info!(%repo, number, %method, "auto-merged");
        }
        // Logged at debug, not info. Every check run on every open pull request
        // reaches this line, and the overwhelmingly common refusal is "another
        // check is still running" — at info that is the only thing in the log.
        Outcome::Refused(refusal) => {
            tracing::debug!(%repo, number, reason = %refusal, "not auto-merging");
        }
        Outcome::Rejected { method, reason } => {
            tracing::warn!(%repo, number, %method, %reason, "the forge refused the merge");
        }
    }
    Ok(())
}

/// Mint the credentials and run the policy.
///
/// The read handle and the write handle are minted separately from the same
/// installation token, and the split is the point: `merge_if_qualified` takes
/// them as two arguments so that the half of the code which decides is
/// statically unable to write. There is no model in this path at all.
async fn evaluate_and_merge(
    state: &AppState,
    repo: &RepoId,
    number: u64,
    installation: u64,
) -> Result<Outcome> {
    let token = state.auth.installation_token(installation).await?;
    let read = crate::forge::github::GitHubRead::new(&token)?;
    let write = crate::forge::github::GitHubWrite::new(&token)?;

    crate::automerge::merge_if_qualified(
        &read,
        &write,
        &state.config.config.automerge,
        repo,
        number,
    )
    .await
}

/// Triage one issue, off the request path.
///
/// The manual seam: anything that can name a repository, an issue number and an
/// installation can call [`triage_inner`] directly — an endpoint, a CLI
/// subcommand, a cron sweep — without going through a webhook payload.
async fn handle_triage(
    state: AppState,
    repo: String,
    number: u64,
    author: String,
    installation: u64,
) {
    if let Err(err) = triage_inner(&state, &repo, number, &author, installation).await {
        // One issue going wrong is a log line, not an outage.
        tracing::error!(%err, %repo, number, "issue triage failed");
    }
}

async fn triage_inner(
    state: &AppState,
    repo: &str,
    number: u64,
    author: &str,
    installation: u64,
) -> Result<()> {
    if !state.config.config.issues.enabled {
        tracing::debug!(%repo, number, "issue triage is off");
        return Ok(());
    }

    let who = state.store.contributor(author).await?;
    if who.trust == Trust::Blocked {
        tracing::info!(%author, "blocked contributor; not triaging");
        return Ok(());
    }

    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    let permit = state
        .permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|err| Error::Forge(err.to_string()))?;

    // Keyed on the issue rather than a SHA — an issue has no head commit — so
    // two deliveries for the same edit cannot both pay for a triage.
    let lease = format!("{repo}#issue-{number}");
    if !state.store.claim_lease(&lease, "server").await? {
        tracing::debug!(%lease, "another worker holds this triage");
        return Ok(());
    }

    let outcome = triage_and_apply(state, &repo_id, number, installation).await;

    if let Err(err) = state.store.release_lease(&lease).await {
        tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
    }
    drop(permit);

    let plan = outcome?;
    tracing::info!(
        %repo,
        number,
        labels = plan.add_labels.len(),
        closed = plan.close.is_some(),
        refusal = plan.close_refusal.unwrap_or("-"),
        "issue triaged"
    );
    Ok(())
}

/// Read the issue, decide, then publish with a token minted afterwards.
///
/// The deployment's own configuration is used, not the repository's: the
/// `[issues]` overlay is read at a commit, and an issue has no commit to read
/// it at. Wiring that up needs a default-branch lookup the forge port does not
/// have yet, so it is deliberately absent rather than half-present.
async fn triage_and_apply(
    state: &AppState,
    repo: &RepoId,
    number: u64,
    installation: u64,
) -> Result<crate::issues::TriagePlan> {
    let read_token = state.auth.installation_token(installation).await?;
    let forge = crate::forge::github::GitHubRead::new(&read_token)?;
    let model = Arc::new(crate::harness::openrouter::GatewayModel::from_config(
        &state.config.config.models,
    )?);

    // The model runs against a read-only handle; the write token below is
    // minted only after it has answered. Same boundary as a review.
    let outcome = crate::issues::triage(
        &forge,
        model,
        &state.config.config,
        repo,
        number,
        // Maintainer protection is expressed as `issues.close.protected_authors`
        // until the forge port can report a repository's collaborators. An
        // invented list would be worse than an empty one: it would look like
        // the guard was doing something.
        &[],
    )
    .await?;

    if outcome.skipped.is_some() {
        return Ok(outcome.plan);
    }

    let write_token = state.auth.installation_token(installation).await?;
    let write = crate::forge::github::GitHubWrite::new(&write_token)?;
    crate::issues::apply_plan(&write, repo, &outcome.plan).await?;

    Ok(outcome.plan)
}

/// The manual review path's way into the worker.
///
/// Everything a webhook delivery supplies and an operator cannot — the
/// installation, and which pull requests are open — is resolved here, on the
/// request, so the operator gets a real answer rather than a log line.
struct ManualDispatch {
    state: AppState,
}

#[async_trait::async_trait]
impl FullReviews for ManualDispatch {
    async fn enqueue(&self, repo: &RepoId, number: Option<u64>) -> Result<Vec<u64>> {
        // A webhook names its installation. A button does not, so ask GitHub
        // which installation covers the repository; the app JWT can answer that
        // and nothing else.
        let installation = self
            .state
            .auth
            .installation_for_repo(&repo.owner, &repo.name)
            .await?;
        let read_token = self.state.auth.installation_token(installation).await?;
        let forge = crate::forge::github::GitHubRead::new(&read_token)?;

        let numbers = match number {
            Some(number) => vec![number],
            None => forge.open_pull_requests(repo, MAX_MANUAL_REVIEWS).await?,
        };

        let mut queued = Vec::new();
        for number in numbers {
            // The author is what the trust check is about, and only the pull
            // request knows it. One extra read per queued review, on a path
            // used a few times a year.
            let author = {
                use crate::ports::forge::ForgeRead;
                forge.pull_request(repo, number).await?.author
            };

            tokio::spawn(handle_review(
                self.state.clone(),
                repo.to_string(),
                number,
                author,
                installation,
                Mode::Full,
            ));
            queued.push(number);
        }

        Ok(queued)
    }
}

/// Whether a review may use what earlier cycles remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The webhook path: replay the last cycle's evidence, dedupe against what
    /// was already said, and record this cycle for the next one.
    Incremental,
    /// The manual path: review as though this pull request had never been seen.
    Full,
}

/// The configuration a review in `mode` runs under.
///
/// `Mode::Full` is exactly `review.incremental = false` for this one run. That
/// single flag is what `crate::app::review` gates all three halves of the
/// memory on — the prior findings read off the pull request, the remembered
/// state in the store, and the write-back at the end — so turning it off both
/// ignores the stored state and leaves it intact for the webhook path. Nothing
/// is deleted: a manual review is an extra opinion, not a reset, and destroying
/// the record would make the *next* webhook review duplicate its comments too.
fn config_for(base: &Config, mode: Mode) -> std::borrow::Cow<'_, Config> {
    match mode {
        Mode::Incremental => std::borrow::Cow::Borrowed(base),
        Mode::Full => {
            let mut full = base.clone();
            full.review.incremental = false;
            std::borrow::Cow::Owned(full)
        }
    }
}

/// Review one pull request, off the request path.
async fn handle_review(
    state: AppState,
    repo: String,
    number: u64,
    author: String,
    installation: u64,
    mode: Mode,
) {
    if let Err(err) = review_inner(&state, &repo, number, &author, installation, mode).await {
        // A failed review must not take the server with it. One pull request
        // going wrong is a log line, not an outage.
        tracing::error!(%err, %repo, number, "review failed");
    }
}

async fn review_inner(
    state: &AppState,
    repo: &str,
    number: u64,
    author: &str,
    installation: u64,
    mode: Mode,
) -> Result<()> {
    let who = state.store.contributor(author).await?;
    if who.trust == Trust::Blocked {
        tracing::info!(%author, "blocked contributor; not reviewing");
        return Ok(());
    }

    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    // The lease is keyed on the head SHA, so two deliveries for the same push
    // cannot both review it, while a *new* push takes a fresh lease and
    // proceeds.
    let permit = state
        .permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|err| Error::Forge(err.to_string()))?;

    let read_token = state.auth.installation_token(installation).await?;
    let forge = crate::forge::github::GitHubRead::new(&read_token)?;

    let pull_request = {
        use crate::ports::forge::ForgeRead;
        forge.pull_request(&repo_id, number).await?
    };

    // Indexing is kicked off here and deliberately not awaited. A cold full
    // index takes minutes; a review is expected in seconds. The review runs
    // against whatever the index holds right now, and `crate::retrieve` says so
    // in the check-run summary when that is nothing. See `server::indexing`.
    if let Some(backend) = &state.index {
        tokio::spawn(index_in_background(
            backend.clone(),
            Arc::new(state.config.config.clone()),
            state.index_permits.clone(),
            repo_id.clone(),
            pull_request.head_sha.clone(),
            read_token.clone(),
        ));
    }

    // A manual review deliberately takes a lease of its own: the operator asked
    // for this run *because* the ordinary one already happened, so sharing the
    // webhook path's key would make the button a silent no-op.
    let lease = match mode {
        Mode::Incremental => format!("{repo}#{number}@{}", pull_request.head_sha),
        Mode::Full => format!("{repo}#{number}@{}!full", pull_request.head_sha),
    };
    if !state.store.claim_lease(&lease, "server").await? {
        tracing::debug!(%lease, "another worker holds this review");
        return Ok(());
    }

    // `AssertUnwindSafe` + `catch_unwind` so a panic inside a lane still
    // reaches the release below. Without it the `?` on the outcome is not the
    // only way out — an unwind skips everything — and the lease survives the
    // worker that took it.
    // The reviewed repository's own policy, read through the forge because
    // there is no checkout here. Without this every repository is reviewed
    // under the *deployment's* `.tinysweeper.toml`, which is tinysweeper's own.
    // Read at the base branch's tip rather than the head: a config is acted on
    // deterministically, so reading it from the branch under review would let a
    // pull request grade its own exam. See `crate::config::remote`.
    let overlay = crate::config::remote::overlay(
        &forge,
        &repo_id,
        &pull_request.base_sha,
        &state.config.config,
    )
    .await;
    if let Some(source) = &overlay.source {
        tracing::info!(%repo, source, "reviewing under the repository's own configuration");
    }

    let outcome = std::panic::AssertUnwindSafe(run_and_publish(
        state,
        &overlay.config,
        &repo_id,
        number,
        installation,
        &forge,
        mode,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|_| Err(Error::lane("review", "the review panicked")));

    // Released regardless of how the review went. The TTL in the store is the
    // backstop for the cases this cannot cover — a kill, or a lost machine.
    if let Err(err) = state.store.release_lease(&lease).await {
        tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
    }
    drop(permit);

    let proposal = outcome?;
    state
        .store
        .record_review(author, proposal.findings().count() as u64)
        .await?;

    // The review has just published its check runs and, when everything passed,
    // its approving review — which is to say it has just changed the two things
    // the auto-merge policy reads most often. Asking now closes the loop in
    // process rather than waiting for GitHub to deliver our own writes back to
    // us, which it only does for the events the App is subscribed to.
    //
    // Spawned rather than awaited: the review is finished, and a merge that
    // fails must not turn a successful review into a logged error.
    //
    // Not a second write path. It goes through the same `merge_if_qualified` a
    // delivery does, so the policy — and the live re-validation inside it —
    // decides here exactly as it does there. The overlaid config is not used:
    // `[automerge]` is not a key a reviewed repository may set about itself.
    tokio::spawn(handle_automerge(
        state.clone(),
        repo.to_string(),
        number,
        installation,
    ));

    Ok(())
}

/// Run the review and publish it.
///
/// `config` is the *effective* config for this repository — the deployment's,
/// with the reviewed repository's own allow-listed keys laid over it. The model
/// gateway and the index are still built from the deployment's config, because
/// model choice, credentials and the index partition key are not things a
/// reviewed repository may set.
async fn run_and_publish(
    state: &AppState,
    config: &Config,
    repo: &RepoId,
    number: u64,
    installation: u64,
    forge: &crate::forge::github::GitHubRead,
    mode: Mode,
) -> Result<crate::app::Proposal> {
    let model = Arc::new(crate::harness::openrouter::GatewayModel::from_config(
        &state.config.config.models,
    )?);

    // The model runs against a read-only handle. The write token is minted
    // below, after this returns — same boundary as the workflow, same reason.
    // The store doubles as the review-state cache: it is what lets the next
    // push replay this run's evidence verbatim and pay cache prices for it.
    // Dedupe does not depend on it — that reads the markers off the pull
    // request — so a database problem costs money, never a duplicate comment.
    // Retrieval is attached when a provider is configured, and left off when
    // one is not. Both are supported: `crate::retrieve` never errors, it
    // returns a status the check-run summary states, so a cold index, a stale
    // one or an unreachable database all produce a diff-only review that says
    // it is diff-only rather than one that quietly is.
    let retriever = state.index.as_ref().map(|backend| {
        crate::retrieve::Retriever::new(backend.embedder.as_ref(), &backend.index.code)
            .with_graph(&backend.index.graph)
            .with_manifest(backend.manifest.as_ref())
    });

    // The mode is layered on top of the *effective* config, so a repository's
    // own `.tinysweeper.toml` still governs a manual review — a full run is the
    // same policy with no memory, not the deployment's policy instead.
    let config = config_for(config, mode);
    let proposal = crate::app::review::review_with_retrieval(
        forge,
        model,
        &config,
        repo,
        number,
        Some(&state.store),
        state.knowledge.as_deref(),
        retriever.as_ref(),
    )
    .await?;

    let write_token = state.auth.installation_token(installation).await?;
    let write = crate::forge::github::GitHubWrite::new(&write_token)?;
    crate::app::apply(forge, &write, &config, &proposal, Some(&state.store)).await?;

    Ok(proposal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_permit_pool_actually_bounds_concurrency() {
        // Asserting on the constant would be a tautology; this asserts the
        // semaphore behaves, which is what stops a delivery burst becoming an
        // unbounded bill.
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS));
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_REVIEWS {
            held.push(permits.clone().acquire_owned().await.expect("acquires"));
        }

        assert_eq!(permits.available_permits(), 0);
        assert!(
            permits.clone().try_acquire_owned().is_err(),
            "a review beyond the cap must wait rather than start"
        );

        drop(held.pop());
        assert!(
            permits.try_acquire_owned().is_ok(),
            "a freed slot is reusable"
        );
    }

    #[test]
    fn a_full_review_turns_the_incremental_path_off_and_changes_nothing_else() {
        // This is the whole of what "full" means. `review.incremental` gates
        // both halves of the memory in `crate::app::review`: the prior findings
        // read off the pull request, and the remembered state in the store —
        // and it also gates the write-back, so a manual run does not overwrite
        // what the webhook path remembers.
        let mut base = Config::default();
        base.review.incremental = true;
        base.models.budget_usd_per_pr = 4.25;

        let full = config_for(&base, Mode::Full);
        assert!(!full.review.incremental);
        assert_eq!(
            full.models.budget_usd_per_pr, base.models.budget_usd_per_pr,
            "a full review is the same review with no memory, not a different policy"
        );

        let incremental = config_for(&base, Mode::Incremental);
        assert!(
            incremental.review.incremental,
            "the webhook path must be untouched"
        );
    }
}
