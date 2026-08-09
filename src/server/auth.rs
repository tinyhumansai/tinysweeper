//! GitHub App authentication.
//!
//! Two credentials, and the distinction matters. The **app JWT** is signed with
//! the private key and proves who the app is; it can enumerate installations
//! and nothing else. An **installation token** is what actually touches a
//! repository, lasts an hour, and is scoped to one installation.
//!
//! Installation tokens are cached until shortly before they expire. Minting one
//! per webhook would be a round trip on the critical path of every delivery,
//! and GitHub rate-limits the exchange.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::error::{Error, Result};

/// Renew a token this long before it actually expires.
///
/// A token that expires mid-request fails the request, and a review that dies
/// two thirds of the way through has already spent the money.
const RENEW_MARGIN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
    expires_at: String,
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires: SystemTime,
}

/// Mints and caches GitHub App credentials.
pub struct AppAuth {
    app_id: String,
    key: EncodingKey,
    cache: Arc<Mutex<HashMap<u64, CachedToken>>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for AppAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never derive: the encoding key would be printed by any log line that
        // formats this.
        f.debug_struct("AppAuth")
            .field("app_id", &self.app_id)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl AppAuth {
    /// Build from an app id and a PEM private key.
    pub fn new(app_id: impl Into<String>, pem: &str) -> Result<Self> {
        let key = EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(|err| {
            Error::Forge(format!("the app private key is not a usable PEM: {err}"))
        })?;
        Self::with_key(app_id, key)
    }

    /// Build from an app id and a DER private key.
    pub fn from_der(app_id: impl Into<String>, der: &[u8]) -> Result<Self> {
        Self::with_key(app_id, EncodingKey::from_rsa_der(der))
    }

    fn with_key(app_id: impl Into<String>, key: EncodingKey) -> Result<Self> {
        Ok(Self {
            app_id: app_id.into(),
            key,
            cache: Arc::new(Mutex::new(HashMap::new())),
            http: reqwest::Client::builder()
                .user_agent("tinysweeper")
                .build()
                .map_err(|err| Error::Forge(err.to_string()))?,
        })
    }

    /// Build from `TINYSWEEPER_APP_ID` and `TINYSWEEPER_APP_PRIVATE_KEY`.
    pub fn from_env() -> Result<Self> {
        let app_id = std::env::var("TINYSWEEPER_APP_ID")
            .map_err(|_| Error::Forge("TINYSWEEPER_APP_ID is not set".into()))?;
        let pem = std::env::var("TINYSWEEPER_APP_PRIVATE_KEY")
            .map_err(|_| Error::Forge("TINYSWEEPER_APP_PRIVATE_KEY is not set".into()))?;
        Self::new(app_id, &pem)
    }

    /// A short-lived JWT proving who the app is.
    pub fn app_jwt(&self) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| Error::Forge(err.to_string()))?
            .as_secs();

        let claims = Claims {
            // Backdated by a minute: GitHub rejects a JWT whose `iat` is in the
            // future, and server clocks drift.
            iat: now.saturating_sub(60),
            exp: now + 9 * 60,
            iss: self.app_id.clone(),
        };

        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|err| Error::Forge(format!("could not sign the app JWT: {err}")))
    }

    /// Which installation covers a repository.
    ///
    /// A webhook delivery names its installation; an operator pressing a button
    /// does not, so the manual review path asks GitHub. The app JWT is the right
    /// credential here — this is a question about the app, and answering it with
    /// an installation token would need the answer first.
    pub async fn installation_for_repo(&self, owner: &str, name: &str) -> Result<u64> {
        let response = self
            .http
            .get(format!(
                "https://api.github.com/repos/{owner}/{name}/installation"
            ))
            .bearer_auth(self.app_jwt()?)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|err| Error::Forge(format!("installation lookup failed: {err}")))?;

        if !response.status().is_success() {
            let status = response.status();
            // A 404 here is the ordinary case of an app that is not installed
            // on the repository, so say that rather than echoing GitHub's body.
            return Err(Error::Forge(format!(
                "no installation for `{owner}/{name}` (GitHub answered {status}); \
                 is tinysweeper installed on it?"
            )));
        }

        let body = response
            .bytes()
            .await
            .map_err(|err| Error::Forge(format!("installation lookup failed: {err}")))?;
        parse_installation(&body)
    }

    /// An installation token, from cache when one is still good.
    pub async fn installation_token(&self, installation: u64) -> Result<String> {
        if let Some(cached) = self.cache.lock().await.get(&installation)
            && cached.expires > SystemTime::now() + RENEW_MARGIN
        {
            return Ok(cached.token.clone());
        }

        let jwt = self.app_jwt()?;
        let response = self
            .http
            .post(format!(
                "https://api.github.com/app/installations/{installation}/access_tokens"
            ))
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|err| Error::Forge(format!("token exchange failed: {err}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Forge(format!(
                "token exchange returned {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }

        let minted: TokenResponse = response
            .json()
            .await
            .map_err(|err| Error::Forge(format!("token exchange returned junk: {err}")))?;

        let expires = parse_expiry(&minted.expires_at)
            .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(3600));

        self.cache.lock().await.insert(
            installation,
            CachedToken {
                token: minted.token.clone(),
                expires,
            },
        );

        Ok(minted.token)
    }
}

