//! Context-aware Markdown rendering for untrusted review content.

/// Put untrusted text in a CommonMark code span without letting it close the span.
pub(crate) fn code_span(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let longest = clean.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    if clean.contains('`') {
        format!("{fence} {clean} {fence}")
    } else {
        format!("{fence}{clean}{fence}")
    }
}

/// Fence untrusted source text without letting it terminate the code block.
pub(crate) fn code_block(text: &str) -> String {
    let clean: String = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect();
    let longest = clean
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{}\n{fence}", clean.trim_matches('\n'))
}

/// Escape untrusted inline prose so it cannot create HTML, links, or mentions.
pub(crate) fn prose(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let first = characters
        .iter()
        .position(|character| !character.is_whitespace());
    let starts_with = |markers: &[char]| {
        first.is_some_and(|index| {
            index <= 3
                && markers.contains(&characters[index])
                && characters
                    .get(index + 1)
                    .is_some_and(|character| character.is_whitespace())
        })
    };
    let list_marker = starts_with(&['-', '+', '*']);
    let heading_marker = first.is_some_and(|index| {
        index <= 3
            && characters[index] == '#'
            && characters
                .get(index + 1)
                .is_some_and(|character| character.is_whitespace() || *character == '#')
    });
    let ordered_separator = first.and_then(|index| {
        let end = index
            + characters[index..]
                .iter()
                .take_while(|character| character.is_ascii_digit())
                .count();
        (end > index
            && end < characters.len()
            && matches!(characters[end], '.' | ')')
            && characters
                .get(end + 1)
                .is_some_and(|character| character.is_whitespace()))
        .then_some(end)
    });
    let mut in_url_token = vec![false; characters.len()];
    let mut cursor = 0;
    while cursor < characters.len() {
        if url_start(&characters, cursor) {
            let end = characters[cursor..]
                .iter()
                .position(|character| character.is_whitespace() || character.is_control())
                .map(|offset| cursor + offset)
                .unwrap_or(characters.len());
            in_url_token[cursor..end].fill(true);
            cursor = end;
        } else {
            cursor += 1;
        }
    }

    let mut out = String::with_capacity(text.len());
    for (index, character) in characters.iter().copied().enumerate() {
        if character.is_control() {
            if character == '\n' || character == '\r' {
                out.push(' ');
            }
            continue;
        }
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '@' => out.push_str("&#64;"),
            '`' => out.push_str("&#96;"),
            ':' if in_url_token[index] => out.push_str("&#58;"),
            '.' if in_url_token[index] => out.push_str("&#46;"),
            '\\' | '[' | ']' | '*' => {
                out.push('\\');
                out.push(character);
            }
            '_' if emphasis_boundary(&characters, index) => {
                out.push('\\');
                out.push(character);
            }
            '#' if heading_marker => {
                out.push('\\');
                out.push(character);
            }
            '#' if characters.get(index + 1).is_some_and(char::is_ascii_digit) => {
                out.push_str("&#35;");
            }
            '-' | '+' if list_marker => {
                out.push('\\');
                out.push(character);
            }
            '.' | ')' if ordered_separator == Some(index) => {
                out.push('\\');
                out.push(character);
            }
            '~' if characters.get(index + 1) == Some(&'~')
                || index
                    .checked_sub(1)
                    .and_then(|previous| characters.get(previous))
                    == Some(&'~') =>
            {
                out.push('\\');
                out.push(character);
            }
            _ => out.push(character),
        }
    }
    out
}

fn url_start(characters: &[char], index: usize) -> bool {
    let remaining: String = characters[index..].iter().take(8).collect();
    let lower = remaining.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("www.")
}

/// Escape prose for a Markdown table cell.
pub(crate) fn table_cell(text: &str) -> String {
    prose(text).replace('|', "\\|")
}

/// Render a code span for a table cell, where a raw pipe separates columns
/// even when it appears inside backticks.
pub(crate) fn table_code_span(text: &str) -> String {
    code_span(text).replace('|', "\\|")
}

fn emphasis_boundary(characters: &[char], index: usize) -> bool {
    let word = |character: char| character.is_alphanumeric();
    let left = index.checked_sub(1).and_then(|left| characters.get(left));
    let right = characters.get(index + 1);
    !left.is_some_and(|character| word(*character))
        || !right.is_some_and(|character| word(*character))
}

