//! What a call costs, in dollars.
//!
//! Always compiled, deliberately. The prices used to live next to the
//! `harness`-gated provider adapter, which meant the rules that decide whether
//! a run stays inside `models.budget_usd_per_pr` were never exercised by the
//! default `cargo test`. Budget enforcement is a safety property; it is tested
//! offline like every other one.
//!
//! ## Unpriced models are charged, not excused
//!
//! The table is data maintained by hand, so it *will* go stale — a new model
//! id, a fallback nobody added, a repository pinning something exotic. The
//! previous behaviour was to log a warning and return `0.0`, which made an
//! unknown model free and therefore unbounded: the one model whose cost nobody
//! had verified was the one model the budget could not stop.
//!
//! So an unpriced model is billed at [`ceiling`], the most expensive rate in
//! the table. Two alternatives were considered and rejected:
//!
//! - **Refuse the call.** The price is only consulted once a response has come
//!   back, so refusing then throws away work already paid for and turns a stale
//!   table into a failed review. Worse, it fires on the fallback path — the
//!   exact moment the primary provider is already down.
//! - **Refuse at startup**, rejecting a config naming an unpriced model. That
//!   makes a stale price table brick every review for a repository whose model
//!   is perfectly fine. [`unpriced`] exists so `doctor` can *report* this
//!   instead, which is the same information without the outage.
//!
//! Charging the ceiling over-reports spend for a cheap unknown model. That
//! error is bounded, visible in the cost line, and fixed by adding a row. The
//! error it replaces was unbounded.

/// Per-million-token prices for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    /// Fresh prompt tokens.
    pub input: f64,
    /// Generated tokens.
    pub output: f64,
    /// Prompt tokens served from the provider's cache.
    pub cached: f64,
}

