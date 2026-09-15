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
use crate::ports::forge::ForgeRead as _;
use crate::ports::knowledge::KnowledgeStore;
use crate::pr_triage::Report as PrTriageReport;
use crate::server::admin::{self, AdminAuth};
use crate::server::auth::AppAuth;
use crate::server::failure;
use crate::server::indexing::{IndexBackend, index_in_background};
use crate::server::manual::{self, FullReviews, MergeReport, Merges, Remembers, Triages};
use crate::server::memory::{
    BackfillStart, BackfillStatus, MemoryBackend, ingest_in_background, remember_in_background,
};
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
const MAX_CONCURRENT_REVIEWS: usize = 4;

/// How long one review may take, wall clock, from delivery to verdict.
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
    /// The memory engine, when `[memory]` names one. `None` reviews without
    /// memory, which is what tinysweeper did before an engine existed.
    memory: Option<Arc<MemoryBackend>>,
    /// Bounds concurrent indexing separately from concurrent reviewing: a
    /// delivery burst must not turn into a burst of full indexes.
    index_permits: Arc<Semaphore>,
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

    // Same shape as the index: off is a choice, unreachable is a boot failure.
    let memory = match MemoryBackend::open(&config.config).await? {
        Some(backend) => {
            tracing::info!(engine = "cortex", "memory is on");
            Some(Arc::new(backend))
        }
        None => {
            tracing::info!("memory is disabled: no engine configured");
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
        memory,
        index_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_INDEXES)),
        preview_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        in_flight: Arc::new(std::sync::Mutex::new(InFlightRegistry::default())),
    };

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
        state: manual_state.clone(),
    });
    let merges: Arc<dyn Merges> = Arc::new(MergeDispatch {
        state: manual_state.clone(),
    });
    let triages: Arc<dyn Triages> = Arc::new(TriageDispatch {
        state: manual_state.clone(),
    });
    // Only when there is an engine: the routes then answer 503 instead of
    // 404, so an operator learns the deployment has no memory rather than
    // that they mistyped the path.
    let remembers: Option<Arc<dyn Remembers>> = manual_state.memory.is_some().then(|| {
        Arc::new(MemoryDispatch {
            state: manual_state.clone(),
        }) as Arc<dyn Remembers>
    });

    // The periodic sweep, spawned only when it has both a switch and an
    // interval. It is what makes triage automatic rather than a button: a
    // duplicate opened at midnight is labelled by morning without anybody
    // pressing anything.
    spawn_triage_sweeps(manual_state, triages.clone());

    if let Some(routes) = manual::router(
        manual_auth,
        manual::allowed_org(),
        dispatch,
        merges,
        triages,
        remembers,
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

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|err| Error::Forge(format!("could not bind {bind}: {err}")))?;

    tracing::info!(%bind, "tinysweeper is listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(shutdown_state))
        .await
        .map_err(|err| Error::Forge(format!("server stopped: {err}")))
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

    // Memory listens to every delivery that touches a conversation,
    // independently of what the delivery is routed to below — including the
    // ones routed to nothing, which is where every other agent's comment
    // lands. Spawned and forgotten: it reads GitHub and writes the engine,
    // never the other way round, so nothing about the delivery waits on it.
    if let (Some(backend), Some(conversation)) =
        (&state.memory, webhook::remember_trigger(&event, &payload))
        && state.config.config.memory.ingest_discussions
    {
        tokio::spawn(remember_in_background(
            backend.clone(),
            Arc::new(state.config.config.clone()),
            state.auth.clone(),
            state.index_permits.clone(),
            conversation,
        ));
    }

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
    tokio::spawn(dispatch(state, action, delivery, event));
    (StatusCode::ACCEPTED, "queued").into_response()
}

