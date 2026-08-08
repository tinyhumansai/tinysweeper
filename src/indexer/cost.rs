//! What embedding costs.
//!
//! Always compiled.
//!
//! This exists because nothing upstream accounts for it. The agent harness
//! tracks tokens and dollars for *completions* and its budget middleware gates
//! on them; embedding calls go through a different client and are counted
//! nowhere at all. Indexing a large monorepo is easily tens of millions of
//! tokens, so "not counted" is not the same as "not much" — it is an unpriced
//! surprise on somebody's invoice, and the first sign of it is the invoice.
//!
//! Two things are honest about the numbers here. The token count is an
//! *estimate*, because the [`Embedder`](crate::ports::embed::Embedder) port
//! reports vectors and not usage — see [`estimate_tokens`]. And a model with no
//! price on file costs zero and says so in a warning, rather than being given
//! an invented rate that would make a budget check meaningless.

use serde::{Deserialize, Serialize};

/// What a set of embedding calls cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct EmbedUsage {
    /// How many calls were made to the provider.
    pub calls: u64,
    /// How many tokens were sent, estimated.
    pub tokens: u64,
    /// What that comes to in USD, at the rates in [`PRICES`].
    pub cost_usd: f64,
}

impl EmbedUsage {
    /// Fold another usage into this one.
    pub fn add(&mut self, other: EmbedUsage) {
        self.calls += other.calls;
        self.tokens += other.tokens;
        self.cost_usd += other.cost_usd;
    }

    /// The usage of one call embedding `texts` under `model`.
    pub fn of_call(model: &str, texts: &[String]) -> Self {
        let tokens: u64 = texts.iter().map(|text| estimate_tokens(text)).sum();
        Self {
            calls: 1,
            tokens,
            cost_usd: cost_of(model, tokens),
        }
    }
}

/// Per-million-token embedding prices, in USD, verified 2026-08-08.
///
/// Keyed by `provider:model` — the same two halves as
/// [`EmbedSignature`](crate::index::EmbedSignature) — because model ids collide
/// across providers and a rate is a property of the pair.
pub const PRICES: &[(&str, f64)] = &[
    ("voyage:voyage-code-3", 0.18),
    ("voyage:voyage-3-large", 0.18),
    ("voyage:voyage-3.5-lite", 0.02),
    ("openai:text-embedding-3-small", 0.02),
    ("openai:text-embedding-3-large", 0.13),
    ("cohere:embed-v4.0", 0.12),
    // The offline mock embeds locally and genuinely costs nothing, so it is
    // priced rather than left unknown — otherwise every test run would log a
    // warning about a model we wrote ourselves.
    ("mock:hash-bag", 0.0),
];

/// What `tokens` tokens cost under `model`, in USD.
///
/// `model` is `provider:model`, or a full signature key
/// (`provider:model:dims`) — the dimensionality is dropped, since it does not
/// change the rate.
pub fn cost_of(model: &str, tokens: u64) -> f64 {
    let key = strip_dims(model);
    let Some((_, per_million)) = PRICES.iter().find(|(id, _)| *id == key) else {
        tracing::warn!(
            model = key,
            "no embedding price known for this model; indexing will not count against the budget"
        );
        return 0.0;
    };
    tokens as f64 * per_million / 1_000_000.0
}

/// Drop a trailing `:dims` from a signature key.
fn strip_dims(model: &str) -> &str {
    match model.rsplit_once(':') {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty() => head,
        _ => model,
    }
}

/// Estimate the tokens in `text`.
///
/// Four bytes per token. That is the usual rule of thumb for English prose and
/// it is a slight *under*-estimate for code, which is dense in punctuation —
/// so the figure is labelled an estimate everywhere it surfaces, and a budget
/// set from it should be set with headroom. Counting exactly would mean
/// shipping a tokenizer per provider to price a call we have already made,
/// which buys accuracy in the wrong place.
pub fn estimate_tokens(text: &str) -> u64 {
    // Rounds up, so a short text is never free.
    text.len().div_ceil(4) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_priced_model_costs_what_the_table_says() {
        // 4 MB of code at four bytes a token is a million tokens; at
        // voyage-code-3's $0.18/M that is eighteen cents, and a repository
        // that size is not unusual.
        let cost = cost_of("voyage:voyage-code-3", 1_000_000);
        assert!((cost - 0.18).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn a_signature_key_prices_the_same_as_the_bare_model() {
        assert_eq!(
            cost_of("voyage:voyage-code-3:1024", 500_000),
            cost_of("voyage:voyage-code-3", 500_000)
        );
    }

    #[test]
    fn an_unknown_model_costs_zero_rather_than_a_guess() {
        assert_eq!(cost_of("someone:unreleased-embed", 10_000_000), 0.0);
    }

    #[test]
    fn a_model_whose_name_ends_in_digits_is_not_mistaken_for_a_dimension() {
        // `voyage-3-large` has no trailing numeric segment, but a model like
        // `openai:text-embedding-3` would; only a *colon*-separated all-digit
        // tail is treated as dimensionality.
        assert_eq!(strip_dims("voyage:voyage-3-large"), "voyage:voyage-3-large");
        assert_eq!(strip_dims("p:m:1024"), "p:m");
        assert_eq!(strip_dims("bare"), "bare");
    }

    #[test]
    fn usage_accumulates_across_calls() {
        let mut total = EmbedUsage::default();
        total.add(EmbedUsage::of_call(
            "voyage:voyage-code-3",
            &["x".repeat(4_000)],
        ));
        total.add(EmbedUsage::of_call(
            "voyage:voyage-code-3",
            &["y".repeat(4_000)],
        ));
        assert_eq!(total.calls, 2);
        assert_eq!(total.tokens, 2_000);
        assert!(total.cost_usd > 0.0);
    }

    #[test]
    fn a_short_text_is_never_free() {
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("12345"), 2);
    }
}
