//! Display-width-aware text wrapping shared by the chat renderer and
//! the input editor. Pure functions only: no terminal or widget types.
//!
//! `wrap_spans` is the core primitive: it runs the exact greedy
//! word-wrap algorithm (oversized words are hard-broken) but records
//! byte-offset spans instead of allocating strings, so the editor can
//! map cursor bytes to visual rows. `wrap_text` renders those spans.

/// Byte-offset spans `(start, end)` of each visual row of `text`
/// wrapped at `width` display columns. Always returns at least one
/// span; offsets are always grapheme (hence `char`) boundaries, so
/// multi-codepoint sequences such as `⬇️` (`U+2B07 U+FE0F`) are never
/// split across rows.
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
                // Hard-break the oversized word across lines, one
                // grapheme cluster at a time. Per-`char` measuring
                // undercounts sequences such as `⬇️` (`U+2B07` is 1,
                // `U+FE0F` is 0, the cluster renders as 2), packing one
                // cell too many per row and spilling into the right
                // margin; it also splits the base char off its
                // variation selector. Clusters are never split.
                if let Some(start) = line_start.take() {
                    out.push((base + start, base + line_end));
                }
                let mut chunk_start = word_start;
                let mut chunk_width = 0usize;
                for (i, g) in grapheme_clusters(word) {
                    let gw = grapheme_width(g);
                    if chunk_width + gw > width && chunk_width > 0 {
                        out.push((base + chunk_start, base + word_start + i));
                        chunk_start = word_start + i;
                        chunk_width = 0;
                    }
                    // A lone grapheme wider than `width` still takes its
                    // own row (the renderer truncates what cannot fit)
                    // instead of emitting an empty span and stalling.
                    if gw > width && chunk_width == 0 {
                        out.push((base + word_start + i, base + word_start + i + g.len()));
                        chunk_start = word_start + i + g.len();
                        continue;
                    }
                    chunk_width += gw;
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
/// wide (CJK) chars, 1 otherwise. Prefer [`grapheme_width`] /
/// [`display_width`] for measuring rendered text: per-`char` widths
/// undercount sequences such as `⬇️` (`U+2B07 U+FE0F`, chars sum to
/// 1, the cluster renders as 2).
pub fn ch_width(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Width of one extended grapheme cluster as ratatui renders it:
/// `UnicodeWidthStr` over the whole cluster (so VS16/ZWJ/keycap
/// sequences measure as the ligature, not the sum of their parts),
/// plus one cell per halfwidth katakana voiced/semi-voiced sound
/// mark (`U+FF9E`/`U+FF9F`), matching
/// `ratatui_core::buffer::CellWidth`. Clusters containing control
/// characters measure 0: ratatui's `styled_graphemes` filters them
/// out before rendering.
pub fn grapheme_width(g: &str) -> usize {
    if g.chars().any(|c| c.is_control()) {
        return 0;
    }
    let w = unicode_width::UnicodeWidthStr::width(g);
    w + g
        .chars()
        .filter(|c| *c == '\u{FF9E}' || *c == '\u{FF9F}')
        .count()
}

/// Ordered `(byte_offset, grapheme cluster)` pairs of `s`, using
/// extended grapheme segmentation — the same clusters ratatui
/// renders as one cell run.
pub fn grapheme_clusters(s: &str) -> Vec<(usize, &str)> {
    use unicode_segmentation::UnicodeSegmentation;
    UnicodeSegmentation::grapheme_indices(s, true).collect()
}

/// Display width of a string in terminal columns, measured per
/// grapheme cluster exactly as ratatui's `CellWidth` measures it.
/// In particular `display_width("⬇️") == 2`, not 1.
pub fn display_width(s: &str) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    UnicodeSegmentation::graphemes(s, true)
        .map(grapheme_width)
        .sum()
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

    #[test]
    fn display_width_counts_vs16_emoji_as_two() {
        // "⬇️" is U+2B07 + U+FE0F: per-char widths sum to 1, but the
        // cluster renders as 2 (ratatui's CellWidth agrees). The old
        // per-char measuring spilled one column into the right margin.
        assert_eq!(display_width("⬇️"), 2);
        assert_eq!(grapheme_width("⬇️"), 2);
        assert_eq!(display_width("🎨"), 2);
        assert_eq!(display_width("ｶﾞ"), 2);
    }

    #[test]
    fn wrap_never_splits_grapheme_or_exceeds_width() {
        // The toolcall overflow: an over-long word containing `⬇️`
        // packed one cell too many per row (measuring the cluster as
        // 1), spilling into the margin.
        assert_eq!(
            wrap_text("id=\"dlBtn\">⬇️", 12),
            vec!["id=\"dlBtn\">", "⬇️"]
        );
        for width in [1usize, 2, 4, 6, 12, 22, 62] {
            for text in [
                "id=\"dlBtn\">⬇️ end",
                "🎨✨⬇️🌊 done 〰️ tail",
                "ｶﾞｷﾞｸﾞ done",
                "→ shell_exec ({\"command\":\"cat ⬇️🎨\"})",
            ] {
                for row in wrap_text(text, width) {
                    // A single grapheme wider than the column (e.g. ⬇️
                    // at width 1) cannot fit unsplit; the renderer
                    // truncates it. Everything else must fit.
                    let single = grapheme_clusters(&row).len() == 1;
                    assert!(
                        display_width(&row) <= width || single,
                        "row {row:?} exceeds {width} in {text:?}"
                    );
                }
                // No span boundary may split a VS16 cluster.
                for (a, b) in wrap_spans(text, width) {
                    let raw = &text.as_bytes()[a..b];
                    assert!(
                        !raw.ends_with("⬇".as_bytes()) && !raw.ends_with("〰".as_bytes()),
                        "split VS16 cluster in {text:?} at {a}..{b}"
                    );
                }
            }
        }
    }

    #[test]
    fn wrap_breaks_before_emoji_at_column_width() {
        // 62-col text column: a long no-space word containing emoji
        // must hard-break so no row exceeds 62; previously one row
        // measured 62 but rendered 63, spilling into the margin.
        let word = format!("{}⬇️{}〰️{}", "x".repeat(40), "y".repeat(40), "z".repeat(40));
        let text = format!("→ shell_exec ({word})");
        let rows = wrap_text(&text, 62);
        assert!(rows.len() > 1);
        for row in &rows {
            assert!(display_width(row) <= 62, "row exceeds 62: {row:?}");
        }
        // The emoji stays glued to its variation selector: no row
        // ends with a bare base char split off from its selector.
        for row in &rows {
            assert!(
                !row.ends_with('⬇') && !row.ends_with('〰'),
                "split cluster: {row:?}"
            );
        }
        // No content lost: rows rejoin to the source modulo spaces.
        let joined: String = rows.join("");
        let keep = |s: &str| s.chars().filter(|c| *c != ' ').collect::<String>();
        assert_eq!(keep(&joined), keep(&text));
    }
}
