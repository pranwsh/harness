//! Markdown [`MessageRenderer`](harness_tui_state::render::MessageRenderer)
//! for assistant messages, via pulldown-cmark with syntect code highlighting.
//!
//! pulldown-cmark emits CommonMark events; this crate maps them to ratatui
//! lines already wrapped to the requested width, so callers must not
//! re-wrap the output. On empty output we fall back to plain wrapping so a
//! broken message never blanks the transcript.
//!
//! Look: emphasis/strong markers are stripped to styles, blockquotes get a
//! `▌ ` bar, rules fill the row with `─`, lists use `- `/`1. ` markers,
//! links render as `▐desc▌◖url◗`, and tables are box-drawn with a separator
//! between every row (including body rows).
//!
//! Code is rendered with real syntax highlighting and no background fill:
//! fenced blocks are highlighted per their fence language, inline code
//! renders as plain assistant text.
//!
//! Only MIT/Apache dependencies (pulldown-cmark, syntect). The engine stays
//! behind the renderer trait so downstream crates never absorb a parser
//! dependency transitively.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use harness_contracts::KEY_MARKDOWN_RENDERER;
use harness_core::{Context, Result};
use harness_tui_state::{
    render::{MessageRenderer, RendererHandle},
    wrap::{display_width, wrap_spans},
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color as RatColor, Modifier as RatModifier, Style},
    text::{Line, Span as RatSpan},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
};

/// Cache of fully rendered assistant messages keyed by content hash and
/// wrap width. The view re-renders every item on every draw (each scroll
/// step, each stream event, each 250ms tick), while parsing plus syntax
/// highlighting costs milliseconds per message — that is the scroll
/// stutter. Chat items are immutable once pushed, so a cache hit
/// (hash + clone) is always valid. Bounded; cleared on overflow.
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

// ============================================================================
// Symbols and styles (same look as the previous engine)
// ============================================================================

const LINK_DESC_OPEN: &str = "▐";
const LINK_DESC_CLOSE: &str = "▌";
const LINK_URL_OPEN: &str = "◖";
const LINK_URL_CLOSE: &str = "◗";
const BLOCKQUOTE_BAR: &str = "▌ ";
const HR_CHAR: &str = "─";
const TASK_CHECKED: &str = "[✓] ";
const TASK_UNCHECKED: &str = "[ ] ";
const TABLE_V: &str = "│";
const TABLE_H: &str = "─";
const TABLE_TL: &str = "┌";
const TABLE_TR: &str = "┐";
const TABLE_BL: &str = "└";
const TABLE_BR: &str = "┘";
const TABLE_TJ: &str = "┬";
const TABLE_BJ: &str = "┴";
const TABLE_LJ: &str = "├";
const TABLE_RJ: &str = "┤";
const TABLE_X: &str = "┼";
const BULLET: &str = "- ";

fn emphasis_style() -> Style {
    Style::default()
        .add_modifier(RatModifier::ITALIC)
        .fg(RatColor::Indexed(220))
}

fn strong_style() -> Style {
    Style::default()
        .add_modifier(RatModifier::BOLD)
        .fg(RatColor::Indexed(220))
}

fn strike_style() -> Style {
    Style::default()
        .add_modifier(RatModifier::CROSSED_OUT | RatModifier::DIM)
        .fg(RatColor::Indexed(245))
}

fn link_text_style() -> Style {
    Style::default()
        .fg(RatColor::Indexed(4))
        .bg(RatColor::Indexed(237))
        .add_modifier(RatModifier::UNDERLINED)
}

fn link_wrap_style() -> Style {
    Style::default().fg(RatColor::Indexed(237))
}

fn hr_style() -> Style {
    Style::default().fg(RatColor::Indexed(240))
}

fn table_border_style() -> Style {
    Style::default().fg(RatColor::Indexed(240))
}

fn table_header_style() -> Style {
    Style::default()
        .add_modifier(RatModifier::BOLD)
        .fg(RatColor::Indexed(255))
}

fn prefix_style() -> Style {
    Style::default().fg(RatColor::Indexed(222))
}

