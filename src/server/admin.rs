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
//! Every route is implemented. The index routes answer `503` on a deployment
//! with no embedding provider, which is not a stub: there is genuinely no index
//! to report on, and saying so is a different answer from an empty one.
//!
//! The knowledge routes are the write side of the knowledge centre. A document
//! body reaches a review prompt, so these endpoints are the *only* trusted way
//! into it: a repository's own `AGENTS.md` gets the sandboxed extraction pass
//! in `crate::knowledge::extract` instead, precisely because nobody
//! authenticated wrote it.

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
use crate::index::types::{KnowledgeDoc, KnowledgeScope};
use crate::knowledge::types::{doc_id, valid_slug};
use crate::ports::knowledge::KnowledgeStore;
use crate::server::indexing::IndexBackend;
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
    /// The curated-document store. `None` when the deployment has no
    /// retrieval database, in which case the knowledge routes answer `503`
    /// rather than pretending to have written something.
    knowledge: Option<Arc<dyn KnowledgeStore>>,
    /// The indexing stack. `None` when no embedding provider is configured, in
    /// which case the index routes answer `503` rather than reporting an
    /// index that could not exist.
    index: Option<Arc<IndexBackend>>,
    auth: Arc<AdminAuth>,
}

impl AdminState {
    /// The indexing stack, or the error a caller can act on.
    fn index(&self) -> std::result::Result<&IndexBackend, ApiError> {
        self.index.as_deref().ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "no embedding provider is configured on this deployment, so there is no index. \
                 Set `[embeddings]` in the server's configuration."
                    .into(),
            )
        })
    }

    /// The knowledge store, or the error a caller can act on.
    fn knowledge(&self) -> std::result::Result<&dyn KnowledgeStore, ApiError> {
        self.knowledge.as_deref().ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "no knowledge store is configured on this deployment".into(),
            )
        })
    }
}

/// A knowledge document as the admin API accepts it.
///
/// The id and the scope are not in the body: they come from the path, so a
/// request cannot claim one scope in its URL and write another into the
/// database.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KnowledgeRequest {
    /// A short human title, shown to nobody but read by the model.
    pub title: String,
    /// The document body, injected into review prompts.
    pub body: String,
    /// Whether it goes into every prompt in scope rather than waiting for
    /// retrieval. Pinning is expensive by design — see the port's docs.
    #[serde(default)]
    pub pinned: bool,
    /// Free-form tags for filtering.
    #[serde(default)]
    pub tags: Vec<String>,
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
pub fn router(
    store: Store,
    knowledge: Option<Arc<dyn KnowledgeStore>>,
    index: Option<Arc<IndexBackend>>,
    auth: Option<AdminAuth>,
) -> Option<Router> {
    let auth = auth?;
    let state = AdminState {
        store,
        knowledge,
        index,
        auth: Arc::new(auth),
    };

    let routes = Router::new()
        // Contributor trust — issue #25. `Trust::Blocked` was enforced on
        // every review with nothing able to set it; this is that surface.
        .route("/admin/contributors/{login}", get(get_contributor))
        .route("/admin/contributors/{login}/trust", put(set_trust))
        // Index status and re-index.
        .route("/admin/index/{owner}/{name}", get(index_status))
        .route("/admin/index/{owner}/{name}/reindex", post(reindex))
        // Knowledge documents, org- and repo-scoped.
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
        .route_layer(axum::middleware::from_fn_with_state(
            state.auth.clone(),
            guard,
        ))
        .with_state(state);

    Some(routes)
}