/// Per-million-token prices, verified against openrouter.ai on 2026-08-09;
/// the two DeepSeek V4 Pro rows re-verified against the endpoint listing on
/// 2026-08-13.
///
/// tinyagents reports tokens but not cost, and the budget ceiling is
/// denominated in dollars, so the conversion happens here.
/// Entries exist for models this deployment does not currently select. That is
/// deliberate: `completion_cost` fails closed on an unpriced model, so a
/// one-line change to `models.deep` would otherwise take the budget ceiling
/// with it. Every model here was measured before being priced.
const MODEL_PRICES: &[(&str, Price)] = &[
    (
        // Not selected. Evaluated as a deep tier and rejected: cheaper per
        // token than kimi-k3 but it reasons 3-6x more, which spends the
        // difference — $0.00598 a call against $0.00556.
        "qwen/qwen3.8-max",
        Price {
            input: 2.00,
            output: 6.00,
            cached: 0.25,
        },
    ),
    (
        // Corrected 2026-08-13; the previous row was $0.07 / $0.22 / $0.013 and
        // understated the real bill by roughly **ten times**. No endpoint has
        // ever served GLM 5.2 at those rates: the cheapest of the thirty-three
        // on the gateway is Baidu at $0.392 / $1.232, and this deployment routes
        // to StreamLake at $0.753 / $2.367 / $0.1399.
        //
        // It mattered in two places, both quiet. `budget_usd_per_pr` is a hard
        // stop on a real bill enforced through this table, so it was failing
        // open by an order of magnitude. And the eval corpus records a cost per
        // case from here whenever the gateway reports none, which made every
        // GLM scorecard look ten times cheaper than the run really was —
        // including the baseline a model change gets compared against.
        //
        // Priced at the worst of the providers this account can reach
        // (DeepInfra $0.750, StreamLake $0.753, DigitalOcean $0.630 input), so
        // the ceiling errs toward stopping early rather than overspending.
        "z-ai/glm-5.2",
        Price {
            input: 0.753,
            output: 2.400,
            cached: 0.140,
        },
    ),
    (
        // Priced at the dearer of the two providers `models.provider` pins —
        // StreamLake at $1.32/$3.96 against DeepInfra's $1.30/$2.60.
        //
        // This row read $0.435/$0.87 on the claim that "this snapshot is the
        // one V4 Pro build DeepSeek serves alone, so there is exactly one
        // endpoint and exactly one price". That was wrong twice: the snapshot
        // has fifteen endpoints, and DeepSeek first-party is not among the ones
        // this deployment can route to. The row was underpriced threefold on
        // output, which is the direction that lets a real bill run past
        // `budget_usd_per_pr`. Measured 2026-08-23.
        "deepseek/deepseek-v4-pro-0813",
        Price {
            input: 1.32,
            output: 3.96,
            // Cache reads are a *thirtieth* of the input price here. On a
            // re-review that is most of the bill.
            cached: 0.044,
        },
    ),
    (
        // DeepSeek's own rates, which is what `[models.provider]` now pins.
        // The note this replaces was right that the floating alias spans
        // $0.42-$1.74 across eighteen providers and that one row could not
        // describe it; pinning the provider is what makes a single row true
        // again. Re-derive from `/api/v1/models/deepseek/<id>/endpoints` if the
        // pin changes.
        // The floating alias, priced at the dearer of the two pinned providers
        // (DeepInfra) for the same reason as the dated row above. Nothing
        // selects this id — it is here so a deployment that sets it is costed
        // rather than billed at `ceiling()`. Measured 2026-08-23.
        "deepseek/deepseek-v4-pro",
        Price {
            input: 1.30,
            output: 2.60,
            cached: 0.10,
        },
    ),
    (
        // Both tiers, and the tier a council runs on, at the dearer of the two
        // providers `models.provider` pins — DeepInfra at $0.09/$0.18 against
        // StreamLake's $0.0503/$0.1005. The dearer rate is deliberate: the pin
        // lists two providers so an outage costs price rather than the review,
        // and a table that recorded the cheaper one would let `budget_usd_per_pr`
        // under-count exactly when the fallback provider is answering.
        //
        // Not DeepSeek's own rates. DeepSeek first-party does not serve this
        // model at all — see `[models.provider]` in `config/defaults.toml`.
        //
        // Cache reads are a *fifth* of the input price here, which is the whole
        // reason several reviewers cost about what one used to: the shared
        // prompt prefix is paid for once.
        "deepseek/deepseek-v4-flash",
        Price {
            input: 0.09,
            output: 0.18,
            cached: 0.018,
        },
    ),
    (
        "moonshotai/kimi-k3",
        Price {
            input: 3.00,
            output: 15.00,
            cached: 0.30,
        },
    ),
    (
        "moonshotai/kimi-k2.7-code",
        Price {
            input: 0.70,
            output: 3.50,
            cached: 0.15,
        },
    ),
    (
        "moonshotai/kimi-k2.6",
        Price {
            input: 0.58,
            output: 2.44,
            cached: 0.15,
        },
    ),
    (
        "minimax/minimax-m3",
        Price {
            input: 0.30,
            output: 1.20,
            cached: 0.06,
        },
    ),
    (
        "minimax/minimax-m2.1",
        Price {
            input: 0.30,
            output: 1.20,
            cached: 0.03,
        },
    ),
];

/// Per-million-token prices for embedding models, keyed by
/// [`EmbedSignature`](crate::index::types::EmbedSignature)'s `provider/model`.
///
/// Embeddings are input-only: there is no completion and no cache read. The
/// offline mock is listed at zero because it really does cost nothing — an
/// explicit row, so it is priced rather than falling through to the ceiling.
const EMBED_PRICES: &[(&str, f64)] = &[
    ("mock/hash-bag", 0.00),
    ("voyage/voyage-code-3", 0.18),
    ("voyage/voyage-3-large", 0.18),
    ("openai/text-embedding-3-small", 0.02),
    ("openai/text-embedding-3-large", 0.13),
    // Through the OpenRouter gateway. Only a fallback: the gateway reports the
    // cost it actually charged and `OpenRouterEmbedder` prefers that, so these
    // are what a response missing its `usage` block falls back to. Prices read
    // from the gateway's own model listing.
    ("openrouter/openai/text-embedding-3-small", 0.02),
    ("openrouter/openai/text-embedding-3-large", 0.13),
    ("openrouter/voyageai/voyage-4", 0.06),
    ("openrouter/voyageai/voyage-4-lite", 0.02),
    ("openrouter/voyageai/voyage-4-large", 0.12),
    ("openrouter/mistralai/codestral-embed-2505", 0.15),
    ("openrouter/qwen/qwen3-embedding-8b", 0.01),
    ("openrouter/qwen/qwen3-embedding-4b", 0.02),
    ("openrouter/baai/bge-m3", 0.01),
    ("openrouter/google/gemini-embedding-001", 0.15),
];

