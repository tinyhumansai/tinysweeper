//! The real Sentry HTTP adapter. Behind the `sentry` feature.
//!
//! The default build links no HTTP client and the test suite never touches the
//! network, so everything in this file is gated and nothing else in
//! `src/sentry/` imports it. The pipeline is written against
//! [`crate::ports::sentry::SentryApi`] and tested against
//! [`crate::sentry::mock::MockSentry`]; this is the one place that speaks to
//! sentry.io.
//!
//! ## The token is read from the environment, once, by name
//!
//! `sentry.token_env` holds the *name* of the variable, never the token —
//! `config::validate` enforces that the field is non-empty and `app::doctor`
//! reports whether the named variable is actually set. The value is read here
//! and kept in the client's default headers; it is never logged, never put in
//! an error message, and never returned through the port.
//!
//! ## Deserialization is the allow-list
//!
//! Responses are parsed straight into [`crate::sentry::types`], which declares
//! only promotable fields. There is deliberately no intermediate
//! `serde_json::Value`: the personal data in a Sentry event is never
//! materialised in this process at all, rather than being materialised and
//! then filtered.

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

use crate::error::{Error, Result};
use crate::ports::sentry::SentryApi;
use crate::sentry::types::{RawEvent, RawIssue};

/// How long any one Sentry call may take.
const TIMEOUT_SECS: u64 = 30;

/// A Sentry installation reached over HTTP.
///
/// The `Debug` derive is safe: `reqwest::Client` renders as an opaque handle
/// and does not print its default headers, so the bearer token cannot reach a
/// log through it. The token is additionally marked sensitive on the header
/// value itself.
#[derive(Debug)]
pub struct SentryClient {
    client: reqwest::Client,
    base_url: String,
    org: String,
}

impl SentryClient {
    /// Build from the `[sentry]` configuration, reading the token from the
    /// environment variable `token_env` names.
    ///
    /// # Errors
    ///
    /// When the named variable is unset, or the token cannot be used as a
    /// header value. The error names the *variable*, never the value.
    pub fn from_config(config: &crate::config::types::Sentry) -> Result<Self> {
        let org = config
            .org
            .as_deref()
            .filter(|org| !org.trim().is_empty())
            .ok_or_else(|| Error::config("`sentry.org` is required to reach Sentry"))?;

        let token = std::env::var(&config.token_env).map_err(|_| {
            Error::config(format!(
                "`{}` is not set; it holds the Sentry auth token named by `sentry.token_env`",
                config.token_env
            ))
        })?;

        Self::from_parts(config, org, &token)
    }

    /// Build from an already-resolved token.
    ///
    /// Split out of [`Self::from_config`] so the token can be supplied
    /// directly. `from_config`'s only extra job is reading the environment,
    /// and a test that wants to assert something about the *client* should not
    /// have to mutate process-global state to get one — `set_var` races every
    /// concurrent `getenv` in the process, including ones inside dependencies,
    /// which is why Rust 2024 made it `unsafe`.
    fn from_parts(config: &crate::config::types::Sentry, org: &str, token: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            // Deliberately does not echo the value.
            Error::config(format!(
                "the token in `{}` is not a usable HTTP header value",
                config.token_env
            ))
        })?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .default_headers(headers)
            .build()
            .map_err(|err| Error::config(format!("could not build the Sentry client: {err}")))?;

        Ok(Self {
            client,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            org: org.to_string(),
        })
    }

    /// Send a request and deserialize the response into `T`.
    ///
    /// A non-success status is an error naming the status and the path — never
    /// the body, which on a Sentry error can echo the request.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|err| Error::Forge(format!("sentry GET {path}: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Forge(format!("sentry GET {path}: HTTP {status}")));
        }

        response
            .json::<T>()
            .await
            .map_err(|err| Error::Forge(format!("sentry GET {path}: unusable response: {err}")))
    }
}

#[async_trait]
impl SentryApi for SentryClient {
    async fn unresolved_issues(&self, project: &str, limit: usize) -> Result<Vec<RawIssue>> {
        let path = format!(
            "/projects/{}/{project}/issues/?query=is%3Aunresolved&statsPeriod=&limit={}",
            self.org,
            limit.min(100)
        );
        self.get(&path).await
    }

    async fn latest_event(&self, issue_id: &str) -> Result<Option<RawEvent>> {
        let path = format!("/issues/{issue_id}/events/latest/");
        match self.get::<RawEvent>(&path).await {
            Ok(event) => Ok(Some(event)),
            // Sentry expires event bodies on its own retention schedule while
            // keeping the issue. That is an ordinary answer, not a failure —
            // the issue is still worth promoting, just without frames.
            Err(Error::Forge(message)) if message.contains("HTTP 404") => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn annotate(&self, issue_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/issues/{issue_id}/comments/", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .map_err(|err| Error::Forge(format!("sentry comment on {issue_id}: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Forge(format!(
                "sentry comment on {issue_id}: HTTP {status}"
            )));
        }
        Ok(())
    }

    async fn resolve(&self, issue_id: &str) -> Result<()> {
        let url = format!("{}/issues/{issue_id}/", self.base_url);
        let response = self
            .client
            .put(&url)
            .json(&serde_json::json!({ "status": "resolved" }))
            .send()
            .await
            .map_err(|err| Error::Forge(format!("sentry resolve {issue_id}: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Forge(format!(
                "sentry resolve {issue_id}: HTTP {status}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Sentry;

    /// The construction paths are testable without a network: they only read
    /// configuration and the environment.
    #[test]
    fn a_missing_token_variable_is_refused_by_name() {
        let config = Sentry {
            org: Some("acme".into()),
            token_env: "TINYSWEEPER_TEST_DEFINITELY_UNSET".into(),
            ..Sentry::default()
        };

        let err = SentryClient::from_config(&config).expect_err("should refuse");
        let message = err.to_string();
        assert!(
            message.contains("TINYSWEEPER_TEST_DEFINITELY_UNSET"),
            "{message}"
        );
    }

    #[test]
    fn a_missing_org_is_refused() {
        let config = Sentry {
            org: Some("   ".into()),
            ..Sentry::default()
        };
        assert!(SentryClient::from_config(&config).is_err());
    }

    /// The `Debug` derive is only safe if `reqwest::Client` does not render
    /// its default headers. That is a property of a dependency, so it is
    /// asserted rather than assumed — a reqwest upgrade that started printing
    /// them would put a bearer token into every `{:?}` of this struct.
    #[test]
    fn debug_output_never_contains_the_token() {
        const TOKEN: &str = "sntrys_thisisatesttokenvalue_0123456789";

        // Built through `from_parts` rather than `from_config` so this test
        // mutates no process-global state. `#[test]` functions run in
        // parallel, and `set_var` is not merely racy with other *tests* — it
        // races every concurrent `getenv` in the process, including ones
        // inside dependencies. What is under test is the `Debug` rendering of
        // the client, which does not care where the token came from.
        let config = Sentry {
            org: Some("acme".into()),
            base_url: "https://sentry.io/api/0".into(),
            ..Sentry::default()
        };
        let client = SentryClient::from_parts(&config, "acme", TOKEN).expect("builds");

        let rendered = format!("{client:?}");
        assert!(
            !rendered.contains(TOKEN),
            "the auth token reached Debug output: {rendered}"
        );
    }
}
