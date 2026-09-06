//! Renderer seam: views render assistant messages through this trait
//! instead of calling a markdown engine directly, so engines stay
//! swappable without touching view code.

use std::sync::Arc;

use ratatui::{
    style::{Color, Style},
    text::Line,
};

/// Base style for assistant messages, shared by views (plain items) and
/// renderer defaults so the two can never silently drift apart.
pub const ASSISTANT_BASE: Style = Style::new().fg(Color::White);

/// Renders an assistant message to ratatui lines already wrapped to
/// `width`. Implementations must not require re-wrapping by the caller.
pub trait MessageRenderer: Send + Sync {
    fn render_assistant(&self, text: &str, width: u16) -> Vec<Line<'static>>;
}

/// Plain renderer with no markdown interpretation: greedy word wrap in
/// the given base style. Used as a fallback and in view tests to prove
/// the view works against any engine.
pub struct PlainRenderer {
    base: Style,
}
impl PlainRenderer {
    pub fn new(base: Style) -> Self {
        PlainRenderer { base }
    }
}

impl MessageRenderer for PlainRenderer {
    fn render_assistant(&self, text: &str, width: u16) -> Vec<Line<'static>> {
        crate::wrap::wrap_text(text, width.max(1) as usize)
            .into_iter()
            .map(|s| Line::styled(s, self.base))
            .collect()
    }
}

/// Sized handle around a dyn renderer so it can live in the service registry
/// (`inject_key` requires `Sized`). Same idiom as `ModelClientHandle`.
pub struct RendererHandle(pub Arc<dyn MessageRenderer>);

impl RendererHandle {
    /// Borrow the engine as a trait object for view rendering.
    pub fn as_renderer(&self) -> &dyn MessageRenderer {
        &*self.0
    }
}

impl MessageRenderer for RendererHandle {
    fn render_assistant(&self, text: &str, width: u16) -> Vec<Line<'static>> {
        self.0.render_assistant(text, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn plain_renderer_wraps_without_interpreting() {
        let r = PlainRenderer::new(Style::new().fg(Color::Cyan));
        let lines = r.render_assistant("**bold** and a b c d e f", 10);
        assert!(lines.len() > 1, "wraps: {lines:?}");
        let text: String = lines
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("|");
        assert!(text.contains("**bold**"), "markers kept: {text:?}");
    }
}
