//! The CortexDB adapter behind the [`Memory`] port. Behind the `cortex` feature.
//!
//! The default build links no HTTP client and the test suite never touches the
//! network, so everything in this file is gated and nothing else in
//! `src/memory/` imports it. Ingest, recall and the prompt block are written
//! against the port and tested against [`crate::memory::MockMemory`]; this is
//! the one place that speaks CortexDB's v1 API.
//!
//! ## What CortexDB is, as far as this adapter cares
//!
//! An append-only event log with an extraction pipeline behind it. A write is
//! `POST /v1/experience`, which captures the text and *then* indexes it —
//! `?wait=indexed` holds the response until the record is readable, which is
//! what makes ingest-then-recall in one process honest. Reads are
//! `POST /v1/recall`, which ranks events (and the facts, beliefs and episodes
//! the engine extracted from them) for a query, and `POST /v1/answer`, which
//! synthesises a cited answer from a recall pack. There is no update route:
//! the same idempotency key with a different body is a `409`, which is why
//! [`MemoryItem::content_id`] hashes the body — a changed section is a new
//! event, not a conflict.
//!
//! ## Scopes
//!
//! CortexDB's scope grammar is `type:id` segments joined by `/`, and recall
//! over a parent covers its children. A repository is `owner:o/repo:r` and a
//! section is a segment under it, so a grounded question is asked of the
//! whole repository and a query is put to one section. Ids outside the
//! grammar's charset — a repository named `tiny.place` — are hex-encoded under
//! a distinguishable type so the mapping is reversible and never collides.
//!
//! ## The envelope
//!
//! The engine indexes the text it is given, so the text has to read well for
//! its extractor *and* carry enough structure for [`Recollection`] to come
//! back typed. Each event's text is a one-line header the adapter can parse —
//! kind, key, path, symbol — followed by the title and body as prose. Recall
//! may prefix the text with a speaker tag; the parser looks for the header
//! line rather than assuming it comes first.
//!
//! ## The credential
//!
//! Read from the environment by name, held in the client's default headers,
//! marked sensitive, and never rendered by `Debug`, an error or a log. Plain
//! HTTP is refused except to loopback — see [`crate::memory::endpoint_allowed`].

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::memory::types::{
    Citation, MemoryAnswer, MemoryItem, MemoryKind, MemoryScope, MemorySection, Recollection,
    RememberReport,
};
use crate::ports::memory::Memory;

/// Default base URL for CortexDB's managed API.
pub const CORTEX_API_ENDPOINT: &str = "https://api-v1.cortexdb.ai";

/// How long a write, or the client's default, may take.
///
/// Generous, because `?wait=indexed` holds a write until the engine has
/// embedded and extracted it — measured at one to four seconds per event,
/// and a bulk batch of [`crate::memory::ingest::REMEMBER_BATCH`] is many.
/// This is the client's own default timeout; [`READ_TIMEOUT`] overrides it
/// per request for the calls on the review's critical path.
const TIMEOUT: Duration = Duration::from_secs(120);

/// How long `recall` or `answer` may take.
///
/// Neither waits on indexing the way a write does, and both run before every
/// lane — a review is best-effort without memory, but it must not queue
/// behind an engine that accepted the connection and then stopped
/// responding. [`TIMEOUT`]'s 120 seconds, held here, would occupy one of the
/// server's review permits for that long per lane that recalls or asks.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The header line that opens every event this adapter writes.
const HEADER: &str = "tinysweeper-memory:";

/// How many events one listing page asks for when forgetting a scope.
const PAGE_SIZE: usize = 200;

/// How many listing pages a forget will walk before giving up.
const MAX_PAGES: usize = 500;

/// A CortexDB deployment reached over HTTP.
pub struct CortexMemory {
    client: reqwest::Client,
    base_url: String,
}

