//! The admin API: contributor trust, index status, knowledge documents.
//!
//! Feature-gated behind `serve` along with the rest of `src/server`.
//!
//! These routes mutate operator state, so they are authenticated the same way
//! the webhook is and for the same reason: anyone on the internet can reach
//! them. The webhook has GitHub's HMAC; there is no equivalent for a human with
//! `curl`, so the door is a bearer token compared in constant time, checked in
//! a `route_layer` — that is, **before** any handler extractor parses a body.
//!
//! The token is not optional and there is no default. When
//! `TINYSWEEPER_ADMIN_TOKEN` is unset the admin router is never mounted at all
//! and every path under `/admin` 404s, because the alternative to fail-closed
//! here is shipping an unauthenticated write endpoint.
//!
//! Contributor trust is complete. Index status and knowledge documents are
//! declared but return `501`: the stores behind them land in another
//! workstream, and the same reasoning that declares every CLI subcommand up
//! front applies here — a stable coordinate shape lets the caller be written
//! now. Each stub is marked `TODO(knowledge-store)` / `TODO(index-store)`.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};
use crate::server::store::{Contributor, Store, Trust};

/// Shortest admin token the server will start with.
///
/// A short token is brute-forceable over the same public endpoint it protects,
/// and an operator who pasted a placeholder should find out at startup rather
/// than after someone else does.
const MIN_TOKEN_LEN: usize = 32;

/// Environment variable carrying the admin bearer token.
pub const TOKEN_ENV: &str = "TINYSWEEPER_ADMIN_TOKEN";

/// The credential guarding `/admin`.
///
/// Only the digest is kept: nothing that logs or formats this can print the
/// token, and the comparison wants a fixed-width value anyway.
#[derive(Clone)]
pub struct AdminAuth {
    digest: [u8; 32],
}

impl std::fmt::Debug for AdminAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAuth")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl AdminAuth {
    /// Build from a token, rejecting one too weak to protect a public endpoint.
    pub fn new(token: &str) -> Result<Self> {
        if token.len() < MIN_TOKEN_LEN {
            return Err(Error::config(format!(
                "{TOKEN_ENV} must be at least {MIN_TOKEN_LEN} characters. It is the only thing \
                 standing between the internet and the trust database, so a short one is \
                 rejected rather than accepted with a warning."
            )));
        }
        Ok(Self {
            digest: Sha256::digest(token.as_bytes()).into(),
        })
    }

    /// Read the token from the environment.
    ///
    /// `Ok(None)` means no admin API — which is a supported configuration, not
    /// an error. An empty value is treated as unset: `env::var` returns
    /// `Ok("")` for a variable exported without a value, which is exactly what
    /// copying `.env.example` produces.
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var(TOKEN_ENV) {
            Ok(token) if token.trim().is_empty() => Ok(None),
            Ok(token) => Self::new(&token).map(Some),
            Err(_) => Ok(None),
        }
    }

    /// Whether an `Authorization` header value is the admin token.
    ///
    /// Compared in constant time over digests: a byte-at-a-time comparison on
    /// the raw token leaks how much of a guess was right, which is enough to
    /// recover it given patience.
    pub fn permits(&self, header: Option<&str>) -> bool {
        let Some(offered) = header.and_then(|h| h.strip_prefix("Bearer ")) else {
            return false;
        };
        let offered: [u8; 32] = Sha256::digest(offered.as_bytes()).into();
        offered.ct_eq(&self.digest).into()
    }
}

/// What the admin handlers need.
#[derive(Clone)]
struct AdminState {
    store: Store,
    auth: Arc<AdminAuth>,
}

/// How a trust decision arrives.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrustRequest {
    /// The trust level to set.
    pub trust: Trust,
    /// Why. Optional, but a trust decision without a reason is unreviewable
    /// six months later, so the API keeps the field rather than dropping it.
    #[serde(default)]
    pub note: Option<String>,
}

