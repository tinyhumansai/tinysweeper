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

use crate::config::types::Severity;

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
        "resilience" => RESILIENCE,
        "data" => DATA,
        "style" => STYLE,
        _ => return None,
    })
}

/// The severity a persona's findings may not exceed.
///
/// `None` for every persona that reports defects, which is most of them: a
/// reviewer that found a critical bug must be able to say so, and capping that
/// would be capping the product.
///
/// `style` is the exception and the reason this exists. Style is the noise this
/// repository is built to suppress — `max_comments`, dedupe and the anchoring
/// rules all exist to keep nits off a pull request — so a style reviewer that
/// could rank itself alongside a correctness finding would undo that in one
/// configuration line. Capped at [`Severity::Low`] it lands *below* the default
/// severity gate, which means its findings reach the check-run summary and
/// never become inline comments. That is the whole intent: visible to somebody
/// who goes looking, invisible to somebody reading their pull request.
///
/// The cap is applied to what the model returned rather than requested in the
/// prompt, because a prompt is a request and a clamp is a guarantee.
pub fn ceiling(name: &str) -> Option<Severity> {
    match name {
        "style" => Some(Severity::Low),
        _ => None,
    }
}

/// Every persona name, for error messages and `doctor`.
pub const NAMES: [&str; 6] = [
    "correctness",
    "integration",
    "adversary",
    "resilience",
    "data",
    "style",
];

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

/// Resilience: what this change does when something it depends on fails.
const RESILIENCE: &str = r#"

## Your angle

Read this change for what happens when the thing it called does not work. Your
subject is the failure path: an error swallowed or logged and continued past, a
retry with no ceiling, a lock or handle or connection not released when the
early return fires, a partial write that leaves the system in a state nothing
can read, a timeout absent where the call can hang.

The happy path is somebody else's, and so is the malicious input. Yours is the
ordinary failure — the disk full, the peer gone, the second caller arriving
while the first is still running.

You are one of several reviewers with different angles. Report what *this*
angle sees and leave the rest."#;

/// Data: what this change does to information that outlives the process.
const DATA: &str = r#"

## Your angle

Read this change for its effect on data that already exists. Your subject is
everything that has to survive a deploy: a schema or serialized shape that has
changed while old records were written with the previous one, a migration that
cannot be run twice or cannot be rolled back, a default that silently rewrites
rows it was only meant to read, an identifier or encoding assumption that holds
for new data and not for old.

A finding needs to name the data that already exists and what becomes of it. A
change that only affects records written after it ships is not yours.

You are one of several reviewers with different angles. Report what *this*
angle sees and leave the rest."#;

/// Style: conventions and readability, deliberately capped.
///
/// The instruction to stay quiet is doing real work. Style is the one subject
/// where a model will always find *something*, so a persona that merely asked
/// for style opinions would return the maximum number of findings on every
/// pull request forever. See [`ceiling`] for the enforcement that does not
/// depend on the model cooperating with any of this.
const STYLE: &str = r#"

## Your angle

Read this change for how it reads against the code around it. Your subject is
consistency with *this* repository: a name that says something different from
what the thing does, a module that has picked up a second responsibility, an
abstraction repeated a fourth time, a comment that describes code that no
longer exists.

Two rules, and they matter more than the subject:

Compare against the surrounding code, never against a general standard. "This
repository does it the other way, here" is a finding. "The convention is X" is
not, and neither is anything you would say about a file you had not read.

Say nothing rather than something. A pull request with no style finding is the
normal outcome and the one you should expect to report. Formatting, import
order, line length and anything a linter or formatter already decides are never
yours — tell the reader nothing at all rather than telling them that.

You are one of several reviewers with different angles, and every one of them
outranks you. Correctness, security and test coverage all belong to somebody
else. Your findings are recorded and deliberately not commented on the pull
request, so write them for somebody reading a summary later, not the author."#;

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
    fn only_style_is_capped() {
        // A cap on a persona that reports defects would silently downgrade real
        // findings below the gate, which is indistinguishable from the reviewer
        // never having run.
        assert_eq!(ceiling("style"), Some(Severity::Low));

        for name in NAMES.iter().filter(|n| **n != "style") {
            assert_eq!(ceiling(name), None, "`{name}` is capped");
        }
    }

    #[test]
    fn an_unknown_persona_is_not_capped() {
        // `ceiling` is consulted for every reviewer, including the persona-less
        // default. Capping something it does not recognise would make a typo
        // quietly mute a reviewer rather than fail at load.
        assert_eq!(ceiling(""), None);
        assert_eq!(ceiling("styles"), None);
    }

    #[test]
    fn style_is_told_to_expect_to_find_nothing() {
        // The one persona guaranteed to find something on any input. Without
        // this it returns `max_comments` findings on every pull request, and
        // the cap alone would not stop it crowding out the reviewers that
        // matter during merge.
        let text = lookup("style").expect("resolves");
        assert!(text.contains("Say nothing rather than something"));
    }

    #[test]
    fn every_persona_names_a_subject_rather_than_asking_for_effort() {
        // The module doc's claim: a persona must change *what* is looked at.
        // "Be thorough" produces the same answer at a second bill.
        for name in NAMES {
            let text = lookup(name).expect("resolves").to_lowercase();
            for empty in ["be thorough", "step by step", "carefully consider"] {
                assert!(!text.contains(empty), "`{name}` contains `{empty}`");
            }
            assert!(
                text.contains("your subject is"),
                "`{name}` does not name a subject"
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
