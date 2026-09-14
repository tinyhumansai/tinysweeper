//! The shapes shared between the brain and the hands.
//!
//! Every type here is serialised across the wire to `actions/ui-preview/`, so
//! the JSON layout is the contract: a variant renamed here is a command the
//! action no longer understands, and the failure shows up as a flow that
//! stops at step one on every pull request. `driver.mjs` mirrors [`Command`]
//! one arm to one arm, and the test at the bottom pins the wire spelling of
//! each so a rename cannot be quiet.
//!
//! Two of these come *from* the action — [`Observation`] and [`Manifest`] —
//! and are untrusted: a same-repository pull request can edit the workflow
//! that runs the action and make it say anything. `manifest::validate` is the
//! only path a manifest takes into a [`Gallery`], and an observation reaches a
//! prompt only fenced as data.

use serde::{Deserialize, Serialize};

/// Which build a step ran against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The pull request's head.
    After,
    /// The merge-base with the target branch.
    Before,
}

/// How the brain names an element.
///
/// Playwright's user-facing locators only, on purpose. A CSS or XPath selector
/// would let the model reach for `div:nth-child(3) > span`, which breaks on the
/// next refactor and says nothing a reviewer can read; a role and a name is
/// what the user sees, and what the accessibility snapshot the model was
/// shown is made of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Locator {
    /// `getByRole(role, { name })`.
    Role {
        /// The ARIA role, e.g. `button`.
        role: String,
        /// The accessible name, matched as a substring unless `exact`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Whether `name` must match whole.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        exact: bool,
    },
    /// `getByLabel(text)`.
    Label {
        /// The label text.
        text: String,
    },
    /// `getByText(text)`.
    Text {
        /// The visible text.
        text: String,
        /// Whether `text` must match whole.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        exact: bool,
    },
    /// `getByPlaceholder(text)`.
    Placeholder {
        /// The placeholder text.
        text: String,
    },
    /// `getByTestId(id)`.
    TestId {
        /// The `data-testid` value.
        id: String,
    },
}

/// One element to point at on a screenshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalloutRequest {
    /// The element.
    pub locator: Locator,
    /// What the pill beside it says, six words at most.
    pub label: String,
}

/// One instruction to the browser.
///
/// The `op` tag is the wire name `driver.mjs` dispatches on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Navigate to a path on the served app.
    Goto {
        /// A path, never a full URL: the action owns the origin.
        path: String,
    },
    /// Click an element.
    Click {
        /// The element.
        locator: Locator,
    },
    /// Type into a field, replacing its contents.
    Fill {
        /// The field.
        locator: Locator,
        /// The text.
        value: String,
    },
    /// Press a key on the focused element, e.g. `Enter`.
    Press {
        /// A Playwright key name.
        key: String,
    },
    /// Choose an option in a `<select>`.
    Select {
        /// The select.
        locator: Locator,
        /// The option's value or label.
        value: String,
    },
    /// Hover an element, to open what appears on hover.
    Hover {
        /// The element.
        locator: Locator,
    },
    /// Wait for an element to be visible, or for a fixed time.
    Wait {
        /// The element to wait for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locator: Option<Locator>,
        /// Milliseconds to wait, capped by the action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ms: Option<u64>,
    },
    /// Take a full-page screenshot.
    Screenshot {
        /// The id later `annotate` commands refer to.
        id: String,
    },
    /// Draw numbered callouts on a screenshot already taken.
    Annotate {
        /// The `screenshot` id.
        shot: String,
        /// What to point at, in order; the first is callout 1.
        callouts: Vec<CalloutRequest>,
    },
    /// Start or stop the clip of this flow.
    Record {
        /// `true` to start, `false` to stop.
        start: bool,
    },
    /// The flow is finished.
    Done {
        /// Why — shown to nobody, logged for the operator.
        reason: String,
    },
}

