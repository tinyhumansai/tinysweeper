//! Turning what the action uploaded into something safe to publish.
//!
//! A manifest is written by a job in the reviewed repository's CI, and a
//! same-repository pull request can edit that job. So it is handled exactly
//! like a diff: the server takes facts from it — which flows ran, which files
//! were uploaded — and takes nothing it will *act on* unchecked.
//!
//! The rules, each of which closes a specific door:
//!
//! - **No URLs.** A manifest carries relative paths; the server composes every
//!   URL from the operator's `preview.public_base_url`, the commit, and the
//!   run. A manifest that could name a host could have the bot embed pictures
//!   from anywhere into every reviewer's browser.
//! - **Paths are one or two plain segments.** No `..`, no leading slash, no
//!   scheme, nothing outside `[A-Za-z0-9._-]`, and an extension the comment
//!   knows how to render. A path is decoration; the allow-list is the rule.
//! - **Text is filtered, then escaped.** Titles and labels are model-authored
//!   or repository-authored; either way they end up inside HTML the comment
//!   renders. They pass a safe alphabet here and `render` escapes them again.
//! - **Counts are capped, and the cap is reported.** A manifest with five
//!   hundred flows is either a bug or an attempt to fill the comment; the first
//!   `max_flows` are kept and the rest are counted into the summary rather
//!   than dropped silently.
//! - **Identity is checked.** The repository, number and head commit must be
//!   the ones the session was opened for.

use crate::error::{Error, Result};
use crate::preview::types::{
    Callout, Flow, FlowStatus, Gallery, GalleryChange, GalleryFlow, Manifest,
};

/// The manifest schema this crate understands.
pub const VERSION: u32 = 1;

/// How many screenshots one flow may contribute.
pub const MAX_CHANGES_PER_FLOW: usize = 6;

/// How many callouts one screenshot may carry.
///
/// Three, because a screenshot with eight numbered pills on it is a diagram
/// nobody reads; the brain is told the same number in its instructions.
pub const MAX_CALLOUTS: usize = 3;

/// The longest title, in characters.
pub const MAX_TITLE: usize = 90;

/// The longest callout label, in characters.
pub const MAX_LABEL: usize = 48;

/// What the session was opened for; the manifest must agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected<'a> {
    /// `owner/name`.
    pub repo: &'a str,
    /// The pull request number.
    pub number: u64,
    /// The head commit.
    pub head_sha: &'a str,
}

/// Parse a manifest body.
pub fn parse(bytes: &[u8]) -> Result<Manifest> {
    serde_json::from_slice(bytes).map_err(|err| Error::Config(format!("manifest: {err}")))
}

/// Validate a manifest against the session and compose its URLs.
///
/// `base_url` is the operator's `preview.public_base_url`; the run prefix is
/// `{base}/{owner}/{name}/{head_sha}/{run}/`.
///
/// `planned` is the session's own plan — the flows the brain told the hands to
/// drive when the session opened. `driven` is which of those the hands
/// actually called `/step` for at least once (the session's `states` keys).
/// The manifest is written by a job in the reviewed repository's CI, so a
/// same-repository pull request that edits that job (or the action it calls)
/// can submit any flow id, title or status it likes — including one it
/// planned but skipped driving entirely, with a fabricated `before_failed`
/// status and arbitrary asset names. Binding every manifest flow to one the
/// session both planned *and* drove, and taking the title from the plan
/// rather than the manifest, is what stops that from fabricating a gallery
/// entry or mislabelling one "new in this PR".
/// Where a run's files are served from, and so what its URLs look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage<'a> {
    /// An object store the hands upload to; `base_url` is the operator's
    /// `preview.public_base_url`.
    Bucket {
        /// The origin, no trailing slash needed.
        base_url: &'a str,
    },
    /// A branch of the reviewed repository, written by the server through
    /// the App. Files render through GitHub's own blob route with `?raw=true`,
    /// which works for public and private repositories alike — the viewer's
    /// own session authorises it — where `raw.githubusercontent.com` would
    /// not.
    Branch {
        /// The branch name, e.g. `tinysweeper/ui-previews`.
        branch: &'a str,
    },
}

impl Storage<'_> {
    /// The URL of one relative asset path within a run.
    pub fn url(&self, repo: &str, head_sha: &str, run: &str, path: &str) -> String {
        match self {
            Storage::Bucket { base_url } => format!(
                "{}/{repo}/{head_sha}/{run}/{path}",
                base_url.trim_end_matches('/')
            ),
            Storage::Branch { branch } => {
                format!("https://github.com/{repo}/blob/{branch}/{head_sha}/{run}/{path}?raw=true")
            }
        }
    }

    /// The path a file is committed at, for [`Storage::Branch`].
    pub fn commit_path(head_sha: &str, run: &str, path: &str) -> String {
        format!("{head_sha}/{run}/{path}")
    }
}