fn blockquote_style(depth: usize) -> Style {
    const COLORS: [RatColor; 6] = [
        RatColor::Indexed(202),
        RatColor::Indexed(203),
        RatColor::Indexed(204),
        RatColor::Indexed(205),
        RatColor::Indexed(206),
        RatColor::Indexed(207),
    ];
    Style::default().fg(COLORS[depth % COLORS.len()])
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

/// Slice pre-styled `(style, text)` runs at plain-text wrap boundaries to
/// `width` display columns. Returns span rows; callers add prefixes, patch
/// base styles, and wrap in lines.
fn wrap_runs(segments: &[(Style, String)], width: usize) -> Vec<Vec<RatSpan<'static>>> {
    let plain: String = segments.iter().map(|(_, t)| t.as_str()).collect();
    let mut runs: Vec<(usize, usize, Style)> = Vec::with_capacity(segments.len());
    let mut off = 0;
    for (style, text) in segments {
        let end = off + text.len();
        runs.push((off, end, *style));
        off = end;
    }
    wrap_spans(&plain, width.max(1))
        .into_iter()
        .map(|(a, b)| {
            runs.iter()
                .filter_map(|&(rs, re, style)| {
                    let (s, e) = (rs.max(a), re.min(b));
                    if s < e {
                        Some(RatSpan::styled(plain[s..e].to_owned(), style))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect()
}

// ============================================================================
// Inline model: pulldown-cmark events to styled segments
// ============================================================================

/// Inline marks carried by a text run. Maps to ratatui styles in
/// [`seg_style`].
#[derive(Debug, Clone, Copy, Default)]
struct Marks {
    em: bool,
    strong: bool,
    strike: bool,
    code: bool,
    link_text: bool,
    link_url: bool,
    link_wrap: bool,
}

#[derive(Debug, Clone)]
struct Seg {
    marks: Marks,
    text: String,
}

fn seg_style(m: Marks, header: bool) -> Style {
    if m.link_wrap {
        return link_wrap_style();
    }
    if m.link_url {
        return link_text_style();
    }
    let mut style = if header {
        table_header_style()
    } else {
        Style::default()
    };
    if m.link_text {
        style = style.patch(link_text_style());
    }
    if m.code {
        // Inline code carries no style of its own: the caller patches the
        // base style underneath, so it inherits the assistant color.
        return style.patch(Style::default());
    }
    if m.em {
        style = style.patch(emphasis_style());
    }
    if m.strong {
        style = style.patch(strong_style());
    }
    if m.strike {
        style = style.patch(strike_style());
    }
    style
}

fn md_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// A block-level tag (everything else is inline content).
fn is_block_tag(tag: &Tag) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::BlockQuote(_)
            | Tag::List(_)
            | Tag::Item
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::MetadataBlock(_)
    )
}

// ============================================================================
// Block assembly
// ============================================================================

struct ListFrame {
    ordered: bool,
    next: u64,
    /// Display width of the current item marker; outer levels indent by this.
    cur_width: usize,
}

struct ItemCtx {
    marker: String,
    width: usize,
    list_id: usize,
    /// Still on the item's first block (gets the marker, not the indent).
    first: bool,
}

struct PrevItem {
    list_id: usize,
    ancestry: Vec<usize>,
}

struct TableBuild {
    aligns: Vec<Alignment>,
    header: Vec<Vec<Seg>>,
    rows: Vec<Vec<Vec<Seg>>>,
}

struct Body<'a> {
    events: Vec<Event<'a>>,
    pos: usize,
    width: usize,
    base: Style,
    out: Vec<Line<'static>>,
    /// A block was emitted that wants a blank line after it (everything
    /// except headers).
    blank_queued: bool,
    bq_depth: usize,
    lists: Vec<ListFrame>,
    /// List ids parallel to [`Body::lists`]: the nesting ancestry.
    list_ids: Vec<usize>,
    next_list_id: usize,
    item: Option<ItemCtx>,
    prev_item: Option<PrevItem>,
}

fn span_width(spans: &[RatSpan]) -> usize {
    spans
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum()
}

/// Patch every span's style over `base`; empty lines carry `base` so an
/// empty paragraph still has the assistant color. Code spans from syntect
/// never pass through here (they keep token colors only).
fn patch_line(line: &mut Line, base: Style) {
    for span in &mut line.spans {
        span.style = base.patch(span.style);
    }
    if line.spans.is_empty() {
        line.style = base;
    }
}

fn wrap_seg(marks: Marks, text: &str) -> Seg {
    Seg {
        marks,
        text: text.to_owned(),
    }
}

/// Closing runs for a link (`▐desc▌◖url◗`) or image (`![alt](url)`).
fn push_link_close(segs: &mut Vec<Seg>, dest: String, image: bool) {
    const WRAP: Marks = Marks {
        em: false,
        strong: false,
        strike: false,
        code: false,
        link_text: false,
        link_url: false,
        link_wrap: true,
    };
    const URL: Marks = Marks {
        em: false,
        strong: false,
        strike: false,
        code: false,
        link_text: false,
        link_url: true,
        link_wrap: false,
    };
    if image {
        segs.push(wrap_seg(WRAP, "]("));
        segs.push(wrap_seg(URL, &dest));
        segs.push(wrap_seg(WRAP, ")"));
    } else {
        segs.push(wrap_seg(WRAP, LINK_DESC_CLOSE));
        segs.push(wrap_seg(WRAP, LINK_URL_OPEN));
        segs.push(wrap_seg(URL, &dest));
        segs.push(wrap_seg(WRAP, LINK_URL_CLOSE));
    }
}

impl<'a> Body<'a> {
    fn run(&mut self) {
        while self.pos < self.events.len() {
            let ev = self.events[self.pos].clone();
            match ev {
                Event::Start(tag) if is_block_tag(&tag) => self.block_start(tag),
                Event::Start(_) => {
                    // Inline-level open at block position (e.g. emphasis
                    // starting a tight list item): collect a paragraph.
                    let segs = self.inline_until(None);
                    if segs.iter().any(|s| !s.text.trim().is_empty()) {
                        self.emit_para(segs, false);
                    }
                }
                Event::End(tag) => self.block_end(tag),
                Event::Rule => {
                    self.pos += 1;
                    self.emit_hr();
                }
                _ => {
                    // Stray inline content (tight list item text): collect a
                    // paragraph running to the next block boundary.
                    let segs = self.inline_until(None);
                    if segs.iter().any(|s| !s.text.trim().is_empty()) {
                        self.emit_para(segs, false);
                    }
                }
            }
        }
    }

    fn block_start(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => {
                let end = Tag::Paragraph.to_end();
                self.pos += 1;
                let segs = self.inline_until(Some(end));
                self.emit_para(segs, false);
            }
            Tag::Heading { .. } => {
                let end = tag.to_end();
                self.pos += 1;
                let segs = self.inline_until(Some(end));
                self.emit_header(segs);
            }
            Tag::CodeBlock(kind) => {
                let lang = match &kind {
                    CodeBlockKind::Fenced(info) => info
                        .split([' ', '\t'])
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                let end = TagEnd::CodeBlock;
                self.pos += 1;
                let code = self.code_until_end(end);
                self.emit_code(&lang, &code);
            }
            Tag::BlockQuote(_) => {
                self.pos += 1;
                self.bq_depth += 1;
            }
            Tag::List(start) => {
                self.pos += 1;
                let id = self.next_list_id;
                self.next_list_id += 1;
                self.lists.push(ListFrame {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                    cur_width: BULLET.len(),
                });
                self.list_ids.push(id);
            }
            Tag::Item => {
                self.pos += 1;
                self.begin_item();
            }
            Tag::Table(aligns) => {
                self.pos += 1;
                let table = self.collect_table(aligns);
                self.emit_table(table);
            }
            Tag::HtmlBlock => {
                let end = Tag::HtmlBlock.to_end();
                self.pos += 1;
                let html = self.literal_until(end);
                self.emit_literal(&html);
            }
            _ => {
                // Stray table-section tags (only reachable inside a table,
                // which `collect_table` consumes) and metadata blocks:
                // metadata is skipped, the rest descend to their children.
                match tag {
                    Tag::FootnoteDefinition(_)
                    | Tag::MetadataBlock(_)
                    | Tag::DefinitionList
                    | Tag::DefinitionListTitle
                    | Tag::DefinitionListDefinition => self.skip_block(),
                    _ => self.pos += 1,
                }
            }
        }
    }

    fn block_end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::BlockQuote(_) => {
                self.bq_depth = self.bq_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.list_ids.pop();
            }
            TagEnd::Item => {
                self.item = None;
            }
            _ => {}
        }
        self.pos += 1;
    }

    fn begin_item(&mut self) {
        if self.lists.is_empty() {
            // Defensive: an item outside any list still gets a bullet.
            let id = self.next_list_id;
            self.next_list_id += 1;
            self.lists.push(ListFrame {
                ordered: false,
                next: 1,
                cur_width: BULLET.len(),
            });
            self.list_ids.push(id);
        }
        let frame = self
            .lists
            .last_mut()
            .expect("list frame pushed above");
        let mut marker = if frame.ordered {
            let n = frame.next;
            frame.next += 1;
            format!("{n}. ")
        } else {
            BULLET.to_owned()
        };
        if let Some(Event::TaskListMarker(checked)) = self.events.get(self.pos).cloned() {
            self.pos += 1;
            marker.push_str(if checked { TASK_CHECKED } else { TASK_UNCHECKED });
        }
        let width = display_width(&marker);
        frame.cur_width = width;
        let list_id = *self.list_ids.last().expect("list id pushed above");
        self.item = Some(ItemCtx {
            marker,
            width,
            list_id,
            first: true,
        });
    }

    /// Blank-line separation plus nesting prefixes for one block.
    /// Returns `(first_line_prefix, continuation_prefix)`.
    fn begin_block(&mut self) -> (Vec<RatSpan<'static>>, Vec<RatSpan<'static>>) {
        let mut normal = true;
        if let Some(item) = &self.item
            && item.first
        {
            // Sibling items of the same list (or the same list family when
            // nesting changes) follow each other without a blank line;
            // anything else is separated.
            let ancestry = self.list_ids.clone();
            if let Some(prev) = &self.prev_item
                && (prev.list_id == item.list_id
                    || ancestry.contains(&prev.list_id)
                    || prev.ancestry.contains(&item.list_id))
            {
                normal = false;
            }
        }
        if self.blank_queued && normal && !self.out.is_empty() {
            let mut blank = Line::from(Vec::new());
            blank.style = self.base;
            self.out.push(blank);
        }
        if normal {
            self.blank_queued = false;
        }
        let prefixes = self.prefixes();
        if let Some(item) = self.item.as_mut() {
            item.first = false;
            self.prev_item = Some(PrevItem {
                list_id: item.list_id,
                ancestry: self.list_ids.clone(),
            });
        }
        prefixes
    }

    fn after_block(&mut self, header: bool) {
        self.blank_queued = !header;
    }

    /// Nesting prefixes: quote bars plus list markers/indents. The first
    /// line of an item's first block gets the marker; every other line
    /// gets an indent of the same width.
    fn prefixes(&self) -> (Vec<RatSpan<'static>>, Vec<RatSpan<'static>>) {
        let mut first = Vec::new();
        let mut cont = Vec::new();
        for d in 0..self.bq_depth {
            let bar = RatSpan::styled(BLOCKQUOTE_BAR.to_owned(), blockquote_style(d));
            first.push(bar.clone());
            cont.push(bar);
        }
        for (idx, frame) in self.lists.iter().enumerate() {
            let last = idx + 1 == self.lists.len();
            if last
                && let Some(item) = &self.item
                && item.first
            {
                first.push(RatSpan::styled(item.marker.clone(), prefix_style()));
                cont.push(RatSpan::styled(
                    " ".repeat(item.width),
                    Style::default(),
                ));
            } else {
                let pad = " ".repeat(frame.cur_width);
                first.push(RatSpan::styled(pad.clone(), Style::default()));
                cont.push(RatSpan::styled(pad, Style::default()));
            }
        }
        (first, cont)
    }

    /// Collect inline content up to (and consuming) `end`. With `None`,
    /// stop without consuming at the next block boundary: any `End`, a
    /// `Rule`, or a block-level `Start`.
    fn inline_until(&mut self, end: Option<TagEnd>) -> Vec<Seg> {
        let mut segs: Vec<Seg> = Vec::new();
        let mut marks: Vec<Marks> = vec![Marks::default()];
        // Parallel to `marks`: the link/image URL opened at each level.
        let mut urls: Vec<Option<(String, bool)>> = vec![None];
        while self.pos < self.events.len() {
            let ev = self.events[self.pos].clone();
            match ev {
                Event::End(e) => {
                    if end.as_ref().is_some_and(|want| *want == e) {
                        self.pos += 1;
                        break;
                    }
                    if end.is_none() {
                        break;
                    }
                    // Pop emphasis/link levels closed inside inline content.
                    match e {
                        TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                            if marks.len() > 1 {
                                marks.pop();
                                urls.pop();
                            }
                            self.pos += 1;
                        }
                        TagEnd::Link | TagEnd::Image => {
                            let url = if marks.len() > 1 {
                                marks.pop();
                                urls.pop().flatten()
                            } else {
                                None
                            };
                            self.pos += 1;
                            if let Some((dest, image)) = url {
                                push_link_close(&mut segs, dest, image);
                            }
                        }
                        _ => {
                            // Tolerate stray ends inside inline content.
                            self.pos += 1;
                        }
                    }
                }
                Event::Start(tag) => match tag {
                    Tag::Emphasis => {
                        let mut m = *marks.last().expect("marks base");
                        m.em = true;
                        marks.push(m);
                        urls.push(None);
                        self.pos += 1;
                    }
                    Tag::Strong => {
                        let mut m = *marks.last().expect("marks base");
                        m.strong = true;
                        marks.push(m);
                        urls.push(None);
                        self.pos += 1;
                    }
                    Tag::Strikethrough => {
                        let mut m = *marks.last().expect("marks base");
                        m.strike = true;
                        marks.push(m);
                        urls.push(None);
                        self.pos += 1;
                    }
                    Tag::Link { dest_url, .. } => {
                        segs.push(Seg {
                            marks: Marks {
                                link_wrap: true,
                                ..Marks::default()
                            },
                            text: LINK_DESC_OPEN.to_owned(),
                        });
                        let mut m = *marks.last().expect("marks base");
                        m.link_text = true;
                        marks.push(m);
                        urls.push(Some((dest_url.into_string(), false)));
                        self.pos += 1;
                    }
                    Tag::Image { dest_url, .. } => {
                        segs.push(Seg {
                            marks: Marks {
                                link_wrap: true,
                                ..Marks::default()
                            },
                            text: "![".to_owned(),
                        });
                        let mut m = *marks.last().expect("marks base");
                        m.link_text = true;
                        marks.push(m);
                        urls.push(Some((dest_url.into_string(), true)));
                        self.pos += 1;
                    }
                    _ if !is_block_tag(&tag) => {
                        // Other inline wrappers (superscript/subscript):
                        // descend, children inherit the current marks.
                        marks.push(*marks.last().expect("marks base"));
                        urls.push(None);
                        self.pos += 1;
                    }
                    _ if end.is_none() => break,
                    _ => {
                        // Nested block inside inline collection (e.g. a
                        // paragraph in a table cell): descend past a
                        // paragraph, skip anything heavier.
                        match tag {
                            Tag::Paragraph => self.pos += 1,
                            _ => {
                                self.pos += 1;
                                self.skip_block();
                            }
                        }
                    }
                },
                Event::Text(t) => {
                    segs.push(Seg {
                        marks: *marks.last().expect("marks base"),
                        text: t.into_string(),
                    });
                    self.pos += 1;
                }
                Event::Code(c) => {
                    let mut m = *marks.last().expect("marks base");
                    m.code = true;
                    segs.push(Seg {
                        marks: m,
                        text: c.into_string(),
                    });
                    self.pos += 1;
                }
                Event::SoftBreak => {
                    segs.push(Seg {
                        marks: *marks.last().expect("marks base"),
                        text: " ".to_owned(),
                    });
                    self.pos += 1;
                }
                Event::HardBreak => {
                    segs.push(Seg {
                        marks: *marks.last().expect("marks base"),
                        text: "\n".to_owned(),
                    });
                    self.pos += 1;
                }
                Event::Html(h) | Event::InlineHtml(h) => {
                    segs.push(Seg {
                        marks: *marks.last().expect("marks base"),
                        text: h.into_string(),
                    });
                    self.pos += 1;
                }
                Event::InlineMath(m) => {
                    segs.push(Seg {
                        marks: *marks.last().expect("marks base"),
                        text: format!("${m}$"),
                    });
                    self.pos += 1;
                }
                Event::DisplayMath(m) => {
                    segs.push(Seg {
                        marks: *marks.last().expect("marks base"),
                        text: format!("$${m}$$"),
                    });
                    self.pos += 1;
                }
                Event::FootnoteReference(name) => {
                    segs.push(Seg {
                        marks: *marks.last().expect("marks base"),
                        text: format!("[^{name}]"),
                    });
                    self.pos += 1;
                }
                Event::TaskListMarker(checked) => {
                    segs.push(Seg {
                        marks: *marks.last().expect("marks base"),
                        text: (if checked { TASK_CHECKED } else { TASK_UNCHECKED }).to_owned(),
                    });
                    self.pos += 1;
                }
                Event::Rule => {
                    if end.is_none() {
                        break;
                    }
                    self.pos += 1;
                }
            }
        }
        segs
    }

    /// Skip from after a `Start` to past its matching `End`.
    fn skip_block(&mut self) {
        let mut depth = 0usize;
        while self.pos < self.events.len() {
            match &self.events[self.pos] {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    if depth == 0 {
                        self.pos += 1;
                        return;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    /// Collect raw code text up to (and consuming) `end`.
    fn code_until_end(&mut self, end: TagEnd) -> String {
        let mut code = String::new();
        while self.pos < self.events.len() {
            let ev = self.events[self.pos].clone();
            match ev {
                Event::End(e) if e == end => {
                    self.pos += 1;
                    break;
                }
                Event::Text(t) | Event::Code(t) => {
                    code.push_str(t.as_ref());
                    self.pos += 1;
                }
                Event::SoftBreak | Event::HardBreak => {
                    code.push('\n');
                    self.pos += 1;
                }
                Event::Html(h) | Event::InlineHtml(h) => {
                    code.push_str(h.as_ref());
                    self.pos += 1;
                }
                Event::Start(_) => {
                    self.pos += 1;
                    self.skip_block();
                }
                _ => {
                    self.pos += 1;
                }
            }
        }
        code
    }

    /// Collect literal text up to (and consuming) `end` (HTML blocks).
    fn literal_until(&mut self, end: TagEnd) -> String {
        let mut text = String::new();
        while self.pos < self.events.len() {
            let ev = self.events[self.pos].clone();
            match ev {
                Event::End(e) if e == end => {
                    self.pos += 1;
                    break;
                }
                Event::Text(t)
                | Event::Code(t)
                | Event::Html(t)
                | Event::InlineHtml(t) => {
                    text.push_str(t.as_ref());
                    self.pos += 1;
                }
                Event::SoftBreak | Event::HardBreak => {
                    text.push('\n');
                    self.pos += 1;
                }
                Event::Start(_) => {
                    self.pos += 1;
                    self.skip_block();
                }
                _ => {
                    self.pos += 1;
                }
            }
        }
        text
    }

    fn collect_table(&mut self, aligns: Vec<Alignment>) -> TableBuild {
        let mut table = TableBuild {
            aligns,
            header: Vec::new(),
            rows: Vec::new(),
        };
        let mut in_head = false;
        let mut cur_row: Vec<Vec<Seg>> = Vec::new();
        while self.pos < self.events.len() {
            let ev = self.events[self.pos].clone();
            match ev {
                Event::Start(Tag::TableHead) => {
                    in_head = true;
                    self.pos += 1;
                }
                Event::Start(Tag::TableRow) => {
                    cur_row = Vec::new();
                    self.pos += 1;
                }
                Event::Start(Tag::TableCell) => {
                    self.pos += 1;
                    let cell = self.inline_until(Some(TagEnd::TableCell));
                    cur_row.push(cell);
                }
                Event::End(TagEnd::TableRow) => {
                    self.pos += 1;
                    if in_head {
                        table.header = std::mem::take(&mut cur_row);
                    } else {
                        table.rows.push(std::mem::take(&mut cur_row));
                    }
                }
                Event::End(TagEnd::TableHead) => {
                    // Header cells arrive directly under `TableHead`
                    // (no `TableRow` wrapper): flush them here.
                    in_head = false;
                    if !cur_row.is_empty() {
                        table.header = std::mem::take(&mut cur_row);
                    }
                    self.pos += 1;
                }
                Event::End(TagEnd::Table) => {
                    self.pos += 1;
                    break;
                }
                Event::Start(_) => {
                    self.pos += 1;
                    self.skip_block();
                }
                _ => {
                    self.pos += 1;
                }
            }
        }
        table
    }

    fn emit_para(&mut self, segs: Vec<Seg>, header: bool) {
        let (first_pre, cont_pre) = self.begin_block();
        let pre_w = span_width(&first_pre);
        let content_w = self.width.saturating_sub(pre_w).max(1);
        let styled: Vec<(Style, String)> = segs
            .into_iter()
            .filter(|s| !s.text.is_empty())
            .map(|s| (seg_style(s.marks, header), s.text))
            .collect();
        if styled.is_empty() {
            let mut line = Line::from(first_pre);
            patch_line(&mut line, self.base);
            self.out.push(line);
        } else {
            for (i, row) in wrap_runs(&styled, content_w).into_iter().enumerate() {
                let mut spans = if i == 0 {
                    first_pre.clone()
                } else {
                    cont_pre.clone()
                };
                spans.extend(row);
                let mut line = Line::from(spans);
                patch_line(&mut line, self.base);
                self.out.push(line);
            }
        }
        self.after_block(header);
    }

    fn emit_header(&mut self, segs: Vec<Seg>) {
        // Headers ignore nesting prefixes (plain text, hash stripped).
        let blank = self.blank_queued && !self.out.is_empty();
        if blank {
            let mut line = Line::from(Vec::new());
            line.style = self.base;
            self.out.push(line);
        }
        self.blank_queued = false;
        if let Some(item) = self.item.as_mut() {
            item.first = false;
            self.prev_item = Some(PrevItem {
                list_id: item.list_id,
                ancestry: self.list_ids.clone(),
            });
        }
        let styled: Vec<(Style, String)> = segs
            .into_iter()
            .filter(|s| !s.text.is_empty())
            .map(|s| (seg_style(s.marks, true), s.text))
            .collect();
        if styled.is_empty() {
            let mut line = Line::from(Vec::new());
            line.style = self.base;
            self.out.push(line);
        } else {
            for row in wrap_runs(&styled, self.width) {
                let mut line = Line::from(row);
                patch_line(&mut line, self.base);
                self.out.push(line);
            }
        }
        self.after_block(true);
    }

    fn emit_hr(&mut self) {
        let (first_pre, _) = self.begin_block();
        let pre_w = span_width(&first_pre);
        let avail = self.width.saturating_sub(pre_w);
        let mut spans = first_pre;
        spans.push(RatSpan::styled(HR_CHAR.repeat(avail), hr_style()));
        let mut line = Line::from(spans);
        patch_line(&mut line, self.base);
        self.out.push(line);
        self.after_block(false);
    }

    fn emit_literal(&mut self, text: &str) {
        self.begin_block();
        for part in text.split('\n') {
            let mut line = Line::from(RatSpan::raw(part.to_owned()));
            patch_line(&mut line, self.base);
            self.out.push(line);
        }
        self.after_block(false);
    }

    /// Render a fenced code block with syntax highlighting. One
    /// highlighter per block carries multi-line token state; blank lines
    /// keep their row so the block keeps its shape. Token colors are
    /// never patched with the base style and never carry a background.
    fn emit_code(&mut self, lang: &str, code: &str) {
        let (first_pre, cont_pre) = self.begin_block();
        // Code lines are not base-patched (syntect token colors win), so
        // tint bare prefixes (indents) with the base color for consistency.
        let tint = |spans: Vec<RatSpan<'static>>, base: Style| {
            spans
                .into_iter()
                .map(|mut s| {
                    if s.style == Style::default() {
                        s.style = base;
                    }
                    s
                })
                .collect::<Vec<_>>()
        };
        let first_pre = tint(first_pre, self.base);
        let cont_pre = tint(cont_pre, self.base);
        let pre_w = span_width(&first_pre);
        let content_w = self.width.saturating_sub(pre_w).max(1);
        let assets = hl_assets();
        let theme = assets
            .themes
            .themes
            .get(HL_THEME)
            .or_else(|| assets.themes.themes.values().next())
            .expect("syntect default themes");
        let syntax = assets
            .syntaxes
            .find_syntax_by_token(lang)
            .unwrap_or_else(|| assets.syntaxes.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, theme);
        let mut visual = 0usize;
        for raw in code.lines() {
            let line = raw.trim_end_matches([' ', '\t']);
            let segments = highlight_segments(&mut hl, &assets.syntaxes, line);
            for row in wrap_runs(&segments, content_w) {
                let mut spans = if visual == 0 {
                    first_pre.clone()
                } else {
                    cont_pre.clone()
                };
                spans.extend(row);
                self.out.push(Line::from(spans));
                visual += 1;
            }
        }
        // An empty code block still occupies no rows (matches the old
        // engine, which emitted nothing for empty code).
        self.after_block(false);
    }

    #[allow(clippy::too_many_lines)]
    fn emit_table(&mut self, table: TableBuild) {
        let num_cols = table.header.len().max(table.aligns.len());
        let (first_pre, cont_pre) = self.begin_block();
        if num_cols == 0 {
            self.after_block(false);
            return;
        }
        let pre_w = span_width(&first_pre);
        let avail = self.width.saturating_sub(pre_w).max(1);

        let mut header = table.header;
        header.resize_with(num_cols, Vec::new);
        let mut rows = table.rows;
        for row in &mut rows {
            row.resize_with(num_cols, Vec::new);
        }
        let aligns: Vec<Alignment> = (0..num_cols)
            .map(|i| table.aligns.get(i).copied().unwrap_or(Alignment::None))
            .collect();

        let cell_width = |cell: &[Seg]| -> usize {
            cell.iter().map(|s| display_width(s.text.as_str())).sum()
        };
        let mut col_widths: Vec<usize> = (0..num_cols)
            .map(|i| {
                let mut w = cell_width(&header[i]);
                for row in &rows {
                    w = w.max(cell_width(&row[i]));
                }
                w + 2
            })
            .collect();
        // Scale down proportionally when the table is wider than the
        // available width (same rule as the previous engine).
        let table_width: usize = col_widths.iter().sum::<usize>() + num_cols + 1;
        if table_width > avail && avail > num_cols + 1 {
            let content_width = avail - num_cols - 1;
            let total: usize = col_widths.iter().sum();
            col_widths = col_widths
                .iter()
                .map(|w| (w * content_width / total).max(3))
                .collect();
        }

        let border = |left: &str, mid: &str, right: &str| -> Vec<RatSpan<'static>> {
            let mut spans = vec![RatSpan::styled(left.to_owned(), table_border_style())];
            for (i, &w) in col_widths.iter().enumerate() {
                spans.push(RatSpan::styled(TABLE_H.repeat(w), table_border_style()));
                if i + 1 < num_cols {
                    spans.push(RatSpan::styled(mid.to_owned(), table_border_style()));
                }
            }
            spans.push(RatSpan::styled(right.to_owned(), table_border_style()));
            spans
        };
        // The header separator and every inter-row separator are
        // identical: `├──┼──┤`.
        let separator = || border(TABLE_LJ, TABLE_X, TABLE_RJ);

        let row_lines = |cells: &[Vec<Seg>], is_header: bool| -> Vec<Vec<RatSpan<'static>>> {
            let wrapped: Vec<Vec<Vec<RatSpan>>> = cells
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    let inner = col_widths.get(i).copied().unwrap_or(3).saturating_sub(2).max(1);
                    let styled: Vec<(Style, String)> = cell
                        .iter()
                        .filter(|s| !s.text.is_empty())
                        .map(|s| (seg_style(s.marks, is_header), s.text.clone()))
                        .collect();
                    wrap_runs(&styled, inner)
                })
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let mut lines = Vec::with_capacity(height);
            for li in 0..height {
                let mut spans =
                    vec![RatSpan::styled(TABLE_V.to_owned(), table_border_style())];
                for (i, &w) in col_widths.iter().enumerate() {
                    let inner = w.saturating_sub(2);
                    let row = wrapped.get(i).and_then(|c| c.get(li));
                    let content_w: usize = row
                        .map(|r| r.iter().map(|s| display_width(s.content.as_ref())).sum())
                        .unwrap_or(0);
                    let pad = inner.saturating_sub(content_w);
                    let (left_pad, right_pad) = match aligns.get(i) {
                        Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                        Some(Alignment::Right) => (pad, 0),
                        _ => (0, pad),
                    };
                    spans.push(RatSpan::raw(format!(" {}", " ".repeat(left_pad))));
                    if let Some(r) = row {
                        spans.extend(r.clone());
                    }
                    spans.push(RatSpan::raw(format!("{} ", " ".repeat(right_pad))));
                    spans.push(RatSpan::styled(TABLE_V.to_owned(), table_border_style()));
                }
                lines.push(spans);
            }
            lines
        };

        let mut table_lines: Vec<Vec<RatSpan>> = Vec::new();
        table_lines.push(border(TABLE_TL, TABLE_TJ, TABLE_TR));
        table_lines.extend(row_lines(&header, true));
        table_lines.push(separator());
        for (ri, row) in rows.iter().enumerate() {
            // Separator between body rows, identical to the header
            // separator (but none after the last row).
            if ri > 0 {
                table_lines.push(separator());
            }
            table_lines.extend(row_lines(row, false));
        }
        table_lines.push(border(TABLE_BL, TABLE_BJ, TABLE_BR));

        for (i, spans) in table_lines.into_iter().enumerate() {
            let mut full = if i == 0 {
                first_pre.clone()
            } else {
                cont_pre.clone()
            };
            full.extend(spans);
            let mut line = Line::from(full);
            patch_line(&mut line, self.base);
            self.out.push(line);
        }
        self.after_block(false);
    }
}

