//! Who reviews, and the guarantee that one reviewer is the old behaviour.

use super::*;
use crate::config::types::{Config, CouncilAgent, ModelRef};

fn config() -> Config {
    crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap()
}

fn agent(id: &str, persona: Option<&str>, lanes: Vec<LaneId>) -> CouncilAgent {
    CouncilAgent {
        id: id.into(),
        lanes,
        model: None,
        persona: persona.map(str::to_string),
    }
}

#[test]
fn a_disabled_council_yields_the_lanes_own_reviewer() {
    let config = config();
    let reviewers = reviewers(&config, LaneId::Critique);

    assert_eq!(reviewers.len(), 1);
    // The lane's own model and no persona: the prompt is byte-identical to the
    // one built before the council existed.
    assert_eq!(reviewers[0].model, config.model_for(LaneId::Critique));
    assert_eq!(reviewers[0].persona, persona::NONE);
    assert!(persona::NONE.is_empty());
}

#[test]
fn an_enabled_council_with_no_agent_for_this_lane_still_reviews_it() {
    // Every caller has one code path. A council branch and a legacy branch
    // would drift apart, and the lane that fell through the gap would go
    // unreviewed rather than loudly failing.
    let mut config = config();
    config.council.enabled = true;
    config.council.agents = vec![agent("sec", None, vec![LaneId::Security])];

    let reviewers = reviewers(&config, LaneId::Critique);
    assert_eq!(reviewers.len(), 1);
    assert_eq!(reviewers[0].id, "reviewer");
}

#[test]
fn agents_run_in_configuration_order() {
    let mut config = config();
    config.council.enabled = true;
    config.council.agents = vec![
        agent("first", Some("correctness"), vec![]),
        agent("second", Some("integration"), vec![]),
    ];

    let reviewers = reviewers(&config, LaneId::Critique);
    let ids: Vec<&str> = reviewers.iter().map(|r| r.id).collect();
    // Order decides which reviewer's prose becomes the summary, so it has to be
    // the operator's rather than a hash map's.
    assert_eq!(ids, ["first", "second"]);
    assert_ne!(reviewers[0].persona, reviewers[1].persona);
}

#[test]
fn an_agent_with_no_model_inherits_the_lanes() {
    // This is what makes a one-agent council identical to no council.
    let mut config = config();
    config.council.enabled = true;
    config.council.agents = vec![agent("solo", None, vec![])];

    let reviewers = reviewers(&config, LaneId::Critique);
    assert_eq!(reviewers[0].model, config.model_for(LaneId::Critique));
}

#[test]
fn an_agent_may_name_a_tier_or_an_explicit_model() {
    let mut config = config();
    config.council.enabled = true;
    config.council.agents = vec![
        CouncilAgent {
            model: Some(ModelRef("deep".into())),
            ..agent("tiered", None, vec![])
        },
        CouncilAgent {
            model: Some(ModelRef("qwen/qwen3.7-plus".into())),
            ..agent("explicit", None, vec![])
        },
    ];

    let reviewers = reviewers(&config, LaneId::Critique);
    // The same three-way rule `Config::model_for` uses, so there is one
    // resolution rule in the codebase rather than three shapes of it.
    assert_eq!(reviewers[0].model, config.models.deep);
    assert_eq!(reviewers[1].model, "qwen/qwen3.7-plus");
}

#[test]
fn a_lane_scoped_agent_sits_out_the_lanes_it_did_not_name() {
    let mut config = config();
    config.council.enabled = true;
    config.council.agents = vec![
        agent("everywhere", Some("correctness"), vec![]),
        agent("security-only", Some("adversary"), vec![LaneId::Security]),
    ];

    let critique: Vec<&str> = reviewers(&config, LaneId::Critique)
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(critique, ["everywhere"]);

    let security: Vec<&str> = reviewers(&config, LaneId::Security)
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(security, ["everywhere", "security-only"]);
}