pub fn validate(
    manifest: &Manifest,
    expected: &Expected<'_>,
    storage: Storage<'_>,
    max_flows: usize,
    planned: &[Flow],
    driven: &std::collections::BTreeSet<String>,
) -> Result<Gallery> {
    if manifest.version != VERSION {
        return Err(Error::Config(format!(
            "manifest version {} is not {VERSION}",
            manifest.version
        )));
    }
    if manifest.repo != expected.repo {
        return Err(Error::Config(format!(
            "manifest is for `{}`, session is for `{}`",
            manifest.repo, expected.repo
        )));
    }
    if manifest.pull_request != expected.number {
        return Err(Error::Config(format!(
            "manifest is for #{}, session is for #{}",
            manifest.pull_request, expected.number
        )));
    }
    if manifest.head_sha != expected.head_sha {
        return Err(Error::Config(format!(
            "manifest is for {}, session is for {}",
            manifest.head_sha, expected.head_sha
        )));
    }
    if !is_segment(&manifest.run) || manifest.run.len() > 64 {
        return Err(Error::Config(format!(
            "manifest run `{}` is not a plain name",
            manifest.run
        )));
    }
    if !is_segment(&manifest.head_sha) {
        return Err(Error::Config(
            "manifest head_sha is not a plain name".into(),
        ));
    }

    let mut files: Vec<String> = Vec::new();
    let url = |path: &str, files: &mut Vec<String>| -> Result<String> {
        if !is_asset_path(path) {
            return Err(Error::Config(format!(
                "manifest names `{path}`, which is not a relative asset path"
            )));
        }
        if !files.iter().any(|f| f == path) {
            files.push(path.to_string());
        }
        Ok(storage.url(&manifest.repo, &manifest.head_sha, &manifest.run, path))
    };

    let mut flows = Vec::new();
    let mut empty_flows = 0;
    for flow in manifest.flows.iter().take(max_flows) {
        // Only a flow the session both planned and actually drove may publish
        // anything: an id the plan never issued, or one the hands never sent
        // a single `/step` call for, is either a bug in the hands or a
        // same-repository pull request that skipped driving to submit a
        // fabricated result, and either way it gets nothing published.
        let Some(plan) = planned.iter().find(|p| p.id == flow.id) else {
            empty_flows += 1;
            continue;
        };
        if !driven.contains(&flow.id) {
            empty_flows += 1;
            continue;
        }
        // A flow that never got a screenshot has nothing to put in a cell. A
        // clip alone is kept: a clip of a flow that works is worth showing
        // even when the brain never pointed at anything.
        let changes: Vec<GalleryChange> = flow
            .changes
            .iter()
            .take(MAX_CHANGES_PER_FLOW)
            .map(|change| {
                Ok(GalleryChange {
                    full_url: url(&change.full, &mut files)?,
                    crop_url: url(&change.crop, &mut files)?,
                    before_url: change
                        .before
                        .as_deref()
                        .map(|p| url(p, &mut files))
                        .transpose()?,
                    path: text(&change.path, MAX_TITLE),
                    callouts: change
                        .callouts
                        .iter()
                        .take(MAX_CALLOUTS)
                        .map(|callout| Callout {
                            n: callout.n,
                            label: text(&callout.label, MAX_LABEL),
                        })
                        .collect(),
                })
            })
            .collect::<Result<_>>()?;
        let clip = match &flow.clip {
            Some(clip) => Some((url(&clip.video, &mut files)?, url(&clip.gif, &mut files)?)),
            None => None,
        };
        // A flow whose head build failed has nothing trustworthy to show: the
        // screenshots it took are of a path that did not reach its goal.
        if flow.status == FlowStatus::Failed || (changes.is_empty() && clip.is_none()) {
            empty_flows += 1;
            continue;
        }
        flows.push(GalleryFlow {
            id: text(&flow.id, 16),
            // The plan's title, not the manifest's: the manifest is written
            // by repository CI and the title is what every reviewer reads
            // next to the pictures.
            title: text(&plan.title, MAX_TITLE),
            caption: None,
            is_new: flow.status == FlowStatus::BeforeFailed,
            clip,
            changes,
        });
    }

    Ok(Gallery {
        number: manifest.pull_request,
        head_sha: manifest.head_sha.clone(),
        run: manifest.run.clone(),
        files,
        flows,
        empty_flows,
        dropped_flows: manifest.flows.len().saturating_sub(max_flows),
    })
}

/// One path segment of the allow-listed alphabet.
fn is_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A relative asset path: one or two segments, with a renderable extension.
fn is_asset_path(path: &str) -> bool {
    let mut segments = path.split('/');
    let (Some(first), second, None) = (segments.next(), segments.next(), segments.next()) else {
        return false;
    };
    let file = second.unwrap_or(first);
    let dir_ok = second.is_none_or(|_| is_segment(first));
    dir_ok
        && is_segment(file)
        && [".png", ".gif", ".mp4", ".webm"]
            .iter()
            .any(|ext| file.ends_with(ext))
}