// ============================================================================
// Public renderer
// ============================================================================

/// Default [`MessageRenderer`] for assistant messages: pulldown-cmark
/// markdown with syntect code highlighting and no background fill.
pub struct MarkdownRenderer {
    base: Style,
}

impl MarkdownRenderer {
    pub fn new(base: Style) -> Self {
        MarkdownRenderer { base }
    }
}

impl Default for MarkdownRenderer {
    fn default() -> Self {
        MarkdownRenderer::new(harness_tui_state::render::ASSISTANT_BASE)
    }
}

impl MessageRenderer for MarkdownRenderer {
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
            Arc::new(RendererHandle(Arc::new(MarkdownRenderer::default()))),
        );
        Ok(())
    }
}

/// Render markdown `text` to ratatui lines already wrapped to `width`.
///
/// `base` is patched underneath every non-code span, so unstyled text keeps
/// the assistant color while markdown styles (emphasis, links, headings, …)
/// win where set. Code blocks carry syntect token colors and never a
/// background fill.
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
    // pulldown-cmark is infallible on any input (unclosed fences and
    // markers degrade to literal text), so there is no error path: only
    // empty output falls back to plain wrapping.
    let events: Vec<Event> = Parser::new_ext(text, md_options()).collect();
    // Skip pure-whitespace inputs early: they carry no blocks.
    if events.iter().all(|e| matches!(e, Event::SoftBreak | Event::HardBreak)) {
        return fallback(text, width as usize, base);
    }
    let mut body = Body {
        events,
        pos: 0,
        width: width as usize,
        base,
        out: Vec::new(),
        blank_queued: false,
        bq_depth: 0,
        lists: Vec::new(),
        list_ids: Vec::new(),
        next_list_id: 0,
        item: None,
        prev_item: None,
    };
    body.run();
    if body.out.is_empty() {
        fallback(text, width as usize, base)
    } else {
        body.out
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

    #[test]
    fn table_renders_with_borders_and_row_separators() {
        let lines = render_assistant("| a | b |\n|---|---|\n| c | d |\n| e | f |\n", 30, base());
        let text = plain_text(&lines);
        // Top, header, separator, row, separator, row, bottom.
        assert_eq!(text.len(), 7, "rows: {text:?}");
        assert!(text[0].starts_with("┌"), "top border: {text:?}");
        assert!(text[0].ends_with("┐"), "top border: {text:?}");
        assert!(text[6].starts_with("└"), "bottom border: {text:?}");
        assert!(text[6].ends_with("┘"), "bottom border: {text:?}");
        // The header separator and the inter-row separator are identical.
        assert_eq!(text[2], text[4], "separators identical: {text:?}");
        assert!(text[2].starts_with("├"), "separator: {text:?}");
        assert!(text[1].contains('a') && text[1].contains('b'), "header: {text:?}");
        assert!(text[3].contains('c') && text[3].contains('d'), "row: {text:?}");
        assert!(text[5].contains('e') && text[5].contains('f'), "row: {text:?}");
        assert!(!has_background(&lines), "no grey fill: {lines:?}");
    }

    #[test]
    fn table_header_cells_are_bold() {
        let lines = render_assistant("| Name |\n|------|\n| x |\n", 30, base());
        let header = &lines[1];
        assert!(
            header
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(ratatui::style::Modifier::BOLD)
                    && s.content.contains("Name")),
            "header bold: {lines:?}"
        );
    }

    #[test]
    fn table_wraps_to_width() {
        use unicode_width::UnicodeWidthStr;
        let text =
            "| name | description |\n|------|-------------|\n| widget | a very long description that must shrink |\n";
        // Wide enough to fit unshrunk: content is kept whole.
        let lines = render_assistant(text, 60, base());
        let joined = plain_text(&lines).join("\n");
        assert!(joined.contains("widget"), "content kept: {joined:?}");
        assert!(
            joined.contains("a very long description that must shrink"),
            "content kept: {joined:?}"
        );
        // Too narrow: columns shrink proportionally but rows still fit.
        let lines = render_assistant(text, 24, base());
        for line in &lines {
            let w = line.spans.iter().map(|s| s.content.width()).sum::<usize>();
            assert!(w <= 24, "width {w}: {line:?}");
        }
    }

    #[test]
    fn blockquote_renders_bar_without_marker() {
        let lines = render_assistant("> quoted text", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains("▌"), "bar: {text:?}");
        assert!(text.contains("quoted text"), "text: {text:?}");
        assert!(!text.contains('>'), "marker mapped: {text:?}");
    }

    #[test]
    fn link_renders_description_and_url() {
        let lines = render_assistant("[docs](https://example.com)", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains("docs"), "desc: {text:?}");
        assert!(text.contains("https://example.com"), "url: {text:?}");
        assert!(text.contains("▐") && text.contains("◖"), "wrappers: {text:?}");
    }

    #[test]
    fn horizontal_rule_fills_width() {
        use unicode_width::UnicodeWidthStr;
        let lines = render_assistant("---", 20, base());
        assert_eq!(lines.len(), 1, "one row: {lines:?}");
        let w: usize = lines[0].spans.iter().map(|s| s.content.width()).sum();
        assert_eq!(w, 20, "fills width: {lines:?}");
        assert!(
            plain_text(&lines)[0].chars().all(|c| c == '─'),
            "rule char: {lines:?}"
        );
    }

    #[test]
    fn ordered_list_numbers_items() {
        let lines = render_assistant("1. first\n2. second", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains("1. first"), "numbered: {text:?}");
        assert!(text.contains("2. second"), "numbered: {text:?}");
    }

    #[test]
    fn unclosed_fence_renders_as_code_without_fences() {
        // Streaming chunks often end mid-fence: the parser must degrade to
        // code lines, never blank the transcript.
        let lines = render_assistant("```rust\nlet x = 1;\n", 80, base());
        let text = plain_text(&lines).join("\n");
        assert!(text.contains("let x = 1;"), "text: {text:?}");
        assert!(!text.contains("```"), "fences stripped: {text:?}");
    }
}