/// What happened to one command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepResult {
    /// The command's position in the batch it came from.
    pub index: usize,
    /// Whether it succeeded.
    pub ok: bool,
    /// Playwright's error, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the action saw after executing a batch. Untrusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// Which build this came from.
    pub side: Side,
    /// The page's URL after the batch.
    pub url: String,
    /// Playwright's ARIA snapshot of the page, as YAML text.
    pub aria: String,
    /// The outcome of every command in the last batch.
    #[serde(default)]
    pub results: Vec<StepResult>,
    /// How many commands this flow has executed so far, on this side.
    #[serde(default)]
    pub steps: usize,
}

/// What the brain expects the base build to show at the end of a flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectBefore {
    /// The same screen; the change is somewhere else or purely cosmetic.
    Same,
    /// The screen or control does not exist there yet.
    Absent,
    /// The screen exists but looks or behaves differently.
    Different,
}

/// One user flow the pull request touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flow {
    /// A short stable id, `f1`, `f2`, …
    pub id: String,
    /// What a user is doing, as a title: "Create a new dynamic secret".
    pub title: String,
    /// Where the flow begins.
    pub start_path: String,
    /// What the screen should show when the flow is done.
    pub goal: String,
    /// What the base build is expected to show at the same point.
    pub expect_before: ExpectBefore,
}

/// A clip of one flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clip {
    /// Relative path of the H.264 file.
    pub mp4: String,
    /// Relative path of the preview GIF.
    pub gif: String,
}

/// One numbered callout as drawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Callout {
    /// The number in the circle.
    pub n: usize,
    /// The pill text.
    pub label: String,
}

/// One annotated screenshot in a flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Position within the flow, from 1.
    pub n: usize,
    /// The step the screenshot was taken at.
    pub step: usize,
    /// The path the browser was on.
    pub path: String,
    /// Relative path of the full annotated screenshot.
    pub full: String,
    /// Relative path of the crop around the callouts.
    pub crop: String,
    /// Relative path of the same step on the base build, when it got there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The callouts drawn, in number order.
    #[serde(default)]
    pub callouts: Vec<Callout>,
}

/// How a flow ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowStatus {
    /// Both sides ran to `done`.
    Ok,
    /// The head build ran to `done`; the base build failed a step.
    BeforeFailed,
    /// The head build failed a step.
    Failed,
}

/// One flow as the action reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowResult {
    /// The [`Flow::id`].
    pub id: String,
    /// The [`Flow::title`], echoed back.
    pub title: String,
    /// How it ended.
    pub status: FlowStatus,
    /// The step that failed, for the two failing statuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<usize>,
    /// The clip, when one was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<Clip>,
    /// The annotated screenshots.
    #[serde(default)]
    pub changes: Vec<Change>,
}

/// What the action uploaded. Untrusted until `manifest::validate` says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version; only `1` exists.
    pub version: u32,
    /// `owner/name`.
    pub repo: String,
    /// The pull request number.
    pub pull_request: u64,
    /// The head commit the `after` side was built from.
    pub head_sha: String,
    /// The merge-base the `before` side was built from.
    pub base_sha: String,
    /// The upload prefix under the commit, `run-<ts>`.
    pub run: String,
    /// The flows, in the order they ran.
    #[serde(default)]
    pub flows: Vec<FlowResult>,
}

/// A validated manifest with every URL composed and every caption attached.
///
/// The only input `render` accepts. Nothing in it came from the wire
/// unchecked: paths were validated and composed onto the operator's base
/// URL, text went through the safe-alphabet filter, and counts were capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gallery {
    /// The pull request number.
    pub number: u64,
    /// The head commit shown.
    pub head_sha: String,
    /// The flows with something to show.
    pub flows: Vec<GalleryFlow>,
    /// How many flows ran but produced nothing to show.
    pub empty_flows: usize,
    /// How many flows the manifest carried past the cap.
    pub dropped_flows: usize,
}

