//! The manual, full re-review escape hatch.
//!
//! Feature-gated behind `serve` along with the rest of `src/server`.
//!
//! Normal operation is webhook-driven and incremental. This module is the
//! rarely-used button for when a review has to be redone *wholesale*: it
//! enqueues a review that ignores everything the incremental path remembers.
//!
//! It is mounted beside the admin API and behind the same bearer token, in a
//! `route_layer`, so an unauthenticated request never gets its body parsed. Like
//! the admin router it is simply absent when no token is configured.
//!
//! The target repository is checked against one organisation here, in the
//! server. The caller — a `workflow_dispatch` button — checks it too, but a
//! workflow input is editable by anyone who can dispatch it, so the client's
//! check is a convenience and this one is the control.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::Result;
use crate::forge::RepoId;
use crate::server::admin::AdminAuth;

/// Environment variable naming the organisation manual reviews may target.
pub const ORG_ENV: &str = "TINYSWEEPER_ALLOWED_ORG";

/// The organisation manual reviews target when the environment says nothing.
pub const DEFAULT_ORG: &str = "tinyhumansai";

/// Which organisation the deployment allows, from the environment.
///
/// The read is kept apart from [`allowed_org_from`] so the policy can be tested
/// without touching process-wide state.
pub fn allowed_org() -> String {
    allowed_org_from(std::env::var(ORG_ENV).ok().as_deref())
}

/// Which organisation a configured value names.
///
/// An unset *or empty* value means the default: exporting a variable with no
/// value is what copying `.env.example` produces, and silently allowing every
/// organisation because of it would be the wrong way to fail.
pub fn allowed_org_from(configured: Option<&str>) -> String {
    match configured.map(str::trim) {
        Some(org) if !org.is_empty() => org.to_string(),
        _ => DEFAULT_ORG.to_string(),
    }
}

/// The target repository, if it is one this deployment will review manually.
///
/// `Err` carries the message the operator sees. Comparison is case-insensitive
/// because GitHub logins are, and a refusal over letter case would look like a
/// permissions bug.
pub fn checked_target(
    owner: &str,
    name: &str,
    allowed: &str,
) -> std::result::Result<RepoId, String> {
    let owner = owner.trim();
    let name = name.trim();

    if !owner.eq_ignore_ascii_case(allowed.trim()) {
        return Err(format!(
            "manual reviews are restricted to the `{allowed}` organisation; `{owner}` is not it"
        ));
    }

    RepoId::parse(&format!("{owner}/{name}"))
        .ok_or_else(|| format!("`{owner}/{name}` is not a plausible owner/name"))
}

/// What a manual review request may say beyond the repository in its path.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct ManualReviewRequest {
    /// One pull request to review. Absent means every open pull request in the
    /// repository, which is the "redo this repository wholesale" case.
    #[serde(default)]
    pub number: Option<u64>,
}

/// How the route reaches the reviewer.
///
/// A trait rather than a direct call into `crate::server::routes` so the route
/// can be tested without a database, a GitHub App key or a model: what the
/// tests need to assert is *which* review would be queued, and this is the
/// seam that carries exactly that.
#[async_trait]
pub trait FullReviews: Send + Sync {
    /// Queue a full review, returning the pull request numbers queued.
    ///
    /// `number` is `None` for every open pull request in the repository. The
    /// call resolves what to review and then returns; the reviews themselves
    /// run off the request path, because one takes minutes.
    async fn enqueue(&self, repo: &RepoId, number: Option<u64>) -> Result<Vec<u64>>;
}

/// How the route reaches the auto-merge policy.
///
/// A second trait rather than a method on [`FullReviews`] because the two jobs
/// have nothing in common but a repository: a review spends money and posts
/// comments, an evaluation is arithmetic over state GitHub already holds.
#[async_trait]
pub trait Merges: Send + Sync {
    /// Evaluate pull requests against `[automerge]`, merging those that
    /// qualify, and report what happened to each.
    ///
    /// `number` is `None` for every open pull request in the repository, which
    /// is the sweep an operator runs after changing the policy. Unlike
    /// [`FullReviews::enqueue`] this waits for the answer: there is no model in
    /// this path, so it is fast enough to report, and "which ones did it refuse
    /// and why" is the entire reason to press the button by hand.
    async fn evaluate(&self, repo: &RepoId, number: Option<u64>) -> Result<Vec<MergeReport>>;
}