/// Claim the delivery, then run whatever it asked for.
///
/// Runs off the request path so nothing here is racing GitHub's clock.
async fn dispatch(state: AppState, action: Action, delivery: String, event: String) {
    match state.store.claim_delivery(&delivery, &event).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(%delivery, "already handled");
            return;
        }
        Err(err) => {
            // The delivery is already acknowledged, so there is no retry to
            // ask for and dropping the work would be silent. Proceeding risks
            // duplicating a review that a redelivery also runs; the lease in
            // `review_inner` is what makes that risk affordable, and doing the
            // work twice is a better failure than never doing it.
            tracing::error!(%err, %delivery, "could not claim the delivery; proceeding unclaimed");
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
        } => {
            handle_review(
                state,
                repo,
                number,
                author,
                installation,
                Mode::Incremental,
                Some(delivery),
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
                tokio::spawn(handle_automerge(
                    state.clone(),
                    repo.clone(),
                    number,
                    installation,
                ));
            }
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
            // Through the port, which is also where pull request triage reads
            // them: one definition of "the open pull requests", so the button
            // and the sweep can never disagree about what is open.
            None => open_numbers(&forge, repo, MAX_MANUAL_REVIEWS).await?,
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
                None,
            ));
            queued.push(number);
        }

        Ok(queued)
    }
}

/// The memory backfill button's way into the engine.
///
/// The installation is resolved from the repository, as the review button
/// does; the token it mints is a read token, used for nothing but listing
/// conversations. The walk runs in the background because it is minutes
/// long; one conversation is remembered inline because it is one request.
struct MemoryDispatch {
    state: AppState,
}

impl MemoryDispatch {
    async fn read_token(&self, repo: &RepoId) -> Result<String> {
        let installation = self
            .state
            .auth
            .installation_for_repo(&repo.owner, &repo.name)
            .await?;
        self.state.auth.installation_token(installation).await
    }

    fn backend(&self) -> Result<Arc<MemoryBackend>> {
        self.state
            .memory
            .clone()
            .ok_or_else(|| Error::Forge("no memory engine is configured".into()))
    }
}

#[async_trait::async_trait]
impl Remembers for MemoryDispatch {
    async fn remember(
        &self,
        repo: &RepoId,
        number: u64,
        pull_request: bool,
    ) -> Result<crate::memory::DiscussionReport> {
        let backend = self.backend()?;
        let token = self.read_token(repo).await?;
        backend
            .remember_conversation(
                &self.state.config.config,
                repo,
                number,
                pull_request,
                &token,
            )
            .await
    }

    async fn backfill(
        &self,
        repo: &RepoId,
        since: Option<String>,
        limit: usize,
    ) -> Result<BackfillStart> {
        let backend = self.backend()?;
        // Resolved and minted once up front, so a repository the app is not
        // installed on is a plain error to the operator rather than a
        // backfill that fails only once the background task gets around to
        // it. The token itself is not carried into the walk: `run_backfill`
        // re-mints per chunk from `installation`, since a walk long enough to
        // renew several times cannot ride on one snapshot of it.
        let installation = self
            .state
            .auth
            .installation_for_repo(&repo.owner, &repo.name)
            .await?;
        self.state.auth.installation_token(installation).await?;
        let started = match backend.start_backfill(repo, since.clone(), limit) {
            BackfillStart::Started(status) => status,
            running @ BackfillStart::AlreadyRunning(_) => return Ok(running),
        };
        let config = Arc::new(self.state.config.config.clone());
        let repo = repo.clone();
        let auth = self.state.auth.clone();
        tokio::spawn(async move {
            // Not on the index permit pool: `run_backfill` serializes walks
            // on a lock of its own, so a minutes-long walk never holds a
            // permit that code ingestion and live re-reads are waiting for.
            backend
                .run_backfill(&config, &repo, since.as_deref(), limit, &auth, installation)
                .await;
        });
        Ok(BackfillStart::Started(started))
    }

