//! The shapes `wireframe` produces: one model call, capped, then rendered.

use serde::{Deserialize, Serialize};

/// What happened to one screen or modal between the merge-base and the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenStatus {
    /// Did not exist before this pull request.
    Added,
    /// Existed before this pull request and is gone after it.
    Removed,
    /// Existed before and after, but looks or behaves differently.
    Changed,
}

/// One screen or modal the diff touches, wireframed before and after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    /// A short title naming it: "Settings / Experimental", "Delete secret
    /// (confirm)".
    pub title: String,
    /// What happened to it.
    pub status: ScreenStatus,
    /// The ASCII wireframe before this pull request. `None` for
    /// [`ScreenStatus::Added`].
    pub before: Option<String>,
    /// The ASCII wireframe after this pull request. `None` for
    /// [`ScreenStatus::Removed`].
    pub after: Option<String>,
}

/// The whole gallery for one pull request, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WireframeSet {
    /// The screens, capped at `wireframe.max_screens`.
    pub screens: Vec<Screen>,
    /// How many screens the model returned past the cap.
    pub dropped: usize,
}