/// What the policy decided about one pull request.
#[derive(Debug, Clone, Serialize)]
pub struct MergeReport {
    /// The pull request.
    pub number: u64,
    /// `merged`, `refused` or `rejected`.
    pub outcome: &'static str,
    /// The refusal or the forge's complaint, rendered for a human. `None` on a
    /// merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What the manual routes need.
#[derive(Clone)]
struct ManualState {
    /// The one organisation this deployment will review on request.
    allowed_org: Arc<str>,
    reviews: Arc<dyn FullReviews>,
    merges: Arc<dyn Merges>,
}

/// Build the manual review router, or nothing when no token is configured.
///
/// `None` is the fail-closed half, exactly as in `crate::server::admin`: a
/// deployment without an admin credential loses the button rather than exposing
/// an unauthenticated way to spend money on model calls.
pub fn router(
    auth: Option<AdminAuth>,
    allowed_org: String,
    reviews: Arc<dyn FullReviews>,
    merges: Arc<dyn Merges>,
) -> Option<Router> {
    let auth = Arc::new(auth?);
    let state = ManualState {
        allowed_org: allowed_org.into(),
        reviews,
        merges,
    };

    Some(
        Router::new()
            .route("/admin/reviews/{owner}/{name}", post(full_review))
            .route("/admin/merges/{owner}/{name}", post(auto_merge))
            // `route_layer`, so the token is checked before the `Json`
            // extractor parses anything an anonymous caller sent.
            .route_layer(axum::middleware::from_fn_with_state(
                auth,
                crate::server::admin::guard,
            ))
            .with_state(state),
    )
}

/// An error shaped like the rest of the admin API's JSON.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

/// Queue a review that ignores everything the incremental path remembers.
async fn full_review(
    State(state): State<ManualState>,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<ManualReviewRequest>,
) -> std::result::Result<Response, ApiError> {
    let repo = checked_target(&owner, &name, &state.allowed_org)
        .map_err(|message| ApiError(StatusCode::FORBIDDEN, message))?;

    let queued = state
        .reviews
        .enqueue(&repo, body.number)
        .await
        // Whatever went wrong here is GitHub or the deployment, not the
        // request: the request was already accepted by the two checks above.
        .map_err(|err| ApiError(StatusCode::BAD_GATEWAY, err.to_string()))?;

    // Logged deliberately. A manual review is an operator action that costs
    // money and posts comments, so it should be reconstructable from the logs.
    tracing::info!(%repo, ?queued, "queued a full review on request");

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "repo": repo.to_string(),
            "mode": "full",
            "queued": queued,
            "note": "these reviews ignore the incremental state: no stored evidence, \
                     no remembered fingerprints, and every finding is proposed afresh",
        })),
    )
        .into_response())
}

