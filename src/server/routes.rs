//! The axum application: routes, and the worker that does the actual work.
//!
//! Review work is durably queued rather than run in the webhook request.
//! Admission may resolve minimal live metadata and publish a queued check inline;
//! durable status intents retry independently if that status-only work fails.
//! GitHub gives a webhook ten seconds and does not automatically redeliver failed
//! requests, so accepted work must survive even when acknowledgment is lost.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::FutureExt;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::{Semaphore, watch};

use crate::automerge::types::Outcome;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::forge::{PullRequest, RepoId};
use crate::index::mongo::MongoIndex;
use crate::indexer::repository_cache::RepositoryCache;
use crate::ports::forge::ForgeRead as _;
use crate::ports::forge::ForgeWrite;
use crate::ports::knowledge::KnowledgeStore;
use crate::ports::model::{Model, ModelRequest, ModelResponse};
use crate::ports::model_factory::ModelFactory;
use crate::ports::review_job::{
    Admission, OwnedReview, PaidStart, ReviewJobMode, ReviewJobStore, ReviewRequest,
    TerminalOutcome,
};
use crate::pr_triage::Report as PrTriageReport;
use crate::server::admin::{self, AdminAuth};
use crate::server::auth::AppAuth;
use crate::server::failure;
use crate::server::indexing::{IndexBackend, index_in_background};
use crate::server::manual::{self, FullReviews, MergeReport, Merges, Triages};
use crate::server::mcp;
use crate::server::model::{GatewayModelFactory, ResolvedModels};
use crate::server::preview::{
    self, FinishReply, FinishRequest, Previews, StartReply, StartRequest,
    StepReply as PreviewStepReply,
};
use crate::server::status;
use crate::server::store::{LEASE_TTL, Store, Trust};
use crate::server::webhook::{self, Action, Payload};

/// How many reviews may run at once.
///
/// Each one holds a model call open for minutes, so the limit is about spend
/// and rate limits rather than CPU. Keep it low: a repository-wide force-push
/// delivers a burst, and an unbounded worker pool turns that into an unbounded
/// bill.
#[cfg(test)]
const MAX_CONCURRENT_REVIEWS: usize = 4;
/// Capacity of the bounded in-process channel of review wakeup hints. Durable
/// accepted-job capacity lives in the review scheduler port.
const MAX_REVIEW_WAKEUP_HINTS: usize = 256;
/// How long one admitted worker may take, from permit acquisition to verdict.
///
/// Every model call is bounded on its own — the gateway client caps a unary
/// request at ten minutes — but a review is dozens of them in sequence, across
/// lanes, files, retries and the fallback chain, and nothing capped the sum.
/// On 2026-09-15 `tinyhumansai/backend#1332` sat "in progress" for over two
/// hours while holding one of the four review permits. A healthy review of a
/// large repository finishes in single-digit minutes; this is several times
/// that, so it only ever fires on something already broken.
///
/// The budget is *checked* at every phase boundary of the lease-held
/// lifecycle — before the lease is claimed, and again before the lanes start —
/// and *enforced* by cancellation only on the lanes themselves, in
/// `run_lanes`. The metadata phase in between is a handful of forge calls
/// that must not be cancelled mid-flight (a check-run POST cut off after
/// GitHub accepted it is an orphaned check), so they are bounded per call by
/// `forge::github::REQUEST_TIMEOUT` and the deadline is re-read once they
/// return. See `Run::check`.
///
/// Strictly less than [`LEASE_TTL`] with room to spare, and that ordering is
/// load-bearing: the lease is what stops a redelivery from reviewing the same
/// commit twice, and a review still running when its lease expires is exactly
/// the duplicate the lease exists to prevent. What the assertion below leaves
/// over covers the metadata phase's per-call bounds and the publish, which
/// has a bound of its own in [`PUBLISH_DEADLINE`].
const REVIEW_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// How long the publish after the lanes may take, wall clock.
///
/// A separate budget rather than the remainder of [`REVIEW_DEADLINE`], and a
/// generous one. `apply` is a handful of sequential, non-idempotent GitHub
/// writes, and cancelling it between two of them cannot retract what GitHub
/// already accepted — so this must never fire on a publish that is merely
/// slow, only on one that is stuck. Each write is capped at a minute by the
/// forge client; a publish that needs five is already broken, and cutting it
/// off then costs at most a partial review, which the umbrella check reports
/// as a failure, rather than a lease that lapses under a review still holding
/// it.
const PUBLISH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// How long the metadata phase between the lease claim and the lanes may
/// take at most: `open_status`, the config overlay and the default-branch
/// read, each capped at `forge::github::REQUEST_TIMEOUT`, plus the token
/// mint. Not enforced here — it is what the per-call bounds add up to — but
/// counted, so the lease assertion below is about the whole lifecycle rather
/// than the two phases that happen to have a name.
const METADATA_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

const _: () = assert!(
    REVIEW_DEADLINE.as_secs() + METADATA_ALLOWANCE.as_secs() + PUBLISH_DEADLINE.as_secs()
        < LEASE_TTL.as_secs(),
    "every phase of a lease-held review must give up before the lease does"
);

const _: () = assert!(
    super::auth::REVIEW_READ_MARGIN.as_secs()
        > REVIEW_DEADLINE.as_secs() + METADATA_ALLOWANCE.as_secs() + PUBLISH_DEADLINE.as_secs(),
    "cached review credentials must outlast every phase that can retain the reader"
);

/// How many repositories may be indexed at once.
///
/// Lower than the review cap and for a different reason. A review is mostly
/// waiting on one model; a full index is a fetch, a tree in memory and
/// thousands of embedding calls, so two of them concurrently is already the
/// provider's rate limit and a good deal of the machine.
const MAX_CONCURRENT_INDEXES: usize = 2;

/// How long `conclude_in_flight` may spend concluding checks before giving up
/// on the rest.
///
/// Compose sends `SIGKILL` ten seconds after `SIGTERM`; this leaves a margin
/// under that for the runtime to actually unwind once `conclude_in_flight`
/// returns. Deliberately shorter than `forge::github::REQUEST_TIMEOUT` (60s):
/// a single stalled `update_check` must not be able to spend the whole grace
/// period on its own and starve every other review's check behind it.
const SHUTDOWN_CLEANUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

/// How many times an `e2e` settlement retries the per-pull-request lease
/// before giving up, and how long it waits between attempts.
///
/// Bounded low: this is only meant to ride out a settlement that is already
/// nearly done, not to turn a lease into a queue. See `settle_e2e_inner`.
const LEASE_CONTENTION_RETRIES: u32 = 4;
const LEASE_CONTENTION_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

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
    /// Resolved once at boot; clones share provider/runtime state across workers.
    models: ResolvedModels,
    /// Curated knowledge documents. `None` when no retrieval database is
    /// reachable: the review still runs, without pinned context.
    knowledge: Option<Arc<dyn KnowledgeStore>>,
    auth: Arc<AppAuth>,
    permits: Arc<Semaphore>,
    review_jobs: Arc<dyn ReviewJobStore>,
    review_owner: Arc<str>,
    accepting_reviews: Arc<std::sync::atomic::AtomicBool>,
    review_queue: mpsc::Sender<()>,
    /// The embedder and the retrieval stores, when `[embeddings]` names a
    /// provider. `None` runs every review diff-only, which is what tinysweeper
    /// did before an index existed.
    index: Option<Arc<IndexBackend>>,
    /// Shared server-owned Git mirrors for indexing and exact-head reviews.
    repository_cache: RepositoryCache,
    /// Bounds concurrent indexing separately from concurrent reviewing: a
    /// delivery burst must not turn into a burst of full indexes.
    index_permits: Arc<Semaphore>,
    /// Issue triage has a separate operator-sized pool and cannot consume review slots.
    triage_permits: Arc<Semaphore>,
    /// One lock per UI preview session ever seen, so two `step` calls for the
    /// same session (the hands retrying a dropped response, or two flows
    /// racing) serialise their load-modify-save instead of one overwriting
    /// the other's transition. Never pruned — a session id is a 32-character
    /// hash, one entry is a handful of bytes, and a deployment restarts long
    /// before that adds up. The map itself is a `std::sync::Mutex` because
    /// the critical section that touches it never awaits.
    preview_locks:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// The umbrella check of every review currently running, so a shutdown
    /// can conclude them. A deploy replaces the container, and a review that
    /// dies with it would otherwise leave its check "in progress" until the
    /// next push — which `automerge` reads as a review still running. Each
    /// slot is registered when its review starts and removed when it ends;
    /// see `conclude_in_flight`. Also carries whether new reviews are still
    /// accepted, so shutdown can refuse one that would otherwise register
    /// after the concluding snapshot and be orphaned exactly like the
    /// deploy this exists to guard against.
    in_flight: Arc<std::sync::Mutex<InFlightRegistry>>,
}

/// The admitted review's identity and revision, carried together into a worker.
struct ReviewJob {
    repo: String,
    number: u64,
    author: String,
    installation: u64,
    mode: Mode,
    delivery: Option<String>,
    expected_head: Option<String>,
    queued_at: tokio::time::Instant,
    owned: Option<OwnedReview>,
    queue_check: Option<crate::ports::review_job::ReviewCheckRecord>,
}

enum WorkerDisposition {
    Terminal(TerminalOutcome),
    Deferred(i64),
    Yielded,
    OwnershipLost,
}

enum InnerReview {
    Complete(Option<(usize, bool, status::ReviewCoverage)>),
    Busy,
    Exhausted,
    Shutdown,
}

/// Run the server until the process is stopped.
pub async fn serve(config: ServerConfig, store: Store, auth: AppAuth) -> Result<()> {
    serve_with_model_factory(config, store, auth, Arc::new(GatewayModelFactory)).await
}

/// Run with host-supplied model construction, resolving shared adapters at startup.
///
/// Factory errors refuse startup before the listener or background workers exist.
/// The factory receives only deployment model configuration, never forge credentials.
pub async fn serve_with_model_factory(
    config: ServerConfig,
    store: Store,
    auth: AppAuth,
    factory: Arc<dyn ModelFactory>,
) -> Result<()> {
    let bind = config.bind.clone();
    let review_concurrency = config.config.server.review_concurrency;
    let triage_concurrency = config.config.server.triage_concurrency;
    let models = ResolvedModels::resolve(factory.as_ref(), &config.config).await?;

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
    let repository_cache = RepositoryCache::from_env()?;
    let index = match IndexBackend::open(&config.config, repository_cache.clone()).await? {
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
    // The channel contains wakeup hints only; the durable scheduler is
    // authoritative and its poll recovers a request if a hint is lost.
    let (review_queue, review_receiver) = mpsc::channel(MAX_REVIEW_WAKEUP_HINTS);
    let review_jobs: Arc<dyn ReviewJobStore> = Arc::new(store.clone());
    let state = AppState {
        config: Arc::new(config),
        store: store.clone(),
        models,
        knowledge: knowledge.clone(),
        auth: Arc::new(auth),
        permits: Arc::new(Semaphore::new(review_concurrency)),
        review_jobs,
        review_owner: Arc::from(format!(
            "{}-{}",
            std::process::id(),
            bson::oid::ObjectId::new().to_hex()
        )),
        accepting_reviews: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        review_queue,
        index: index.clone(),
        repository_cache,
        index_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_INDEXES)),
        triage_permits: Arc::new(Semaphore::new(triage_concurrency)),
        preview_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        in_flight: Arc::new(std::sync::Mutex::new(InFlightRegistry::default())),
    };

    // Status retry is independent of review permits and review ownership. A
    // failed or interrupted check POST stays attached to the accepted job and
    // is retried on a bounded durable claim after restart or API recovery.
    let retry_jobs = state.review_jobs.clone();
    let retry_factory = github_queue_status_client(state.auth.clone(), 0);
    tokio::spawn(async move {
        let mut poll = tokio::time::interval(std::time::Duration::from_secs(2));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            poll.tick().await;
            if let Err(err) =
                publish_pending_queue_checks(retry_jobs.as_ref(), retry_factory.clone()).await
            {
                tracing::warn!(%err, "queued review status retry failed; durable claim will expire for retry");
            }
        }
    });

    let shutdown_state = state.clone();
    let manual_state = state.clone();
    let manual_auth = admin_auth.clone();
    let preview_state = state.clone();
    let preview_auth = AdminAuth::from_named_env(preview::TOKEN_ENV)?;
    let enabled_preview = state.config.config.preview.enabled;
    // Logged once at boot rather than discovered from behaviour. "Auto-merge
    // is configured and nothing acts on it" was a real bug in this repository;
    // a line at startup saying which way the switch is set is the cheapest
    // thing that would have caught it.
    let enabled_automerge = state.config.config.automerge.enabled;

    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/webhook", post(receive))
        .with_state(state.clone());

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
    let full_reviews: Arc<dyn FullReviews> = Arc::new(ManualDispatch {
        state: manual_state.clone(),
    });
    let merges: Arc<dyn Merges> = Arc::new(MergeDispatch {
        state: manual_state.clone(),
    });
    let triages: Arc<dyn Triages> = Arc::new(TriageDispatch {
        state: manual_state.clone(),
    });
    // The periodic sweep, spawned only when it has both a switch and an
    // interval. It is what makes triage automatic rather than a button: a
    // duplicate opened at midnight is labelled by morning without anybody
    // pressing anything.
    spawn_triage_sweeps(manual_state, triages.clone());

    if let Some(routes) = manual::router(
        manual_auth,
        manual::allowed_org(),
        full_reviews,
        merges,
        triages,
    ) {
        app = app.merge(routes);
        tracing::info!(
            organisation = %manual::allowed_org(),
            automerge = enabled_automerge,
            "manual full reviews are available under /admin/reviews, \
             and auto-merge sweeps under /admin/merges"
        );
    }

    // The UI preview routes, behind a token of their own: this one is handed
    // to every repository's CI, and the admin token must never be. Absent
    // when there is no token, like the admin router and for the same reason.
    let previews: Arc<dyn Previews> = Arc::new(PreviewDispatch {
        state: preview_state,
    });
    match preview::router(preview_auth, previews) {
        Some(routes) => {
            app = app.merge(routes);
            tracing::info!(
                enabled = enabled_preview,
                "UI preview sessions are available under /preview"
            );
        }
        None => tracing::info!(
            "{} is not set; the UI preview routes are not mounted",
            preview::TOKEN_ENV
        ),
    }

    // MCP is a separate, least-privilege door. It is never implicitly covered
    // by the admin token: agent clients get only the credential intended for
    // their tools, and a missing token leaves no endpoint to probe.
    if state.config.config.mcp.enabled {
        let mcp_auth = mcp::McpAuth::from_env(&state.config.config.mcp.token_env)?;
        if let Some(routes) = mcp::router(
            mcp_auth,
            state.config.config.mcp.allowed_org.clone(),
            state.config.config.mcp.allowed_repos.clone(),
            state.auth.clone(),
            state.index.clone(),
            state.store.clone(),
        ) {
            app = app.merge(routes);
            tracing::info!("authenticated MCP is mounted at /mcp");
        } else {
            tracing::warn!("MCP is enabled but its token is unset; endpoint is not mounted");
        }
    }

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|err| Error::Forge(format!("could not bind {bind}: {err}")))?;

    tracing::info!(%bind, "tinysweeper is listening");
    let review_dispatcher = tokio::spawn(drain_review_queue(
        review_receiver,
        state.permits.clone(),
        review_concurrency,
        state.review_jobs.clone(),
        state.review_owner.clone(),
        state.accepting_reviews.clone(),
        move |job, permit| {
            tokio::spawn(run_owned_review(state.clone(), job, permit));
        },
    ));
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(shutdown_state))
        .await
        .map_err(|err| Error::Forge(format!("server stopped: {err}")));
    review_dispatcher.abort();
    served
}

/// Resolve when the process has been asked to stop and is ready to.
///
/// `SIGTERM` is what `docker compose up` sends on a redeploy, `SIGINT` what
/// an operator's terminal sends. Either way the answer is the same: conclude
/// the checks of the reviews still running, *then* let axum drain. That order
/// matters. Compose gives the process ten seconds before `SIGKILL`, and axum
/// only returns once every open connection has finished — a preview step
/// in flight could spend the whole grace period on its own — so anything that
/// runs after `serve` returns may never run at all.
async fn shutdown(state: AppState) {
    shutdown_signal().await;
    conclude_in_flight(&state).await;
}

/// Resolve when the process is asked to stop.
///
/// `SIGTERM` only exists as a signal tokio can listen for on Unix, which is
/// the only platform this ever runs on — Compose, and every workflow in this
/// repository, are Linux containers. `#[cfg(unix)]` still guards the import
/// rather than depending on that: the alternative is a hard build failure on
/// any other target, and `ctrl_c` alone is a correct, if smaller, shutdown
/// path everywhere `tokio::signal` builds at all.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(err) => {
            // Without a handler the runtime would take the default action
            // and die mid-review; waiting on `ctrl_c` alone is the best that
            // can be done, and it is worth saying that happened.
            tracing::error!(%err, "could not install a SIGTERM handler; a redeploy will orphan running reviews");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM; shutting down"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT; shutting down"),
    }
}

/// The non-Unix fallback: no `SIGTERM` to listen for, so `ctrl_c` is the
/// whole story.
#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("received an interrupt; shutting down");
}

/// Conclude the umbrella check of every review still running, and stop
/// taking new ones.
///
/// Taking each slot's status out is what makes this safe against the review
/// itself: if a lane happens to finish during the grace period, its own
/// `close_status` finds the slot empty and does nothing, so no check is
/// concluded twice. The reviews are not cancelled here — the process exit
/// does that, and a lane that gets a few more seconds costs nothing.
///
/// Flipping `accepting` off in the same locked section as the snapshot is
/// what closes the race with `InFlight::register`: a webhook accepted while
/// this function is still awaiting the network calls below (axum has not
/// started draining yet — that only happens once `shutdown` returns) would
/// otherwise be able to register a slot after the snapshot was taken, and
/// that review would then run unwatched, with only whatever is left of the
/// grace period before Compose's `SIGKILL` to finish and conclude its own
/// check. Declining it here, before it opens a check, is strictly better
/// than that — GitHub's own redelivery or the next push starts it again once
/// the new container is up.
async fn conclude_in_flight(state: &AppState) {
    state
        .accepting_reviews
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let slots = {
        let mut registry = state.in_flight.lock().expect("in-flight reviews");
        registry.accepting = false;
        std::mem::take(&mut registry.slots)
    };
    if slots.is_empty() {
        return;
    }
    tracing::warn!(
        reviews = slots.len(),
        "shutting down with reviews in flight; concluding their checks as failed"
    );
    let err = Error::lane(
        "review",
        "tinysweeper was restarted while this review was running",
    );

    // Concurrently, and under a deadline shorter than the grace period, not
    // a serial loop with none. `GitHubWrite`'s client already times a single
    // request out at `forge::github::REQUEST_TIMEOUT` (60s) — longer than
    // Compose's whole ten seconds before `SIGKILL` on its own — so closing
    // slots one at a time could starve every review after the first behind
    // one stalled request, leaving their checks pending regardless of how
    // fast they themselves would have concluded. Whatever this deadline
    // does not reach in time stays pending until the next push, the same
    // fallback every other best-effort write in this module already relies
    // on.
    let closes = slots
        .iter()
        .map(|slot| close_status(state, slot, Conclusion::Failed(&err)));
    if tokio::time::timeout(SHUTDOWN_CLEANUP_DEADLINE, futures::future::join_all(closes))
        .await
        .is_err()
    {
        tracing::error!(
            reviews = slots.len(),
            "shutdown's grace period ran out before every in-flight check could be concluded"
        );
    }
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

    // Routing is pure — headers and the parsed body, no I/O — so the two
    // outcomes that do no work are answered without touching the database at
    // all. Most deliveries land here: a repository the app is installed on
    // produces a check run for everything its CI does.
    let action = webhook::route(&event, &payload);
    match action {
        Action::TrackDraft => {
            tracing::debug!(%event, "tracking draft pull request");
            return (StatusCode::OK, "tracked").into_response();
        }
        Action::Ignore(reason) => {
            tracing::debug!(%event, reason, "ignoring");
            return (StatusCode::OK, "ignored").into_response();
        }
        _ => {}
    }

    if matches!(&action, Action::Review { .. }) {
        if !state
            .accepting_reviews
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "server is stopping; operator redelivery required",
            )
                .into_response();
        }
        let installation = match &action {
            Action::Review { installation, .. } => *installation,
            _ => unreachable!("review action branch checked above"),
        };
        let author = match &action {
            Action::Review { author, .. } => author,
            _ => unreachable!("review action branch checked above"),
        };
        match state.store.contributor(author).await {
            Ok(contributor) if contributor.trust == Trust::Blocked => {
                tracing::info!(%author, "blocked contributor; not admitting review status");
                return (StatusCode::OK, "ignored").into_response();
            }
            Err(err) => {
                tracing::error!(%err, %author, "could not verify review trust before status admission");
                return (StatusCode::SERVICE_UNAVAILABLE, "review trust unavailable")
                    .into_response();
            }
            _ => {}
        }
        let admission = match admit_webhook_review(
            state.review_jobs.as_ref(),
            &action,
            &delivery,
            &event,
            github_queue_status_client(state.auth.clone(), installation),
        )
        .await
        {
            Ok(Some(admission)) => admission,
            Ok(None) => return (StatusCode::OK, "ignored").into_response(),
            Err(err) => {
                tracing::error!(%err, %delivery, "could not durably accept review; operator redelivery required");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "review scheduler unavailable; operator redelivery required",
                )
                    .into_response();
            }
        };
        match admission {
            Admission::Full => {
                tracing::error!(%delivery, "durable review scheduler is full; operator redelivery required");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "review queue is full; operator redelivery required",
                )
                    .into_response();
            }
            Admission::Admitted | Admission::Duplicate => {
                // A dropped/full hint is harmless; the bounded durable
                // scheduler is polled periodically by the dispatcher.
                let _ = state.review_queue.try_send(());
            }
        }
        return (StatusCode::ACCEPTED, "queued").into_response();
    }

    // Everything past here is work, and none of it happens on this task.
    //
    // The delivery claim used to run *here*, inline, and that is what made a
    // slow database a dropped delivery: `claim_delivery` is a round trip, the
    // handler could not answer until it returned, and GitHub allows ten
    // seconds. On 2026-08-13 a large graph write saturated Mongo and eight
    // deliveries were lost in ninety seconds — four to a ten-second timeout and
    // four to the 503 this function used to return. Among them was a
    // `pull_request opened`, so that pull request was simply never reviewed.
    //
    // The 503 was meant to make GitHub retry. It does not: the delivery log
    // says `giving up after 1 attempt(s)`, so failing the request did not buy a
    // second chance, it only converted a slow database into permanent data
    // loss. Acknowledging first and claiming in the worker cannot lose a
    // delivery that way.
    //
    // Dedupe is not weakened by the move, because the claim still runs before
    // any work — just on the other side of the response. It is also not the
    // only guard: `review_inner` takes a lease keyed on `repo#number@sha`, so
    // even a claim that is lost outright cannot produce two reviews of one
    // commit.
    tokio::spawn(dispatch(
        state,
        action,
        Some(delivery),
        event,
        None,
        Mode::Incremental,
        None,
    ));
    (StatusCode::ACCEPTED, "queued").into_response()
}

async fn admit_webhook_review(
    jobs: &dyn ReviewJobStore,
    action: &Action,
    delivery: &str,
    event: &str,
    status_client: QueueStatusClientFactory,
) -> Result<Option<Admission>> {
    let Action::Review {
        repo,
        number,
        author,
        installation,
        head_sha,
    } = action
    else {
        return Err(Error::Forge(
            "only review actions use the durable scheduler".into(),
        ));
    };
    if delivery.is_empty() {
        return Err(Error::Forge("review delivery has no delivery ID".into()));
    }
    let admission = jobs
        .admit_batch(vec![ReviewRequest {
            id: crate::ports::review_job::ReviewJobId(format!("webhook:{delivery}")),
            repo: repo.clone(),
            number: *number,
            author: author.clone(),
            installation: *installation,
            head_sha: head_sha.clone(),
            delivery: Some(delivery.to_owned()),
            event: event.to_owned(),
            mode: ReviewJobMode::Incremental,
            accepted_at_ms: unix_time_ms(),
        }])
        .await?;
    if matches!(admission, Admission::Admitted | Admission::Duplicate) {
        // This deterministic write is the narrow #19 exception: it reports
        // trusted accepted work only. No review context, lease, budget, index,
        // model or model credential is reachable from this path.
        if let Err(err) = publish_pending_queue_checks(jobs, status_client).await {
            tracing::warn!(%err, %delivery, "queued review status remains durable and will be retried");
        }
    }
    Ok(Some(admission))
}

async fn publish_pending_queue_checks(
    jobs: &dyn ReviewJobStore,
    factory: QueueStatusClientFactory,
) -> Result<()> {
    publish_pending_queue_checks_with_clock(jobs, factory, unix_time_ms).await
}

#[cfg(test)]
async fn publish_pending_queue_checks_at(
    jobs: &dyn ReviewJobStore,
    factory: QueueStatusClientFactory,
    now_ms: i64,
) -> Result<()> {
    publish_pending_queue_checks_with_clock(jobs, factory, || now_ms).await
}

async fn publish_pending_queue_checks_with_clock<F>(
    jobs: &dyn ReviewJobStore,
    factory: QueueStatusClientFactory,
    now: F,
) -> Result<()>
where
    F: Fn() -> i64 + Send + Sync,
{
    let mut first_error = None;
    for _ in 0..QUEUE_STATUS_DRAIN_BATCH {
        let Some(pending) = jobs.claim_pending_metadata(now(), 1).await?.pop() else {
            break;
        };
        let attempt = tokio::time::timeout(QUEUE_STATUS_OPERATION_TIMEOUT, async {
            if !jobs.metadata_claim_current(&pending, now()).await? {
                return Ok::<(), Error>(());
            }
            let clients = factory(pending.installation).await?;
            let repo = RepoId::parse(&pending.repo)
                .ok_or_else(|| Error::Forge("queued review has an invalid repository".into()))?;
            let metadata = clients
                .read
                .pull_request_metadata(&repo, pending.number)
                .await?;
            if !jobs.metadata_claim_current(&pending, now()).await? {
                return Ok(());
            }
            let _ = jobs
                .resolve_pending_metadata(
                    &pending,
                    &metadata.head_sha,
                    metadata.open,
                    metadata.draft,
                    now(),
                )
                .await?;
            Ok(())
        })
        .await;
        let attempt = match attempt {
            Ok(result) => result,
            Err(_) => Err(Error::Forge(
                "queued review metadata operation exceeded its deadline".into(),
            )),
        };
        if let Err(err) = attempt {
            first_error.get_or_insert(err);
        }
    }
    for _ in 0..QUEUE_STATUS_DRAIN_BATCH {
        let Some(check) = jobs.claim_pending_checks(now(), 1).await?.pop() else {
            break;
        };
        let attempt = tokio::time::timeout(QUEUE_STATUS_OPERATION_TIMEOUT, async {
            if !jobs.queued_check_claim_current(&check, now()).await? {
                return Ok::<(), Error>(());
            }
            let clients = factory(check.installation).await?;
            let repo = RepoId::parse(&check.repo)
                .ok_or_else(|| Error::Forge("queued review has an invalid repository".into()))?;
            if check.retired {
                let check_id = match check.check.check_id {
                    Some(id) => Some(id),
                    None => {
                        clients
                            .read
                            .check_run_by_external_id(
                                &repo,
                                &check.check.head_sha,
                                &check.check.external_id,
                            )
                            .await?
                    }
                };
                if let Some(check_id) = check_id {
                    if !jobs.queued_check_claim_current(&check, now()).await? {
                        return Ok(());
                    }
                    if check.check.check_id.is_none()
                        && !jobs.record_queued_check_id(&check, check_id, now()).await?
                    {
                        return Ok(());
                    }
                    if !jobs.queued_check_claim_current(&check, now()).await? {
                        return Ok(());
                    }
                    crate::app::apply::retire_queued_review_status(
                        clients.write.as_ref(),
                        &repo,
                        check_id,
                        &check.check.head_sha,
                    )
                    .await?;
                } else {
                    // The exact head/external ID was not found. Publish a
                    // terminal status directly, never another queued run.
                    if !jobs.queued_check_claim_current(&check, now()).await? {
                        return Ok(());
                    }
                    let check_id = crate::app::apply::publish_superseded_review_status(
                        clients.write.as_ref(),
                        &repo,
                        &check.check.head_sha,
                        &check.check.external_id,
                    )
                    .await?;
                    if !jobs.record_queued_check_id(&check, check_id, now()).await? {
                        return Ok(());
                    }
                }
                let _ = jobs.complete_retired_check(&check, now()).await?;
            } else {
                let check_id = match clients
                    .read
                    .check_run_by_external_id(
                        &repo,
                        &check.check.head_sha,
                        &check.check.external_id,
                    )
                    .await?
                {
                    Some(id) => id,
                    None => {
                        if !jobs.queued_check_claim_current(&check, now()).await? {
                            return Ok(());
                        }
                        if !jobs.mark_queue_post_attempted(&check, now()).await? {
                            return Ok(());
                        }
                        if !jobs.queued_check_claim_current(&check, now()).await? {
                            return Ok(());
                        }
                        crate::app::apply::publish_queued_review_status(
                            clients.write.as_ref(),
                            &repo,
                            &check.check.head_sha,
                            check.reviews_ahead,
                            &check.check.external_id,
                        )
                        .await?
                    }
                };
                let _ = jobs.record_queued_check_id(&check, check_id, now()).await?;
            }
            Ok::<(), Error>(())
        })
        .await;
        let attempt = match attempt {
            Ok(result) => result,
            Err(_) => Err(Error::Forge(
                "queued review status operation exceeded its deadline".into(),
            )),
        };
        if let Err(err) = attempt {
            first_error.get_or_insert(err);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Lazily mint a status-only GitHub client after admission policy allows its
/// first use. The factory itself performs no token exchange or forge request.
type QueueStatusClientFactory = Arc<
    dyn Fn(u64) -> futures::future::BoxFuture<'static, Result<QueueStatusClients>> + Send + Sync,
>;

const QUEUE_STATUS_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);
const QUEUE_STATUS_DRAIN_BATCH: usize = 16;

struct QueueStatusClients {
    read: Arc<dyn crate::ports::forge::ForgeRead>,
    write: Arc<dyn ForgeWrite>,
}

fn github_queue_status_client(auth: Arc<AppAuth>, _installation: u64) -> QueueStatusClientFactory {
    Arc::new(move |installation| {
        let auth = auth.clone();
        async move {
            let token = auth.installation_token(installation).await?;
            let read = crate::forge::github::GitHubRead::for_app(&token, auth.app_id()?)?;
            let write = crate::forge::github::GitHubWrite::new(&token)?;
            Ok(QueueStatusClients {
                read: Arc::new(read),
                write: Arc::new(write),
            })
        }
        .boxed()
    })
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// Drain the bounded review queue, acquiring capacity before each worker is
/// spawned. At most one queue-drainer waits on the semaphore; pending requests
/// remain bounded channel entries.
async fn drain_review_queue<F>(
    mut receiver: mpsc::Receiver<()>,
    permits: Arc<Semaphore>,
    max_concurrency: usize,
    jobs: Arc<dyn ReviewJobStore>,
    owner: Arc<str>,
    accepting: Arc<std::sync::atomic::AtomicBool>,
    mut start: F,
) where
    F: FnMut(OwnedReview, tokio::sync::OwnedSemaphorePermit),
{
    let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            signal = receiver.recv() => {
                if signal.is_none() {
                    return;
                }
            }
            _ = poll.tick() => {}
        }
        if !accepting.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        match jobs
            .claim_next_with_limit(&owner, unix_time_ms(), max_concurrency)
            .await
        {
            Ok(Some(job)) => start(job, permit),
            Ok(None) => drop(permit),
            Err(err) => {
                drop(permit);
                tracing::error!(%err, "could not claim durable review work");
            }
        }
    }
}

async fn run_owned_review(
    state: AppState,
    owned: OwnedReview,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let store = state.review_jobs.clone();
    let now_ms = unix_time_ms();
    if !state
        .accepting_reviews
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        if let Err(err) = store.defer(&owned, now_ms, now_ms).await {
            tracing::error!(%err, "could not defer a review during shutdown");
        }
        return;
    }

    let initial_renewal_started = tokio::time::Instant::now();
    match store.renew(&owned, unix_time_ms()).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            tracing::error!(%err, job_id = %owned.request.id.0, "could not establish initial review ownership heartbeat");
            return;
        }
    }
    let (lost_tx, lost_rx) = watch::channel(false);
    let (owner_until_tx, owner_until_rx) =
        watch::channel(conservative_owner_deadline(initial_renewal_started));
    let _heartbeat_guard = spawn_review_heartbeat(
        store.clone(),
        owned.clone(),
        lost_tx,
        owner_until_tx,
        std::time::Duration::from_secs(15),
    );

    let request = &owned.request;
    let elapsed_ms = unix_time_ms().saturating_sub(request.accepted_at_ms).max(0) as u64;
    let queued_at = tokio::time::Instant::now()
        .checked_sub(std::time::Duration::from_millis(elapsed_ms))
        .unwrap_or_else(tokio::time::Instant::now);
    let job = ReviewJob {
        repo: request.repo.clone(),
        number: request.number,
        author: request.author.clone(),
        installation: request.installation,
        mode: match request.mode {
            ReviewJobMode::Incremental => Mode::Incremental,
            ReviewJobMode::Full => Mode::Full,
        },
        delivery: request.delivery.clone(),
        expected_head: request.head_sha.clone(),
        queued_at,
        owned: Some(owned.clone()),
        queue_check: owned.review_check.clone(),
    };
    let disposition = handle_review(
        state,
        job,
        Some(permit),
        Some(lost_rx),
        Some(owner_until_rx),
    )
    .await;

    let result =
        record_owned_disposition(store.as_ref(), &owned, disposition, unix_time_ms()).await;
    match result {
        Some(Ok(true)) => {}
        Some(Ok(false)) => {
            tracing::warn!(job_id = %owned.request.id.0, "durable review ownership changed before recording its outcome")
        }
        Some(Err(err)) => {
            tracing::error!(%err, job_id = %owned.request.id.0, "could not record durable review outcome")
        }
        None => {}
    }
}

async fn record_owned_disposition(
    store: &dyn ReviewJobStore,
    owned: &OwnedReview,
    disposition: WorkerDisposition,
    now_ms: i64,
) -> Option<Result<bool>> {
    match disposition {
        WorkerDisposition::Terminal(TerminalOutcome::Deadline) => {
            finish_or_yield(store, owned, now_ms, || {
                store.finish_deadline(owned, now_ms)
            })
            .await
        }
        WorkerDisposition::Terminal(outcome) => {
            finish_or_yield(store, owned, now_ms, || {
                store.finish(owned, outcome, now_ms, now_ms)
            })
            .await
        }
        WorkerDisposition::Deferred(not_before_ms) => {
            store.defer(owned, now_ms, not_before_ms).await
        }
        WorkerDisposition::Yielded => store.defer_yield(owned, now_ms).await,
        WorkerDisposition::OwnershipLost => return None,
    }
    .into()
}

/// Record control-plane completion before the worker's timing guard is dropped.
/// A superseding hint cancels this worker, but its terminal outcome describes
/// the deliberate yield; a publication completed before that hint stays success.
fn finish_control_timing(
    timing: &crate::review_timing::ReviewTiming,
    disposition: WorkerDisposition,
) -> WorkerDisposition {
    match disposition {
        WorkerDisposition::Yielded | WorkerDisposition::Terminal(TerminalOutcome::Superseded) => {
            timing.finish("superseded")
        }
        WorkerDisposition::Terminal(TerminalOutcome::Published) => timing.finish("success"),
        WorkerDisposition::Terminal(TerminalOutcome::Deadline) => timing.finish("deadline"),
        WorkerDisposition::Terminal(TerminalOutcome::Exhausted) => timing.finish("failure"),
        WorkerDisposition::Terminal(TerminalOutcome::Declined) => timing.finish("declined"),
        WorkerDisposition::Deferred(_) | WorkerDisposition::OwnershipLost => {}
    }
    disposition
}

async fn classify_retry_boundary_error<Valid>(
    err: &Error,
    jobs: &dyn ReviewJobStore,
    owned: Option<&OwnedReview>,
    local_owner_valid: Valid,
) -> Result<Option<WorkerDisposition>>
where
    Valid: Fn() -> bool,
{
    if matches!(err, Error::ReviewOwnershipLost) {
        return Ok(Some(WorkerDisposition::OwnershipLost));
    }
    if matches!(err, Error::ReviewSuperseded) {
        if !local_owner_valid() {
            return Ok(Some(WorkerDisposition::OwnershipLost));
        }
        let Some(owned) = owned else {
            return Ok(Some(WorkerDisposition::Terminal(
                TerminalOutcome::Superseded,
            )));
        };
        let owns = jobs.owns(owned, unix_time_ms()).await?;
        if !local_owner_valid() || !owns {
            return Ok(Some(WorkerDisposition::OwnershipLost));
        }
        let should_continue = jobs.should_continue(owned, unix_time_ms()).await?;
        if !local_owner_valid() {
            return Ok(Some(WorkerDisposition::OwnershipLost));
        }
        return Ok(Some(if should_continue {
            WorkerDisposition::Terminal(TerminalOutcome::Superseded)
        } else {
            WorkerDisposition::Yielded
        }));
    }
    Ok(None)
}

/// Reconcile a lane failure with durable ownership before it can be retried or
/// published as a failure for a head that has already been superseded.
async fn ownership_fence_disposition<Valid>(
    failure: &Error,
    execution: &crate::app::review_execution::ReviewExecution,
    jobs: &dyn ReviewJobStore,
    owned: Option<&OwnedReview>,
    local_owner_valid: Valid,
) -> Result<Option<WorkerDisposition>>
where
    Valid: Fn() -> bool,
{
    if matches!(failure, Error::ReviewOwnershipLost) {
        execution.cancel();
        return Ok(Some(WorkerDisposition::OwnershipLost));
    }
    let Some(owned) = owned else {
        return Ok(execution
            .is_cancelled()
            .then_some(WorkerDisposition::OwnershipLost));
    };
    let fence = match check_owned_work(jobs, owned, &local_owner_valid).await {
        Err(err @ (Error::ReviewOwnershipLost | Error::ReviewSuperseded)) => err,
        Err(_) | Ok(()) => {
            return Ok(execution
                .is_cancelled()
                .then_some(WorkerDisposition::OwnershipLost));
        }
    };
    execution.cancel();
    match classify_retry_boundary_error(&fence, jobs, Some(owned), local_owner_valid).await {
        Ok(Some(disposition)) => Ok(Some(disposition)),
        Ok(None) | Err(_) => Ok(Some(WorkerDisposition::OwnershipLost)),
    }
}

async fn finish_or_yield<F, Fut>(
    store: &dyn ReviewJobStore,
    owned: &OwnedReview,
    now_ms: i64,
    finish: F,
) -> Result<bool>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let finished = finish().await?;
    if !finished
        && store.owns(owned, now_ms).await?
        && !store.should_continue(owned, now_ms).await?
    {
        store.defer_yield(owned, now_ms).await
    } else {
        Ok(finished)
    }
}

async fn finalize_exhausted_review<CheckOwner, CheckOwnerFuture, Conclude, ConcludeFuture>(
    timing: &crate::review_timing::ReviewTiming,
    check_owner: CheckOwner,
    conclude: Conclude,
) -> WorkerDisposition
where
    CheckOwner: FnOnce() -> CheckOwnerFuture,
    CheckOwnerFuture: std::future::Future<Output = Result<()>>,
    Conclude: FnOnce() -> ConcludeFuture,
    ConcludeFuture: std::future::Future<Output = ()>,
{
    if check_owner().await.is_err() {
        return WorkerDisposition::OwnershipLost;
    }
    conclude().await;
    timing.finish("failure");
    WorkerDisposition::Terminal(TerminalOutcome::Exhausted)
}

fn shutdown_disposition(review: &InnerReview, now_ms: i64) -> Option<WorkerDisposition> {
    matches!(review, InnerReview::Shutdown).then_some(WorkerDisposition::Deferred(now_ms))
}

enum PaidWork<T> {
    Started(T),
    Exhausted,
    OwnershipLost,
}

async fn with_paid_review_attempt<T, F, Fut>(
    store: &dyn ReviewJobStore,
    owned: &OwnedReview,
    now_ms: i64,
    work: F,
) -> Result<PaidWork<T>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    match store.begin_paid(owned, now_ms).await? {
        PaidStart::Started => Ok(PaidWork::Started(work().await)),
        PaidStart::Exhausted => Ok(PaidWork::Exhausted),
        PaidStart::OwnershipLost => Ok(PaidWork::OwnershipLost),
    }
}

/// Keep a detached Tokio task from renewing ownership after its worker future
/// has been cancelled or unwound.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn spawn_review_heartbeat(
    store: Arc<dyn ReviewJobStore>,
    owned: OwnedReview,
    lost: watch::Sender<bool>,
    owner_until: watch::Sender<tokio::time::Instant>,
    interval: std::time::Duration,
) -> AbortOnDrop<()> {
    AbortOnDrop(tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let renewal_started = tokio::time::Instant::now();
            match store.renew(&owned, unix_time_ms()).await {
                Ok(true) => {
                    let _ = owner_until.send(conservative_owner_deadline(renewal_started));
                }
                Ok(false) => {
                    let _ = lost.send(true);
                    return;
                }
                Err(err) => {
                    tracing::error!(%err, "could not renew durable review ownership");
                    let _ = lost.send(true);
                    return;
                }
            }
        }
    }))
}

fn conservative_owner_deadline(started: tokio::time::Instant) -> tokio::time::Instant {
    started
        + std::time::Duration::from_millis(
            (crate::ports::review_job::REVIEW_OWNER_TTL_MS - 10_000) as u64,
        )
}

/// Claim the delivery, then run whatever it asked for.
///
/// Runs off the request path so nothing here is racing GitHub's clock.
async fn dispatch(
    state: AppState,
    action: Action,
    delivery: Option<String>,
    event: String,
    review_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    mode: Mode,
    queued_at: Option<tokio::time::Instant>,
) {
    if let Some(delivery_id) = &delivery {
        match state.store.claim_delivery(delivery_id, &event).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(delivery = %delivery_id, "already handled");
                return;
            }
            Err(err) => {
                // The delivery is already acknowledged, so dropping work
                // would be silent. The revision lease prevents duplicate work.
                tracing::error!(%err, delivery = %delivery_id, "could not claim the delivery; proceeding unclaimed");
            }
        }
    }

    match action {
        // Both were answered on the request path and never reach here.
        Action::TrackDraft | Action::Ignore(_) => {}
        Action::Review {
            repo,
            number,
            author,
            installation,
            head_sha,
        } => {
            let _ = handle_review(
                state,
                ReviewJob {
                    repo,
                    number,
                    author,
                    installation,
                    mode,
                    delivery,
                    expected_head: head_sha,
                    queued_at: queued_at.unwrap_or_else(tokio::time::Instant::now),
                    owned: None,
                    queue_check: None,
                },
                review_permit,
                None,
                None,
            )
            .await;
        }
        Action::TriageIssue {
            repo,
            number,
            author,
            installation,
        } => {
            handle_triage(state, repo, number, author, installation).await;
        }
        Action::AutoMerge {
            repo,
            numbers,
            installation,
        } => {
            for number in numbers {
                // Two consumers of the same event, and the order matters: the
                // `e2e` check run has to be concluded before the merge gate
                // reads it, or a pull request whose only red job just
                // finished would be evaluated against the `Neutral` the
                // review left behind. `settle_e2e` publishes the terminal
                // conclusion; the merge evaluation then sees it.
                tokio::spawn(handle_check_completed(
                    state.clone(),
                    repo.clone(),
                    number,
                    installation,
                ));
            }
        }
    }
}

/// A check completed on a pull request: settle the `e2e` lane if it was
/// waiting on that check, then reconsider the merge.
///
/// Not gated on the *deployment's* `enabled_lanes()`: `review.lanes` is a
/// repository override (`crate::config::remote::overlay`), so a repository
/// can run `e2e` while the deployment default does not. Gating on the
/// deployment list here would skip settlement for exactly that repository
/// and leave its `tinysweeper/e2e` check `Neutral` forever. The cheap guard
/// that actually matters — is anything being watched — lives inside
/// `settle_e2e_inner`, keyed off the stored review state rather than config.
async fn handle_check_completed(state: AppState, repo: String, number: u64, installation: u64) {
    if let Err(err) = settle_e2e_inner(&state, &repo, number, installation).await {
        // Logged and dropped, like auto-merge: the watch stays in the store,
        // and the next completion event on the same head retries it.
        tracing::error!(%err, %repo, number, "could not settle the e2e check run");
    }
    handle_automerge(state, repo, number, installation).await;
}

async fn settle_e2e_inner(
    state: &AppState,
    repo: &str,
    number: u64,
    installation: u64,
) -> Result<()> {
    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    // The cheap exit before any credential is minted: most completions land
    // on pull requests with nothing to settle.
    let key = crate::state::key(repo, number);
    let watching = crate::ports::review_state::ReviewStateStore::load_state(&state.store, &key)
        .await?
        .is_some_and(|reviewed| reviewed.e2e.is_some());
    if !watching {
        return Ok(());
    }

    // Serialised with the review's own lease shape: several jobs finishing at
    // once is the normal case, and two settlements racing would publish the
    // same check twice.
    //
    // Contention is retried rather than dropped: several jobs from the same
    // matrix typically conclude within milliseconds of each other, and the
    // event that loses the race is often the one whose job just completed
    // the watch. Bailing outright on that event would leave the check
    // `Neutral` until some *other*, unrelated completion happened to retry
    // it — not guaranteed to ever happen. A short bounded retry lets the
    // loser re-attempt once the winner (a settlement that reads checks,
    // possibly publishes, and releases) has had time to finish.
    let lease = format!("{repo}#e2e-settle-{number}");
    let mut acquired = state.store.claim_lease(&lease, "server").await?;
    for _ in 0..LEASE_CONTENTION_RETRIES {
        if acquired {
            break;
        }
        tokio::time::sleep(LEASE_CONTENTION_BACKOFF).await;
        acquired = state.store.claim_lease(&lease, "server").await?;
    }
    if !acquired {
        tracing::debug!(%lease, "another worker is still settling this e2e check");
        return Ok(());
    }

    let token = state.auth.installation_token(installation).await;
    let outcome = match token {
        Ok(token) => {
            let read = crate::forge::github::GitHubRead::new(&token);
            let write = crate::forge::github::GitHubWrite::new(&token);
            match (read, write) {
                (Ok(read), Ok(write)) => {
                    // The repository's own policy, not the deployment
                    // default: `lanes.e2e.fail_on` is a repository override
                    // like `review.lanes` above, and settling against the
                    // deployment default can publish `Success` for a job the
                    // repository's own threshold would have failed. Read at
                    // the pull request's base tip, on the same reasoning as
                    // `crate::config::remote::overlay`'s other callers.
                    let live = read.pull_request(&repo_id, number).await;
                    match live {
                        Ok(live) => {
                            let overlay = crate::config::remote::overlay(
                                &read,
                                &repo_id,
                                &live.base_sha,
                                &state.config.config,
                            )
                            .await;
                            crate::app::apply::settle_e2e(
                                &read,
                                &write,
                                &overlay.config,
                                &state.store,
                                &repo_id,
                                number,
                            )
                            .await
                        }
                        Err(err) => Err(err),
                    }
                }
                (Err(err), _) | (_, Err(err)) => Err(err),
            }
        }
        Err(err) => Err(err),
    };

    if let Err(err) = state.store.release_lease(&lease).await {
        tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
    }

    match outcome? {
        crate::app::apply::E2eSettlement::Published(conclusion) => {
            tracing::info!(%repo, number, ?conclusion, "settled the e2e check run");
        }
        other => tracing::debug!(%repo, number, ?other, "e2e check run not settled"),
    }
    Ok(())
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

    let plan = run_triage_job(&state.permits, &state.triage_permits, || async {
        // Keyed on the issue rather than a SHA — an issue has no head commit — so
        // two deliveries for the same edit cannot both pay for a triage.
        let lease = format!("{repo}#issue-{number}");
        if !state.store.claim_lease(&lease, "server").await? {
            tracing::debug!(%lease, "another worker holds this triage");
            return Ok(None);
        }

        let outcome = triage_and_apply(state, &repo_id, number, installation).await;
        if let Err(err) = state.store.release_lease(&lease).await {
            tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
        }
        outcome.map(Some)
    })
    .await?;
    let Some(plan) = plan else {
        return Ok(());
    };
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

fn triage_permit_pool(
    review_pool: &Arc<Semaphore>,
    triage_pool: &Arc<Semaphore>,
) -> Arc<Semaphore> {
    let _ = review_pool;
    triage_pool.clone()
}

async fn run_triage_job<T, F, Fut>(
    review_pool: &Arc<Semaphore>,
    triage_pool: &Arc<Semaphore>,
    work: F,
) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let pool = triage_permit_pool(review_pool, triage_pool);
    with_triage_permit(&pool, work).await
}

async fn with_triage_permit<T, F, Fut>(pool: &Arc<Semaphore>, work: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let _permit = pool
        .clone()
        .acquire_owned()
        .await
        .map_err(|err| Error::Forge(err.to_string()))?;
    work().await
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
    let model = state.models.text();

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

/// The pull request triage button's and the periodic sweep's way into the job.
///
/// Answers synchronously, like the auto-merge button and for the same reason:
/// there is no model in this path, so the caller can be handed what it
/// concluded rather than being sent to read the log.
struct TriageDispatch {
    state: AppState,
}

#[async_trait::async_trait]
impl Triages for TriageDispatch {
    async fn triage(&self, repo: &RepoId, number: Option<u64>) -> Result<Vec<PrTriageReport>> {
        // Refused once, up front. An operator who has not turned the sweep on
        // wants to be told that once rather than a hundred times with a number
        // attached.
        if !self.state.config.config.pr_triage.enabled {
            return Err(Error::Forge(
                "`[pr_triage] enabled` is false in the deployment's configuration".into(),
            ));
        }

        let installation = self
            .state
            .auth
            .installation_for_repo(&repo.owner, &repo.name)
            .await?;

        // One lease for the whole sweep, keyed on the repository rather than on
        // a pull request: two sweeps running at once would each read the other
        // half-applied state, and the second would post a second comment on
        // everything the first had not finished labelling.
        let lease = format!("{repo}#pr-triage");
        if !self.state.store.claim_lease(&lease, "server").await? {
            return Err(Error::Forge(
                "another sweep of this repository is already running".into(),
            ));
        }

        let outcome = self.sweep_and_apply(repo, number, installation).await;

        if let Err(err) = self.state.store.release_lease(&lease).await {
            tracing::error!(%err, %lease, "could not release the sweep lease; it will expire");
        }

        outcome
    }
}

impl TriageDispatch {
    /// Read, decide, then publish with a token minted afterwards.
    ///
    /// The read token and the write token are separate mints even though the
    /// installation is the same, so the sweep keeps the shape every other job
    /// here has: the half that decides never holds a handle that could write.
    async fn sweep_and_apply(
        &self,
        repo: &RepoId,
        number: Option<u64>,
        installation: u64,
    ) -> Result<Vec<PrTriageReport>> {
        let read_token = self.state.auth.installation_token(installation).await?;
        let read = crate::forge::github::GitHubRead::new(&read_token)?;

        let outcome = crate::pr_triage::sweep(
            &read,
            &self.state.config.config,
            repo,
            number,
            // Maintainer protection is expressed as
            // `pr_triage.close.protected_authors` until the forge port can
            // report a repository's collaborators — the same gap issue triage
            // has, and an invented list would be worse than an empty one.
            &[],
        )
        .await?;

        if let Some(reason) = outcome.skipped {
            return Err(Error::Forge(reason.to_string()));
        }

        for (number, why) in &outcome.unread {
            tracing::info!(%repo, number, why, "pull request skipped by the sweep");
        }

        let write_token = self.state.auth.installation_token(installation).await?;
        let write = crate::forge::github::GitHubWrite::new(&write_token)?;
        // The same read handle the sweep used. `apply_all` re-fetches every
        // pull request it is about to close and re-runs the gate against its
        // live state, because a sweep of a hundred takes minutes and a
        // maintainer can intervene inside them.
        Ok(crate::pr_triage::apply_all(
            &read,
            &write,
            &self.state.config.config,
            repo,
            &outcome.plans,
            &[],
        )
        .await)
    }
}

/// Start the periodic pull request sweep, if it is configured.
///
/// Spawned once at boot and never restarted: a task that dies takes the
/// periodic sweep with it until the next deploy, which is loud enough to
/// notice and much better than a supervisor quietly retrying a sweep that
/// fails for a reason nobody has looked at.
fn spawn_triage_sweeps(state: AppState, triages: Arc<dyn Triages>) {
    let policy = &state.config.config.pr_triage;
    if !policy.enabled {
        tracing::info!("`[pr_triage] enabled` is false; no periodic pull request sweep");
        return;
    }
    let Some(minutes) = policy.sweep_every_minutes.filter(|every| *every > 0) else {
        tracing::info!(
            "pull request triage is on but `sweep_every_minutes` is unset;              it runs only from /admin/pr-triage"
        );
        return;
    };
    let repositories = policy.sweep_repositories.clone();
    if repositories.is_empty() {
        tracing::warn!(
            "`[pr_triage] sweep_every_minutes` is set but `sweep_repositories` is empty;              there is nothing to sweep"
        );
        return;
    }

    tracing::info!(
        every_minutes = minutes,
        ?repositories,
        "the periodic pull request sweep is on"
    );

    tokio::spawn(async move {
        let period = std::time::Duration::from_secs(u64::from(minutes) * 60);
        let mut ticker = tokio::time::interval(period);
        // A sweep of a large repository can outlast its own interval. Tokio's
        // default then fires every missed tick back to back, so the sweep
        // restarts with no pause and keeps the installation permanently rate
        // limited. `Delay` skips the backlog and waits a full period.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately, which is not what a deploy wants:
        // a restart loop would sweep on every crash. Consume it here so the
        // first real sweep is one interval after boot.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            for name in &repositories {
                let Some(repo) = RepoId::parse(name) else {
                    tracing::error!(%name, "`pr_triage.sweep_repositories` has a bad entry");
                    continue;
                };
                match triages.triage(&repo, None).await {
                    Ok(reports) => {
                        let closed = reports
                            .iter()
                            .filter(|report| {
                                report.outcome == crate::pr_triage::apply::Outcome::Closed
                            })
                            .count();
                        tracing::info!(
                            %repo,
                            considered = reports.len(),
                            closed,
                            "periodic pull request sweep finished"
                        );
                    }
                    // One repository failing must not stop the timer: the next
                    // tick tries again, and a permanent failure shows up as a
                    // repeating log line rather than as silence.
                    Err(err) => tracing::error!(%err, %repo, "periodic pull request sweep failed"),
                }
            }
        }
    });
}

/// How many open pull requests the manual buttons look at before picking the
/// newest `MAX_MANUAL_REVIEWS` of them.
///
/// The port lists oldest first, because duplicate detection needs the
/// originals. The manual buttons want the opposite: an operator pressing
/// "review everything" twice must not enqueue the same twenty oldest pull
/// requests both times and starve everything opened since. So they read a wider
/// window and take the newest end of it.
///
/// Wide enough to cover the whole backlog, and that is the point rather than
/// generosity: a window that truncates from the *oldest* end still hands the
/// caller the newest of what it read, which on a repository with more open pull
/// requests than the window is not the newest of what exists. Twenty pages of
/// a hundred, on a button pressed by hand a few times a year.
const MANUAL_SCAN_LIMIT: usize = 2_000;

/// The numbers of a repository's most recent open pull requests, capped.
///
/// A thin wrapper over the port so the two manual buttons do not each grow
/// their own idea of what "open" means.
async fn open_numbers(
    read: &dyn crate::ports::forge::ForgeRead,
    repo: &RepoId,
    limit: usize,
) -> Result<Vec<u64>> {
    let mut numbers: Vec<u64> = read
        .open_pull_requests(repo, MANUAL_SCAN_LIMIT)
        .await?
        .into_iter()
        .map(|pull_request| pull_request.number)
        .collect();

    // Newest first, then capped, so the cap drops the oldest rather than the
    // newest. Sorted here rather than trusted from the adapter: the port
    // promises ascending, and a manual button is not the place to depend on
    // that promise being reversed.
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    numbers.truncate(limit);
    Ok(numbers)
}

/// The manual review path's way into the worker.
///
/// Everything a webhook delivery supplies and an operator cannot — the
/// installation, and which pull requests are open — is resolved here, on the
/// request, so the operator gets a real answer rather than a log line.
struct ManualDispatch {
    state: AppState,
}

async fn with_manual_review_admission<T, F, Fut>(permits: Arc<Semaphore>, work: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let permit = permits
        .try_acquire_owned()
        .map_err(|_| Error::ReviewCapacityFull)?;
    let result = work().await;
    drop(permit);
    result
}

async fn with_durable_manual_capacity<T, F, Fut>(
    store: &dyn ReviewJobStore,
    count: usize,
    work: F,
) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    if !store.has_capacity_for(count).await? {
        return Err(Error::ReviewCapacityFull);
    }
    work().await
}

async fn enqueue_manual_jobs<'a, F>(
    sender: &mpsc::Sender<()>,
    store: &dyn ReviewJobStore,
    repo: &str,
    installation: u64,
    numbers: Vec<u64>,
    mut resolve: F,
) -> Result<Vec<u64>>
where
    F: FnMut(
        u64,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<crate::forge::types::PullRequest>> + Send + 'a>,
    >,
{
    // Reserve the whole expansion before per-PR reads, then publish none until
    // all resolutions succeed. Dropping this iterator returns every slot on an
    // error, so a partial manual expansion cannot leak queue capacity or jobs.
    let queue_slots = sender
        .try_reserve_many(numbers.len())
        .map_err(|_| Error::ReviewCapacityFull)?;

    let mut jobs = Vec::with_capacity(numbers.len());
    for number in numbers {
        let pull_request = resolve(number).await?;
        jobs.push((
            number,
            ReviewRequest {
                id: crate::ports::review_job::ReviewJobId(format!(
                    "manual:{}:{}",
                    bson::oid::ObjectId::new().to_hex(),
                    number
                )),
                repo: repo.to_owned(),
                number,
                author: pull_request.author,
                installation,
                head_sha: Some(pull_request.head_sha),
                delivery: None,
                event: "manual-review".into(),
                mode: ReviewJobMode::Full,
                accepted_at_ms: unix_time_ms(),
            },
        ));
    }

    let admitted = store
        .admit_batch(jobs.iter().map(|(_, job)| job.clone()).collect())
        .await?;
    match admitted {
        Admission::Admitted => {}
        Admission::Duplicate => return Ok(Vec::new()),
        Admission::Full => return Err(Error::ReviewCapacityFull),
    }

    let mut queued = Vec::with_capacity(jobs.len());
    for ((number, _job), slot) in jobs.into_iter().zip(queue_slots) {
        slot.send(());
        queued.push(number);
    }
    Ok(queued)
}

#[async_trait::async_trait]
impl FullReviews for ManualDispatch {
    async fn enqueue(&self, repo: &RepoId, number: Option<u64>) -> Result<Vec<u64>> {
        with_manual_review_admission(self.state.permits.clone(), || async {
            with_durable_manual_capacity(self.state.review_jobs.as_ref(), 1, || async {
                // Manual expansion needs live GitHub reads to resolve the queued PRs.
                // Refuse immediately when every worker slot is busy, before minting a
                // token or making those reads. The permit is held until all bounded
                // queue slots and job data are ready.

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
                    // Through the port, which is also where pull request triage reads
                    // them: one definition of "the open pull requests", so the button
                    // and the sweep can never disagree about what is open.
                    None => open_numbers(&forge, repo, MAX_MANUAL_REVIEWS).await?,
                };

                // The author is what trust checks use; enqueue only once every
                // requested PR has resolved successfully.
                use crate::ports::forge::ForgeRead;
                let repo_name = repo.to_string();
                enqueue_manual_jobs(
                    &self.state.review_queue,
                    self.state.review_jobs.as_ref(),
                    &repo_name,
                    installation,
                    numbers,
                    |number| Box::pin(forge.pull_request(repo, number)),
                )
                .await
            })
            .await
        })
        .await
    }
}

/// The manual auto-merge button's way into the policy.
///
/// Unlike the review button this answers synchronously. There is no model in
/// the path — four reads and possibly a merge — so the operator gets the
/// refusals back rather than having to go and read the log for them, and the
/// refusals are the point of pressing it.
struct MergeDispatch {
    state: AppState,
}

#[async_trait::async_trait]
impl Merges for MergeDispatch {
    async fn evaluate(&self, repo: &RepoId, number: Option<u64>) -> Result<Vec<MergeReport>> {
        // Refused up front rather than reported per pull request. An operator
        // who has not turned the feature on wants to be told that once, not
        // twenty times with a number attached.
        if !self.state.config.config.automerge.enabled {
            return Err(Error::Forge(
                "`[automerge] enabled` is false in the deployment's configuration".into(),
            ));
        }

        let installation = self
            .state
            .auth
            .installation_for_repo(&repo.owner, &repo.name)
            .await?;
        let token = self.state.auth.installation_token(installation).await?;
        let read = crate::forge::github::GitHubRead::new(&token)?;

        let numbers = match number {
            Some(number) => vec![number],
            None => open_numbers(&read, repo, MAX_MANUAL_REVIEWS).await?,
        };

        let mut reports = Vec::new();
        for number in numbers {
            // Sequential, not concurrent. Each merge changes the default
            // branch, which can make the *next* pull request unmergeable;
            // evaluating them all against the state that held before the first
            // merge would be evaluating a snapshot that no longer exists.
            let outcome = automerge_inner_reporting(&self.state, repo, number, installation).await;
            reports.push(match outcome {
                // Busy is not a refusal: the policy did not decide anything,
                // another worker is deciding it. Reporting it as one would put
                // a reason in front of the operator that no threshold produced.
                Ok(None) => MergeReport {
                    number,
                    outcome: "busy",
                    detail: Some("a webhook delivery is already evaluating this one".into()),
                },
                Ok(Some(Outcome::Merged { .. })) => MergeReport {
                    number,
                    outcome: "merged",
                    detail: None,
                },
                Ok(Some(Outcome::Refused(refusal))) => MergeReport {
                    number,
                    outcome: "refused",
                    detail: Some(refusal.to_string()),
                },
                Ok(Some(Outcome::Rejected { method, reason })) => MergeReport {
                    number,
                    outcome: "rejected",
                    detail: Some(format!("the forge refused a `{method}` merge: {reason}")),
                },
                // One pull request failing to evaluate must not abandon the
                // rest of the sweep, which is the operator's whole request.
                Err(err) => MergeReport {
                    number,
                    outcome: "error",
                    detail: Some(err.to_string()),
                },
            });
        }

        Ok(reports)
    }
}

/// Evaluate one pull request and hand back the outcome rather than logging it.
///
/// `None` means another worker holds the lease. Shared with the webhook path
/// deliberately: a sweep running while a delivery is being handled must not
/// evaluate the same pull request twice and race to merge it.
async fn automerge_inner_reporting(
    state: &AppState,
    repo: &RepoId,
    number: u64,
    installation: u64,
) -> Result<Option<Outcome>> {
    let lease = format!("{repo}#automerge-{number}");
    if !state.store.claim_lease(&lease, "server").await? {
        return Ok(None);
    }

    let outcome = evaluate_and_merge(state, repo, number, installation).await;

    if let Err(err) = state.store.release_lease(&lease).await {
        tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
    }

    outcome.map(Some)
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
/// `Mode::Full` is exactly `review.incremental = false` for this one run.
/// That single flag is this pull request's own incremental state — the
/// prior findings read off it, and the cached evidence in the store — so
/// turning it off makes this one run argue from scratch, and leaves the
/// stored state intact for the webhook path. Nothing is deleted: a manual
/// review is an extra opinion, not a reset, and destroying the record would
/// make the *next* webhook review duplicate its comments too.
///
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

/// A published in-progress check, and everything needed to conclude it.
///
/// Exists because the check is *opened* deep inside `review_inner`, where the
/// head SHA is first known, and *closed* by whichever path the review ends on
/// — including the error paths, which unwind past every local in that function.
/// Holding it in a slot the caller owns is what makes "always concluded"
/// structural rather than a rule each `return` has to remember.
#[derive(Debug, Clone)]
struct ReviewStatus {
    repo: RepoId,
    /// The check run to update. Never re-created: a second POST of the same
    /// name leaves the first one pending forever, and a pending check refuses
    /// auto-merge on that commit for good.
    check_id: u64,
    head_sha: String,
    installation: u64,
    stage: crate::ports::review_job::ReviewCheckStage,
    owned: Option<OwnedReview>,
    /// Durable review-hub comment opened alongside the status check.
    hub_comment_id: Option<u64>,
    /// Last completed body, retained if this pass fails.
    prior_hub_body: Option<String>,
}

/// Where a review's in-progress check lives between opening and concluding.
///
/// `None` means there is nothing to conclude — the review returned before it
/// had a SHA to pin a check to, or another worker already holds the lease for
/// this commit and owns the check that goes with it.
type StatusSlot = Arc<std::sync::Mutex<Option<ReviewStatus>>>;

/// What one review carries from `handle_review` down to the lanes.
///
/// The three things every attempt shares: the mode it runs in, the check it
/// owns, and the wall-clock deadline it must beat. Bundled so a retry cannot
/// re-derive any of them differently from the first attempt.
#[derive(Clone)]
struct Run {
    mode: Mode,
    slot: StatusSlot,
    timing: crate::review_timing::ReviewTiming,
    /// Fixed when the worker starts, not per attempt. Queue wait is measured
    /// separately and does not consume the execution deadline.
    deadline: tokio::time::Instant,
    /// Shared physical and monetary model scope retained across retries.
    execution: Arc<crate::app::review_execution::ReviewRunContext>,
    /// Revision admitted by the queue, retained across retries and all phases.
    expected_head: Option<String>,
    owned: Option<OwnedReview>,
    owner_lost: Option<watch::Receiver<bool>>,
    owner_until: Option<watch::Receiver<tokio::time::Instant>>,
    lease_busy: Arc<std::sync::atomic::AtomicBool>,
}

/// Refuse an individual paid model dispatch after durable ownership expires.
/// A process can be paused longer than its owner lease and then resume while
/// its heartbeat is still queued; phase-boundary checks alone would let that
/// stale worker spend against a job now owned by another process.
struct OwnedModel {
    inner: Arc<dyn Model>,
    execution: Arc<crate::app::review_execution::ReviewExecution>,
    jobs: Arc<dyn ReviewJobStore>,
    owned: OwnedReview,
    owner_lost: Option<watch::Receiver<bool>>,
    owner_until: Option<watch::Receiver<tokio::time::Instant>>,
}

impl OwnedModel {
    fn local_owner_valid(&self) -> bool {
        !self.owner_lost.as_ref().is_some_and(|lost| *lost.borrow())
            && !self
                .owner_until
                .as_ref()
                .is_some_and(|until| tokio::time::Instant::now() >= *until.borrow())
    }

    async fn check_owner(&self) -> Result<()> {
        let result = self.check_owner_inner().await;
        if matches!(
            &result,
            Err(Error::ReviewOwnershipLost | Error::ReviewSuperseded)
        ) {
            self.execution.cancel();
        }
        result
    }

    async fn check_owner_inner(&self) -> Result<()> {
        if !self.local_owner_valid() {
            return Err(Error::ReviewOwnershipLost);
        }
        let should_continue = self
            .jobs
            .should_continue(&self.owned, unix_time_ms())
            .await?;
        if !self.local_owner_valid() {
            return Err(Error::ReviewOwnershipLost);
        }
        if !should_continue {
            let owns = self.jobs.owns(&self.owned, unix_time_ms()).await?;
            if !self.local_owner_valid() || !owns {
                return Err(Error::ReviewOwnershipLost);
            }
            return Err(Error::ReviewSuperseded);
        }
        Ok(())
    }

    fn wrap(&self, inner: Arc<dyn Model>) -> Arc<dyn Model> {
        Arc::new(Self {
            inner,
            execution: self.execution.clone(),
            jobs: self.jobs.clone(),
            owned: self.owned.clone(),
            owner_lost: self.owner_lost.clone(),
            owner_until: self.owner_until.clone(),
        })
    }
}

#[async_trait::async_trait]
impl Model for OwnedModel {
    fn complete_reports_physical_calls(&self) -> bool {
        self.inner.complete_reports_physical_calls()
    }

    fn review_reports_physical_calls(&self) -> bool {
        self.inner.review_reports_physical_calls()
    }

    fn reports_raw_completion_usage(&self) -> bool {
        self.inner.reports_raw_completion_usage()
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.check_owner().await?;
        self.inner.complete(request).await
    }

    async fn review(
        &self,
        request: ModelRequest,
        tree: &dyn crate::ports::tree::TreeReader,
        policy: &crate::config::types::LookupPolicy,
    ) -> Result<ModelResponse> {
        self.check_owner().await?;
        self.inner.review(request, tree, policy).await
    }

    fn scoped_budget(&self, budget_usd: f64) -> Option<Arc<dyn Model>> {
        self.inner
            .scoped_budget(budget_usd)
            .map(|inner| self.wrap(inner))
    }

    fn with_review_budget(&self, limit_usd: f64) -> Option<Arc<dyn Model>> {
        self.inner
            .with_review_budget(limit_usd)
            .map(|inner| self.wrap(inner))
    }

    fn with_execution_control(
        &self,
        control: Arc<dyn crate::ports::model_execution::ModelExecutionControl>,
    ) -> Option<Arc<dyn Model>> {
        self.inner
            .with_execution_control(control)
            .map(|inner| self.wrap(inner))
    }
}

impl Run {
    /// Refuse to start the next phase if the budget is already spent.
    ///
    /// The deadline is enforced by cancellation only where cancellation is
    /// safe — the lanes. Everywhere else it is enforced here, at the boundary
    /// between phases, which is what keeps a retry that arrives after a slow
    /// failure from claiming a lease and opening a check for a review it can
    /// no longer run.
    fn check(&self, repo: &str, number: u64) -> Result<()> {
        if tokio::time::Instant::now() >= self.deadline {
            return Err(Error::timeout(
                format!("the review of {repo}#{number}"),
                REVIEW_DEADLINE,
            ));
        }
        Ok(())
    }

    fn cancel_for_ownership_fence(&self, error: &Error) {
        if matches!(error, Error::ReviewOwnershipLost | Error::ReviewSuperseded) {
            self.execution.execution().cancel();
        }
    }

    async fn check_owner(&self, state: &AppState) -> Result<()> {
        if !self.local_owner_valid() {
            return Err(Error::ReviewOwnershipLost);
        }
        if let Some(owned) = &self.owned {
            check_owner_after_await(
                || self.local_owner_valid(),
                state.review_jobs.owns(owned, unix_time_ms()),
            )
            .await?;
        }
        Ok(())
    }

    async fn check_work(&self, state: &AppState) -> Result<()> {
        self.check_owner(state).await?;
        if let Some(owned) = &self.owned {
            check_owned_work(state.review_jobs.as_ref(), owned, || {
                self.local_owner_valid()
            })
            .await?;
        }
        Ok(())
    }

    fn local_owner_valid(&self) -> bool {
        !self.owner_lost.as_ref().is_some_and(|lost| *lost.borrow())
            && !self
                .owner_until
                .as_ref()
                .is_some_and(|until| tokio::time::Instant::now() >= *until.borrow())
    }
}

async fn check_owned_work<Valid>(
    jobs: &dyn ReviewJobStore,
    owned: &OwnedReview,
    local_owner_valid: Valid,
) -> Result<()>
where
    Valid: Fn() -> bool,
{
    let should_continue = jobs.should_continue(owned, unix_time_ms()).await?;
    if !local_owner_valid() {
        return Err(Error::ReviewOwnershipLost);
    }
    if !should_continue {
        return Err(Error::ReviewSuperseded);
    }
    Ok(())
}

async fn check_owner_after_await<F, Fut>(locally_valid: F, ownership_check: Fut) -> Result<()>
where
    F: Fn() -> bool,
    Fut: std::future::Future<Output = Result<bool>>,
{
    if !locally_valid() {
        return Err(Error::ReviewOwnershipLost);
    }
    // Mongo can answer after the conservative local deadline. Ownership must
    // still be locally valid when the awaited check returns.
    if !ownership_check.await? || !locally_valid() {
        return Err(Error::ReviewOwnershipLost);
    }
    Ok(())
}

/// The registry behind `AppState::in_flight`.
///
/// `accepting` shares a lock with `slots` on purpose. `conclude_in_flight`
/// needs to take an exact snapshot of every review it is about to conclude
/// and refuse every registration from then on, atomically — otherwise a
/// review could register in the gap between the snapshot and the flag being
/// set, land on neither side, and be exactly the orphaned check this whole
/// registry exists to prevent. One lock covering both makes that gap not
/// exist: a registration either lands in the snapshot or observes shutdown
/// already in progress.
struct InFlightRegistry {
    slots: Vec<StatusSlot>,
    accepting: bool,
}

impl Default for InFlightRegistry {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            accepting: true,
        }
    }
}

/// A review's membership in `AppState::in_flight`, for as long as it runs.
///
/// A guard rather than a pair of calls so that every exit from
/// `handle_review` — including an unwind — deregisters the slot. Dropping a
/// guard removes exactly its own slot, by pointer, so two reviews finishing
/// in either order cannot remove each other.
struct InFlight {
    registry: Arc<std::sync::Mutex<InFlightRegistry>>,
    slot: StatusSlot,
}

impl InFlight {
    /// `None` once shutdown has taken its snapshot: the caller declines the
    /// review outright, before opening a check, rather than register a slot
    /// nothing will ever conclude.
    fn register(
        registry: &Arc<std::sync::Mutex<InFlightRegistry>>,
        slot: &StatusSlot,
    ) -> Option<Self> {
        let mut guard = registry.lock().expect("in-flight reviews");
        if !guard.accepting {
            return None;
        }
        guard.slots.push(slot.clone());
        drop(guard);
        Some(Self {
            registry: registry.clone(),
            slot: slot.clone(),
        })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.registry
            .lock()
            .expect("in-flight reviews")
            .slots
            .retain(|other| !Arc::ptr_eq(other, &self.slot));
    }
}

/// Publish the in-progress check, and record how to conclude it.
///
/// Best-effort in both directions: a failure to publish is logged and the
/// review proceeds without a status, because a missing progress indicator is a
/// far smaller problem than a pull request that goes unreviewed because its
/// progress indicator could not be drawn.
///
/// ## On the write token
///
/// This mints one before the lanes run, which the security boundary in
/// `AGENTS.md` otherwise reserves for after every model call has returned. The
/// property that rule protects is that *the model* never holds a write handle,
/// and that is preserved exactly: the token is minted here, used for one
/// request, and dropped before this function returns — it is never placed in
/// `AppState`, never passed to `run_lanes`, and no lane or model can
/// reach it. `report_failure` has always minted one on the same terms. See the
/// pull request that introduced this for the discussion the boundary requires.
/// Returns whether the review should go on. `false` means the process is
/// shutting down and this check has already been concluded as failed, so
/// running the lanes would spend model calls on a verdict nothing will
/// publish.
async fn open_status(
    state: &AppState,
    slot: &StatusSlot,
    repo: &RepoId,
    head_sha: &str,
    installation: u64,
) -> bool {
    // A retry re-enters `review_inner`, so without this the second attempt
    // would open a second check and orphan the first.
    let existing = { slot.lock().expect("status slot").as_ref().cloned() };
    if let Some(existing) = existing {
        if existing.head_sha != head_sha {
            tracing::warn!(%repo, check_id = existing.check_id, queued_head = %existing.head_sha, live_head = %head_sha, "queued review check belongs to a superseded head");
            close_status(state, slot, Conclusion::NotReviewed).await;
            return false;
        }
        if existing.stage == crate::ports::review_job::ReviewCheckStage::Queued {
            let transitioned = async {
                if let Some(owned) = &existing.owned
                    && !state
                        .review_jobs
                        .check_transition_current(
                            owned,
                            &existing.head_sha,
                            existing.check_id,
                            unix_time_ms(),
                        )
                        .await?
                {
                    return Ok::<(), Error>(());
                }
                let token = state.auth.installation_token(installation).await?;
                // Token minting awaits. Revalidate immediately before the write
                // so a replacement owner cannot have its terminal state overwritten.
                if let Some(owned) = &existing.owned
                    && !state
                        .review_jobs
                        .check_transition_current(
                            owned,
                            &existing.head_sha,
                            existing.check_id,
                            unix_time_ms(),
                        )
                        .await?
                {
                    return Ok(());
                }
                crate::app::apply::start_queued_review_status(
                    &crate::forge::github::GitHubWrite::new(&token)?,
                    repo,
                    existing.check_id,
                    &existing.head_sha,
                )
                .await
            }
            .await;
            match transitioned {
                Ok(()) => {
                    if let Some(owned) = &existing.owned {
                        let _ = state
                            .review_jobs
                            .set_check_stage(
                                owned,
                                &existing.head_sha,
                                existing.check_id,
                                crate::ports::review_job::ReviewCheckStage::InProgress,
                                unix_time_ms(),
                            )
                            .await;
                    }
                    if let Some(status) = slot.lock().expect("status slot").as_mut() {
                        status.stage = crate::ports::review_job::ReviewCheckStage::InProgress;
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, %repo, check_id = existing.check_id, "could not transition queued check to in-progress")
                }
            }
        }
        return true;
    }

    let published = async {
        use crate::ports::forge::ForgeWrite;
        let token = state.auth.installation_token(installation).await?;
        crate::forge::github::GitHubWrite::new(&token)?
            .publish_check(repo, status::in_progress(head_sha))
            .await
    }
    .await;

    match published {
        Ok(check_id) => {
            *slot.lock().expect("status slot") = Some(ReviewStatus {
                repo: repo.clone(),
                check_id,
                head_sha: head_sha.to_string(),
                installation,
                stage: crate::ports::review_job::ReviewCheckStage::InProgress,
                owned: None,
                hub_comment_id: None,
                prior_hub_body: None,
            });

            // `conclude_in_flight`'s snapshot only concludes slots it can see
            // at the instant it runs. This publish call was in flight for the
            // whole time it was awaiting `installation_token`/`publish_check`
            // above, so shutdown could have taken its snapshot — and flipped
            // `accepting` off — before the slot held anything to conclude,
            // leaving a fresh "in progress" check that nothing would ever
            // revisit. Re-checking `accepting` right here, under the same
            // registry lock `conclude_in_flight` uses, closes that gap
            // exactly: either this observes `accepting` still true, in which
            // case the slot is already registered and the shutdown pass that
            // has not run yet will pick it up normally, or shutdown has
            // already run and this concludes the check itself, immediately,
            // rather than leave it orphaned.
            let missed_the_snapshot = !state.in_flight.lock().expect("in-flight reviews").accepting;
            if missed_the_snapshot {
                let err = Error::lane(
                    "review",
                    "tinysweeper was restarted while this review was running",
                );
                close_status(state, slot, Conclusion::Failed(&err)).await;
                return false;
            }
        }
        Err(err) => {
            tracing::warn!(%err, %repo, "could not publish the in-progress check");
        }
    }
    true
}

/// Conclude the in-progress check, if one was ever opened.
///
/// Takes the status out of the slot, so a check cannot be concluded twice —
/// the second write would be a PATCH to a run already in its terminal state,
/// and the API is entitled to reject it.
async fn close_status(state: &AppState, slot: &StatusSlot, conclusion: Conclusion<'_>) {
    let Some(open) = slot.lock().expect("status slot").take() else {
        return;
    };

    let write = match state
        .auth
        .installation_token(open.installation)
        .await
        .and_then(|token| crate::forge::github::GitHubWrite::new(&token))
    {
        Ok(write) => write,
        Err(err) => {
            tracing::error!(
                %err, repo = %open.repo, check_id = open.check_id,
                "could not authenticate to conclude the in-progress check"
            );
            return;
        }
    };

    let (concluded, hub_updated) =
        conclude_status_with_writer(&open, conclusion, &write, || async {
            if let Some(owned) = &open.owned {
                let _ = state
                    .review_jobs
                    .set_check_stage(
                        owned,
                        &open.head_sha,
                        open.check_id,
                        crate::ports::review_job::ReviewCheckStage::Completed,
                        unix_time_ms(),
                    )
                    .await;
            }
        })
        .await;
    if let Err(err) = concluded {
        // Worth an error rather than a warning: the check is now stuck
        // in-progress, and a pending check refuses auto-merge on this commit
        // until somebody pushes again.
        tracing::error!(
            %err, repo = %open.repo, check_id = open.check_id,
            "could not conclude the in-progress check; it will block auto-merge until the next push"
        );
    }
    if let Some(Err(err)) = hub_updated {
        // The check conclusion is already durable. A comment retry must not
        // turn that successful terminal write into a failed review attempt.
        tracing::warn!(
            %err, repo = %open.repo, comment_id = open.hub_comment_id,
            "could not conclude the review hub; the check conclusion was preserved"
        );
    }
}

/// Apply the already-decided conclusion through a caller-owned forge writer.
/// Keeping payload selection here lets offline route tests exercise the same
/// terminal check and hub writes as `close_status` without minting credentials.
async fn conclude_status_with_writer<AfterCheck, AfterCheckFuture>(
    open: &ReviewStatus,
    conclusion: Conclusion<'_>,
    write: &dyn crate::ports::forge::ForgeWrite,
    after_check: AfterCheck,
) -> (Result<()>, Option<Result<()>>)
where
    AfterCheck: FnOnce() -> AfterCheckFuture,
    AfterCheckFuture: std::future::Future<Output = ()>,
{
    let check = match conclusion {
        Conclusion::Reviewed {
            findings,
            changes_requested,
            coverage,
        } => status::completed(&open.head_sha, findings, changes_requested, coverage),
        Conclusion::NotReviewed => status::not_reviewed(&open.head_sha),
        Conclusion::Failed(err) => failure::check_run(&open.head_sha, err),
    };

    let hub_body = match conclusion {
        Conclusion::Reviewed { .. } => None,
        Conclusion::NotReviewed => Some(crate::summary::failed(
            &open.head_sha,
            "This pass stopped before a review could be completed; the pull request may have closed or changed while it was running.",
            open.prior_hub_body.as_deref(),
        )),
        Conclusion::Failed(err) => Some(crate::summary::failed(
            &open.head_sha,
            &err.to_string(),
            open.prior_hub_body.as_deref(),
        )),
    };
    let concluded = write.update_check(&open.repo, open.check_id, check).await;
    if concluded.is_ok() {
        after_check().await;
    }
    let hub_updated = match (open.hub_comment_id, hub_body) {
        (Some(comment_id), Some(body)) => {
            Some(write.update_comment(&open.repo, comment_id, &body).await)
        }
        _ => None,
    };
    (concluded, hub_updated)
}

/// Create or migrate the durable hub before the first model call.
#[allow(clippy::too_many_arguments)]
async fn open_review_hub(
    state: &AppState,
    slot: &StatusSlot,
    read: &dyn crate::ports::forge::ForgeRead,
    config: &Config,
    repo: &RepoId,
    number: u64,
    head_sha: &str,
    installation: u64,
) {
    if !config.summary.enabled || !hub_slot_is_open(slot, head_sha) {
        return;
    }
    let existing =
        match crate::findings::prior::own_comment(read, repo, number, crate::summary::MARKER).await
        {
            Ok(Some(comment)) => Some(comment),
            Ok(None) => match crate::findings::prior::own_comment(
                read,
                repo,
                number,
                crate::summary::LEGACY_MARKER,
            )
            .await
            {
                Ok(comment) => comment,
                Err(err) => {
                    tracing::warn!(%err, "could not discover the legacy review hub");
                    return;
                }
            },
            Err(err) => {
                tracing::warn!(%err, "could not discover the review hub");
                // Unknown is not absence. Creating after a transient list
                // failure would orphan the durable hub and duplicate it on
                // every similarly affected pass.
                return;
            }
        };
    let prior = existing.as_ref().map(|comment| comment.body.clone());
    let body = crate::summary::in_progress(head_sha, prior.as_deref());
    let written = async {
        use crate::ports::forge::ForgeWrite;
        let token = state.auth.installation_token(installation).await?;
        let write = crate::forge::github::GitHubWrite::new(&token)?;
        // Discovery and authentication both await. Shutdown may have removed
        // the slot while either was running, so do not start a comment write
        // for a review that has already concluded.
        if !hub_slot_is_open(slot, head_sha) {
            return Ok(None);
        }
        match existing.and_then(|comment| comment.id) {
            Some(id) => {
                write.update_comment(repo, id, &body).await?;
                Ok(Some(id))
            }
            None => write.create_comment(repo, number, &body).await.map(Some),
        }
    }
    .await;
    match written {
        Ok(Some(id)) => {
            let attached = if let Some(open) = slot.lock().expect("status slot").as_mut()
                && open.head_sha == head_sha
            {
                open.hub_comment_id = Some(id);
                open.prior_hub_body = prior.clone();
                true
            } else {
                false
            };
            if !attached {
                // The network write and shutdown raced. The conclusion could
                // not see this ID, so settle the comment here instead of
                // leaving a permanent "Reviewing" report behind.
                let terminal = crate::summary::failed(
                    head_sha,
                    "This pass ended while the review hub was being published.",
                    prior.as_deref(),
                );
                let reconciled = async {
                    use crate::ports::forge::ForgeWrite;
                    let token = state.auth.installation_token(installation).await?;
                    crate::forge::github::GitHubWrite::new(&token)?
                        .update_comment(repo, id, &terminal)
                        .await
                }
                .await;
                if let Err(err) = reconciled {
                    tracing::warn!(%err, comment_id = id, "could not reconcile a review hub published during shutdown");
                }
                return;
            }
            let key = crate::state::key(&repo.to_string(), number);
            if let Ok(current) =
                crate::ports::review_state::ReviewStateStore::load_state(&state.store, &key).await
            {
                let mut current = current.unwrap_or_default();
                current.hub_comment_id = Some(id);
                if let Err(err) = crate::ports::review_state::ReviewStateStore::save_state(
                    &state.store,
                    &key,
                    &current,
                )
                .await
                {
                    tracing::warn!(%err, "could not persist the review-hub comment id");
                }
            }
        }
        Ok(None) => {}
        Err(err) => tracing::warn!(%err, "could not publish the in-progress review hub"),
    }
}

fn hub_slot_is_open(slot: &StatusSlot, head_sha: &str) -> bool {
    slot.lock()
        .expect("status slot")
        .as_ref()
        .is_some_and(|open| open.head_sha == head_sha && open.hub_comment_id.is_none())
}

/// How a review ended, for the umbrella check.
enum Conclusion<'a> {
    /// The lanes ran.
    Reviewed {
        /// The finding count, for the title.
        findings: usize,
        /// Whether the published review requested changes, so the check
        /// cannot show a pass beside that verdict.
        changes_requested: bool,
        /// File and whole-lane coverage aggregated across the proposal.
        coverage: status::ReviewCoverage,
    },
    /// The run stopped deliberately, without reviewing anything.
    ///
    /// Reachable when a check was already opened and the run *then* declined —
    /// a retry whose pull request has since been converted back to a draft, say.
    /// It exists so that case does not report "Reviewed" for a commit nothing
    /// looked at, which is the exact confusion this whole check is here to end.
    NotReviewed,
    /// The review could not be produced.
    Failed(&'a Error),
}

fn review_conclusion(
    findings: Option<(usize, bool, status::ReviewCoverage)>,
) -> Conclusion<'static> {
    match findings {
        Some((findings, changes_requested, coverage)) => Conclusion::Reviewed {
            findings,
            changes_requested,
            coverage,
        },
        None => Conclusion::NotReviewed,
    }
}

/// Classify a completed, already-applied review the same way for the worker
/// lifecycle and its durable scheduler record.
fn completed_review_disposition(
    findings: Option<(usize, bool, status::ReviewCoverage)>,
) -> (Conclusion<'static>, WorkerDisposition) {
    let outcome = if findings.is_some() {
        TerminalOutcome::Published
    } else {
        TerminalOutcome::Declined
    };
    (
        review_conclusion(findings),
        WorkerDisposition::Terminal(outcome),
    )
}

/// Review one pull request, off the request path.
///
/// A failed review must not take the server with it — one pull request going
/// wrong is not an outage — but it must also not be *invisible*. A transient
/// failure is retried a few times, and a review that still cannot run says so
/// on the pull request through a blocking check. See `server::failure` for why
/// the log line alone was not enough.
#[tracing::instrument(skip_all, fields(repo = %job.repo, pr = job.number, head_sha = tracing::field::Empty, mode = ?job.mode))]
async fn handle_review(
    state: AppState,
    job: ReviewJob,
    queued_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    owner_lost: Option<watch::Receiver<bool>>,
    owner_until: Option<watch::Receiver<tokio::time::Instant>>,
) -> WorkerDisposition {
    let ReviewJob {
        repo,
        number,
        author,
        installation,
        mode,
        delivery,
        expected_head,
        queued_at,
        owned,
        queue_check,
    } = job;
    let timing = crate::review_timing::ReviewTiming::new(repo.clone(), number, format!("{mode:?}"));
    timing.record(|record| record.queue_wait_ms = queued_at.elapsed().as_millis() as u64);
    tracing::debug!(%repo, number, queue_wait_ms = queued_at.elapsed().as_millis(), "starting admitted review worker");
    let queued_status = queue_check.as_ref().and_then(|check| {
        let check_id = check.check_id?;
        Some(ReviewStatus {
            repo: RepoId::parse(&repo)?,
            check_id,
            head_sha: check.head_sha.clone(),
            installation,
            stage: check.stage,
            owned: owned.clone(),
            hub_comment_id: None,
            prior_hub_body: None,
        })
    });
    let slot: StatusSlot = Arc::new(std::sync::Mutex::new(queued_status));
    let Some(_registered) = InFlight::register(&state.in_flight, &slot) else {
        // Shutdown has already taken its concluding snapshot: no check has
        // been opened yet, so there is nothing to conclude. Declining here,
        // before `review_inner` does any work, is what keeps this review off
        // the leftover-grace-period path — see `conclude_in_flight`.
        tracing::info!(%repo, number, "declining to start a review: shutting down");
        // The claim still has to be released on this path like every other:
        // `dispatch` already persisted it before calling in here, and leaving
        // it held would make `claim_delivery` refuse GitHub's own redelivery
        // of the same webhook forever, with no other trigger left to review
        // this commit until the next push.
        if owned.is_none()
            && let Some(delivery) = delivery
            && let Err(release) = state.store.release_delivery(&delivery).await
        {
            tracing::error!(%release, %delivery, "could not release the declined delivery claim");
        }
        return WorkerDisposition::Deferred(unix_time_ms());
    };

    // The permit is taken here, before the clock starts, and held across
    // every attempt. Queueing behind the other reviews is not time this
    // review spent, and counting it would let a delivery that merely waited
    // its turn "time out" without ever running — and then, having no SHA of
    // its own, report that failure against whatever head is live by then.
    // Holding it across retries also keeps a retry from going to the back of
    // the queue behind reviews that arrived while it was failing.
    let permit = match queued_permit {
        Some(permit) => Ok(permit),
        None => {
            crate::review_timing::measure(
                Some(&timing),
                "permit",
                state.permits.clone().acquire_owned(),
            )
            .await
        }
    };
    let permit = match permit {
        Ok(permit) => permit,
        Err(err) => {
            // Unreachable in practice — nothing closes the pool — but a
            // terminal path all the same, and a terminal path releases its
            // delivery claim so a redelivery is not refused forever.
            tracing::error!(%err, %repo, number, "the review permit pool is closed");
            if owned.is_none()
                && let Some(delivery) = delivery
                && let Err(release) = state.store.release_delivery(&delivery).await
            {
                tracing::error!(%release, %delivery, "could not release the delivery claim");
            }
            return WorkerDisposition::Deferred(unix_time_ms() + 5_000);
        }
    };

    // One deadline for the whole review, retries included. A per-attempt
    // deadline would let three transient failures late in the run stretch a
    // single pull request to three times the budget, all under one check that
    // has said "reviewing" the entire time.
    let started_at = tokio::time::Instant::now();
    let deadline = started_at + REVIEW_DEADLINE;
    let large_pr = &state.config.config.review.large_pr;
    let execution = Arc::new(crate::app::review_execution::ReviewRunContext::new(
        crate::app::review_execution::ReviewExecutionOptions {
            started_at,
            hard_deadline: deadline,
            #[cfg(test)]
            mode: crate::lanes::large_pr::ReviewMode::Normal,
            max_calls: None,
            soft_deadline_percent: large_pr.soft_deadline_percent,
            drain_seconds: large_pr.drain_seconds,
        },
    ));
    let run = Run {
        mode,
        slot: slot.clone(),
        deadline,
        execution,
        timing: timing.clone(),
        expected_head,
        owned: owned.clone(),
        owner_lost: owner_lost.clone(),
        owner_until: owner_until.clone(),
        lease_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let mut attempt = 1;
    let err = loop {
        if let Err(err) = run.check_work(&state).await {
            run.cancel_for_ownership_fence(&err);
            match classify_retry_boundary_error(
                &err,
                state.review_jobs.as_ref(),
                run.owned.as_ref(),
                || run.local_owner_valid(),
            )
            .await
            {
                Ok(Some(WorkerDisposition::Yielded)) => {
                    close_status(&state, &slot, Conclusion::NotReviewed).await;
                    drop(permit);
                    return finish_control_timing(&timing, WorkerDisposition::Yielded);
                }
                Ok(Some(WorkerDisposition::Terminal(TerminalOutcome::Superseded))) => {
                    close_status(&state, &slot, Conclusion::NotReviewed).await;
                    drop(permit);
                    return finish_control_timing(
                        &timing,
                        WorkerDisposition::Terminal(TerminalOutcome::Superseded),
                    );
                }
                Ok(Some(WorkerDisposition::OwnershipLost)) | Err(_) => {
                    drop(permit);
                    return WorkerDisposition::OwnershipLost;
                }
                Ok(Some(WorkerDisposition::Terminal(outcome))) => {
                    close_status(&state, &slot, Conclusion::NotReviewed).await;
                    drop(permit);
                    return finish_control_timing(&timing, WorkerDisposition::Terminal(outcome));
                }
                Ok(Some(WorkerDisposition::Deferred(not_before_ms))) => {
                    drop(permit);
                    return WorkerDisposition::Deferred(not_before_ms);
                }
                Ok(None) => {}
            }
            break err;
        }
        let review_result = review_inner(&state, &repo, number, &author, installation, &run).await;
        if let Ok(review) = &review_result
            && let Some(disposition) = shutdown_disposition(review, unix_time_ms())
        {
            drop(permit);
            close_status(&state, &slot, Conclusion::NotReviewed).await;
            return disposition;
        }
        match review_result {
            Ok(InnerReview::Complete(findings)) => {
                // Usually there is nothing to close: a run that declines — a
                // blocked contributor, a draft, a lease another worker holds —
                // does so before opening a check, and `close_status` is a no-op
                // when the slot is empty. `NotReviewed` covers the one ordering
                // where it is not: an earlier attempt opened the check and this
                // one declined.
                if let Err(err) = run.check_work(&state).await {
                    run.cancel_for_ownership_fence(&err);
                    if matches!(err, Error::ReviewSuperseded)
                        && let Some(owned) = &owned
                        && run.check_owner(&state).await.is_ok()
                        && !state
                            .review_jobs
                            .should_continue(owned, unix_time_ms())
                            .await
                            .unwrap_or(true)
                    {
                        let published = findings.is_some();
                        let conclusion = if published {
                            review_conclusion(findings)
                        } else {
                            Conclusion::NotReviewed
                        };
                        close_status(&state, &slot, conclusion).await;
                        drop(permit);
                        return finish_control_timing(
                            &timing,
                            if published {
                                WorkerDisposition::Terminal(TerminalOutcome::Published)
                            } else {
                                WorkerDisposition::Yielded
                            },
                        );
                    }
                    drop(permit);
                    return WorkerDisposition::OwnershipLost;
                }
                let (conclusion, disposition) = completed_review_disposition(findings);
                let published = matches!(
                    disposition,
                    WorkerDisposition::Terminal(TerminalOutcome::Published)
                );
                if let Err(err) = run.check_owner(&state).await {
                    run.cancel_for_ownership_fence(&err);
                    drop(permit);
                    return WorkerDisposition::OwnershipLost;
                }
                // The review is over; concluding the check is one GitHub
                // write that should not hold a review slot against other
                // pull requests.
                drop(permit);
                close_status(&state, &slot, conclusion).await;
                if published {
                    timing.finish("success");
                } else if timing.snapshot().outcome == "cancelled" {
                    timing.finish("declined");
                }
                return disposition;
            }
            Ok(InnerReview::Busy) => {
                if let Err(err) = run.check_owner(&state).await {
                    run.cancel_for_ownership_fence(&err);
                    drop(permit);
                    return WorkerDisposition::OwnershipLost;
                }
                drop(permit);
                close_status(&state, &slot, Conclusion::NotReviewed).await;
                return WorkerDisposition::Deferred(unix_time_ms() + 5_000);
            }
            Ok(InnerReview::Exhausted) => {
                drop(permit);
                return finalize_exhausted_review(
                    &timing,
                    || run.check_owner(&state),
                    || async { close_status(&state, &slot, Conclusion::NotReviewed).await },
                )
                .await;
            }
            Ok(review @ InnerReview::Shutdown) => {
                let disposition = shutdown_disposition(&review, unix_time_ms())
                    .expect("shutdown review has a deferred disposition");
                drop(permit);
                close_status(&state, &slot, Conclusion::NotReviewed).await;
                return disposition;
            }
            Err(err) => {
                if let Some(disposition) = ownership_fence_disposition(
                    &err,
                    &run.execution.execution(),
                    state.review_jobs.as_ref(),
                    run.owned.as_ref(),
                    || run.local_owner_valid(),
                )
                .await
                .unwrap_or(Some(WorkerDisposition::OwnershipLost))
                {
                    match disposition {
                        WorkerDisposition::Yielded => {
                            close_status(&state, &slot, Conclusion::NotReviewed).await;
                            drop(permit);
                            return finish_control_timing(&timing, WorkerDisposition::Yielded);
                        }
                        WorkerDisposition::Terminal(TerminalOutcome::Superseded) => {
                            close_status(&state, &slot, Conclusion::NotReviewed).await;
                            drop(permit);
                            timing.finish("superseded");
                            return finish_control_timing(
                                &timing,
                                WorkerDisposition::Terminal(TerminalOutcome::Superseded),
                            );
                        }
                        WorkerDisposition::OwnershipLost => {
                            drop(permit);
                            return WorkerDisposition::OwnershipLost;
                        }
                        other => {
                            drop(permit);
                            return other;
                        }
                    }
                }
                run.cancel_for_ownership_fence(&err);
                if matches!(err, Error::ReviewOwnershipLost) {
                    drop(permit);
                    return WorkerDisposition::OwnershipLost;
                }
                if matches!(err, Error::ReviewSuperseded) {
                    if let Some(owned) = &owned
                        && run.check_owner(&state).await.is_ok()
                    {
                        match state
                            .review_jobs
                            .should_continue(owned, unix_time_ms())
                            .await
                        {
                            Ok(false) => {
                                close_status(&state, &slot, Conclusion::NotReviewed).await;
                                drop(permit);
                                return finish_control_timing(&timing, WorkerDisposition::Yielded);
                            }
                            Ok(true) => {}
                            Err(err) => {
                                tracing::error!(%err, job_id = %owned.request.id.0, "could not confirm review supersession");
                                drop(permit);
                                return WorkerDisposition::OwnershipLost;
                            }
                        }
                    }
                    drop(permit);
                    close_status(&state, &slot, Conclusion::NotReviewed).await;
                    timing.finish("superseded");
                    tracing::debug!(%repo, number, "head changed while preparing review context");
                    return finish_control_timing(
                        &timing,
                        WorkerDisposition::Terminal(TerminalOutcome::Superseded),
                    );
                }
                // The lease is released inside `review_inner` on every path,
                // including this one, so a retry re-claims it rather than
                // colliding with itself and returning a silent `Ok`.
                //
                // Bounded by `run.deadline` too, not just `MAX_ATTEMPTS`: the
                // backoff sleep between attempts is outside `run_lanes`'
                // `timeout_at`, so without this check a run already out of
                // budget would sleep anyway and try again, spending more of
                // the `LEASE_TTL` margin on a review that has already missed
                // its window. `Instant::now() >= run.deadline` is the same
                // "refuse late rather than cancel mid-flight" rule the
                // non-idempotent writes elsewhere in this function use — a
                // sleep is trivially safe to just not start.
                let deadline_spent = tokio::time::Instant::now() >= run.deadline;
                if !deadline_spent && attempt < failure::MAX_ATTEMPTS && failure::is_transient(&err)
                {
                    let wait = failure::backoff_ms(attempt);
                    tracing::warn!(
                        %err, %repo, number, attempt, wait_ms = wait,
                        "review failed; retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    attempt += 1;
                    continue;
                }
                break err;
            }
        }
    };

    // The review is over; the failure report below is a GitHub write that
    // should not hold a review slot against other pull requests.
    drop(permit);

    tracing::error!(%err, %repo, number, attempts = attempt, "review failed");
    timing.finish(crate::review_timing::outcome_for_error(&err));

    // Two ways to report the same thing, and which one applies depends on how
    // far the review got. If a check is already open, concluding it in place is
    // both cheaper and correct — a fresh POST would leave the open one pending
    // and refuse auto-merge forever. If the review died before it had a SHA,
    // there is nothing to conclude and the failure has to open its own check.
    let opened = slot.lock().expect("status slot").is_some();
    let deadline_hit = matches!(
        &err,
        Error::Timeout { what, seconds }
            if *seconds == REVIEW_DEADLINE.as_secs()
                && what.starts_with("the review of ")
    );
    if run.check_owner(&state).await.is_err() {
        return WorkerDisposition::OwnershipLost;
    }
    // An empty slot after shutdown's snapshot means shutdown concluded this
    // review's check already; a fresh failure check on top of it would be a
    // duplicate, not a report.
    let shutting_down = !state.in_flight.lock().expect("in-flight reviews").accepting;
    if opened {
        close_status(&state, &slot, Conclusion::Failed(&err)).await;
    } else if shutting_down {
        tracing::info!(%repo, number, "not reporting a failure shutdown already concluded");
    } else if let Err(report) = report_failure(&state, &repo, number, installation, &err).await {
        // Reporting is best-effort by necessity: the most likely reason it
        // fails is the same forge outage that failed the review. Log both, so
        // the pod still carries the whole story even when GitHub does not.
        tracing::error!(%report, %repo, number, "could not report the failed review");
    }

    // A delivery has already been acknowledged, so this is the only recovery
    // available to a transiently failed worker without a durable job queue.
    // Keep successful claims for dedupe; only terminal failures become
    // retryable through a later GitHub redelivery.
    if owned.is_none()
        && let Some(delivery) = delivery
        && let Err(release) = state.store.release_delivery(&delivery).await
    {
        tracing::error!(%release, %delivery, "could not release the failed delivery claim");
    }
    WorkerDisposition::Terminal(if deadline_hit {
        TerminalOutcome::Deadline
    } else {
        TerminalOutcome::Exhausted
    })
}

/// Publish the check that says this pull request was not reviewed.
///
/// Deliberately mints its own write token rather than receiving one: the
/// security boundary keeps write credentials out of everything that runs
/// before or alongside a model call, and this runs strictly after the review
/// has finished failing.
async fn report_failure(
    state: &AppState,
    repo: &str,
    number: u64,
    installation: u64,
    err: &Error,
) -> Result<()> {
    use crate::ports::forge::{ForgeRead, ForgeWrite};

    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    // The check is pinned to a SHA, so the head has to be read even though the
    // review just failed to read it. When *that* read is what is broken there
    // is nothing to pin the check to, and the error propagates to the caller.
    let token = state.auth.installation_token(installation).await?;
    let head_sha = crate::forge::github::GitHubRead::new(&token)?
        .pull_request(&repo_id, number)
        .await?
        .head_sha;

    let write = crate::forge::github::GitHubWrite::new(&token)?;
    write
        .publish_check(&repo_id, failure::check_run(&head_sha, err))
        .await
        .map(|_| ())
}

/// Run one review.
///
/// `Ok(None)` is a run that deliberately did nothing — a blocked contributor, a
/// draft, or a commit another worker is already reviewing — and is distinct
/// from `Ok(Some(0))`, a review that ran and found nothing. Only the latter
/// should tell a pull request it has been reviewed.
#[tracing::instrument(skip_all, fields(repo = %repo, pr = number, head_sha = tracing::field::Empty, mode = ?run.mode))]
async fn review_inner(
    state: &AppState,
    repo: &str,
    number: u64,
    author: &str,
    installation: u64,
    run: &Run,
) -> Result<InnerReview> {
    run.check_work(state).await?;
    let who = state.store.contributor(author).await?;
    if who.trust == Trust::Blocked {
        run.timing.finish("declined");
        tracing::info!(%author, "blocked contributor; not reviewing");
        return Ok(InnerReview::Complete(None));
    }

    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    // Webhook pull-request events carry the revision that caused the delivery,
    // so claim it before spending a credential or a forge read. Comment and
    // manual requests have no revision; serialize those per pull request until
    // the live head is known, then take its revision lease before any indexing.
    let initial_lease = review_lease(repo, number, run.mode, run.expected_head.as_deref());
    let holder = review_lease_holder(run);
    let mut lease = Some(initial_lease.clone());
    let claimed = state.store.claim_lease(&initial_lease, &holder).await?;
    let reviewed = continue_if_lease_claimed(claimed, || async {
        run.check_work(state).await?;
        let read_token = state.auth.review_read_token(installation).await?;
        let forge = crate::forge::github::GitHubRead::new(&read_token)?;

        let worker = ReviewRun {
            state,
            repo,
            number,
            author,
            installation,
            run,
            repo_id: &repo_id,
            read_token: &read_token,
            forge: &forge,
            lease: &mut lease,
        };
        with_open_pull_request(&forge, &repo_id, number, |pull_request| {
            worker.run(pull_request)
        })
        .await
    })
    .await;
    if !claimed {
        tracing::debug!(%initial_lease, "another worker holds this review");
        return Ok(InnerReview::Busy);
    }
    if let Some(lease) = &lease
        && let Err(err) = state.store.release_lease_for(lease, &holder).await
    {
        tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
    }
    let reviewed = reviewed?.flatten();
    if let Some(result) = reviewed {
        return Ok(result);
    }
    if run
        .lease_busy
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        Ok(InnerReview::Busy)
    } else {
        Ok(InnerReview::Complete(None))
    }
}

async fn continue_if_lease_claimed<T, F, Fut>(claimed: bool, work: F) -> Result<Option<T>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    if !claimed {
        return Ok(None);
    }
    work().await.map(Some)
}

async fn continue_after_open_status<F, Fut>(go_on: bool, review: F) -> Result<InnerReview>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<InnerReview>>,
{
    if go_on {
        review().await
    } else {
        Ok(InnerReview::Shutdown)
    }
}

async fn review_with_paid_budget_and_status<
    Start,
    StartFuture,
    OpenStatus,
    OpenStatusFuture,
    Review,
    ReviewFuture,
>(
    store: &dyn ReviewJobStore,
    owned: Option<&OwnedReview>,
    now_ms: i64,
    start: Start,
    open_status: OpenStatus,
    review: Review,
) -> Result<InnerReview>
where
    Start: FnOnce() -> StartFuture,
    StartFuture: std::future::Future<Output = Result<()>>,
    OpenStatus: FnOnce() -> OpenStatusFuture,
    OpenStatusFuture: std::future::Future<Output = Result<bool>>,
    Review: FnOnce() -> ReviewFuture,
    ReviewFuture: std::future::Future<Output = Result<InnerReview>>,
{
    // Open or resume the check before exhausting the paid-attempt budget. A
    // recovered job at the limit still needs a check to conclude, and shutdown
    // must defer without consuming a paid attempt.
    let go_on = open_status().await?;
    if !go_on {
        return continue_after_open_status(false, review).await;
    }

    if let Some(owned) = owned {
        match with_paid_review_attempt(store, owned, now_ms, start).await? {
            PaidWork::Started(result) => result?,
            PaidWork::Exhausted => return Ok(InnerReview::Exhausted),
            PaidWork::OwnershipLost => return Err(Error::ReviewOwnershipLost),
        }
    } else {
        start().await?;
    }
    continue_after_open_status(true, review).await
}

fn review_lease(repo: &str, number: u64, mode: Mode, head: Option<&str>) -> String {
    let suffix = if mode == Mode::Full { "!full" } else { "" };
    match head {
        Some(head) => format!("{repo}#{number}@{head}{suffix}"),
        None => format!("{repo}#{number}!manual{suffix}"),
    }
}

/// A delayed incremental delivery may resolve to a head already completed by
/// a newer delivery. Only the authoritative head check can suppress that
/// duplicate; webhook SHA values remain hints.
async fn confirmed_head_is_terminal(
    jobs: &dyn ReviewJobStore,
    owned: &OwnedReview,
    mode: Mode,
) -> Result<bool> {
    if mode == Mode::Full {
        return Ok(false);
    }
    jobs.head_is_terminal(owned, unix_time_ms()).await
}

/// Run the actual review continuation only for a nonterminal confirmed head.
/// Keeping this gate around the production continuation makes delayed webhook
/// behavior testable with the same model and Forge ports used by the worker.
async fn run_if_confirmed_head_needs_review<F, Fut>(
    jobs: &dyn ReviewJobStore,
    owned: Option<&OwnedReview>,
    mode: Mode,
    review: F,
) -> Result<InnerReview>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<InnerReview>>,
{
    if let Some(owned) = owned
        && confirmed_head_is_terminal(jobs, owned, mode).await?
    {
        return Ok(InnerReview::Complete(None));
    }
    review().await
}

fn review_lease_holder(run: &Run) -> String {
    run.owned.as_ref().map_or_else(
        || "server".to_owned(),
        |owned| format!("{}:{}", owned.owner, owned.generation),
    )
}

/// The server dependencies for the work after the pull request passes its gate.
struct ReviewRun<'a> {
    state: &'a AppState,
    repo: &'a str,
    number: u64,
    author: &'a str,
    installation: u64,
    run: &'a Run,
    repo_id: &'a RepoId,
    read_token: &'a String,
    forge: &'a crate::forge::github::GitHubRead,
    lease: &'a mut Option<String>,
}

impl ReviewRun<'_> {
    /// Run the remainder of one review after its live pull request passes the gate.
    async fn run(self, pull_request: PullRequest) -> Result<InnerReview> {
        let Self {
            state,
            repo,
            number,
            author,
            installation,
            run,
            repo_id,
            read_token,
            forge,
            lease,
        } = self;
        // Comment and review-comment deliveries do not carry the draft flag, so
        // routing alone cannot prevent a manual-looking request from waking the
        // workflow. Read the live state before claiming a lease, indexing, calling
        // a model, or minting a write token; drafts are recorded through delivery
        // claims and start their first workflow only once GitHub says they are
        // ready for review.
        tracing::Span::current().record("head_sha", pull_request.head_sha.as_str());
        run.timing
            .record(|record| record.head_sha = pull_request.head_sha.clone());
        if pull_request.draft {
            run.timing.finish("declined");
            tracing::debug!(%repo, number, "tracking draft pull request without reviewing");
            return Ok(InnerReview::Complete(None));
        }

        // The head may have moved after the webhook was delivered, or there
        // may have been no SHA to claim (manual/comment request). Re-key before
        // indexing or model work. The caller releases this final key on every
        // normal return, while the old claim is released only after the new
        // one is acquired.
        if run.expected_head.as_deref() != Some(pull_request.head_sha.as_str()) {
            let next = review_lease(repo, number, run.mode, Some(&pull_request.head_sha));
            let holder = review_lease_holder(run);
            let claimed = state.store.claim_lease(&next, &holder).await?;
            if continue_if_lease_claimed(claimed, || async { Ok(()) })
                .await?
                .is_none()
            {
                tracing::debug!(%next, "another worker holds the live pull request head");
                run.lease_busy
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return Ok(InnerReview::Busy);
            }
            if let Some(previous) = lease.replace(next)
                && let Err(err) = state.store.release_lease_for(&previous, &holder).await
            {
                tracing::error!(%err, %previous, "could not release the previous review lease");
            }
        }

        // The live head whose exact lease is now held is the revision passed
        // into context construction, even for a stateless request that had no
        // webhook SHA. This lets the app recheck for a move during that fetch.
        let run = Run {
            expected_head: Some(pull_request.head_sha.clone()),
            ..run.clone()
        };
        if let Some(owned) = &run.owned {
            match state
                .review_jobs
                .reconcile_live_check(owned, &pull_request.head_sha, unix_time_ms())
                .await?
            {
                crate::ports::review_job::ReconciledReviewCheck::Ready(check) => {
                    let Some(check_id) = check.check_id else {
                        return Err(Error::ReviewOwnershipLost);
                    };
                    *run.slot.lock().expect("status slot") = Some(ReviewStatus {
                        repo: repo_id.clone(),
                        check_id,
                        head_sha: check.head_sha,
                        installation,
                        stage: check.stage,
                        owned: Some(owned.clone()),
                        hub_comment_id: None,
                        prior_hub_body: None,
                    });
                }
                crate::ports::review_job::ReconciledReviewCheck::Pending(_)
                | crate::ports::review_job::ReconciledReviewCheck::CapacityDeferred => {
                    // The old head has already moved to the detached outbox.
                    // Clearing the slot prevents Busy handling from PATCHing it
                    // independently while the durable worker publishes the new head.
                    run.slot.lock().expect("status slot").take();
                    let _ = state
                        .review_jobs
                        .defer(owned, unix_time_ms(), unix_time_ms())
                        .await?;
                    return Ok(InnerReview::Busy);
                }
                crate::ports::review_job::ReconciledReviewCheck::OwnershipLost => {
                    return Err(Error::ReviewOwnershipLost);
                }
            }
        }
        run.check_work(state).await?;

        run_if_confirmed_head_needs_review(
            state.review_jobs.as_ref(),
            run.owned.as_ref(),
            run.mode,
            || async {
                // A manual review deliberately takes a lease of its own: the operator asked
                // for this run *because* the ordinary one already happened, so sharing the
                // webhook path's key would make the button a silent no-op.
                // The lease is held and the pull request is real, so this run is the one
                // that will review this commit — which makes it the run that owns the
                // status check. Opening it here rather than on the delivery path is what
                // keeps a blocked contributor, a draft, or a duplicate delivery from
                // announcing a review that is not going to happen.
                //
                // Still early: everything above is metadata reads, and every model call is
                // below. A contributor sees the check appear seconds after pushing, not
                // minutes.
                //
                // Deliberately *not* under `run.deadline`, and neither is anything else
                // between here and `run_lanes`. The deadline is a cancellation, and
                // cancelling a check-run POST after GitHub accepted it orphans the check
                // this function exists to conclude. The forge calls here are bounded on
                // their own by `forge::github::REQUEST_TIMEOUT`, which is what the margin
                // between `REVIEW_DEADLINE` and `LEASE_TTL` is for.
                review_with_paid_budget_and_status(
            state.review_jobs.as_ref(),
            run.owned.as_ref(),
            unix_time_ms(),
            || async {
                run.check_work(state).await?;
                if let Some(backend) = &state.index {
                    tokio::spawn(index_in_background(
                        backend.clone(),
                        Arc::new(state.config.config.clone()),
                        state.index_permits.clone(),
                        repo_id.clone(),
                        pull_request.head_sha.clone(),
                        read_token.to_owned(),
                    ));
                }
                run.check(repo, number)?;
                Ok(())
            },
            || async {
                run.check_work(state).await?;
                run.check(repo, number)?;
                Ok(crate::review_timing::measure(Some(&run.timing), "open_status", open_status(
                    state,
                    &run.slot,
                    repo_id,
                    &pull_request.head_sha,
                    installation,
                ))
                .await)
            },
            || async {
                run.check_work(state).await?;
            let outcome = {

            // `AssertUnwindSafe` + `catch_unwind` so a panic inside a lane still
            // reaches the release below. Without it the `?` on the outcome is not
            // the only way out — an unwind skips everything — and the lease
            // survives the worker that took it.
            // The reviewed repository's own policy, read through the forge
            // because there is no checkout here. Without this every repository is
            // reviewed under the *deployment's* `.tinysweeper.toml`, which is
            // tinysweeper's own. Read at the base branch's tip rather than the
            // head: a config is acted on deterministically, so reading it from
            // the branch under review would let a pull request grade its own
            // exam. See `crate::config::remote`.
            let overlay = crate::review_timing::measure(Some(&run.timing), "knowledge",
                crate::config::remote::overlay(
                    forge, repo_id, &pull_request.base_sha, &state.config.config,
                ),
            ).await;
            if let Some(source) = &overlay.source {
                tracing::info!(%repo, source, "reviewing under the repository's own configuration");
            }

            // The lease is owned and repository policy is known. Publish the hub
            // now, before any model call, so it stays an early timeline reference.
            if !review_is_kill_switched(&overlay.config, &pull_request) {
                open_review_hub(
                    state,
                    &run.slot,
                    forge,
                    &overlay.config,
                    repo_id,
                    number,
                    &pull_request.head_sha,
                    installation,
                )
                .await;
            }

            // `AssertUnwindSafe` + `catch_unwind` so a panic inside a lane still
            // reaches the release below. Without it the `?` on the outcome is not
            // the only way out — an unwind skips everything — and the lease
            // survives the worker that took it. The boundary check first: the
            // metadata phase above was bounded per call, not by the deadline, so
            // this is where a budget it exhausted is noticed.
            let lanes = match run.check(repo, number).map(|_| ()) {
                Ok(()) => match run.check_work(state).await {
                    Ok(()) => std::panic::AssertUnwindSafe(run_lanes(
                        state,
                        &overlay.config,
                        LaneTarget {
                            repo: repo_id,
                            number,
                        },
                        forge,
                        read_token,
                        &pull_request,
                        &run,
                    ))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err(Error::lane("review", "the review panicked"))),
                    Err(err) => Err(err),
                },
                Err(spent) => Err(spent),
            };

            // The publish runs under its own, separate budget rather than the
            // remainder of `run.deadline`: a review that used all of its time in
            // the lanes still gets a full window to publish, because cancelling
            // `apply` between two of its non-idempotent writes leaves a
            // permanently partial review, and that must only ever happen to a
            // publish that is stuck. See `PUBLISH_DEADLINE`. The write token is
            // minted only now, after every model call has returned — the
            // boundary in `AGENTS.md`.
            match lanes {
                Ok((config, proposal)) => {
                    // `AssertUnwindSafe` + `catch_unwind` here too, same reason as
                    // around `run_lanes`: without it a panic inside `apply` skips
                    // `release_lease` below and escapes
                    // `handle_review` entirely, deregistering the `InFlight` slot
                    // on the way out (`Drop` always runs) without ever concluding
                    // its check — a lease held until `LEASE_TTL` and a check stuck
                    // "in progress" forever, which is exactly what this whole
                    // umbrella-check mechanism exists to prevent.
                    let publish = std::panic::AssertUnwindSafe(async {
                run.check_work(state).await?;
                        let write_token = state.auth.installation_token(installation).await?;
                        run.check_work(state).await?;
                        // The review reader has already been held across all the
                        // model calls. Rebuild the publisher's reader with the
                        // credential acquired now so live-head validation and
                        // later dedupe reads have the same lifetime as writes.
                        let read = crate::forge::github::GitHubRead::new(&write_token)?;
                        let write = crate::forge::github::GitHubWrite::new(&write_token)?;
                        crate::app::apply(&read, &write, &config, &proposal, Some(&state.store))
                            .await
                    })
                    .catch_unwind();
                    crate::review_timing::measure(Some(&run.timing), "publish",
                        tokio::time::timeout(PUBLISH_DEADLINE, publish))
                        .await
                        .map_err(|_elapsed| {
                            Error::timeout(
                                format!("publishing the review of {repo}#{number}"),
                                PUBLISH_DEADLINE,
                            )
                        })
                        .and_then(|published| {
                            published.unwrap_or_else(|_| {
                                Err(Error::lane("review", "publishing panicked"))
                            })
                        })
                        .map(|disposition| {
                            let changes_requested =
                                crate::app::apply::requests_changes(&config, &proposal);
                            (disposition, proposal, changes_requested)
                        })
                }
                Err(err) => Err(err),
            }
            };

        let (disposition, proposal, changes_requested) = outcome?;
        let Some((findings, changes_requested, coverage)) = published_review_result(
            disposition,
            proposal.findings().count(),
            changes_requested,
            status::aggregate_coverage(&proposal),
        ) else {
            run.timing.finish("superseded");
            // The head moved, or the PR closed/merged, after the lanes ran.
            // Leave the already-open umbrella check terminally NotReviewed;
            // neither count the attempt nor kick off automerge.
            return Ok(InnerReview::Complete(None));
        };
        state.store.record_review(author, findings as u64).await?;

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

            Ok(InnerReview::Complete(Some((
                findings,
                changes_requested,
                coverage,
            ))))
            },
        )
        .await
            },
        )
        .await
    }
}

/// Read the live pull request and allow work only while it is still reviewable.
///
/// Webhook payloads can be stale by the time the queued worker starts, and
/// manual reviews are keyed by a head SHA rather than a delivery. This is the
/// shared gate before indexing, lease/check writes, and model calls.
async fn with_open_pull_request<T, F, Fut>(
    forge: &dyn crate::ports::forge::ForgeRead,
    repo: &RepoId,
    number: u64,
    work: F,
) -> Result<Option<T>>
where
    F: FnOnce(PullRequest) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let pull_request = forge.pull_request(repo, number).await?;
    if !pull_request.open || pull_request.merged {
        return Ok(None);
    }
    work(pull_request).await.map(Some)
}

fn published_review_result(
    disposition: crate::app::apply::ApplyDisposition,
    findings: usize,
    changes_requested: bool,
    coverage: status::ReviewCoverage,
) -> Option<(usize, bool, status::ReviewCoverage)> {
    if disposition != crate::app::apply::ApplyDisposition::Published {
        return None;
    }
    Some((findings, changes_requested, coverage))
}

fn review_is_kill_switched(config: &Config, pull_request: &PullRequest) -> bool {
    [&config.labels.human_review, &config.labels.manual_only]
        .into_iter()
        .any(|label| !label.is_empty() && pull_request.labels.contains(label))
}

/// Repository and pull request identity used by the review lanes.
#[derive(Clone, Copy)]
struct LaneTarget<'a> {
    repo: &'a RepoId,
    number: u64,
}

/// Run the checkout and the lanes under the deadline, and hand back what they
/// produced for `review_inner` to publish uncancelled.
///
/// `config` is the *effective* config for this repository — the deployment's,
/// with the reviewed repository's own allow-listed keys laid over it. The model
/// gateway and the index are still built from the deployment's config, because
/// model choice, credentials and the index partition key are not things a
/// reviewed repository may set. The config handed back is that one with the
/// run's mode layered on, so the publish applies the same policy the lanes
/// ran under.
///
/// Holds no write credential: nothing in here runs after a model call has
/// returned, so nothing in here may mint one.
#[tracing::instrument(
    skip_all,
    fields(repo = %target.repo, pr = target.number, phase = "checkout_and_review")
)]
async fn run_lanes(
    state: &AppState,
    config: &Config,
    target: LaneTarget<'_>,
    forge: &crate::forge::github::GitHubRead,
    read_token: &str,
    pull_request: &PullRequest,
    run: &Run,
) -> Result<(Config, crate::app::Proposal)> {
    let LaneTarget { repo, number } = target;
    let binding = run
        .execution
        .bind_model(state.models.text(), config.models.budget_usd_per_pr);
    let model = match &run.owned {
        Some(owned) => Arc::new(OwnedModel {
            inner: binding.model.clone(),
            execution: run.execution.execution(),
            jobs: state.review_jobs.clone(),
            owned: owned.clone(),
            owner_lost: run.owner_lost.clone(),
            owner_until: run.owner_until.clone(),
        }) as Arc<dyn Model>,
        None => binding.model.clone(),
    };

    // The model runs against a read-only handle. The write token is minted by
    // the caller, after this returns — same boundary as the workflow, same
    // reason.
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
    // same policy, not the deployment's policy instead.

    let config = config_for(config, run.mode);
    let config_ref = &config;

    // The deadline bounds the checkout and the lanes — everything that can
    // take minutes and nothing that must not be cut short. `timeout_at` drops
    // the inner future when it elapses, which cancels every model call in
    // flight. The checkout sits inside it so that a deadline already in the
    // past — a retry after a slow failure — resolves at once, before a clone
    // is even started, which is the intended way of refusing the retry.
    // The tree the reviewers may look things up in. A shallow checkout of
    // the head when `[lookup].checkout` allows it — one commit, no history,
    // no hooks, the same fetch the indexer makes — so search works and a
    // read costs no API call; the forge reader behind it for what a shallow
    // checkout lacks, such as a submodule that was not fetched. Read-only
    // either way: the token here is the review-read one the forge already
    // holds, and the checkout is deleted with the review.
    let review = checkout_then_review_before_deadline(
        &run.timing,
        run.deadline,
        repo,
        number,
        || async {
            if config_ref.lookup.enabled && config_ref.lookup.checkout {
                let head = &pull_request.head_sha;
                match state
                    .repository_cache
                    .checkout_for_diff(
                        &super::indexing::git_host(),
                        &repo.to_string(),
                        head,
                        &pull_request.base_sha,
                        read_token,
                    )
                    .await
                {
                    Ok(checkout) => {
                        if !config_ref.retrieval.submodules.is_empty()
                            && let Err(err) = checkout
                                .fetch_submodules_for_diff(
                                    &pull_request.base_sha,
                                    &super::indexing::git_host(),
                                    read_token,
                                    &config_ref.retrieval.submodules,
                                )
                                .await
                        {
                            tracing::warn!(%repo, %err, "submodules not fetched for the review's tree");
                        }
                        Ok::<Option<crate::indexer::fetch::Checkout>, Error>(Some(checkout))
                    }
                    Err(err) => {
                        tracing::warn!(%repo, %err, "no checkout for the review; lookups read through the forge");
                        Ok::<Option<crate::indexer::fetch::Checkout>, Error>(None)
                    }
                }
            } else {
                Ok::<Option<crate::indexer::fetch::Checkout>, Error>(None)
            }
        },
        |checkout| async move {
            // The review chains the forge reader behind whatever it is given,
            // so a path the shallow checkout lacks — a submodule that was not
            // fetched — is still read through the API.
            let dir_tree = checkout
                .as_ref()
                .map(|c| crate::ports::tree::DirTree::new(c.path()).at_revision(c.revision()));
            let tree = dir_tree
                .as_ref()
                .map(|dir| dir as &dyn crate::ports::tree::TreeReader);

            crate::app::review::review_with_tree_observed_at_head_in_run(
                forge,
                model,
                config_ref.as_ref(),
                repo,
                number,
                Some(&state.store),
                state.knowledge.as_deref(),
                retriever.as_ref(),
                tree,
                run.expected_head.as_deref(),
                Arc::clone(&run.execution),
                Some(run.timing.clone()),
            )
            .await
        },
    );
    let proposal = select_review_control(
        review,
        state.review_jobs.as_ref(),
        run.owned.as_ref(),
        run.owner_lost.clone(),
        run.owner_until.clone(),
        run.execution.execution(),
    )
    .await?;

    Ok((config.into_owned(), proposal))
}

async fn wait_for_owner_fence(
    mut lost: Option<watch::Receiver<bool>>,
    mut until: Option<watch::Receiver<tokio::time::Instant>>,
) {
    loop {
        if lost.as_ref().is_some_and(|receiver| *receiver.borrow()) {
            return;
        }
        if until
            .as_ref()
            .is_some_and(|receiver| tokio::time::Instant::now() >= *receiver.borrow())
        {
            return;
        }

        match (&mut lost, &mut until) {
            (Some(lost), Some(until)) => {
                let deadline = *until.borrow();
                tokio::select! {
                    changed = lost.changed() => if changed.is_err() { return; },
                    changed = until.changed() => if changed.is_err() { return; },
                    _ = tokio::time::sleep_until(deadline) => return,
                }
            }
            (Some(lost), None) => {
                if lost.changed().await.is_err() {
                    return;
                }
            }
            (None, Some(until)) => {
                let deadline = *until.borrow();
                tokio::select! {
                    changed = until.changed() => if changed.is_err() { return; },
                    _ = tokio::time::sleep_until(deadline) => return,
                }
            }
            (None, None) => std::future::pending::<()>().await,
        }
    }
}

/// Enforce the server's review deadline and record it before cancellation
/// drops the in-flight pipeline and its model futures.
async fn review_before_deadline<F, T>(
    timing: &crate::review_timing::ReviewTiming,
    deadline: tokio::time::Instant,
    repo: &RepoId,
    number: u64,
    future: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    match tokio::time::timeout_at(deadline, future).await {
        Ok(result) => result,
        Err(_) => {
            timing.finish("deadline");
            Err(Error::timeout(
                format!("the review of {repo}#{number}"),
                REVIEW_DEADLINE,
            ))
        }
    }
}

/// Sequence the durable status gate, read-only review work, and publication.
/// The route supplies closures so no model work starts before the status is
/// open, and publication cannot start until checkout and review have returned.
#[cfg(test)]
async fn server_review_lifecycle<
    Open,
    OpenFuture,
    Review,
    ReviewFuture,
    Publish,
    PublishFuture,
    R,
    T,
>(
    timing: &crate::review_timing::ReviewTiming,
    open_status: Open,
    review: Review,
    publish: Publish,
) -> Result<Option<T>>
where
    Open: FnOnce() -> OpenFuture,
    OpenFuture: std::future::Future<Output = Result<bool>>,
    Review: FnOnce() -> ReviewFuture,
    ReviewFuture: std::future::Future<Output = Result<R>>,
    Publish: FnOnce(R) -> PublishFuture,
    PublishFuture: std::future::Future<Output = Result<Option<T>>>,
{
    let go_on = crate::review_timing::measure(Some(timing), "open_status", open_status()).await?;
    if !go_on {
        return Ok(None);
    }
    let review = review().await?;
    publish(review).await
}

/// Measure checkout separately while bounding checkout and review as one
/// cancellable server stage. This is the production boundary used by both the
/// route and MockForge/MockModel lifecycle tests.
async fn checkout_then_review_before_deadline<
    C,
    R,
    Checkout,
    CheckoutFuture,
    Review,
    ReviewFuture,
>(
    timing: &crate::review_timing::ReviewTiming,
    deadline: tokio::time::Instant,
    repo: &RepoId,
    number: u64,
    checkout: Checkout,
    review: Review,
) -> Result<R>
where
    Checkout: FnOnce() -> CheckoutFuture,
    CheckoutFuture: std::future::Future<Output = Result<C>>,
    Review: FnOnce(C) -> ReviewFuture,
    ReviewFuture: std::future::Future<Output = Result<R>>,
{
    let pipeline = async {
        let checkout = crate::review_timing::measure(Some(timing), "checkout", checkout()).await?;
        review(checkout).await
    };
    review_before_deadline(timing, deadline, repo, number, pipeline).await
}

/// Poll the durable control plane while model work is in flight. The provider
/// future is dropped as soon as another process records a superseding hint;
/// the owner lease remains held until `handle_review` acknowledges the yield.
async fn wait_for_review_yield(
    jobs: &dyn ReviewJobStore,
    owned: Option<&OwnedReview>,
) -> Result<()> {
    let Some(owned) = owned else {
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if jobs.should_continue(owned, unix_time_ms()).await? {
            continue;
        }
        return if jobs.owns(owned, unix_time_ms()).await? {
            Err(Error::ReviewSuperseded)
        } else {
            Err(Error::ReviewOwnershipLost)
        };
    }
}

async fn select_review_control<F, T>(
    review: F,
    jobs: &dyn ReviewJobStore,
    owned: Option<&OwnedReview>,
    owner_lost: Option<watch::Receiver<bool>>,
    owner_until: Option<watch::Receiver<tokio::time::Instant>>,
    execution: Arc<crate::app::review_execution::ReviewExecution>,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::select! {
        biased;
        _ = wait_for_owner_fence(owner_lost, owner_until) => {
            execution.cancel();
            Err(Error::ReviewOwnershipLost)
        },
        result = wait_for_review_yield(jobs, owned) => {
            let err = result.expect_err("yield waiter only completes when control changes");
            if matches!(err, Error::ReviewOwnershipLost | Error::ReviewSuperseded) {
                execution.cancel();
            }
            Err(err)
        },
        result = review => {
            if matches!(&result, Err(Error::ReviewOwnershipLost | Error::ReviewSuperseded)) {
                execution.cancel();
            }
            result
        },
    }
}

/// The UI preview routes' way into the brain.
///
/// Each call loads the session, does one thing, and writes it back. Nothing
/// is kept in memory between calls, so a redeploy mid-session costs the turn
/// in flight and nothing else.
struct PreviewDispatch {
    state: AppState,
}

impl PreviewDispatch {
    /// The session, or the error the hands can act on.
    async fn session(&self, id: &str) -> Result<crate::preview::session::Session> {
        self.state
            .store
            .preview_session(id)
            .await?
            .ok_or_else(|| Error::Config(format!("no preview session `{id}`; it may have expired")))
    }

    /// Hold this session's lock for the duration of a load-modify-save.
    ///
    /// Two `step` calls for the same session — a retried request, or two
    /// flows genuinely racing — would otherwise each load the same document,
    /// mutate their own copy, and save it back; the second save wins and the
    /// first flow's transition is lost. Serialising through one lock per
    /// session id, rather than one lock for the whole dispatcher, keeps
    /// unrelated sessions from waiting on each other.
    async fn lock_session(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.state.preview_locks.lock().expect("preview locks");
            locks.entry(id.to_string()).or_default().clone()
        };
        lock.lock_owned().await
    }
}

#[async_trait::async_trait]
impl Previews for PreviewDispatch {
    async fn start(&self, request: StartRequest) -> Result<StartReply> {
        let config = &self.state.config.config;
        let off = StartReply {
            enabled: false,
            session: None,
            flows: vec![],
            max_steps: 0,
        };
        if !config.preview.enabled {
            return Ok(off);
        }
        let base_url = config.preview.public_base_url.as_deref().unwrap_or("");
        if base_url.is_empty() {
            // Validation refuses this configuration, but the check is cheap
            // and the alternative is composing URLs onto nothing.
            return Ok(off);
        }

        let (owner, name) = request
            .repo
            .split_once('/')
            .ok_or_else(|| Error::Config(format!("`{}` is not owner/name", request.repo)))?;
        let repo =
            manual::checked_target(owner, name, &manual::allowed_org()).map_err(Error::Config)?;

        // The token proved a CI job in our organisation; this proves *which*
        // pull request, against GitHub rather than the request.
        let installation = self
            .state
            .auth
            .installation_for_repo(&repo.owner, &repo.name)
            .await?;
        // Read-scoped, like the review path's own pre-model read
        // (`review_read_token` around line 1470): everything below this
        // point, through the planning model call, only reads.
        let read_token = self.state.auth.review_read_token(installation).await?;
        let forge = crate::forge::github::GitHubRead::new(&read_token)?;
        let pull_request = forge.pull_request(&repo, request.pull_request).await?;
        if pull_request.head_sha != request.head_sha {
            return Err(Error::Config(format!(
                "#{} is at {} on GitHub, not {}; a push has happened since this job started",
                request.pull_request, pull_request.head_sha, request.head_sha
            )));
        }

        // The repository may turn previews off for itself, read at the base
        // commit like every other repository setting.
        let overlaid =
            crate::config::remote::overlay(&forge, &repo, &pull_request.base_sha, config).await;
        if !overlaid.config.preview.enabled {
            return Ok(off);
        }
        let effective = overlaid.config;

        let files = forge.changed_files(&repo, request.pull_request).await?;
        let diffs = crate::evidence::diff::parse_changed_files(&files);
        let model = self.state.models.text();
        let plan = crate::preview::plan::plan(
            &crate::preview::plan::PlanInputs {
                diffs: &diffs,
                title: &pull_request.title,
                entry_points: &request.entry_points,
                max_flows: effective.preview.max_flows,
                model: effective.model_for_workload(crate::config::types::Workload::Preview),
                max_tokens: effective.models.max_tokens,
            },
            model,
        )
        .await?;

        // Nothing planned: the diff changes nothing a user can see. The hands
        // never call `step` or `finish` for an empty plan (there is nothing
        // to drive or publish), so a session saved here would sit unused
        // until its TTL. Answer "enabled, nothing to do" without persisting
        // one.
        if plan.flows.is_empty() {
            return Ok(StartReply {
                enabled: true,
                session: None,
                flows: vec![],
                max_steps: effective.preview.max_steps,
            });
        }

        let session = crate::preview::session::Session {
            id: crate::preview::session::new_id(
                &request.repo,
                request.pull_request,
                &request.head_sha,
            ),
            repo: repo.to_string(),
            number: request.pull_request,
            head_sha: request.head_sha.clone(),
            base_sha: request.base_sha.clone(),
            installation,
            flows: plan.flows.clone(),
            states: Default::default(),
            diff_excerpt: crate::preview::session::excerpt(&diffs),
            spent_usd: plan.spend.usage.cost_usd,
            max_steps: effective.preview.max_steps,
            check_id: None,
        };
        self.state.store.save_preview_session(&session).await?;
        tracing::info!(
            repo = %repo,
            number = request.pull_request,
            flows = session.flows.len(),
            "opened a UI preview session"
        );

        Ok(StartReply {
            enabled: true,
            session: Some(session.id),
            flows: plan.flows,
            max_steps: effective.preview.max_steps,
        })
    }

    async fn step(
        &self,
        id: &str,
        flow_id: &str,
        observation: crate::preview::types::Observation,
    ) -> Result<PreviewStepReply> {
        let config = &self.state.config.config;
        // Held for the whole load-modify-save below, so a retried or racing
        // call for this same session waits rather than clobbering it.
        let _lock = self.lock_session(id).await;
        let mut session = self.session(id).await?;
        let flow = session
            .flow(flow_id)
            .cloned()
            .ok_or_else(|| Error::Config(format!("no flow `{flow_id}` in this session")))?;

        // The `before` side replays the `after` script and asks nothing; a
        // step call for it is answered from the recorded script so a hands
        // implementation that asks anyway gets the same answer for free.
        if observation.side == crate::preview::types::Side::Before {
            let script = session
                .states
                .get(flow_id)
                .map(|state| state.commands.clone())
                .unwrap_or_default();
            return Ok(PreviewStepReply {
                commands: vec![],
                done: true,
                replay: Some(script),
            });
        }

        let mut state = session.states.remove(flow_id).unwrap_or_default();
        let reply = if session.exhausted(config.preview.budget_usd) {
            // Closed without a call, and every later flow the same way: the
            // ceiling is per session, not per flow.
            state.done = true;
            let mut commands = Vec::new();
            if !state.commands.is_empty() {
                commands.push(crate::preview::types::Command::Record { start: false });
            }
            commands.push(crate::preview::types::Command::Done {
                reason: "session budget spent".into(),
            });
            state.commands.extend(commands.iter().cloned());
            crate::preview::step::StepReply {
                commands,
                done: true,
                spend: Default::default(),
            }
        } else {
            let model = self.state.models.text();
            crate::preview::step::next(
                &crate::preview::step::StepContext {
                    flow: &flow,
                    diff_excerpt: &session.diff_excerpt,
                    max_steps: session.max_steps,
                    model: config.model_for_workload(crate::config::types::Workload::Preview),
                    max_tokens: config.models.max_tokens,
                },
                &mut state,
                &observation,
                model,
            )
            .await?
        };
        session.spent_usd += reply.spend.usage.cost_usd;
        let replay = reply.done.then(|| state.commands.clone());
        session.states.insert(flow_id.to_string(), state);
        self.state.store.save_preview_session(&session).await?;

        Ok(PreviewStepReply {
            commands: reply.commands,
            done: reply.done,
            replay,
        })
    }

    async fn finish(&self, id: &str, request: FinishRequest) -> Result<FinishReply> {
        let config = &self.state.config.config;
        // Same lock as `step`: `finish` also loads, mutates (`check_id`) and
        // saves this session, and a retried `finish` racing a straggling
        // `step` must not interleave with it either.
        let _lock = self.lock_session(id).await;
        let mut session = self.session(id).await?;
        let base_url = config.preview.public_base_url.as_deref().unwrap_or("");

        let mut gallery = crate::preview::manifest::validate(
            &request.manifest,
            &crate::preview::manifest::Expected {
                repo: &session.repo,
                number: session.number,
                head_sha: &session.head_sha,
            },
            base_url,
            config.preview.max_flows,
            &session.flows,
            &session.states.keys().cloned().collect(),
        )?;

        // Captions are the last model calls, and they are made before the
        // write token exists — the same order as a review. `step` already
        // stops driving once the session's budget is spent; captioning after
        // that point would make more calls past the same ceiling, so it is
        // gated the same way.
        if config.preview.caption
            && !gallery.flows.is_empty()
            && !session.exhausted(config.preview.budget_usd)
        {
            let states: Vec<(String, crate::preview::step::FlowState)> = session
                .states
                .iter()
                .map(|(id, state)| (id.clone(), state.clone()))
                .collect();
            let (model, model_id, vision) = self.state.models.caption(config);
            let spend = crate::preview::caption::caption(
                &mut gallery,
                &crate::preview::caption::CaptionInputs {
                    diff_excerpt: &session.diff_excerpt,
                    states: &states,
                    model: model_id,
                    vision,
                    max_tokens: config.models.max_tokens,
                    spent_usd: session.spent_usd,
                    budget_usd: config.preview.budget_usd,
                },
                model,
            )
            .await;
            // Persisted, not just logged: a retried finish for a still-present
            // session (see below) must see this spend already accounted for,
            // not repeat every caption call.
            session.spent_usd += spend.usage.cost_usd;
            tracing::info!(
                session = %session.id,
                cost_usd = session.spent_usd,
                "UI preview session spent"
            );
        }

        let repo = RepoId::parse(&session.repo)
            .ok_or_else(|| Error::Forge(format!("`{}` is not owner/name", session.repo)))?;
        let read_token = self
            .state
            .auth
            .installation_token(session.installation)
            .await?;
        let read = crate::forge::github::GitHubRead::new(&read_token)?;
        let write_token = self
            .state
            .auth
            .installation_token(session.installation)
            .await?;
        let write = crate::forge::github::GitHubWrite::new(&write_token)?;
        let (outcome, check_id) = crate::preview::apply::publish(
            &read,
            &write,
            &repo.to_string(),
            &gallery,
            session.check_id,
        )
        .await?;

        // Not deleted: the hands' own HTTP client retries a dropped response,
        // and a `finish` that deleted its session on success would make that
        // retry fail with "no preview session" despite the first attempt
        // having already published. `publish` is idempotent per head commit
        // (the comment is edited in place, and `check_id` — saved back here —
        // makes the check run reused rather than duplicated), so a retried
        // finish for a still-present session simply reconfirms the same
        // outcome. The session store's own TTL index (`PREVIEW_SESSION_TTL`,
        // two hours) is what actually reclaims it.
        session.check_id = check_id;
        if let Err(err) = self.state.store.save_preview_session(&session).await {
            tracing::warn!(%err, "could not record the published check id on the preview session");
        }

        Ok(FinishReply {
            outcome: format!("{outcome:?}").to_ascii_lowercase(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_execution_context(
        started_at: tokio::time::Instant,
        hard_deadline: tokio::time::Instant,
    ) -> Arc<crate::app::review_execution::ReviewRunContext> {
        let large_pr = Config::default().review.large_pr;
        Arc::new(crate::app::review_execution::ReviewRunContext::new(
            crate::app::review_execution::ReviewExecutionOptions {
                started_at,
                hard_deadline,
                mode: crate::lanes::large_pr::ReviewMode::Normal,
                max_calls: None,
                soft_deadline_percent: large_pr.soft_deadline_percent,
                drain_seconds: large_pr.drain_seconds,
            },
        ))
    }

    fn test_execution_scope() -> Arc<crate::app::review_execution::ReviewExecution> {
        let started_at = tokio::time::Instant::now();
        test_execution_context(started_at, started_at + std::time::Duration::from_secs(60))
            .execution()
    }

    #[derive(Clone, Default)]
    struct ReviewBudgetProbe {
        remaining: Option<Arc<std::sync::atomic::AtomicUsize>>,
        completed: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Model for ReviewBudgetProbe {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            use std::sync::atomic::Ordering;

            if let Some(remaining) = &self.remaining
                && remaining
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        count.checked_sub(1)
                    })
                    .is_err()
            {
                return Err(Error::Budget {
                    spent: 1.0,
                    limit: 1.0,
                    usage: None,
                });
            }
            self.completed.fetch_add(1, Ordering::Relaxed);
            Ok(ModelResponse {
                value: json!({"ok": true}),
                model: "review-budget-probe".into(),
                usage: crate::ports::model::Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                    calls_attempted: 1,
                    calls_succeeded: 1,
                    cost_usd: 1.0,
                    ..Default::default()
                },
            })
        }

        fn scoped_budget(&self, _budget_usd: f64) -> Option<Arc<dyn Model>> {
            Some(Arc::new(self.clone()))
        }

        fn with_review_budget(&self, limit_usd: f64) -> Option<Arc<dyn Model>> {
            let calls = limit_usd.floor().max(0.0) as usize;
            Some(Arc::new(Self {
                remaining: Some(Arc::new(std::sync::atomic::AtomicUsize::new(calls))),
                completed: self.completed.clone(),
            }))
        }
    }

    async fn owned_review_budget_probe() -> (
        OwnedModel,
        Arc<crate::ports::review_job_mock::MockReviewJobStore>,
        ReviewRequest,
        ReviewBudgetProbe,
    ) {
        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let mut request = queued_test_review(928);
        request.repo = "owner/repo".into();
        request.number = 7;
        request.head_sha = Some("head-a".into());
        store.admit_batch(vec![request.clone()]).await.unwrap();
        let now = unix_time_ms();
        seed_previously_published_queue_checks(&*store, now).await;
        let owned = store
            .claim_next("review-budget-worker", now)
            .await
            .unwrap()
            .unwrap();
        assert!(store.confirm_head(&owned, "head-a", now).await.unwrap());

        let probe = ReviewBudgetProbe::default();
        let guarded = OwnedModel {
            inner: Arc::new(probe.clone()),
            execution: test_execution_scope(),
            jobs: store.clone(),
            owned,
            owner_lost: None,
            owner_until: None,
        };
        (guarded, store, request, probe)
    }

    #[tokio::test]
    async fn owned_model_preserves_review_budget_and_ownership_across_rescoping() {
        use crate::ports::model::Message;
        use std::sync::atomic::Ordering;

        for bind_after_scope in [false, true] {
            let (guarded, store, mut original, probe) = owned_review_budget_probe().await;
            let model: Arc<dyn Model> = Arc::new(guarded);
            let root = if bind_after_scope {
                model
                    .scoped_budget(8.0)
                    .expect("owned wrapper forwards lane scopes")
                    .with_review_budget(1.0)
                    .expect("owned wrapper forwards review binding")
            } else {
                model
                    .with_review_budget(1.0)
                    .expect("owned wrapper forwards review binding")
            };
            let lane = root
                .scoped_budget(8.0)
                .expect("review-root scopes retain worker ownership");
            let model_request = || ModelRequest {
                model: "probe".into(),
                messages: vec![Message::user("review the leased head")],
                schema: json!({"type":"object"}),
                schema_name: "review-root-probe".into(),
                reasoning_effort: None,
                max_tokens: 16,
            };

            assert!(root.complete(model_request()).await.is_ok());
            assert!(matches!(
                lane.complete(model_request()).await,
                Err(Error::Budget { .. })
            ));
            assert_eq!(probe.completed.load(Ordering::Relaxed), 1);

            original.id = crate::ports::review_job::ReviewJobId("new-head-delivery".into());
            original.delivery = Some("new-head-delivery".into());
            original.head_sha = Some("head-b".into());
            store.admit_batch(vec![original]).await.unwrap();
            assert!(matches!(
                lane.complete(model_request()).await,
                Err(Error::ReviewSuperseded)
            ));
            assert_eq!(
                probe.completed.load(Ordering::Relaxed),
                1,
                "the old lease cannot reach the provider after ownership changes"
            );
        }
    }

    #[tokio::test]
    async fn owned_model_preserves_dispatch_control_and_lease_across_rescoping() {
        use crate::harness::mock::MockModel;
        use crate::harness::mock::execution_test_support::{DispatchProbe, execution_control};
        use crate::ports::model::Message;
        use crate::ports::model_execution::ModelExecutionRefusal;

        for scope_first in [false, true] {
            for control_first in [false, true] {
                let (mut guarded, store, mut original, _) = owned_review_budget_probe().await;
                let mock = MockModel::new()
                    .then(json!({"summary":"first"}))
                    .then(json!({"summary":"must be refused"}))
                    .with_usage(crate::ports::model::Usage {
                        input_tokens: 2,
                        output_tokens: 1,
                        calls_attempted: 1,
                        calls_succeeded: 1,
                        cost_usd: 1.0,
                        ..Default::default()
                    });
                guarded.inner = Arc::new(DispatchProbe::new(mock.clone()));
                let model: Arc<dyn Model> = Arc::new(guarded);
                let (execution, control) = execution_control(1);
                let scoped = if scope_first {
                    model
                        .scoped_budget(8.0)
                        .expect("owned wrapper creates a lane scope")
                } else {
                    model
                };
                let root = if control_first {
                    let controlled = scoped
                        .with_execution_control(control)
                        .unwrap_or_else(|| scoped.clone());
                    controlled
                        .with_review_budget(10.0)
                        .expect("owned wrapper binds a review budget")
                } else {
                    let budgeted = scoped
                        .with_review_budget(10.0)
                        .expect("owned wrapper binds a review budget");
                    budgeted.with_execution_control(control).unwrap_or(budgeted)
                };
                let lane = root
                    .scoped_budget(8.0)
                    .expect("scoped owned model retains control and ownership");
                let model_request = || ModelRequest {
                    model: "probe".into(),
                    messages: vec![Message::user("review the leased head")],
                    schema: json!({"type":"object"}),
                    schema_name: "owned-dispatch-control".into(),
                    reasoning_effort: None,
                    max_tokens: 16,
                };

                assert!(root.complete(model_request()).await.is_ok());
                assert!(matches!(
                    lane.complete(model_request()).await,
                    Err(Error::ModelExecutionRefused {
                        refusal: ModelExecutionRefusal::CallLimit,
                        ..
                    })
                ));
                assert_eq!(mock.calls(), 1, "refusal occurs before the provider probe");
                assert_eq!(execution.snapshot().committed_calls, 1);

                original.id = crate::ports::review_job::ReviewJobId(format!(
                    "new-head-delivery-{scope_first}-{control_first}"
                ));
                original.delivery =
                    Some(format!("new-head-delivery-{scope_first}-{control_first}"));
                original.head_sha = Some("head-b".into());
                store.admit_batch(vec![original.clone()]).await.unwrap();
                assert!(matches!(
                    lane.complete(model_request()).await,
                    Err(Error::ReviewSuperseded)
                ));
                assert_eq!(mock.calls(), 1);
            }
        }
    }

    struct TimingLogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for TimingLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct SlowMockModel(crate::harness::MockModel);

    #[test]
    fn control_plane_completion_records_yield_and_completed_publication() {
        for (disposition, expected) in [
            (WorkerDisposition::Yielded, "superseded"),
            (
                WorkerDisposition::Terminal(TerminalOutcome::Superseded),
                "superseded",
            ),
            (
                WorkerDisposition::Terminal(TerminalOutcome::Published),
                "success",
            ),
        ] {
            let timing = crate::review_timing::ReviewTiming::new("owner/repo", 7, "Incremental");
            finish_control_timing(&timing, disposition);
            assert_eq!(timing.snapshot().outcome, expected);
        }
    }

    #[async_trait::async_trait]
    impl crate::ports::model::Model for SlowMockModel {
        async fn complete(
            &self,
            request: crate::ports::model::ModelRequest,
        ) -> Result<crate::ports::model::ModelResponse> {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            self.0.complete(request).await
        }
    }

    #[test]
    fn review_timing_classifies_deadline_errors_without_parsing_display_text() {
        let timeout = Error::Timeout {
            what: "review".into(),
            seconds: 1,
        };
        let model = Error::Model("provider timed out".into());
        assert_eq!(
            crate::review_timing::outcome_for_error(&timeout),
            "deadline"
        );
        assert_eq!(crate::review_timing::outcome_for_error(&model), "failure");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_server_deadline_records_the_cancelled_mock_review_lifecycle() {
        use crate::forge::types::{ChangedFile, FileStatus, PullRequest};
        use crate::forge::{MockForge, MockState, RepoId};
        use crate::harness::MockModel;

        let mut config: crate::config::Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.review.lanes = vec!["critique".into()];
        config.summary.enabled = false;
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                title: "review lifecycle fixture".into(),
                body: "A fixture with a real changed file.".into(),
                head_sha: "abc123".into(),
                ..PullRequest::default()
            },
        );
        state.files.insert(
            7,
            vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                patch: Some("@@ -1 +1,2 @@\n fn value() {}\n+fn next() {}\n".into()),
                additions: 1,
                ..ChangedFile::default()
            }],
        );
        let forge = MockForge::with_state(state);
        let repo = RepoId::parse("tinyhumansai/tinysweeper").unwrap();
        let model = SlowMockModel(MockModel::always(serde_json::json!({
            "summary": "Reviewed.", "findings": [], "resolved": []
        })));
        let timing = crate::review_timing::ReviewTiming::new(repo.to_string(), 7, "Incremental");
        let future = crate::app::review::review_with_tree_observed(
            &forge,
            Arc::new(model),
            &config,
            &repo,
            7,
            None,
            None,
            None,
            None,
            Some(timing.clone()),
        );
        let error = checkout_then_review_before_deadline(
            &timing,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            &repo,
            7,
            || async {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                Ok::<(), Error>(())
            },
            |_| future,
        )
        .await
        .expect_err("the server deadline cancels the delayed model call");
        assert!(matches!(error, Error::Timeout { .. }));
        let record = timing.snapshot();
        assert_eq!(record.outcome, "deadline");
        assert_eq!(record.head_sha, "abc123");
        assert_eq!(record.files_total, 1);
        assert_eq!(record.files_answered, 0);
        assert_eq!(record.files_unanswered, 1);
        assert!(record.phases_ms.contains_key("context"));
        assert!(record.phases_ms["checkout"] > 0);
        assert!(record.phases_ms["lanes"] > 0);
        assert_eq!(record.calls_attempted, 1);
        assert_eq!(record.calls_errored, 1);

        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer_bytes = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || TimingLogWriter(writer_bytes.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || drop(timing));
        let line = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(line.contains("review timing"), "{line}");
        assert!(line.contains("outcome=deadline"), "{line}");
        assert_eq!(line.matches("review timing").count(), 1, "{line}");
        for phase in crate::review_timing::PHASES {
            assert!(line.contains(phase), "missing {phase}: {line}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mock_forge_review_lifecycle_measures_each_success_stage() {
        use crate::forge::types::{ChangedFile, FileStatus, PullRequest};
        use crate::forge::{MockForge, MockState, RepoId};
        use crate::harness::MockModel;

        struct DelayedModel(MockModel);
        #[async_trait::async_trait]
        impl crate::ports::model::Model for DelayedModel {
            async fn complete(
                &self,
                request: crate::ports::model::ModelRequest,
            ) -> Result<crate::ports::model::ModelResponse> {
                tokio::time::sleep(std::time::Duration::from_millis(3)).await;
                self.0.complete(request).await
            }
        }

        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                head_sha: "abc123".into(),
                ..PullRequest::default()
            },
        );
        state.files.insert(
            7,
            vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                patch: Some("@@ -1 +1,2 @@\n fn value() {}\n+fn next() {}\n".into()),
                additions: 1,
                ..ChangedFile::default()
            }],
        );
        let forge = MockForge::with_state(state);
        let repo = RepoId::parse("tinyhumansai/tinysweeper").unwrap();
        let mut config: crate::config::Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.review.lanes = vec!["critique".into()];
        config.summary.enabled = false;
        config.overview.enabled = false;
        let model = Arc::new(DelayedModel(MockModel::always(serde_json::json!({
            "summary": "Reviewed.", "findings": [], "resolved": []
        }))));
        let timing = crate::review_timing::ReviewTiming::new(repo.to_string(), 7, "Incremental");

        let permits = Arc::new(Semaphore::new(1));
        let mut held = Some(permits.clone().acquire_owned().await.unwrap());
        let permit = {
            let waiting = crate::review_timing::measure(
                Some(&timing),
                "permit",
                permits.clone().acquire_owned(),
            );
            tokio::pin!(waiting);
            tokio::select! {
                _permit = &mut waiting => panic!("held semaphore should keep the permit pending"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(3)) => {}
            }
            drop(held.take());
            waiting.await.unwrap()
        };
        drop(permit);
        let published = server_review_lifecycle(
            &timing,
            || async {
                tokio::time::sleep(std::time::Duration::from_millis(3)).await;
                let pr = forge.pull_request(&repo, 7).await?;
                timing.record(|record| record.head_sha = pr.head_sha);
                Ok(true)
            },
            || async {
                checkout_then_review_before_deadline(
                    &timing,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                    &repo,
                    7,
                    || async {
                        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
                        Ok::<(), Error>(())
                    },
                    |_| async {
                        crate::app::review::review_with_tree_observed(
                            &forge,
                            model,
                            &config,
                            &repo,
                            7,
                            None,
                            None,
                            None,
                            None,
                            Some(timing.clone()),
                        )
                        .await
                    },
                )
                .await
                .map(|proposal| (config, proposal))
            },
            |(_config, proposal)| async {
                crate::review_timing::measure(Some(&timing), "publish", async {
                    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
                    forge
                        .publish_check(&repo, status::in_progress("abc123"))
                        .await?;
                    Ok(Some(proposal))
                })
                .await
            },
        )
        .await
        .expect("review lifecycle succeeds")
        .expect("status check opens");
        assert_eq!(published.head_sha, "abc123");
        timing.finish("success");

        let record = timing.snapshot();
        assert_eq!(record.outcome, "success");
        assert_eq!(record.head_sha, "abc123");
        for phase in ["permit", "open_status", "checkout", "lanes", "publish"] {
            assert!(
                record.phases_ms[phase] > 0,
                "{phase}: {:?}",
                record.phases_ms
            );
        }
        assert!(
            record.phases_ms.values().sum::<u64>()
                <= record.started.unwrap().elapsed().as_millis() as u64,
            "phase durations overlap: {:?}",
            record.phases_ms
        );
        assert_eq!(record.files_total, 1);

        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer_bytes = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || TimingLogWriter(writer_bytes.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || drop(timing));
        let line = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(line.matches("review timing").count(), 1, "{line}");
        assert!(line.contains("outcome=success"), "{line}");
        for phase in crate::review_timing::PHASES {
            assert!(line.contains(phase), "missing {phase}: {line}");
        }
    }

    #[tokio::test]
    async fn an_expired_owner_deadline_wins_before_the_review_future_is_polled() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (_lost, lost_receiver) = watch::channel(false);
        let (_until_sender, until) =
            watch::channel(tokio::time::Instant::now() - std::time::Duration::from_millis(1));
        let paid_step = Arc::new(AtomicBool::new(false));
        let observed = paid_step.clone();
        tokio::select! {
            biased;
            _ = wait_for_owner_fence(Some(lost_receiver), Some(until)) => {}
            _ = async move {
                observed.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            } => panic!("expired review future must not be polled"),
        }
        assert!(!paid_step.load(Ordering::SeqCst));
    }

    #[test]
    fn a_slow_renewal_cannot_extend_the_local_deadline_from_its_response_time() {
        let response_time = tokio::time::Instant::now();
        let renewal_started = response_time - std::time::Duration::from_secs(40);
        assert!(
            conservative_owner_deadline(renewal_started) <= response_time,
            "a renewal response after the safety margin must not permit more model work"
        );
    }

    #[tokio::test]
    async fn ownership_expiring_during_mongo_check_blocks_publication_continuation() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let valid = Arc::new(AtomicBool::new(true));
        let delayed_valid = valid.clone();
        let publications = AtomicUsize::new(0);
        let result = check_owner_after_await(|| valid.load(Ordering::SeqCst), async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            delayed_valid.store(false, Ordering::SeqCst);
            Ok(true)
        })
        .await;
        if result.is_ok() {
            publications.fetch_add(1, Ordering::SeqCst);
        }
        assert!(matches!(result, Err(Error::ReviewOwnershipLost)));
        assert_eq!(publications.load(Ordering::SeqCst), 0);

        let positive = check_owner_after_await(|| true, async { Ok(true) }).await;
        assert!(positive.is_ok(), "a live owner passes the post-await fence");
        if positive.is_ok() {
            publications.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(publications.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn status_open_shutdown_defers_job_for_replacement_owner() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let request = queued_test_review(71);
        assert_eq!(
            store.admit_batch(vec![request]).await.unwrap(),
            Admission::Admitted
        );
        seed_previously_published_queue_checks(&store, unix_time_ms()).await;
        let first = store
            .claim_next("first-owner", unix_time_ms())
            .await
            .unwrap()
            .expect("first process owns accepted review");
        let continued = AtomicUsize::new(0);
        let shutdown_result = continue_after_open_status(false, || async {
            continued.fetch_add(1, Ordering::SeqCst);
            Ok(InnerReview::Complete(None))
        })
        .await
        .expect("shutdown after status-open resolves as a typed result");
        assert_eq!(continued.load(Ordering::SeqCst), 0);
        assert!(matches!(shutdown_result, InnerReview::Shutdown));
        let disposition = shutdown_disposition(&shutdown_result, unix_time_ms())
            .expect("the production handler defers shutdown results");
        assert!(
            record_owned_disposition(&store, &first, disposition, unix_time_ms())
                .await
                .unwrap()
                .expect("shutdown disposition must be persisted")
        );

        let replacement = store
            .claim_next("replacement-owner", unix_time_ms() + 1)
            .await
            .unwrap()
            .expect("shutdown-interrupted work remains recoverable");
        confirm_test_head(&store, &replacement, unix_time_ms()).await;
        assert!(matches!(
            store
                .begin_paid(&replacement, unix_time_ms())
                .await
                .unwrap(),
            crate::ports::review_job::PaidStart::Started
        ));
        assert!(
            store
                .finish(
                    &replacement,
                    TerminalOutcome::Published,
                    unix_time_ms(),
                    unix_time_ms()
                )
                .await
                .unwrap()
        );
        assert_eq!(store.unfinished(), 0);

        let normal_store = crate::ports::review_job_mock::MockReviewJobStore::default();
        assert_eq!(
            normal_store
                .admit_batch(vec![queued_test_review(72)])
                .await
                .unwrap(),
            Admission::Admitted
        );
        seed_previously_published_queue_checks(&normal_store, unix_time_ms()).await;
        let normal_owner = normal_store
            .claim_next("normal-owner", unix_time_ms())
            .await
            .unwrap()
            .expect("positive control owns work");
        let normal_result = continue_after_open_status(true, || async {
            continued.fetch_add(1, Ordering::SeqCst);
            Ok(InnerReview::Complete(None))
        })
        .await
        .expect("live status continues review");
        assert_eq!(continued.load(Ordering::SeqCst), 1);
        assert!(shutdown_disposition(&normal_result, unix_time_ms()).is_none());
        assert!(
            record_owned_disposition(
                &normal_store,
                &normal_owner,
                WorkerDisposition::Terminal(TerminalOutcome::Declined),
                unix_time_ms()
            )
            .await
            .unwrap()
            .expect("ordinary review uses normal terminal path")
        );
        assert_eq!(normal_store.unfinished(), 0);
    }

    #[tokio::test]
    async fn a_superseding_delivery_stops_the_old_model_dispatch_before_provider_call() {
        use crate::harness::MockModel;
        use crate::ports::model::{Message, Model, ModelRequest};
        use crate::ports::model_execution::{ModelExecutionControl, ModelExecutionRefusal};
        use serde_json::json;

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let now = unix_time_ms();
        let mut first = queued_test_review(73);
        first.repo = "owner/repo".into();
        first.number = 7;
        first.head_sha = Some("head-a".into());
        store.admit_batch(vec![first.clone()]).await.unwrap();
        seed_previously_published_queue_checks(&*store, now).await;
        let owned = store.claim_next("old-worker", now).await.unwrap().unwrap();
        assert!(store.confirm_head(&owned, "head-a", now + 1).await.unwrap());

        let model = MockModel::silent();
        let started_at = tokio::time::Instant::now();
        let execution = Arc::new(crate::app::review_execution::ReviewExecution::new(
            crate::app::review_execution::ReviewExecutionOptions {
                started_at,
                hard_deadline: started_at + std::time::Duration::from_secs(60),
                mode: crate::lanes::large_pr::ReviewMode::Normal,
                max_calls: None,
                soft_deadline_percent: 80,
                drain_seconds: 30,
            },
        ));
        let control: Arc<dyn ModelExecutionControl> = execution.clone();
        let guarded = OwnedModel {
            inner: model
                .with_execution_control(control)
                .expect("the model receives this run's execution scope"),
            execution: execution.clone(),
            jobs: store.clone(),
            owned: owned.clone(),
            owner_lost: None,
            owner_until: None,
        };
        let mut next = first;
        next.id = crate::ports::review_job::ReviewJobId("push-b".into());
        next.delivery = Some("push-b".into());
        next.head_sha = Some("head-b".into());
        store.admit_batch(vec![next]).await.unwrap();

        let result = guarded
            .complete(ModelRequest {
                model: "test".into(),
                messages: vec![Message::user("review this head")],
                schema: json!({"type": "object"}),
                schema_name: "review".into(),
                reasoning_effort: None,
                max_tokens: 16,
            })
            .await;
        assert!(matches!(result, Err(Error::ReviewSuperseded)));
        assert!(
            model.requests().is_empty(),
            "the old provider request never starts"
        );
        assert!(
            store.owns(&owned, 102).await.unwrap(),
            "the old owner keeps its slot until yield acknowledgement"
        );
        assert!(!store.should_continue(&owned, 102).await.unwrap());
        assert_eq!(
            execution.acquire().await.err(),
            Some(ModelExecutionRefusal::Cancelled),
            "a typed supersession fence cancels the shared execution scope"
        );
    }

    #[tokio::test]
    async fn an_uncancelled_durable_supersession_precedes_a_terminal_lane_error() {
        use crate::forge::MockForge;

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let now = unix_time_ms();
        let mut first = queued_test_review(74);
        first.repo = "owner/repo".into();
        first.number = 7;
        first.head_sha = Some("head-a".into());
        store.admit_batch(vec![first.clone()]).await.unwrap();
        seed_previously_published_queue_checks(&*store, now).await;
        let owned = store.claim_next("old-worker", now).await.unwrap().unwrap();
        assert!(store.confirm_head(&owned, "head-a", now + 1).await.unwrap());
        assert_eq!(
            store.begin_paid(&owned, now + 2).await.unwrap(),
            PaidStart::Started
        );

        let mut next = first;
        next.id = crate::ports::review_job::ReviewJobId("push-b-budget-race".into());
        next.delivery = Some("push-b-budget-race".into());
        next.head_sha = Some("head-b".into());
        next.accepted_at_ms = now + 3;
        store.admit_batch(vec![next]).await.unwrap();

        // This models a sibling lane's terminal Budget result arriving before
        // the old worker's ownership watcher has canceled its shared scope.
        let budget_failure = Error::Budget {
            spent: 1.0,
            limit: 1.0,
            usage: None,
        };
        let execution = test_execution_scope();
        assert!(!execution.is_cancelled());
        let result = ownership_fence_disposition(
            &budget_failure,
            &execution,
            store.as_ref(),
            Some(&owned),
            || true,
        )
        .await
        .unwrap();

        assert!(matches!(result, Some(WorkerDisposition::Yielded)));
        assert!(execution.is_cancelled());
        assert!(store.owns(&owned, now + 4).await.unwrap());
        assert!(!store.should_continue(&owned, now + 4).await.unwrap());

        let forge = MockForge::new();
        let open = ReviewStatus {
            repo: RepoId::parse("owner/repo").unwrap(),
            check_id: 417,
            head_sha: "head-a".into(),
            installation: 1,
            stage: crate::ports::review_job::ReviewCheckStage::InProgress,
            owned: Some(owned),
            hub_comment_id: Some(418),
            prior_hub_body: Some("previous review body".into()),
        };
        let callback_forge = forge.clone();
        let (concluded, hub_updated) =
            conclude_status_with_writer(&open, Conclusion::NotReviewed, &forge, || async move {
                let writes = callback_forge.writes();
                assert!(writes.iter().any(|write| matches!(
                    write,
                    crate::forge::Write::CheckUpdate { check_id: 417, .. }
                )));
                assert!(writes.iter().all(|write| !matches!(
                    write,
                    crate::forge::Write::CommentUpdate {
                        comment_id: 418,
                        ..
                    }
                )));
            })
            .await;
        concluded.expect("the stale head receives a neutral terminal check");
        hub_updated
            .expect("the open review hub is also retired")
            .expect("the review hub update succeeds");
        let writes = forge.writes();
        assert!(writes.iter().any(|write| matches!(
            write,
            crate::forge::Write::CheckUpdate { check_id: 417, check }
                if check.head_sha == "head-a"
                    && check.conclusion == Some(crate::forge::CheckConclusion::Neutral)
                    && check.title == "Not reviewed"
        )));
        assert!(writes.iter().any(|write| matches!(
            write,
            crate::forge::Write::CommentUpdate { comment_id: 418, body }
                if body.contains("# Tiny Sweeper review") && body.contains("head-a")
        )));
    }

    #[tokio::test]
    async fn a_transient_review_error_does_not_cancel_the_run_execution_scope() {
        use crate::ports::model_execution::ModelExecutionControl;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let execution = test_execution_scope();
        let result: Result<()> = select_review_control(
            async { Err(Error::Forge("temporary read failure".into())) },
            &store,
            None,
            None,
            None,
            execution.clone(),
        )
        .await;

        assert!(matches!(result, Err(Error::Forge(_))));
        assert!(
            execution.acquire().await.is_ok(),
            "retryable errors keep the shared scope available for the next attempt"
        );
    }

    #[tokio::test]
    async fn a_model_dispatch_rechecks_local_expiry_after_a_slow_store_reply() {
        use crate::harness::MockModel;
        use crate::ports::model::{Message, Model, ModelRequest};
        use serde_json::json;

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let mut request = queued_test_review(75);
        request.head_sha = Some("head-a".into());
        store.admit_batch(vec![request]).await.unwrap();
        let now = unix_time_ms();
        seed_previously_published_queue_checks(&*store, now).await;
        let owned = store
            .claim_next("slow-store-worker", now)
            .await
            .unwrap()
            .unwrap();
        assert!(store.confirm_head(&owned, "head-a", now).await.unwrap());
        store.delay_should_continue(std::time::Duration::from_millis(50));

        let model = MockModel::silent();
        let local_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(10);
        let (_, owner_until) = watch::channel(local_deadline);
        let guarded = OwnedModel {
            inner: Arc::new(model.clone()),
            execution: test_execution_scope(),
            jobs: store,
            owned,
            owner_lost: None,
            owner_until: Some(owner_until),
        };
        let result = guarded
            .complete(ModelRequest {
                model: "test".into(),
                messages: vec![Message::user("must not dispatch after local lease expiry")],
                schema: json!({"type": "object"}),
                schema_name: "review".into(),
                reasoning_effort: None,
                max_tokens: 16,
            })
            .await;

        assert!(matches!(result, Err(Error::ReviewOwnershipLost)));
        assert!(model.requests().is_empty());
    }

    #[tokio::test]
    async fn a_queued_push_burst_runs_and_publishes_only_the_confirmed_last_head() {
        use crate::forge::MockForge;
        use crate::harness::MockModel;
        use crate::ports::forge::ForgeWrite;
        use crate::ports::model::{Message, Model, ModelRequest};
        use serde_json::json;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let now = unix_time_ms();
        let requests = ["head-a", "head-b", "head-c"]
            .into_iter()
            .enumerate()
            .map(|(index, head)| {
                let mut request = queued_test_review(81 + index as u64);
                request.repo = "owner/repo".into();
                request.number = 7;
                request.head_sha = Some(head.into());
                request.accepted_at_ms = now + index as i64;
                request
            })
            .collect::<Vec<_>>();
        assert_eq!(
            store.admit_batch(requests).await.unwrap(),
            Admission::Admitted
        );
        seed_previously_published_queue_checks(&store, now + 4).await;

        let owned = store
            .claim_next("burst-worker", now + 4)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owned.accepted_ids.len(), 3);
        assert!(store.confirm_head(&owned, "head-c", now + 5).await.unwrap());
        assert_eq!(
            store.begin_paid(&owned, now + 6).await.unwrap(),
            PaidStart::Started
        );

        let model = MockModel::silent();
        let forge = MockForge::new();
        let repo = RepoId::parse("owner/repo").unwrap();
        let result =
            run_if_confirmed_head_needs_review(&store, Some(&owned), Mode::Incremental, || async {
                model
                    .complete(ModelRequest {
                        model: "test".into(),
                        messages: vec![Message::user("review confirmed head-c")],
                        schema: json!({"type": "object"}),
                        schema_name: "review".into(),
                        reasoning_effort: None,
                        max_tokens: 16,
                    })
                    .await?;
                forge
                    .publish_check(
                        &repo,
                        crate::forge::CheckRun {
                            name: "tinysweeper/review".into(),
                            head_sha: "head-c".into(),
                            conclusion: Some(crate::forge::CheckConclusion::Success),
                            title: "Reviewed".into(),
                            summary: "Only the confirmed coalesced head ran".into(),
                            status: None,
                            external_id: None,
                            images: vec![],
                        },
                    )
                    .await?;
                Ok(InnerReview::Complete(Some((
                    1,
                    false,
                    status::ReviewCoverage::default(),
                ))))
            })
            .await
            .unwrap();

        assert!(matches!(result, InnerReview::Complete(Some(_))));
        assert_eq!(model.requests().len(), 1);
        assert!(model.requests()[0].messages[0].content.contains("head-c"));
        let writes = forge.writes();
        assert_eq!(writes.len(), 1);
        assert!(matches!(
            &writes[0],
            crate::forge::Write::Check(check) if check.head_sha == "head-c"
        ));
        assert!(
            store
                .finish(&owned, TerminalOutcome::Published, now + 7, now + 7)
                .await
                .unwrap()
        );
        assert_eq!(store.snapshot_for_test().terminal.len(), 3);
    }

    #[tokio::test]
    async fn an_inflight_a_model_call_yields_then_only_b_publishes() {
        use crate::forge::{MockForge, RepoId};
        use crate::ports::model::{Message, Model, ModelRequest, ModelResponse, Usage};
        use crate::ports::model_execution::ModelExecutionControl;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::Notify;

        struct CallTracker {
            requests: Mutex<Vec<String>>,
            a_started: Notify,
            a_dropped: Arc<AtomicBool>,
        }
        struct ACallDrop(Arc<AtomicBool>);
        impl Drop for ACallDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        #[async_trait::async_trait]
        impl Model for CallTracker {
            async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
                let content = request
                    .messages
                    .iter()
                    .map(|message| message.content.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                self.requests
                    .lock()
                    .expect("requests")
                    .push(content.clone());
                if content.contains("head-a") {
                    let _drop = ACallDrop(self.a_dropped.clone());
                    self.a_started.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(ModelResponse {
                    value: serde_json::Value::Null,
                    model: "test".into(),
                    usage: Usage::default(),
                })
            }
        }

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let now = unix_time_ms();
        let mut request_a = queued_test_review(83);
        request_a.repo = "owner/repo".into();
        request_a.number = 7;
        request_a.head_sha = Some("head-a".into());
        store.admit_batch(vec![request_a.clone()]).await.unwrap();
        let forge = MockForge::new();
        publish_test_queued_statuses_through_forge(&*store, forge.clone(), now).await;
        let owned_a = store.claim_next("worker-a", now).await.unwrap().unwrap();
        assert!(store.confirm_head(&owned_a, "head-a", now).await.unwrap());
        let owned_a_for_status = owned_a.clone();
        assert_eq!(
            store.begin_paid(&owned_a, now + 1).await.unwrap(),
            PaidStart::Started
        );

        let a_dropped = Arc::new(AtomicBool::new(false));
        let tracker = Arc::new(CallTracker {
            requests: Mutex::new(Vec::new()),
            a_started: Notify::new(),
            a_dropped: a_dropped.clone(),
        });
        let execution = test_execution_scope();
        let guarded_a = OwnedModel {
            inner: tracker.clone(),
            execution: execution.clone(),
            jobs: store.clone(),
            owned: owned_a.clone(),
            owner_lost: None,
            owner_until: None,
        };
        let permits = Arc::new(Semaphore::new(
            crate::ports::review_job::MAX_RUNNING_REVIEWS,
        ));
        let mut held = Vec::new();
        for _ in 0..crate::ports::review_job::MAX_RUNNING_REVIEWS {
            held.push(permits.clone().acquire_owned().await.unwrap());
        }
        let worker_permit = held.pop().unwrap();
        let jobs: Arc<dyn ReviewJobStore> = store.clone();
        let a_started = tracker.a_started.notified();
        let request_for_call = || ModelRequest {
            model: "test".into(),
            messages: vec![Message::user("review head-a")],
            schema: serde_json::json!({"type": "object"}),
            schema_name: "review".into(),
            reasoning_effort: None,
            max_tokens: 16,
        };
        let control_execution = execution.clone();
        let worker = tokio::spawn(async move {
            let result = select_review_control(
                guarded_a.complete(request_for_call()),
                jobs.as_ref(),
                Some(&owned_a),
                None,
                None,
                control_execution,
            )
            .await;
            if matches!(result, Err(Error::ReviewSuperseded)) {
                assert!(jobs.defer_yield(&owned_a, unix_time_ms()).await.unwrap());
            }
            drop(worker_permit);
            result
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), a_started)
            .await
            .expect("A reaches the model provider before B arrives");

        let mut request_b = request_a;
        request_b.id = crate::ports::review_job::ReviewJobId("push-b".into());
        request_b.delivery = Some("push-b".into());
        request_b.head_sha = Some("head-b".into());
        request_b.accepted_at_ms = now + 2;
        store.admit_batch(vec![request_b]).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), worker)
                .await
                .expect("B cancels the in-flight A provider future")
                .unwrap(),
            Err(Error::ReviewSuperseded)
        ));
        assert!(a_dropped.load(Ordering::SeqCst));
        publish_test_queued_statuses_through_forge(&*store, forge.clone(), now + 4).await;
        assert!(execution.is_cancelled());
        let replacement_execution = test_execution_scope();
        assert!(!Arc::ptr_eq(&execution, &replacement_execution));
        assert!(replacement_execution.acquire().await.is_ok());
        assert!(
            permits.try_acquire().is_ok(),
            "A releases its permit on yield"
        );
        let queued_a = forge
            .writes()
            .into_iter()
            .find_map(|write| match write {
                crate::forge::Write::Check(check)
                    if check.head_sha == "head-a"
                        && check.status == Some(crate::forge::types::CheckRunStatus::Queued) =>
                {
                    Some(check)
                }
                _ => None,
            })
            .expect("A's queued check is written before review work");
        let queued_a_id = crate::ports::forge::ForgeRead::check_run_by_external_id(
            &forge,
            &RepoId::parse("owner/repo").unwrap(),
            "head-a",
            queued_a.external_id.as_deref().expect("queued A identity"),
        )
        .await
        .unwrap()
        .expect("A's queued check has a remote ID");
        let old_status = ReviewStatus {
            repo: RepoId::parse("owner/repo").unwrap(),
            check_id: queued_a_id,
            head_sha: "head-a".into(),
            installation: 1,
            stage: crate::ports::review_job::ReviewCheckStage::InProgress,
            owned: Some(owned_a_for_status),
            hub_comment_id: None,
            prior_hub_body: None,
        };
        let status_store = store.clone();
        let status_owner = old_status.owned.clone().expect("A's durable owner");
        let (old_check, old_hub) = conclude_status_with_writer(
            &old_status,
            Conclusion::NotReviewed,
            &forge,
            || async move {
                let _ = status_store
                    .set_check_stage(
                        &status_owner,
                        "head-a",
                        queued_a_id,
                        crate::ports::review_job::ReviewCheckStage::Completed,
                        unix_time_ms(),
                    )
                    .await;
            },
        )
        .await;
        old_check.expect("A's superseded check concludes neutrally");
        assert!(old_hub.is_none());

        let owned_b = store
            .claim_next("worker-b", unix_time_ms())
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .confirm_head(&owned_b, "head-b", unix_time_ms())
                .await
                .unwrap()
        );
        assert_eq!(
            store.begin_paid(&owned_b, unix_time_ms()).await.unwrap(),
            PaidStart::Started
        );
        let repo = RepoId::parse("owner/repo").unwrap();
        let queued_b = forge
            .writes()
            .into_iter()
            .find_map(|write| match write {
                crate::forge::Write::Check(check)
                    if check.head_sha == "head-b"
                        && check.status == Some(crate::forge::types::CheckRunStatus::Queued) =>
                {
                    Some(check)
                }
                _ => None,
            })
            .expect("B's queued check is written before review work");
        let queued_b_id = crate::ports::forge::ForgeRead::check_run_by_external_id(
            &forge,
            &repo,
            "head-b",
            queued_b.external_id.as_deref().expect("queued B identity"),
        )
        .await
        .unwrap()
        .expect("B's queued check has a remote ID");
        let result = run_if_confirmed_head_needs_review(
            store.as_ref(),
            Some(&owned_b),
            Mode::Incremental,
            || async {
                tracker
                    .complete(ModelRequest {
                        model: "test".into(),
                        messages: vec![Message::user("review head-b")],
                        schema: serde_json::json!({"type": "object"}),
                        schema_name: "review".into(),
                        reasoning_effort: None,
                        max_tokens: 16,
                    })
                    .await?;
                let status_store = store.clone();
                let status_owner = owned_b.clone();
                let status = ReviewStatus {
                    repo: repo.clone(),
                    check_id: queued_b_id,
                    head_sha: "head-b".into(),
                    installation: 1,
                    stage: crate::ports::review_job::ReviewCheckStage::InProgress,
                    owned: Some(status_owner.clone()),
                    hub_comment_id: None,
                    prior_hub_body: None,
                };
                let (check, hub) = conclude_status_with_writer(
                    &status,
                    Conclusion::Reviewed {
                        findings: 1,
                        changes_requested: false,
                        coverage: status::ReviewCoverage::default(),
                    },
                    &forge,
                    || async move {
                        let _ = status_store
                            .set_check_stage(
                                &status_owner,
                                "head-b",
                                queued_b_id,
                                crate::ports::review_job::ReviewCheckStage::Completed,
                                unix_time_ms(),
                            )
                            .await;
                    },
                )
                .await;
                check?;
                assert!(hub.is_none());
                Ok(InnerReview::Complete(Some((
                    1,
                    false,
                    status::ReviewCoverage::default(),
                ))))
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, InnerReview::Complete(Some(_))));
        let requests = tracker.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("head-a"));
        assert!(requests[1].contains("head-b"));
        drop(requests);
        let writes = forge.writes();
        assert_eq!(
            writes
                .iter()
                .filter(|write| matches!(
                    write,
                    crate::forge::Write::CheckUpdate { check_id, check }
                        if *check_id == queued_b_id
                            && check.head_sha == "head-b"
                            && check.conclusion == Some(crate::forge::CheckConclusion::Success)
                ))
                .count(),
            1,
            "B's completed review updates its queued check once: {writes:#?}"
        );
        assert!(
            writes.iter().any(|write| matches!(
                write,
                crate::forge::Write::CheckUpdate { check, .. }
                    if check.head_sha == "head-a"
                        && check.conclusion == Some(crate::forge::CheckConclusion::Neutral)
                        && check.title == "Not reviewed"
            )),
            "the explicit ownership-fence writer emits a neutral A status"
        );
        assert!(
            writes.iter().all(|write| match write {
                crate::forge::Write::Check(check) if check.head_sha == "head-a" => {
                    check.conclusion.is_none()
                        && check.status == Some(crate::forge::types::CheckRunStatus::Queued)
                }
                crate::forge::Write::CheckUpdate { check, .. } if check.head_sha == "head-a" => {
                    check.conclusion == Some(crate::forge::CheckConclusion::Neutral)
                        && matches!(check.title.as_str(), "Review incomplete" | "Not reviewed")
                }
                _ => true,
            }),
            "A may receive its queued check and only neutral retirement updates: {writes:#?}"
        );
    }

    #[tokio::test]
    async fn a_delayed_old_delivery_does_not_rerun_or_republish_a_completed_live_head() {
        use crate::forge::{MockForge, RepoId};
        use crate::harness::MockModel;
        use crate::ports::forge::ForgeWrite;
        use crate::ports::model::{Message, Model, ModelRequest};
        use serde_json::json;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let now = unix_time_ms();
        let mut request = queued_test_review(7);
        request.head_sha = Some("head-a-delayed".into());
        store.admit_batch(vec![request.clone()]).await.unwrap();
        seed_previously_published_queue_checks(&store, now).await;
        let newer = store
            .claim_next("newer-worker", now)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .confirm_head(&newer, "head-b-live", now + 1)
                .await
                .unwrap()
        );
        assert_eq!(
            store.begin_paid(&newer, now + 2).await.unwrap(),
            PaidStart::Started
        );
        let forge = MockForge::new();
        let model = MockModel::silent();
        let repo_id = RepoId::parse("owner/repo").unwrap();
        let published =
            run_if_confirmed_head_needs_review(&store, Some(&newer), Mode::Incremental, || async {
                model
                    .complete(ModelRequest {
                        model: "test".into(),
                        messages: vec![Message::user("review the live head")],
                        schema: json!({"type": "object"}),
                        schema_name: "review".into(),
                        reasoning_effort: None,
                        max_tokens: 16,
                    })
                    .await?;
                forge
                    .publish_check(
                        &repo_id,
                        crate::forge::CheckRun {
                            name: "tinysweeper/review".into(),
                            head_sha: "head-b-live".into(),
                            conclusion: Some(crate::forge::CheckConclusion::Success),
                            title: "Reviewed".into(),
                            summary: "Reviewed".into(),
                            status: None,
                            external_id: None,
                            images: vec![],
                        },
                    )
                    .await?;
                Ok(InnerReview::Complete(Some((
                    1,
                    false,
                    status::ReviewCoverage::default(),
                ))))
            })
            .await
            .unwrap();
        assert!(matches!(published, InnerReview::Complete(Some(_))));

        request.id = crate::ports::review_job::ReviewJobId("delayed-a".into());
        request.delivery = Some("delayed-a".into());
        request.accepted_at_ms = now + 4;
        store.admit_batch(vec![request.clone()]).await.unwrap();
        seed_previously_published_queue_checks(&store, now + 4).await;
        assert!(
            record_owned_disposition(
                &store,
                &newer,
                WorkerDisposition::Terminal(TerminalOutcome::Published),
                now + 5,
            )
            .await
            .expect("owned terminal disposition")
            .expect("publication records terminal head while yielding")
        );
        assert_eq!(
            store.snapshot_for_test().incremental_heads[0].terminal_outcome,
            Some(TerminalOutcome::Published)
        );
        let delayed = store
            .claim_next("delayed-worker", now + 6)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .confirm_head(&delayed, "head-b-live", now + 7)
                .await
                .unwrap()
        );
        assert_eq!(
            store.snapshot_for_test().incremental_heads[0].terminal_outcome,
            Some(TerminalOutcome::Published),
            "publication remains terminal through the delayed delivery"
        );

        let delayed_result = run_if_confirmed_head_needs_review(
            &store,
            Some(&delayed),
            Mode::Incremental,
            || async {
                model
                    .complete(ModelRequest {
                        model: "test".into(),
                        messages: vec![Message::user("review the live head again")],
                        schema: json!({"type": "object"}),
                        schema_name: "review".into(),
                        reasoning_effort: None,
                        max_tokens: 16,
                    })
                    .await?;
                forge
                    .publish_check(
                        &repo_id,
                        crate::forge::CheckRun {
                            name: "tinysweeper/review".into(),
                            head_sha: "head-b-live".into(),
                            conclusion: Some(crate::forge::CheckConclusion::Success),
                            title: "Reviewed again".into(),
                            summary: "duplicate".into(),
                            status: None,
                            external_id: None,
                            images: vec![],
                        },
                    )
                    .await?;
                Ok(InnerReview::Complete(None))
            },
        )
        .await
        .unwrap();
        assert!(matches!(delayed_result, InnerReview::Complete(None)));
        assert!(
            model.requests().len() == 1,
            "published B is not sent to the model again"
        );
        assert!(
            forge.writes().len() == 1,
            "the delayed delivery publishes no duplicate check"
        );
        let mut latest_hint = request.clone();
        latest_hint.id = crate::ports::review_job::ReviewJobId("push-c".into());
        latest_hint.delivery = Some("push-c".into());
        latest_hint.head_sha = Some("head-c-hint".into());
        latest_hint.accepted_at_ms = now + 8;
        store.admit_batch(vec![latest_hint]).await.unwrap();
        assert!(
            store
                .finish(&delayed, TerminalOutcome::Declined, now + 9, now + 9)
                .await
                .unwrap()
        );
        let state = store.snapshot_for_test();
        assert_eq!(
            state.incremental_heads[0].terminal_outcome,
            Some(TerminalOutcome::Published),
            "a yielded stale completion cannot replace a known publication"
        );
        assert_eq!(state.unfinished.len(), 1);
        assert_eq!(state.unfinished[0].accepted_ids.len(), 3);
    }

    #[tokio::test]
    async fn an_applied_review_is_classified_published_and_suppresses_same_head_redelivery() {
        use crate::forge::types::{ChangedFile, FileStatus, PullRequest};
        use crate::forge::{MockForge, MockState, RepoId};
        use crate::harness::MockModel;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let now_ms = unix_time_ms();
        let mut request = queued_test_review(7);
        request.repo = "owner/repo".into();
        request.head_sha = Some("published-head".into());
        store.admit_batch(vec![request.clone()]).await.unwrap();

        let forge = MockForge::with_state({
            let mut state = MockState::default();
            state.pull_requests.insert(
                7,
                PullRequest {
                    number: 7,
                    head_sha: "published-head".into(),
                    author: "contributor".into(),
                    ..PullRequest::default()
                },
            );
            state.files.insert(
                7,
                vec![
                    ChangedFile {
                        path: "src/lib.rs".into(),
                        status: FileStatus::Modified,
                        patch: Some("@@ -1 +1,2 @@\n fn value() {}\n+fn next() {}\n".into()),
                        additions: 1,
                        ..ChangedFile::default()
                    },
                    ChangedFile {
                        path: "src/other.rs".into(),
                        status: FileStatus::Modified,
                        patch: Some("@@ -1 +1,2 @@\n fn other() {}\n+fn another() {}\n".into()),
                        additions: 1,
                        ..ChangedFile::default()
                    },
                ],
            );
            state
        });
        publish_test_queued_statuses_through_forge(&store, forge.clone(), now_ms).await;
        let owned = store
            .claim_next("worker-publish", now_ms + 1)
            .await
            .unwrap()
            .expect("the admitted head is claimed by the worker");
        assert!(
            store
                .confirm_head(&owned, "published-head", now_ms + 2)
                .await
                .unwrap()
        );

        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.review.lanes = vec!["critique".into()];
        config.review.large_pr.files = 1;
        config.review.large_pr.groups = 1;
        config.review.large_pr.max_groups = 1;
        config.grouping.enabled = false;
        config.summary.enabled = false;
        config.overview.enabled = false;
        config.automerge.enabled = false;
        let model = Arc::new(MockModel::always(json!({
            "summary": "Reviewed.", "findings": [], "resolved": []
        })));
        let repo = RepoId::parse("owner/repo").unwrap();

        let completed = review_with_paid_budget_and_status(
            &store,
            Some(&owned),
            now_ms + 3,
            || async { Ok(()) },
            || async { Ok(true) },
            || async {
                let proposal = crate::app::review::review_with_tree(
                    &forge,
                    model.clone(),
                    &config,
                    &repo,
                    7,
                    None,
                    None,
                    None,
                    None,
                )
                .await?;
                assert!(
                    !proposal.complete(),
                    "large-review selection leaves a named gap"
                );
                assert!(proposal.partial_review.is_some());
                assert_eq!(
                    proposal
                        .partial_review
                        .as_ref()
                        .unwrap()
                        .selected_groups
                        .len(),
                    1
                );
                assert_eq!(proposal.unreviewed.len(), 1);
                let applied =
                    crate::app::apply::apply(&forge, &forge, &config, &proposal, None).await?;
                let findings = published_review_result(
                    applied,
                    proposal.findings().count(),
                    crate::app::apply::requests_changes(&config, &proposal),
                    status::aggregate_coverage(&proposal),
                );
                Ok(InnerReview::Complete(findings))
            },
        )
        .await
        .expect("the real review and apply complete within paid work");
        assert!(matches!(completed, InnerReview::Complete(Some(_))));
        assert!(
            forge.writes().iter().any(|write| matches!(
                write,
                crate::forge::Write::Review { event, .. }
                    if *event == crate::forge::types::ReviewEvent::Approve
                        || *event == crate::forge::types::ReviewEvent::Comment
            )),
            "the real apply path writes its review: {:#?}",
            forge.writes()
        );
        let paid_model_calls = model.requests().len();
        assert!(paid_model_calls > 0, "the real review invoked its model");

        let InnerReview::Complete(findings) = completed else {
            panic!("the app completion result was already checked above");
        };
        let (conclusion, disposition) = completed_review_disposition(findings);
        assert!(matches!(conclusion, Conclusion::Reviewed { .. }));
        assert!(matches!(
            disposition,
            WorkerDisposition::Terminal(TerminalOutcome::Published)
        ));
        assert!(
            record_owned_disposition(&store, &owned, disposition, now_ms + 4)
                .await
                .expect("owned worker terminal update")
                .expect("publication is recorded durably")
        );
        let state = store.snapshot_for_test();
        assert_eq!(state.unfinished.len(), 0);
        assert_eq!(
            state.incremental_heads[0].terminal_outcome,
            Some(TerminalOutcome::Published)
        );
        assert_eq!(state.incremental_heads[0].paid_attempts, 1);

        request.id = crate::ports::review_job::ReviewJobId("redelivered-head".into());
        request.delivery = Some("redelivered-head".into());
        request.accepted_at_ms = now_ms + 5;
        store.admit_batch(vec![request]).await.unwrap();
        seed_previously_published_queue_checks(&store, now_ms + 5).await;
        let redelivery = store
            .claim_next("worker-redelivery", now_ms + 6)
            .await
            .unwrap()
            .expect("the delayed delivery is independently claimable");
        assert!(
            store
                .confirm_head(&redelivery, "published-head", now_ms + 7)
                .await
                .unwrap()
        );
        let before_redelivery_writes = forge.writes().len();
        let skipped = run_if_confirmed_head_needs_review(
            &store,
            Some(&redelivery),
            Mode::Incremental,
            || async {
                model
                    .complete(ModelRequest {
                        model: "test".into(),
                        messages: vec![crate::ports::model::Message::user("duplicate review")],
                        schema: json!({"type": "object"}),
                        schema_name: "duplicate-review".into(),
                        reasoning_effort: None,
                        max_tokens: 16,
                    })
                    .await?;
                Ok(InnerReview::Complete(None))
            },
        )
        .await
        .expect("terminal head is a normal duplicate");
        assert!(matches!(skipped, InnerReview::Complete(None)));
        assert_eq!(
            model.requests().len(),
            paid_model_calls,
            "redelivery makes no additional paid model call"
        );
        assert_eq!(
            forge.writes().len(),
            before_redelivery_writes,
            "redelivery makes no apply writes"
        );
        assert_eq!(
            store.snapshot_for_test().incremental_heads[0].paid_attempts,
            1,
            "redelivery does not start another paid attempt"
        );
        assert!(
            record_owned_disposition(
                &store,
                &redelivery,
                WorkerDisposition::Terminal(TerminalOutcome::Declined),
                now_ms + 8,
            )
            .await
            .expect("duplicate terminal disposition")
            .expect("duplicate completion is durably released")
        );
        assert_eq!(
            store.snapshot_for_test().incremental_heads[0].terminal_outcome,
            Some(TerminalOutcome::Published),
            "a duplicate delivery cannot downgrade the published head"
        );
    }

    #[tokio::test]
    async fn an_apply_check_failure_keeps_the_last_good_hub_report() {
        use crate::forge::types::{ChangedFile, FileStatus, PullRequest};
        use crate::forge::{MockForge, MockState, RepoId};
        use crate::harness::MockModel;
        use crate::ports::forge::{ForgeRead, ForgeWrite};
        use std::sync::atomic::{AtomicBool, Ordering};

        let repo = RepoId::parse("owner/repo").unwrap();
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                head_sha: "previous-good-head".into(),
                title: "LAST GOOD TITLE".into(),
                ..PullRequest::default()
            },
        );
        state.files.insert(
            7,
            vec![
                ChangedFile {
                    path: "src/a.rs".into(),
                    status: FileStatus::Modified,
                    patch: Some("@@ -1 +1,2 @@\n fn a() {}\n+fn next_a() {}\n".into()),
                    additions: 1,
                    ..ChangedFile::default()
                },
                ChangedFile {
                    path: "src/b.rs".into(),
                    status: FileStatus::Modified,
                    patch: Some("@@ -1 +1,2 @@\n fn b() {}\n+fn next_b() {}\n".into()),
                    additions: 1,
                    ..ChangedFile::default()
                },
            ],
        );
        let forge = MockForge::with_state(state);
        let last_good_model = Arc::new(MockModel::always(json!({
            "summary": "A review completed successfully.",
            "executive_summary": "The change updates the reviewed source files.",
            "changes": "The source files gain a small behavior update.",
            "features": [],
            "tests": [],
            "positive_observations": {"critique": []},
            "findings": [],
            "resolved": []
        })));
        let partial_model = Arc::new(MockModel::always(json!({
            "summary": "The next review is incomplete.",
            "executive_summary": "The change updates two reviewed source files.",
            "changes": "The source files gain a small behavior update.",
            "features": [],
            "tests": [],
            "positive_observations": {"critique": []},
            "findings": [],
            "resolved": []
        })));
        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.review.lanes = vec!["critique".into()];
        config.grouping.enabled = false;
        config.summary.enabled = true;
        config.overview.enabled = false;
        config.automerge.enabled = false;
        config.review.max_changed_files = 10;
        config.review.max_changed_lines = 100;
        config.review.large_pr.files = 99;
        let last_good = crate::app::review::review_with_tree(
            &forge,
            last_good_model,
            &config,
            &repo,
            7,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the baseline review completes");
        assert!(last_good.complete(), "baseline proposal: {last_good:#?}");
        assert_eq!(last_good.head_sha, "previous-good-head");
        // The prior body is a real app-rendered publication with a distinct
        // pull-request title, so replacement by the next report is visible.
        crate::app::apply::apply(&forge, &forge, &config, &last_good, None)
            .await
            .expect("the complete baseline report publishes");
        let previous_hub = forge
            .comments(&repo, 7)
            .await
            .unwrap()
            .into_iter()
            .find(|comment| {
                comment.author == "tinysweeper"
                    && comment.body.contains(crate::summary::MARKER)
                    && comment
                        .body
                        .contains("The change updates the reviewed source files.")
            })
            .expect("the last-good report was published to the review hub");
        let last_good_report = previous_hub.body;
        let hub_comment_id = previous_hub.id.expect("published hub has an id");
        assert!(last_good_report.contains("**Reviewed head:** `previous-good-head`"));

        // A second real app review is partial by large-review selection. Its
        // failed apply must not replace the previously completed report.
        forge.replace_pull_request(PullRequest {
            number: 7,
            head_sha: "failed-apply-head".into(),
            title: "PARTIAL ATTEMPT TITLE".into(),
            ..PullRequest::default()
        });
        config.review.large_pr.files = 1;
        config.review.large_pr.groups = 1;
        config.review.large_pr.max_groups = 1;
        let partial = crate::app::review::review_with_tree(
            &forge,
            partial_model,
            &config,
            &repo,
            7,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the partial review still produces an applicable proposal");
        assert!(!partial.complete());
        assert!(partial.partial_review.is_some());
        assert_eq!(partial.head_sha, "failed-apply-head");
        assert_eq!(partial.unreviewed.len(), 1);
        assert!(
            partial
                .summary
                .as_ref()
                .is_some_and(|summary| !summary.executive_summary.is_empty())
        );
        let failed_attempt_report = crate::summary::render(&config, &partial);
        assert!(failed_attempt_report.contains("The change updates two reviewed source files."));
        assert!(!failed_attempt_report.contains("**Reviewed head:**"));
        assert_ne!(failed_attempt_report, last_good_report);

        let check_id = forge
            .publish_check(&repo, status::in_progress("failed-apply-head"))
            .await
            .unwrap();

        // The real apply path fails while publishing the lane check. The
        // worker then uses its ordinary failure finalizer for the open check.
        forge.fail_next_check_publishes(1);
        let apply_error = crate::app::apply::apply(&forge, &forge, &config, &partial, None)
            .await
            .expect_err("the lane-check publication failure reaches the worker");
        assert!(matches!(apply_error, Error::Forge(_)));

        let open = ReviewStatus {
            repo: repo.clone(),
            check_id,
            head_sha: partial.head_sha.clone(),
            installation: 1,
            stage: crate::ports::review_job::ReviewCheckStage::InProgress,
            owned: None,
            hub_comment_id: Some(hub_comment_id),
            prior_hub_body: Some(last_good_report.clone()),
        };
        let stage_completed = Arc::new(AtomicBool::new(false));
        let callback_stage = stage_completed.clone();
        let (check_result, hub_result) = conclude_status_with_writer(
            &open,
            Conclusion::Failed(&apply_error),
            &forge,
            || async move {
                callback_stage.store(true, Ordering::SeqCst);
            },
        )
        .await;
        check_result.expect("the worker concludes the existing check");
        hub_result
            .expect("the failed review updates its existing hub")
            .expect("the hub update succeeds");
        assert!(stage_completed.load(Ordering::SeqCst));

        let writes = forge.writes();
        assert!(writes.iter().any(|write| matches!(
            write,
            crate::forge::Write::CheckUpdate { check_id: updated, check }
                if *updated == check_id
                    && check.head_sha == "failed-apply-head"
                    && check.conclusion
                        == Some(crate::forge::types::CheckConclusion::ActionRequired)
        )));
        let failed_hub = writes.iter().find_map(|write| match write {
            crate::forge::Write::CommentUpdate { comment_id, body }
                if *comment_id == hub_comment_id =>
            {
                Some(body)
            }
            _ => None,
        });
        let failed_hub = failed_hub.expect("the existing hub is updated in place");
        assert!(failed_hub.contains("Last completed report"), "{failed_hub}");
        assert_eq!(failed_hub.matches(last_good_report.as_str()).count(), 1);
        assert!(failed_hub.contains(&last_good_report), "{failed_hub}");
        assert!(
            !failed_hub.contains("The change updates two reviewed source files."),
            "{failed_hub}"
        );
        assert_eq!(
            writes
                .iter()
                .filter(|write| matches!(
                    write,
                    crate::forge::Write::CommentUpdate { comment_id, .. }
                        if *comment_id == hub_comment_id
                ))
                .count(),
            1,
            "the failure report updates the original hub once"
        );
        let persisted_hub = forge
            .comments(&repo, 7)
            .await
            .unwrap()
            .into_iter()
            .find(|comment| comment.id == Some(hub_comment_id))
            .expect("the same bot comment remains persisted");
        assert_eq!(persisted_hub.body.as_str(), failed_hub.as_str());
    }

    #[tokio::test(start_paused = true)]
    async fn a_superseding_head_cancels_held_work_during_the_original_soft_drain() {
        use crate::app::review_execution::ReviewExecutionOptions;
        use crate::forge::types::{ChangedFile, FileStatus, PullRequest};
        use crate::forge::{MockForge, MockState, RepoId, Write};
        use crate::harness::MockModel;
        use crate::ports::model::{Model, ModelRequest, ModelResponse, Usage};
        use crate::ports::model_execution::ModelExecutionControl;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::sync::{Semaphore, oneshot, watch};

        struct HeldCallDrop(Arc<AtomicUsize>);
        impl Drop for HeldCallDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }

        struct DrainModel {
            control: Option<Arc<dyn ModelExecutionControl>>,
            calls: Arc<std::sync::Mutex<Vec<(String, String)>>>,
            held_tx: watch::Sender<bool>,
            falsified_tx: watch::Sender<bool>,
            dropped_held: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl Model for DrainModel {
            async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
                let prompt = request
                    .messages
                    .iter()
                    .map(|message| message.content.as_str())
                    .collect::<String>();
                let path = ["src/a.rs", "src/b.rs"]
                    .into_iter()
                    .find(|path| prompt.contains(&format!("The file is `{path}`.")))
                    .or_else(|| {
                        ["src/a.rs", "src/b.rs"]
                            .into_iter()
                            .find(|path| prompt.contains(path))
                    })
                    .unwrap_or("<no-path>")
                    .to_owned();
                self.calls
                    .lock()
                    .expect("drain-model calls")
                    .push((request.schema_name.clone(), path.clone()));
                let control = self
                    .control
                    .as_ref()
                    .ok_or(Error::ReviewExecutionUnavailable)?;
                let permit =
                    control
                        .acquire()
                        .await
                        .map_err(|refusal| Error::ModelExecutionRefused {
                            refusal,
                            usage: None,
                            accounting_complete: true,
                        })?;
                permit
                    .mark_dispatched()
                    .map_err(|refusal| Error::ModelExecutionRefused {
                        refusal,
                        usage: None,
                        accounting_complete: true,
                    })?;

                if request.schema_name == "tinysweeper_critique" && path == "src/b.rs" {
                    let _drop = HeldCallDrop(Arc::clone(&self.dropped_held));
                    self.held_tx.send_replace(true);
                    std::future::pending::<()>().await;
                }

                let value = match request.schema_name.as_str() {
                    "tinysweeper_critique" if path == "src/a.rs" => json!({
                        "summary": "The changed function needs a contract comment.",
                        "findings": [{
                            "path": "src/a.rs",
                            "existing_code": "fn a() {}",
                            "rule": "document-function-contract",
                            "title": "Document the function contract",
                            "body": "The changed function has no contract comment.",
                            "severity": "medium",
                            "confidence": 0.95
                        }]
                    }),
                    "tinysweeper_critique" => json!({
                        "summary": "No issue found.", "findings": []
                    }),
                    "tinysweeper_falsify" => {
                        self.falsified_tx.send_replace(true);
                        json!({ "incorrect": [] })
                    }
                    schema => return Err(Error::Model(format!("unexpected schema {schema}"))),
                };
                drop(permit);
                Ok(ModelResponse {
                    value,
                    model: request.model,
                    usage: Usage {
                        input_tokens: 2,
                        output_tokens: 1,
                        cost_usd: 0.01,
                        ..Usage::default()
                    },
                })
            }

            fn with_execution_control(
                &self,
                control: Arc<dyn ModelExecutionControl>,
            ) -> Option<Arc<dyn Model>> {
                Some(Arc::new(Self {
                    control: Some(control),
                    calls: Arc::clone(&self.calls),
                    held_tx: self.held_tx.clone(),
                    falsified_tx: self.falsified_tx.clone(),
                    dropped_held: Arc::clone(&self.dropped_held),
                }))
            }
        }

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let now_ms = unix_time_ms();
        let mut request_a = queued_test_review(7);
        request_a.id = crate::ports::review_job::ReviewJobId("push-a-drain".into());
        request_a.repo = "owner/repo".into();
        request_a.number = 7;
        request_a.head_sha = Some("head-a".into());
        request_a.delivery = Some("push-a-drain".into());
        request_a.accepted_at_ms = now_ms;
        assert_eq!(
            store.admit_batch(vec![request_a.clone()]).await.unwrap(),
            Admission::Admitted
        );

        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                open: true,
                title: "soft-drain supersession".into(),
                head_sha: "head-a".into(),
                ..PullRequest::default()
            },
        );
        state.files.insert(
            7,
            ["src/a.rs", "src/b.rs"]
                .into_iter()
                .map(|path| ChangedFile {
                    path: path.into(),
                    status: FileStatus::Modified,
                    additions: 1,
                    patch: Some("@@ -1 +1,2 @@\n fn a() {}\n+fn next() {}\n".into()),
                    ..ChangedFile::default()
                })
                .collect(),
        );
        let forge = MockForge::with_state(state);
        let repo = RepoId::parse("owner/repo").unwrap();
        seed_previously_published_queue_checks(store.as_ref(), now_ms).await;
        publish_test_queued_statuses_through_forge(store.as_ref(), forge.clone(), now_ms).await;
        let owned_a = store
            .claim_next("drain-worker-a", now_ms + 1)
            .await
            .unwrap()
            .unwrap();
        confirm_test_head(store.as_ref(), &owned_a, now_ms + 2).await;

        let started_at = tokio::time::Instant::now();
        let hard_deadline = started_at + Duration::from_secs(100);
        let context_a = Arc::new(crate::app::review_execution::ReviewRunContext::new(
            ReviewExecutionOptions {
                started_at,
                hard_deadline,
                mode: crate::lanes::large_pr::ReviewMode::Large,
                max_calls: Some(8),
                soft_deadline_percent: 80,
                drain_seconds: 5,
            },
        ));
        let (held_tx, held_rx) = watch::channel(false);
        let (falsified_tx, falsified_rx) = watch::channel(false);
        let dropped_held = Arc::new(AtomicUsize::new(0));
        let drain_model = Arc::new(DrainModel {
            control: None,
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            held_tx,
            falsified_tx,
            dropped_held: dropped_held.clone(),
        });
        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.review.lanes = vec!["critique".into()];
        config.review.strictness = 3;
        config.review.passes = 1;
        config.review.large_pr.files = 1;
        config.review.large_pr.max_groups = 2;
        config.review.large_pr.max_calls = 8;
        config.grouping.enabled = false;
        config.summary.enabled = false;
        config.overview.enabled = false;
        config.knowledge.extract = false;
        config.retrieval.enabled = false;
        config.threads.resolve_fixed = false;
        config.threads.ask_model = false;
        config.wireframe.enabled = false;
        config.automerge.enabled = false;
        let raw_model: Arc<dyn Model> = drain_model.clone();
        let bound_a = context_a
            .bind_model(raw_model, config.models.budget_usd_per_pr)
            .model
            .clone();
        let jobs: Arc<dyn ReviewJobStore> = store.clone();
        let model_a: Arc<dyn Model> = Arc::new(OwnedModel {
            inner: bound_a,
            execution: context_a.execution(),
            jobs: jobs.clone(),
            owned: owned_a.clone(),
            owner_lost: None,
            owner_until: None,
        });
        let apply_a = Arc::new(AtomicUsize::new(0));
        let apply_a_in_worker = apply_a.clone();
        let worker_slots = Arc::new(Semaphore::new(1));
        let worker_permit = worker_slots.clone().acquire_owned().await.unwrap();
        let (checkout_tx, checkout_rx) = oneshot::channel();
        let worker = {
            let store = store.clone();
            let forge = forge.clone();
            let config = config.clone();
            let repo = repo.clone();
            let owned = owned_a.clone();
            let context = context_a.clone();
            let model = model_a.clone();
            tokio::spawn(async move {
                let checkout_repo = repo.clone();
                let review_repo = repo.clone();
                let review_store = store.clone();
                let review_owned = owned.clone();
                let review_forge = forge.clone();
                let review_config = config.clone();
                let review_context = context.clone();
                let control_execution = context.execution();
                let paid_review = review_with_paid_budget_and_status(
                    store.as_ref(),
                    Some(&owned),
                    now_ms + 3,
                    || async { Ok(()) },
                    || async { Ok(true) },
                    || async move {
                        let timing =
                            crate::review_timing::ReviewTiming::new("owner/repo", 7, "Incremental");
                        let findings = checkout_then_review_before_deadline(
                            &timing,
                            hard_deadline,
                            &checkout_repo,
                            7,
                            || async move {
                                let _ = checkout_tx.send(());
                                tokio::time::sleep(Duration::from_secs(75)).await;
                                Ok::<(), Error>(())
                            },
                            move |_| async move {
                                assert_eq!(tokio::time::Instant::now(), started_at + Duration::from_secs(75));
                                run_if_confirmed_head_needs_review(
                                    review_store.as_ref(),
                                    Some(&review_owned),
                                    Mode::Incremental,
                                    || async {
                                        let proposal = crate::app::review::review_with_tree_observed_at_head_in_run(
                                            &review_forge,
                                            model,
                                            &review_config,
                                            &review_repo,
                                            7,
                                            None,
                                            None,
                                            None,
                                            None,
                                            Some("head-a"),
                                            review_context,
                                            None,
                                        )
                                        .await
                                        ?;
                                        apply_a_in_worker.fetch_add(1, Ordering::SeqCst);
                                        let applied = crate::app::apply::apply(
                                            &review_forge,
                                            &review_forge,
                                            &review_config,
                                            &proposal,
                                            None,
                                        )
                                        .await?;
                                        Ok(InnerReview::Complete(published_review_result(
                                            applied,
                                            proposal.findings().count(),
                                            crate::app::apply::requests_changes(
                                                &review_config,
                                                &proposal,
                                            ),
                                            status::aggregate_coverage(&proposal),
                                        )))
                                    },
                                )
                                .await
                            },
                        )
                        .await?;
                        Ok(findings)
                    },
                );
                let result = select_review_control(
                    paid_review,
                    store.as_ref(),
                    Some(&owned),
                    None,
                    None,
                    control_execution,
                )
                .await;
                if matches!(result, Err(Error::ReviewSuperseded)) {
                    assert!(matches!(
                        record_owned_disposition(
                            store.as_ref(),
                            &owned,
                            WorkerDisposition::Yielded,
                            unix_time_ms(),
                        )
                        .await,
                        Some(Ok(true))
                    ));
                }
                drop(worker_permit);
                result
            })
        };

        checkout_rx
            .await
            .expect("A begins its original paid attempt");
        tokio::time::advance(Duration::from_secs(75)).await;
        for _ in 0..10_000 {
            if *held_rx.borrow() && *falsified_rx.borrow() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            *falsified_rx.borrow(),
            "A's finding reaches its actual falsifier"
        );
        assert!(
            *held_rx.borrow(),
            "another selected group is still in model work"
        );
        for _ in 0..10_000 {
            if context_a.logical_usage().calls_attempted >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            context_a.logical_usage().calls_attempted,
            2,
            "A's critique and falsifier responses are both returned before drain"
        );
        assert_eq!(context_a.execution().started_at(), started_at);
        assert_eq!(
            context_a.execution().soft_deadline(),
            started_at + Duration::from_secs(80)
        );

        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            tokio::time::Instant::now(),
            started_at + Duration::from_secs(80)
        );
        assert!(
            !worker.is_finished(),
            "the held group remains inside the drain window"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let mut request_b = request_a;
        request_b.id = crate::ports::review_job::ReviewJobId("push-b-drain".into());
        request_b.delivery = Some("push-b-drain".into());
        request_b.head_sha = Some("head-b".into());
        request_b.accepted_at_ms = unix_time_ms();
        assert_eq!(
            store.admit_batch(vec![request_b]).await.unwrap(),
            Admission::Admitted
        );
        assert!(
            !store
                .should_continue(&owned_a, unix_time_ms())
                .await
                .unwrap()
        );
        tokio::time::advance(Duration::from_millis(500)).await;
        for _ in 0..100 {
            if worker.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            worker.is_finished(),
            "the durable supersession poll cancels A before t85"
        );
        assert!(matches!(
            worker.await.unwrap(),
            Err(Error::ReviewSuperseded)
        ));
        assert!(context_a.execution().is_cancelled());
        assert_eq!(dropped_held.load(Ordering::Acquire), 1);
        assert_eq!(
            apply_a.load(Ordering::SeqCst),
            0,
            "A never reaches real apply"
        );
        assert!(
            worker_slots.try_acquire().is_ok(),
            "A releases its worker slot on yield"
        );

        forge.replace_pull_request(PullRequest {
            number: 7,
            open: true,
            title: "soft-drain supersession".into(),
            head_sha: "head-b".into(),
            ..PullRequest::default()
        });
        publish_test_queued_statuses_through_forge(store.as_ref(), forge.clone(), unix_time_ms())
            .await;
        let owned_b = store
            .claim_next("drain-worker-b", unix_time_ms())
            .await
            .unwrap()
            .expect("the yielded job is reclaimed for B's hinted head");
        assert!(
            store
                .confirm_head(&owned_b, "head-b", unix_time_ms())
                .await
                .unwrap()
        );
        let started_b = tokio::time::Instant::now();
        let context_b = Arc::new(crate::app::review_execution::ReviewRunContext::new(
            ReviewExecutionOptions {
                started_at: started_b,
                hard_deadline: started_b + Duration::from_secs(100),
                mode: crate::lanes::large_pr::ReviewMode::Large,
                max_calls: Some(8),
                soft_deadline_percent: 80,
                drain_seconds: 5,
            },
        ));
        let mock_b = MockModel::silent();
        let base_b: Arc<dyn Model> = Arc::new(mock_b.clone());
        let bound_b = context_b
            .bind_model(base_b, config.models.budget_usd_per_pr)
            .model
            .clone();
        let model_b: Arc<dyn Model> = Arc::new(OwnedModel {
            inner: bound_b,
            execution: context_b.execution(),
            jobs: jobs.clone(),
            owned: owned_b.clone(),
            owner_lost: None,
            owner_until: None,
        });
        let result_b = run_if_confirmed_head_needs_review(
            store.as_ref(),
            Some(&owned_b),
            Mode::Incremental,
            || async {
                review_with_paid_budget_and_status(
                    store.as_ref(),
                    Some(&owned_b),
                    unix_time_ms(),
                    || async { Ok(()) },
                    || async { Ok(true) },
                    || async {
                        let proposal =
                            crate::app::review::review_with_tree_observed_at_head_in_run(
                                &forge,
                                model_b,
                                &config,
                                &repo,
                                7,
                                None,
                                None,
                                None,
                                None,
                                Some("head-b"),
                                context_b,
                                None,
                            )
                            .await?;
                        let applied =
                            crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
                                .await?;
                        Ok(InnerReview::Complete(published_review_result(
                            applied,
                            proposal.findings().count(),
                            crate::app::apply::requests_changes(&config, &proposal),
                            status::aggregate_coverage(&proposal),
                        )))
                    },
                )
                .await
            },
        )
        .await
        .expect("the replacement worker completes on its fresh execution scope");
        let InnerReview::Complete(findings) = result_b else {
            panic!("the replacement review reaches actual publication");
        };
        let (conclusion, disposition) = completed_review_disposition(findings);
        assert!(matches!(
            disposition,
            WorkerDisposition::Terminal(TerminalOutcome::Published)
        ));
        assert!(matches!(conclusion, Conclusion::Reviewed { .. }));
        assert!(matches!(
            record_owned_disposition(store.as_ref(), &owned_b, disposition, unix_time_ms()).await,
            Some(Ok(true))
        ));
        assert!(mock_b.calls() > 0);
        let writes = forge.writes();
        assert!(
            writes.iter().any(|write| matches!(
                write,
                Write::Check(check)
                    if check.head_sha == "head-b"
                        && check.name == "tinysweeper/critique"
                        && check.conclusion.is_some()
            ) || matches!(
                write,
                Write::CheckUpdate { check, .. }
                    if check.head_sha == "head-b"
                        && check.name == "tinysweeper/critique"
                        && check.conclusion.is_some()
            )),
            "B reaches a completed critique lane check on its confirmed head"
        );
        assert!(
            !writes.iter().any(|write| {
                matches!(
                    write,
                    Write::Check(check)
                        if check.head_sha == "head-a"
                            && check.name == "tinysweeper/critique"
                            && check.conclusion.is_some()
                ) || matches!(
                    write,
                    Write::CheckUpdate { check, .. }
                        if check.head_sha == "head-a"
                            && check.name == "tinysweeper/critique"
                            && check.conclusion.is_some()
                )
            }),
            "A's interrupted critique lane is never published as complete"
        );
        assert!(store.snapshot_for_test().terminal.iter().any(|entry| {
            entry.id == crate::ports::review_job::ReviewJobId("push-b-drain".into())
                && entry.outcome == TerminalOutcome::Published
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn checkout_keeps_the_original_window_and_publishes_drained_work_at_the_deadline() {
        use crate::app::review_execution::ReviewExecutionOptions;
        use crate::forge::types::{ChangedFile, FileStatus, PullRequest, ReviewEvent};
        use crate::forge::{MockForge, MockState, RepoId, Write};
        use crate::ports::model::{Model, ModelRequest, ModelResponse, Usage};
        use crate::ports::model_execution::ModelExecutionControl;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::sync::{oneshot, watch};

        struct ClockModel {
            control: Option<Arc<dyn ModelExecutionControl>>,
            attempts: Arc<std::sync::Mutex<Vec<(String, String)>>>,
            committed: Arc<std::sync::Mutex<Vec<(String, String)>>>,
            held: Arc<std::sync::Mutex<Vec<String>>>,
            held_tx: watch::Sender<Vec<String>>,
            release_fast_tx: watch::Sender<bool>,
            falsified_tx: watch::Sender<bool>,
            aborted_slow: Arc<AtomicUsize>,
        }

        struct SlowCallDrop(Arc<AtomicUsize>);
        impl Drop for SlowCallDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }

        #[async_trait::async_trait]
        impl Model for ClockModel {
            async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
                let prompt = request
                    .messages
                    .iter()
                    .map(|message| message.content.as_str())
                    .collect::<String>();
                let paths = ('a'..='j')
                    .map(|letter| format!("src/{letter}.rs"))
                    .collect::<Vec<_>>();
                let path = paths
                    .iter()
                    .find(|path| prompt.contains(&format!("The file is `{path}`.")))
                    .or_else(|| paths.iter().find(|path| prompt.contains(*path)))
                    .cloned()
                    .unwrap_or_else(|| "<no-isolated-path>".into());
                self.attempts
                    .lock()
                    .expect("attempt list lock")
                    .push((request.schema_name.clone(), path.clone()));

                let control = self
                    .control
                    .as_ref()
                    .ok_or(Error::ReviewExecutionUnavailable)?;
                let permit =
                    control
                        .acquire()
                        .await
                        .map_err(|refusal| Error::ModelExecutionRefused {
                            refusal,
                            usage: None,
                            accounting_complete: true,
                        })?;
                permit
                    .mark_dispatched()
                    .map_err(|refusal| Error::ModelExecutionRefused {
                        refusal,
                        usage: None,
                        accounting_complete: true,
                    })?;
                self.committed
                    .lock()
                    .expect("committed call list lock")
                    .push((request.schema_name.clone(), path.clone()));

                if request.schema_name == "tinysweeper_critique" && path != "src/a.rs" {
                    let mut release = self.release_fast_tx.subscribe();
                    {
                        let mut held = self.held.lock().expect("held call list lock");
                        held.push(path.clone());
                        self.held_tx.send_replace(held.clone());
                    }
                    if path == "src/i.rs" {
                        let _drop_on_drain = SlowCallDrop(Arc::clone(&self.aborted_slow));
                        std::future::pending::<()>().await;
                    } else {
                        while !*release.borrow() {
                            release
                                .changed()
                                .await
                                .expect("the test retains the release sender");
                        }
                    }
                }

                let value = match request.schema_name.as_str() {
                    "tinysweeper_critique" if path == "src/a.rs" => json!({
                        "summary": "The changed function needs a contract comment.",
                        "findings": [{
                            "path": "src/a.rs",
                            "existing_code": "fn a() {}",
                            "rule": "document-function-contract",
                            "title": "Document the function contract",
                            "body": "The changed function has no contract comment.",
                            "severity": "medium",
                            "confidence": 0.95
                        }]
                    }),
                    "tinysweeper_critique" => json!({
                        "summary": "No issue found.", "findings": []
                    }),
                    "tinysweeper_falsify" => {
                        self.falsified_tx.send_replace(true);
                        json!({ "incorrect": [] })
                    }
                    schema => return Err(Error::Model(format!("unexpected schema {schema}"))),
                };
                drop(permit);
                Ok(ModelResponse {
                    value,
                    model: request.model,
                    usage: Usage {
                        input_tokens: 2,
                        output_tokens: 1,
                        cost_usd: 0.01,
                        ..Usage::default()
                    },
                })
            }

            fn with_execution_control(
                &self,
                control: Arc<dyn ModelExecutionControl>,
            ) -> Option<Arc<dyn Model>> {
                Some(Arc::new(Self {
                    control: Some(control),
                    attempts: Arc::clone(&self.attempts),
                    committed: Arc::clone(&self.committed),
                    held: Arc::clone(&self.held),
                    held_tx: self.held_tx.clone(),
                    release_fast_tx: self.release_fast_tx.clone(),
                    falsified_tx: self.falsified_tx.clone(),
                    aborted_slow: Arc::clone(&self.aborted_slow),
                }))
            }
        }

        let started_at = tokio::time::Instant::now();
        let hard_deadline = started_at + Duration::from_secs(100);
        let run_context = Arc::new(crate::app::review_execution::ReviewRunContext::new(
            ReviewExecutionOptions {
                started_at,
                hard_deadline,
                mode: crate::lanes::large_pr::ReviewMode::Large,
                max_calls: Some(48),
                soft_deadline_percent: 80,
                drain_seconds: 5,
            },
        ));
        assert_eq!(
            run_context.execution().soft_deadline(),
            started_at + Duration::from_secs(80)
        );
        let (held_tx, held_rx) = watch::channel(Vec::<String>::new());
        let (release_fast_tx, _) = watch::channel(false);
        let (falsified_tx, falsified_rx) = watch::channel(false);
        let model = Arc::new(ClockModel {
            control: None,
            attempts: Arc::new(std::sync::Mutex::new(Vec::new())),
            committed: Arc::new(std::sync::Mutex::new(Vec::new())),
            held: Arc::new(std::sync::Mutex::new(Vec::new())),
            held_tx,
            release_fast_tx: release_fast_tx.clone(),
            falsified_tx,
            aborted_slow: Arc::new(AtomicUsize::new(0)),
        });

        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.review.lanes = vec!["critique".into()];
        config.review.strictness = 3;
        config.review.passes = 1;
        config.review.large_pr.files = 1;
        config.review.large_pr.max_groups = 10;
        config.grouping.enabled = false;
        config.summary.enabled = false;
        config.overview.enabled = false;
        config.knowledge.extract = false;
        config.retrieval.enabled = false;
        config.threads.resolve_fixed = false;
        config.threads.ask_model = false;
        config.wireframe.enabled = false;
        config.automerge.enabled = true;

        let files = ('a'..='j')
            .map(|letter| ChangedFile {
                path: format!("src/{letter}.rs"),
                status: FileStatus::Modified,
                additions: 1,
                patch: Some(format!(
                    "@@ -1 +1,2 @@\n fn {letter}() {{}}\n+fn {letter}_next() {{}}\n"
                )),
                ..ChangedFile::default()
            })
            .collect::<Vec<_>>();
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                title: "original review window".into(),
                head_sha: "abc123".into(),
                ..PullRequest::default()
            },
        );
        state.files.insert(7, files);
        let forge = MockForge::with_state(state);
        let repo = RepoId::parse("owner/repo").unwrap();

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let now_ms = unix_time_ms();
        let mut request = queued_test_review(7);
        request.repo = "owner/repo".into();
        request.head_sha = Some("abc123".into());
        store.admit_batch(vec![request]).await.unwrap();
        seed_previously_published_queue_checks(&*store, now_ms).await;
        publish_test_queued_statuses_through_forge(store.as_ref(), forge.clone(), now_ms).await;
        let owned = store
            .claim_next("clock-worker", now_ms + 1)
            .await
            .unwrap()
            .expect("the original head is claimed");
        assert!(
            store
                .confirm_head(&owned, "abc123", now_ms + 2)
                .await
                .unwrap()
        );

        let base_model: Arc<dyn Model> = model.clone();
        let bound_model = run_context
            .bind_model(base_model, config.models.budget_usd_per_pr)
            .model
            .clone();

        let (checkout_tx, checkout_rx) = oneshot::channel();
        let (proposal_tx, proposal_rx) = oneshot::channel();
        let task = {
            let store = Arc::clone(&store);
            let owned = owned.clone();
            let forge = forge.clone();
            let model = bound_model;
            let config = config.clone();
            let checkout_repo = repo.clone();
            let review_repo = repo.clone();
            let context = Arc::clone(&run_context);
            tokio::spawn(async move {
                review_with_paid_budget_and_status(
                    store.as_ref(),
                    Some(&owned),
                    now_ms + 3,
                    || async { Ok(()) },
                    || async { Ok(true) },
                    || async move {
                        let timing =
                            crate::review_timing::ReviewTiming::new("owner/repo", 7, "Incremental");
                        let findings = checkout_then_review_before_deadline(
                            &timing,
                            hard_deadline,
                            &checkout_repo,
                            7,
                            || async move {
                                let _ = checkout_tx.send(());
                                tokio::time::sleep(Duration::from_secs(75)).await;
                                Ok::<(), Error>(())
                            },
                            move |_| async move {
                                assert_eq!(
                                    tokio::time::Instant::now(),
                                    started_at + Duration::from_secs(75)
                                );
                                let proposal =
                                    crate::app::review::review_with_tree_observed_at_head_in_run(
                                        &forge,
                                        model,
                                        &config,
                                        &review_repo,
                                        7,
                                        None,
                                        None,
                                        None,
                                        None,
                                        Some("abc123"),
                                        context,
                                        None,
                                    )
                                    .await?;
                                let applied = crate::app::apply::apply(
                                    &forge, &forge, &config, &proposal, None,
                                )
                                .await?;
                                let result = published_review_result(
                                    applied,
                                    proposal.findings().count(),
                                    crate::app::apply::requests_changes(&config, &proposal),
                                    status::aggregate_coverage(&proposal),
                                );
                                let _ = proposal_tx.send(proposal);
                                Ok::<_, Error>(result)
                            },
                        )
                        .await?;
                        Ok(InnerReview::Complete(findings))
                    },
                )
                .await
            })
        };

        checkout_rx
            .await
            .expect("checkout starts in the paid attempt");
        tokio::time::advance(Duration::from_secs(75)).await;
        for _ in 0..10_000 {
            if held_rx.borrow().len() >= 8 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            held_rx.borrow().len(),
            8,
            "eight calls are held inside the drain fixture"
        );
        for _ in 0..10_000 {
            if *falsified_rx.borrow() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(*falsified_rx.borrow(), "A's finding reaches its falsifier");
        assert_eq!(
            tokio::time::Instant::now(),
            started_at + Duration::from_secs(75)
        );
        assert_eq!(run_context.execution().started_at(), started_at);
        assert_eq!(
            run_context.execution().soft_deadline(),
            started_at + Duration::from_secs(80)
        );

        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            tokio::time::Instant::now(),
            started_at + Duration::from_secs(80)
        );
        assert!(
            !task.is_finished(),
            "admitted model work remains in the drain window"
        );
        assert!(
            !model
                .attempts
                .lock()
                .unwrap()
                .iter()
                .any(|(_, path)| path == "src/j.rs"),
            "the queued tenth group does not start at the soft deadline"
        );

        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            tokio::time::Instant::now(),
            started_at + Duration::from_secs(84)
        );
        release_fast_tx.send_replace(true);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            !task.is_finished(),
            "the final admitted group is still draining at t=84"
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        let completed = task
            .await
            .expect("review worker task joins")
            .expect("review succeeds");
        let proposal = proposal_rx
            .await
            .expect("the real App proposal reaches the publication assertion");
        assert_eq!(
            tokio::time::Instant::now(),
            started_at + Duration::from_secs(85)
        );
        let InnerReview::Complete(findings) = completed else {
            panic!("the drained review reaches the worker completion path");
        };
        assert!(
            findings.is_some(),
            "a real apply publishes the partial review"
        );
        let (conclusion, disposition) = completed_review_disposition(findings);
        assert!(matches!(
            conclusion,
            Conclusion::Reviewed { findings: 1, .. }
        ));
        assert!(matches!(
            disposition,
            WorkerDisposition::Terminal(TerminalOutcome::Published)
        ));
        assert_eq!(model.aborted_slow.load(Ordering::Acquire), 1);
        assert!(
            proposal
                .findings()
                .any(|finding| finding.title == "Document the function contract"),
            "the completed finding survives the later drain cutoff"
        );
        let partial = proposal
            .partial_review
            .as_ref()
            .expect("soft and drain cutoffs produce host metadata");
        assert!(partial.omitted.iter().any(|omission| {
            omission.path == "src/j.rs"
                && omission.reason == crate::app::review::PartialReviewReason::SoftDeadline
        }));
        assert!(partial.omitted.iter().any(|omission| {
            omission.path == "src/i.rs"
                && omission.reason == crate::app::review::PartialReviewReason::DrainDeadline
        }));
        let usage = proposal.usage();
        assert_eq!(usage.input_tokens, 18);
        assert_eq!(usage.output_tokens, 9);
        assert!((usage.cost_usd - 0.09).abs() < f64::EPSILON * 8.0);
        assert_eq!(usage.calls_attempted, 9);
        let attempts = model.attempts.lock().unwrap().clone();
        let committed = model.committed.lock().unwrap().clone();
        assert_eq!(
            attempts.len(),
            10,
            "A plus eight held groups and A's falsifier are attempted"
        );
        assert_eq!(
            committed.len(),
            10,
            "the last admitted request is dropped only at drain cutoff"
        );

        let proposal_partial = run_context.execution().snapshot();
        assert_eq!(proposal_partial.committed_calls, 10);
        let writes = forge.writes();
        assert!(
            writes
                .iter()
                .any(|write| matches!(write, Write::Check(check)
            if check.conclusion == Some(crate::forge::types::CheckConclusion::Neutral)))
        );
        assert!(!writes.iter().any(|write| matches!(
            write,
            Write::Review {
                event: ReviewEvent::Approve,
                ..
            } | Write::Merged { .. }
        )));
        assert!(
            store
                .snapshot_for_test()
                .incremental_heads
                .first()
                .is_some_and(|head| head.paid_attempts == 1),
            "the same paid attempt owns the 75-second checkout and review"
        );
        assert!(
            record_owned_disposition(&*store, &owned, disposition, now_ms + 4)
                .await
                .expect("the partial publication is durably recorded")
                .expect("the worker still owns the completed head")
        );
        assert_eq!(
            store.snapshot_for_test().incremental_heads[0].terminal_outcome,
            Some(TerminalOutcome::Published)
        );
    }

    #[tokio::test]
    async fn a_push_during_retry_backoff_yields_instead_of_exhausting_the_live_head() {
        use crate::forge::{MockForge, RepoId};
        use crate::harness::MockModel;
        use crate::ports::forge::ForgeWrite;
        use crate::ports::model::{Message, Model, ModelRequest};
        use crate::ports::model_execution::ModelExecutionControl;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let now = unix_time_ms();
        let mut request = queued_test_review(84);
        request.head_sha = Some("head-b".into());
        store.admit_batch(vec![request.clone()]).await.unwrap();
        seed_previously_published_queue_checks(&store, now).await;
        let active = store
            .claim_next("retrying-worker", now)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .confirm_head(&active, "head-b", now + 1)
                .await
                .unwrap()
        );
        assert_eq!(
            store.begin_paid(&active, now + 2).await.unwrap(),
            PaidStart::Started
        );

        // The prior model attempt failed transiently. A delivery arrives during
        // its backoff, so the next handle_review attempt boundary sees the yield.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        request.id = crate::ports::review_job::ReviewJobId("delayed-a-retry".into());
        request.delivery = Some("delayed-a-retry".into());
        request.head_sha = Some("head-a-older-hint".into());
        request.accepted_at_ms = now + 3;
        store.admit_batch(vec![request]).await.unwrap();
        assert!(!store.should_continue(&active, now + 4).await.unwrap());

        let started_at = tokio::time::Instant::now();
        let deadline = started_at + REVIEW_DEADLINE;
        let run = Run {
            mode: Mode::Incremental,
            timing: crate::review_timing::ReviewTiming::new("owner/repo", 7, "Incremental"),
            slot: Arc::new(std::sync::Mutex::new(None)),
            deadline,
            execution: test_execution_context(started_at, deadline),
            expected_head: Some("head-b".into()),
            owned: Some(active.clone()),
            owner_lost: None,
            owner_until: None,
            lease_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let retry_attempt = run.clone();
        assert!(Arc::ptr_eq(&run.execution, &retry_attempt.execution));
        assert_eq!(run.deadline, retry_attempt.deadline);
        assert_eq!(run.execution.execution().started_at(), started_at);
        let boundary_error = check_owned_work(&store, &active, || run.local_owner_valid())
            .await
            .expect_err("the production retry-boundary check sees the durable yield");
        assert!(matches!(boundary_error, Error::ReviewSuperseded));
        run.cancel_for_ownership_fence(&boundary_error);
        assert!(run.execution.execution().is_cancelled());
        let disposition =
            classify_retry_boundary_error(&boundary_error, &store, Some(&active), || {
                run.local_owner_valid()
            })
            .await
            .unwrap();
        assert!(matches!(disposition, Some(WorkerDisposition::Yielded)));
        assert!(
            record_owned_disposition(
                &store,
                &active,
                disposition.expect("the boundary classified the yield"),
                now + 5,
            )
            .await
            .expect("owned retry yield")
            .expect("scheduler update")
        );
        let status_forge = MockForge::new();
        publish_test_queued_statuses_through_forge(&store, status_forge, now + 5).await;

        let resumed = store
            .claim_next("replacement-worker", now + 6)
            .await
            .unwrap()
            .unwrap();
        let replacement_started = tokio::time::Instant::now();
        let replacement_execution =
            test_execution_context(replacement_started, replacement_started + REVIEW_DEADLINE);
        assert!(!Arc::ptr_eq(&run.execution, &replacement_execution));
        assert!(replacement_execution.execution().acquire().await.is_ok());
        assert!(
            store
                .confirm_head(&resumed, "head-b", now + 7)
                .await
                .unwrap()
        );
        assert!(!store.head_is_terminal(&resumed, now + 8).await.unwrap());
        assert_eq!(
            store.begin_paid(&resumed, now + 9).await.unwrap(),
            PaidStart::Started,
            "a delivery hint during backoff must not mark B exhausted"
        );
        let model = MockModel::silent();
        let forge = MockForge::new();
        let repo = RepoId::parse("owner/repo").unwrap();
        let result = run_if_confirmed_head_needs_review(
            &store,
            Some(&resumed),
            Mode::Incremental,
            || async {
                model
                    .complete(ModelRequest {
                        model: "test".into(),
                        messages: vec![Message::user("review live head-b after retry yield")],
                        schema: serde_json::json!({"type": "object"}),
                        schema_name: "review".into(),
                        reasoning_effort: None,
                        max_tokens: 16,
                    })
                    .await?;
                forge
                    .publish_check(
                        &repo,
                        crate::forge::CheckRun {
                            name: "tinysweeper/review".into(),
                            head_sha: "head-b".into(),
                            conclusion: Some(crate::forge::CheckConclusion::Success),
                            title: "Reviewed B".into(),
                            summary: "Retry yield left the live head reviewable".into(),
                            status: None,
                            external_id: None,
                            images: vec![],
                        },
                    )
                    .await?;
                Ok(InnerReview::Complete(Some((
                    1,
                    false,
                    status::ReviewCoverage::default(),
                ))))
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, InnerReview::Complete(Some(_))));
        assert_eq!(model.requests().len(), 1);
        assert!(matches!(
            &forge.writes()[..],
            [crate::forge::Write::Check(check)] if check.head_sha == "head-b"
        ));
    }

    #[tokio::test]
    async fn a_cross_process_supersession_drops_the_in_flight_review_future_and_frees_its_permit() {
        use crate::ports::model_execution::{
            ModelExecutionControl, ModelExecutionRefusal, ModelExecutionTerminal,
            ModelExecutionTerminalOutcome, ModelExecutionTerminalUsage,
        };
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let now = unix_time_ms();
        let mut first = queued_test_review(74);
        first.repo = "owner/repo".into();
        first.number = 7;
        first.head_sha = Some("head-a".into());
        store.admit_batch(vec![first.clone()]).await.unwrap();
        seed_previously_published_queue_checks(&*store, now).await;
        let owned = store.claim_next("old-worker", now).await.unwrap().unwrap();
        assert!(store.confirm_head(&owned, "head-a", now + 1).await.unwrap());

        let execution_started = tokio::time::Instant::now();
        let worker_execution = Arc::new(crate::app::review_execution::ReviewExecution::new(
            crate::app::review_execution::ReviewExecutionOptions {
                started_at: execution_started,
                hard_deadline: execution_started + std::time::Duration::from_secs(60),
                mode: crate::lanes::large_pr::ReviewMode::Normal,
                max_calls: Some(2),
                soft_deadline_percent: 80,
                drain_seconds: 30,
            },
        ));
        let committed_permit = worker_execution.acquire().await.unwrap();
        committed_permit.mark_dispatched().unwrap();
        worker_execution.on_terminal(ModelExecutionTerminal {
            requested_model: "primary".into(),
            answered_model: Some("primary".into()),
            outcome: ModelExecutionTerminalOutcome::Succeeded,
            refused: false,
            usage: Some(ModelExecutionTerminalUsage {
                input_tokens: 11,
                output_tokens: 4,
                reasoning_tokens: 0,
                cached_tokens: 0,
                cost_usd: Some(0.02),
            }),
        });
        let held_reservation = worker_execution.acquire().await.unwrap();
        let waiting_execution = worker_execution.clone();
        let (pending_tx, pending_rx) = tokio::sync::oneshot::channel();
        let mut pending_tx = Some(pending_tx);
        let mut waiting = tokio::spawn(async move {
            let acquire = waiting_execution.acquire();
            tokio::pin!(acquire);
            std::future::poll_fn(|cx| match acquire.as_mut().poll(cx) {
                std::task::Poll::Pending => {
                    if let Some(pending_tx) = pending_tx.take() {
                        let _ = pending_tx.send(());
                    }
                    std::task::Poll::Pending
                }
                ready => ready,
            })
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), pending_rx)
            .await
            .expect("the third acquisition waits behind the held reservation")
            .expect("the waiter reports its first pending poll");

        let permits = Arc::new(Semaphore::new(
            crate::ports::review_job::MAX_RUNNING_REVIEWS,
        ));
        let mut held = Vec::new();
        for _ in 0..crate::ports::review_job::MAX_RUNNING_REVIEWS {
            held.push(permits.clone().acquire_owned().await.unwrap());
        }
        let worker_permit = held
            .pop()
            .expect("one running review owns the worker permit");
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_by_future = dropped.clone();
        let jobs: Arc<dyn ReviewJobStore> = store.clone();
        let review = async move {
            let _drop_flag = DropFlag(dropped_by_future);
            std::future::pending::<Result<()>>().await
        };
        let worker_execution_scope = worker_execution.clone();
        let worker = tokio::spawn(async move {
            let result = select_review_control(
                review,
                jobs.as_ref(),
                Some(&owned),
                None,
                None,
                worker_execution_scope,
            )
            .await;
            if matches!(result, Err(Error::ReviewSuperseded)) {
                assert!(jobs.defer_yield(&owned, unix_time_ms()).await.unwrap());
            }
            drop(worker_permit);
            result
        });

        let mut next = first;
        next.id = crate::ports::review_job::ReviewJobId("push-b".into());
        next.delivery = Some("push-b".into());
        next.head_sha = Some("head-b".into());
        store.admit_batch(vec![next]).await.unwrap();

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), worker)
                .await
                .expect("control polling must cancel without waiting for owner expiry")
                .unwrap(),
            Err(Error::ReviewSuperseded)
        ));
        assert!(
            dropped.load(Ordering::SeqCst),
            "the provider future is dropped"
        );
        let waiter_result =
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting).await;
        let cancelled = matches!(
            &waiter_result,
            Ok(Ok(Err(ModelExecutionRefusal::Cancelled)))
        );
        drop(held_reservation);
        if waiter_result.is_err() {
            if let Ok(Ok(Ok(replacement))) =
                tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting).await
            {
                drop(replacement);
            } else {
                waiting.abort();
                let _ = waiting.await;
            }
        } else if let Ok(Ok(Ok(replacement))) = waiter_result {
            drop(replacement);
        }
        assert!(
            cancelled,
            "supersession wakes pending acquisitions as cancelled"
        );
        let snapshot = worker_execution.snapshot();
        assert_eq!(snapshot.committed_calls, 1);
        assert_eq!(snapshot.reserved_calls, 0);
        assert_eq!(snapshot.physical_usage.input_tokens, 11);
        assert_eq!(snapshot.physical_usage.output_tokens, 4);
        assert_eq!(snapshot.physical_usage.cost_usd, 0.02);
        drop(committed_permit);
        assert!(
            permits.try_acquire().is_ok(),
            "the acknowledged worker releases its slot"
        );
    }

    #[tokio::test]
    async fn recovered_paid_attempts_are_bounded_before_worker_work_starts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let request = ReviewRequest {
            id: crate::ports::review_job::ReviewJobId("webhook:recovery-budget".into()),
            repo: "owner/repo".into(),
            number: 7,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some("head".into()),
            delivery: Some("recovery-budget".into()),
            event: "pull_request".into(),
            mode: ReviewJobMode::Incremental,
            accepted_at_ms: 1,
        };
        assert_eq!(
            store.admit_batch(vec![request]).await.unwrap(),
            Admission::Admitted
        );
        seed_previously_published_queue_checks(&store, 2).await;
        let starts = Arc::new(AtomicUsize::new(0));
        let mut now_ms = 2;
        for generation in 0..crate::ports::review_job::MAX_REVIEW_PAID_ATTEMPTS {
            let owned = store
                .claim_next(&format!("worker-{generation}"), now_ms)
                .await
                .unwrap()
                .expect("each expired owner is recoverable");
            confirm_test_head(&store, &owned, now_ms).await;
            let observed = starts.clone();
            assert!(matches!(
                with_paid_review_attempt(&store, &owned, now_ms, || async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                })
                .await
                .unwrap(),
                PaidWork::Started(())
            ));
            now_ms += crate::ports::review_job::REVIEW_OWNER_TTL_MS + 1;
        }

        let exhausted = store
            .claim_next("last-worker", now_ms)
            .await
            .unwrap()
            .expect("the final check is made under an owned generation");
        confirm_test_head(&store, &exhausted, now_ms).await;
        assert!(matches!(
            with_paid_review_attempt(&store, &exhausted, now_ms, || async {
                starts.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .unwrap(),
            PaidWork::Exhausted
        ));
        assert_eq!(
            starts.load(Ordering::SeqCst),
            crate::ports::review_job::MAX_REVIEW_PAID_ATTEMPTS as usize,
            "exhausted recovery must not restart token or model work"
        );
        assert_eq!(
            store.unfinished(),
            1,
            "the owner remains until status cleanup"
        );
        assert!(
            store
                .finish(&exhausted, TerminalOutcome::Exhausted, now_ms, now_ms,)
                .await
                .unwrap()
        );
        assert_eq!(store.unfinished(), 0);
        assert_eq!(
            store
                .admit_batch(vec![ReviewRequest {
                    id: crate::ports::review_job::ReviewJobId("webhook:recovery-budget".into()),
                    repo: "owner/repo".into(),
                    number: 7,
                    author: "contributor".into(),
                    installation: 1,
                    head_sha: Some("head".into()),
                    delivery: Some("recovery-budget".into()),
                    event: "pull_request".into(),
                    mode: ReviewJobMode::Incremental,
                    accepted_at_ms: now_ms,
                }])
                .await
                .unwrap(),
            Admission::Duplicate,
            "exhaustion remains terminal and deduped"
        );
    }

    #[tokio::test]
    async fn exhausted_retry_budget_concludes_the_open_check_before_tombstoning() {
        use crate::forge::{MockForge, RepoId, Write};
        use crate::harness::MockModel;
        use crate::ports::forge::ForgeWrite;
        use crate::ports::model::{Message, Model, ModelRequest};
        use serde_json::json;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let request = queued_test_review(7);
        assert_eq!(
            store.admit_batch(vec![request.clone()]).await.unwrap(),
            Admission::Admitted
        );
        let now_ms = request.accepted_at_ms + 1;
        seed_previously_published_queue_checks(&store, now_ms).await;
        let owned = store
            .claim_next("same-worker", now_ms)
            .await
            .unwrap()
            .expect("accepted review is claimable");
        assert!(
            store
                .confirm_head(&owned, "sha-7", now_ms)
                .await
                .expect("confirm the live head")
        );
        let repo = RepoId::parse("owner/repo").expect("repo id");
        let forge = MockForge::new();
        let model = MockModel::silent();
        let mut check_id = None;
        for _ in 0..crate::ports::review_job::MAX_REVIEW_PAID_ATTEMPTS {
            let attempt_model = model.clone();
            let attempt_forge = forge.clone();
            let result = review_with_paid_budget_and_status(
                &store,
                Some(&owned),
                now_ms,
                || async move {
                    attempt_model
                        .complete(ModelRequest {
                            model: "test".into(),
                            messages: vec![Message::user("retry this review")],
                            schema: json!({"type": "object"}),
                            schema_name: "review".into(),
                            reasoning_effort: None,
                            max_tokens: 16,
                        })
                        .await?;
                    Ok(())
                },
                || async {
                    if check_id.is_none() {
                        check_id = Some(
                            attempt_forge
                                .publish_check(&repo, status::in_progress("sha-7"))
                                .await
                                .expect("open the in-flight check"),
                        );
                    }
                    Ok(true)
                },
                || async { Err(Error::Forge("transient test failure".into())) },
            )
            .await;
            assert!(matches!(result, Err(Error::Forge(_))));
        }
        assert_eq!(
            model.requests().len(),
            5,
            "five paid starts reached the model"
        );
        let check_id = check_id.expect("the transient review opened one check");

        let exhausted = review_with_paid_budget_and_status(
            &store,
            Some(&owned),
            now_ms,
            || async {
                panic!("the exhausted sixth start must not reach model work");
                #[allow(unreachable_code)]
                Ok(())
            },
            || async { Ok(true) },
            || async { panic!("exhaustion must not enter review work") },
        )
        .await
        .expect("an exhausted budget is a normal terminal result");
        assert!(matches!(exhausted, InnerReview::Exhausted));

        let timing = crate::review_timing::ReviewTiming::new("owner/repo", 7, "Incremental");
        let disposition = finalize_exhausted_review(
            &timing,
            || async {
                if store.owns(&owned, now_ms).await? {
                    Ok(())
                } else {
                    Err(Error::ReviewOwnershipLost)
                }
            },
            || async {
                forge
                    .update_check(&repo, check_id, status::not_reviewed("sha-7"))
                    .await
                    .expect("conclude the open check");
            },
        )
        .await;
        assert!(matches!(
            &disposition,
            WorkerDisposition::Terminal(TerminalOutcome::Exhausted)
        ));
        assert_eq!(
            timing.snapshot().outcome,
            "failure",
            "exhausted retries are a terminal failure, not cancellation"
        );
        let record = timing.snapshot();
        assert!(
            record.phases_ms.values().sum::<u64>()
                <= record.started.unwrap().elapsed().as_millis() as u64,
            "phase durations exceed the exhausted review's wall time"
        );
        assert!(matches!(
            record_owned_disposition(&store, &owned, disposition, now_ms).await,
            Some(Ok(true))
        ));
        let conclusions: Vec<_> = forge
            .writes()
            .into_iter()
            .filter_map(|write| match write {
                Write::CheckUpdate {
                    check_id: id,
                    check,
                } if id == check_id => Some(check),
                _ => None,
            })
            .collect();
        assert_eq!(conclusions.len(), 1, "the open status is concluded once");
        assert_eq!(
            conclusions[0].conclusion,
            Some(crate::forge::CheckConclusion::Neutral)
        );
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer_bytes = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || TimingLogWriter(writer_bytes.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || drop(timing));
        let line = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(line.matches("review timing").count(), 1, "{line}");
        assert!(line.contains("outcome=failure"), "{line}");
        for phase in crate::review_timing::PHASES {
            assert!(line.contains(phase), "missing {phase}: {line}");
        }
        assert_eq!(model.requests().len(), 5, "no sixth model call was made");
        assert_eq!(
            store.admit_batch(vec![request]).await.unwrap(),
            Admission::Duplicate,
            "terminal exhaustion remains deduplicated"
        );
    }

    #[tokio::test]
    async fn exhausted_cold_recovery_opens_and_concludes_its_fresh_check() {
        use crate::forge::{MockForge, RepoId, Write};
        use crate::ports::forge::ForgeWrite;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let request = queued_test_review(8);
        store.admit_batch(vec![request]).await.unwrap();
        seed_previously_published_queue_checks(&store, 2).await;
        let mut now_ms = 2;
        for generation in 0..crate::ports::review_job::MAX_REVIEW_PAID_ATTEMPTS {
            let owned = store
                .claim_next(&format!("old-process-{generation}"), now_ms)
                .await
                .unwrap()
                .expect("each paid start has an owner");
            confirm_test_head(&store, &owned, now_ms).await;
            assert!(matches!(
                with_paid_review_attempt(&store, &owned, now_ms, || async {})
                    .await
                    .unwrap(),
                PaidWork::Started(())
            ));
            now_ms += crate::ports::review_job::REVIEW_OWNER_TTL_MS + 1;
        }

        let recovered = store
            .claim_next("replacement-process", now_ms)
            .await
            .unwrap()
            .expect("the fifth spent job remains recoverable until finalized");
        confirm_test_head(&store, &recovered, now_ms).await;
        let repo = RepoId::parse("owner/repo").expect("repo id");
        let forge = MockForge::new();
        let model_starts = Arc::new(AtomicUsize::new(0));
        let observed = model_starts.clone();
        let mut check_id = None;
        let attempt = review_with_paid_budget_and_status(
            &store,
            Some(&recovered),
            now_ms,
            || async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || async {
                check_id = Some(
                    forge
                        .publish_check(&repo, status::in_progress("sha-8"))
                        .await
                        .expect("cold recovery opens a fresh check"),
                );
                Ok(true)
            },
            || async { panic!("spent recovery must not enter review work") },
        )
        .await
        .expect("exhaustion is terminal");
        assert!(matches!(attempt, InnerReview::Exhausted));
        assert_eq!(model_starts.load(Ordering::SeqCst), 0);
        let check_id = check_id.expect("cold recovery must open a local status slot");

        let disposition = finalize_exhausted_review(
            &crate::review_timing::ReviewTiming::new("owner/repo", 7, "Incremental"),
            || async {
                if store.owns(&recovered, now_ms).await? {
                    Ok(())
                } else {
                    Err(Error::ReviewOwnershipLost)
                }
            },
            || async {
                forge
                    .update_check(&repo, check_id, status::not_reviewed("sha-8"))
                    .await
                    .expect("conclude fresh recovery check");
            },
        )
        .await;
        assert!(matches!(
            disposition,
            WorkerDisposition::Terminal(TerminalOutcome::Exhausted)
        ));
        assert!(matches!(
            record_owned_disposition(&store, &recovered, disposition, now_ms).await,
            Some(Ok(true))
        ));
        let checks: Vec<_> = forge
            .writes()
            .into_iter()
            .filter_map(|write| match write {
                Write::Check(check) => Some(check),
                Write::CheckUpdate {
                    check_id: id,
                    check,
                } if id == check_id => Some(check),
                _ => None,
            })
            .collect();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].conclusion, None);
        assert_eq!(
            checks[1].conclusion,
            Some(crate::forge::CheckConclusion::Neutral)
        );
        assert_eq!(model_starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shutdown_while_opening_status_does_not_spend_a_paid_attempt() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let request = queued_test_review(10);
        assert_eq!(
            store.admit_batch(vec![request.clone()]).await.unwrap(),
            Admission::Admitted
        );
        let now_ms = request.accepted_at_ms + 1;
        seed_previously_published_queue_checks(&store, now_ms).await;
        let owned = store
            .claim_next("shutdown-owner", now_ms)
            .await
            .unwrap()
            .expect("accepted review is claimable");
        let starts = Arc::new(AtomicUsize::new(0));
        let reviews = Arc::new(AtomicUsize::new(0));
        let observed_starts = starts.clone();
        let observed_reviews = reviews.clone();
        let result = review_with_paid_budget_and_status(
            &store,
            Some(&owned),
            now_ms,
            || async move {
                observed_starts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || async { Ok(false) },
            || async move {
                observed_reviews.fetch_add(1, Ordering::SeqCst);
                Ok(InnerReview::Complete(None))
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, InnerReview::Shutdown));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(reviews.load(Ordering::SeqCst), 0);

        let disposition = shutdown_disposition(&result, now_ms)
            .expect("shutdown leaves the accepted job recoverable");
        assert!(matches!(
            record_owned_disposition(&store, &owned, disposition, now_ms).await,
            Some(Ok(true))
        ));
        let replacement = store
            .claim_next("replacement-owner", now_ms + 5_000)
            .await
            .unwrap()
            .expect("shutdown-deferred work remains queued");
        confirm_test_head(&store, &replacement, now_ms + 5_000).await;
        assert!(matches!(
            with_paid_review_attempt(&store, &replacement, now_ms + 5_000, || async {})
                .await
                .unwrap(),
            PaidWork::Started(())
        ));
    }

    #[tokio::test]
    async fn an_exhausted_stale_generation_cannot_conclude_the_replacement_check() {
        use crate::forge::{MockForge, RepoId, Write};
        use crate::ports::forge::ForgeWrite;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        store
            .admit_batch(vec![queued_test_review(9)])
            .await
            .unwrap();
        seed_previously_published_queue_checks(&store, 2).await;
        let stale = store.claim_next("old-process", 2).await.unwrap().unwrap();
        let stale_check_time = 3;
        let forge = MockForge::new();
        let repo = RepoId::parse("owner/repo").expect("repo id");
        let stale_check_id = forge
            .publish_check(&repo, status::in_progress("sha-9"))
            .await
            .expect("old worker opens status");
        let replacement_time = 2 + crate::ports::review_job::REVIEW_OWNER_TTL_MS + 1;
        let replacement = store
            .claim_next("replacement-process", replacement_time)
            .await
            .unwrap()
            .expect("expired work gets a new generation");
        let replacement_check_id = forge
            .publish_check(&repo, status::in_progress("sha-9"))
            .await
            .expect("replacement opens its own status");

        let disposition = finalize_exhausted_review(
            &crate::review_timing::ReviewTiming::new("owner/repo", 7, "Incremental"),
            || async {
                if store.owns(&stale, stale_check_time).await? {
                    Ok(())
                } else {
                    Err(Error::ReviewOwnershipLost)
                }
            },
            || async {
                forge
                    .update_check(&repo, stale_check_id, status::not_reviewed("sha-9"))
                    .await
                    .expect("stale close is rejected before reaching the forge");
            },
        )
        .await;
        assert!(matches!(disposition, WorkerDisposition::OwnershipLost));
        let conclusions: Vec<_> = forge
            .writes()
            .into_iter()
            .filter_map(|write| match write {
                Write::CheckUpdate { check_id, check } => Some((check_id, check)),
                _ => None,
            })
            .collect();
        assert!(conclusions.is_empty());
        assert_ne!(stale_check_id, replacement_check_id);
        assert!(store.owns(&replacement, replacement_time).await.unwrap());
    }

    #[tokio::test]
    async fn cancelling_an_owned_worker_drops_heartbeat_and_allows_expiry_recovery() {
        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let request = ReviewRequest {
            id: crate::ports::review_job::ReviewJobId("webhook:cancelled".into()),
            repo: "owner/repo".into(),
            number: 7,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some("head".into()),
            delivery: Some("cancelled".into()),
            event: "pull_request".into(),
            mode: ReviewJobMode::Incremental,
            accepted_at_ms: unix_time_ms(),
        };
        assert_eq!(
            store.admit_batch(vec![request]).await.unwrap(),
            Admission::Admitted
        );
        seed_previously_published_queue_checks(&*store, unix_time_ms()).await;
        let now = unix_time_ms();
        let owned = store
            .claim_next("first", now)
            .await
            .unwrap()
            .expect("first process owns the job");
        let (lost, _receiver) = watch::channel(false);
        let (owner_until, _expiry_receiver) = watch::channel(tokio::time::Instant::now());
        let guard = spawn_review_heartbeat(
            store.clone(),
            owned.clone(),
            lost,
            owner_until,
            std::time::Duration::from_millis(5),
        );
        tokio::task::yield_now().await;
        drop(guard);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let recovered = store
            .claim_next(
                "replacement",
                now + crate::ports::review_job::REVIEW_OWNER_TTL_MS + 1,
            )
            .await
            .unwrap()
            .expect("cancelled worker's lease expires and is recoverable");
        assert_eq!(recovered.request.id, owned.request.id);
        assert!(recovered.generation > owned.generation);
    }

    #[tokio::test]
    async fn saturated_manual_admission_runs_no_expensive_resolution() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let permits = Arc::new(Semaphore::new(0));
        let expensive_reads = Arc::new(AtomicUsize::new(0));
        let reads = expensive_reads.clone();
        let result = with_manual_review_admission(permits, move || async move {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;

        assert!(matches!(result, Err(Error::ReviewCapacityFull)));
        assert_eq!(
            expensive_reads.load(Ordering::SeqCst),
            0,
            "a saturated manual request must be refused before token or PR reads"
        );

        let available = Arc::new(Semaphore::new(1));
        let started = Arc::new(AtomicUsize::new(0));
        let observed = started.clone();
        with_manual_review_admission(available, move || async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("an available permit admits the work");
        assert_eq!(started.load(Ordering::SeqCst), 1);

        let permits = Arc::new(Semaphore::new(1));
        let result: Result<()> = with_manual_review_admission(permits.clone(), || async {
            Err(Error::Forge("simulated GitHub read failure".into()))
        })
        .await;
        assert!(matches!(result, Err(Error::Forge(_))));
        assert_eq!(
            permits.available_permits(),
            1,
            "failed expansion releases admission"
        );
    }

    #[tokio::test]
    async fn durable_full_manual_queue_refuses_before_credentials_or_pull_request_reads() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let full = crate::ports::review_job_mock::MockReviewJobStore::default();
        let requests = (0..crate::ports::review_job::MAX_UNFINISHED_REVIEWS)
            .map(|number| queued_test_review(number as u64 + 1))
            .collect();
        assert_eq!(
            full.admit_batch(requests).await.unwrap(),
            Admission::Admitted
        );
        let credentials = AtomicUsize::new(0);
        let pull_requests = AtomicUsize::new(0);
        let result = with_durable_manual_capacity(&full, 1, || async {
            credentials.fetch_add(1, Ordering::SeqCst);
            pull_requests.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(result, Err(Error::ReviewCapacityFull)));
        assert_eq!(credentials.load(Ordering::SeqCst), 0);
        assert_eq!(pull_requests.load(Ordering::SeqCst), 0);

        let available = crate::ports::review_job_mock::MockReviewJobStore::default();
        with_durable_manual_capacity(&available, 1, || async {
            credentials.fetch_add(1, Ordering::SeqCst);
            pull_requests.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("capacity preflight admits work when there is durable room");
        assert_eq!(credentials.load(Ordering::SeqCst), 1);
        assert_eq!(pull_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn manual_admission_permit_lives_until_suspended_expansion_finishes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let permits = Arc::new(Semaphore::new(1));
        let expensive_reads = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let first_reads = expensive_reads.clone();
        let first_permits = permits.clone();
        let first = tokio::spawn(async move {
            with_manual_review_admission(first_permits, || async move {
                first_reads.fetch_add(1, Ordering::SeqCst);
                let _ = started_tx.send(());
                finish_rx
                    .await
                    .map_err(|_| Error::Forge("simulated GitHub read was cancelled".into()))?;
                Ok(())
            })
            .await
        });
        started_rx.await.expect("first expansion began");

        let second_reads = expensive_reads.clone();
        let second = with_manual_review_admission(permits.clone(), move || async move {
            second_reads.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(second, Err(Error::ReviewCapacityFull)));
        assert_eq!(expensive_reads.load(Ordering::SeqCst), 1);

        finish_tx.send(()).expect("first expansion is waiting");
        first
            .await
            .expect("first task joins")
            .expect("first succeeds");
        assert_eq!(permits.available_permits(), 1);

        let permits = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let pending_permits = permits.clone();
        let pending = tokio::spawn(async move {
            with_manual_review_admission(pending_permits, || async move {
                let _ = started_tx.send(());
                std::future::pending::<Result<()>>().await
            })
            .await
        });
        started_rx.await.expect("cancelled expansion began");
        pending.abort();
        assert!(
            pending
                .await
                .expect_err("task was cancelled")
                .is_cancelled()
        );
        assert_eq!(
            permits.available_permits(),
            1,
            "cancellation releases admission"
        );
        with_manual_review_admission(permits.clone(), || async { Ok(()) })
            .await
            .expect("a cancelled expansion does not strand capacity");
    }

    #[tokio::test]
    async fn a_failed_manual_expansion_releases_all_slots_without_partial_jobs() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (sender, mut receiver) = mpsc::channel(2);
        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let result = enqueue_manual_jobs(
            &sender,
            &store,
            "owner/repo",
            1,
            vec![7, 8],
            move |number| {
                let observed = observed.clone();
                Box::pin(async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    if number == 8 {
                        return Err(Error::Forge("simulated second PR read failure".into()));
                    }
                    Ok(crate::forge::types::PullRequest {
                        number,
                        author: "contributor".into(),
                        head_sha: "head".into(),
                        ..Default::default()
                    })
                })
            },
        )
        .await;

        assert!(matches!(result, Err(Error::Forge(_))));
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(
            sender.capacity(),
            2,
            "all reserved queue slots are returned"
        );
        assert_eq!(store.unfinished(), 0);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn a_full_manual_queue_refuses_before_resolving_pull_requests() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (sender, _receiver) = mpsc::channel(1);
        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let result = enqueue_manual_jobs(&sender, &store, "owner/repo", 1, vec![7, 8], move |_| {
            let observed = observed.clone();
            Box::pin(async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(crate::forge::types::PullRequest::default())
            })
        })
        .await;

        assert!(matches!(result, Err(Error::ReviewCapacityFull)));
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(
            sender.capacity(),
            1,
            "failed reservation leaves queue intact"
        );
    }

    #[tokio::test]
    async fn a_duplicate_revision_lease_skips_token_and_live_pr_reads() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let token_mints = Arc::new(AtomicUsize::new(0));
        let live_reads = Arc::new(AtomicUsize::new(0));
        let mint = token_mints.clone();
        let fetch = live_reads.clone();
        let result = continue_if_lease_claimed(false, move || async move {
            mint.fetch_add(1, Ordering::SeqCst);
            fetch.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("a duplicate lease is a normal skip");

        assert!(result.is_none());
        assert_eq!(token_mints.load(Ordering::SeqCst), 0);
        assert_eq!(live_reads.load(Ordering::SeqCst), 0);

        let mint = token_mints.clone();
        let fetch = live_reads.clone();
        assert_eq!(
            continue_if_lease_claimed(true, move || async move {
                mint.fetch_add(1, Ordering::SeqCst);
                fetch.fetch_add(1, Ordering::SeqCst);
                Ok(7)
            })
            .await
            .expect("the claimed lease proceeds"),
            Some(7)
        );
        assert_eq!(token_mints.load(Ordering::SeqCst), 1);
        assert_eq!(live_reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_live_head_lease_collision_skips_models() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let model_calls = Arc::new(AtomicUsize::new(0));
        let observed = model_calls.clone();
        let result = continue_if_lease_claimed(false, move || async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("a concurrent owner is a normal skip");

        assert!(result.is_none());
        assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    }

    fn queued_test_review(number: u64) -> ReviewRequest {
        ReviewRequest {
            id: crate::ports::review_job::ReviewJobId(format!("delivery-{number}")),
            repo: "owner/repo".into(),
            number,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some(format!("sha-{number}")),
            delivery: Some(format!("delivery-{number}")),
            event: "pull_request".into(),
            mode: ReviewJobMode::Incremental,
            accepted_at_ms: unix_time_ms(),
        }
    }

    /// Seed remote acknowledgements for low-level ownership/paid-helper tests.
    /// This assumes a status worker already published the remote checks; it does
    /// not exercise remote publication. Lifecycle tests use the Forge-backed helper.
    async fn seed_previously_published_queue_checks(store: &dyn ReviewJobStore, now_ms: i64) {
        let pending = store
            .claim_pending_checks(now_ms, QUEUE_STATUS_DRAIN_BATCH)
            .await
            .expect("test status claims succeed");
        static NEXT_FIXTURE_CHECK_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(90_000);
        for check in pending {
            if check.retired {
                continue;
            }
            assert!(
                store
                    .mark_queue_post_attempted(&check, now_ms)
                    .await
                    .unwrap()
            );
            assert!(
                store
                    .record_queued_check_id(
                        &check,
                        NEXT_FIXTURE_CHECK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                        now_ms,
                    )
                    .await
                    .unwrap()
            );
        }
    }

    fn test_status_client_factory(forge: crate::forge::MockForge) -> QueueStatusClientFactory {
        Arc::new(move |_| {
            let forge = forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        })
    }

    async fn publish_test_queued_statuses_through_forge(
        store: &dyn ReviewJobStore,
        forge: crate::forge::MockForge,
        now_ms: i64,
    ) {
        publish_pending_queue_checks_at(store, test_status_client_factory(forge), now_ms)
            .await
            .expect("test queued statuses publish through the production drainer");
    }

    async fn confirm_test_head(store: &dyn ReviewJobStore, owned: &OwnedReview, now_ms: i64) {
        let head_sha = owned
            .request
            .head_sha
            .as_deref()
            .expect("test review has a head hint to resolve");
        assert!(
            store.confirm_head(owned, head_sha, now_ms).await.unwrap(),
            "test worker confirms its Forge-resolved head before paid work"
        );
    }

    #[tokio::test]
    async fn accepted_webhook_and_manual_reviews_recover_after_wakeup_loss() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let forge = crate::forge::MockForge::new();
        let status_factory = test_status_client_factory(forge.clone());
        let webhook = Action::Review {
            repo: "owner/repo".into(),
            number: 7,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some("webhook-head".into()),
        };
        assert_eq!(
            admit_webhook_review(
                store.as_ref(),
                &webhook,
                "delivery-7",
                "pull_request",
                status_factory.clone(),
            )
            .await
            .expect("webhook admitted durably"),
            Some(Admission::Admitted)
        );

        // Manual admission resolves every PR then atomically journals the
        // complete batch. Drop its old process's channel without draining it
        // to model SIGTERM after persistence but before a worker wakeup.
        let (old_sender, old_receiver) = mpsc::channel(2);
        let queued = enqueue_manual_jobs(
            &old_sender,
            store.as_ref(),
            "owner/repo",
            1,
            vec![8, 9],
            |number| {
                Box::pin(async move {
                    Ok(crate::forge::types::PullRequest {
                        number,
                        author: "contributor".into(),
                        head_sha: format!("manual-head-{number}"),
                        ..Default::default()
                    })
                })
            },
        )
        .await
        .expect("whole manual batch accepted");
        assert_eq!(queued, vec![8, 9]);
        drop(old_sender);
        drop(old_receiver);
        publish_test_queued_statuses_through_forge(store.as_ref(), forge.clone(), unix_time_ms())
            .await;
        let queued_checks = forge.writes().into_iter().filter(|write| matches!(
            write,
            crate::forge::Write::Check(check) if check.status == Some(crate::forge::types::CheckRunStatus::Queued)
        )).count();
        assert_eq!(
            queued_checks, 3,
            "the status drainer publishes webhook and manual work before dispatch"
        );

        // A fresh dispatcher has a new, empty wakeup channel. Polling the
        // durable scheduler must run each accepted PR exactly once.
        let (new_sender, new_receiver) = mpsc::channel(1);
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS));
        let accepting = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let finished = Arc::new(AtomicUsize::new(0));
        let observed = finished.clone();
        let completing_store = store.clone();
        let dispatcher = tokio::spawn(drain_review_queue(
            new_receiver,
            permits,
            MAX_CONCURRENT_REVIEWS,
            store.clone(),
            Arc::from("restart-process"),
            accepting,
            move |owned, permit| {
                let store = completing_store.clone();
                let finished = observed.clone();
                tokio::spawn(async move {
                    let now = unix_time_ms();
                    assert!(
                        store
                            .finish(&owned, TerminalOutcome::Published, now, now)
                            .await
                            .expect("owned job finishes")
                    );
                    finished.fetch_add(1, Ordering::SeqCst);
                    drop(permit);
                });
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while finished.load(Ordering::SeqCst) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all persisted reviews recover despite lost wakeups");
        assert_eq!(finished.load(Ordering::SeqCst), 3);
        assert_eq!(store.unfinished(), 0);
        dispatcher.abort();
        drop(new_sender);
    }

    #[tokio::test]
    async fn bounded_review_queue_keeps_backlog_as_data_and_starts_after_a_permit_frees() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let permits = Arc::new(Semaphore::new(1));
        let held = permits.clone().acquire_owned().await.expect("slot");
        let (sender, receiver) = mpsc::channel(1);
        sender.try_send(()).expect("queued wakeup");
        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        assert_eq!(
            store
                .admit_batch(vec![queued_test_review(7)])
                .await
                .expect("admitted"),
            Admission::Admitted
        );
        let forge = crate::forge::MockForge::new();
        publish_test_queued_statuses_through_forge(&*store, forge.clone(), unix_time_ms()).await;
        assert_eq!(
            forge.writes().iter().filter(|write| matches!(
                write,
                crate::forge::Write::Check(check) if check.status == Some(crate::forge::types::CheckRunStatus::Queued)
            )).count(),
            1,
            "the queue worker waits for a real published check identity"
        );

        let started = Arc::new(AtomicUsize::new(0));
        let observed = started.clone();
        let dispatcher = tokio::spawn(drain_review_queue(
            receiver,
            permits,
            1,
            store,
            Arc::from("test-owner"),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            move |_job, permit| {
                observed.fetch_add(1, Ordering::SeqCst);
                drop(permit);
            },
        ));
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::SeqCst), 0);

        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a freed permit starts the queued worker");
        assert_eq!(started.load(Ordering::SeqCst), 1);
        dispatcher.abort();
    }

    #[test]
    fn a_full_review_queue_does_not_change_durable_acceptance() {
        let (sender, _receiver) = mpsc::channel(1);
        sender.try_send(()).expect("first hint fits");
        assert!(sender.try_send(()).is_err());
    }

    #[tokio::test]
    async fn accepted_review_publishes_queued_status_before_a_permit_is_available() {
        use crate::forge::MockForge;
        use crate::forge::types::CheckRunStatus;

        let permits = Semaphore::new(0);
        assert_eq!(permits.available_permits(), 0);
        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        let status_forge = forge.clone();
        let status_client: QueueStatusClientFactory = Arc::new(move |_| {
            let status_forge = status_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(status_forge.clone()),
                    write: Arc::new(status_forge),
                })
            }
            .boxed()
        });
        let action = Action::Review {
            repo: "owner/repo".into(),
            number: 7,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some("accepted-head".into()),
        };

        // This is the same durable admission helper used by the webhook route.
        // Queue status must be visible before a worker can acquire a review
        // permit; otherwise a valid accepted request looks like no work exists.
        assert_eq!(
            admit_webhook_review(
                &store,
                &action,
                "delivery-queued",
                "pull_request",
                status_client,
            )
            .await
            .expect("accepted durably"),
            Some(Admission::Admitted)
        );

        let check = forge.checks().get(status::CHECK_NAME).cloned();
        let check = check.expect("queued status appears before the permit is released");
        assert_eq!(check.head_sha, "accepted-head");
        assert_eq!(check.status, Some(CheckRunStatus::Queued));
        assert_eq!(check.conclusion, None);
        assert!(
            check.external_id.is_some(),
            "status identity is recoverable"
        );
    }

    #[tokio::test]
    async fn failed_queued_status_retries_while_all_review_slots_are_held() {
        use crate::forge::MockForge;
        use crate::forge::types::CheckRunStatus;

        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS));
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_REVIEWS {
            held.push(
                permits
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("review permit"),
            );
        }
        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        forge.fail_next_check_publishes(1);
        let status_forge = forge.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = status_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let action = Action::Review {
            repo: "owner/repo".into(),
            number: 44,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some("accepted-head".into()),
        };
        assert_eq!(
            admit_webhook_review(
                &store,
                &action,
                "delivery-status-retry",
                "pull_request",
                factory.clone(),
            )
            .await
            .expect("status failure leaves accepted work durable"),
            Some(Admission::Admitted)
        );
        assert!(forge.checks().is_empty());
        let external_id = store.snapshot_for_test().unfinished[0].review_checks[0]
            .external_id
            .clone();

        publish_pending_queue_checks_at(
            &store,
            factory,
            unix_time_ms() + crate::ports::review_job::QUEUE_STATUS_CLAIM_TTL_MS + 1,
        )
        .await
        .expect("status drainer retries without another delivery");
        assert_eq!(permits.available_permits(), 0);
        let queued = forge
            .checks()
            .remove(status::CHECK_NAME)
            .expect("queued check published");
        assert_eq!(queued.status, Some(CheckRunStatus::Queued));
        assert_eq!(queued.external_id.as_deref(), Some(external_id.as_str()));
        let durable = store.snapshot_for_test();
        assert_eq!(
            durable.unfinished[0].review_checks[0].external_id,
            external_id
        );
        assert!(durable.unfinished[0].review_checks[0].published);
        drop(held);
    }

    #[tokio::test]
    async fn queued_review_check_uses_one_remote_id_through_completion() {
        use crate::forge::types::{CheckConclusion, CheckRunStatus};
        use crate::forge::{MockForge, RepoId};
        use crate::ports::forge::{ForgeRead, ForgeWrite};
        use crate::ports::review_job::{ReviewCheckStage, ReviewJobStore};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        let repo = RepoId::parse("owner/repo").unwrap();
        let now = 10;
        let mut request = queued_test_review(224);
        request.head_sha = Some("same-id-head".into());
        request.accepted_at_ms = now;
        assert_eq!(
            store.admit_batch(vec![request]).await.unwrap(),
            Admission::Admitted
        );

        publish_pending_queue_checks_at(&store, test_status_client_factory(forge.clone()), now)
            .await
            .expect("the production drainer publishes the queued run");
        let queued_record = store.snapshot_for_test().unfinished[0].review_checks[0].clone();
        assert_eq!(queued_record.stage, ReviewCheckStage::Queued);
        let queued_id = forge
            .check_run_by_external_id(&repo, "same-id-head", &queued_record.external_id)
            .await
            .unwrap()
            .expect("the queued run has a remote ID");

        let owned = store
            .claim_next("same-id-worker", now + 1)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .confirm_head(&owned, "same-id-head", now + 2)
                .await
                .unwrap()
        );
        assert_eq!(
            store.begin_paid(&owned, now + 3).await.unwrap(),
            PaidStart::Started
        );
        assert!(
            store
                .check_transition_current(&owned, "same-id-head", queued_id, now + 4)
                .await
                .unwrap()
        );
        crate::app::apply::start_queued_review_status(&forge, &repo, queued_id, "same-id-head")
            .await
            .expect("the permit transition patches the queued run");
        assert!(
            store
                .set_check_stage(
                    &owned,
                    "same-id-head",
                    queued_id,
                    ReviewCheckStage::InProgress,
                    now + 5
                )
                .await
                .unwrap()
        );
        assert_eq!(
            store.snapshot_for_test().unfinished[0].review_checks[0].stage,
            ReviewCheckStage::InProgress
        );

        forge
            .update_check(
                &repo,
                queued_id,
                status::completed("same-id-head", 1, false, status::ReviewCoverage::default()),
            )
            .await
            .expect("completion patches the same queued run");
        assert!(
            store
                .set_check_stage(
                    &owned,
                    "same-id-head",
                    queued_id,
                    ReviewCheckStage::Completed,
                    now + 6
                )
                .await
                .unwrap()
        );
        assert_eq!(
            store.snapshot_for_test().unfinished[0].review_checks[0].stage,
            ReviewCheckStage::Completed
        );

        let writes = forge.writes();
        assert_eq!(writes.len(), 3, "one POST and two updates: {writes:#?}");
        assert!(matches!(
            &writes[0],
            crate::forge::Write::Check(check)
                if check.head_sha == "same-id-head"
                    && check.status == Some(CheckRunStatus::Queued)
        ));
        assert!(matches!(
            &writes[1],
            crate::forge::Write::CheckUpdate { check_id, check }
                if *check_id == queued_id
                    && check.head_sha == "same-id-head"
                    && check.status.unwrap_or(CheckRunStatus::InProgress)
                        == CheckRunStatus::InProgress
                    && check.conclusion.is_none()
        ));
        assert!(matches!(
            &writes[2],
            crate::forge::Write::CheckUpdate { check_id, check }
                if *check_id == queued_id
                    && check.head_sha == "same-id-head"
                    && check.conclusion == Some(CheckConclusion::Success)
        ));
        assert_eq!(
            writes
                .iter()
                .filter(|write| matches!(write, crate::forge::Write::Check(_)))
                .count(),
            1,
            "status progression never creates a second overall-review run"
        );
    }

    #[tokio::test]
    async fn failed_in_progress_write_does_not_label_exhausted_paid_work_unreviewed() {
        use crate::forge::MockForge;
        use crate::ports::review_job::{ReviewJobId, ReviewRequest};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        let status_forge = forge.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = status_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        assert_eq!(
            store
                .admit_batch(vec![ReviewRequest {
                    id: ReviewJobId("failed-stage-paid-work".into()),
                    repo: "owner/repo".into(),
                    number: 47,
                    author: "contributor".into(),
                    installation: 1,
                    head_sha: Some("head-with-paid-work".into()),
                    delivery: Some("failed-stage-paid-work".into()),
                    event: "pull_request".into(),
                    mode: ReviewJobMode::Incremental,
                    accepted_at_ms: 10,
                }])
                .await
                .unwrap(),
            Admission::Admitted,
        );
        publish_pending_queue_checks_at(&store, factory.clone(), 10)
            .await
            .unwrap();
        let queued = forge.checks().remove(status::CHECK_NAME).unwrap();
        let check_id = forge
            .check_run_by_external_id(
                &RepoId::parse("owner/repo").unwrap(),
                &queued.head_sha,
                queued.external_id.as_deref().unwrap(),
            )
            .await
            .unwrap()
            .unwrap();

        // The remote in-progress PATCH fails, but paid work can still reach
        // the scheduler's exhausted terminal state before cleanup retries.
        forge.fail_next_check_updates(1);
        assert!(
            crate::app::apply::start_queued_review_status(
                &forge,
                &RepoId::parse("owner/repo").unwrap(),
                check_id,
                &queued.head_sha,
            )
            .await
            .is_err()
        );
        let owner = store.claim_next("worker", 12).await.unwrap().unwrap();
        assert!(
            store
                .confirm_head(&owner, &queued.head_sha, 13)
                .await
                .unwrap()
        );
        assert_eq!(
            store.begin_paid(&owner, 14).await.unwrap(),
            PaidStart::Started
        );
        assert!(
            store
                .finish(&owner, TerminalOutcome::Exhausted, 20, 20)
                .await
                .unwrap()
        );

        publish_pending_queue_checks_at(&store, factory, 21)
            .await
            .unwrap();
        let terminal = forge
            .writes()
            .into_iter()
            .find_map(|write| match write {
                crate::forge::Write::CheckUpdate {
                    check_id: updated_id,
                    check,
                } if updated_id == check_id => Some(check),
                _ => None,
            })
            .expect("retired status is retried after paid work ends");
        assert_eq!(
            terminal.conclusion,
            Some(crate::forge::CheckConclusion::Neutral)
        );
        assert_eq!(terminal.title, "Review incomplete");
        assert!(terminal.summary.contains("could not confirm completion"));
        assert!(terminal.summary.contains("Work may have started"));
        assert!(!terminal.summary.contains("Nothing is wrong with the code"));
        assert!(!terminal.summary.contains("nothing was checked"));
    }

    #[tokio::test]
    async fn headless_review_delivery_is_durable_when_status_metadata_is_unavailable() {
        use crate::forge::types::CheckRunStatus;
        use crate::forge::{MockForge, types::PullRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS));
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_REVIEWS {
            held.push(
                permits
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("review permit"),
            );
        }
        assert_eq!(permits.available_permits(), 0);

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new().with_pull_request(
            PullRequest {
                number: 78,
                head_sha: "live-head-b".into(),
                ..PullRequest::default()
            },
            vec![],
            vec![],
        );
        let attempts = Arc::new(AtomicUsize::new(0));
        let retry_forge = forge.clone();
        let retry_attempts = attempts.clone();
        let status_client: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = retry_forge.clone();
            let attempts = retry_attempts.clone();
            async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(Error::Forge("simulated status metadata outage".into()));
                }
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let action = Action::Review {
            repo: "owner/repo".into(),
            number: 78,
            author: "contributor".into(),
            installation: 1,
            head_sha: None,
        };

        let admission = admit_webhook_review(
            &store,
            &action,
            "headless-metadata-outage",
            "issue_comment",
            status_client.clone(),
        )
        .await
        .expect("metadata failure leaves the webhook accepted");

        assert_eq!(admission, Some(Admission::Admitted));
        let durable = store.snapshot_for_test();
        assert_eq!(durable.unfinished.len(), 1);
        assert_eq!(
            durable.unfinished[0].accepted_ids[0].0,
            "webhook:headless-metadata-outage"
        );
        assert_eq!(durable.unfinished[0].request.head_sha, None);
        assert!(durable.unfinished[0].status_reconcile_reserved);
        assert_eq!(permits.available_permits(), 0);

        publish_pending_queue_checks_at(
            &store,
            status_client,
            unix_time_ms() + crate::ports::review_job::QUEUE_STATUS_CLAIM_TTL_MS + 1,
        )
        .await
        .expect("metadata and queued status retry independently of reviews");
        let queued = forge
            .checks()
            .remove(status::CHECK_NAME)
            .expect("resolved metadata produces a queued check");
        assert_eq!(queued.head_sha, "live-head-b");
        assert_eq!(queued.status, Some(CheckRunStatus::Queued));
        assert_eq!(permits.available_permits(), 0);
        drop(held);
    }

    #[tokio::test]
    async fn headless_metadata_read_failure_retries_without_redelivery() {
        use crate::forge::types::CheckRunStatus;
        use crate::forge::{MockForge, types::PullRequest};

        let review_pool = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS));
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_REVIEWS {
            held.push(
                review_pool
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("review permit"),
            );
        }
        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        let client_forge = forge.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = client_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let action = Action::Review {
            repo: "owner/repo".into(),
            number: 79,
            author: "contributor".into(),
            installation: 1,
            head_sha: None,
        };
        assert_eq!(
            admit_webhook_review(
                &store,
                &action,
                "metadata-read-fails",
                "issue_comment",
                factory.clone()
            )
            .await
            .expect("metadata outage does not undo durable acceptance"),
            Some(Admission::Admitted)
        );
        assert_eq!(store.snapshot_for_test().unfinished.len(), 1);
        assert!(forge.writes().is_empty());

        forge.replace_pull_request(PullRequest {
            number: 79,
            head_sha: "recovered-head".into(),
            ..PullRequest::default()
        });
        publish_pending_queue_checks_at(
            &store,
            factory,
            unix_time_ms() + crate::ports::review_job::QUEUE_STATUS_CLAIM_TTL_MS + 1,
        )
        .await
        .expect("metadata retry publishes accepted status");
        let queued = forge
            .checks()
            .remove(status::CHECK_NAME)
            .expect("queued check exists");
        assert_eq!(queued.head_sha, "recovered-head");
        assert_eq!(queued.status, Some(CheckRunStatus::Queued));
        assert_eq!(review_pool.available_permits(), 0);
        drop(held);
    }

    #[tokio::test]
    async fn closed_and_draft_headless_deliveries_write_nothing_and_allow_a_fresh_delivery() {
        use crate::forge::types::CheckRunStatus;
        use crate::forge::{MockForge, types::PullRequest};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new()
            .with_pull_request(
                PullRequest {
                    number: 80,
                    open: false,
                    head_sha: "closed-head".into(),
                    ..PullRequest::default()
                },
                vec![],
                vec![],
            )
            .with_pull_request(
                PullRequest {
                    number: 81,
                    draft: true,
                    head_sha: "draft-head".into(),
                    ..PullRequest::default()
                },
                vec![],
                vec![],
            );
        let client_forge = forge.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = client_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let action = |number| Action::Review {
            repo: "owner/repo".into(),
            number,
            author: "contributor".into(),
            installation: 1,
            head_sha: None,
        };
        for (number, delivery) in [(80, "closed-delivery"), (81, "draft-delivery")] {
            assert_eq!(
                admit_webhook_review(
                    &store,
                    &action(number),
                    delivery,
                    "issue_comment",
                    factory.clone()
                )
                .await
                .expect("accepted then resolved by metadata worker"),
                Some(Admission::Admitted)
            );
        }
        assert!(store.snapshot_for_test().unfinished.is_empty());
        assert!(
            forge.writes().is_empty(),
            "closed/draft metadata must not publish checks"
        );

        for (number, head) in [(80, "reopened-head"), (81, "ready-head")] {
            forge.replace_pull_request(PullRequest {
                number,
                open: true,
                draft: false,
                head_sha: head.into(),
                ..PullRequest::default()
            });
            assert_eq!(
                admit_webhook_review(
                    &store,
                    &action(number),
                    &format!("new-delivery-{number}"),
                    "issue_comment",
                    factory.clone(),
                )
                .await
                .expect("new delivery after eligibility changes"),
                Some(Admission::Admitted)
            );
        }
        let queued = forge
            .writes()
            .into_iter()
            .filter_map(|write| match write {
                crate::forge::Write::Check(check)
                    if check.status == Some(CheckRunStatus::Queued) =>
                {
                    Some(check.head_sha)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(queued.contains(&"reopened-head".to_string()));
        assert!(queued.contains(&"ready-head".to_string()));
    }

    #[tokio::test]
    async fn slow_successful_metadata_lookup_keeps_its_status_claim_valid() {
        use crate::forge::types::CheckRunStatus;
        use crate::forge::{MockForge, types::PullRequest};
        use crate::ports::review_job::{ReviewJobId, ReviewJobMode, ReviewRequest};
        use std::sync::atomic::{AtomicI64, Ordering};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        store
            .admit_batch(vec![ReviewRequest {
                id: ReviewJobId("slow-metadata".into()),
                repo: "owner/repo".into(),
                number: 82,
                author: "contributor".into(),
                installation: 1,
                head_sha: None,
                delivery: Some("slow-metadata".into()),
                event: "issue_comment".into(),
                mode: ReviewJobMode::Incremental,
                accepted_at_ms: 10,
            }])
            .await
            .expect("headless acceptance is durable");
        let forge = MockForge::new().with_pull_request(
            PullRequest {
                number: 82,
                head_sha: "slow-live-head".into(),
                ..PullRequest::default()
            },
            vec![],
            vec![],
        );
        let clock_ms = Arc::new(AtomicI64::new(100));
        let client_forge = forge.clone();
        let client_clock = clock_ms.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = client_forge.clone();
            let clock = client_clock.clone();
            async move {
                // Model a successful token+metadata operation that took 45s.
                clock.fetch_add(45_000, Ordering::SeqCst);
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let clock = clock_ms.clone();
        publish_pending_queue_checks_with_clock(&store, factory, move || {
            clock.load(Ordering::SeqCst)
        })
        .await
        .expect("slow status operation stays within its bounded claim");
        let queued = forge
            .checks()
            .remove(status::CHECK_NAME)
            .expect("slow metadata response still produces the check");
        assert_eq!(queued.head_sha, "slow-live-head");
        assert_eq!(queued.status, Some(CheckRunStatus::Queued));
    }

    #[tokio::test]
    async fn status_drain_claims_later_items_after_prior_network_operations() {
        use crate::forge::MockForge;
        use crate::forge::types::CheckRunStatus;
        use crate::ports::review_job::{ReviewJobId, ReviewJobMode, ReviewRequest};
        use std::sync::atomic::{AtomicI64, Ordering};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let requests = (0..QUEUE_STATUS_DRAIN_BATCH)
            .map(|index| ReviewRequest {
                id: ReviewJobId(format!("slow-status-{index}")),
                repo: "owner/repo".into(),
                number: 100 + index as u64,
                author: "contributor".into(),
                installation: 1,
                head_sha: Some(format!("head-{index}")),
                delivery: Some(format!("slow-status-{index}")),
                event: "pull_request".into(),
                mode: ReviewJobMode::Incremental,
                accepted_at_ms: index as i64,
            })
            .collect();
        assert_eq!(
            store.admit_batch(requests).await.expect("batch accepted"),
            crate::ports::review_job::Admission::Admitted
        );
        let forge = MockForge::new();
        let clock_ms = Arc::new(AtomicI64::new(100));
        let client_forge = forge.clone();
        let client_clock = clock_ms.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = client_forge.clone();
            let clock = client_clock.clone();
            async move {
                clock.fetch_add(45_000, Ordering::SeqCst);
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let clock = clock_ms.clone();
        publish_pending_queue_checks_with_clock(&store, factory, move || {
            clock.load(Ordering::SeqCst)
        })
        .await
        .expect("bounded status drain succeeds");
        let queued = forge.writes().into_iter().filter(|write| matches!(
            write,
            crate::forge::Write::Check(check) if check.status == Some(CheckRunStatus::Queued)
        )).count();
        assert_eq!(
            queued, QUEUE_STATUS_DRAIN_BATCH,
            "each item receives a fresh claim"
        );
    }

    #[tokio::test]
    async fn superseded_check_without_recovered_id_gets_terminal_status_and_releases_outbox() {
        use crate::forge::MockForge;
        use crate::forge::types::CheckConclusion;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        // A's queued POST fails, then B coalesces it before a check ID can be
        // stored. The outbox must eventually publish a terminal result rather
        // than retain A forever when external_id recovery is empty.
        forge.fail_next_check_publishes(1);
        let client_forge = forge.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = client_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let action = |head: &str| Action::Review {
            repo: "owner/repo".into(),
            number: 77,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some(head.into()),
        };
        assert_eq!(
            admit_webhook_review(
                &store,
                &action("head-a"),
                "delivery-a",
                "pull_request",
                factory.clone()
            )
            .await
            .expect("A remains durably accepted"),
            Some(Admission::Admitted)
        );
        assert!(store.snapshot_for_test().unfinished[0].review_checks[0].post_attempted);
        assert_eq!(
            admit_webhook_review(
                &store,
                &action("head-b"),
                "delivery-b",
                "pull_request",
                factory.clone()
            )
            .await
            .expect("B coalesces A"),
            Some(Admission::Admitted)
        );
        publish_pending_queue_checks_at(
            &store,
            factory,
            unix_time_ms() + crate::ports::review_job::QUEUE_STATUS_CLAIM_TTL_MS + 1,
        )
        .await
        .expect("retired status drain succeeds");

        let check = forge
            .writes()
            .into_iter()
            .find_map(|write| match write {
                crate::forge::Write::Check(check) if check.head_sha == "head-a" => Some(check),
                _ => None,
            })
            .expect("a terminal status is published for superseded A");
        assert_eq!(check.status, None, "terminal checks omit pending status");
        assert_eq!(check.conclusion, Some(CheckConclusion::Neutral));
        assert_eq!(check.title, "Review incomplete");
        assert!(check.summary.contains("could not confirm completion"));
        assert!(check.summary.contains("Work may have started"));
        assert!(!check.summary.contains("No review ran"));
        assert!(!check.summary.contains("nothing was checked"));
        assert!(store.snapshot_for_test().retired_checks.is_empty());
    }

    #[tokio::test]
    async fn accepted_terminal_post_response_loss_recovers_without_duplicate_post() {
        use crate::forge::types::{CheckConclusion, CheckRunStatus};
        use crate::forge::{MockForge, RepoId};

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        forge.fail_next_check_publishes(1);
        forge.fail_after_terminal_check_publishes(1);
        let client_forge = forge.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = client_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let action = |head: &str| Action::Review {
            repo: "owner/repo".into(),
            number: 83,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some(head.into()),
        };
        assert_eq!(
            admit_webhook_review(
                &store,
                &action("lost-a"),
                "lost-a-delivery",
                "pull_request",
                factory.clone()
            )
            .await
            .expect("A is durably accepted"),
            Some(Admission::Admitted)
        );
        assert_eq!(
            admit_webhook_review(
                &store,
                &action("lost-b"),
                "lost-b-delivery",
                "pull_request",
                factory.clone()
            )
            .await
            .expect("B supersedes A"),
            Some(Admission::Admitted)
        );
        let retired = store.snapshot_for_test().retired_checks[0].clone();
        assert!(
            retired.check.check_id.is_none(),
            "ambiguous response leaves recovery intent durable"
        );

        publish_pending_queue_checks_at(
            &store,
            factory,
            unix_time_ms() + crate::ports::review_job::QUEUE_STATUS_CLAIM_TTL_MS + 1,
        )
        .await
        .expect("external ID recovery and terminal patch complete");

        let writes = forge.writes();
        let repo = RepoId::parse("owner/repo").unwrap();
        let external_id = retired.check.external_id.as_str();
        let recovered_id = crate::ports::forge::ForgeRead::check_run_by_external_id(
            &forge,
            &repo,
            "lost-a",
            external_id,
        )
        .await
        .unwrap()
        .expect("the accepted terminal POST is found by its external identity");
        let queued_b = writes
            .iter()
            .find_map(|write| match write {
                crate::forge::Write::Check(check)
                    if check.head_sha == "lost-b"
                        && check.status == Some(CheckRunStatus::Queued) =>
                {
                    Some(check)
                }
                _ => None,
            })
            .expect("B has its own queued check");
        let queued_b_id = crate::ports::forge::ForgeRead::check_run_by_external_id(
            &forge,
            &repo,
            "lost-b",
            queued_b.external_id.as_deref().expect("queued B identity"),
        )
        .await
        .unwrap()
        .expect("the queued B check is discoverable");
        assert_ne!(
            recovered_id, queued_b_id,
            "A recovery cannot adopt B's check"
        );
        let terminal_posts = writes
            .iter()
            .filter(|write| {
                matches!(
                    write,
                    crate::forge::Write::Check(check)
                        if check.head_sha == "lost-a"
                            && check.conclusion == Some(CheckConclusion::Neutral)
                )
            })
            .count();
        assert_eq!(
            terminal_posts, 1,
            "recovery finds the accepted terminal POST"
        );
        assert!(
            writes.iter().any(|write| matches!(
                write,
                crate::forge::Write::CheckUpdate { check_id, check }
                    if *check_id == recovered_id
                        && check.head_sha == "lost-a"
                        && check.conclusion == Some(CheckConclusion::Neutral)
            )),
            "terminal write should update the ID returned by recovery: {writes:#?}"
        );
        assert!(writes.iter().any(|write| matches!(
            write,
            crate::forge::Write::Check(check)
                if check.head_sha == "lost-b" && check.status == Some(CheckRunStatus::Queued)
        )));
        assert!(store.snapshot_for_test().retired_checks.is_empty());
    }

    #[tokio::test]
    async fn superseded_no_id_statuses_do_not_fill_the_bounded_outbox() {
        use crate::forge::MockForge;

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        let client_forge = forge.clone();
        let factory: QueueStatusClientFactory = Arc::new(move |_| {
            let forge = client_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(forge.clone()),
                    write: Arc::new(forge),
                })
            }
            .boxed()
        });
        let action = |head: String, number| Action::Review {
            repo: "owner/repo".into(),
            number,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some(head),
        };

        for index in 0..=crate::ports::review_job::MAX_REVIEW_STATUS_RECORDS {
            let number = 84;
            let first = format!("outbox-a-{index}");
            forge.fail_next_check_publishes(1);
            assert_eq!(
                admit_webhook_review(
                    &store,
                    &action(first.clone(), number),
                    &format!("outbox-a-delivery-{index}"),
                    "pull_request",
                    factory.clone(),
                )
                .await
                .expect("first delivery remains durable"),
                Some(Admission::Admitted)
            );
            assert_eq!(
                admit_webhook_review(
                    &store,
                    &action(format!("outbox-b-{index}"), number),
                    &format!("outbox-b-delivery-{index}"),
                    "pull_request",
                    factory.clone(),
                )
                .await
                .expect("new delivery supersedes the failed queued POST"),
                Some(Admission::Admitted)
            );
            assert!(store.snapshot_for_test().retired_checks.is_empty());
        }
    }

    #[tokio::test]
    async fn triage_work_runs_while_every_review_permit_is_occupied() {
        use crate::forge::MockForge;
        use crate::forge::types::Issue;
        use crate::harness::MockModel;

        let review_pool = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS));
        let mut review_permits = Vec::new();
        for _ in 0..MAX_CONCURRENT_REVIEWS {
            review_permits.push(
                review_pool
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("review permit"),
            );
        }
        assert_eq!(review_pool.available_permits(), 0);

        let triage_pool = Arc::new(Semaphore::new(2));
        let repo = RepoId::parse("owner/repo").expect("repo id");
        let forge = MockForge::new().with_issue(Issue {
            number: 7,
            title: "The editor crashes while saving".into(),
            body: "Saving a large file crashes the editor.".into(),
            author: "reporter".into(),
            open: true,
            age_days: 90,
            quiet_days: 90,
            ..Issue::default()
        });
        let model = MockModel::always(serde_json::json!({
            "priority": "p1",
            "summary": "The editor crashes while saving."
        }));
        let config = Config {
            issues: crate::config::types::Issues {
                enabled: true,
                comment: false,
                apply_labels: true,
                max_labels: 3,
                dedupe: false,
                ..crate::config::types::Issues::default()
            },
            ..Config::default()
        };
        let plan = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            run_triage_job(&review_pool, &triage_pool, || async {
                let outcome =
                    crate::issues::triage(&forge, Arc::new(model.clone()), &config, &repo, 7, &[])
                        .await?;
                crate::issues::apply_plan(&forge, &repo, &outcome.plan).await?;
                Ok(outcome.plan)
            }),
        )
        .await
        .expect("triage is not queued behind reviews")
        .expect("triage permit acquired and applied");
        assert!(
            !plan.add_labels.is_empty(),
            "the real triage planner chose a label"
        );
        assert_eq!(model.requests().len(), 1, "triage called the model");
        let updated = crate::ports::forge::ForgeRead::issue(&forge, &repo, 7)
            .await
            .expect("triaged issue is readable");
        assert!(
            plan.add_labels
                .iter()
                .all(|label| updated.labels.contains(label)),
            "the deterministic triage plan was applied"
        );
        assert_eq!(review_pool.available_permits(), 0);
        drop(review_permits);
    }

    #[tokio::test]
    async fn failed_queued_status_is_retried_from_durable_state_without_review_capacity() {
        use crate::forge::MockForge;
        use crate::forge::types::CheckRunStatus;

        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS));
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_REVIEWS {
            held.push(permits.clone().acquire_owned().await.expect("review slot"));
        }
        assert_eq!(permits.available_permits(), 0);

        let store = crate::ports::review_job_mock::MockReviewJobStore::default();
        let forge = MockForge::new();
        forge.fail_next_check_publishes(1);
        let status_forge = forge.clone();
        let status_client: QueueStatusClientFactory = Arc::new(move |_| {
            let status_forge = status_forge.clone();
            async move {
                Ok(QueueStatusClients {
                    read: Arc::new(status_forge.clone()),
                    write: Arc::new(status_forge),
                })
            }
            .boxed()
        });
        let action = Action::Review {
            repo: "owner/repo".into(),
            number: 44,
            author: "contributor".into(),
            installation: 1,
            head_sha: Some("accepted-head".into()),
        };
        assert_eq!(
            admit_webhook_review(
                &store,
                &action,
                "delivery-retry-status",
                "pull_request",
                status_client.clone(),
            )
            .await
            .expect("status failure does not undo durable acceptance"),
            Some(Admission::Admitted)
        );
        assert!(forge.checks().is_empty());
        let original_external_id = store.snapshot_for_test().unfinished[0].review_checks[0]
            .external_id
            .clone();

        // This is the periodic retry path, independent of webhook redelivery
        // and review permits. Advancing the clock expires only the bounded
        // status-write claim; it does not create a new job or check identity.
        publish_pending_queue_checks_at(
            &store,
            status_client,
            unix_time_ms() + crate::ports::review_job::QUEUE_STATUS_CLAIM_TTL_MS + 1,
        )
        .await
        .expect("retry publishes queued check");
        assert_eq!(permits.available_permits(), 0);
        let check = forge
            .checks()
            .get(status::CHECK_NAME)
            .cloned()
            .expect("status retry publishes without a new delivery");
        assert_eq!(check.status, Some(CheckRunStatus::Queued));
        assert_eq!(
            check.external_id.as_deref(),
            Some(original_external_id.as_str())
        );
        assert_eq!(
            store.snapshot_for_test().unfinished[0].review_checks[0].check_id,
            Some(1)
        );
        drop(held);
    }

    #[test]
    fn manual_full_reviews_have_a_distinct_claim_key() {
        assert_ne!(
            review_lease("owner/repo", 7, Mode::Full, Some("abc")),
            review_lease("owner/repo", 7, Mode::Incremental, Some("abc"))
        );
        assert_ne!(
            review_lease("owner/repo", 7, Mode::Full, Some("abc")),
            review_lease("owner/repo", 7, Mode::Full, None)
        );
    }

    #[tokio::test]
    async fn a_queued_review_that_is_closed_before_start_does_no_work() {
        use crate::forge::types::PullRequest as PullRequestData;
        use crate::harness::MockModel;
        use crate::ports::forge::ForgeWrite;
        use crate::ports::model::{Message, Model, ModelRequest};
        use serde_json::json;

        let repo = RepoId::parse("owner/repo").expect("repo id");
        let queued = PullRequestData {
            number: 7,
            open: true,
            ..PullRequestData::default()
        };
        let forge =
            crate::forge::MockForge::new().with_pull_request(queued.clone(), vec![], vec![]);

        // The webhook queued the number while it was open. GitHub reports it
        // merged before the worker's live read, so the lane path must not run.
        forge.replace_pull_request(PullRequestData {
            open: false,
            merged: true,
            ..queued
        });
        let model = MockModel::silent();

        let result = with_open_pull_request(&forge, &repo, 7, |_| async {
            model
                .complete(ModelRequest {
                    model: "test".into(),
                    messages: vec![Message::user("run the review")],
                    schema: json!({"type": "object"}),
                    schema_name: "review".into(),
                    reasoning_effort: None,
                    max_tokens: 16,
                })
                .await?;
            forge
                .publish_check(
                    &repo,
                    crate::forge::CheckRun {
                        name: "tinysweeper/review".into(),
                        head_sha: "queued-head".into(),
                        conclusion: Some(crate::forge::CheckConclusion::Success),
                        title: "Reviewed".into(),
                        summary: "Reviewed".into(),
                        status: None,
                        external_id: None,
                        images: vec![],
                    },
                )
                .await?;
            Ok(())
        })
        .await
        .expect("closed pull request is a successful no-op");

        assert!(result.is_none(), "closed PR must skip the continuation");
        assert!(model.requests().is_empty(), "the model must not be called");
        assert!(
            forge.writes().is_empty(),
            "no review or check may be published"
        );
    }

    #[tokio::test]
    async fn an_open_pull_request_runs_the_guarded_review_continuation() {
        use crate::forge::types::PullRequest as PullRequestData;
        use crate::harness::MockModel;
        use crate::ports::forge::ForgeWrite;
        use crate::ports::model::{Message, Model, ModelRequest};
        use serde_json::json;

        let repo = RepoId::parse("owner/repo").expect("repo id");
        let pull_request = PullRequestData {
            number: 7,
            open: true,
            ..PullRequestData::default()
        };
        let forge =
            crate::forge::MockForge::new().with_pull_request(pull_request.clone(), vec![], vec![]);
        let model = MockModel::silent();

        let result = with_open_pull_request(&forge, &repo, 7, |_| async {
            model
                .complete(ModelRequest {
                    model: "test".into(),
                    messages: vec![Message::user("run the review")],
                    schema: json!({"type": "object"}),
                    schema_name: "review".into(),
                    reasoning_effort: None,
                    max_tokens: 16,
                })
                .await?;
            forge
                .publish_check(
                    &repo,
                    crate::forge::CheckRun {
                        name: "tinysweeper/review".into(),
                        head_sha: "queued-head".into(),
                        conclusion: Some(crate::forge::CheckConclusion::Success),
                        title: "Reviewed".into(),
                        summary: "Reviewed".into(),
                        status: None,
                        external_id: None,
                        images: vec![],
                    },
                )
                .await?;
            Ok(())
        })
        .await
        .expect("open pull request runs review work");

        assert!(result.is_some());
        assert_eq!(model.calls(), 1);
        assert_eq!(forge.writes().len(), 1);
    }

    #[test]
    fn a_closed_publication_stays_not_reviewed_and_does_not_produce_success() {
        let coverage = status::ReviewCoverage {
            files: Default::default(),
            unanswered_lane: false,
        };
        let result = published_review_result(
            crate::app::apply::ApplyDisposition::Closed,
            3,
            true,
            coverage,
        );

        assert!(
            result.is_none(),
            "closed publication must skip success effects"
        );
        assert!(matches!(review_conclusion(result), Conclusion::NotReviewed));
        let terminal = status::not_reviewed("reviewed-head");
        assert_eq!(
            terminal.conclusion,
            Some(crate::forge::CheckConclusion::Neutral)
        );
        let hub = crate::summary::failed(
            "reviewed-head",
            "This pass stopped before a review could be completed; the pull request may have closed or changed while it was running.",
            None,
        );
        assert!(hub.contains("stopped because the pull request closed or changed"));
        assert!(
            !hub.contains("Error"),
            "the summary is explanatory, not a raw error"
        );
    }

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

    #[tokio::test]
    async fn a_review_past_its_deadline_fails_as_a_timeout_and_is_not_retried() {
        // The shape `run_lanes` relies on: a deadline already in the
        // past resolves immediately, so a retry that arrives after the budget
        // is spent is refused instead of starting another twenty minutes.
        let now = tokio::time::Instant::now();
        let deadline = now
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap_or(now);

        let outcome = tokio::time::timeout_at(deadline, std::future::pending::<()>())
            .await
            .map_err(|_| Error::timeout("the review of o/r#1", REVIEW_DEADLINE));
        let err = outcome.expect_err("a spent deadline must not wait");
        assert!(
            matches!(err, Error::Timeout { seconds, .. } if seconds == REVIEW_DEADLINE.as_secs())
        );
        assert!(
            !failure::is_transient(&err),
            "retrying a timed-out review would spend the whole budget again"
        );

        // What a contributor reads: the check names the review and the budget
        // it missed, not a generic "something timed out".
        let check = failure::check_run("abc123", &err);
        assert_eq!(check.title, "The review ran out of time");
        assert!(
            check
                .summary
                .contains("the review of o/r#1 did not finish within 900s"),
            "summary was: {}",
            check.summary
        );
    }

    #[tokio::test]
    async fn the_mock_forge_keeps_one_completed_report_through_failed_review_cycles() {
        use crate::forge::{MockForge, RepoId, Write};
        use crate::ports::forge::ForgeWrite;

        let forge = MockForge::new();
        let repo = RepoId::parse("owner/repo").expect("repository id");
        let completed = format!(
            "{}\n\n**Reviewed head:** `def5678`\n\n## Findings\n\nNone.",
            crate::summary::MARKER
        );
        let comment_id = forge
            .create_comment(&repo, 7, &completed)
            .await
            .expect("create review hub");
        let mut prior = completed;

        for cycle in 0..10 {
            let head = format!("failed-{cycle}");
            let progress = crate::summary::in_progress(&head, Some(&prior));
            let failed = crate::summary::failed(
                &head,
                "the review of owner/repo#7 did not finish within 900s",
                Some(&progress),
            );
            forge
                .update_comment(&repo, comment_id, &failed)
                .await
                .expect("update review hub");
            prior = failed;
        }

        let final_body = forge
            .writes()
            .into_iter()
            .rev()
            .find_map(|write| match write {
                Write::CommentUpdate { body, .. } => Some(body),
                _ => None,
            })
            .expect("final hub update");
        assert_eq!(
            final_body.matches("**Reviewed head:** `def5678`").count(),
            1,
            "the MockForge output should preserve the original report once"
        );
        assert!(
            final_body.contains("timed out after 15 min"),
            "{final_body}"
        );
        assert!(
            final_body.contains("last completed review (`def5678`) is shown below"),
            "{final_body}"
        );
        assert!(!final_body.contains("\\#"), "{final_body}");
        assert!(!final_body.contains("Error::"), "{final_body}");
        assert!(
            final_body.len() < 2_000,
            "failure wrappers grew: {}",
            final_body.len()
        );
    }

    #[tokio::test]
    async fn a_phase_boundary_refuses_a_spent_budget_and_passes_a_live_one() {
        // The lease claim and the lanes both consult this before starting.
        // A spent budget must stop the run *before* it holds anything, and
        // must surface as the same non-transient timeout the lanes report.
        let slot: StatusSlot = Arc::new(std::sync::Mutex::new(None));
        let now = tokio::time::Instant::now();
        let spent = Run {
            mode: Mode::Incremental,
            slot: slot.clone(),
            timing: crate::review_timing::ReviewTiming::new("o/r", 7, "Incremental"),
            deadline: now
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap_or(now),
            execution: test_execution_context(
                now,
                now.checked_sub(std::time::Duration::from_secs(1))
                    .unwrap_or(now),
            ),
            expected_head: None,
            owned: None,
            owner_lost: None,
            owner_until: None,
            lease_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let err = spent
            .check("o/r", 7)
            .expect_err("a spent budget must refuse");
        assert!(matches!(err, Error::Timeout { ref what, .. } if what == "the review of o/r#7"));
        assert!(!failure::is_transient(&err));

        let live = Run {
            mode: Mode::Incremental,
            slot,
            timing: crate::review_timing::ReviewTiming::new("o/r", 7, "Incremental"),
            deadline: now + REVIEW_DEADLINE,
            execution: test_execution_context(now, now + REVIEW_DEADLINE),
            expected_head: None,
            owned: None,
            owner_lost: None,
            owner_until: None,
            lease_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        assert!(
            live.check("o/r", 7).is_ok(),
            "a live budget must not refuse"
        );
    }

    #[test]
    fn an_in_flight_review_is_listed_until_it_ends_and_only_removes_itself() {
        let registry = Arc::new(std::sync::Mutex::new(InFlightRegistry::default()));
        let first: StatusSlot = Arc::new(std::sync::Mutex::new(None));
        let second: StatusSlot = Arc::new(std::sync::Mutex::new(None));

        let a = InFlight::register(&registry, &first).expect("accepting");
        let b = InFlight::register(&registry, &second).expect("accepting");
        assert_eq!(registry.lock().unwrap().slots.len(), 2);

        // Finishing in the other order from registration must remove exactly
        // the finished review, by identity, not whichever came first.
        drop(a);
        let left = registry.lock().unwrap();
        assert_eq!(left.slots.len(), 1);
        assert!(Arc::ptr_eq(&left.slots[0], &second));
        drop(left);

        drop(b);
        assert!(registry.lock().unwrap().slots.is_empty());
    }

    #[test]
    fn a_review_declines_to_register_once_shutdown_has_taken_its_snapshot() {
        // The race this closes: a webhook accepted while `conclude_in_flight`
        // is still awaiting its network calls must not be able to land a slot
        // after the snapshot — it would then run unwatched by any shutdown
        // pass. Flipping `accepting` and taking the snapshot under the same
        // lock is what makes that impossible; this asserts the caller's half
        // of that contract.
        let registry = Arc::new(std::sync::Mutex::new(InFlightRegistry::default()));
        registry.lock().unwrap().accepting = false;

        let slot: StatusSlot = Arc::new(std::sync::Mutex::new(None));
        assert!(InFlight::register(&registry, &slot).is_none());
        assert!(registry.lock().unwrap().slots.is_empty());
    }

    #[tokio::test]
    async fn a_full_job_runs_after_an_incremental_terminal_without_resetting_run_limits() {
        use crate::app::review_execution::ReviewExecutionOptions;
        use crate::forge::types::{ChangedFile, FileStatus, PullRequest};
        use crate::forge::{MockForge, MockState, RepoId};
        use crate::harness::MockModel;
        use crate::ports::model::Model;
        use crate::ports::review_job::ReviewJobId;
        use std::time::Duration;

        let store = Arc::new(crate::ports::review_job_mock::MockReviewJobStore::default());
        let now_ms = unix_time_ms();
        let mut incremental_request = queued_test_review(7);
        incremental_request.repo = "owner/repo".into();
        incremental_request.head_sha = Some("abc123".into());
        incremental_request.accepted_at_ms = now_ms;
        assert_eq!(
            store.admit_batch(vec![incremental_request]).await.unwrap(),
            Admission::Admitted
        );
        let forge = MockForge::with_state(MockState {
            pull_requests: [(
                7,
                PullRequest {
                    number: 7,
                    title: "full review keeps the original limits".into(),
                    head_sha: "abc123".into(),
                    ..PullRequest::default()
                },
            )]
            .into_iter()
            .collect(),
            files: [(
                7,
                ["src/a.rs", "src/b.rs"]
                    .into_iter()
                    .map(|path| ChangedFile {
                        path: path.into(),
                        status: FileStatus::Modified,
                        additions: 1,
                        patch: Some("@@ -1 +1,2 @@\n fn item() {}\n+fn next() {}\n".into()),
                        ..ChangedFile::default()
                    })
                    .collect(),
            )]
            .into_iter()
            .collect(),
            ..MockState::default()
        });

        seed_previously_published_queue_checks(store.as_ref(), now_ms).await;
        publish_test_queued_statuses_through_forge(store.as_ref(), forge.clone(), now_ms).await;
        let incremental = store
            .claim_next("full-mode-worker", now_ms + 1)
            .await
            .unwrap()
            .expect("incremental work is claimed first");
        confirm_test_head(store.as_ref(), &incremental, now_ms + 2).await;

        let mut full_request = queued_test_review(7);
        full_request.id = ReviewJobId("manual-full-7".into());
        full_request.repo = incremental.request.repo.clone();
        full_request.head_sha = incremental.request.head_sha.clone();
        full_request.delivery = None;
        full_request.event = "manual-review".into();
        full_request.mode = ReviewJobMode::Full;
        full_request.accepted_at_ms = now_ms + 3;
        assert_eq!(
            store.admit_batch(vec![full_request]).await.unwrap(),
            Admission::Admitted,
            "manual Full work has its own identity beside an active Incremental job"
        );
        let queued = store.snapshot_for_test();
        assert_eq!(queued.unfinished.len(), 2);
        assert_ne!(
            queued.unfinished[0].request.id,
            queued.unfinished[1].request.id
        );
        assert!(
            queued
                .unfinished
                .iter()
                .any(|job| { job.request.mode == ReviewJobMode::Full && job.owner.is_none() })
        );
        publish_test_queued_statuses_through_forge(store.as_ref(), forge.clone(), now_ms + 3).await;
        assert!(
            store
                .should_continue(&incremental, now_ms + 4)
                .await
                .unwrap()
        );
        assert!(
            store
                .claim_next_with_limit("at-global-limit", now_ms + 4, 1)
                .await
                .unwrap()
                .is_none(),
            "the Full job stays queued while the one shared worker slot is occupied"
        );

        assert_eq!(
            store.begin_paid(&incremental, now_ms + 5).await.unwrap(),
            PaidStart::Started
        );
        assert!(
            store
                .finish(
                    &incremental,
                    TerminalOutcome::Published,
                    now_ms + 6,
                    now_ms + 6,
                )
                .await
                .unwrap()
        );
        let inc_snapshot = store.snapshot_for_test();
        assert!(inc_snapshot.terminal.iter().any(|entry| {
            entry.id == incremental.request.id && entry.outcome == TerminalOutcome::Published
        }));

        let full = store
            .claim_next("full-mode-worker", now_ms + 8)
            .await
            .unwrap()
            .expect("the queued Full job becomes claimable after the slot is released");
        assert_eq!(full.request.mode, ReviewJobMode::Full);
        assert_eq!(full.request.head_sha.as_deref(), Some("abc123"));
        confirm_test_head(store.as_ref(), &full, now_ms + 9).await;
        assert_eq!(
            store.begin_paid(&full, now_ms + 10).await.unwrap(),
            PaidStart::Started
        );
        let incrementals = store.snapshot_for_test().incremental_heads;
        assert!(
            incrementals.iter().any(|head| {
                head.repo == incremental.request.repo
                    && head.number == incremental.request.number
                    && head.head_sha == "abc123"
                    && head.paid_attempts == 1
                    && head.terminal_outcome == Some(TerminalOutcome::Published)
            }),
            "the Incremental terminal marker and paid count remain after Full starts"
        );

        let mut base: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        base.review.incremental = true;
        base.review.lanes = vec!["critique".into()];
        base.review.passes = 3;
        base.review.large_pr.files = 1;
        base.review.large_pr.max_groups = 1;
        base.review.large_pr.max_calls = 1;
        base.models.budget_usd_per_pr = 4.25;
        base.grouping.enabled = false;
        base.summary.enabled = false;
        base.overview.enabled = false;
        base.knowledge.extract = false;
        base.retrieval.enabled = false;
        base.threads.resolve_fixed = false;
        base.threads.ask_model = false;
        base.wireframe.enabled = false;
        let full_config = config_for(&base, Mode::Full);
        assert!(!full_config.review.incremental);
        assert_eq!(full_config.review.passes, base.review.passes);
        assert_eq!(full_config.models.budget_usd_per_pr, 4.25);
        assert_eq!(full_config.review.large_pr.files, 1);
        assert_eq!(full_config.review.large_pr.max_groups, 1);

        let started_at = tokio::time::Instant::now();
        let hard_deadline = started_at + Duration::from_secs(100);
        let execution = Arc::new(crate::app::review_execution::ReviewRunContext::new(
            ReviewExecutionOptions {
                started_at,
                hard_deadline,
                mode: crate::lanes::large_pr::ReviewMode::Large,
                max_calls: Some(1),
                soft_deadline_percent: 80,
                drain_seconds: 5,
            },
        ));
        let mock_model = MockModel::silent();
        let model_port: Arc<dyn Model> = Arc::new(mock_model.clone());
        let model = execution
            .bind_model(model_port, full_config.models.budget_usd_per_pr)
            .model
            .clone();
        let execution_for_assert = execution.clone();
        let observed_proposal = Arc::new(std::sync::Mutex::new(None));
        let proposal_slot = observed_proposal.clone();
        let repo = RepoId::parse("owner/repo").unwrap();
        let proposal = run_if_confirmed_head_needs_review(
            store.as_ref(),
            Some(&full),
            Mode::Full,
            || async move {
                let proposal = crate::app::review::review_with_tree_observed_at_head_in_run(
                    &forge,
                    model,
                    &full_config,
                    &repo,
                    7,
                    None,
                    None,
                    None,
                    None,
                    Some("abc123"),
                    execution,
                    None,
                )
                .await?;
                *proposal_slot.lock().expect("Full proposal capture lock") = Some(proposal.clone());
                Ok(InnerReview::Complete(Some((
                    proposal.findings().count(),
                    crate::app::apply::requests_changes(&full_config, &proposal),
                    status::aggregate_coverage(&proposal),
                ))))
            },
        )
        .await
        .expect("Full mode bypasses the Incremental terminal-head suppression");
        assert!(matches!(proposal, InnerReview::Complete(Some(_))));
        assert_eq!(
            mock_model.calls(),
            1,
            "the Large execution shares its one-call cap"
        );
        let proposal = observed_proposal
            .lock()
            .expect("Full proposal capture lock")
            .take()
            .expect("the confirmed Full path invokes the actual App review");
        let plan = proposal
            .partial_review
            .as_ref()
            .expect("Large plan metadata");
        assert_eq!(plan.mode, crate::lanes::large_pr::ReviewMode::Large);
        assert_eq!(plan.selected_groups.len(), 1);
        assert!(
            !proposal.unreviewed.is_empty(),
            "the one-group plan reports omitted files"
        );
        assert_eq!(
            execution_for_assert.execution().soft_deadline(),
            started_at + Duration::from_secs(80),
            "Full execution keeps the same run clock instead of opening a fresh window"
        );
        let proposal_metadata = store.snapshot_for_test();
        assert_eq!(
            proposal_metadata.incremental_heads.len(),
            1,
            "a Full pass does not clear the Incremental ledger"
        );
        assert!(
            store
                .finish(&full, TerminalOutcome::Published, now_ms + 12, now_ms + 12)
                .await
                .unwrap()
        );
    }

    #[test]
    fn a_full_review_turns_the_incremental_path_off_and_changes_nothing_else() {
        // This is the whole of what "full" means. `review.incremental` gates
        // both halves of the incremental state in `crate::app::review`: the prior findings
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
            "a full review is the same review with incremental reuse disabled"
        );

        let incremental = config_for(&base, Mode::Incremental);
        assert!(
            incremental.review.incremental,
            "the webhook path must be untouched"
        );
    }

    #[test]
    fn kill_switch_labels_prevent_the_early_hub_write() {
        let mut config = Config::default();
        config.labels.human_review = "human-review".into();
        let pull_request = PullRequest {
            labels: vec![config.labels.human_review.clone()],
            ..PullRequest::default()
        };

        assert!(review_is_kill_switched(&config, &pull_request));
        assert!(!review_is_kill_switched(&config, &PullRequest::default()));
    }
}
