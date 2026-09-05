//! Markdown [`MessageRenderer`](harness_tui_state::render::MessageRenderer)
//! for assistant messages, via mdfrier with syntect code highlighting.
//!
//! mdfrier already wraps to the given width, so callers must not re-wrap
//! its output. On any parser failure we fall back to plain wrapping so a
//! broken message never blanks the transcript.
//!
//! Code is rendered with real syntax highlighting and no background fill:
//! fenced blocks are highlighted per their fence language, inline code
//! renders as plain assistant text.
//!
//! NOTE: this crate depends on mdfrier, which is GPL-3.0-or-later. Keep the
//! markdown engine behind the renderer trait so downstream crates never
//! absorb that dependency transitively.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, Once, OnceLock};

use harness_contracts::KEY_MARKDOWN_RENDERER;
use harness_core::{Context, Result};
use harness_tui_state::render::{MessageRenderer, RendererHandle};
use ratatui::{
    style::{Color as RatColor, Modifier as RatModifier, Style},
    text::{Line, Span as RatSpan},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
};

thread_local! {
    static FRIER: RefCell<Option<mdfrier::MdFrier>> = const { RefCell::new(None) };
}

/// Cache of fully rendered assistant messages keyed by content hash and
/// wrap width. The view re-renders every item on every draw (each scroll
/// step, each stream event, each 250ms tick), while parsing (tree-sitter)
/// plus syntax highlighting costs tens of milliseconds per message — that
/// is the scroll stutter. Chat items are immutable once pushed, so a cache
/// hit (hash + clone) is always valid. Bounded; cleared on overflow.
type RenderCache = Mutex<HashMap<(u64, u16), Vec<Line<'static>>>>;

static RENDER_CACHE: OnceLock<RenderCache> = OnceLock::new();

fn render_cache() -> &'static RenderCache {
    RENDER_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Maximum cached messages before the cache is reset.
const CACHE_CAP: usize = 256;

fn cache_key(text: &str, width: u16) -> (u64, u16) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    (hasher.finish(), width)
}

static WARN_INIT: Once = Once::new();
static WARN_PARSE: Once = Once::new();

// ============================================================================
// Theme: StyledMapper symbols, but no code background
// ============================================================================

/// mdfrier theme matching [`mdfrier::ratatui::DefaultTheme`] except code
/// spans carry no style of their own: fenced blocks are syntax-highlighted
/// separately below, and inline code falls back to the assistant base style
/// (patched on by the caller).
#[derive(Debug, Clone, Copy, Default)]
struct TuiTheme;

const STYLED: mdfrier::StyledMapper = mdfrier::StyledMapper;

impl mdfrier::Mapper for TuiTheme {
    fn link_desc_open(&self) -> &str {
        STYLED.link_desc_open()
    }
    fn link_desc_close(&self) -> &str {
        STYLED.link_desc_close()
    }
    fn link_url_open(&self) -> &str {
        STYLED.link_url_open()
    }
    fn link_url_close(&self) -> &str {
        STYLED.link_url_close()
    }
    fn blockquote_bar(&self) -> &str {
        STYLED.blockquote_bar()
    }
    fn horizontal_rule_char(&self) -> &str {
        STYLED.horizontal_rule_char()
    }
    fn task_checked(&self) -> &str {
        STYLED.task_checked()
    }
    fn table_vertical(&self) -> &str {
        STYLED.table_vertical()
    }
    fn table_horizontal(&self) -> &str {
        STYLED.table_horizontal()
    }
    fn table_top_left(&self) -> &str {
        STYLED.table_top_left()
    }
    fn table_top_right(&self) -> &str {
        STYLED.table_top_right()
    }
    fn table_bottom_left(&self) -> &str {
        STYLED.table_bottom_left()
    }
    fn table_bottom_right(&self) -> &str {
        STYLED.table_bottom_right()
    }
    fn table_top_junction(&self) -> &str {
        STYLED.table_top_junction()
    }
    fn table_bottom_junction(&self) -> &str {
        STYLED.table_bottom_junction()
    }
    fn table_left_junction(&self) -> &str {
        STYLED.table_left_junction()
    }
    fn table_right_junction(&self) -> &str {
        STYLED.table_right_junction()
    }
    fn table_cross(&self) -> &str {
        STYLED.table_cross()
    }
    fn emphasis_open(&self) -> &str {
        STYLED.emphasis_open()
    }
    fn emphasis_close(&self) -> &str {
        STYLED.emphasis_close()
    }
    fn strong_open(&self) -> &str {
        STYLED.strong_open()
    }
    fn strong_close(&self) -> &str {
        STYLED.strong_close()
    }
    fn code_open(&self) -> &str {
        STYLED.code_open()
    }
    fn code_close(&self) -> &str {
        STYLED.code_close()
    }
    fn strikethrough_open(&self) -> &str {
        STYLED.strikethrough_open()
    }
    fn strikethrough_close(&self) -> &str {
        STYLED.strikethrough_close()
    }
}

