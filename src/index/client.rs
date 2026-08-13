//! How every MongoDB client in tinysweeper is configured. Requires `serve`.
//!
//! There are three of them — `server::store`, `index::mongo` and
//! `indexer::mongo` — and until this module existed all three were built with a
//! bare `Client::with_uri_str`, which means every one of them ran on driver
//! defaults nobody had chosen.
//!
//! Two of those defaults were actively wrong for this workload, and together
//! they took the server down for a minute and a half.
//!
//! **`max_pool_size` defaults to 10.** `index::mongo::UPSERT_CONCURRENCY` fires
//! **32** concurrent upserts, so a graph write ran three times as many
//! operations as it had connections to run them on. For a small repository that
//! is a queue nobody notices; for openhuman's 323,209 nodes it is a sustained
//! backlog, and operations at the back of it aged out. When one failed on a
//! network timeout the driver did what SDAM says to do — marked the server
//! Unknown and **cleared the pool** — which failed every other in-flight
//! operation with it. That is the
//! `Connection pool … cleared because another operation failed` in the logs.
//!
//! **`server_selection_timeout` defaults to 30 seconds.** GitHub gives a
//! webhook **10**. So once the server was marked Unknown, a webhook handler
//! that touched Mongo could not fail fast enough to answer in time — it sat in
//! server selection for three times GitHub's budget. GitHub gave up after one
//! attempt and the `pull_request opened` for tinyflows#51 was lost outright.
//!
//! So the pool is sized to the concurrency that uses it, and every timeout is
//! shorter than the deadline of the thing waiting on it.

use std::time::Duration;

use mongodb::Client;
use mongodb::options::ClientOptions;

use crate::error::{Error, Result};

/// Connections per client pool.
///
/// Must be at least `index::mongo::UPSERT_CONCURRENCY`, and is deliberately a
/// little above it: the graph write is not the only thing using the pool, and a
/// pool sized to exactly its heaviest user leaves a lease renewal queued behind
/// a bulk upsert. There are three clients, so this is a third of the
/// connections tinysweeper opens.
pub const MAX_POOL_SIZE: u32 = 48;

/// Connections kept warm.
///
/// Establishing a TLS connection to Atlas is not free, and the webhook path
/// pays that cost on the request GitHub is timing. Keeping a few open means a
/// delivery arriving after an idle period is not also paying for a handshake.
pub const MIN_POOL_SIZE: u32 = 4;

/// How long an operation may wait for a usable server.
///
/// **Five seconds, and the number is chosen against GitHub's webhook timeout
/// of ten.** This is the one that has to fail fast: if Mongo is unreachable,
/// the webhook handler needs an error while it still has time to answer, not a
/// 30-second wait that guarantees a dropped delivery. Halving GitHub's budget
/// leaves room for the rest of the handler.
pub const SERVER_SELECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// How long establishing a connection may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long GitHub waits for a webhook response before giving up.
///
/// Not ours to change — it is GitHub's, recorded here because two of the
/// timeouts above are chosen against it and a test asserts they stay under it.
/// The delivery log is explicit that exceeding it is unrecoverable:
/// `giving up after 1 attempt(s)`.
pub const GITHUB_WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect to `uri` with the options above.
///
/// Every client in the crate goes through here. A bare
/// `Client::with_uri_str` elsewhere is a bug: it silently opts that client out
/// of the pool sizing and the timeouts, and the failure it produces shows up
/// somewhere else entirely — as a dropped webhook, not as a slow index.
///
/// Options set in the URI still win. The connection string is deployment
/// configuration and an operator who has tuned it should not have to discover
/// that the binary quietly overrode them; these are defaults for a URI that
/// says nothing, which is what every tinysweeper URI says today.
pub async fn connect(uri: &str) -> Result<Client> {
    let options = options_for(uri).await?;
    Client::with_options(options)
        .map_err(|err| Error::Forge(format!("could not reach MongoDB: {err}")))
}

