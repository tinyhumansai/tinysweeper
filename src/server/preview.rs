//! The `/preview` routes: how the hands in a repository's CI reach the brain.
//!
//! Feature-gated behind `serve` with the rest of `src/server`.
//!
//! Three routes, one session:
//!
//! - `POST /preview/sessions` opens a session for one pull request head and
//!   answers with the planned flows.
//! - `POST /preview/sessions/{id}/flows/{flow}/step` hands over what the
//!   browser sees and gets the next commands.
//! - `POST /preview/sessions/{id}/finish` hands over the manifest of what was
//!   uploaded and has the comment published.
//!
//! The door is a bearer token, `TINYSWEEPER_PREVIEW_TOKEN`, checked in a
//! `route_layer` before any body is parsed — the same door as `/admin`, with a
//! token of its own because this one is handed to every repository's CI and
//! the admin token must never be. Unset, the router is not mounted at all.
//!
//! The token proves "a CI job in an organisation we serve". It does not prove
//! which pull request, so the session start reads the pull request through
//! the GitHub App and refuses to open unless the head commit named matches
//! the one on GitHub. Everything after that is bounded by the session: a
//! caller cannot step a flow that was not planned or finish with a manifest
//! for another commit.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::preview::types::{Command, Flow, Manifest, Observation};
use crate::server::admin::AdminAuth;

/// Environment variable carrying the preview bearer token.
pub const TOKEN_ENV: &str = "TINYSWEEPER_PREVIEW_TOKEN";

/// The largest request body the routes accept.
///
/// An accessibility snapshot of a busy page is tens of kilobytes; a manifest
/// is a few. Anything past this is not a page, it is a payload.
pub const MAX_BODY_BYTES: usize = 512 * 1024;

/// What the hands say to open a session.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StartRequest {
    /// `owner/name`.
    pub repo: String,
    /// The pull request number.
    pub pull_request: u64,
    /// The head commit the hands built. Must match GitHub's.
    pub head_sha: String,
    /// The merge-base the hands built.
    pub base_sha: String,
    /// The application's entry points, `(name, path)`, from its config.
    #[serde(default)]
    pub entry_points: Vec<(String, String)>,
}

/// The answer to a session start.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StartReply {
    /// Whether previews are on for this repository. When false, nothing
    /// else is set and the hands should stop.
    pub enabled: bool,
    /// The session id to present on every later call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The flows to drive, in order. Empty when the diff has no UI change.
    #[serde(default)]
    pub flows: Vec<Flow>,
    /// The step ceiling the hands should also enforce.
    #[serde(default)]
    pub max_steps: usize,
}

/// The answer to a step.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StepReply {
    /// The commands to run next.
    pub commands: Vec<Command>,
    /// Whether this batch ends the flow on this side.
    pub done: bool,
    /// When the `after` side is done: the whole script, for the `before`
    /// side to replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<Vec<Command>>,
}

/// What the hands say when they are finished.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FinishRequest {
    /// What was uploaded.
    pub manifest: Manifest,
}

/// The answer to a finish.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FinishReply {
    /// What publishing did, as `preview::apply::Outcome` spells it.
    pub outcome: String,
}

/// How the routes reach the brain.
///
/// A trait rather than a direct call into `crate::server::routes`, for the
/// reason `manual::FullReviews` is one: the routes are tested without a
/// database, an App key or a model, against what they would have asked for.
#[async_trait]
pub trait Previews: Send + Sync {
    /// Open a session, planning the flows.
    async fn start(&self, request: StartRequest) -> Result<StartReply>;
    /// Decide the next commands for one flow.
    async fn step(&self, session: &str, flow: &str, observation: Observation) -> Result<StepReply>;
    /// Validate the manifest and publish.
    async fn finish(&self, session: &str, request: FinishRequest) -> Result<FinishReply>;
}

#[derive(Clone)]
struct PreviewState {
    previews: Arc<dyn Previews>,
}

/// Build the router, or `None` when there is no token to guard it with.
pub fn router(auth: Option<AdminAuth>, previews: Arc<dyn Previews>) -> Option<Router> {
    let auth = Arc::new(auth?);
    let state = PreviewState { previews };
    Some(
        Router::new()
            .route("/preview/sessions", post(start))
            .route("/preview/sessions/{id}/flows/{flow}/step", post(step))
            .route("/preview/sessions/{id}/finish", post(finish))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
            // `route_layer`, so the token is checked before the `Json`
            // extractor parses anything an anonymous caller sent.
            .route_layer(axum::middleware::from_fn_with_state(
                auth,
                crate::server::admin::guard,
            ))
            .with_state(state),
    )
}

async fn start(
    State(state): State<PreviewState>,
    Json(request): Json<StartRequest>,
) -> Response {
    match state.previews.start(request).await {
        Ok(reply) => (StatusCode::OK, Json(reply)).into_response(),
        Err(err) => failure(err),
    }
}

async fn step(
    State(state): State<PreviewState>,
    Path((id, flow)): Path<(String, String)>,
    Json(observation): Json<Observation>,
) -> Response {
    if !is_id(&id) || !is_id(&flow) {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such session"}))).into_response();
    }
    match state.previews.step(&id, &flow, observation).await {
        Ok(reply) => (StatusCode::OK, Json(reply)).into_response(),
        Err(err) => failure(err),
    }
}

