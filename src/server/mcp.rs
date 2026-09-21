//! The authenticated Model Context Protocol endpoint.
//!
//! This is intentionally a small JSON-RPC transport rather than an agent
//! framework. It exposes repository facts, not a general shell: GitHub App
//! installation tokens remain server-side, vector search stays scoped to one
//! installed repository, and the sole mutation has deterministic duplicate
//! protection before it obtains a write handle.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::forge::RepoId;
use crate::index::types::HybridQuery;
use crate::ports::forge::ForgeRead;
use crate::ports::index::ChunkIndex;
use crate::server::auth::AppAuth;
use crate::server::indexing::IndexBackend;
use crate::server::store::Store;

const MAX_HITS: usize = 20;
const MAX_DOCS: usize = 20;
const MAX_HTTP_BODY: usize = 128 * 1024;
const MAX_QUERY_BYTES: usize = 4 * 1024;
const MAX_TITLE_BYTES: usize = 256;
const MAX_ISSUE_BODY_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 1024;
const MAX_LABELS: usize = 20;
const MAX_LABEL_BYTES: usize = 50;
const PROTOCOL_VERSION: &str = "2025-03-26";
const MIN_TOKEN_LEN: usize = 32;

/// The credential guarding only the MCP endpoint.
#[derive(Clone)]
pub struct McpAuth {
    digest: [u8; 32],
}

impl std::fmt::Debug for McpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpAuth")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl McpAuth {
    /// Build from the dedicated bearer, rejecting weak public-endpoint tokens.
    pub fn new(token: &str, var: &str) -> crate::Result<Self> {
        if token.len() < MIN_TOKEN_LEN {
            return Err(crate::Error::config(format!(
                "{var} must be at least {MIN_TOKEN_LEN} characters"
            )));
        }
        Ok(Self {
            digest: Sha256::digest(token.as_bytes()).into(),
        })
    }

    /// Read the dedicated bearer from its configured environment variable.
    pub fn from_env(var: &str) -> crate::Result<Option<Self>> {
        match std::env::var(var) {
            Ok(token) if token.trim().is_empty() => Ok(None),
            Ok(token) => Self::new(&token, var).map(Some),
            Err(_) => Ok(None),
        }
    }

    fn permits(&self, header: Option<&str>) -> bool {
        let Some(offered) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
            return false;
        };
        let offered: [u8; 32] = Sha256::digest(offered.as_bytes()).into();
        offered.ct_eq(&self.digest).into()
    }
}

#[derive(Clone)]
struct McpState {
    allowed_org: Arc<str>,
    auth: Arc<AppAuth>,
    index: Option<Arc<IndexBackend>>,
    store: Option<Store>,
}

/// Build an MCP router, or no router when the MCP bearer is unset.
pub fn router(
    authz: Option<McpAuth>,
    allowed_org: String,
    auth: Arc<AppAuth>,
    index: Option<Arc<IndexBackend>>,
    store: Store,
) -> Option<Router> {
    build_router(authz?, allowed_org, auth, index, Some(store))
}

fn build_router(
    authz: McpAuth,
    allowed_org: String,
    auth: Arc<AppAuth>,
    index: Option<Arc<IndexBackend>>,
    store: Option<Store>,
) -> Option<Router> {
    let authz = Arc::new(authz);
    Some(
        Router::new()
            .route("/mcp", post(handle))
            .layer(DefaultBodyLimit::max(MAX_HTTP_BODY))
            .with_state(McpState {
                allowed_org: Arc::from(allowed_org),
                auth,
                index,
                store,
            })
            .route_layer(axum::middleware::from_fn_with_state(authz, guard)),
    )
}