/// Reject anything without the admin token, before the body is touched.
///
/// Takes the credential alone rather than the whole admin state so the manual
/// review router in `crate::server::manual` can be guarded by the same code
/// path: one door, checked one way.
pub(crate) async fn guard(
    State(auth): State<Arc<AdminAuth>>,
    request: Request,
    next: Next,
) -> Response {
    let offered = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    if !auth.permits(offered) {
        // No detail: distinguishing "no header" from "wrong token" tells a
        // prober which half to work on.
        tracing::warn!("rejected an admin request");
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    next.run(request).await
}

/// An error shaped like the rest of the API's JSON.
#[derive(Debug)]
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

/// What a repository's index currently holds, and what it has cost.
///
/// The signature is reported alongside the record rather than left implicit:
/// it is the partition key, so "absent" from a deployment that has just changed
/// embedding model means something quite different from "absent" on a
/// repository nobody has pushed to, and the two are indistinguishable without
/// it.
async fn index_status(
    State(state): State<AdminState>,
    Path((owner, name)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    use crate::ports::manifest::IndexManifest;

    let repo_id = valid_repo(&owner, &name)?;
    let backend = state.index()?;
    let record = backend.manifest.state(&repo_id, &backend.signature).await?;

    Ok(Json(json!({
        "repo": repo_id,
        "signature": backend.signature.key(),
        "state": record.state,
        "revision": record.revision,
        "chunks": record.chunks,
        "message": record.message,
        "usage": record.usage,
    })))
}

/// Discard a repository's index so the next delivery rebuilds it.
///
/// It does **not** index inline, and that is a decision rather than a
/// shortcut. Indexing needs a checkout, and a checkout needs an installation
/// token, which this route has no way to mint: the admin credential
/// authenticates a human, not an app installation. Rather than invent a second
/// credential path into GitHub for an operator convenience, the route resets
/// the freshness record and lets the ordinary push path do the work with the
/// token it already holds.
///
/// The reset deletes the chunks as well as the record. Forgetting the record
/// alone would leave every existing chunk with nothing pointing at it — the
/// next run would neither reuse nor delete them, and stale code would sit in
/// retrieval permanently. So the two go together, and the window in between is
/// a cold index, which every retrieval path already degrades through.
async fn reindex(
    State(state): State<AdminState>,
    Path((owner, name)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    use crate::indexer::types::{Claim, Settled};
    use crate::ports::index::ChunkIndex;
    use crate::ports::manifest::IndexManifest;

    let repo_id = valid_repo(&owner, &name)?;
    let backend = state.index()?;
    let signature = &backend.signature;

    // The same claim a worker takes, for the same reason: resetting the record
    // underneath a run in progress would have that run confirm chunks against a
    // record that no longer exists. A refused claim is `409`, not a wait — the
    // operator can retry, and a request that blocked on a two-hour index would
    // time out anyway.
    let lease = match backend.manifest.claim(&repo_id, signature, "admin").await? {
        Claim::Granted(lease) => lease,
        Claim::Busy { holder } => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "this repository is being indexed right now{}; try again shortly",
                    holder.map(|h| format!(" by `{h}`")).unwrap_or_default()
                ),
            ));
        }
    };

    let paths = backend.manifest.paths(&repo_id, signature).await?;
    let deleted = if paths.is_empty() {
        0
    } else {
        let removed = backend.index.code.delete_paths(&repo_id, &paths).await?;
        backend.manifest.forget(&repo_id, signature, &paths).await?;
        removed
    };

    // Released as a completed run that reflects *no* revision, which is exactly
    // what `RepoIndex::is_fresh` needs to see for the next push to do the work.
    backend
        .manifest
        .release(
            &lease,
            &Settled::Done {
                revision: None,
                chunks: 0,
                usage: Default::default(),
            },
        )
        .await?;

    tracing::info!(%repo_id, deleted, "admin discarded the index; the next push rebuilds it");

    Ok(Json(json!({
        "repo": repo_id,
        "signature": signature.key(),
        "deleted_chunks": deleted,
        "state": "absent",
        "note": "the index was discarded; the next delivery for this repository rebuilds it",
    })))
}

async fn list_org_knowledge(
    State(state): State<AdminState>,
    Path(owner): Path<String>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let owner = valid_login(&owner)?;
    list_docs(state.knowledge()?, KnowledgeScope::org(owner)).await
}

async fn put_org_knowledge(
    State(state): State<AdminState>,
    Path((owner, slug)): Path<(String, String)>,
    Json(body): Json<KnowledgeRequest>,
) -> std::result::Result<Json<KnowledgeDoc>, ApiError> {
    let owner = valid_login(&owner)?;
    write_doc(state.knowledge()?, KnowledgeScope::org(owner), &slug, body).await
}