async fn finish(
    State(state): State<PreviewState>,
    Path(id): Path<String>,
    Json(request): Json<FinishRequest>,
) -> Response {
    if !is_id(&id) {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such session"}))).into_response();
    }
    match state.previews.finish(&id, request).await {
        Ok(reply) => (StatusCode::OK, Json(reply)).into_response(),
        Err(err) => failure(err),
    }
}

/// A path segment that could be a session or flow id.
fn is_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The status a failure maps to.
///
/// `Config` is the caller's fault — a mismatched commit, a malformed manifest
/// — and `Forge` is ours or GitHub's. The hands retry the second kind and
/// give up on the first, so the distinction is the whole point.
fn failure(err: Error) -> Response {
    let status = match &err {
        Error::Config(_) | Error::ConfigNotFound(_) | Error::Json(_) => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    tracing::warn!(%err, "preview request failed");
    (status, Json(json!({"error": err.to_string()}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::types::Side;

    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Default)]
    struct Recorder {
        started: Mutex<Vec<StartRequest>>,
        stepped: Mutex<Vec<(String, String, Observation)>>,
        finished: Mutex<Vec<(String, Manifest)>>,
    }

    #[async_trait]
    impl Previews for Recorder {
        async fn start(&self, request: StartRequest) -> Result<StartReply> {
            if request.head_sha == "moved" {
                return Err(Error::Config("head is not abc".into()));
            }
            self.started.lock().unwrap().push(request);
            Ok(StartReply {
                enabled: true,
                session: Some("s1".into()),
                flows: vec![],
                max_steps: 25,
            })
        }
        async fn step(&self, session: &str, flow: &str, observation: Observation) -> Result<StepReply> {
            self.stepped
                .lock()
                .unwrap()
                .push((session.into(), flow.into(), observation));
            Ok(StepReply {
                commands: vec![Command::Done {
                    reason: "test".into(),
                }],
                done: true,
                replay: None,
            })
        }
        async fn finish(&self, session: &str, request: FinishRequest) -> Result<FinishReply> {
            self.finished
                .lock()
                .unwrap()
                .push((session.into(), request.manifest));
            Ok(FinishReply {
                outcome: "published".into(),
            })
        }
    }

    fn app(recorder: Arc<Recorder>) -> Router {
        router(Some(AdminAuth::new(TOKEN).unwrap()), recorder).unwrap()
    }

    fn post(path: &str, token: Option<&str>, body: &str) -> Request<Body> {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        request.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn no_token_no_router() {
        assert!(router(None, Arc::new(Recorder::default())).is_none());
    }

    #[tokio::test]
    async fn an_unauthenticated_call_is_refused_before_the_body_is_read() {
        let recorder = Arc::new(Recorder::default());
        let response = app(recorder.clone())
            .oneshot(post("/preview/sessions", None, "not even json"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(recorder.started.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_session_start_reaches_the_brain_with_what_the_hands_said() {
        let recorder = Arc::new(Recorder::default());
        let response = app(recorder.clone())
            .oneshot(post(
                "/preview/sessions",
                Some(TOKEN),
                r#"{"repo":"o/r","pull_request":7,"head_sha":"abc","base_sha":"b","entry_points":[["home","/"]]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let started = recorder.started.lock().unwrap();
        assert_eq!(started[0].repo, "o/r");
        assert_eq!(started[0].entry_points, vec![("home".to_string(), "/".to_string())]);
    }

    #[tokio::test]
    async fn a_callers_mistake_is_422_not_503() {
        let response = app(Arc::new(Recorder::default()))
            .oneshot(post(
                "/preview/sessions",
                Some(TOKEN),
                r#"{"repo":"o/r","pull_request":7,"head_sha":"moved","base_sha":"b"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_step_is_routed_by_session_and_flow() {
        let recorder = Arc::new(Recorder::default());
        let response = app(recorder.clone())
            .oneshot(post(
                "/preview/sessions/s1/flows/f1/step",
                Some(TOKEN),
                r#"{"side":"after","url":"http://127.0.0.1:3001/","aria":"- heading \"Hi\"","results":[],"steps":0}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let stepped = recorder.stepped.lock().unwrap();
        assert_eq!(stepped[0].0, "s1");
        assert_eq!(stepped[0].1, "f1");
        assert_eq!(stepped[0].2.side, Side::After);
    }

    #[tokio::test]
    async fn an_implausible_id_is_404_without_reaching_the_brain() {
        let recorder = Arc::new(Recorder::default());
        let response = app(recorder.clone())
            .oneshot(post(
                "/preview/sessions/..%2Fetc/flows/f1/step",
                Some(TOKEN),
                r#"{"side":"after","url":"u","aria":"a"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(recorder.stepped.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_finish_carries_the_manifest() {
        let recorder = Arc::new(Recorder::default());
        let response = app(recorder.clone())
            .oneshot(post(
                "/preview/sessions/s1/finish",
                Some(TOKEN),
                r#"{"manifest":{"version":1,"repo":"o/r","pull_request":7,"head_sha":"abc","base_sha":"b","run":"run-1","flows":[]}}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let finished = recorder.finished.lock().unwrap();
        assert_eq!(finished[0].1.run, "run-1");
    }
}