impl mdfrier::ratatui::Theme for TuiTheme {
    /// No code background or foreground: highlighted blocks supply their
    /// own token colors, inline code inherits the caller base style.
    fn code_style(&self) -> Style {
        Style::default()
    }
}

// ============================================================================
// Syntax highlighting assets (loaded once, shared)
// ============================================================================

struct HlAssets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

static HL_ASSETS: OnceLock<HlAssets> = OnceLock::new();

fn hl_assets() -> &'static HlAssets {
    HL_ASSETS.get_or_init(|| HlAssets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        themes: ThemeSet::load_defaults(),
    })
}

/// syntect theme for code. Dark, readable, foreground-only use (the
/// background is always dropped so code never gets a grey fill).
const HL_THEME: &str = "base16-ocean.dark";

/// syntect foreground + font style mapped to ratatui. The background is
/// deliberately dropped: code renders on the terminal background.
fn syn_style(s: syntect::highlighting::Style) -> Style {
    let fg = s.foreground;
    let mut style = Style::default().fg(RatColor::Rgb(fg.r, fg.g, fg.b));
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(RatModifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(RatModifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(RatModifier::UNDERLINED);
    }
    style
}

/// Highlight one source line, returning owned `(style, text)` segments.
/// `hl` carries the block state across lines (multi-line tokens work);
/// unknown languages fall back to plain unstyled text.
fn highlight_segments(
    hl: &mut HighlightLines,
    syntaxes: &SyntaxSet,
    code: &str,
) -> Vec<(Style, String)> {
    // syntect expects newline-terminated lines; strip the one we add.
    let mut with_nl = String::with_capacity(code.len() + 1);
    with_nl.push_str(code);
    with_nl.push('\n');
    let ranges = match hl.highlight_line(&with_nl, syntaxes) {
        Ok(r) => r,
        Err(_) => return vec![(Style::default(), code.to_owned())],
    };
    let mut segs: Vec<(Style, String)> = ranges
        .into_iter()
        .map(|(s, t)| (syn_style(s), t.to_owned()))
        .collect();
    if let Some((_, last)) = segs.last_mut()
        && last.ends_with('\n')
    {
        last.pop();
    }
    segs.retain(|(_, t)| !t.is_empty());
    segs
}

/// Wrap pre-styled segments to `width` display columns, slicing token
/// runs at the same wrap boundaries as plain text.
fn wrap_code_line(segments: &[(Style, String)], width: usize) -> Vec<Line<'static>> {
    let plain: String = segments.iter().map(|(_, t)| t.as_str()).collect();
    let mut runs: Vec<(usize, usize, Style)> = Vec::with_capacity(segments.len());
    let mut off = 0;
    for (style, text) in segments {
        let end = off + text.len();
        runs.push((off, end, *style));
        off = end;
    }
    harness_tui_state::wrap::wrap_spans(&plain, width.max(1))
        .into_iter()
        .map(|(a, b)| {
            let spans: Vec<RatSpan> = runs
                .iter()
                .filter_map(|&(rs, re, style)| {
                    let (s, e) = (rs.max(a), re.min(b));
                    if s < e {
                        Some(RatSpan::styled(plain[s..e].to_owned(), style))
                    } else {
                        None
                    }
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

/// Render a run of consecutive fenced-code lines with syntax highlighting.
/// One highlighter per fence language carries multi-line token state;
/// blank lines keep their row so the block keeps its shape.
fn flush_code(
    group: &mut Vec<(String, mdfrier::Line)>,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    if group.is_empty() {
        return;
    }
    let assets = hl_assets();
    let theme = assets
        .themes
        .themes
        .get(HL_THEME)
        .or_else(|| assets.themes.themes.values().next())
        .expect("syntect default themes");
    let mut hl: Option<HighlightLines> = None;
    let mut lang = String::new();
    let mut started = false;
    for (language, md_line) in group.drain(..) {
        if !started || language != lang {
            let syntax = assets
                .syntaxes
                .find_syntax_by_token(&language)
                .unwrap_or_else(|| assets.syntaxes.find_syntax_plain_text());
            hl = Some(HighlightLines::new(syntax, theme));
            lang = language;
            started = true;
        }
        // Join the raw code text, dropping mdfrier's trailing bg padding
        // (spaces added to fill the row). Leading indentation is kept.
        let raw: String = md_line.spans.iter().map(|s| s.content.as_str()).collect();
        let code = raw.trim_end_matches([' ', '\t']);
        let hl = hl.as_mut().expect("highlighter initialised above");
        let segments = highlight_segments(hl, &assets.syntaxes, code);
        out.extend(wrap_code_line(&segments, width));
    }
}

/// Default [`MessageRenderer`] for assistant messages: mdfrier markdown
/// with syntect code highlighting and no background fill.
pub struct MdfrierRenderer {
    base: Style,
}

impl MdfrierRenderer {
    pub fn new(base: Style) -> Self {
        MdfrierRenderer { base }
    }
}

impl Default for MdfrierRenderer {
    fn default() -> Self {
        MdfrierRenderer::new(harness_tui_state::render::ASSISTANT_BASE)
    }
}

impl MessageRenderer for MdfrierRenderer {
    fn render_assistant(&self, text: &str, width: u16) -> Vec<Line<'static>> {
        render_assistant(text, width, self.base)
    }
}

/// Plugin providing the shared markdown renderer as
/// `Arc<RendererHandle>` under [`KEY_MARKDOWN_RENDERER`].
/// A different plugin can provide the same key to swap engines without
/// touching the TUI shell.
pub struct MarkdownPlugin;

impl harness_core::Plugin for MarkdownPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("markdown").provides(KEY_MARKDOWN_RENDERER)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        ctx.provide_key(
            KEY_MARKDOWN_RENDERER,
            Arc::new(RendererHandle(Arc::new(MdfrierRenderer::default()))),
        );
        Ok(())
    }
}

/// Render markdown `text` to ratatui lines already wrapped to `width`.
///
/// `base` is patched underneath every non-code span, so unstyled text keeps
/// the assistant color while markdown styles (emphasis, links, headings, …)
/// win where mdfrier sets them. Code blocks carry syntect token colors and
/// never a background fill.
///
/// Results are cached per content and width: the view calls this for every
/// assistant item on every draw, and repeat renders are hash + clone instead
/// of a full parse + highlight.
pub fn render_assistant(text: &str, width: u16, base: Style) -> Vec<Line<'static>> {
    let width = width.max(1);
    let key = cache_key(text, width);
    if let Ok(cache) = render_cache().lock()
        && let Some(hit) = cache.get(&key).cloned()
    {
        return hit;
    }
    let lines = render_assistant_uncached(text, width, base);
    if let Ok(mut cache) = render_cache().lock() {
        if cache.len() >= CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, lines.clone());
    }
    lines
}

