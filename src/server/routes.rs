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

use crate::config::Config;
use crate::error::{Error, Result};
use crate::forge::RepoId;
use crate::server::auth::AppAuth;
use crate::server::store::{Store, Trust};
use crate::server::webhook::{self, Action, Payload};

/// How many reviews may run at once.
///
/// Each one holds a model call open for minutes, so the limit is about spend
/// and rate limits rather than CPU. Keep it low: a repository-wide force-push
/// delivers a burst, and an unbounded worker pool turns that into an unbounded
/// bill.
const MAX_CONCURRENT_REVIEWS: usize = 4;

/// How the server was configured.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind.
    pub bind: String,
    /// Shared secret GitHub signs deliveries with.
    pub webhook_secret: String,
    /// Review configuration used when a repository has no file of its own.
    pub config: Config,
    /// The embedding signature retrieval runs under, when retrieval is enabled.
    ///
    /// `None` disables retrieval entirely, which is a supported deployment.
    pub embedding: Option<EmbedSignature>,
}

/// Everything a handler needs.
#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
    store: Store,
    auth: Arc<AppAuth>,
    permits: Arc<Semaphore>,
}

/// Run the server until the process is stopped.
pub async fn serve(config: ServerConfig, store: Store, auth: AppAuth) -> Result<()> {
    let bind = config.bind.clone();
    let state = AppState {
        config: Arc::new(config),
        store,
        auth: Arc::new(auth),
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEWS)),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/webhook", post(receive))
        .with_state(state);

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
            tokio::spawn(handle_review(state, repo, number, author, installation));
            (StatusCode::ACCEPTED, "queued").into_response()
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
) {
    if let Err(err) = review_inner(&state, &repo, number, &author, installation).await {
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

    let lease = format!("{repo}#{number}@{}", pull_request.head_sha);
    if !state.store.claim_lease(&lease, "server").await? {
        tracing::debug!(%lease, "another worker holds this review");
        return Ok(());
    }

    // `AssertUnwindSafe` + `catch_unwind` so a panic inside a lane still
    // reaches the release below. Without it the `?` on the outcome is not the
    // only way out — an unwind skips everything — and the lease survives the
    // worker that took it.
    let outcome = std::panic::AssertUnwindSafe(run_and_publish(
        state,
        &repo_id,
        number,
        installation,
        &forge,
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

    Ok(())
}

async fn run_and_publish(
    state: &AppState,
    repo: &RepoId,
    number: u64,
    installation: u64,
    forge: &crate::forge::github::GitHubRead,
) -> Result<crate::app::Proposal> {
    let model = Arc::new(crate::harness::openrouter::GatewayModel::from_config(
        &state.config.config.models,
    )?);

    // The model runs against a read-only handle. The write token is minted
    // below, after this returns — same boundary as the workflow, same reason.
    let proposal = crate::app::review(forge, model, &state.config.config, repo, number).await?;

    let write_token = state.auth.installation_token(installation).await?;
    let write = crate::forge::github::GitHubWrite::new(&write_token)?;
    crate::app::apply(forge, &write, &proposal).await?;

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
}
