//! Core types for snippet resolution.
//!
//! Every line number here is a **head-revision** line number, the same
//! invariant `src/evidence/diff.rs` holds, because that is what GitHub's
//! review-comment API anchors to. A snippet that matched the base revision is
//! still reported as a head-revision line — see [`Side`].

/// Which revision the snippet was found in.
///
/// A match on [`Side::Old`] is still reported as a head-revision line: the
/// resolver maps a deleted line to the nearest surviving line of the same
/// hunk. Reporting the base-revision number would put the comment on whatever
/// unrelated code now occupies that number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Matched context or added lines.
    New,
    /// Matched context or deleted lines.
    Old,
}

/// Which stage found the match.
///
/// Recorded rather than discarded because the mix is the health metric for
/// this whole mechanism: a corpus that suddenly resolves mostly through
/// [`Stage::WholeFile`] means the lane prompt stopped quoting the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Found by sliding the snippet over a hunk.
    Hunk,
    /// Found by sliding the snippet over the whole head-revision file.
    WholeFile,
}

/// Where a snippet lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// First head-revision line the snippet covers.
    pub start: u64,
    /// Last head-revision line the snippet covers.
    pub end: u64,
    /// Which revision matched.
    pub side: Side,
    /// Which stage matched.
    pub stage: Stage,
    /// Whether the snippet had to be recovered by a re-location model call.
    pub relocated: bool,
}

/// Why a snippet could not be placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unanchored {
    /// The model reported no snippet at all.
    NoSnippet,
    /// Nothing in the diff or the file matched it.
    NoMatch,
}

impl Unanchored {
    /// A short reason, for the check-run summary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSnippet => "the model quoted no code",
            Self::NoMatch => "the quoted code matched nothing in the file",
        }
    }
}

/// The outcome of resolving one snippet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Placed at a head-revision range.
    Anchored(Anchor),
    /// Not placed. The finding survives without a line and is rendered into
    /// the summary rather than posted inline — dropping it would throw away a
    /// real finding over an arithmetic failure, which is the bug this module
    /// exists to fix.
    Unanchored(Unanchored),
}

impl Resolution {
    /// The range, when there is one.
    pub fn range(&self) -> Option<(u64, u64)> {
        match self {
            Self::Anchored(anchor) => Some((anchor.start, anchor.end)),
            Self::Unanchored(_) => None,
        }
    }

    /// Whether the snippet was placed.
    pub fn is_anchored(&self) -> bool {
        matches!(self, Self::Anchored(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unanchored_resolution_has_no_range() {
        let resolution = Resolution::Unanchored(Unanchored::NoMatch);
        assert_eq!(resolution.range(), None);
        assert!(!resolution.is_anchored());
        assert!(!Unanchored::NoMatch.as_str().is_empty());
        assert!(!Unanchored::NoSnippet.as_str().is_empty());
    }

    #[test]
    fn an_anchored_resolution_reports_its_range() {
        let resolution = Resolution::Anchored(Anchor {
            start: 10,
            end: 12,
            side: Side::New,
            stage: Stage::Hunk,
            relocated: false,
        });
        assert_eq!(resolution.range(), Some((10, 12)));
        assert!(resolution.is_anchored());
    }
}