async fn guard(State(auth): State<Arc<McpAuth>>, request: Request, next: Next) -> Response {
    let offered = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if auth.permits(offered) {
        next.run(request).await
    } else {
        axum::http::StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn handle(State(state): State<McpState>, Json(request): Json<Value>) -> impl IntoResponse {
    if is_notification(&request) {
        return axum::http::StatusCode::ACCEPTED.into_response();
    }
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(
            json!({"protocolVersion":PROTOCOL_VERSION,"serverInfo":{"name":"tinysweeper","version":env!("CARGO_PKG_VERSION")},"capabilities":{"tools":{}}}),
        ),
        "tools/list" => Ok(tools()),
        "tools/call" => call(&state, &params).await,
        _ => Err(format!("unsupported MCP method `{method}`")),
    };
    match result {
        Ok(result) => Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response(),
        Err(message) => {
            Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":message}}))
                .into_response()
        }
    }
}

fn is_notification(request: &Value) -> bool {
    request.get("id").is_none()
}

fn tools() -> Value {
    json!({"tools":[
     {"name":"search_code","description":"Hybrid vector and lexical search of one indexed repository.","inputSchema":{"type":"object","required":["repo","query"],"properties":{"repo":{"type":"string"},"query":{"type":"string","maxLength":4096},"limit":{"type":"integer"}}}},
     {"name":"read_docs","description":"Read repository documentation and issue templates from the default branch.","inputSchema":{"type":"object","required":["repo"],"properties":{"repo":{"type":"string"},"path":{"type":"string","maxLength":1024}}}},
     {"name":"create_issue","description":"Create a title-deduplicated GitHub issue enriched with vector-derived code locations and a repository Markdown issue template. Returns likely duplicates instead of writing unless force is true.","inputSchema":{"type":"object","required":["repo","title","body"],"properties":{"repo":{"type":"string"},"title":{"type":"string","maxLength":256},"body":{"type":"string","maxLength":65536},"template":{"type":"string","maxLength":1024,"description":"Optional repo-relative Markdown issue template path."},"labels":{"type":"array","maxItems":20,"items":{"type":"string","maxLength":50}},"force":{"type":"boolean"}}}}
    ]})
}

async fn call(state: &McpState, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call needs a tool name")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let requested_repo = checked_repo(
        &state.allowed_org,
        args.get("repo")
            .and_then(Value::as_str)
            .ok_or("repo is required")?,
    )?;
    let token = read_token(state, &requested_repo).await?;
    let forge = crate::forge::github::GitHubRead::new(&token).map_err(|err| err.to_string())?;
    let repo = forge
        .canonical_repo(&requested_repo)
        .await
        .map_err(|err| err.to_string())?;
    checked_repo(&state.allowed_org, &repo.to_string())?;
    match name {
        "search_code" => search_code(state, &repo, &args).await,
        "read_docs" => read_docs(state, &repo, &args).await,
        "create_issue" => create_issue(state, &repo, &args).await,
        _ => Err(format!("unknown tool `{name}`")),
    }
    .map(content)
}

fn checked_repo(allowed_org: &str, raw: &str) -> Result<RepoId, String> {
    let repo = RepoId::parse(raw).ok_or("repo must be owner/name")?;
    if !repo.owner.eq_ignore_ascii_case(allowed_org) {
        return Err("repository is outside this MCP server's allowed organisation".into());
    }
    Ok(repo)
}

async fn read_token(state: &McpState, repo: &RepoId) -> Result<String, String> {
    let installation = state
        .auth
        .installation_for_repo(&repo.owner, &repo.name)
        .await
        .map_err(|e| e.to_string())?;
    state
        .auth
        .review_read_token(installation)
        .await
        .map_err(|e| e.to_string())
}

async fn search_code(state: &McpState, repo: &RepoId, args: &Value) -> Result<Value, String> {
    let backend = state
        .index
        .as_ref()
        .ok_or("vector search is not configured")?;
    let text = bounded_required(args, "query", MAX_QUERY_BYTES)?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .min(MAX_HITS as u64) as usize;
    let vector = backend
        .embedder
        .embed(&[text.to_string()])
        .await
        .map_err(|e| e.to_string())?
        .into_query_vector()
        .map_err(|e| e.to_string())?;
    let hits = backend
        .index
        .code
        .query(
            &HybridQuery::new(backend.signature.clone(), text, vector)
                .in_repo(repo.to_string())
                .limit(limit),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"repo":repo.to_string(),"hits":hits.into_iter().map(|hit| json!({"path":hit.chunk.path,"start_line":hit.chunk.start_line,"end_line":hit.chunk.end_line,"symbol":hit.chunk.symbol,"score":hit.score,"text":hit.chunk.text})).collect::<Vec<_>>() }),
    )
}

