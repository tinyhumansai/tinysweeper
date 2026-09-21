//! The authenticated Model Context Protocol endpoint.
//!
//! This is intentionally a small JSON-RPC transport rather than an agent
//! framework. It exposes repository facts, not a general shell: GitHub App
//! installation tokens remain server-side, vector search stays scoped to one
//! installed repository, and the sole mutation has deterministic duplicate
//! protection before it obtains a write handle.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::forge::RepoId;
use crate::index::types::HybridQuery;
use crate::ports::forge::ForgeRead;
use crate::ports::index::ChunkIndex;
use crate::server::admin::AdminAuth;
use crate::server::auth::AppAuth;
use crate::server::indexing::IndexBackend;
use crate::server::store::Store;

mod apply;

const MAX_HITS: usize = 20;
const MAX_DOCS: usize = 20;

#[derive(Clone)]
struct McpState {
    allowed_org: Arc<str>,
    auth: Arc<AppAuth>,
    index: Option<Arc<IndexBackend>>,
    store: Store,
}

/// Build an MCP router, or no router when the MCP bearer is unset.
pub fn router(
    authz: Option<AdminAuth>,
    allowed_org: String,
    auth: Arc<AppAuth>,
    index: Option<Arc<IndexBackend>>,
    store: Store,
) -> Option<Router> {
    let authz = Arc::new(authz?);
    Some(
        Router::new()
            .route("/mcp", post(handle))
            .with_state(McpState {
                allowed_org: Arc::from(allowed_org),
                auth,
                index,
                store,
            })
            .route_layer(axum::middleware::from_fn_with_state(authz, guard)),
    )
}