/// The price of `model`, when it is known.
pub fn price_of(model: &str) -> Option<Price> {
    MODEL_PRICES
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, price)| *price)
}

/// Which of `models` this build has no price for.
///
/// Reported by `doctor` so a stale table is visible before a review runs,
/// rather than after it has been billed at the ceiling.
pub fn unpriced<'a>(models: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    models
        .into_iter()
        .filter(|model| price_of(model).is_none())
        .map(str::to_string)
        .collect()
}

/// The most expensive rate in the table, charged for an unpriced model.
///
/// Each component is maximised independently. The point is an upper bound on
/// spend, not a plausible price list.
pub fn ceiling() -> Price {
    MODEL_PRICES.iter().fold(
        Price {
            input: 0.0,
            output: 0.0,
            cached: 0.0,
        },
        |mut worst, (_, price)| {
            worst.input = worst.input.max(price.input);
            worst.output = worst.output.max(price.output);
            worst.cached = worst.cached.max(price.cached);
            worst
        },
    )
}

/// What one completion cost, in USD.
///
/// `input_tokens` includes `cached_tokens`, exactly as providers report it.
pub fn completion_cost(
    model: &str,
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
) -> f64 {
    let price = price_of(model).unwrap_or_else(|| {
        tracing::warn!(
            model,
            "no price known for this model; billing it at the most expensive rate in the table so \
             it still counts against the budget"
        );
        ceiling()
    });

    // Cached input tokens are billed at the cache-read rate, not the input
    // rate, and on kimi-k3 that is a tenfold difference — charging them at full
    // price would make every re-review look ten times more expensive than it is.
    let fresh_input = input_tokens.saturating_sub(cached_tokens);
    let million = 1_000_000.0;

    (fresh_input as f64 * price.input
        + cached_tokens as f64 * price.cached
        + output_tokens as f64 * price.output)
        / million
}

/// What embedding `tokens` cost, in USD.
///
/// `signature` is the embedder's `provider/model`. An unknown embedder is
/// charged the most expensive embedding rate for the same reason an unknown
/// completion model is: indexing a repository is the largest token count this
/// program produces, and it must not be the one that escapes the budget.
pub fn embedding_cost(signature: &str, tokens: u64) -> f64 {
    // A locally served model is not billed by anybody, and listing every GGUF
    // somebody might run behind Ollama is a table nobody can keep current. The
    // rule is the provider, not the model: `ollama` means the vectors were
    // computed on this machine, and charging the ceiling for that would make
    // the budget stop a run that costs nothing.
    if signature.starts_with("ollama/") {
        return 0.0;
    }

    let per_million = EMBED_PRICES
        .iter()
        .find(|(id, _)| *id == signature)
        .map(|(_, price)| *price)
        .unwrap_or_else(|| {
            let worst = EMBED_PRICES
                .iter()
                .fold(0.0_f64, |worst, (_, price)| worst.max(*price));
            tracing::warn!(
                signature,
                "no price known for this embedder; billing it at the most expensive embedding rate"
            );
            worst
        });

    tokens as f64 * per_million / 1_000_000.0
}