/// Text that will be printed: filtered to a safe alphabet and cut to length.
///
/// Filtered rather than escaped, for the reason `overview::mermaid::label`
/// gives: an allow-list is a statement about what can appear, an escape is a
/// guess about a renderer. `render` escapes on top of this, so the two are
/// belt and braces rather than one or the other.
pub fn text(s: &str, max: usize) -> String {
    let filtered: String = s
        .chars()
        .filter(|c| {
            c.is_alphanumeric()
                || matches!(
                    c,
                    ' ' | '.'
                        | ','
                        | ':'
                        | ';'
                        | '\''
                        | '"'
                        | '('
                        | ')'
                        | '-'
                        | '/'
                        | '&'
                        | '!'
                        | '?'
                        | '_'
                        | '#'
                        | '+'
                        | '%'
                        | '@'
                )
        })
        .collect();
    let trimmed = filtered.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() <= max {
        return trimmed;
    }
    let cut: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::types::{Change, Clip, FlowResult};

    const BASE: Storage<'static> = Storage::Bucket {
        base_url: "https://previews.example.org",
    };

    fn expected() -> Expected<'static> {
        Expected {
            repo: "o/r",
            number: 7,
            head_sha: "abc123",
        }
    }

    /// The plan behind every fixture manifest below: enough ids (`f0`..`f9`)
    /// to cover the cap test, each with a title distinct from the manifest's
    /// own so a test can tell which one `validate` actually used.
    fn planned() -> Vec<Flow> {
        (0..10)
            .map(|n| Flow {
                id: format!("f{n}"),
                title: format!("Planned flow {n}"),
                start_path: "/".into(),
                goal: "reach the goal".into(),
                expect_before: crate::preview::types::ExpectBefore::Same,
            })
            .collect()
    }

    /// Every planned flow, as driven: the common case for fixtures below that
    /// are not specifically testing the driven/undriven distinction.
    fn driven() -> std::collections::BTreeSet<String> {
        planned().into_iter().map(|f| f.id).collect()
    }

    fn manifest() -> Manifest {
        Manifest {
            version: 1,
            repo: "o/r".into(),
            pull_request: 7,
            head_sha: "abc123".into(),
            base_sha: "base".into(),
            run: "run-1".into(),
            flows: vec![FlowResult {
                id: "f1".into(),
                title: "Toggle the thing".into(),
                status: FlowStatus::Ok,
                failed_at: None,
                clip: Some(Clip {
                    video: "clip-01.mp4".into(),
                    gif: "clip-01.gif".into(),
                }),
                changes: vec![Change {
                    n: 1,
                    step: 3,
                    path: "/settings".into(),
                    full: "change-01.png".into(),
                    crop: "change-01.crop.png".into(),
                    before: Some("before-01.png".into()),
                    callouts: vec![Callout {
                        n: 1,
                        label: "New toggle".into(),
                    }],
                }],
            }],
        }
    }

    #[test]
    fn urls_are_composed_from_the_operators_base_and_never_taken_from_the_file() {
        let gallery = validate(&manifest(), &expected(), BASE, 4, &planned(), &driven()).unwrap();
        let change = &gallery.flows[0].changes[0];
        assert_eq!(
            change.crop_url,
            "https://previews.example.org/o/r/abc123/run-1/change-01.crop.png"
        );
        assert_eq!(
            gallery.flows[0].clip.as_ref().unwrap().1,
            "https://previews.example.org/o/r/abc123/run-1/clip-01.gif"
        );
    }

    #[test]
    fn a_path_that_is_not_a_plain_relative_asset_is_refused() {
        for bad in [
            "https://evil.example/x.png",
            "../x.png",
            "/x.png",
            "a/b/c.png",
            "x.svg",
            "x.html",
            "",
            ".hidden.png",
            "a b.png",
        ] {
            let mut m = manifest();
            m.flows[0].changes[0].crop = bad.into();
            assert!(
                validate(&m, &expected(), BASE, 4, &planned(), &driven()).is_err(),
                "`{bad}` should be refused"
            );
        }
        let mut m = manifest();
        m.flows[0].changes[0].crop = "shots/change-01.crop.png".into();
        assert!(validate(&m, &expected(), BASE, 4, &planned(), &driven()).is_ok());
    }

    #[test]
    fn a_manifest_for_another_session_is_refused() {
        let mut other = manifest();
        other.head_sha = "def456".into();
        assert!(validate(&other, &expected(), BASE, 4, &planned(), &driven()).is_err());
        let mut other = manifest();
        other.pull_request = 8;
        assert!(validate(&other, &expected(), BASE, 4, &planned(), &driven()).is_err());
        let mut other = manifest();
        other.repo = "o/other".into();
        assert!(validate(&other, &expected(), BASE, 4, &planned(), &driven()).is_err());
        let mut other = manifest();
        other.version = 2;
        assert!(validate(&other, &expected(), BASE, 4, &planned(), &driven()).is_err());
    }

    #[test]
    fn flows_past_the_cap_are_counted_not_silently_dropped() {
        let mut m = manifest();
        let flow = m.flows[0].clone();
        m.flows = (0..10)
            .map(|n| FlowResult {
                id: format!("f{n}"),
                ..flow.clone()
            })
            .collect();
        let gallery = validate(&m, &expected(), BASE, 4, &planned(), &driven()).unwrap();
        assert_eq!(gallery.flows.len(), 4);
        assert_eq!(gallery.dropped_flows, 6);
    }

    #[test]
    fn a_failed_flow_and_an_empty_flow_are_counted_rather_than_shown() {
        let mut m = manifest();
        let mut failed = m.flows[0].clone();
        failed.id = "f2".into();
        failed.status = FlowStatus::Failed;
        let mut empty = m.flows[0].clone();
        empty.id = "f3".into();
        empty.clip = None;
        empty.changes = vec![];
        m.flows.extend([failed, empty]);
        let gallery = validate(&m, &expected(), BASE, 4, &planned(), &driven()).unwrap();
        assert_eq!(gallery.flows.len(), 1);
        assert_eq!(gallery.empty_flows, 2);
    }

    #[test]
    fn a_flow_id_the_session_never_planned_is_refused_not_published() {
        let mut m = manifest();
        m.flows[0].id = "not-a-planned-flow".into();
        let gallery = validate(&m, &expected(), BASE, 4, &planned(), &driven()).unwrap();
        assert!(
            gallery.flows.is_empty(),
            "an unplanned flow publishes nothing"
        );
        assert_eq!(gallery.empty_flows, 1);
    }

    #[test]
    fn a_planned_flow_the_session_never_drove_is_refused_not_published() {
        // Planned (its id is in `planned()`) but the hands never called
        // `/step` for it, so it is not in `driven()` — the same-repository
        // CI job could otherwise skip driving entirely and submit a
        // fabricated `before_failed` result with invented asset names.
        let m = manifest();
        assert_eq!(m.flows[0].id, "f1");
        let undriven: std::collections::BTreeSet<String> =
            driven().into_iter().filter(|id| id != "f1").collect();
        let gallery = validate(&m, &expected(), BASE, 4, &planned(), &undriven).unwrap();
        assert!(
            gallery.flows.is_empty(),
            "an undriven flow publishes nothing even though it was planned"
        );
        assert_eq!(gallery.empty_flows, 1);
    }

    #[test]
    fn the_gallery_title_comes_from_the_plan_not_the_manifest() {
        let mut m = manifest();
        m.flows[0].title = "a hostile title the CI job made up".into();
        let gallery = validate(&m, &expected(), BASE, 4, &planned(), &driven()).unwrap();
        assert_eq!(gallery.flows[0].title, "Planned flow 1");
    }

    #[test]
    fn a_flow_the_base_build_could_not_finish_is_marked_new() {
        let mut m = manifest();
        m.flows[0].status = FlowStatus::BeforeFailed;
        m.flows[0].failed_at = Some(2);
        let gallery = validate(&m, &expected(), BASE, 4, &planned(), &driven()).unwrap();
        assert!(gallery.flows[0].is_new);
    }

    #[test]
    fn hostile_text_loses_its_markup_and_is_cut_to_length() {
        assert_eq!(
            text("<img src=x onerror=alert(1)> New   toggle", MAX_LABEL),
            "img srcx onerroralert(1) New toggle"
        );
        let long = "word ".repeat(40);
        let cut = text(&long, MAX_TITLE);
        assert!(cut.chars().count() <= MAX_TITLE);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn callouts_and_changes_are_capped() {
        let mut m = manifest();
        let change = m.flows[0].changes[0].clone();
        m.flows[0].changes = (0..MAX_CHANGES_PER_FLOW + 2)
            .map(|n| Change {
                n: n + 1,
                callouts: (0..MAX_CALLOUTS + 2)
                    .map(|c| Callout {
                        n: c + 1,
                        label: "l".into(),
                    })
                    .collect(),
                ..change.clone()
            })
            .collect();
        let gallery = validate(&m, &expected(), BASE, 4, &planned(), &driven()).unwrap();
        assert_eq!(gallery.flows[0].changes.len(), MAX_CHANGES_PER_FLOW);
        assert_eq!(gallery.flows[0].changes[0].callouts.len(), MAX_CALLOUTS);
    }
}
