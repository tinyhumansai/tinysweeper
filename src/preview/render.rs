//! The preview comment: one durable comment per pull request.
//!
//! Always compiled. This renders the body; [`crate::preview::apply`] is what
//! puts it on GitHub, and it edits the same comment on every run rather than
//! adding one — a comment per push is a gallery nobody scrolls to.
//!
//! The layout is a two-column HTML table of thumbnails, each linking to the
//! full asset, with the flow's title in bold and a one-line caption after a
//! dash. HTML rather than Markdown because Markdown cannot size an image or
//! put two side by side, and a full-width 2880-pixel screenshot per flow is
//! the difference between a comment people look at and one they collapse.
//!
//! Every string that reaches this file has already been through
//! `manifest::text`; it is escaped again here anyway. Two layers because they
//! guard different things — the filter decides what a label may *be*, the
//! escape decides how it is *written* — and because a future caller that
//! skips the filter should still not be able to close a `<td>`.

use std::fmt::Write as _;

use crate::VERSION;
use crate::forge::types::{CheckImage, MAX_CHECK_IMAGES};
use crate::preview::types::{Gallery, GalleryFlow};

/// The marker that identifies tinysweeper's own preview comment.
///
/// In the body so a later run finds and edits the same comment. Spelled out
/// rather than composed from [`crate::MARKER_PREFIX`] because a `const` cannot
/// call `format!`; the test below keeps them from drifting.
pub const MARKER: &str = "<!-- tinysweeper:ui-preview -->";

/// The name of the check run the preview publishes under.
pub const CHECK_NAME: &str = "tinysweeper/ui-preview";

/// Thumbnail width, in CSS pixels. Two of them fit a GitHub comment.
const THUMB_WIDTH: u32 = 380;

/// Render the comment, or `None` when there is nothing to show.
///
/// The body [`crate::preview::apply::publish`] writes over an existing
/// preview comment when the current head has nothing to show.
///
/// Not the same as never having posted at all — see [`comment`]'s own
/// "`None` rather than..." reasoning, which still applies to a pull request
/// that never had a preview comment. But once a comment exists, showing a
/// stale head's screenshots under a check run that now says there is nothing
/// to see is actively misleading, not merely quiet.
pub fn stale(head_sha: &str) -> String {
    format!(
        "{MARKER}\n<!-- tinysweeper:ui-preview-sha={} -->\n### 🎬 UI preview\n\n_No visible change on this commit._\n",
        escape(head_sha),
    )
}

/// `None` rather than "no visible change found": the pull request that has no
/// UI change has plenty of other comments, and one more saying so is noise.
/// The check run says it instead, where it costs no screen space.
pub fn comment(gallery: &Gallery) -> Option<String> {
    let cells = cells(gallery);
    if cells.is_empty() {
        return None;
    }

    let mut body = format!(
        "{MARKER}\n<!-- tinysweeper:ui-preview-sha={} -->\n### 🎬 UI preview — PR #{}\n\n<table>\n",
        escape(&gallery.head_sha),
        gallery.number
    );
    for row in cells.chunks(2) {
        body.push_str("  <tr>");
        for cell in row {
            let _ = write!(body, "<td width=\"50%\" valign=\"top\">{cell}</td>");
        }
        body.push_str("</tr>\n");
    }
    body.push_str("</table>\n");

    let omitted = gallery.empty_flows + gallery.dropped_flows;
    if omitted > 0 {
        let _ = write!(
            body,
            "\n<sub>{} more flow{} ran without producing a picture.</sub>\n",
            omitted,
            if omitted == 1 { "" } else { "s" }
        );
    }
    let _ = write!(
        body,
        "\n![tinysweeper {VERSION}](https://img.shields.io/badge/tinysweeper-{}-8b949e?style=flat-square)\n",
        VERSION.replace('-', "--")
    );
    Some(body)
}

/// The images to attach to the check run: one crop per flow, captioned.
///
/// The check's page is the one place in the checks tab that can show a
/// picture, so the first screenshot of each flow goes there; the rest live
/// in the comment. Capped at the API's ceiling, most-changed-first is not
/// knowable, so first-planned-first.
pub fn check_images(gallery: &Gallery) -> Vec<CheckImage> {
    gallery
        .flows
        .iter()
        .filter_map(|flow| {
            flow.changes.first().map(|change| CheckImage {
                alt: flow.title.clone(),
                image_url: change.crop_url.clone(),
                caption: flow.caption.clone(),
            })
        })
        .take(MAX_CHECK_IMAGES)
        .collect()
}

/// The check run's one-line title.
pub fn check_title(gallery: &Gallery) -> String {
    match gallery.flows.len() {
        0 => "No visible change found".to_string(),
        1 => "1 flow previewed".to_string(),
        n => format!("{n} flows previewed"),
    }
}

/// Every table cell, in order: for each flow its clip, then its screenshots.
fn cells(gallery: &Gallery) -> Vec<String> {
    let mut out = Vec::new();
    for flow in &gallery.flows {
        if let Some((video, gif)) = &flow.clip {
            out.push(cell(video, gif, flow, None));
        }
        for change in &flow.changes {
            out.push(cell(
                &change.full_url,
                &change.crop_url,
                flow,
                Some(&change.path),
            ));
        }
    }
    out
}

