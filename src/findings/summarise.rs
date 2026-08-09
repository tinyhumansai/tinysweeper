//! Shortening model-authored text for the places that have a length limit.
//!
//! A check-run title is capped by GitHub, and a finding title is capped by our
//! own schema, so both need a truncation that leaves something readable rather
//! than a hard cut mid-word.

/// Shorten `text` to at most `limit` characters, ending on a word boundary.
///
/// Used for check-run titles, where GitHub imposes its own ceiling and a title
/// cut mid-word reads worse than one that stops early.
pub fn shorten(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }

    // Character boundaries, not byte offsets: `&text[..limit]` panics the
    // moment a multibyte character straddles the cut, and model-authored text
    // is full of them — an em dash in a title would take the process down.
    let head: String = text.chars().take(limit).collect();
    match head.rfind(' ') {
        Some(space) => format!("{}…", &head[..space]),
        None => format!("{head}…"),
    }
}

/// The first sentence of `text`, for a one-line summary.
pub fn first_sentence(text: &str) -> String {
    let text = text.trim();
    match text.find(". ") {
        Some(end) => text[..end + 1].to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_string_is_returned_unchanged() {
        assert_eq!(shorten("Guard the index", 80), "Guard the index");
    }

    #[test]
    fn a_long_string_stops_on_a_word_boundary() {
        let out = shorten("Guard the index before dereferencing the slice", 20);
        assert!(out.ends_with('…'), "{out}");
        assert!(out.len() <= 21, "{out}");
    }

    #[test]
    fn a_multibyte_title_does_not_panic() {
        // The bug this replaces: byte slicing at `limit` splits a multibyte
        // character and panics. Every one of these is longer in bytes than in
        // characters, so a byte-based cut lands mid-character.
        for text in ["Guard the índex before dereferencing —— twice", "日本語のタイトルはとても長いのです", "café ".repeat(40).as_str()] {
            let out = shorten(text, 20);
            assert!(out.chars().count() <= 21, "{out}");
        }
    }

    #[test]
    fn the_first_sentence_is_extracted() {
        assert_eq!(
            first_sentence("This is wrong. And here is why."),
            "This is wrong."
        );
    }
}