async fn guard(State(auth): State<Arc<AdminAuth>>, request: Request, next: Next) -> Response {
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
            json!({"protocolVersion":"2024-11-05","serverInfo":{"name":"tinysweeper","version":env!("CARGO_PKG_VERSION")},"capabilities":{"tools":{}}}),
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
     {"name":"search_code","description":"Hybrid vector and lexical search of one indexed repository.","inputSchema":{"type":"object","required":["repo","query"],"properties":{"repo":{"type":"string"},"query":{"type":"string"},"limit":{"type":"integer"}}}},
     {"name":"read_docs","description":"Read repository documentation and issue templates from the default branch.","inputSchema":{"type":"object","required":["repo"],"properties":{"repo":{"type":"string"},"path":{"type":"string"}}}},
     {"name":"create_issue","description":"Create a deduplicated GitHub issue enriched with vector-derived code locations and a repository Markdown issue template. Returns likely duplicates instead of writing unless force is true.","inputSchema":{"type":"object","required":["repo","title","body"],"properties":{"repo":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"},"template":{"type":"string","description":"Optional repo-relative Markdown issue template path."},"labels":{"type":"array","items":{"type":"string"}},"force":{"type":"boolean"}}}}
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
    let text = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("query is required")?;
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
    let paths = forge
        .tree_paths(repo, &sha)
        .await
        .map_err(|e| e.to_string())?
        .paths;
    let requested = args.get("path").and_then(Value::as_str);
    if let Some(path) = requested
        && !is_doc_path(path)
    {
        return Err("path must name Markdown documentation or an issue template".into());
    }
    let docs: Vec<_> = paths
        .into_iter()
        .filter(|path| requested.map_or_else(|| is_doc_path(path), |wanted| path == wanted))
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
    path.to_ascii_lowercase().ends_with(".md")
        || path.starts_with("docs/")
        || path.starts_with(".github/ISSUE_TEMPLATE/")
}

async fn create_issue(state: &McpState, repo: &RepoId, args: &Value) -> Result<Value, String> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("title is required")?;
    let body = args
        .get("body")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("body is required")?;

    // This claim is the authoritative retry/concurrency memory. GitHub issue
    // search below catches old duplicates, but its index is eventually
    // consistent and cannot make a check-then-create sequence atomic.
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    let request_key = issue_request_key(repo, title, force);
    if !state
        .store
        .claim_delivery(&request_key, "mcp-create-issue")
        .await
        .map_err(|err| err.to_string())?
    {
        return Ok(json!({
            "created": false,
            "reason": "an identical issue request was already accepted recently"
        }));
    }

    let outcome = create_claimed_issue(state, repo, args, title, body).await;
    if outcome.is_err() {
        // A failed attempt is retryable. Successful claims deliberately remain
        // until the store's TTL expires, covering GitHub's search-index lag.
        state
            .store
            .release_delivery(&request_key)
            .await
            .map_err(|err| err.to_string())?;
    }
    outcome
}

async fn create_claimed_issue(
    state: &McpState,
    repo: &RepoId,
    args: &Value,
    title: &str,
    body: &str,
) -> Result<Value, String> {
    let token = read_token(state, repo).await?;
    let read = crate::forge::github::GitHubRead::new(&token).map_err(|e| e.to_string())?;
    let duplicates = read
        .search_issues(repo, title)
        .await
        .map_err(|e| e.to_string())?;
    if !duplicates.is_empty() && !args.get("force").and_then(Value::as_bool).unwrap_or(false) {
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
    let paths = read
        .tree_paths(repo, &sha)
        .await
        .map_err(|e| e.to_string())?
        .paths;
    let template_path = choose_template(&paths, args.get("template").and_then(Value::as_str))?;
    let template = match &template_path {
        Some(path) => read
            .file_at(repo, path, &sha)
            .await
            .map_err(|e| e.to_string())?,
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
    let plan = apply::IssuePlan {
        repo: repo.clone(),
        installation,
        title: title.to_string(),
        body: issue_body,
        labels,
    };
    let number = apply::apply(state.auth.as_ref(), &plan)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({"created":true,"number":number,"template":template_path,"code_locations":evidence}))
}

fn issue_request_key(repo: &RepoId, title: &str, force: bool) -> String {
    use sha2::{Digest, Sha256};
    let normalized = format!(
        "{}\n{}\n{force}",
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

fn choose_template(paths: &[String], requested: Option<&str>) -> Result<Option<String>, String> {
    let templates = paths
        .iter()
        .filter(|path| {
            path.starts_with(".github/ISSUE_TEMPLATE/")
                && path.to_ascii_lowercase().ends_with(".md")
        })
        .collect::<Vec<_>>();
    if let Some(requested) = requested {
        return templates
            .into_iter()
            .find(|path| path.as_str() == requested)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("issue template `{requested}` does not exist"));
    }
    Ok((templates.len() == 1).then(|| templates[0].clone()))
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
        out.push_str(template.trim_end());
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

fn content(value: Value) -> Value {
    json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())}],"structuredContent":value})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_scope_is_case_insensitive_and_closed() {
        assert!(checked_repo("tinyhumansai", "TinyHumansAI/teeny").is_ok());
        assert!(checked_repo("tinyhumansai", "somebody/teeny").is_err());
    }

    #[test]
    fn the_only_markdown_template_is_selected_automatically() {
        let paths = vec![
            ".github/ISSUE_TEMPLATE/config.yml".into(),
            ".github/ISSUE_TEMPLATE/bug.md".into(),
        ];
        assert_eq!(
            choose_template(&paths, None).unwrap().as_deref(),
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
        assert!(!is_doc_path(".env"));
        assert!(!is_doc_path("src/lib.rs"));
    }

    #[test]
    fn issue_request_keys_normalize_case_and_whitespace() {
        let first = RepoId::parse("TinyHumansAI/Teeny").unwrap();
        let second = RepoId::parse("tinyhumansai/teeny").unwrap();
        assert_eq!(
            issue_request_key(&first, "Bug  in parser", false),
            issue_request_key(&second, "  bug in PARSER ", false)
        );
        assert_ne!(
            issue_request_key(&first, "Bug in parser", false),
            issue_request_key(&first, "Bug in parser", true)
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
