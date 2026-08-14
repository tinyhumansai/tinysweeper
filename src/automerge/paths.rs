//! Path and author matching for the auto-merge policy.
//!
//! Both halves fail closed. A glob that does not compile becomes an error the
//! caller turns into a refusal rather than an empty matcher that would silently
//! wave everything through, and a login comparison is exact rather than a
//! prefix — the lesson `findings::prior::is_own_login` already learned here,
//! where `starts_with` would have counted `tinysweeper-anything` as itself.

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Compile a set of path globs.
///
/// The error carries the offending pattern, because an operator whose merge
/// policy has quietly stopped matching needs to know which line to fix.
pub fn glob_set(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|err| format!("`{pattern}`: {err}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|err| format!("could not build the matcher: {err}"))
}

/// Whether `login` is the account `configured` names.
///
/// A GitHub App acts as `<slug>[bot]`, so that suffix is presentation and is
/// normalised off both sides before an exact, case-insensitive comparison.
/// Everything else is exact: `dependabot-evil[bot]` is a login anyone can
/// register, and a `starts_with` here would hand it the dependency-bump
/// exemption. An empty login or an empty configured name matches nothing, so
/// an unconfigured bot is never accidentally everyone.
pub fn logins_match(configured: &str, login: &str) -> bool {
    let expected = bare(configured.trim());
    let actual = bare(login.trim());
    if expected.is_empty() || actual.is_empty() {
        return false;
    }
    expected.eq_ignore_ascii_case(actual)
}

fn bare(login: &str) -> &str {
    login.strip_suffix("[bot]").unwrap_or(login)
}

/// The first label in `carried` that `configured` names, if any.
///
/// Trimmed and case-insensitive, because that is how GitHub itself treats a
/// label name: a repository cannot hold both `automerge` and `AutoMerge`, so
/// the two are one label wearing two spellings. Comparing them exactly is a
/// bug in the dangerous direction — a `Do-Not-Merge` veto read against a
/// configured `do-not-merge` looks *absent*, and the gate merges straight past
/// a human's stop sign. `issues::labels` has always compared this way; the gate
/// did not, which meant one label name meant two different things depending on
/// which half of the bot was reading it.
///
/// Returns the label as the pull request spells it, not as the config does, so
/// a refusal quotes what a maintainer will actually see on the pull request.
pub fn carries<'a>(carried: &'a [String], configured: &[String]) -> Option<&'a String> {
    carried.iter().find(|label| {
        configured
            .iter()
            .any(|name| name.trim().eq_ignore_ascii_case(label.trim()))
    })
}
