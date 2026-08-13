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
//! **A long line is not cut at the target.** A minified bundle would otherwise
//! be sliced mid-token into spans that mean nothing. The chunk goes over the
//! target instead, and the file-size cap in
//! [`select`](crate::chunk::select) is what keeps that bounded.
//!
//! It *is* cut at `max_embed_bytes`, and that is a different question. The
//! target is a preference about retrieval quality, so a long line is allowed
//! to overrun it; the embed ceiling is the provider's own per-input limit, and
//! a chunk past it fails the whole call. A file whose lines run to tens of
//! kilobytes — generated code, an embedded literal — left the largest
//! repository in the fleet unindexed for exactly this reason. Slicing such a
//! line mid-token is a poor chunk; refusing to slice it means no index at all.

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
    // The line of the last piece actually appended. Tracked rather than
    // derived as `line_number - 1`, because a flush can now happen part-way
    // *through* a line, and then the buffer ends on the current line rather
    // than the previous one.
    let mut buffer_end = first_line;

    for (line_number, line) in (first_line..).zip(text.split_inclusive('\n')) {
        // One line can exceed every ceiling on its own — a minified bundle, a
        // generated table, a long embedded literal. The loop below only ever
        // flushes *before* appending, so without this such a line is never
        // broken and becomes a chunk the provider rejects. Splitting it is the
        // only option that keeps the content: dropping it loses the file.
        for piece in pieces(line, options.max_embed_bytes) {
            // A blank line is the only structural hint available without a
            // grammar, so a chunk that is already substantial ends at one
            // rather than at an arbitrary character count part-way through a
            // paragraph of code.
            let blank = piece.trim().is_empty();
            // A buffer holding only blank lines is never flushed: it would be
            // dropped as whitespace and those lines would vanish from the file
            // the chunks reconstruct. It keeps accumulating instead, and the
            // blank run ends up at the head of the next real chunk.
            let flushable = !buffer.trim().is_empty();
            // Whichever ceiling is lower binds. Splitting the *line* is not
            // enough on its own: the buffer goes on accumulating pieces until
            // it reaches `target_chars`, so without the hard ceiling here a
            // run of pieces reassembles into exactly the oversized chunk the
            // split was meant to prevent.
            let ceiling = options.target_chars.min(options.max_embed_bytes);
            let would_exceed = flushable && buffer.len() + piece.len() > ceiling;
            let at_a_seam = flushable && blank && buffer.len() >= ceiling / 2;

            if would_exceed || at_a_seam {
                push(&mut chunks, &mut buffer, buffer_start, buffer_end);
                buffer_start = line_number;
            }

            buffer.push_str(piece);
            buffer_end = line_number;
        }
    }

    push(&mut chunks, &mut buffer, buffer_start, buffer_end);
    chunks
}

/// `line` broken into pieces of at most `ceiling` bytes, on char boundaries.
///
/// Almost always yields the line itself — the split path exists for the
/// pathological line, not the ordinary one. Cuts walk backwards to a char
/// boundary so a multi-byte character is never sliced in half, which would
/// produce invalid UTF-8 and panic on `split_at`.
fn pieces(line: &str, ceiling: usize) -> Vec<&str> {
    let ceiling = ceiling.max(1);
    if line.len() <= ceiling {
        return vec![line];
    }

    let mut pieces = Vec::new();
    let mut rest = line;
    while rest.len() > ceiling {
        let mut cut = ceiling;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            // A single character wider than the ceiling. Emitting the
            // remainder whole beats looping forever; the provider may reject
            // it, but a ceiling set below one character is a misconfiguration,
            // not a case to design around.
            break;
        }
        let (head, tail) = rest.split_at(cut);
        pieces.push(head);
        rest = tail;
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
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

    // --- the per-input ceiling -------------------------------------------
    //
    // These guard the provider's own limit rather than a preference. A chunk
    // over it fails the whole embedding call, which leaves a repository
    // unindexed and its reviews silently diff-only.

    /// The longest chunk `split` produced, in bytes.
    fn widest(chunks: &[SourceChunk]) -> usize {
        chunks
            .iter()
            .map(|chunk| chunk.text.len())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn one_enormous_line_is_split_rather_than_emitted_whole() {
        // The production failure. `openhuman` carries lines like this —
        // generated code and embedded literals — and the flush-before-append
        // loop never broke them, so one chunk arrived at the provider over its
        // 8,192-token per-input limit and failed the entire batch with
        // `Invalid 'input[19]': maximum input length is 8192 tokens`.
        let mut options = options(1_800);
        options.max_embed_bytes = 500;
        let source = format!("const DATA = \"{}\";\n", "a".repeat(40_000));

        let chunks = split(&source, 1, &options);
        assert!(chunks.len() > 1, "an oversized line must be broken up");
        assert!(
            widest(&chunks) <= 500,
            "a chunk of {} bytes still exceeds the ceiling",
            widest(&chunks)
        );
    }

    #[test]
    fn splitting_a_long_line_keeps_every_byte() {
        // Splitting must not become dropping. A lost span is a file that is
        // silently unsearchable, which is worse than the rejected call.
        let mut options = options(1_800);
        options.max_embed_bytes = 300;
        let body = "x".repeat(5_000);
        let source = format!("{body}\n");

        let rejoined: String = split(&source, 1, &options)
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect();
        assert_eq!(rejoined, source);
    }

    #[test]
    fn a_multi_byte_character_is_never_cut_in_half() {
        // `split_at` panics on a non-boundary index, so a naive ceiling-sized
        // cut would crash the indexer on any file with non-ASCII content.
        let mut options = options(1_800);
        options.max_embed_bytes = 10;
        // Three bytes each, so a 10-byte cut lands mid-character.
        let source = format!("{}\n", "€".repeat(200));

        let chunks = split(&source, 1, &options);
        let rejoined: String = chunks.iter().map(|c| c.text.clone()).collect();
        assert_eq!(rejoined, source, "no byte may be lost or corrupted");
        assert!(widest(&chunks) <= 12, "a cut may fall short, never over");
    }

    #[test]
    fn ordinary_lines_are_untouched_by_the_ceiling() {
        // The split path is for the pathological line. Normal source must
        // chunk exactly as it did before the ceiling existed.
        let options = options(1_800);
        let source = "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n";
        let chunks = split(source, 1, &options);
        let rejoined: String = chunks.iter().map(|c| c.text.clone()).collect();
        assert_eq!(rejoined, source);
        assert!(widest(&chunks) < 1_800);
    }

    #[test]
    fn line_numbers_survive_a_mid_line_split() {
        // A flush can now land part-way through a line, so the end line is
        // tracked rather than assumed to be the previous one. Getting this
        // wrong points a review comment at the wrong line.
        let mut options = options(200);
        options.max_embed_bytes = 100;
        let source = format!("short\n{}\ntail\n", "y".repeat(900));

        let chunks = split(&source, 1, &options);
        for chunk in &chunks {
            assert!(
                chunk.start_line <= chunk.end_line,
                "{}..{} is backwards",
                chunk.start_line,
                chunk.end_line
            );
            assert!(
                chunk.end_line <= 3,
                "line {} is past the source",
                chunk.end_line
            );
        }
    }
}