/// One flow, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryFlow {
    /// The [`Flow::id`].
    pub id: String,
    /// The title as it will be printed.
    pub title: String,
    /// The caption, once `caption` has run.
    pub caption: Option<String>,
    /// Whether the base build could not get to the end of this flow.
    pub is_new: bool,
    /// The clip's URLs, `(mp4, gif)`.
    pub clip: Option<(String, String)>,
    /// The screenshots, with URLs.
    pub changes: Vec<GalleryChange>,
}

/// One screenshot, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryChange {
    /// The full annotated screenshot.
    pub full_url: String,
    /// The crop.
    pub crop_url: String,
    /// The base build at the same step.
    pub before_url: Option<String>,
    /// The path the browser was on, for the alt text.
    pub path: String,
    /// The callouts, filtered.
    pub callouts: Vec<Callout>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_keeps_its_wire_spelling() {
        // `driver.mjs` dispatches on these exact strings. Renaming a variant
        // here without renaming it there is a flow that fails at step one on
        // every pull request, and this is where that shows up first.
        let cases = [
            (
                Command::Goto {
                    path: "/settings".into(),
                },
                r#"{"op":"goto","path":"/settings"}"#,
            ),
            (
                Command::Click {
                    locator: Locator::Role {
                        role: "button".into(),
                        name: Some("Save".into()),
                        exact: false,
                    },
                },
                r#"{"op":"click","locator":{"by":"role","role":"button","name":"Save"}}"#,
            ),
            (
                Command::Fill {
                    locator: Locator::Label {
                        text: "Name".into(),
                    },
                    value: "x".into(),
                },
                r#"{"op":"fill","locator":{"by":"label","text":"Name"},"value":"x"}"#,
            ),
            (
                Command::Press {
                    key: "Enter".into(),
                },
                r#"{"op":"press","key":"Enter"}"#,
            ),
            (
                Command::Wait {
                    locator: None,
                    ms: Some(500),
                },
                r#"{"op":"wait","ms":500}"#,
            ),
            (
                Command::Screenshot { id: "s1".into() },
                r#"{"op":"screenshot","id":"s1"}"#,
            ),
            (
                Command::Annotate {
                    shot: "s1".into(),
                    callouts: vec![CalloutRequest {
                        locator: Locator::TestId { id: "t".into() },
                        label: "New toggle".into(),
                    }],
                },
                r#"{"op":"annotate","shot":"s1","callouts":[{"locator":{"by":"test_id","id":"t"},"label":"New toggle"}]}"#,
            ),
            (
                Command::Record { start: true },
                r#"{"op":"record","start":true}"#,
            ),
            (
                Command::Done {
                    reason: "goal shown".into(),
                },
                r#"{"op":"done","reason":"goal shown"}"#,
            ),
        ];
        for (command, wire) in cases {
            assert_eq!(serde_json::to_string(&command).unwrap(), wire);
            assert_eq!(serde_json::from_str::<Command>(wire).unwrap(), command);
        }
    }

    #[test]
    fn a_manifest_round_trips_with_its_optional_fields_absent() {
        let wire = r#"{"version":1,"repo":"o/r","pull_request":7,"head_sha":"h","base_sha":"b","run":"run-1",
            "flows":[{"id":"f1","title":"T","status":"before_failed","failed_at":3,
                      "changes":[{"n":1,"step":4,"path":"/","full":"a.png","crop":"a.crop.png"}]}]}"#;
        let manifest: Manifest = serde_json::from_str(wire).unwrap();
        assert_eq!(manifest.flows[0].status, FlowStatus::BeforeFailed);
        assert_eq!(manifest.flows[0].failed_at, Some(3));
        assert!(manifest.flows[0].clip.is_none());
        assert!(manifest.flows[0].changes[0].before.is_none());
        assert!(manifest.flows[0].changes[0].callouts.is_empty());
    }
}
