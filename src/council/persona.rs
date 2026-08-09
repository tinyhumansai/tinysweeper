//! What makes one reviewer look at a diff differently from another.
//!
//! Always compiled. Every persona is a `&'static str` in this file, selected by
//! name, and that is a security boundary rather than a style choice.
//!
//! # Personas are never free text
//!
//! `src/config/remote.rs` excludes `path_instructions` from what a reviewed
//! repository may override, with the reason stated there: it is "free text
//! injected straight into a lane's instructions, unfenced", and repository
//! prose reaches a prompt through exactly one door — the sandboxed extraction
//! in `crate::knowledge`. A persona is the same shape of text in the same
//! position, so a configurable one would be a second door, and `[council]` is
//! excluded from the overridable set for exactly this reason.
//!
//! Naming an unknown persona is a configuration error reported by
//! `tinysweeper check`, not a silently weaker reviewer.
//!
//! # Why a persona is not a second opinion
//!
//! Asking one model the same question twice at the same temperature produces
//! the same answer twice, and paying for both is not a council. A persona has
//! to change *what the reviewer looks at* — the failure classes it reaches for
//! first — rather than merely how it phrases the answer. That is why each one
//! below names concrete things to go and check, and why none of them says
//! "be thorough" or "think step by step".

/// The persona applied when a council agent names none.
///
/// Empty rather than a default flavour: an agent with no persona is the lane's
/// own instructions, unmodified, which is what makes a one-agent council a
/// provable no-op.
pub const NONE: &str = "";

/// Resolve a persona name to the text appended to the lane instructions.
///
/// `None` for an unknown name, so `config::validate` can report it once at load
/// rather than every review silently running a reviewer with no character.
pub fn lookup(name: &str) -> Option<&'static str> {
    Some(match name {
        "correctness" => CORRECTNESS,
        "integration" => INTEGRATION,
        "adversary" => ADVERSARY,
        _ => return None,
    })
}

/// Every persona name, for error messages and `doctor`.
pub const NAMES: [&str; 3] = ["correctness", "integration", "adversary"];

/// Local correctness: what this code does on its own, read closely.
const CORRECTNESS: &str = r#"

## Your angle

Read this change as though you were stepping through it. Your subject is what
the code in front of you does on inputs the author did not picture: the empty
collection, the zero, the value that arrives twice, the error branch nobody
took. Boundaries, ordering, arithmetic, and the path taken when something
returns nothing.

You are one of several reviewers with different angles. Report what *this*
angle sees and leave the rest; another reviewer is reading for the things you
are being told to skip."#;

/// Integration: what this change does to code that already exists.
const INTEGRATION: &str = r#"

## Your angle

Read this change for its effect on everything it touches but does not show.
Your subject is the contract: a function whose meaning moved while its
signature did not, a caller that now receives something it never handled, an
invariant asserted somewhere else that this quietly breaks, a value that used
to be impossible and now is not.

Where the retrieved context shows you a caller or a definition, use it — that is
what it is for. Where it does not, say what you would need to see rather than
guessing, and lower your confidence accordingly.

You are one of several reviewers with different angles. Another is reading the
same code line by line for local correctness, so leave that to them."#;

/// Adversary: what a hostile input does with this change.
const ADVERSARY: &str = r#"

## Your angle

Read this change as somebody trying to make it misbehave. Your subject is the
input the author did not consider hostile: the field they assumed was short,
the path they assumed was inside the directory, the identifier they assumed was
theirs, the loop they assumed terminates.

A finding needs a source, a sink and a path between them that you can point at.
"This looks dangerous" is not one. If you cannot name all three from what you
were shown, you do not have a finding.

You are one of several reviewers with different angles. Style, naming and test
coverage belong to somebody else."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_persona_resolves() {
        for name in NAMES {
            assert!(
                lookup(name).is_some(),
                "`{name}` is listed but not resolved"
            );
        }
    }

    #[test]
    fn an_unknown_persona_is_none_rather_than_a_default() {
        // A typo must be a configuration error, not a reviewer that silently
        // has no character.
        assert!(lookup("corectness").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn every_persona_tells_the_reviewer_it_is_not_alone() {
        // Without this clause each agent reports the same cross-cutting
        // observation and the author gets it N times — the failure
        // `harness::prompt::ISOLATION_CLAUSE` was written against.
        for name in NAMES {
            let text = lookup(name).expect("resolves");
            assert!(
                text.contains("one of several reviewers"),
                "`{name}` does not tell the reviewer others are running"
            );
        }
    }

    #[test]
    fn no_persona_overrides_the_lanes_own_subject() {
        // A persona narrows *within* a lane. One that told the reviewer to
        // ignore the lane instructions would make the check run mean something
        // different from its name.
        for name in NAMES {
            let text = lookup(name).expect("resolves").to_lowercase();
            for forbidden in ["ignore the", "instead of the instructions", "disregard"] {
                assert!(!text.contains(forbidden), "`{name}` contains `{forbidden}`");
            }
        }
    }
}