fn render_assistant_uncached(text: &str, width: u16, base: Style) -> Vec<Line<'static>> {
    // mdfrier drops a trailing heading without a final newline
    // ("# Title" parses to nothing, "# Title\n" to "Title"), so
    // normalize block input to be newline-terminated for parsing.
    // The fallback below still uses the original text.
    let normalized: Option<String> = if text.is_empty() || text.ends_with('\n') {
        None
    } else {
        let mut s = String::with_capacity(text.len() + 1);
        s.push_str(text);
        s.push('\n');
        Some(s)
    };
    let parse_text = normalized.as_deref().unwrap_or(text);
    let rendered = FRIER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let frier = match slot.as_mut() {
            Some(f) => f,
            None => match mdfrier::MdFrier::new() {
                Ok(f) => slot.insert(f),
                Err(e) => {
                    WARN_INIT.call_once(|| {
                        eprintln!("tui: markdown parser init failed ({e}); showing plain text");
                    });
                    return None;
                }
            },
        };
        let theme = TuiTheme;
        let iter = match frier.parse(width, parse_text, &theme) {
            Ok(it) => it,
            Err(e) => {
                WARN_PARSE.call_once(|| {
                    eprintln!("tui: markdown parse failed ({e}); showing plain text");
                });
                return None;
            }
        };
        let mut out = Vec::new();
        let mut code_group: Vec<(String, mdfrier::Line)> = Vec::new();
        for md_line in iter {
            if let mdfrier::LineKind::CodeBlock { language } = &md_line.kind {
                code_group.push((language.clone(), md_line));
                continue;
            }
            flush_code(&mut code_group, width as usize, &mut out);
            let (mut line, _) = mdfrier::ratatui::render_line(md_line, &theme);
            for span in &mut line.spans {
                span.style = base.patch(span.style);
            }
            // An empty line has no spans to patch; give it the base
            // style so an empty paragraph still carries the color.
            if line.spans.is_empty() {
                line.style = base;
            }
            out.push(line);
        }
        flush_code(&mut code_group, width as usize, &mut out);
        Some(out)
    });

    match rendered {
        Some(lines) if !lines.is_empty() => lines,
        _ => fallback(text, width as usize, base),
    }
}

