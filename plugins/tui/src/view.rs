//! Rendering: turns `App` state into widgets. The only module that
//! imports ratatui; `draw` is a pure function of the app state.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use harness_tui_state::{
    app::{App, ChatItem, INPUT_VISIBLE_ROWS, ItemKind, max_chat_scroll},
    render::{ASSISTANT_BASE, MessageRenderer},
    wrap::{display_width, wrap_spans, wrap_text},
};

/// Margin, in columns, on each side of assistant/tool/notice output.
const SIDE_MARGIN: u16 = 4;

/// Single source for box corners: user messages (synthesized text rows)
/// and the input box (`Block`) both use rounded corners from here.
const BOX_BORDER_TYPE: BorderType = BorderType::Rounded;

/// Draws the whole UI: chat pane (fills remaining space) above the
/// input box, which grows from 1 up to `INPUT_VISIBLE_ROWS` text rows
/// as the input wraps. Takes `&mut App` to record the measured input
/// width, which key handling needs for visual cursor motion. Assistant
/// messages render through `renderer`, keeping this module free of any
/// markdown engine dependency.
pub fn draw(f: &mut Frame, app: &mut App, renderer: &dyn MessageRenderer) {
    let area = f.area();
    app.set_input_width(area.width.saturating_sub(2).max(1) as usize);
    let input_height = app.input_rows().min(INPUT_VISIBLE_ROWS) as u16 + 2;
    let [chat_area, input_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(input_height)]).areas(area);

    draw_chat(f, app, chat_area, renderer);
    draw_input(f, app, input_area);
}

/// One chat item laid out for the current chat-area width: position,
/// size, and wrapped content rows.
#[derive(Debug)]
struct MsgLayout {
    x: u16,
    width: u16,
    lines: Vec<Line<'static>>,
    /// Total rows this item occupies.
    height: usize,
}

fn draw_chat(f: &mut Frame, app: &mut App, area: Rect, renderer: &dyn MessageRenderer) {
    if area.width < 2 || area.height == 0 {
        return;
    }
    let layouts: Vec<MsgLayout> = app
        .items()
        .iter()
        .map(|item| layout_item(item, area.width, renderer))
        .collect();
    let total: usize = layouts.iter().map(|l| l.height).sum();
    let viewport = area.height as usize;
    // Report the measured geometry so the stored offset is clamped to
    // the displayable range (no invisible scroll debt).
    app.set_chat_geometry(total, viewport);

    // Bottom-anchored row window: `scroll` content rows stay hidden
    // below the viewport; the rest fills upward from the bottom edge.
    let (skip, used) = visible_range(total, app.scroll_rows(), viewport);
    let mut y = area.y + (viewport - used) as u16;
    let mut remaining = used;
    let mut to_skip = skip;
    for lay in &layouts {
        if to_skip >= lay.height {
            to_skip -= lay.height;
            continue;
        }
        let take = (lay.height - to_skip).min(remaining);
        if take == 0 {
            break;
        }
        let rect = Rect {
            x: area.x + lay.x,
            y,
            width: lay.width,
            height: take as u16,
        };
        if rect.width == 0 || rect.height == 0 {
            y += take as u16;
            remaining -= take;
            to_skip = 0;
            continue;
        }
        let paragraph = Paragraph::new(lay.lines.clone()).scroll((to_skip as u16, 0));
        f.render_widget(paragraph, rect);
        y += take as u16;
        remaining -= take;
        to_skip = 0;
        if remaining == 0 {
            break;
        }
    }
}

