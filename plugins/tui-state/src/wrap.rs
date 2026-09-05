//! Display-width-aware text wrapping shared by the chat renderer and
//! the input editor. Pure functions only: no terminal or widget types.
//!
//! `wrap_spans` is the core primitive: it runs the exact greedy
//! word-wrap algorithm (oversized words are hard-broken) but records
//! byte-offset spans instead of allocating strings, so the editor can
//! map cursor bytes to visual rows. `wrap_text` renders those spans.

/// Byte-offset spans `(start, end)` of each visual row of `text`
/// wrapped at `width` display columns. Always returns at least one
/// span; offsets are always `char` boundaries.
pub fn wrap_spans(text: &str, width: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if width == 0 {
        let mut off = 0;
        for line in text.split('\n') {
            out.push((off, off + line.len()));
            off += line.len() + 1;
        }
        return out;
    }
    // Byte offset of the current paragraph within `text`.
    let mut base = 0usize;
    for para in text.split('\n') {
        // Start/end (paragraph-relative) of the first/last word on the
        // open visual line; `None` while the line holds no words yet.
        let mut line_start: Option<usize> = None;
        let mut line_end = 0usize;
        let mut line_width = 0usize;
        // Byte offset of the current word within the paragraph.
        let mut off = 0usize;
        for word in para.split(' ') {
            let word_start = off;
            let word_end = off + word.len();
            off = word_end + 1; // skip the single-byte separator
            let word_width = display_width(word);
            let gap = if line_start.is_none() { 0 } else { 1 };
            if line_width + gap + word_width <= width {
                if line_start.is_none() {
                    line_start = Some(word_start);
                }
                line_end = word_end;
                line_width += gap + word_width;
            } else if word_width > width {
                // Hard-break the oversized word across lines.
                if let Some(start) = line_start.take() {
                    out.push((base + start, base + line_end));
                }
                let mut chunk_start = word_start;
                let mut chunk_width = 0usize;
                for (i, ch) in word.char_indices() {
                    let cw = ch_width(ch);
                    if chunk_width + cw > width {
                        out.push((base + chunk_start, base + word_start + i));
                        chunk_start = word_start + i;
                        chunk_width = 0;
                    }
                    chunk_width += cw;
                }
                line_start = Some(chunk_start);
                line_end = word_end;
                line_width = chunk_width;
            } else {
                if let Some(start) = line_start.take() {
                    out.push((base + start, base + line_end));
                }
                line_start = Some(word_start);
                line_end = word_end;
                line_width = word_width;
            }
        }
        match line_start {
            Some(start) => out.push((base + start, base + line_end)),
            // Unreachable: every paragraph yields at least one word,
            // but keep the invariant of one span per paragraph.
            None => out.push((base, base)),
        }
        base += para.len() + 1; // skip the single-byte newline
    }
    if out.is_empty() {
        out.push((0, 0));
    }
    out
}

/// Greedy word wrap on display-width boundaries; oversized words are
/// hard-broken. Paragraph breaks (`\n`) are preserved.
pub fn wrap_text(s: &str, width: usize) -> Vec<String> {
    wrap_spans(s, width)
        .into_iter()
        .map(|(a, b)| s[a..b].to_owned())
        .collect()
}

/// Display width of one character: 0 for control chars, 2 for
/// wide (CJK) chars, 1 otherwise.
pub fn ch_width(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Display width of a string in terminal columns.
pub fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_slice_back_to_wrapped_text() {
        for (text, width) in [
            ("", 10),
            ("hello", 20),
            ("hello world", 20),
            ("aaa bbb ccc", 7),
            ("aaa bbb ccc", 3),
            ("abcdefghij", 4),
            ("x abcdefghij", 4),
            ("a  b", 10),
            ("a  b", 2),
            ("a\nb", 10),
            ("a\n\nb", 10),
            ("éé éé", 5),
            ("éé éé", 4),
            ("日本語テスト\nsecond line here", 4),
            ("trailing space ", 20),
            ("trailing space ", 5),
        ] {
            let spans = wrap_spans(text, width);
            assert!(!spans.is_empty(), "spans for {text:?}");
            for (i, &(a, b)) in spans.iter().enumerate() {
                assert!(text.is_char_boundary(a), "start {a} in {text:?}");
                assert!(text.is_char_boundary(b), "end {b} in {text:?}");
                assert!(a <= b, "span {i} inverted in {text:?}");
                if i > 0 {
                    assert!(spans[i - 1].0 <= a, "spans ordered in {text:?}");
                }
            }
            let rendered: Vec<String> = spans
                .into_iter()
                .map(|(a, b)| text[a..b].to_owned())
                .collect();
            assert_eq!(rendered, wrap_text(text, width), "text {text:?}");
        }
    }

    #[test]
    fn wrap_text_short_line_unchanged() {
        assert_eq!(wrap_text("hello", 20), vec!["hello"]);
        assert_eq!(wrap_text("hello world", 20), vec!["hello world"]);
    }

    #[test]
    fn wrap_text_breaks_on_spaces() {
        assert_eq!(wrap_text("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap_text("aaa bbb ccc", 3), vec!["aaa", "bbb", "ccc"]);
    }

    #[test]
    fn wrap_text_hard_breaks_long_words() {
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(
            wrap_text("x abcdefghij", 4),
            vec!["x", "abcd", "efgh", "ij"]
        );
    }

    #[test]
    fn wrap_text_collapses_multiple_spaces() {
        // split(' ') yields empty words that rejoin without trailing gap.
        assert_eq!(wrap_text("a  b", 10), vec!["a  b"]);
        assert_eq!(wrap_text("a  b", 2), vec!["a ", "b"]);
    }

    #[test]
    fn wrap_text_preserves_paragraphs() {
        assert_eq!(wrap_text("a\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap_text("", 10), vec![""]);
    }

    #[test]
    fn wrap_text_uses_display_width() {
        // "éé éé" is 5 display columns: fits at 5, wraps at 4 and below.
        assert_eq!(wrap_text("éé éé", 5), vec!["éé éé"]);
        assert_eq!(wrap_text("éé éé", 4), vec!["éé", "éé"]);
        assert_eq!(wrap_text("éé éé", 2), vec!["éé", "éé"]);
    }
}
