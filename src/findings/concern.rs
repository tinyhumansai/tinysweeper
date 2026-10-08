//! Whether two findings raise the same concern, however they are worded.

use std::collections::BTreeSet;

use crate::findings::types::Finding;

/// One finding, reduced to what concern identity compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Concern;

impl Concern {
    /// The concern a finding raises.
    pub fn of(_finding: &Finding) -> Self {
        Self
    }

    /// Whether `self` repeats `other`.
    pub fn same_as(&self, _other: &Self) -> bool {
        false
    }

    /// Whether `self` rewords a concern a maintainer already declined.
    pub fn same_as_declined(&self, _declined: &Self) -> bool {
        false
    }
}

/// Whether two rule names are synonyms.
pub fn rules_agree(_left: &str, _right: &str) -> bool {
    false
}

/// The normalised words of `text`.
pub fn tokens(_text: &str) -> BTreeSet<String> {
    BTreeSet::new()
}

/// Whether two paths are a file and its own tests.
pub fn siblings(_left: &str, _right: &str) -> bool {
    false
}

#[cfg(test)]
#[path = "concern_test.rs"]
mod tests;