/// Splits `total_rows` of content into the rows shown in a `viewport`-
/// tall window with `scroll_rows` hidden below it. Returns `(skip,
/// used)`: content rows to skip from the top and rows actually drawn.
/// The scroll offset is clamped so the earliest row never sinks below
/// one row under the viewport top.
fn visible_range(total_rows: usize, scroll_rows: usize, viewport: usize) -> (usize, usize) {
    let scroll = scroll_rows.min(max_chat_scroll(total_rows, viewport));
    let available = total_rows.saturating_sub(scroll);
    let used = available.min(viewport);
    (available.saturating_sub(viewport), used)
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let busy = app.live_turns > 0;
    let title = if busy {
        " input (turn in flight) "
    } else {
        " input "
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BOX_BORDER_TYPE)
        .border_style(Style::new().fg(Color::Blue))
        .title(title)
        .title_style(
            Style::new()
                .fg(if busy { Color::Yellow } else { Color::Blue })
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    if inner.width == 0 || inner.height == 0 {
        f.render_widget(block, area);
        return;
    }

    // Window the wrapped input rows through the app's scroll offset so
    // at most INPUT_VISIBLE_ROWS rows show; the cursor is always kept
    // inside that window by `follow_input_cursor`.
    let width = inner.width as usize;
    let text = app.input();
    let lines: Vec<Line<'static>> = wrap_spans(text, width)
        .into_iter()
        .map(|(a, b)| Line::from(text[a..b].to_owned()))
        .collect();
    let total = lines.len();
    let visible = total.clamp(1, INPUT_VISIBLE_ROWS);
    let scroll = app.input_scroll().min(total.saturating_sub(1)) as u16;
    let paragraph = Paragraph::new(lines).scroll((scroll, 0)).block(block);
    f.render_widget(paragraph, area);

    // Place the terminal cursor on the cursor's visual row/column,
    // relative to the scrolled window.
    let (cursor_row, cursor_col) = app.input_cursor_visual();
    let row = cursor_row
        .saturating_sub(scroll as usize)
        .min(visible.saturating_sub(1));
    let col = (cursor_col as u16).min(inner.width.saturating_sub(1));
    f.set_cursor_position(ratatui::layout::Position {
        x: inner.x.saturating_add(col),
        y: inner.y.saturating_add(row as u16),
    });
}

/// Lays out one chat item for a chat area of `area_width` columns.
/// User messages become shrink-to-fit bordered boxes hugging the right
/// margin; everything else is unbordered text with 4-column margins on
/// both sides.
///
/// Box borders are synthesized as text rows (not a `Block` widget) so
/// that partial boxes at the scroll edge clip row-by-row like any
/// other content; the glyphs come from [`BOX_BORDER_TYPE`], matching
/// the input box.
fn layout_item(item: &ChatItem, area_width: u16, renderer: &dyn MessageRenderer) -> MsgLayout {
    let style = item_style(item.kind);
    if item.kind == ItemKind::User {
        // The box's right edge sits on the 4-column right margin, and
        // its width leaves at least the left margin intact, so user
        // messages share the assistant text column.
        let avail = area_width.saturating_sub(SIDE_MARGIN) as usize;
        let inner_cap = avail.saturating_sub(2 + SIDE_MARGIN as usize).max(1);
        let natural = item.text.split('\n').map(display_width).max().unwrap_or(0);
        let inner = natural.clamp(1, inner_cap);
        let strs = wrap_text(&item.text, inner);
        let content_width = strs.iter().map(|s| display_width(s)).max().unwrap_or(0);
        let box_width = (content_width + 2)
            .min(area_width as usize)
            .min(avail.max(2));
        // Chrome follows the User fg but never takes content modifiers
        // (BOLD box glyphs look heavy / brighten on many terminals).
        let border = Style::new()
            .patch(style)
            .remove_modifier(style.add_modifier);
        let set = BOX_BORDER_TYPE.to_border_set();
        let mut lines = Vec::with_capacity(strs.len() + 2);
        lines.push(Line::from(vec![
            Span::styled(set.top_left, border),
            Span::styled(
                set.horizontal_top.repeat(box_width.saturating_sub(2)),
                border,
            ),
            Span::styled(set.top_right, border),
        ]));
        for row in strs {
            let pad = content_width.saturating_sub(display_width(&row));
            lines.push(Line::from(vec![
                Span::styled(set.vertical_left, border),
                Span::styled(row, style),
                Span::styled(" ".repeat(pad), style),
                Span::styled(set.vertical_right, border),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled(set.bottom_left, border),
            Span::styled(
                set.horizontal_bottom.repeat(box_width.saturating_sub(2)),
                border,
            ),
            Span::styled(set.bottom_right, border),
        ]));
        let height = lines.len();
        return MsgLayout {
            x: (avail as u16).saturating_sub(box_width as u16),
            width: box_width as u16,
            lines,
            height,
        };
    }

    let x = SIDE_MARGIN.min(area_width);
    // Both margins come out of the text column so long lines wrap
    // before reaching the right edge. Assistant messages render through
    // the injected engine (which wraps itself); everything else is
    // plain wrapped text.
    let width = area_width.saturating_sub(x + SIDE_MARGIN).max(1);
    let lines = if item.kind == ItemKind::Assistant {
        renderer.render_assistant(&item.text, width)
    } else {
        wrap_text(&item.text, width as usize)
            .into_iter()
            .map(|s| Line::styled(s, style))
            .collect::<Vec<_>>()
    };
    let height = lines.len().max(1);
    MsgLayout {
        x,
        width,
        lines,
        height,
    }
}

fn item_style(kind: ItemKind) -> Style {
    match kind {
        ItemKind::User => Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ItemKind::Assistant => ASSISTANT_BASE,
        ItemKind::ToolStarted | ItemKind::ToolOk | ItemKind::Notice => {
            Style::new().fg(Color::DarkGray)
        }
        ItemKind::ToolErr => Style::new().fg(Color::Red),
        ItemKind::Error => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_tui_markdown::MdfrierRenderer;
    use harness_tui_state::{
        app::{AppMsg, KeyEvent},
        render::PlainRenderer,
    };

    fn user_item(text: &str) -> ChatItem {
        ChatItem {
            text: text.to_owned(),
            kind: ItemKind::User,
        }
    }

    fn assistant_item(text: &str) -> ChatItem {
        ChatItem {
            text: text.to_owned(),
            kind: ItemKind::Assistant,
        }
    }

    /// Renderer with no markdown interpretation, for layout tests that
    /// must not depend on any engine.
    fn plain() -> PlainRenderer {
        PlainRenderer::new(Style::new())
    }

    fn markdown() -> MdfrierRenderer {
        MdfrierRenderer::default()
    }

    #[test]
    fn visible_range_bottom_anchors_when_following() {
        // 9 content rows, 5-row viewport, nothing hidden: skip the
        // first 4 rows and draw 5.
        assert_eq!(visible_range(9, 0, 5), (4, 5));
    }

    #[test]
    fn visible_range_short_content_draws_everything() {
        assert_eq!(visible_range(3, 0, 9), (0, 3));
    }

    #[test]
    fn visible_range_scrolled_hides_rows_below() {
        // 9 rows, 2 hidden below, 5-row viewport: draw rows 2..=6.
        assert_eq!(visible_range(9, 2, 5), (2, 5));
    }

    #[test]
    fn visible_range_clamps_excessive_scroll() {
        // More rows hidden than displayable: stop with the earliest
        // row one below the viewport top (4 of 9 rows shown).
        assert_eq!(visible_range(9, 100, 5), (0, 4));
        assert_eq!(visible_range(0, 5, 5), (0, 0));
    }

    #[test]
    fn user_box_shrinks_to_content_and_hugs_right_margin() {
        let lay = layout_item(&user_item("hi"), 30, &plain());
        // 4-wide box (borders + "hi") ending 4 columns from the edge,
        // with top border, text, and bottom border rows.
        assert_eq!((lay.x, lay.width, lay.height), (22, 4, 3));
        assert_eq!(lay.lines.len(), 3);
    }

    #[test]
    fn user_box_wraps_long_messages() {
        let lay = layout_item(&user_item(&"ab ".repeat(20)), 30, &plain());
        // Box shares the assistant text column: 4-column margins.
        assert_eq!(lay.x, 4);
        assert_eq!(lay.x + lay.width, 26);
        assert!(lay.height > 3);
    }

    #[test]
    fn assistant_text_has_side_margins() {
        let lay = layout_item(&assistant_item("hello"), 30, &markdown());
        // 4-column margins on both sides of the text column.
        assert_eq!((lay.x, lay.width, lay.height), (4, 22, 1));
    }

    #[test]
    fn wrap_text_lives_in_wrap_module() {
        // Behavior covered by wrap::tests; spot-check the import.
        assert_eq!(wrap_text("a b", 10), vec!["a b"]);
    }

    /// Renders `draw` on a fixed-size test backend and returns every
    /// visible row (trailing space trimmed).
    fn render(
        app: &mut App,
        renderer: &dyn MessageRenderer,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| draw(f, app, renderer))
            .expect("test draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// Renders `draw` and returns the chat-area rows only. Valid while
    /// the input box is a single text row tall (empty or short input).
    fn render_chat(
        app: &mut App,
        renderer: &dyn MessageRenderer,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        render(app, renderer, width, height)[..(height - 3) as usize].to_vec()
    }

    fn submit(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
        let effect = app.reduce(AppMsg::Key(KeyEvent::Enter));
        assert!(effect.submitted.is_some(), "submit {text:?}");
    }

    #[test]
    fn user_message_renders_as_right_aligned_box() {
        let mut app = App::new();
        submit(&mut app, "hi");
        let rows = render_chat(&mut app, &plain(), 30, 12);
        // 3-row box bottom-anchored in the 9-row chat area, ending 4
        // columns from the right edge. Corner glyphs vary by ratatui
        // version, so only assert the stable parts: horizontals,
        // verticals, text, and both margins.
        let top: Vec<char> = rows[6].chars().collect();
        let mid: Vec<char> = rows[7].chars().collect();
        let bot: Vec<char> = rows[8].chars().collect();
        assert_eq!((top.len(), mid.len(), bot.len()), (26, 26, 26));
        assert_eq!(&top[23..25], &['─', '─']);
        assert_eq!(&mid[22..26], &['│', 'h', 'i', '│']);
        assert_eq!(&bot[23..25], &['─', '─']);
        assert_eq!(rows[5], "", "nothing above the box");
    }

    #[test]
    fn assistant_message_has_four_column_margins() {
        let mut app = App::new();
        app.update(AppMsg::Assistant("hello".into()));
        let rows = render_chat(&mut app, &markdown(), 30, 12);
        assert_eq!(rows[8], "    hello", "rows: {rows:?}");
    }

    #[test]
    fn assistant_message_renders_markdown() {
        let mut app = App::new();
        app.update(AppMsg::Assistant("**bold** and *italic*".into()));
        let rows = render_chat(&mut app, &markdown(), 30, 12);
        assert_eq!(rows[8], "    bold and italic", "rows: {rows:?}");
    }

    #[test]
    fn assistant_heading_strips_marker() {
        let mut app = App::new();
        app.update(AppMsg::Assistant("# Title".into()));
        let rows = render_chat(&mut app, &markdown(), 30, 12);
        assert_eq!(rows[8], "    Title", "rows: {rows:?}");
    }

    #[test]
    fn assistant_renders_through_any_engine() {
        // The seam: a plain engine keeps markers, proving the view does
        // not hard-depend on markdown behavior.
        let mut app = App::new();
        app.update(AppMsg::Assistant("**bold**".into()));
        let rows = render_chat(&mut app, &plain(), 30, 12);
        assert_eq!(rows[8], "    **bold**", "rows: {rows:?}");
    }

    #[test]
    fn scroll_limit_parks_earliest_message_below_top() {
        let mut app = App::new();
        for i in 0..10 {
            submit(&mut app, &format!("m{i}"));
        }
        // 30 content rows in a 9-row viewport: ten pages up banks far
        // more than displayable, but the draw clamps to the limit
        // (earliest row one below the viewport top).
        for _ in 0..10 {
            app.update(AppMsg::Key(KeyEvent::PageUp));
        }
        let rows = render_chat(&mut app, &plain(), 30, 12);
        assert_eq!(app.scroll_rows(), 22, "debt clamped to limit");
        assert_eq!(rows[0], "", "one breathing row at top");
        let top: Vec<char> = rows[1].chars().collect();
        assert_eq!(&top[23..25], &['─', '─'], "m0 top border: {rows:?}");
        assert!(rows[2].ends_with("│m0│"), "first message: {rows:?}");
        let next: Vec<char> = rows[4].chars().collect();
        assert_eq!(&next[23..25], &['─', '─'], "m1 packed under: {rows:?}");

        // Scrolling further changes nothing on screen or in state.
        app.update(AppMsg::Key(KeyEvent::PageUp));
        let again = render_chat(&mut app, &plain(), 30, 12);
        assert_eq!(again, rows);
        assert_eq!(app.scroll_rows(), 22);
    }

    #[test]
    fn chat_scrolls_one_row_per_wheel_notch() {
        let mut app = App::new();
        for i in 0..10 {
            submit(&mut app, &format!("m{i}"));
        }
        // Ten 3-row boxes = 30 content rows; the 9-row viewport shows
        // the last three boxes, bottom-anchored. Item `mi` owns rows
        // `3i..=3i+2`; the m9 text row sits on viewport row 7.
        let rows = render_chat(&mut app, &plain(), 30, 12);
        assert!(rows[7].ends_with("│m9│"), "tail: {rows:?}");
        assert!(rows[4].ends_with("│m8│"));

        // One wheel notch hides a single row: every text row moves
        // down exactly one viewport row.
        app.update(AppMsg::ScrollUp);
        assert!(!app.follows());
        let rows = render_chat(&mut app, &plain(), 30, 12);
        assert!(rows[8].ends_with("│m9│"), "shifted one row: {rows:?}");
        assert!(rows[5].ends_with("│m8│"));

        // A second notch drops the m9 text out; its top border stays
        // on the last row and the m6 text row appears first.
        app.update(AppMsg::ScrollUp);
        let rows = render_chat(&mut app, &plain(), 30, 12);
        assert!(
            !rows.iter().any(|r| r.contains("│m9│")),
            "m9 text hidden: {rows:?}"
        );
        let first: Vec<char> = rows[0].chars().collect();
        assert_eq!(&first[22..26], &['│', 'm', '6', '│']);
        let last: Vec<char> = rows[8].chars().collect();
        assert_eq!(&last[23..25], &['─', '─'], "m9 top border");

        // Paging up 10 rows lands mid-transcript; paging back down
        // twice restores the live tail.
        app.update(AppMsg::Key(KeyEvent::PageUp));
        let rows = render_chat(&mut app, &plain(), 30, 12);
        assert!(rows[7].ends_with("│m5│"), "paged up: {rows:?}");
        assert!(!rows.iter().any(|r| r.contains("m9")));
        app.update(AppMsg::Key(KeyEvent::PageDown));
        assert!(!app.follows());
        app.update(AppMsg::Key(KeyEvent::PageDown));
        assert!(app.follows());
        let rows = render_chat(&mut app, &plain(), 30, 12);
        assert!(rows[7].ends_with("│m9│"));
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
    }

    /// Input-box rows of the last render: the box is
    /// `min(rows, 3) + 2` rows tall at the frame bottom.
    fn input_box<'a>(rows: &'a [String], app: &App) -> &'a [String] {
        let height = (app.input_rows().min(INPUT_VISIBLE_ROWS) + 2).min(rows.len());
        &rows[rows.len() - height..]
    }

    #[test]
    fn input_box_grows_with_wrapped_text() {
        let mut app = App::new();
        // 28 inner columns at width 30: "ab " * 14 wraps to two rows.
        type_text(&mut app, &"ab ".repeat(14));
        let rows = render(&mut app, &plain(), 30, 12);
        assert_eq!(app.input_rows(), 2);
        let input = input_box(&rows, &app);
        // Top border + 2 text rows + bottom border.
        assert_eq!(input.len(), 4, "input box: {input:?}");
        assert!(input[1].contains("ab ab"), "first row: {:?}", input[1]);
        // Chat keeps the remaining 8 rows.
        assert_eq!(rows.len() - input.len(), 8);
    }

    #[test]
    fn input_box_caps_at_three_rows_and_scrolls() {
        let mut app = App::new();
        // 40 two-letter words wrap to five rows at 28 columns.
        type_text(&mut app, &"ab ".repeat(40));
        let rows = render(&mut app, &plain(), 30, 14);
        assert_eq!(app.input_rows(), 5);
        let input = input_box(&rows, &app);
        // Top border + 3 text rows + bottom border; the cursor sits on
        // the last row, so the tail shows.
        assert_eq!(input.len(), 5, "input box: {input:?}");
        assert!(
            input[1..4].iter().all(|r| r.contains("ab")),
            "tail rows visible: {input:?}"
        );

        // Up walks the cursor up; the window follows to the head.
        app.update(AppMsg::Key(KeyEvent::Up));
        app.update(AppMsg::Key(KeyEvent::Up));
        app.update(AppMsg::Key(KeyEvent::Up));
        app.update(AppMsg::Key(KeyEvent::Up));
        assert_eq!(app.input_scroll(), 0);
        let rows = render(&mut app, &plain(), 30, 14);
        let input = input_box(&rows, &app);
        assert_eq!(input.len(), 5, "still capped: {input:?}");
        assert!(
            input[1..4].iter().all(|r| r.contains("ab")),
            "head rows visible: {input:?}"
        );
    }

    #[test]
    fn input_newline_renders_as_extra_row() {
        let mut app = App::new();
        type_text(&mut app, "ab");
        app.update(AppMsg::Key(KeyEvent::Newline));
        type_text(&mut app, "cd");
        let rows = render(&mut app, &plain(), 30, 12);
        assert_eq!(app.input_rows(), 2);
        let input = input_box(&rows, &app);
        assert_eq!(input.len(), 4, "input box: {input:?}");
        assert!(input[1].contains("ab"), "first line: {input:?}");
        assert!(input[2].contains("cd"), "second line: {input:?}");
    }
}