fn fallback(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    harness_tui_state::wrap::wrap_text(text, width.max(1))
        .into_iter()
        .map(|s| Line::styled(s, base))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn base() -> Style {
        Style::new().fg(Color::Cyan)
    }

    fn plain_text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn has_background(lines: &[Line]) -> bool {
        lines
            .iter()
            .any(|l| l.style.bg.is_some() || l.spans.iter().any(|s| s.style.bg.is_some()))
    }

    #[test]
    fn plain_paragraph_round_trips() {
        let lines = render_assistant("hello", 22, base());
        assert_eq!(plain_text(&lines), vec!["hello"]);
        // Unstyled text keeps the assistant base color.
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn emphasis_markers_are_stripped_and_styled() {
        let lines = render_assistant("*emphasis* and **strong**", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(!text.contains('*'), "decorators stripped: {text:?}");
        assert!(text.contains("emphasis"), "text: {text:?}");
        assert!(text.contains("strong"), "text: {text:?}");
        // At least one span carries a non-base modifier (italic/bold).
        let styled = lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.style.add_modifier != ratatui::style::Modifier::empty());
        assert!(styled, "expected styled spans: {lines:?}");
    }

    #[test]
    fn heading_renders_without_hash() {
        let lines = render_assistant("# Title", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains("Title"), "text: {text:?}");
        assert!(!text.starts_with('#'), "hash stripped: {text:?}");
    }

    #[test]
    fn code_block_keeps_content_without_fences() {
        let lines = render_assistant("```rust\nlet x = 1;\n```\n", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains("let x = 1;"), "text: {text:?}");
        assert!(!text.contains("```"), "fences stripped: {text:?}");
    }

    #[test]
    fn code_block_has_syntax_colors_and_no_background() {
        let lines = render_assistant("```rust\nfn main() {\n    let x = 1;\n}\n```\n", 80, base());
        assert!(!has_background(&lines), "no grey fill: {lines:?}");
        let fgs: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(
            fgs.len() >= 2,
            "expected token colors, got {fgs:?}: {lines:?}"
        );
    }

    #[test]
    fn unknown_language_falls_back_without_background() {
        let lines = render_assistant("```nosuchlang\nsome code here\n```\n", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains("some code here"), "text: {text:?}");
        assert!(!text.contains("```"), "fences stripped: {text:?}");
        assert!(!has_background(&lines), "no grey fill: {lines:?}");
    }

    #[test]
    fn inline_code_has_no_background() {
        let lines = render_assistant("use `let x` here", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains("let x"), "text: {text:?}");
        assert!(!text.contains('`'), "backticks stripped: {text:?}");
        assert!(!has_background(&lines), "no grey fill: {lines:?}");
    }

    #[test]
    fn list_renders_items() {
        let lines = render_assistant("- a\n- b", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains('a'), "text: {text:?}");
        assert!(text.contains('b'), "text: {text:?}");
    }

    #[test]
    fn long_markdown_wraps_to_width() {
        use unicode_width::UnicodeWidthStr;
        let lines = render_assistant(
            "This is a long paragraph with *emphasis* that must wrap within the width.",
            20,
            base(),
        );
        assert!(lines.len() > 1, "wraps: {lines:?}");
        for line in &lines {
            let w = line.spans.iter().map(|s| s.content.width()).sum::<usize>();
            assert!(w <= 20, "width {w}: {line:?}");
        }
    }

    #[test]
    fn long_code_line_wraps_to_width() {
        use unicode_width::UnicodeWidthStr;
        let lines = render_assistant(
            "```rust\nlet very_long_variable_name = some_function_call(with_arguments);\n```\n",
            20,
            base(),
        );
        assert!(lines.len() > 1, "wraps: {lines:?}");
        for line in &lines {
            let w = line.spans.iter().map(|s| s.content.width()).sum::<usize>();
            assert!(w <= 20, "width {w}: {line:?}");
        }
        assert!(!has_background(&lines), "no grey fill: {lines:?}");
    }

    #[test]
    fn empty_input_yields_one_line() {
        let lines = render_assistant("", 20, base());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn repeat_renders_are_identical() {
        // Scroll redraws hit the cache: same content and width must
        // return the same rows without re-parsing.
        let text = "```rust\nfn main() {\n    let x = 1;\n}\n```\n";
        let first = render_assistant(text, 40, base());
        let second = render_assistant(text, 40, base());
        assert_eq!(first, second);
    }

    #[test]
    fn cache_is_keyed_by_width() {
        // A resize must not reuse rows wrapped for another width.
        let text = "word ".repeat(20);
        let narrow = render_assistant(&text, 20, base());
        let wide = render_assistant(&text, 80, base());
        assert!(narrow.len() > wide.len(), "narrow wraps more");
        // Both still served correctly on repeat.
        assert_eq!(render_assistant(&text, 20, base()), narrow);
        assert_eq!(render_assistant(&text, 80, base()), wide);
    }

    #[test]
    fn plugin_provides_renderer_through_di() {
        let ctx = Context::root();
        ctx.load(MarkdownPlugin).unwrap();
        let renderer: Arc<RendererHandle> = ctx.inject_key(KEY_MARKDOWN_RENDERER).unwrap();
        let lines = renderer.render_assistant("**bold**", 80);
        let text: String = lines
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "bold", "DI renderer strips markers: {text:?}");
    }
}
