//! The wireframe gallery: one durable pull request comment.
//!
//! Always compiled, pure string formatting — no model, no I/O — the same
//! discipline as `preview::render` and `overview::render`: a body this
//! function can produce from a fixture is a body a golden test can pin.

use std::fmt::Write as _;

use crate::VERSION;
use crate::wireframe::types::{Screen, ScreenStatus, WireframeSet};

/// The marker that identifies tinysweeper's own wireframe comment.
///
/// In the body so a later run finds and edits the same comment, rather than
/// leaving a scroll of stale wireframes behind on every push.
pub const MARKER: &str = "<!-- tinysweeper:wireframe -->";

/// Render the comment, or `None` when there is nothing to show.
///
/// `None` rather than "no UI change found": a pull request with nothing to
/// wireframe has plenty of other comments, and one more saying so is noise.
pub fn comment(set: &WireframeSet) -> Option<String> {
    if set.screens.is_empty() {
        return None;
    }

    let mut body = format!("{MARKER}\n### \u{1f5bc}\u{fe0f} UI wireframes\n\n");
    for screen in &set.screens {
        body.push_str(&section(screen));
    }

    if set.dropped > 0 {
        let _ = write!(
            body,
            "<sub>{} more screen{} omitted past the cap.</sub>\n\n",
            set.dropped,
            if set.dropped == 1 { "" } else { "s" }
        );
    }

    let _ = writeln!(
        body,
        "![tinysweeper {VERSION}](https://img.shields.io/badge/tinysweeper-{}-8b949e?style=flat-square)",
        VERSION.replace('-', "--")
    );
    Some(body)
}

/// One screen: its title, status tag, and a before/after pair of columns.
fn section(screen: &Screen) -> String {
    let tag = match screen.status {
        ScreenStatus::Added => " <sub>new in this PR</sub>",
        ScreenStatus::Removed => " <sub>removed in this PR</sub>",
        ScreenStatus::Changed => "",
    };
    let mut out = format!("<b>{}</b>{tag}\n\n<table>\n  <tr>\n", escape(&screen.title));
    out.push_str(&cell("Before", screen.before.as_deref()));
    out.push_str(&cell("After", screen.after.as_deref()));
    out.push_str("  </tr>\n</table>\n\n");
    out
}

/// One `<td>`: a labelled `<pre>` block, or an em dash when that side has
/// nothing — a screen that is new in this pull request has no "before".
fn cell(label: &str, ascii: Option<&str>) -> String {
    match ascii {
        Some(ascii) => format!(
            "    <td width=\"50%\" valign=\"top\"><sub>{label}</sub><br><pre>{}</pre></td>\n",
            escape(ascii)
        ),
        None => format!(
            "    <td width=\"50%\" valign=\"top\"><sub>{label}</sub><br><em>\u{2014}</em></td>\n"
        ),
    }
}

/// HTML-escape for text and `<pre>` positions.
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

    fn set() -> WireframeSet {
        WireframeSet {
            screens: vec![
                Screen {
                    title: "Settings / Experimental".into(),
                    status: ScreenStatus::Added,
                    before: None,
                    after: Some(
                        "+------------------+\n| [ ] Dynamic Secrets |\n+------------------+"
                            .into(),
                    ),
                },
                Screen {
                    title: "Secrets list".into(),
                    status: ScreenStatus::Changed,
                    before: Some("+-------+\n| empty |\n+-------+".into()),
                    after: Some("+-------+\n| 1 item |\n+-------+".into()),
                },
            ],
            dropped: 1,
        }
    }

    #[test]
    fn the_comment_shows_every_screen_with_a_before_and_after_column() {
        let body = comment(&set()).expect("something to show");
        assert!(body.starts_with(MARKER));
        assert!(body.contains("Settings / Experimental"));
        assert!(body.contains("new in this PR"));
        assert!(
            body.contains("<em>\u{2014}</em>"),
            "no before for an added screen"
        );
        assert!(body.contains("Dynamic Secrets"));
        assert!(body.contains("1 more screen omitted past the cap."));
    }

    #[test]
    fn nothing_to_show_is_no_comment() {
        assert!(comment(&WireframeSet::default()).is_none());
    }

    #[test]
    fn text_is_escaped() {
        let mut s = set();
        s.screens[0].title = "a <b> & \"c\"".into();
        let body = comment(&s).unwrap();
        assert!(body.contains("a &lt;b&gt; &amp; &quot;c&quot;"));
        assert!(!body.contains("<b>a <b>"));
    }

    #[test]
    fn the_marker_uses_the_shared_prefix() {
        assert!(MARKER.starts_with(&format!("<!-- {MARKER_PREFIX}")));
    }
}