async fn read_docs(state: &McpState, repo: &RepoId, args: &Value) -> Result<Value, String> {
    let token = read_token(state, repo).await?;
    let forge = crate::forge::github::GitHubRead::new(&token).map_err(|e| e.to_string())?;
    let head = forge
        .default_branch(repo)
        .await
        .map_err(|e| e.to_string())?;
    let sha = forge
        .branch_head(repo, &head)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("default branch has no head")?;
    let requested = args.get("path").and_then(Value::as_str);
    if let Some(path) = requested {
        if path.len() > MAX_PATH_BYTES || !is_doc_path(path) {
            return Err(
                "path must name supported repository documentation or an issue template".into(),
            );
        }
        let file = forge
            .file_at(repo, path, &sha)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("documentation path `{path}` does not exist"))?;
        return Ok(json!({
            "repo": repo.to_string(),
            "revision": sha,
            "files": [{"path": path, "text": file}]
        }));
    }
    let docs: Vec<_> = forge
        .tree_paths(repo, &sha)
        .await
        .map_err(|e| e.to_string())?
        .paths
        .into_iter()
        .filter(|path| is_doc_path(path))
        .take(MAX_DOCS)
        .collect();
    let mut files = Vec::new();
    for path in docs {
        if let Some(text) = forge
            .file_at(repo, &path, &sha)
            .await
            .map_err(|e| e.to_string())?
        {
            files.push(json!({"path":path,"text":text}));
        }
    }
    Ok(json!({"repo":repo.to_string(),"revision":sha,"files":files}))
}

fn is_doc_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with(".github/issue_template/") {
        return lower.ends_with(".md") || lower.ends_with(".yml") || lower.ends_with(".yaml");
    }
    let filename = lower.rsplit('/').next().unwrap_or(&lower);
    let documentation_extension = [".md", ".mdx", ".rst", ".adoc", ".txt"]
        .iter()
        .any(|extension| lower.ends_with(extension));
    let conventional = [
        "readme",
        "contributing",
        "changelog",
        "security",
        "code_of_conduct",
        "agents",
    ]
    .iter()
    .any(|stem| {
        filename == *stem || (filename.starts_with(&format!("{stem}.")) && documentation_extension)
    });
    let under_docs = lower.starts_with("docs/") || lower.contains("/docs/");
    conventional || (under_docs && documentation_extension)
}

fn bounded_required<'a>(args: &'a Value, field: &str, max: usize) -> Result<&'a str, String> {
    let value = args
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{field} is required"))?;
    if value.len() > max {
        return Err(format!("{field} must be at most {max} bytes"));
    }
    Ok(value)
}

async fn create_issue(state: &McpState, repo: &RepoId, args: &Value) -> Result<Value, String> {
    let title = bounded_required(args, "title", MAX_TITLE_BYTES)?;
    let body = bounded_required(args, "body", MAX_ISSUE_BODY_BYTES)?;

    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    let token = read_token(state, repo).await?;
    let read = crate::forge::github::GitHubRead::new(&token).map_err(|e| e.to_string())?;
    let duplicates = read
        .search_issues(repo, title)
        .await
        .map_err(|e| e.to_string())?;
    if !duplicates.is_empty() && !force {
        return Ok(
            json!({"created":false,"reason":"possible duplicates","duplicates":duplicates.into_iter().map(|i| json!({"number":i.number,"title":i.title,"open":i.open})).collect::<Vec<_>>() }),
        );
    }

    let head = read.default_branch(repo).await.map_err(|e| e.to_string())?;
    let sha = read
        .branch_head(repo, &head)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("default branch has no head")?;
    let listing = read
        .tree_paths(repo, &sha)
        .await
        .map_err(|e| e.to_string())?;
    let requested_template = args.get("template").and_then(Value::as_str);
    if let Some(path) = requested_template
        && (path.len() > MAX_PATH_BYTES || !is_issue_template_path(path))
    {
        return Err("template must name a Markdown file under .github/ISSUE_TEMPLATE/".into());
    }
    let template_path = match requested_template {
        Some(path) => Some(path.to_string()),
        None if listing.truncated => None,
        None => choose_template(&listing.paths),
    };
    let template = match &template_path {
        Some(path) => Some(
            read.file_at(repo, path, &sha)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("issue template `{path}` does not exist"))?,
        ),
        None => None,
    };
    let evidence = issue_evidence(state, repo, title).await;
    let issue_body = enriched_issue_body(template.as_deref(), body, &evidence);

    let installation = state
        .auth
        .installation_for_repo(&repo.owner, &repo.name)
        .await
        .map_err(|e| e.to_string())?;
    let labels: Vec<String> = args
        .get("labels")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    if labels.len() > MAX_LABELS || labels.iter().any(|label| label.len() > MAX_LABEL_BYTES) {
        return Err(format!(
            "labels are limited to {MAX_LABELS} entries of {MAX_LABEL_BYTES} bytes each"
        ));
    }
    let plan = crate::app::apply::McpIssuePlan {
        repo: repo.clone(),
        installation,
        title: title.to_string(),
        body: issue_body,
        labels,
    };

    // Planning is deliberately complete before claiming. The claim makes the
    // eventual-consistency gap between GitHub search and creation atomic, and
    // remains after a create request when the response is ambiguous: a timeout
    // may mean GitHub accepted the issue but its response was lost. Failures
    // known to precede that request release the claim below and remain retryable.
    let request_key = issue_request_key(repo, title);
    if !state
        .store
        .as_ref()
        .ok_or("issue memory is unavailable")?
        .claim_delivery(&request_key, "mcp-create-issue")
        .await
        .map_err(|err| err.to_string())?
    {
        return Ok(json!({
            "created": false,
            "reason": "an issue request with this repository and title was already accepted recently"
        }));
    }
    let number = match crate::app::apply::apply_mcp_issue(state.auth.as_ref(), &plan).await {
        Ok(number) => number,
        Err(crate::app::apply::McpIssueApplyError::BeforeWrite(error)) => {
            state
                .store
                .as_ref()
                .expect("the claim required a store")
                .release_delivery(&request_key)
                .await
                .map_err(|release| release.to_string())?;
            return Err(error.to_string());
        }
        Err(crate::app::apply::McpIssueApplyError::Ambiguous(error)) => {
            return Err(error.to_string());
        }
    };
    Ok(json!({"created":true,"number":number,"template":template_path,"code_locations":evidence}))
}