/// Evaluate pull requests against `[automerge]` and merge those that qualify.
///
/// Behind the same credential and the same organisation check as the review
/// button above, and for a stronger reason: this one can put code on the
/// default branch. It adds no authority of its own — the deterministic policy
/// in `crate::automerge` decides, exactly as it does on a webhook — so the
/// button is a way to ask *now* rather than a way to ask for more.
async fn auto_merge(
    State(state): State<ManualState>,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<ManualReviewRequest>,
) -> std::result::Result<Response, ApiError> {
    let repo = checked_target(&owner, &name, &state.allowed_org)
        .map_err(|message| ApiError(StatusCode::FORBIDDEN, message))?;

    let reports = state
        .merges
        .evaluate(&repo, body.number)
        .await
        .map_err(|err| ApiError(StatusCode::BAD_GATEWAY, err.to_string()))?;

    let merged: Vec<u64> = reports
        .iter()
        .filter(|report| report.outcome == "merged")
        .map(|report| report.number)
        .collect();

    // Logged at info even when nothing merged. An operator pressing this is
    // an action on the default branch, and the refusals are the useful half of
    // the record — "why did it not merge" is the question that gets asked.
    tracing::info!(%repo, ?merged, considered = reports.len(), "evaluated auto-merge on request");

    Ok((
        StatusCode::OK,
        Json(json!({
            "repo": repo.to_string(),
            "merged": merged,
            "results": reports,
        })),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Mutex;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// Records what the route asked for, so the tests assert on the request
    /// that would have been queued rather than on a mock's call count.
    #[derive(Default)]
    struct Recorder {
        queued: Mutex<Vec<(String, Option<u64>)>>,
    }

    #[async_trait::async_trait]
    impl FullReviews for Recorder {
        async fn enqueue(
            &self,
            repo: &RepoId,
            number: Option<u64>,
        ) -> crate::error::Result<Vec<u64>> {
            self.queued
                .lock()
                .expect("not poisoned")
                .push((repo.to_string(), number));
            Ok(number.into_iter().collect())
        }
    }

    /// Records which pull requests the merge route asked about, and answers
    /// with a refusal — the outcome that matters, since a route that reports
    /// only successes hides the half operators press the button for.
    #[derive(Default)]
    struct MergeRecorder {
        asked: Mutex<Vec<(String, Option<u64>)>>,
    }

    #[async_trait::async_trait]
    impl Merges for MergeRecorder {
        async fn evaluate(
            &self,
            repo: &RepoId,
            number: Option<u64>,
        ) -> crate::error::Result<Vec<MergeReport>> {
            self.asked
                .lock()
                .expect("not poisoned")
                .push((repo.to_string(), number));
            Ok(vec![
                MergeReport {
                    number: 7,
                    outcome: "merged",
                    detail: None,
                },
                MergeReport {
                    number: 9,
                    outcome: "refused",
                    detail: Some("it is a draft".into()),
                },
            ])
        }
    }

    fn app(reviews: Arc<Recorder>) -> axum::Router {
        app_with(reviews, Arc::new(MergeRecorder::default()))
    }

    fn app_with(reviews: Arc<Recorder>, merges: Arc<MergeRecorder>) -> axum::Router {
        router(
            Some(AdminAuth::new(TOKEN).expect("a long enough token")),
            "tinyhumansai".into(),
            reviews,
            merges,
        )
        .expect("a token mounts the router")
    }

    #[tokio::test]
    async fn an_unauthenticated_merge_sweep_is_refused() {
        // The route that can put code on the default branch gets the same
        // guard as the one that only spends money, checked the same way.
        let merges = Arc::new(MergeRecorder::default());
        let response = app_with(Arc::new(Recorder::default()), merges.clone())
            .oneshot(post(
                "/admin/merges/tinyhumansai/tinysweeper",
                None,
                "{ this is not json",
            ))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(merges.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_merge_sweep_outside_the_organisation_is_refused() {
        let merges = Arc::new(MergeRecorder::default());
        let response = app_with(Arc::new(Recorder::default()), merges.clone())
            .oneshot(post("/admin/merges/someone-else/their-repo", Some(TOKEN), "{}"))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            merges.asked.lock().unwrap().is_empty(),
            "nothing outside the organisation may be merged"
        );
    }

    #[tokio::test]
    async fn a_merge_sweep_reports_the_refusals_as_well_as_the_merges() {
        let merges = Arc::new(MergeRecorder::default());
        let response = app_with(Arc::new(Recorder::default()), merges.clone())
            .oneshot(post("/admin/merges/tinyhumansai/tinysweeper", Some(TOKEN), "{}"))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            *merges.asked.lock().unwrap(),
            vec![("tinyhumansai/tinysweeper".to_string(), None)]
        );

        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("a body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["merged"], serde_json::json!([7]));
        // The refusal and its reason survive to the operator. A sweep that
        // reported only what it merged would leave "why not the other one?"
        // answerable solely from the server's log.
        assert_eq!(json["results"][1]["outcome"], "refused");
        assert_eq!(json["results"][1]["detail"], "it is a draft");
    }

    #[tokio::test]
    async fn a_merge_sweep_may_name_one_pull_request() {
        let merges = Arc::new(MergeRecorder::default());
        let response = app_with(Arc::new(Recorder::default()), merges.clone())
            .oneshot(post(
                "/admin/merges/tinyhumansai/tinysweeper",
                Some(TOKEN),
                "{\"number\":42}",
            ))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            *merges.asked.lock().unwrap(),
            vec![("tinyhumansai/tinysweeper".to_string(), Some(42))]
        );
    }

    fn post(path: &str, token: Option<&str>, body: &'static str) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            // Assembled at runtime: a literal `Bearer <token>` in a fixture is
            // credential-shaped and push protection has rejected one here.
            request = request.header("authorization", format!("Bearer {token}"));
        }
        request.body(Body::from(body)).expect("a request")
    }

    #[tokio::test]
    async fn no_token_configured_means_no_manual_review_route_at_all() {
        assert!(
            router(
                None,
                "tinyhumansai".into(),
                Arc::new(Recorder::default()),
                Arc::new(MergeRecorder::default())
            )
            .is_none(),
            "an unauthenticated way to spend money on reviews is not a supported deployment"
        );
    }

    #[tokio::test]
    async fn an_unauthenticated_request_is_refused_before_its_body_is_parsed() {
        let reviews = Arc::new(Recorder::default());
        // Unparseable on purpose: a 400 here would prove the JSON extractor ran
        // before the guard, which is work done on behalf of an attacker.
        let response = app(reviews.clone())
            .oneshot(post(
                "/admin/reviews/tinyhumansai/tinysweeper",
                None,
                "{ this is not json",
            ))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(reviews.queued.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_wrong_token_is_refused() {
        let reviews = Arc::new(Recorder::default());
        let response = app(reviews.clone())
            .oneshot(post(
                "/admin/reviews/tinyhumansai/tinysweeper",
                Some("0123456789abcdef0123456789abcdeg"),
                "{}",
            ))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(reviews.queued.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_repository_outside_the_organisation_is_refused_by_the_route_itself() {
        // The workflow checks this too, but a workflow input is editable by
        // anyone who can dispatch it, so this is the check that counts.
        let reviews = Arc::new(Recorder::default());
        let response = app(reviews.clone())
            .oneshot(post(
                "/admin/reviews/someone-else/their-repo",
                Some(TOKEN),
                "{}",
            ))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            reviews.queued.lock().unwrap().is_empty(),
            "nothing outside the organisation may be queued"
        );

        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("a body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        let message = json["error"].as_str().unwrap_or_default();
        assert!(message.contains("tinyhumansai"), "{message}");
    }

    #[tokio::test]
    async fn an_authenticated_request_queues_a_full_review_of_one_pull_request() {
        let reviews = Arc::new(Recorder::default());
        let response = app(reviews.clone())
            .oneshot(post(
                "/admin/reviews/tinyhumansai/tinysweeper",
                Some(TOKEN),
                "{\"number\":12}",
            ))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            *reviews.queued.lock().unwrap(),
            vec![("tinyhumansai/tinysweeper".to_string(), Some(12))]
        );

        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("a body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["mode"], "full");
        assert_eq!(json["queued"], serde_json::json!([12]));
    }

    #[tokio::test]
    async fn a_request_without_a_number_asks_for_the_whole_repository() {
        let reviews = Arc::new(Recorder::default());
        let response = app(reviews.clone())
            .oneshot(post(
                "/admin/reviews/tinyhumansai/tinysweeper",
                Some(TOKEN),
                "{}",
            ))
            .await
            .expect("a response");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            *reviews.queued.lock().unwrap(),
            vec![("tinyhumansai/tinysweeper".to_string(), None)]
        );
    }

    #[test]
    fn a_repository_inside_the_organisation_is_accepted() {
        let repo = checked_target("tinyhumansai", "tinysweeper", "tinyhumansai").expect("accepted");
        assert_eq!(repo.to_string(), "tinyhumansai/tinysweeper");
    }

    #[test]
    fn the_organisation_check_ignores_letter_case() {
        // GitHub logins are case-insensitive, so refusing `TinyHumansAI` would
        // look like a permissions bug rather than a policy.
        assert!(checked_target("TinyHumansAI", "tinysweeper", "tinyhumansai").is_ok());
    }

    #[test]
    fn a_repository_outside_the_organisation_is_refused_with_a_clear_message() {
        let err = checked_target("someone-else", "tinysweeper", "tinyhumansai").expect_err("bad");
        assert!(
            err.contains("restricted to the `tinyhumansai` organisation"),
            "{err}"
        );
        assert!(err.contains("someone-else"), "{err}");
    }

    #[test]
    fn a_lookalike_owner_does_not_pass_as_the_organisation() {
        // Prefix and suffix matching are the two ways this check is usually got
        // wrong, and either one hands the button to an attacker's account.
        for owner in ["tinyhumansai-evil", "eviltinyhumansai", "tinyhumansai.evil"] {
            assert!(
                checked_target(owner, "tinysweeper", "tinyhumansai").is_err(),
                "accepted `{owner}`"
            );
        }
    }

    #[test]
    fn a_malformed_repository_name_is_refused() {
        assert!(checked_target("tinyhumansai", "", "tinyhumansai").is_err());
        assert!(checked_target("tinyhumansai", "a/b", "tinyhumansai").is_err());
    }

    #[test]
    fn an_unset_or_empty_organisation_falls_back_to_the_default() {
        assert_eq!(allowed_org_from(None), DEFAULT_ORG);
        assert_eq!(allowed_org_from(Some("   ")), DEFAULT_ORG);
        assert_eq!(allowed_org_from(Some(" acme ")), "acme");
    }
}