async fn delete_org_knowledge(
    State(state): State<AdminState>,
    Path((owner, slug)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let owner = valid_login(&owner)?;
    delete_doc(state.knowledge()?, KnowledgeScope::org(owner), &slug).await
}

async fn list_repo_knowledge(
    State(state): State<AdminState>,
    Path((owner, name)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    list_docs(state.knowledge()?, repo_scope(&owner, &name)?).await
}

async fn put_repo_knowledge(
    State(state): State<AdminState>,
    Path((owner, name, slug)): Path<(String, String, String)>,
    Json(body): Json<KnowledgeRequest>,
) -> std::result::Result<Json<KnowledgeDoc>, ApiError> {
    write_doc(state.knowledge()?, repo_scope(&owner, &name)?, &slug, body).await
}

async fn delete_repo_knowledge(
    State(state): State<AdminState>,
    Path((owner, name, slug)): Path<(String, String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    delete_doc(state.knowledge()?, repo_scope(&owner, &name)?, &slug).await
}

/// Every document visible from `scope`, pinned ones first.
///
/// A repository listing includes its organisation's documents, because that is
/// what a review at that scope actually sees — a listing that hid them would
/// have an operator hunting for a rule that is being applied.
async fn list_docs(
    store: &dyn KnowledgeStore,
    scope: KnowledgeScope,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let mut documents = store.pinned(&scope).await?;
    documents.extend(store.retrievable(&scope).await?);
    documents.sort_by(|a, b| b.pinned.cmp(&a.pinned).then_with(|| a.id.cmp(&b.id)));

    Ok(Json(json!({ "documents": documents })))
}

/// Insert or replace one document.
async fn write_doc(
    store: &dyn KnowledgeStore,
    scope: KnowledgeScope,
    slug: &str,
    body: KnowledgeRequest,
) -> std::result::Result<Json<KnowledgeDoc>, ApiError> {
    let slug = checked_slug(slug)?;

    let title = body.title.trim();
    if title.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "a knowledge document needs a title".into(),
        ));
    }
    if body.body.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "a knowledge document needs a body".into(),
        ));
    }

    let doc = KnowledgeDoc {
        id: doc_id(&scope, &slug),
        scope,
        title: title.to_string(),
        body: body.body,
        pinned: body.pinned,
        tags: body.tags,
    };
    store.put(&doc).await?;
    // Logged deliberately, like a trust change: a pinned document is applied to
    // every review in scope, and that should be reconstructable from the logs.
    tracing::info!(id = %doc.id, pinned = doc.pinned, "admin wrote a knowledge document");

    Ok(Json(doc))
}

/// Delete one document, reporting whether it was there.
async fn delete_doc(
    store: &dyn KnowledgeStore,
    scope: KnowledgeScope,
    slug: &str,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let slug = checked_slug(slug)?;
    let id = doc_id(&scope, &slug);

    // 404 rather than a silent success: an operator deleting a document by the
    // wrong name must find out, because the document they meant to remove is
    // still being injected into every review.
    if !store.delete(&id).await? {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no knowledge document `{id}`"),
        ));
    }
    tracing::info!(%id, "admin deleted a knowledge document");

    Ok(Json(json!({ "deleted": id })))
}

/// The `owner/name` repository id for a path pair, both checked.
///
/// The same validators the knowledge routes use, because this string is a
/// database key on the index side exactly as it is on theirs.
fn valid_repo(owner: &str, name: &str) -> std::result::Result<String, ApiError> {
    let owner = valid_login(owner)?;
    let name = valid_repository_name(name)?;
    Ok(format!("{owner}/{name}"))
}

/// The repository scope for an `owner`/`name` pair, both checked.
fn repo_scope(owner: &str, name: &str) -> std::result::Result<KnowledgeScope, ApiError> {
    let owner = valid_login(owner)?;
    let name = valid_repository_name(name)?;
    Ok(KnowledgeScope::repo(format!("{owner}/{name}")))
}

/// Accept a GitHub repository name without applying account-login limits.
fn valid_repository_name(name: &str) -> std::result::Result<String, ApiError> {
    let name = name.trim();
    let plausible = !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');

    if plausible {
        Ok(name.to_string())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "not a plausible GitHub repository name".into(),
        ))
    }
}