/// The id GitHub reports for the installation covering a repository.
#[derive(Debug, Deserialize)]
struct InstallationRef {
    id: u64,
}

/// Read an installation id out of GitHub's answer.
///
/// Split from the request so the parsing is testable offline: the network half
/// is three lines and the same as every other call here.
fn parse_installation(body: &[u8]) -> Result<u64> {
    serde_json::from_slice::<InstallationRef>(body)
        .map(|found| found.id)
        .map_err(|err| {
            Error::Forge(format!(
                "could not read the installation from GitHub: {err}"
            ))
        })
}

/// Parse GitHub's RFC 3339 expiry into a `SystemTime`.
///
/// Hand-rolled rather than pulling in a date library for one field. A value we
/// cannot parse yields `None`, and the caller falls back to an hour — which is
/// what GitHub issues anyway, so the failure mode is a slightly early renewal.
fn parse_expiry(text: &str) -> Option<SystemTime> {
    let (date, rest) = text.split_once('T')?;
    let time = rest.trim_end_matches('Z');

    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;

    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next().unwrap_or("0").split('.').next()?.parse().ok()?;

    // Days since the Unix epoch, by the civil-from-days algorithm.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    (seconds > 0).then(|| UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::server::test_key::test_key_der;

    fn auth(app_id: &str) -> AppAuth {
        AppAuth::from_der(app_id, &test_key_der()).expect("builds")
    }

    #[test]
    fn a_signed_jwt_has_three_parts_and_the_app_id_as_issuer() {
        let auth = auth("4526585");
        let jwt = auth.app_jwt().expect("signs");

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT is header.payload.signature");

        let payload = parts[1];
        let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
        let decoded = base64_decode(&padded).expect("decodes");
        let json: serde_json::Value = serde_json::from_slice(&decoded).expect("parses");
        assert_eq!(json["iss"], "4526585");
    }

    #[test]
    fn the_jwt_is_backdated_because_server_clocks_drift() {
        let jwt = auth("1").app_jwt().expect("signs");
        let payload = jwt.split('.').nth(1).unwrap();
        let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
        let json: serde_json::Value =
            serde_json::from_slice(&base64_decode(&padded).unwrap()).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let iat = json["iat"].as_u64().unwrap();
        assert!(iat < now, "GitHub rejects a JWT issued in the future");
        assert!(
            json["exp"].as_u64().unwrap() <= now + 600,
            "max lifetime is 10 minutes"
        );
    }

    #[test]
    fn a_key_that_is_not_a_pem_fails_with_a_useful_message() {
        let err = AppAuth::new("1", "not a key").unwrap_err().to_string();
        assert!(err.contains("not a usable PEM"), "{err}");
    }

    #[test]
    fn debug_never_prints_the_private_key() {
        let rendered = format!("{:?}", auth("1"));
        assert!(!rendered.contains("PRIVATE"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn github_expiry_timestamps_parse() {
        let parsed = parse_expiry("2026-08-08T14:30:00Z").expect("parses");
        let seconds = parsed.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(seconds, 1_786_199_400);

        // A leap year, and a date before March, which is where the civil-from-
        // days algorithm is easiest to get wrong.
        let leap = parse_expiry("2024-02-29T00:00:00Z").expect("parses");
        assert_eq!(
            leap.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_709_164_800
        );
    }

    #[test]
    fn an_installation_lookup_reads_the_id_out_of_githubs_answer() {
        // The manual review route has no webhook to tell it which installation
        // covers a repository, so it asks GitHub. This is the parsing half.
        let body = br#"{"id": 98765, "account": {"login": "tinyhumansai"}}"#;
        assert_eq!(parse_installation(body).expect("parses"), 98765);
    }

    #[test]
    fn an_installation_lookup_that_answers_junk_fails_loudly() {
        let err = parse_installation(b"<html>not json</html>").unwrap_err();
        assert!(err.to_string().contains("installation"), "{err}");
    }

    #[test]
    fn an_unparseable_expiry_yields_none_rather_than_a_wrong_time() {
        // The caller then falls back to an hour, which is what GitHub issues
        // anyway — so the failure mode is renewing slightly early.
        assert!(parse_expiry("whenever").is_none());
        assert!(parse_expiry("").is_none());
    }

    fn base64_decode(text: &str) -> Option<Vec<u8>> {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = Vec::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for byte in text.bytes() {
            if byte == b'=' {
                break;
            }
            let value = TABLE.iter().position(|c| *c == byte)? as u32;
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buffer >> bits) as u8);
            }
        }
        Some(out)
    }
}
