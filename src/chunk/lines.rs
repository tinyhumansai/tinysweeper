//! The fallback splitter: line boundaries, no grammar.
//!
//! Always compiled. Used for files no grammar claims, and for the rare
//! definition too large to keep whole. Every chunk it produces is marked
//! [`ChunkMethod::Lines`](crate::index::types::ChunkMethod::Lines), which is the
//! honest part: these spans may well begin halfway through a function.
//!
//! Two decisions worth stating.
//!
//! **No overlap.** Repeating the last few lines of one chunk at the head of the
//! next is a cheap imitation of context, and it costs real money — every
//! duplicated line is embedded twice and stored twice — while making dedupe
//! harder, because two chunks now legitimately share text. The grammar path
//! makes overlap unnecessary; on this path it would only paper over the split.
//!
//! **A long line is never cut.** A minified bundle would otherwise be sliced
//! mid-token into spans that mean nothing. The chunk goes over the target
//! instead, and the file-size cap in [`select`](crate::chunk::select) is what
//! keeps that bounded.

use crate::chunk::types::{ChunkOptions, SourceChunk};
use crate::index::types::ChunkMethod;

/// Split `text` on line boundaries.
///
/// `first_line` is the 1-based line number of `text`'s first line, so this can
/// split a fragment of a file — an oversized function body, say — and still
/// report line numbers in the file's own coordinates.
pub fn split(text: &str, first_line: u32, options: &ChunkOptions) -> Vec<SourceChunk> {
    let mut chunks = Vec::new();
    let mut buffer = String::new();
    let mut buffer_start = first_line;
    let mut line_number = first_line;

    for line in text.split_inclusive('\n') {
        // A blank line is the only structural hint available without a grammar,
        // so a chunk that is already substantial ends at one rather than at an
        // arbitrary character count part-way through a paragraph of code.
        let blank = line.trim().is_empty();
        let would_exceed = !buffer.is_empty() && buffer.len() + line.len() > options.target_chars;
        let at_a_seam = blank && buffer.len() >= options.target_chars / 2;

        if would_exceed || at_a_seam {
            push(&mut chunks, &mut buffer, buffer_start, line_number - 1);
            buffer_start = line_number;
        }

        buffer.push_str(line);
        line_number += 1;
    }

    push(&mut chunks, &mut buffer, buffer_start, line_number - 1);
    chunks
}

fn push(chunks: &mut Vec<SourceChunk>, buffer: &mut String, start_line: u32, end_line: u32) {
    if buffer.trim().is_empty() {
        // Whitespace-only spans are dropped rather than emitted: embedding them
        // costs money and they can never be the right answer to a query.
        buffer.clear();
        return;
    }
    chunks.push(SourceChunk {
        start_line,
        end_line: end_line.max(start_line),
        text: std::mem::take(buffer),
        symbol: None,
        method: ChunkMethod::Lines,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(target: usize) -> ChunkOptions {
        ChunkOptions::with_target(target)
    }

    #[test]
    fn line_numbers_are_contiguous_and_cover_the_input() {
        let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let chunks = split(&text, 1, &options(120));

        assert!(chunks.len() > 1, "the input should not fit in one chunk");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks.last().expect("a chunk").end_line, 200);
        for pair in chunks.windows(2) {
            assert_eq!(
                pair[1].start_line,
                pair[0].end_line + 1,
                "chunks must tile the file without gaps or overlap"
            );
        }
    }

    #[test]
    fn the_concatenated_chunks_reproduce_the_input() {
        // A splitter that loses or duplicates a line is worse than no index at
        // all, because the loss is invisible at query time.
        let text = "alpha\nbeta\n\ngamma\ndelta\n\nepsilon\n";
        let rejoined: String = split(text, 1, &options(8))
            .iter()
            .map(|c| c.text.clone())
            .collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn a_line_longer_than_the_target_is_not_cut() {
        let long = "x".repeat(5_000);
        let chunks = split(&format!("{long}\n"), 1, &options(100));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text.trim_end(), long);
    }

    #[test]
    fn a_fragment_reports_line_numbers_in_the_files_coordinates() {
        let chunks = split("a\nb\n", 41, &options(1_000));
        assert_eq!(chunks[0].start_line, 41);
        assert_eq!(chunks[0].end_line, 42);
    }

    #[test]
    fn whitespace_only_input_produces_nothing() {
        assert!(split("\n\n   \n", 1, &options(10)).is_empty());
    }

    #[test]
    fn every_fallback_chunk_admits_it_was_cut_on_lines() {
        let text: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        for chunk in split(&text, 1, &options(60)) {
            assert_eq!(chunk.method, ChunkMethod::Lines);
            assert_eq!(chunk.symbol, None);
        }
    }
}