fn issue_request_key(repo: &RepoId, title: &str) -> String {
    let normalized = format!(
        "{}\n{}",
        repo.to_string().to_ascii_lowercase(),
        title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    );
    use std::fmt::Write as _;
    let mut key = String::from("mcp-issue:");
    for byte in Sha256::digest(normalized.as_bytes()) {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

fn is_issue_template_path(path: &str) -> bool {
    path.starts_with(".github/ISSUE_TEMPLATE/") && path.to_ascii_lowercase().ends_with(".md")
}

fn choose_template(paths: &[String]) -> Option<String> {
    let templates = paths
        .iter()
        .filter(|path| is_issue_template_path(path))
        .collect::<Vec<_>>();
    (templates.len() == 1).then(|| templates[0].clone())
}

async fn issue_evidence(state: &McpState, repo: &RepoId, query: &str) -> Vec<Value> {
    let Some(backend) = &state.index else {
        return Vec::new();
    };
    let Ok(embedded) = backend.embedder.embed(&[query.to_string()]).await else {
        return Vec::new();
    };
    let Ok(vector) = embedded.into_query_vector() else {
        return Vec::new();
    };
    let Ok(hits) = backend
        .index
        .code
        .query(
            &HybridQuery::new(backend.signature.clone(), query, vector)
                .in_repo(repo.to_string())
                .limit(5),
        )
        .await
    else {
        return Vec::new();
    };
    hits.into_iter()
        .map(|hit| {
            json!({
                "path": hit.chunk.path,
                "start_line": hit.chunk.start_line,
                "end_line": hit.chunk.end_line,
                "symbol": hit.chunk.symbol,
            })
        })
        .collect()
}

fn enriched_issue_body(template: Option<&str>, body: &str, evidence: &[Value]) -> String {
    let mut out = String::new();
    if let Some(template) = template {
        out.push_str(strip_template_front_matter(template).trim_end());
        out.push_str("\n\n## Teeny-provided details\n\n");
    }
    out.push_str(body.trim());
    if !evidence.is_empty() {
        out.push_str("\n\n## Relevant code locations\n\n");
        for hit in evidence {
            let path = hit["path"].as_str().unwrap_or("unknown");
            let start = hit["start_line"].as_u64().unwrap_or(0);
            let end = hit["end_line"].as_u64().unwrap_or(start);
            let symbol = hit["symbol"]
                .as_str()
                .map(|value| format!(" — `{value}`"))
                .unwrap_or_default();
            out.push_str(&format!("- `{path}:{start}-{end}`{symbol}\n"));
        }
    }
    out.push_str("\n<!-- tinysweeper:mcp -->\n");
    out
}

fn strip_template_front_matter(template: &str) -> &str {
    if let Some(rest) = template.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        return &rest[end + 5..];
    }
    if let Some(rest) = template.strip_prefix("---\r\n")
        && let Some(end) = rest.find("\r\n---\r\n")
    {
        return &rest[end + 8..];
    }
    template
}

fn content(value: Value) -> Value {
    json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())}],"structuredContent":value})
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn protocol_router() -> Router {
        let auth = AppAuth::from_der("1", &crate::server::test_key::test_key_der()).unwrap();
        build_router(
            McpAuth::new(TOKEN, "TINYSWEEPER_MCP_TOKEN").unwrap(),
            "tinyhumansai".into(),
            Arc::new(auth),
            None,
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn the_http_route_requires_its_own_bearer_and_initializes() {
        let request = Request::post("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            ))
            .unwrap();
        let denied = protocol_router().oneshot(request).await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let request = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            ))
            .unwrap();
        let response = protocol_router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(value["result"]["serverInfo"]["name"], "tinysweeper");

        let request = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            ))
            .unwrap();
        let response = protocol_router().oneshot(request).await.unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["result"]["tools"].as_array().unwrap().len(), 3);

        let request = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ))
            .unwrap();
        let response = protocol_router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_mcp_credential_is_strong_redacted_and_exact() {
        assert!(McpAuth::new("short", "TINYSWEEPER_MCP_TOKEN").is_err());
        let auth = McpAuth::new(TOKEN, "TINYSWEEPER_MCP_TOKEN").unwrap();
        assert!(auth.permits(Some(&format!("Bearer {TOKEN}"))));
        assert!(!auth.permits(Some(&format!("Bearer {TOKEN}x"))));
        assert!(!format!("{auth:?}").contains(TOKEN));
    }

    #[test]
    fn repository_scope_is_case_insensitive_and_closed() {
        assert!(checked_repo("tinyhumansai", "TinyHumansAI/teeny").is_ok());
        assert!(checked_repo("tinyhumansai", "somebody/teeny").is_err());
    }

    #[test]
    fn streamable_http_protocol_version_is_advertised() {
        assert_eq!(PROTOCOL_VERSION, "2025-03-26");
    }

    #[test]
    fn the_only_markdown_template_is_selected_automatically() {
        let paths = vec![
            ".github/ISSUE_TEMPLATE/config.yml".into(),
            ".github/ISSUE_TEMPLATE/bug.md".into(),
        ];
        assert_eq!(
            choose_template(&paths).as_deref(),
            Some(".github/ISSUE_TEMPLATE/bug.md")
        );
    }

    #[test]
    fn enrichment_keeps_the_template_details_and_code_locations() {
        let body = enriched_issue_body(
            Some("## Expected\n"),
            "The result should be stable.",
            &[json!({"path":"src/lib.rs","start_line":4,"end_line":9,"symbol":"run"})],
        );
        assert!(body.starts_with("## Expected"));
        assert!(body.contains("Teeny-provided details"));
        assert!(body.contains("`src/lib.rs:4-9` — `run`"));
        assert!(body.ends_with("<!-- tinysweeper:mcp -->\n"));
    }

    #[test]
    fn requested_paths_are_limited_to_documentation() {
        assert!(is_doc_path("README.md"));
        assert!(is_doc_path("docs/setup.txt"));
        assert!(is_doc_path(".github/ISSUE_TEMPLATE/bug.yml"));
        assert!(!is_doc_path("docs/config.json"));
        assert!(!is_doc_path("src/secrets.md"));
        assert!(!is_doc_path(".env"));
        assert!(!is_doc_path("src/lib.rs"));
    }

    #[test]
    fn issue_inputs_are_bounded_before_processing() {
        let args = json!({"title":"x".repeat(MAX_TITLE_BYTES + 1)});
        assert!(bounded_required(&args, "title", MAX_TITLE_BYTES).is_err());
        let args = json!({"query":"where is parsing handled?"});
        assert_eq!(
            bounded_required(&args, "query", MAX_QUERY_BYTES).unwrap(),
            "where is parsing handled?"
        );
    }

    #[test]
    fn github_template_front_matter_is_not_copied_into_the_issue() {
        let template = "---\nname: Bug\nlabels: bug\n---\n## Reproduction\n";
        let body = enriched_issue_body(Some(template), "It loops.", &[]);
        assert!(!body.contains("name: Bug"));
        assert!(body.starts_with("## Reproduction"));
        assert!(body.contains("It loops."));
    }

    #[test]
    fn issue_request_keys_normalize_case_and_whitespace() {
        let first = RepoId::parse("TinyHumansAI/Teeny").unwrap();
        let second = RepoId::parse("tinyhumansai/teeny").unwrap();
        assert_eq!(
            issue_request_key(&first, "Bug  in parser"),
            issue_request_key(&second, "  bug in PARSER ")
        );
    }

    #[test]
    fn requests_without_an_id_are_notifications() {
        assert!(is_notification(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        ));
        assert!(!is_notification(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
        ));
    }
}