/// One `<td>` body: a thumbnail linking to the full asset, then the caption.
fn cell(href: &str, thumb: &str, flow: &GalleryFlow, path: Option<&str>) -> String {
    let title = escape(&flow.title);
    let mut cell = format!(
        "<a href=\"{}\"><img src=\"{}\" width=\"{THUMB_WIDTH}\" alt=\"{}\"></a><br><b>{}</b>",
        escape(href),
        escape(thumb),
        escape(path.unwrap_or(&flow.title)),
        title
    );
    if flow.is_new {
        cell.push_str(" <sub>new in this PR</sub>");
    }
    if let Some(caption) = &flow.caption {
        let _ = write!(cell, " — {}", escape(caption));
    }
    cell
}

/// HTML-escape for text and attribute positions.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MARKER_PREFIX;
    use crate::preview::types::{Callout, GalleryChange};

    fn gallery() -> Gallery {
        Gallery {
            number: 8634,
            head_sha: "53ee083".into(),
            run: "run-1".into(),
            files: vec![],
            flows: vec![
                GalleryFlow {
                    id: "f1".into(),
                    title: "Toggle 'Dynamic Secrets' experimental setting".into(),
                    caption: Some("Experimental settings gain a Dynamic Secrets toggle.".into()),
                    is_new: true,
                    clip: Some((
                        "https://p.example/o/r/53ee083/run-1/clip-01.mp4".into(),
                        "https://p.example/o/r/53ee083/run-1/clip-01.gif".into(),
                    )),
                    changes: vec![GalleryChange {
                        full_url: "https://p.example/o/r/53ee083/run-1/change-01.png".into(),
                        crop_url: "https://p.example/o/r/53ee083/run-1/change-01.crop.png".into(),
                        before_url: None,
                        path: "/settings/experimental".into(),
                        callouts: vec![Callout {
                            n: 1,
                            label: "New Dynamic Secrets opt-in".into(),
                        }],
                    }],
                },
                GalleryFlow {
                    id: "f2".into(),
                    title: "Create new dynamic secret".into(),
                    caption: None,
                    is_new: false,
                    clip: None,
                    changes: vec![GalleryChange {
                        full_url: "https://p.example/o/r/53ee083/run-1/change-02.png".into(),
                        crop_url: "https://p.example/o/r/53ee083/run-1/change-02.crop.png".into(),
                        before_url: Some(
                            "https://p.example/o/r/53ee083/run-1/before-02.png".into(),
                        ),
                        path: "/secrets".into(),
                        callouts: vec![],
                    }],
                },
            ],
            empty_flows: 1,
            dropped_flows: 0,
        }
    }

    #[test]
    fn the_comment_is_the_two_column_gallery() {
        let body = comment(&gallery()).expect("something to show");
        let expected = format!(
            "{MARKER}\n<!-- tinysweeper:ui-preview-sha=53ee083 -->\n### 🎬 UI preview — PR #8634\n\n<table>\n  <tr><td width=\"50%\" valign=\"top\"><a href=\"https://p.example/o/r/53ee083/run-1/clip-01.mp4\"><img src=\"https://p.example/o/r/53ee083/run-1/clip-01.gif\" width=\"380\" alt=\"Toggle &#39;Dynamic Secrets&#39; experimental setting\"></a><br><b>Toggle &#39;Dynamic Secrets&#39; experimental setting</b> <sub>new in this PR</sub> — Experimental settings gain a Dynamic Secrets toggle.</td><td width=\"50%\" valign=\"top\"><a href=\"https://p.example/o/r/53ee083/run-1/change-01.png\"><img src=\"https://p.example/o/r/53ee083/run-1/change-01.crop.png\" width=\"380\" alt=\"/settings/experimental\"></a><br><b>Toggle &#39;Dynamic Secrets&#39; experimental setting</b> <sub>new in this PR</sub> — Experimental settings gain a Dynamic Secrets toggle.</td></tr>\n  <tr><td width=\"50%\" valign=\"top\"><a href=\"https://p.example/o/r/53ee083/run-1/change-02.png\"><img src=\"https://p.example/o/r/53ee083/run-1/change-02.crop.png\" width=\"380\" alt=\"/secrets\"></a><br><b>Create new dynamic secret</b></td></tr>\n</table>\n\n<sub>1 more flow ran without producing a picture.</sub>\n\n![tinysweeper {VERSION}](https://img.shields.io/badge/tinysweeper-{}-8b949e?style=flat-square)\n",
            VERSION.replace('-', "--")
        );
        assert_eq!(body, expected);
    }

    #[test]
    fn nothing_to_show_is_no_comment() {
        let empty = Gallery {
            flows: vec![],
            ..gallery()
        };
        assert!(comment(&empty).is_none());
        assert_eq!(check_title(&empty), "No visible change found");
    }

    #[test]
    fn text_is_escaped_even_though_it_was_filtered_upstream() {
        let mut g = gallery();
        g.flows[0].title = "a <b> & \"c\"".into();
        let body = comment(&g).unwrap();
        assert!(body.contains("<b>a &lt;b&gt; &amp; &quot;c&quot;</b>"));
        assert!(!body.contains("<b>a <b>"));
    }

    #[test]
    fn the_check_carries_one_crop_per_flow_with_its_caption() {
        let images = check_images(&gallery());
        assert_eq!(images.len(), 2);
        assert_eq!(
            images[0].image_url,
            "https://p.example/o/r/53ee083/run-1/change-01.crop.png"
        );
        assert_eq!(
            images[0].caption.as_deref(),
            Some("Experimental settings gain a Dynamic Secrets toggle.")
        );
        assert_eq!(images[1].caption, None);
        assert_eq!(check_title(&gallery()), "2 flows previewed");
    }

    #[test]
    fn the_marker_uses_the_shared_prefix() {
        assert!(MARKER.starts_with(&format!("<!-- {MARKER_PREFIX}")));
    }
}