impl std::fmt::Debug for CortexMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CortexMemory")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl CortexMemory {
    /// Build from the `[memory]` configuration, reading the key from the
    /// environment variable it names.
    pub fn from_config(config: &crate::config::types::Memory) -> Result<Self> {
        let key = std::env::var(&config.api_key_env).map_err(|_| {
            Error::config(format!(
                "memory.api_key_env names `{}`, which is not set",
                config.api_key_env
            ))
        })?;
        let endpoint = if config.endpoint.trim().is_empty() {
            CORTEX_API_ENDPOINT
        } else {
            config.endpoint.trim()
        };
        Self::new(endpoint, &key)
    }

    /// Connect to `endpoint` with `api_key` as the bearer.
    pub fn new(endpoint: &str, api_key: &str) -> Result<Self> {
        if api_key.trim().is_empty() {
            return Err(Error::config("the CortexDB API key is empty"));
        }
        crate::memory::endpoint_allowed(endpoint)
            .map_err(|reason| Error::config(format!("memory.endpoint: {reason}")))?;
        let mut value = HeaderValue::from_str(&format!("Bearer {}", api_key.trim()))
            .map_err(|_| Error::config("the CortexDB API key is not a valid header value"))?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(TIMEOUT)
            // This adapter only ever calls the one configured endpoint and
            // never needs a redirect to get there. `reqwest` already strips
            // `Authorization` across a host, port, or scheme change, but a
            // same-origin `307`/`308` still replays the request — including
            // this header — to wherever the engine's own response pointed.
            // Refusing every redirect keeps the bearer's destination exactly
            // the endpoint this process was configured with.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| Error::Model(format!("cortex: could not build client: {err}")))?;
        Ok(Self {
            client,
            base_url: endpoint.trim_end_matches('/').to_string(),
        })
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .client
            .post(format!("{}/{path}", self.base_url))
            .json(body)
            .send()
            .await
            .map_err(|err| Error::Model(format!("cortex: {path}: {}", scrub(&err.to_string()))))?;
        Self::read(path, response).await
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .client
            .get(format!("{}/{path}", self.base_url))
            .send()
            .await
            .map_err(|err| Error::Model(format!("cortex: {path}: {}", scrub(&err.to_string()))))?;
        Self::read(path, response).await
    }

    async fn read(path: &str, response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        parse_response(path, status, &text)
    }
}

/// The synchronous half of [`CortexMemory::read`]: given the status and body
/// text already read off the wire, decide what to return.
///
/// Split out so the leak this guards against — a response body reaching an
/// unavailable-memory note on a check-run summary — is a plain unit test
/// against strings, with no socket in the test suite the module doc promises
/// stays offline.
fn parse_response(path: &str, status: reqwest::StatusCode, text: &str) -> Result<Value> {
    if !status.is_success() {
        // The body is the engine's error envelope: not a credential, but
        // still the engine's own text, on a route this process does not
        // control. It goes to the operator's logs only. The error this call
        // returns carries just the route and status — that is what an
        // unavailable-memory note ends up quoting on the check-run summary,
        // and a one-line response body is not a stable, generic reason a
        // repository's collaborators should ever see there.
        let route = path.split('?').next().unwrap_or(path);
        tracing::warn!(
            route,
            %status,
            body = %crate::memory::excerpt(text, 200),
            "cortex answered with an error"
        );
        return Err(Error::Model(format!("cortex: {route} answered {status}")));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(text)
        .map_err(|err| Error::Model(format!("cortex: {path}: unparseable answer: {err}")))
}

/// One `type:id` scope segment, hex-encoded under a marked type when the id
/// falls outside the grammar's charset.
fn segment(kind: &str, id: &str) -> String {
    let safe = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if safe {
        format!("{kind}:{id}")
    } else {
        let hex: String = id.bytes().map(|b| format!("{b:02x}")).collect();
        format!("{kind}x:{hex}")
    }
}

/// The CortexDB scope path for a memory scope.
pub fn scope_path(scope: &MemoryScope) -> Result<String> {
    let (owner, name) = scope
        .repo
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty())
        .ok_or_else(|| Error::config(format!("`{}` is not owner/name", scope.repo)))?;
    let mut path = format!("{}/{}", segment("owner", owner), segment("repo", name));
    if let Some(section) = scope.section {
        path.push('/');
        path.push_str(&segment("section", section.as_str()));
    }
    Ok(path)
}

/// Percent-encode the characters the header line uses as structure.
fn enc(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '%' => out.push_str("%25"),
            ';' => out.push_str("%3B"),
            '\n' => out.push_str("%0A"),
            '=' => out.push_str("%3D"),
            c => out.push(c),
        }
    }
    out
}

/// The inverse of [`enc`].
fn dec(value: &str) -> String {
    value
        .replace("%0A", "\n")
        .replace("%3B", ";")
        .replace("%3D", "=")
        .replace("%25", "%")
}