    async fn status(&self, repo: &RepoId) -> Result<Option<BackfillStatus>> {
        Ok(self.backend()?.backfill_status(repo))
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
/// Memory is deliberately untouched by this flag: it is the repository's
/// accumulated knowledge — conventions, and what became of earlier findings —
/// not this pull request's incremental state, and a manual full review wants
/// "you said this before and they said no" exactly as much as an ordinary
/// one does.
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
struct Run {
    mode: Mode,
    slot: StatusSlot,
    /// Fixed when the review is accepted, not per attempt. See `handle_review`.
    deadline: tokio::time::Instant,
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
    if slot.lock().expect("status slot").is_some() {
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

    let check = match conclusion {
        Conclusion::Reviewed(findings) => status::completed(&open.head_sha, findings),
        Conclusion::NotReviewed => status::not_reviewed(&open.head_sha),
        Conclusion::Failed(err) => failure::check_run(&open.head_sha, err),
    };

    let written = async {
        use crate::ports::forge::ForgeWrite;
        let token = state.auth.installation_token(open.installation).await?;
        crate::forge::github::GitHubWrite::new(&token)?
            .update_check(&open.repo, open.check_id, check)
            .await
    }
    .await;

    if let Err(err) = written {
        // Worth an error rather than a warning: the check is now stuck
        // in-progress, and a pending check refuses auto-merge on this commit
        // until somebody pushes again.
        tracing::error!(
            %err, repo = %open.repo, check_id = open.check_id,
            "could not conclude the in-progress check; it will block auto-merge until the next push"
        );
    }
}

/// How a review ended, for the umbrella check.
enum Conclusion<'a> {
    /// The lanes ran. Carries the finding count, for the title.
    Reviewed(usize),
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

/// Review one pull request, off the request path.
///
/// A failed review must not take the server with it — one pull request going
/// wrong is not an outage — but it must also not be *invisible*. A transient
/// failure is retried a few times, and a review that still cannot run says so
/// on the pull request through a blocking check. See `server::failure` for why
/// the log line alone was not enough.
async fn handle_review(
    state: AppState,
    repo: String,
    number: u64,
    author: String,
    installation: u64,
    mode: Mode,
    delivery: Option<String>,
) {
    let slot: StatusSlot = Arc::new(std::sync::Mutex::new(None));
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
        if let Some(delivery) = delivery
            && let Err(release) = state.store.release_delivery(&delivery).await
        {
            tracing::error!(%release, %delivery, "could not release the declined delivery claim");
        }
        return;
    };

    // The permit is taken here, before the clock starts, and held across
    // every attempt. Queueing behind the other reviews is not time this
    // review spent, and counting it would let a delivery that merely waited
    // its turn "time out" without ever running — and then, having no SHA of
    // its own, report that failure against whatever head is live by then.
    // Holding it across retries also keeps a retry from going to the back of
    // the queue behind reviews that arrived while it was failing.
    let permit = match state.permits.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(err) => {
            tracing::error!(%err, %repo, number, "the review permit pool is closed");
            return;
        }
    };

    // One deadline for the whole review, retries included. A per-attempt
    // deadline would let three transient failures late in the run stretch a
    // single pull request to three times the budget, all under one check that
    // has said "reviewing" the entire time.
    let run = Run {
        mode,
        slot: slot.clone(),
        deadline: tokio::time::Instant::now() + REVIEW_DEADLINE,
    };

    let mut attempt = 1;
    let err = loop {
        match review_inner(&state, &repo, number, &author, installation, &run).await {
            Ok(findings) => {
                // Usually there is nothing to close: a run that declines — a
                // blocked contributor, a draft, a lease another worker holds —
                // does so before opening a check, and `close_status` is a no-op
                // when the slot is empty. `NotReviewed` covers the one ordering
                // where it is not: an earlier attempt opened the check and this
                // one declined.
                let conclusion = match findings {
                    Some(findings) => Conclusion::Reviewed(findings),
                    None => Conclusion::NotReviewed,
                };
                close_status(&state, &slot, conclusion).await;
                return;
            }
            Err(err) => {
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

    tracing::error!(%err, %repo, number, attempts = attempt, "review failed");

    // Two ways to report the same thing, and which one applies depends on how
    // far the review got. If a check is already open, concluding it in place is
    // both cheaper and correct — a fresh POST would leave the open one pending
    // and refuse auto-merge forever. If the review died before it had a SHA,
    // there is nothing to conclude and the failure has to open its own check.
    let opened = slot.lock().expect("status slot").is_some();
    if opened {
        close_status(&state, &slot, Conclusion::Failed(&err)).await;
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
    if let Some(delivery) = delivery
        && let Err(release) = state.store.release_delivery(&delivery).await
    {
        tracing::error!(%release, %delivery, "could not release the failed delivery claim");
    }
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
async fn review_inner(
    state: &AppState,
    repo: &str,
    number: u64,
    author: &str,
    installation: u64,
    run: &Run,
) -> Result<Option<usize>> {
    let who = state.store.contributor(author).await?;
    if who.trust == Trust::Blocked {
        tracing::info!(%author, "blocked contributor; not reviewing");
        return Ok(None);
    }

    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    let read_token = state.auth.review_read_token(installation).await?;
    let forge = crate::forge::github::GitHubRead::new(&read_token)?;

    let pull_request = {
        use crate::ports::forge::ForgeRead;
        forge.pull_request(&repo_id, number).await?
    };

    // Comment and review-comment deliveries do not carry the draft flag, so
    // routing alone cannot prevent a manual-looking request from waking the
    // workflow. Read the live state before claiming a lease, indexing, calling
    // a model, or minting a write token; drafts are recorded through delivery
    // claims and start their first workflow only once GitHub says they are
    // ready for review.
    if pull_request.draft {
        tracing::debug!(%repo, number, "tracking draft pull request without reviewing");
        return Ok(None);
    }

    // Indexing is kicked off here and deliberately not awaited. A cold full
    // index takes minutes; a review is expected in seconds. The review runs
    // against whatever the index holds right now, and `crate::retrieve` says so
    // in the check-run summary when that is nothing. See `server::indexing`.
    //
    // This uses the *deployment's* configuration, not the repository's own
    // overlay fetched below — a pre-existing tradeoff this pull request does
    // not change. Memory ingestion, spawned after the overlay below, does not
    // repeat it: `paths.ignore` is repository-overridable, and starting
    // ingestion before the overlay is read would persist paths the repository
    // explicitly excluded into an external store the deployment does not own.
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

    // Nothing lease-held starts on a spent budget. Before this point the run
    // has only read metadata and holds nothing another worker could want.
    run.check(repo, number)?;

    // A manual review deliberately takes a lease of its own: the operator asked
    // for this run *because* the ordinary one already happened, so sharing the
    // webhook path's key would make the button a silent no-op.
    let lease = match run.mode {
        Mode::Incremental => format!("{repo}#{number}@{}", pull_request.head_sha),
        Mode::Full => format!("{repo}#{number}@{}!full", pull_request.head_sha),
    };
    if !state.store.claim_lease(&lease, "server").await? {
        tracing::debug!(%lease, "another worker holds this review");
        return Ok(None);
    }

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
    let outcome = {
        let go_on = open_status(
            state,
            &run.slot,
            &repo_id,
            &pull_request.head_sha,
            installation,
        )
        .await;
        if !go_on {
            // Declined, not failed: the check is already concluded as
            // failed, and an `Err` here would have `handle_review` post a
            // second one. The lease goes back like any other outcome; the
            // next push, or the manual review the check points at, reviews
            // this commit properly.
            tracing::info!(%repo, number, "shutting down; not starting this review");
            if let Err(err) = state.store.release_lease(&lease).await {
                tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
            }
            drop(permit);
            return Ok(None);
        }

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

        // Memory is fed from the *base* tip, not the head: what the
        // repository has committed to, not what this pull request proposes.
        // See `server::memory`. Spawned only now, under `overlay.config`
        // rather than the deployment's own, so a repository's own
        // `paths.ignore` — which is repository-overridable — is honored
        // before anything from an excluded path is persisted into the
        // engine.
        //
        // Skipped entirely when the overlay could not be read or applied:
        // `overlay.config` is then only a fallback, not the repository's
        // actual policy, and ingesting under it risks persisting paths the
        // repository excludes. A later delivery for the same base tip that
        // successfully loads the real overlay still ingests normally — this
        // delivery just does not, rather than ingesting under a policy that
        // might be wrong.
        //
        // And only from the default branch. Memory is repository-wide, so a
        // pull request against a release branch must not replace `main`'s
        // snapshot, and an older base must not roll the memory backwards; the
        // ingest forgets a section before rewriting it, so either would.
        if overlay.unavailable {
            tracing::warn!(
                %repo,
                "skipping memory ingestion: the repository's own configuration could not be read"
            );
        } else if let Some(backend) = &state.memory {
            let default_branch = {
                use crate::ports::forge::ForgeRead;
                forge.default_branch(&repo_id).await
            };
            match default_branch {
                Ok(branch) if branch == pull_request.base_ref => {
                    tokio::spawn(ingest_in_background(
                        backend.clone(),
                        Arc::new(overlay.config.clone()),
                        state.index_permits.clone(),
                        repo_id.clone(),
                        pull_request.base_sha.clone(),
                        read_token.clone(),
                    ));
                }
                Ok(branch) => tracing::debug!(
                    %repo,
                    base = %pull_request.base_ref,
                    default = %branch,
                    "skipping memory ingestion: the base is not the default branch"
                ),
                Err(err) => tracing::warn!(
                    %repo,
                    %err,
                    "skipping memory ingestion: could not read the default branch"
                ),
            }
        }

        // `AssertUnwindSafe` + `catch_unwind` so a panic inside a lane still
        // reaches the release below. Without it the `?` on the outcome is not
        // the only way out — an unwind skips everything — and the lease
        // survives the worker that took it. The boundary check first: the
        // metadata phase above was bounded per call, not by the deadline, so
        // this is where a budget it exhausted is noticed.
        let lanes = match run.check(repo, number) {
            Ok(()) => std::panic::AssertUnwindSafe(run_lanes(
                state,
                &overlay.config,
                &repo_id,
                number,
                &forge,
                &read_token,
                run,
            ))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(Error::lane("review", "the review panicked"))),
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
        let outcome = match lanes {
            Ok((config, proposal)) => {
                // `AssertUnwindSafe` + `catch_unwind` here too, same reason as
                // around `run_lanes`: without it a panic inside `apply` skips
                // `release_lease` and `drop(permit)` below and escapes
                // `handle_review` entirely, deregistering the `InFlight` slot
                // on the way out (`Drop` always runs) without ever concluding
                // its check — a lease held until `LEASE_TTL` and a check stuck
                // "in progress" forever, which is exactly what this whole
                // umbrella-check mechanism exists to prevent.
                let publish = std::panic::AssertUnwindSafe(async {
                    let write_token = state.auth.installation_token(installation).await?;
                    let write = crate::forge::github::GitHubWrite::new(&write_token)?;
                    crate::app::apply(&forge, &write, &config, &proposal, Some(&state.store)).await
                })
                .catch_unwind();
                tokio::time::timeout(PUBLISH_DEADLINE, publish)
                    .await
                    .map_err(|_elapsed| {
                        Error::timeout(
                            format!("publishing the review of {repo}#{number}"),
                            PUBLISH_DEADLINE,
                        )
                    })
                    .and_then(|published| {
                        published
                            .unwrap_or_else(|_| Err(Error::lane("review", "publishing panicked")))
                    })
                    .map(|()| proposal)
            }
            Err(err) => Err(err),
        };

        // Released regardless of how the review went. The TTL in the store is
        // the backstop for the cases this cannot cover — a kill, or a lost
        // machine.
        if let Err(err) = state.store.release_lease(&lease).await {
            tracing::error!(%err, %lease, "could not release the lease; it will expire on its own");
        }
        drop(permit);

        outcome
    };

    let proposal = outcome?;
    let findings = proposal.findings().count();
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

    Ok(Some(findings))
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
async fn run_lanes(
    state: &AppState,
    config: &Config,
    repo: &RepoId,
    number: u64,
    forge: &crate::forge::github::GitHubRead,
    read_token: &str,
    run: &Run,
) -> Result<(Config, crate::app::Proposal)> {
    let model = Arc::new(crate::harness::openrouter::GatewayModel::from_config(
        &state.config.config.models,
    )?);

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
    // same policy with no memory, not the deployment's policy instead.
    let recaller = state.memory.as_ref().map(|backend| backend.recaller());

    let config = config_for(config, run.mode);

    // The deadline bounds the checkout and the lanes — everything that can
    // take minutes and nothing that must not be cut short. `timeout_at` drops
    // the inner future when it elapses, which cancels every model call in
    // flight. The checkout sits inside it so that a deadline already in the
    // past — a retry after a slow failure — resolves at once, before a clone
    // is even started, which is the intended way of refusing the retry.
    let review = async {
        // The tree the reviewers may look things up in. A shallow checkout of
        // the head when `[lookup].checkout` allows it — one commit, no history,
        // no hooks, the same fetch the indexer makes — so search works and a
        // read costs no API call; the forge reader behind it for what a shallow
        // checkout lacks, such as a submodule that was not fetched. Read-only
        // either way: the token here is the review-read one the forge already
        // holds, and the checkout is deleted with the review.
        let checkout = if config.lookup.enabled && config.lookup.checkout {
            let head = forge.pull_request(repo, number).await?.head_sha;
            match crate::indexer::fetch::Checkout::fetch(
                &super::indexing::git_host(),
                &repo.to_string(),
                &head,
                read_token,
            )
            .await
            {
                Ok(checkout) => {
                    if !config.retrieval.submodules.is_empty()
                        && let Err(err) = checkout
                            .fetch_submodules(
                                &super::indexing::git_host(),
                                read_token,
                                &config.retrieval.submodules,
                            )
                            .await
                    {
                        tracing::warn!(%repo, %err, "submodules not fetched for the review's tree");
                    }
                    Some(checkout)
                }
                Err(err) => {
                    tracing::warn!(%repo, %err, "no checkout for the review; lookups read through the forge");
                    None
                }
            }
        } else {
            None
        };
        // The review chains the forge reader behind whatever it is given, so a
        // path the shallow checkout lacks — a submodule that was not fetched —
        // is still read through the API.
        let dir_tree = checkout
            .as_ref()
            .map(|c| crate::ports::tree::DirTree::new(c.path()).at_revision(c.revision()));
        let tree = dir_tree
            .as_ref()
            .map(|dir| dir as &dyn crate::ports::tree::TreeReader);

        crate::app::review::review_with_tree(
            forge,
            model,
            &config,
            repo,
            number,
            Some(&state.store),
            state.knowledge.as_deref(),
            retriever.as_ref(),
            recaller.as_ref(),
            tree,
        )
        .await
    };
    let proposal = tokio::time::timeout_at(run.deadline, review)
        .await
        .map_err(|_elapsed| {
            Error::timeout(format!("the review of {repo}#{number}"), REVIEW_DEADLINE)
        })??;

    Ok((config.into_owned(), proposal))
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
        let model = Arc::new(crate::harness::openrouter::GatewayModel::from_config(
            &effective.models,
        )?);
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
            let model = Arc::new(crate::harness::openrouter::GatewayModel::from_config(
                &config.models,
            )?);
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
            let (model, model_id, vision): (Arc<dyn crate::ports::model::Model>, &str, bool) =
                match config.model_for_vision() {
                    Some(vision) => (
                        Arc::new(crate::harness::openrouter::GatewayModel::for_vision(
                            &config.models,
                        )?),
                        vision,
                        true,
                    ),
                    None => (
                        Arc::new(crate::harness::openrouter::GatewayModel::from_config(
                            &config.models,
                        )?),
                        config.model_for_workload(crate::config::types::Workload::Preview),
                        false,
                    ),
                };
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
    async fn a_phase_boundary_refuses_a_spent_budget_and_passes_a_live_one() {
        // The lease claim and the lanes both consult this before starting.
        // A spent budget must stop the run *before* it holds anything, and
        // must surface as the same non-transient timeout the lanes report.
        let slot: StatusSlot = Arc::new(std::sync::Mutex::new(None));
        let now = tokio::time::Instant::now();
        let spent = Run {
            mode: Mode::Incremental,
            slot: slot.clone(),
            deadline: now
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap_or(now),
        };
        let err = spent
            .check("o/r", 7)
            .expect_err("a spent budget must refuse");
        assert!(matches!(err, Error::Timeout { ref what, .. } if what == "the review of o/r#7"));
        assert!(!failure::is_transient(&err));

        let live = Run {
            mode: Mode::Incremental,
            slot,
            deadline: now + REVIEW_DEADLINE,
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