/// Render a citation as a pinned GitHub link, or as code spans for symbols.
pub(crate) fn citation(citation: &str, repo: &str, sha: &str) -> String {
    if !valid_repo(repo) || sha.is_empty() || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return code_span(citation);
    }
    if let Some((path, line)) = citation.rsplit_once(':')
        && !path.is_empty()
        && line.parse::<u32>().is_ok_and(|line| line > 0)
        && safe_path(path)
    {
        let encoded = path
            .split('/')
            .map(encode_path_segment)
            .collect::<Vec<_>>()
            .join("/");
        return format!(
            "[{}](https://github.com/{}/blob/{}/{encoded}#L{})",
            code_span(&format!("{path}:{line}")),
            repo,
            sha,
            line
        );
    }
    if let Some((path, symbol)) = citation.split_once('#') {
        return format!("{} {}", code_span(path), code_span(symbol));
    }
    code_span(citation)
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    parts.clone().count() == 2
        && parts.all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn safe_path(path: &str) -> bool {
    !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn encode_path_segment(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_spans_preserve_path_punctuation_and_choose_a_safe_fence() {
        assert_eq!(
            code_span("crates/tinyjuice-module/src/savings_store.rs"),
            "`crates/tinyjuice-module/src/savings_store.rs`"
        );
        assert_eq!(
            code_span("path`with``ticks.rs"),
            "``` path`with``ticks.rs ```"
        );
    }

    #[test]
    fn fenced_source_uses_a_fence_longer_than_untrusted_backticks() {
        assert_eq!(
            code_block("safe\n```\nunsafe"),
            "````\nsafe\n```\nunsafe\n````"
        );
    }

    #[test]
    fn prose_cannot_create_mentions_html_links_or_new_blocks() {
        let rendered = prose("@user <script>[click](https://evil.example)\n# heading");
        assert!(!rendered.contains("@user"));
        assert!(!rendered.contains("<script>"));
        assert!(!rendered.contains("[click]("));
        assert!(!rendered.contains("https://evil.example"));
        assert_eq!(prose("# heading"), "\\# heading");
        assert_eq!(prose("- item"), "\\- item");
        assert_eq!(prose("1. item"), "1\\. item");
        assert!(!prose("see #123").contains("#123"));
        assert!(!prose("~~strike~~").contains("~~"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn url_tokens_escape_markdown_delimiters_as_well_as_autolink_punctuation() {
        let rendered =
            prose("https://a.b/[click](https://evil.example) @maintainer `code` **bold** |\\path");
        assert!(!rendered.contains("[click]("), "{rendered}");
        assert!(!rendered.contains("@maintainer"), "{rendered}");
        assert!(!rendered.contains("`code`"), "{rendered}");
        assert!(!rendered.contains("**bold**"), "{rendered}");
        assert!(!rendered.contains("https://"), "{rendered}");
        let table_rendered = table_cell(
            "https://a.b/[click](https://evil.example) @maintainer `code` **bold** |\\path",
        );
        assert!(!table_rendered.contains(" | "), "{table_rendered}");
        assert!(rendered.contains("|\\\\path"), "{rendered}");
    }

    #[test]
    fn ordinary_punctuation_and_intra_word_underscores_remain_readable() {
        assert_eq!(
            prose("tiny-module savings_store.rs v1.2.3"),
            "tiny-module savings_store.rs v1.2.3"
        );
    }

    #[test]
    fn table_cells_escape_the_column_separator() {
        assert_eq!(
            table_cell("left | <b>right</b>"),
            "left \\| &lt;b&gt;right&lt;/b&gt;"
        );
        assert_eq!(table_code_span("src/a|b.rs"), "`src/a\\|b.rs`");
    }

    #[test]
    fn citations_link_lines_at_the_reviewed_commit_and_code_span_symbols() {
        assert_eq!(
            citation("src/savings/ledger.rs:42", "acme/widget", "abc123"),
            "[`src/savings/ledger.rs:42`](https://github.com/acme/widget/blob/abc123/src/savings/ledger.rs#L42)"
        );
        assert_eq!(
            citation(
                "src/savings/ledger.rs#impl SavingsLedger",
                "acme/widget",
                "abc123"
            ),
            "`src/savings/ledger.rs` `impl SavingsLedger`"
        );
        assert!(
            citation("src/arr[0].rs:7", "acme/widget", "abc123").contains("src/arr%5B0%5D.rs#L7")
        );
        assert_eq!(
            citation("src/arr[`0`].rs:7", "acme/widget", "abc123"),
            "[`` src/arr[`0`].rs:7 ``](https://github.com/acme/widget/blob/abc123/src/arr%5B%600%60%5D.rs#L7)"
        );
    }
}
