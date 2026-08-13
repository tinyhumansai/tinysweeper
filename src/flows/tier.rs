//! Which model a node asks for, and how that name becomes a model id.
//!
//! A graph node names a *tier*, never a model id. Two reasons, and both are
//! about the graph being data:
//!
//! - A tier is stable across deployments. A graph that hard-codes
//!   `deepseek/deepseek-v4-flash` stops being portable the moment a repository
//!   overrides `models.flash`, and stops being *correct* the moment the id is
//!   one `harness::pricing` has no row for.
//! - The resolution happens in one place, so there is exactly one answer to
//!   "what did this call actually run on" — which is what the spend line
//!   reports.

use crate::config::types::Models;
use crate::error::{Error, Result};

/// The wire name a node's `config.tier` carries.
///
/// Deliberately not `Deserialize`: the mapping from string to variant is the
/// error-reporting surface, and a derived one would say `unknown variant` where
/// [`Tier::parse`] names the key that has to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Mechanical, high-volume work: one call, cheap model.
    Scan,
    /// Close reading: one call, strong model.
    Deep,
    /// One opinion of several. Cheap enough that a panel of them costs less
    /// than the single [`Tier::Deep`] call it replaces.
    Flash,
}

impl Tier {
    /// The wire name, as it appears in a node's config.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Scan => "scan",
            Tier::Deep => "deep",
            Tier::Flash => "flash",
        }
    }

    /// Parse a node's `tier` value.
    ///
    /// # Errors
    /// Returns [`Error::Model`] naming the tiers that exist. A graph is data and
    /// can name a tier this build does not have; that has to fail loudly rather
    /// than silently falling back to a tier the author did not choose — the
    /// cheap fallback would under-review and the expensive one would overspend,
    /// and neither would appear anywhere a human looks.
    pub fn parse(name: &str) -> Result<Self> {
        match name.trim() {
            "scan" => Ok(Tier::Scan),
            "deep" => Ok(Tier::Deep),
            "flash" => Ok(Tier::Flash),
            other => Err(Error::Model(format!(
                "unknown model tier `{other}`; expected one of `scan`, `deep`, `flash`"
            ))),
        }
    }

    /// The configured model id for this tier.
    pub fn model_id(self, models: &Models) -> &str {
        match self {
            Tier::Scan => &models.scan,
            Tier::Deep => &models.deep,
            Tier::Flash => &models.flash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Models {
        Models {
            scan: "vendor/scan".into(),
            deep: "vendor/deep".into(),
            flash: "vendor/flash".into(),
            ..Models::default()
        }
    }

    #[test]
    fn every_tier_resolves_to_its_own_configured_id() {
        let models = models();
        assert_eq!(Tier::Scan.model_id(&models), "vendor/scan");
        assert_eq!(Tier::Deep.model_id(&models), "vendor/deep");
        assert_eq!(Tier::Flash.model_id(&models), "vendor/flash");
    }

    #[test]
    fn a_tier_round_trips_through_its_wire_name() {
        for tier in [Tier::Scan, Tier::Deep, Tier::Flash] {
            assert_eq!(Tier::parse(tier.as_str()).unwrap(), tier);
        }
    }

    #[test]
    fn surrounding_whitespace_is_not_a_different_tier() {
        assert_eq!(Tier::parse("  flash ").unwrap(), Tier::Flash);
    }

    #[test]
    fn an_unknown_tier_is_refused_rather_than_defaulted() {
        // Defaulting is the tempting behaviour and the wrong one: falling back
        // to `scan` under-reviews and falling back to `deep` overspends, and a
        // graph naming a tier that does not exist is a bug either way.
        let err = Tier::parse("turbo").unwrap_err().to_string();
        assert!(err.contains("turbo"), "{err}");
        assert!(err.contains("flash"), "{err}");
    }
}
