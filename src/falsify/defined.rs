//! The deterministic half of falsification: a finding that says a symbol is
//! undefined, or that the code will not compile, when the evidence defines it.
//!
//! Field data put "will fail to compile" on pull requests whose CI was green,
//! and "module not defined" next to the module. A model that was not shown a
//! definition reports it missing; this pass reads the same evidence the model
//! filter does and drops the claim when a definition is right there.
//!
//! It is deliberately narrow, so it can be wrong only in the safe direction:
//!
//! - the finding must make a compile or undefined-symbol claim, by phrase;
//! - it must name at least one symbol in backticks — the title's, when the
//!   title names any, otherwise the body's;
//! - **every** named symbol must be defined in the evidence: a definition
//!   keyword immediately before it on a line that was not removed, or a
//!   changed path with a component of that name (a module that exists).
//!
//! Anything else is kept. A missed rejection costs one noisy comment; a wrong
//! one silences a real defect.

use crate::config::types::LaneId;
use crate::falsify::types::Rejection;
use crate::findings::types::Finding;

/// Drop the findings whose undefined-symbol claim the evidence disproves.
///
/// `evidence` is every text the model filter is also shown — the rendered
/// diff and what the reviewer looked up. Returns the survivors and the
/// rejections, in order.
pub fn reject_disproved_symbol_claims(
    lane: LaneId,
    findings: Vec<Finding>,
    evidence: &[&str],
) -> (Vec<Finding>, Vec<Rejection>) {
    let mut kept = Vec::with_capacity(findings.len());
    let mut rejected = Vec::new();
    for finding in findings {
        match disproved_symbols(&finding, evidence) {
            Some(symbols) => rejected.push(Rejection {
                lane,
                title: finding.title.clone(),
                reason: format!(
                    "deterministic: the evidence defines {}, so the claim that it is undefined \
                     or will not compile is disproved",
                    symbols
                        .iter()
                        .map(|symbol| format!("`{symbol}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
            None => kept.push(finding),
        }
    }
    (kept, rejected)
}

/// Phrases that make a finding a compile or undefined-symbol claim.
///
/// Matched lowercased against the title and body. A phrase list rather than
/// anything cleverer because a miss here is safe: the finding simply goes on to
/// the model filter.
const CLAIMS: &[&str] = &[
    "will not compile",
    "won't compile",
    "fail to compile",
    "fails to compile",
    "does not compile",
    "doesn't compile",
    "compile error",
    "compilation error",
    "compile-time error",
    "is undefined",
    "not defined",
    "undefined symbol",
    "undefined variable",
    "undefined function",
    "undefined reference",
    "undeclared",
    "cannot find",
    "unresolved",
    "nameerror",
    "referenceerror",
    "does not exist",
    "doesn't exist",
];

/// Keywords that introduce a definition of the identifier that follows them,
/// across the languages the graph parses.
const DEFINES: &[&str] = &[
    "fn",
    "struct",
    "enum",
    "trait",
    "type",
    "union",
    "mod",
    "const",
    "static",
    "let",
    "macro_rules",
    "def",
    "class",
    "function",
    "interface",
    "var",
    "func",
    "record",
];

/// Words that may sit between a definition keyword and the name it defines.
const MODIFIERS: &[&str] = &[
    "pub",
    "crate",
    "super",
    "self",
    "in",
    "mut",
    "ref",
    "async",
    "unsafe",
    "extern",
    "export",
    "default",
    "abstract",
    "final",
    "public",
    "private",
    "protected",
    "declare",
    "readonly",
];

/// The symbols a finding claims are missing, when the evidence defines all of
/// them. `None` keeps the finding.
fn disproved_symbols(finding: &Finding, evidence: &[&str]) -> Option<Vec<String>> {
    let text = format!("{}\n{}", finding.title, finding.body).to_lowercase();
    if !CLAIMS.iter().any(|claim| text.contains(claim)) {
        return None;
    }
    let mut symbols = named_symbols(&finding.title);
    if symbols.is_empty() {
        symbols = named_symbols(&finding.body);
    }
    let all_defined = !symbols.is_empty()
        && symbols
            .iter()
            .all(|symbol| evidence.iter().any(|text| defines(text, symbol)));
    all_defined.then_some(symbols)
}

/// Identifiers quoted in backticks: `a::b::name()` and `obj.name` count as
/// `name`; a span with whitespace in it is code, not a name, and is skipped.
fn named_symbols(text: &str) -> Vec<String> {
    let mut symbols: Vec<String> = text
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| !span.chars().any(char::is_whitespace))
        .filter_map(|span| {
            let last = span.rsplit(['.', ':', '/']).next()?;
            let name = last.trim_end_matches("()").trim_end_matches('!');
            is_identifier(name).then(|| name.to_string())
        })
        .collect();
    symbols.sort();
    symbols.dedup();
    symbols
}

fn is_identifier(word: &str) -> bool {
    let mut chars = word.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `text` defines `symbol` on a line that survives the change, or
/// names a changed path with a component called `symbol`.
fn defines(text: &str, symbol: &str) -> bool {
    text.lines().any(|line| {
        if let Some(path) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            return path_names(path, symbol);
        }
        // The rendered diff prefixes a line with its head-revision number and
        // a `+`, `-` or space marker. A removed line is what no longer exists.
        let code = line.trim_start_matches(|c: char| c.is_ascii_digit() || c == ' ');
        if code.starts_with('-') || code.starts_with("@@") {
            return false;
        }
        let words: Vec<&str> = code
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| !word.is_empty())
            .collect();
        words.iter().enumerate().any(|(index, word)| {
            DEFINES.contains(word)
                && words[index + 1..]
                    .iter()
                    .find(|next| !MODIFIERS.contains(next))
                    .is_some_and(|name| *name == symbol)
        })
    })
}

/// Whether a diff header path has a directory or file-stem component named
/// `symbol` — the module of that name exists.
fn path_names(path: &str, symbol: &str) -> bool {
    let path = path.trim().trim_start_matches("a/").trim_start_matches("b/");
    path.split('/').any(|component| {
        component.split('.').next().is_some_and(|stem| stem == symbol)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Severity;

    const DIFF: &str = "--- src/hub.rs\n\
        @@ -1,2 +1,6 @@\n\
        \x20   1  use std::fmt;\n\
        \x20   2 +pub(crate) fn render_hub(body: &str) -> String {\n\
        \x20   3 +    body.to_string()\n\
        \x20   4 +}\n\
        \x20     -fn legacy_hub() {}\n";

    fn finding(title: &str, body: &str) -> Finding {
        Finding {
            lane: LaneId::Critique,
            severity: Severity::High,
            confidence: 0.9,
            path: "src/hub.rs".into(),
            line: Some(2),
            end_line: None,
            rule: "undefined-symbol".into(),
            title: title.into(),
            body: body.into(),
            suggestion: None,
            applicable: None,
            late: false,
            identity: None,
            aliases: vec![],
            grouped: false,
            review_pass: 1,
            corroboration: 1,
        }
    }

    fn run(findings: Vec<Finding>, evidence: &[&str]) -> (Vec<Finding>, Vec<Rejection>) {
        reject_disproved_symbol_claims(LaneId::Critique, findings, evidence)
    }

    #[test]
    fn an_undefined_claim_about_a_symbol_the_diff_defines_is_rejected() {
        let (kept, rejected) = run(
            vec![finding(
                "Define `render_hub` before calling it",
                "`render_hub` is not defined anywhere, so this will fail to compile.",
            )],
            &[DIFF],
        );
        assert!(kept.is_empty());
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].lane, LaneId::Critique);
        assert!(rejected[0].reason.contains("`render_hub`"), "{rejected:?}");
    }

    #[test]
    fn a_symbol_the_evidence_does_not_define_keeps_the_finding() {
        let (kept, rejected) = run(
            vec![finding(
                "Define `render_summary` before calling it",
                "`render_summary` is undefined.",
            )],
            &[DIFF],
        );
        assert_eq!(kept.len(), 1);
        assert!(rejected.is_empty());
    }

    #[test]
    fn a_finding_that_makes_no_compile_claim_is_never_touched() {
        let (kept, _) = run(
            vec![finding(
                "Escape the body in `render_hub`",
                "`render_hub` passes contributor text through unescaped.",
            )],
            &[DIFF],
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn a_definition_on_a_removed_line_is_not_evidence_it_exists() {
        let (kept, _) = run(
            vec![finding(
                "Restore `legacy_hub`",
                "`legacy_hub` is still called but no longer defined; this won't compile.",
            )],
            &[DIFF],
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn every_named_symbol_must_be_defined() {
        // One defined and one not: the claim may be about the other one.
        let (kept, _) = run(
            vec![finding(
                "`render_hub` calls `escape_body`, which is not defined",
                "This will not compile.",
            )],
            &[DIFF],
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn a_module_named_by_a_changed_path_exists() {
        let diff = "--- src/helpers/format.py\n@@ -0,0 +1 @@\n    1 +X = 1\n";
        let (kept, rejected) = run(
            vec![finding(
                "Import of `helpers` fails",
                "Module `helpers` is not defined, so the import raises.",
            )],
            &[diff],
        );
        assert!(kept.is_empty(), "{kept:?}");
        assert_eq!(rejected.len(), 1);
    }

    #[test]
    fn what_the_reviewer_looked_up_counts_as_evidence() {
        let looked_up = "src/util.rs:\nclass Widget:\n    pass\n";
        let (kept, _) = run(
            vec![finding("`Widget` is undefined", "NameError at import time.")],
            &[DIFF, looked_up],
        );
        assert!(kept.is_empty());
    }

    #[test]
    fn a_finding_that_names_no_symbol_is_kept() {
        let (kept, _) = run(
            vec![finding(
                "This will not compile",
                "The types do not line up.",
            )],
            &[DIFF],
        );
        assert_eq!(kept.len(), 1);
    }
}