/// A rough token count for text about to be embedded.
///
/// Four bytes per token is the usual approximation for code and English prose,
/// and it is what the estimate has to be: embedding providers report usage only
/// in the response, and the budget has to be checked before the call as well as
/// after it. It rounds up, so the estimate never under-charges.
pub fn estimate_tokens(text: &str) -> u64 {
    text.len().div_ceil(4) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_tokens_are_billed_at_the_cache_rate() {
        // kimi-k3 reads cache at a tenth of the input price. Charging cached
        // tokens at full rate would make every re-review look ten times more
        // expensive than it is, and trip the budget ceiling for no reason.
        let cost = completion_cost("moonshotai/kimi-k3", 100_000, 90_000, 1_000);
        // 10k fresh at $3/M + 90k cached at $0.30/M + 1k out at $15/M
        assert!((cost - (0.03 + 0.027 + 0.015)).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn an_unpriced_model_does_not_silently_cost_zero() {
        let cost = completion_cost("someone/unreleased", 1_000_000, 0, 0);
        assert!(
            cost > 0.0,
            "an unpriced model must still count against the budget"
        );
        // The ceiling, not a guess: the most expensive input rate in the table.
        assert!((cost - ceiling().input).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn an_unpriced_model_is_never_cheaper_than_a_priced_one() {
        for (id, _) in MODEL_PRICES {
            let priced = completion_cost(id, 50_000, 10_000, 5_000);
            let unknown = completion_cost("someone/unreleased", 50_000, 10_000, 5_000);
            assert!(unknown >= priced, "{id}: {unknown} < {priced}");
        }
    }

    #[test]
    fn unpriced_names_only_the_models_missing_from_the_table() {
        let missing = unpriced(["moonshotai/kimi-k3", "someone/unreleased"]);
        assert_eq!(missing, vec!["someone/unreleased".to_string()]);
    }

    #[test]
    fn a_locally_served_model_is_free_whatever_it_is_called() {
        // The provider is the rule, not the model id: nobody bills for vectors
        // computed on this machine, and a table of every GGUF is unmaintainable.
        assert_eq!(embedding_cost("ollama/nomic-embed-text", 10_000_000), 0.0);
        assert_eq!(embedding_cost("ollama/anything-at-all", 10_000_000), 0.0);
    }

    #[test]
    fn the_offline_embedder_is_priced_at_zero_rather_than_the_ceiling() {
        assert_eq!(embedding_cost("mock/hash-bag", 1_000_000), 0.0);
    }

    #[test]
    fn an_unpriced_embedder_does_not_silently_cost_zero() {
        let cost = embedding_cost("someone/unreleased-embedder", 1_000_000);
        assert!(cost > 0.0, "indexing must count against the budget");
    }

    #[test]
    fn a_token_estimate_rounds_up_so_it_never_undercharges() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn every_model_the_defaults_select_has_a_price() {
        // The test that matters more than any individual row. `completion_cost`
        // fails closed on an unpriced model, so shipping a `defaults.toml` that
        // names one turns the budget ceiling into a hard error on the first
        // review — and the failure would land in production, not here.
        let config: crate::config::types::Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .expect("defaults parse")
            .try_into()
            .expect("defaults deserialize");

        let mut named = vec![config.models.scan.clone(), config.models.deep.clone()];
        named.extend(config.models.fallback.iter().cloned());

        let missing = unpriced(named.iter().map(String::as_str));
        assert!(
            missing.is_empty(),
            "unpriced models in defaults: {missing:?}"
        );
    }

    #[test]
    fn the_deep_tier_costs_what_the_table_says() {
        // Anchored on the same real figure every deep-tier change has been
        // measured against: PR #62's security lane sent 82,914 input and 842
        // output tokens. That call cost $0.2614 on kimi-k3 and $0.0061 on GLM
        // 5.2; on the model selected now it is pinned below rather than
        // asserted vaguely, because the price of the *selected* tier is what
        // the budget ceiling is actually spent through.
        //
        // Note the argument order: input, **cached**, output. Getting it wrong
        // is silent — every argument is a `u64` — and I did exactly that while
        // writing this test, which is why the ordering is now pinned below.
        //
        // Read from the shipped config rather than naming a model id. This test
        // hardcoded `deepseek-v4-pro-0813` and kept asserting its price after
        // the deep tier had moved to Flash, so the one number it exists to
        // guard described a model no review had run on.
        let config: crate::config::types::Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .expect("defaults parse")
            .try_into()
            .expect("defaults deserialize");

        let cost = completion_cost(&config.models.deep, 82_914, 0, 842);

        assert!(
            (0.0070..0.0085).contains(&cost),
            "expected roughly $0.0076 for the 83k-token security call on `{}`, got {cost:.5}",
            config.models.deep
        );
    }

    /// The cheapest rate any OpenRouter endpoint charges for each priced model,
    /// in USD per million tokens, as `(model, input, output)`.
    ///
    /// Measured 2026-08-23 against
    /// `https://openrouter.ai/api/v1/models/<id>/endpoints`, taking the minimum
    /// over every endpoint — not over the pinned ones. A floor that assumed the
    /// pin would be wrong the moment the pin is, which is the failure this
    /// whole file has already had once.
    ///
    /// Re-measure when adding a row. A model absent here fails the test below
    /// rather than passing unchecked: an unmeasured row is exactly the state
    /// every wrong row in this file has been in.
    const CHEAPEST_ENDPOINT: &[(&str, f64, f64)] = &[
        ("deepseek/deepseek-v4-flash", 0.0503, 0.1005),
        ("deepseek/deepseek-v4-pro", 0.3969, 0.7938),
        ("deepseek/deepseek-v4-pro-0813", 0.66, 1.98),
        ("minimax/minimax-m2.1", 0.30, 1.20),
        ("minimax/minimax-m3", 0.23, 0.96),
        ("moonshotai/kimi-k2.6", 0.5415, 2.28),
        ("moonshotai/kimi-k2.7-code", 0.67, 3.40),
        ("moonshotai/kimi-k3", 2.60, 13.00),
        ("qwen/qwen3.8-max", 2.00, 6.00),
        ("z-ai/glm-5.2", 0.336, 1.056),
    ];

    #[test]
    fn no_model_is_priced_below_what_any_endpoint_actually_charges() {
        // The GLM 5.2 row sat at $0.07 / $0.22 for months. Nothing caught it,
        // because every test here compares this table against itself: a row can
        // be wrong by any factor and still be internally consistent.
        //
        // This was a single `$0.10` floor for every model, justified by "the
        // cheapest endpoint serving any model this table lists is $0.392 per
        // million input tokens". Both halves aged badly. Flash is genuinely
        // cheaper than that floor, so the constant started rejecting a correct
        // row; and a blanket floor never checked the rows it passed, so
        // `deepseek-v4-pro-0813` sat underpriced threefold on output — the
        // direction that makes `budget_usd_per_pr` fail open — while the test
        // stayed green. A per-model floor catches both.
        let mut wrong = Vec::new();

        for (model, price) in MODEL_PRICES {
            let Some((_, floor_in, floor_out)) =
                CHEAPEST_ENDPOINT.iter().find(|(id, _, _)| id == model)
            else {
                wrong.push(format!(
                    "{model}: no measured floor; add one to CHEAPEST_ENDPOINT"
                ));
                continue;
            };

            // Strictly below, so a row priced exactly at the cheapest endpoint
            // is correct rather than suspect — several are.
            if price.input < *floor_in {
                wrong.push(format!(
                    "{model}: input ${:.4} is below the cheapest endpoint's ${floor_in:.4}",
                    price.input
                ));
            }
            if price.output < *floor_out {
                wrong.push(format!(
                    "{model}: output ${:.4} is below the cheapest endpoint's ${floor_out:.4}",
                    price.output
                ));
            }
        }

        assert!(
            wrong.is_empty(),
            "priced below a real endpoint, so the budget ceiling fails open:\n  {}",
            wrong.join("\n  ")
        );
    }

    #[test]
    fn a_cached_read_is_charged_at_the_cache_rate() {
        // GLM 5.2 reads cache at a fifth of its input price, and a re-review is
        // mostly cache. Charging cached tokens at full price would misreport
        // exactly the number this change exists to move.
        let cold = completion_cost("z-ai/glm-5.2", 100_000, 0, 1_000);
        let warm = completion_cost("z-ai/glm-5.2", 100_000, 90_000, 1_000);

        assert!(
            warm < cold,
            "cached tokens must cost less: {warm} vs {cold}"
        );
    }

    #[test]
    fn the_middle_argument_is_cached_tokens_not_output() {
        // Every parameter is a `u64`, so swapping two of them compiles and
        // silently misreports the bill. Output is priced far above cache reads,
        // so the two orderings are distinguishable — which is what this asserts.
        let correct = completion_cost("z-ai/glm-5.2", 1_000_000, 0, 1_000_000);
        let swapped = completion_cost("z-ai/glm-5.2", 1_000_000, 1_000_000, 0);

        assert!(
            correct > swapped,
            "input+output must cost more than input-all-cached: {correct} vs {swapped}"
        );
    }
}