/// The text written for one item: a parseable header, then prose.
pub fn envelope(item: &MemoryItem) -> String {
    let mut out = String::with_capacity(item.body.len() + 160);
    out.push_str(HEADER);
    out.push_str(&format!(
        " kind={}; key={}",
        item.kind.as_str(),
        enc(&item.key)
    ));
    if let Some(path) = &item.path {
        out.push_str(&format!("; path={}", enc(path)));
    }
    if let Some(symbol) = &item.symbol {
        out.push_str(&format!("; symbol={}", enc(symbol)));
    }
    out.push('\n');
    out.push_str(&item.title);
    out.push_str("\n\n");
    out.push_str(item.body.trim_end());
    out.push('\n');
    out
}

/// Parse an event's text back into the item it was written from.
///
/// `None` for text this adapter did not write — an event somebody put in the
/// scope by hand, or one from a different writer — which recall skips rather
/// than guesses at.
pub fn parse_envelope(text: &str) -> Option<MemoryItem> {
    let start = text.find(HEADER)?;
    let rest = &text[start + HEADER.len()..];
    let (header, body) = rest.split_once('\n')?;
    let mut fields: BTreeMap<&str, String> = BTreeMap::new();
    for pair in header.split(';') {
        if let Some((k, v)) = pair.trim().split_once('=') {
            fields.insert(k.trim(), dec(v.trim()));
        }
    }
    let kind = match fields.get("kind").map(String::as_str) {
        Some("code") => MemoryKind::CodeChunk,
        Some("convention") => MemoryKind::Convention,
        Some("finding") => MemoryKind::ReviewFinding,
        Some("outcome") => MemoryKind::ReviewOutcome,
        _ => return None,
    };
    let key = fields.remove("key")?;
    let (title, body) = body.split_once("\n\n").unwrap_or((body, ""));
    let mut item = MemoryItem::new(key, kind, title.trim(), body.trim_end());
    item.path = fields.remove("path");
    item.symbol = fields.remove("symbol");
    Some(item)
}

/// The modality an item is written under. Code and conventions are
/// documents; what happened on a review is an observation, which the engine
/// treats as a dated fact about the world rather than as reference material.
fn modality(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::CodeChunk | MemoryKind::Convention => "document",
        MemoryKind::ReviewFinding | MemoryKind::ReviewOutcome => "observation",
    }
}

/// The `/v1/experience` body for one item.
fn experience(scope: &str, item: &MemoryItem) -> Value {
    let mut context = serde_json::Map::new();
    let mut labels: Vec<String> = vec![
        format!("kind:{}", item.kind.as_str()),
        "writer:tinysweeper".into(),
    ];
    labels.extend(item.labels.iter().map(|l| l.chars().take(64).collect()));
    labels.truncate(32);
    context.insert("labels".into(), json!(labels));
    if let Some(at) = &item.observed_at {
        context.insert("observed_at".into(), json!(at));
    }
    json!({
        "scope": scope,
        "modality": modality(item.kind),
        "content": { "kind": "text", "text": envelope(item) },
        "context": context,
        "directives": {
            "extract": ["facts", "entities", "beliefs", "episodes", "understanding"],
            "embed": "eager",
        },
        "idempotency_key": item.content_id(),
    })
}