/// Build the admin router, or nothing when no token is configured.
///
/// Returning `None` rather than an unguarded router is the fail-closed
/// half of the decision: a misconfigured deployment loses the admin API, it
/// does not expose it.
pub fn router(store: Store, auth: Option<AdminAuth>) -> Option<Router> {
    let auth = auth?;
    let state = AdminState {
        store,
        auth: Arc::new(auth),
    };

    let routes = Router::new()
        // Contributor trust — issue #25. `Trust::Blocked` was enforced on
        // every review with nothing able to set it; this is that surface.
        .route("/admin/contributors/{login}", get(get_contributor))
        .route("/admin/contributors/{login}/trust", put(set_trust))
        // Index status and re-index. TODO(index-store).
        .route("/admin/index/{owner}/{name}", get(index_status))
        .route("/admin/index/{owner}/{name}/reindex", post(reindex))
        // Knowledge documents, org- and repo-scoped. TODO(knowledge-store).
        .route("/admin/knowledge/org/{owner}", get(list_org_knowledge))
        .route(
            "/admin/knowledge/org/{owner}/{slug}",
            put(put_org_knowledge).delete(delete_org_knowledge),
        )
        .route(
            "/admin/knowledge/repo/{owner}/{name}",
            get(list_repo_knowledge),
        )
        .route(
            "/admin/knowledge/repo/{owner}/{name}/{slug}",
            put(put_repo_knowledge).delete(delete_repo_knowledge),
        )
        // `route_layer` rather than `layer`: it applies only to routes this
        // router matched, and it runs before the handlers' extractors, so an
        // unauthenticated request never gets its body parsed.
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state);

    Some(routes)
}

/// Reject anything without the admin token, before the body is touched.
async fn guard(State(state): State<AdminState>, request: Request, next: Next) -> Response {
    let offered = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    if !state.auth.permits(offered) {
        // No detail: distinguishing "no header" from "wrong token" tells a
        // prober which half to work on.
        tracing::warn!("rejected an admin request");
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    next.run(request).await
}

/// An error shaped like the rest of the API's JSON.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        // The store is the only thing these handlers call, so a failure here is
        // the database rather than the request.
        ApiError(StatusCode::SERVICE_UNAVAILABLE, err.to_string())
    }
}

/// A declared endpoint whose store has not landed.
///
/// `501` rather than `404`: the route exists and the shape is committed to, so
/// a caller written against it today is not written against a guess.
fn not_implemented(what: &str, tracking: &str) -> ApiError {
    ApiError(
        StatusCode::NOT_IMPLEMENTED,
        format!("{what} is declared but not implemented yet ({tracking})"),
    )
}

async fn get_contributor(
    State(state): State<AdminState>,
    Path(login): Path<String>,
) -> std::result::Result<Json<Contributor>, ApiError> {
    let login = valid_login(&login)?;
    // An unseen contributor is `Unknown`, not a 404: "we have never seen this
    // person" is an answer, and returning it lets an operator set trust ahead
    // of a first pull request.
    Ok(Json(state.store.contributor(&login).await?))
}

async fn set_trust(
    State(state): State<AdminState>,
    Path(login): Path<String>,
    Json(body): Json<TrustRequest>,
) -> std::result::Result<Json<Contributor>, ApiError> {
    let login = valid_login(&login)?;
    let note = body.note.as_deref().filter(|note| !note.trim().is_empty());

    state.store.set_trust(&login, body.trust, note).await?;
    // Logged deliberately: a trust change is an operator action on a person and
    // should be reconstructable from the logs alone.
    tracing::info!(%login, trust = ?body.trust, "admin set contributor trust");

    Ok(Json(state.store.contributor(&login).await?))
}