/// The options [`connect`] will use for `uri`.
///
/// Split out so the defaults can be asserted on directly. Building a client
/// and inspecting it proves less: the driver does not hand back what it was
/// configured with, so a test against a live client can only check that
/// construction succeeded, which it would with any values at all.
async fn options_for(uri: &str) -> Result<ClientOptions> {
    let mut options = ClientOptions::parse(uri)
        .await
        .map_err(|err| Error::Forge(format!("could not parse the MongoDB URI: {err}")))?;

    if options.max_pool_size.is_none() {
        options.max_pool_size = Some(MAX_POOL_SIZE);
    }
    if options.min_pool_size.is_none() {
        options.min_pool_size = Some(MIN_POOL_SIZE);
    }
    if options.server_selection_timeout.is_none() {
        options.server_selection_timeout = Some(SERVER_SELECTION_TIMEOUT);
    }
    if options.connect_timeout.is_none() {
        options.connect_timeout = Some(CONNECT_TIMEOUT);
    }
    // Names the process in Atlas's connection and profiler views. When three
    // pools misbehave at once, knowing which one is worth more than the byte
    // it costs.
    if options.app_name.is_none() {
        options.app_name = Some("tinysweeper".to_string());
    }

    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URI that parses offline. `mongodb://` rather than `mongodb+srv://`
    /// because the latter resolves DNS at parse time, and the test suite never
    /// touches the network.
    const URI: &str = "mongodb://example.invalid:27017/?replicaSet=rs0";

    #[test]
    fn the_pool_is_at_least_as_large_as_the_upsert_concurrency() {
        // The bug this module exists for. A pool smaller than the concurrency
        // driving it is a queue, and a long enough queue is a cleared pool.
        // Coupled deliberately: raising `UPSERT_CONCURRENCY` without raising
        // the pool re-creates the outage, and this fails the moment it does.
        assert!(
            MAX_POOL_SIZE as usize >= crate::index::mongo::UPSERT_CONCURRENCY,
            "a pool of {MAX_POOL_SIZE} cannot serve {} concurrent upserts",
            crate::index::mongo::UPSERT_CONCURRENCY
        );
    }

    #[test]
    fn server_selection_fails_inside_githubs_webhook_budget() {
        // GitHub allows ten seconds and does not retry. A server-selection
        // timeout at or above that guarantees a dropped delivery whenever
        // Mongo is unwell, which is exactly how tinyflows#51 was lost.
        assert!(
            SERVER_SELECTION_TIMEOUT < GITHUB_WEBHOOK_TIMEOUT,
            "server selection must fail before GitHub gives up"
        );
        assert!(CONNECT_TIMEOUT < GITHUB_WEBHOOK_TIMEOUT);
    }

    #[tokio::test]
    async fn defaults_are_applied_to_a_uri_that_says_nothing() {
        // The fixture must not set what is under test, or the assertions
        // below would pass on the URI's values rather than ours.
        let bare = ClientOptions::parse(URI).await.expect("parses");
        assert!(bare.max_pool_size.is_none());
        assert!(bare.server_selection_timeout.is_none());

        let options = options_for(URI).await.expect("builds");
        assert_eq!(options.max_pool_size, Some(MAX_POOL_SIZE));
        assert_eq!(options.min_pool_size, Some(MIN_POOL_SIZE));
        assert_eq!(
            options.server_selection_timeout,
            Some(SERVER_SELECTION_TIMEOUT)
        );
        assert_eq!(options.connect_timeout, Some(CONNECT_TIMEOUT));
        assert_eq!(options.app_name.as_deref(), Some("tinysweeper"));
    }

    #[tokio::test]
    async fn a_tuned_uri_is_not_overridden_by_our_defaults() {
        // Deployment configuration wins. An operator who tuned the connection
        // string must not have it silently overridden by the binary.
        let tuned = "mongodb://example.invalid:27017/?maxPoolSize=7&serverSelectionTimeoutMS=1234";
        let options = options_for(tuned).await.expect("builds");
        assert_eq!(options.max_pool_size, Some(7));
        assert_eq!(
            options.server_selection_timeout,
            Some(Duration::from_millis(1234))
        );
        // Untouched by the URI, so ours still applies.
        assert_eq!(options.min_pool_size, Some(MIN_POOL_SIZE));
    }
}