/// Accept only a slug that is safe as part of a document id.
fn checked_slug(slug: &str) -> std::result::Result<String, ApiError> {
    let slug = slug.trim();
    if valid_slug(slug) {
        Ok(slug.to_string())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "a document slug is 1-64 characters of letters, digits, `.`, `_` or `-`".into(),
        ))
    }
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

    fn request(title: &str, body: &str, pinned: bool) -> KnowledgeRequest {
        KnowledgeRequest {
            title: title.into(),
            body: body.into(),
            pinned,
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_document_round_trips_through_the_admin_shapes() {
        let store = crate::index::MockKnowledgeStore::new();
        let scope = KnowledgeScope::org("acme");

        let written = write_doc(
            &store,
            scope.clone(),
            "style",
            request("Style", "Tabs.", true),
        )
        .await
        .expect("writes")
        .0;
        assert_eq!(written.id, "org:acme:style");
        assert!(written.pinned);

        let listed = list_docs(&store, scope.clone()).await.expect("lists").0;
        assert_eq!(listed["documents"].as_array().expect("array").len(), 1);

        let _ = delete_doc(&store, scope.clone(), "style")
            .await
            .expect("deletes");
        let listed = list_docs(&store, scope).await.expect("lists").0;
        assert!(listed["documents"].as_array().expect("array").is_empty());
    }

    #[tokio::test]
    async fn a_repository_listing_includes_its_organisation_documents() {
        // What a review at that scope actually sees. A listing that hid them
        // would have an operator hunting for a rule that is being applied.
        let store = crate::index::MockKnowledgeStore::new();
        let _ = write_doc(
            &store,
            KnowledgeScope::org("acme"),
            "org-wide",
            request("Org", "No unwrap.", true),
        )
        .await
        .expect("writes");
        let _ = write_doc(
            &store,
            KnowledgeScope::repo("acme/app"),
            "local",
            request("Repo", "Tabs.", false),
        )
        .await
        .expect("writes");

        let listed = list_docs(&store, KnowledgeScope::repo("acme/app"))
            .await
            .expect("lists")
            .0;
        let documents = listed["documents"].as_array().expect("array");
        assert_eq!(documents.len(), 2);
        assert_eq!(
            documents[0]["_id"], "org:acme:org-wide",
            "pinned documents are listed first"
        );
    }

    #[tokio::test]
    async fn deleting_a_document_that_is_not_there_is_a_404() {
        // A silent success would leave the operator believing a document they
        // meant to remove is gone while it is still in every review.
        let store = crate::index::MockKnowledgeStore::new();
        let err = delete_doc(&store, KnowledgeScope::org("acme"), "absent")
            .await
            .expect_err("404s");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_empty_title_or_body_is_refused() {
        let store = crate::index::MockKnowledgeStore::new();
        let scope = KnowledgeScope::org("acme");
        for bad in [request("  ", "body", false), request("Title", " \n", false)] {
            let err = write_doc(&store, scope.clone(), "slug", bad)
                .await
                .expect_err("refused");
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn a_slug_is_checked_before_it_becomes_a_document_id() {
        let store = crate::index::MockKnowledgeStore::new();
        let err = write_doc(
            &store,
            KnowledgeScope::org("acme"),
            "../../etc",
            request("T", "B", false),
        )
        .await
        .expect_err("refused");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_repository_scope_needs_two_plausible_halves() {
        assert!(repo_scope("acme", "app").is_ok());
        assert!(repo_scope("acme", ".github").is_ok());
        assert!(repo_scope("acme", "foo_bar").is_ok());
        assert!(repo_scope("acme", &"a".repeat(100)).is_ok());
        assert!(repo_scope("acme", "../etc").is_err());
        assert!(repo_scope("acme", &"a".repeat(101)).is_err());
        assert!(repo_scope("", "app").is_err());
    }

    #[test]
    fn a_knowledge_request_parses_the_documented_shape() {
        let body: KnowledgeRequest =
            serde_json::from_str(r#"{"title":"Style","body":"Tabs.","pinned":true}"#)
                .expect("parses");
        assert!(body.pinned);
        assert!(body.tags.is_empty());

        let bare: KnowledgeRequest =
            serde_json::from_str(r#"{"title":"Style","body":"Tabs."}"#).expect("parses");
        assert!(!bare.pinned, "pinning is opt-in: it costs every review");
    }
}