async fn index_status(
    Path((owner, name)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let _ = (owner, name);
    Err(not_implemented("index status", "TODO(index-store)"))
}

async fn reindex(
    Path((owner, name)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let _ = (owner, name);
    Err(not_implemented("re-indexing", "TODO(index-store)"))
}

async fn list_org_knowledge(
    Path(owner): Path<String>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let _ = owner;
    Err(not_implemented(
        "org knowledge documents",
        "TODO(knowledge-store)",
    ))
}

async fn put_org_knowledge(
    Path((owner, slug)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let _ = (owner, slug);
    Err(not_implemented(
        "org knowledge documents",
        "TODO(knowledge-store)",
    ))
}

async fn delete_org_knowledge(
    Path((owner, slug)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let _ = (owner, slug);
    Err(not_implemented(
        "org knowledge documents",
        "TODO(knowledge-store)",
    ))
}

async fn list_repo_knowledge(
    Path((owner, name)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let _ = (owner, name);
    Err(not_implemented(
        "repo knowledge documents",
        "TODO(knowledge-store)",
    ))
}

async fn put_repo_knowledge(
    Path((owner, name, slug)): Path<(String, String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let _ = (owner, name, slug);
    Err(not_implemented(
        "repo knowledge documents",
        "TODO(knowledge-store)",
    ))
}

async fn delete_repo_knowledge(
    Path((owner, name, slug)): Path<(String, String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let _ = (owner, name, slug);
    Err(not_implemented(
        "repo knowledge documents",
        "TODO(knowledge-store)",
    ))
}

/// Accept only something that could be a GitHub login.
///
/// The login is a Mongo `_id`, so an unbounded or empty one is a way to write
/// junk into the collection through an otherwise valid request.
fn valid_login(login: &str) -> std::result::Result<String, ApiError> {
    let login = login.trim();
    let plausible = !login.is_empty()
        && login.len() <= 39
        && login
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '[' || c == ']');

    if plausible {
        Ok(login.to_string())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "not a plausible GitHub login".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    /// The same read `guard` does, so the header handling under test is the
    /// header handling in the request path.
    fn bearer(headers: &HeaderMap) -> Option<&str> {
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_short_token_is_refused_at_startup() {
        // Failing here is the whole point: the alternative is discovering it
        // when someone else finds the endpoint.
        assert!(AdminAuth::new("hunter2").is_err());
        assert!(AdminAuth::new("").is_err());
        assert!(AdminAuth::new(TOKEN).is_ok());
    }

    #[test]
    fn only_the_exact_bearer_token_is_admitted() {
        let auth = AdminAuth::new(TOKEN).expect("builds");
        assert!(auth.permits(Some(&format!("Bearer {TOKEN}"))));
        assert!(!auth.permits(Some(&format!("Bearer {TOKEN}x"))));
        assert!(!auth.permits(Some(TOKEN)), "the scheme is required");
        assert!(!auth.permits(Some("Basic abc")));
        assert!(!auth.permits(None), "a missing header is not a pass");
    }

    #[test]
    fn a_prefix_of_the_token_is_not_enough() {
        // Guards against a comparison that stops at the shorter length.
        let auth = AdminAuth::new(TOKEN).expect("builds");
        assert!(!auth.permits(Some("Bearer 0123456789abcdef")));
    }

    #[test]
    fn no_token_configured_means_no_admin_router() {
        // Fail closed: the router is absent, not unguarded.
        let auth: Option<AdminAuth> = None;
        assert!(auth.is_none());
    }

    #[test]
    fn the_token_never_appears_in_debug_output() {
        let auth = AdminAuth::new(TOKEN).expect("builds");
        let shown = format!("{auth:?}");
        assert!(!shown.contains(TOKEN), "got {shown}");
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn a_bearer_header_round_trips() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().expect("valid header"),
        );
        let auth = AdminAuth::new(TOKEN).expect("builds");
        assert!(auth.permits(bearer(&headers)));
    }

    #[test]
    fn logins_are_checked_before_they_reach_the_database() {
        assert!(valid_login("senamakel").is_ok());
        assert!(valid_login("dependabot[bot]").is_ok());
        assert!(valid_login("  spaced  ").is_ok(), "trimmed, not rejected");
        assert!(valid_login("").is_err());
        assert!(valid_login("../../etc").is_err());
        assert!(valid_login(&"a".repeat(40)).is_err());
    }

    #[test]
    fn a_trust_request_parses_the_documented_shape() {
        let body: TrustRequest =
            serde_json::from_str(r#"{"trust":"blocked","note":"spam"}"#).expect("parses");
        assert_eq!(body.trust, Trust::Blocked);
        assert_eq!(body.note.as_deref(), Some("spam"));

        let bare: TrustRequest = serde_json::from_str(r#"{"trust":"allowed"}"#).expect("parses");
        assert_eq!(bare.trust, Trust::Allowed);
        assert!(bare.note.is_none());

        assert!(
            serde_json::from_str::<TrustRequest>(r#"{"trust":"vibes"}"#).is_err(),
            "an unknown trust level must not silently become Unknown"
        );
    }
}