/// One write receipt: whether the engine had already seen it.
fn replayed(receipt: &Value) -> bool {
    receipt
        .get("replayed_from_idempotency")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The recall budget: `limit` events and nothing from the extracted layers,
/// which carry no envelope and cannot come back as items.
fn events_only(limit: usize) -> Value {
    json!({
        "per_layer_limits": {
            "events": limit,
            "facts": 0,
            "beliefs": 0,
            "episodes": 0,
            "understanding": 0,
        }
    })
}

/// The recall budget behind an answer.
///
/// Weighted to events, because the events are the sections and outcomes
/// whose words the answer is asked to quote; the extracted layers are kept
/// because the facts and beliefs the engine drew from them are what make its
/// answer better than a pasted list. Measured: ten events is enough for the
/// section a question is about to be in the pack, and a pack that lacks it
/// answers "not enough information" however good the model.
fn answer_budget() -> Value {
    json!({
        "per_layer_limits": {
            "events": 10,
            "facts": 5,
            "beliefs": 3,
            "episodes": 2,
            "understanding": 2,
        }
    })
}

/// Strip anything that could be a query string or a key from an error text.
fn scrub(message: &str) -> String {
    crate::memory::excerpt(message.split('?').next().unwrap_or(message), 160)
}

/// The events layer of a recall answer.
fn events_of(answer: &Value) -> Vec<Value> {
    answer
        .pointer("/layers/events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The items in a pack's events layer, by event id.
///
/// The answer route cites event ids and nothing else — measured: a citation
/// is `{marker, layer, id, support_strength}` — so the path a citation is
/// about has to come from the pack the answer was built on, which this
/// process holds.
fn items_by_id(pack: &Value) -> BTreeMap<String, MemoryItem> {
    events_of(pack)
        .iter()
        .filter_map(|event| {
            let id = event.get("id").and_then(Value::as_str)?;
            let text = event.pointer("/content/text").and_then(Value::as_str)?;
            Some((id.to_string(), parse_envelope(text)?))
        })
        .collect()
}

/// A citation as the answer route reports it: a bare id or an object,
/// resolved against the pack's events for its path and text.
fn citation_of(value: &Value, index: usize, cited: &BTreeMap<String, MemoryItem>) -> Citation {
    let id = match value.as_str() {
        Some(id) => id.to_string(),
        None => value
            .get("id")
            .or_else(|| value.get("event_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("citation-{index}")),
    };
    let inline = value
        .get("content")
        .or_else(|| value.get("text"))
        .and_then(Value::as_str)
        .and_then(parse_envelope);
    let item = cited.get(&id).cloned().or(inline);
    Citation {
        id,
        path: item.as_ref().and_then(|item| item.path.clone()),
        excerpt: item.map(|item| crate::memory::excerpt(&item.title, 200)),
    }
}

#[async_trait]
impl Memory for CortexMemory {
    fn name(&self) -> &str {
        "cortex"
    }

    async fn health(&self) -> Result<()> {
        self.get("v1/admin/health").await.map(|_| ())
    }

    async fn remember(&self, scope: &MemoryScope, items: &[MemoryItem]) -> Result<RememberReport> {
        let mut report = RememberReport::default();
        if items.is_empty() {
            return Ok(report);
        }
        // Every item is filed under its own section, whatever scope the
        // caller named; a section scope that disagrees is a caller bug and is
        // refused rather than quietly re-filed.
        let mut bodies = Vec::with_capacity(items.len());
        for item in items {
            let filed = MemoryScope::section(scope.repo.clone(), item.section());
            if !scope.covers(&filed) {
                return Err(Error::Model(format!(
                    "memory item `{}` is a {} and cannot be filed under {scope}",
                    item.key,
                    item.kind.as_str()
                )));
            }
            bodies.push(experience(&scope_path(&filed)?, item));
        }
        if bodies.len() == 1 {
            let receipt = self.post("v1/experience?wait=indexed", &bodies[0]).await?;
            if replayed(&receipt) {
                report.replayed += 1;
            } else {
                report.written += 1;
            }
            return Ok(report);
        }
        let answer = self
            .post(
                "v1/experience/bulk?wait=indexed",
                &json!({ "items": bodies }),
            )
            .await?;
        let results = answer
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Model("cortex: bulk write returned no results".into()))?;
        if results.len() != items.len() {
            return Err(Error::Model(format!(
                "cortex: bulk write returned {} receipts for {} items",
                results.len(),
                items.len()
            )));
        }
        for receipt in results {
            if replayed(receipt) {
                report.replayed += 1;
            } else {
                report.written += 1;
            }
        }
        Ok(report)
    }

    async fn recall(
        &self,
        scope: &MemoryScope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Recollection>> {
        if limit == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let answer = self
            .post(
                "v1/recall",
                &json!({
                    "scope": scope_path(scope)?,
                    "query": query,
                    "budgets": events_only(limit),
                }),
            )
            .await?;
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for event in events_of(&answer) {
            let Some(text) = event.pointer("/content/text").and_then(Value::as_str) else {
                continue;
            };
            let Some(item) = parse_envelope(text) else {
                continue;
            };
            // A section scope must not return another section's items even if
            // the engine's scope matching is looser than ours.
            if let Some(section) = scope.section
                && item.section() != section
            {
                continue;
            }
            if !seen.insert(item.key.clone()) {
                continue;
            }
            out.push(Recollection { item, score: None });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn answer(
        &self,
        scope: &MemoryScope,
        question: &str,
        instructions: Option<&str>,
    ) -> Result<MemoryAnswer> {
        let scope_path = scope_path(scope)?;
        let pack = self
            .post(
                "v1/recall",
                &json!({
                    "scope": scope_path,
                    "query": question,
                    "budgets": answer_budget(),
                }),
            )
            .await?;
        let ungrounded = || MemoryAnswer {
            question: question.to_string(),
            answer: String::new(),
            citations: Vec::new(),
            model: None,
        };
        // A pack with no events is a scope the engine holds nothing for.
        // Asking for an answer anyway costs a model call to be told so.
        if events_of(&pack).is_empty() {
            return Ok(ungrounded());
        }
        let Some(pack_id) = pack.get("pack_id").and_then(Value::as_str) else {
            return Err(Error::Model(
                "cortex: recall answered without a pack_id".into(),
            ));
        };
        let mut body = json!({
            "scope": scope_path,
            "question": question,
            "use_pack_id": pack_id,
            "cite_sources": true,
            "include_context": true,
        });
        if let Some(instructions) = instructions {
            body["answer_instructions"] = json!(instructions);
        }
        let response = self.post("v1/answer", &body).await?;
        let text = response
            .get("answer")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            return Ok(ungrounded());
        }
        let cited = items_by_id(&pack);
        let citations = response
            .get("citations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, c)| citation_of(c, index, &cited))
            .collect();
        Ok(MemoryAnswer {
            question: question.to_string(),
            answer: text,
            citations,
            model: response
                .pointer("/diagnostics/answer_model")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    async fn forget(&self, scope: &MemoryScope) -> Result<u64> {
        // Every section, when the whole repository is named: the engine's
        // forget selector is by id, so the ids have to be listed first, and
        // listing is per scope path.
        let paths: Vec<String> = match scope.section {
            Some(_) => vec![scope_path(scope)?],
            None => MemorySection::ALL
                .iter()
                .map(|s| scope_path(&MemoryScope::section(scope.repo.clone(), *s)))
                .collect::<Result<_>>()?,
        };
        let mut gone = 0u64;
        for path in paths {
            let mut ids: Vec<String> = Vec::new();
            let mut cursor: Option<String> = None;
            // Whether the loop below ran out of pages before the engine
            // said there were no more. Left `true` on a scope that turns out
            // to have exactly `MAX_PAGES` pages and no more, which trades a
            // false-positive refusal on that one boundary size for never
            // reporting a truncated listing as if it were the whole scope.
            let mut truncated = true;
            for _ in 0..MAX_PAGES {
                let route = match &cursor {
                    Some(c) => format!(
                        "v1/events?scope={}&limit={PAGE_SIZE}&cursor={}",
                        urlencode(&path),
                        urlencode(c)
                    ),
                    None => format!("v1/events?scope={}&limit={PAGE_SIZE}", urlencode(&path)),
                };
                let page = self.get(&route).await?;
                for item in page
                    .get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(id) = item.get("id").and_then(Value::as_str)
                        && !ids.iter().any(|known| known == id)
                    {
                        ids.push(id.to_string());
                    }
                }
                match next_page_cursor(&page) {
                    Some(next) => cursor = Some(next),
                    None => {
                        truncated = false;
                        break;
                    }
                }
            }
            if truncated {
                // Deleting a partial listing and reporting `Ok(gone)` would
                // tell a caller — including `tinysweeper memory forget` — that
                // a scope with more than `MAX_PAGES * PAGE_SIZE` events was
                // wholly forgotten when most of it was left behind.
                return Err(Error::Model(format!(
                    "cortex: forget: {path} has more than {} events; refusing a partial delete",
                    MAX_PAGES * PAGE_SIZE
                )));
            }
            if ids.is_empty() {
                continue;
            }
            // Ids, never an empty selector: an empty selector means the whole
            // scope to the engine, and "the whole scope" is what this method
            // is *for* — but only by listing it, so a scope the listing could
            // not read is never wiped on the strength of a wildcard.
            for batch in ids.chunks(PAGE_SIZE) {
                self.post(
                    "v1/forget",
                    &json!({
                        "scope": path,
                        "layers": ["events"],
                        "selector": { "memory_ids": batch },
                        "audit_note": "tinysweeper: forget scope",
                    }),
                )
                .await?;
                gone += batch.len() as u64;
            }
        }
        Ok(gone)
    }
}

/// Whether a `/v1/events` listing page says there is another page, and if
/// so, the cursor to fetch it with.
///
/// Kept separate from the HTTP loop in [`CortexMemory::forget`] so the
/// truncation case it guards against — running out of `MAX_PAGES` while the
/// engine still says there is more — is a plain unit test over JSON, not
/// something that needs hundreds of real HTTP round trips to exercise.
fn next_page_cursor(page: &Value) -> Option<String> {
    let next = page
        .get("next_cursor")
        .and_then(Value::as_str)
        .map(str::to_string);
    match (page.get("has_more").and_then(Value::as_bool), next) {
        (Some(true), Some(next)) => Some(next),
        _ => None,
    }
}

/// Percent-encode a query-string value.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_paths_follow_the_grammar_and_encode_what_does_not_fit() {
        assert_eq!(
            scope_path(&MemoryScope::repo("tinyhumansai/tinysweeper")).unwrap(),
            "owner:tinyhumansai/repo:tinysweeper"
        );
        assert_eq!(
            scope_path(&MemoryScope::section("o/r", MemorySection::Reviews)).unwrap(),
            "owner:o/repo:r/section:reviews"
        );
        let dotted = scope_path(&MemoryScope::repo("o/tiny.place")).unwrap();
        assert!(dotted.starts_with("owner:o/repox:"), "{dotted}");
        assert!(scope_path(&MemoryScope::repo("no-slash")).is_err());
    }

    #[test]
    fn next_page_cursor_follows_has_more_and_next_cursor_together() {
        assert_eq!(
            next_page_cursor(&json!({"has_more": true, "next_cursor": "c2"})),
            Some("c2".to_string())
        );
        // Any of these must stop the walk: a missing `has_more`, an explicit
        // `false`, or `has_more: true` with no cursor to actually continue.
        assert_eq!(next_page_cursor(&json!({})), None);
        assert_eq!(
            next_page_cursor(&json!({"has_more": false, "next_cursor": "c2"})),
            None
        );
        assert_eq!(next_page_cursor(&json!({"has_more": true})), None);
    }

    #[test]
    fn a_listing_that_never_stops_would_exhaust_max_pages_still_truncated() {
        // Regression for `forget` reporting success on a partial delete: if
        // every page this engine could return keeps saying "there is more",
        // the `for _ in 0..MAX_PAGES` loop in `forget` runs out without ever
        // taking the `None` branch that clears `truncated`. This proves that
        // exhaustion, over `MAX_PAGES` calls to the same decision function
        // `forget` uses per page — no HTTP involved.
        let always_more = json!({"has_more": true, "next_cursor": "same"});
        for _ in 0..MAX_PAGES {
            assert_eq!(
                next_page_cursor(&always_more),
                Some("same".to_string()),
                "a page that always says more must never look exhausted on its own"
            );
        }
    }

    #[test]
    fn an_error_response_body_never_reaches_the_returned_error() {
        // Regression: the error this returns is what an unavailable-memory
        // note quotes on a check-run summary, so the engine's response body
        // — its own text, not ours to publish — must never appear in it,
        // even though it is still worth a log line for the operator.
        let err = parse_response(
            "v1/recall",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream dial tcp 10.0.4.12:5432: connection refused (secret-project-x)",
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("10.0.4.12"), "{err}");
        assert!(!err.contains("secret-project-x"), "{err}");
        assert!(err.contains("v1/recall"), "{err}");
        assert!(err.contains("500"), "{err}");
    }

    #[test]
    fn a_query_string_is_stripped_from_the_route_an_error_names() {
        let err = parse_response(
            "v1/recall?scope=owner:o/repo:r&secret=shh",
            reqwest::StatusCode::FORBIDDEN,
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("secret=shh"), "{err}");
        assert!(err.contains("v1/recall"), "{err}");
    }

    #[test]
    fn envelopes_round_trip_through_a_speaker_prefixed_recall() {
        let item = MemoryItem::new(
            "convention:AGENTS.md#a=b;c",
            MemoryKind::Convention,
            "AGENTS.md › Rules",
            "Never unwrap.\n\nReturn Result.\n",
        )
        .at_path("dir/AGENTS.md")
        .at_symbol("Sym");
        let text = envelope(&item);
        let rendered = format!("[user] {text}");
        let back = parse_envelope(&rendered).unwrap();
        assert_eq!(back.key, item.key);
        assert_eq!(back.kind, item.kind);
        assert_eq!(back.path.as_deref(), Some("dir/AGENTS.md"));
        assert_eq!(back.symbol.as_deref(), Some("Sym"));
        assert_eq!(back.title, "AGENTS.md › Rules");
        assert_eq!(back.body, "Never unwrap.\n\nReturn Result.");
    }

    #[test]
    fn foreign_text_is_not_an_item() {
        assert!(parse_envelope("just some event").is_none());
        assert!(parse_envelope("tinysweeper-memory: kind=alien; key=k\nt\n\nb").is_none());
    }

    #[test]
    fn a_write_carries_the_content_id_as_its_idempotency_key() {
        let item = MemoryItem::new("k", MemoryKind::CodeChunk, "t", "b").labelled("lang:rust");
        let body = experience("owner:o/repo:r/section:code", &item);
        assert_eq!(body["idempotency_key"], json!(item.content_id()));
        assert_eq!(body["modality"], json!("document"));
        assert!(
            body["context"]["labels"]
                .as_array()
                .unwrap()
                .contains(&json!("lang:rust"))
        );
        let outcome = MemoryItem::new("k", MemoryKind::ReviewOutcome, "t", "b");
        assert_eq!(experience("s", &outcome)["modality"], json!("observation"));
    }

    #[test]
    fn a_plain_http_endpoint_off_loopback_is_refused_and_the_key_never_debugs() {
        assert!(CortexMemory::new("http://cortex.internal:3141", "k").is_err());
        assert!(CortexMemory::new("https://api-v1.cortexdb.ai", "").is_err());
        let memory = CortexMemory::new("http://127.0.0.1:3141", "sk-secret-value").unwrap();
        let debug = format!("{memory:?}");
        assert!(!debug.contains("sk-secret-value"));
        assert!(debug.contains("127.0.0.1:3141"));
    }

    #[test]
    fn budgets_are_shaped_as_the_engine_expects() {
        let only = events_only(7);
        assert_eq!(only["per_layer_limits"]["events"], json!(7));
        assert_eq!(only["per_layer_limits"]["facts"], json!(0));
        let spread = answer_budget();
        assert_eq!(spread["per_layer_limits"]["events"], json!(10));
        assert!(spread["per_layer_limits"]["facts"].as_u64().unwrap() > 0);
    }

    #[test]
    fn citations_resolve_against_the_pack_and_accept_inline_objects() {
        let item = MemoryItem::new("k", MemoryKind::Convention, "AGENTS.md › Rules", "b")
            .at_path("AGENTS.md");
        let pack = json!({ "layers": { "events": [
            { "id": "ev-1", "content": { "text": envelope(&item) } },
            { "id": "ev-x", "content": { "text": "not ours" } },
        ]}});
        let cited = items_by_id(&pack);
        assert_eq!(cited.len(), 1);

        // What the engine actually sends: marker, layer, id, strength.
        let live = citation_of(
            &json!({ "marker": "[S1]", "layer": "event", "id": "ev-1", "support_strength": 1.0 }),
            0,
            &cited,
        );
        assert_eq!(live.id, "ev-1");
        assert_eq!(live.path.as_deref(), Some("AGENTS.md"));
        assert_eq!(live.excerpt.as_deref(), Some("AGENTS.md › Rules"));

        let bare = citation_of(&json!("ev-1"), 0, &cited);
        assert_eq!(bare.path.as_deref(), Some("AGENTS.md"));
        let inline = citation_of(
            &json!({ "event_id": "ev-2", "content": envelope(&item) }),
            1,
            &cited,
        );
        assert_eq!(inline.path.as_deref(), Some("AGENTS.md"));
        let anon = citation_of(&json!({}), 3, &cited);
        assert_eq!(anon.id, "citation-3");
        assert!(anon.path.is_none());
    }

    #[test]
    fn urlencode_escapes_the_scope_separators() {
        assert_eq!(urlencode("owner:o/repo:r"), "owner%3Ao%2Frepo%3Ar");
    }
}
