//! What the thread policy decided, and the plan it produced.

use serde::{Deserialize, Serialize};

/// What to do with one review thread.
///
/// `Leave` carries its reason as a `&'static str` on purpose: every reason is
/// written here, in this crate, so a reason can never be attacker-controlled
/// text that ends up in a log or a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Resolve it, on deterministic evidence alone.
    Resolve(&'static str),
    /// Leave it for a human, with the reason why.
    Leave(&'static str),
    /// Nothing deterministic settles it. Only reachable when `threads.ask_model`
    /// is on; otherwise the thread is left alone.
    Ask,
}

/// One thread the policy decided to resolve.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedResolve {
    /// The GraphQL node id of the thread.
    pub id: String,
    /// Why, for the log and the check-run summary.
    pub reason: String,
}

/// The threads one run decided to resolve.
///
/// Serializable because it travels in the proposal: the decision is taken on
/// the read side, where the model runs, and executed on the write side, where
/// no model has ever been. That is the same split the lanes have, and it is
/// what keeps "a model verdict is advisory" true for this feature.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadPlan {
    /// The threads to resolve, in the order the forge reported them.
    pub resolve: Vec<PlannedResolve>,
}

impl ThreadPlan {
    /// Whether there is anything to do.
    pub fn is_empty(&self) -> bool {
        self.resolve.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_keep_their_deterministic_reasons() {
        assert!(matches!(
            Decision::Resolve("fixed"),
            Decision::Resolve("fixed")
        ));
        assert!(matches!(
            Decision::Leave("human reply"),
            Decision::Leave("human reply")
        ));
        assert!(matches!(Decision::Ask, Decision::Ask));
    }

    #[test]
    fn plans_round_trip_and_report_emptiness() {
        let empty = ThreadPlan::default();
        assert!(empty.is_empty());

        let plan = ThreadPlan {
            resolve: vec![PlannedResolve {
                id: "thread-1".into(),
                reason: "the code changed".into(),
            }],
        };
        assert!(!plan.is_empty());
        assert_eq!(
            serde_json::from_str::<ThreadPlan>(&serde_json::to_string(&plan).unwrap()).unwrap(),
            plan
        );
    }
}
