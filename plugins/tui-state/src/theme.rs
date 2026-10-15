//! Theme vocabulary: every color the TUI draws, in one copyable struct.
//!
//! No consuming plugin hardcodes a color. The theme plugin provides this
//! struct under `KEY_THEME`; renderers, the view, and any future TUI
//! plugin read their slots from an injected [`ThemeHandle`] and never
//! import a palette.
//!
//! Two properties make this cheap to use:
//!
//! - **Copy.** A `Theme` is plain data (styles + one `u64`), so a draw
//!   takes one snapshot off the handle and reads plain fields for the
//!   rest of the frame — no lock, no pointer chasing per widget.
//! - **Versioned.** [`Theme::epoch`] changes whenever the active theme is
//!   replaced. Render caches (markdown output, laid-out chat rows) key on
//!   it so a theme swap can never serve rows painted with the old palette.
//!
//! Grouping is by concern (`text`, `chrome`, `markdown`, `code`) so a slot
//! name reads as a role rather than a hex value, and `[theme.*]` in
//! `config.toml` mirrors the same shape.

use std::sync::Arc;

use ratatui::style::{Color, Modifier, Style};

/// Transcript text roles: what the reader is reading, as opposed to the
/// chrome around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Text {
    /// Body text of an assistant message; also the base style patched
    /// underneath unstyled markdown spans.
    pub assistant: Style,
    /// Text inside the user's message box.
    pub user: Style,
    /// Border of the user's message box.
    pub user_border: Style,
    /// Tool chatter (`started` lines, `ok` results, notices).
    pub tool: Style,
    /// A failed tool call.
    pub error: Style,
    /// A turn-level error: an [`Text::error`] the reader must not miss.
    pub error_emphasis: Style,
    /// De-emphasized rows (inactive tree branches, popup side items).
    pub muted: Style,
}

/// Input box and floating popup chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Chrome {
    /// Input box border.
    pub input_border: Style,
    /// Input box title while idle.
    pub input_title: Style,
    /// Input box title while a turn is in flight.
    pub input_title_busy: Style,
    /// Popup border.
    pub popup_border: Style,
    /// Popup title.
    pub popup_title: Style,
    /// The popup's cursor row (inverted against `popup_border`).
    pub popup_cursor: Style,
}

/// Assistant-message markdown. These are *composed* styles: the markdown
/// engine owns which span gets which slot (emphasis is italic because it
/// is emphasis), while the theme owns the color, so a palette swap can
/// never lose a modifier and a re-themed app can never contradict the
/// markup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Markdown {
    /// `*emphasis*`
    pub emphasis: Style,
    /// `**strong**`
    pub strong: Style,
    /// `~~strikethrough~~`
    pub strike: Style,
    /// Link description text.
    pub link_text: Style,
    /// Link bracket/url delimiters, which read as chrome around the URL.
    pub link_wrap: Style,
    /// Thematic break (`---`).
    pub rule: Style,
    /// Table box drawing.
    pub table_border: Style,
    /// Table header cells.
    pub table_header: Style,
    /// List item markers (`- `, `1. `, task boxes).
    pub list_marker: Style,
    /// Blockquote bars, one per nesting depth; depth wraps around.
    pub blockquote: [Style; 6],
}

/// Fenced code blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Code {
    /// Syntect theme. Colors only: the engine always drops the
    /// background so code renders on the terminal's own background.
    pub theme: CodeTheme,
}

/// One of the syntect themes this app is willing to hand the highlighter.
/// A closed enum (not a free string) keeps the choice validated at the
/// theme boundary and lets [`CodeTheme::as_str`] stay allocation-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CodeTheme {
    /// Dark, foreground-only, the default.
    #[default]
    Base16OceanDark,
    Base16Eighties,
}

impl CodeTheme {
    /// syntect theme name for this variant.
    pub fn as_str(self) -> &'static str {
        match self {
            CodeTheme::Base16OceanDark => "base16-ocean.dark",
            CodeTheme::Base16Eighties => "base16-eighties.dark",
        }
    }

    /// Parses a theme name, accepting the exact syntect spelling.
    /// Unknown names fall back to the default rather than erroring, so a
    /// stale config never costs the user their whole palette.
    pub fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "base16-eighties.dark" => CodeTheme::Base16Eighties,
            _ => CodeTheme::default(),
        }
    }
}

/// The active theme: one snapshot, handed to a whole frame at a time.
///
/// `epoch` is not a style — it is a monotonic version token, bumped on
/// every [`ThemeService::set`](crate::theme::ThemeService) style swap so
/// consumers can detect a change without hashing. Caches may key on it or
/// on the theme content; the latter needs no cooperation from the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Theme {
    /// Increments on every replacement. A version token, not a style.
    pub epoch: u64,
    pub text: Text,
    pub chrome: Chrome,
    pub markdown: Markdown,
    pub code: Code,
}

/// `Default` is the built-in palette, *not* an all-empty style set: a
/// derived `Default` would hand out unstyled rows and silently look like
/// "the theme is broken". Hand-written so `Theme::default()` and
/// `Theme::DEFAULT` are the same value by construction.
impl Default for Theme {
    fn default() -> Self {
        Theme::DEFAULT
    }
}

impl Theme {
    /// The built-in theme: every current hardcoded value, in one place.
    pub const DEFAULT: Theme = Theme {
        epoch: 0,
        text: Text {
            assistant: Style::new().fg(Color::White),
            user: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            user_border: Style::new().fg(Color::Green),
            tool: Style::new().fg(Color::DarkGray),
            error: Style::new().fg(Color::Red),
            error_emphasis: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            muted: Style::new().fg(Color::DarkGray),
        },
        chrome: Chrome {
            input_border: Style::new().fg(Color::Blue),
            input_title: Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
            input_title_busy: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            popup_border: Style::new().fg(Color::Cyan),
            popup_title: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            popup_cursor: Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        },
        markdown: Markdown {
            emphasis: Style::new().fg(Color::Indexed(220)).add_modifier(Modifier::ITALIC),
            strong: Style::new().fg(Color::Indexed(220)).add_modifier(Modifier::BOLD),
            // `CROSSED_OUT | DIM` in one call: bitflags' `BitOr` is not
            // `const` in this ratatui build, so compose through two const
            // `add_modifier` calls (a later add only ever sets more bits).
            strike: Style::new()
                .fg(Color::Indexed(245))
                .add_modifier(Modifier::CROSSED_OUT)
                .add_modifier(Modifier::DIM),
            link_text: Style::new()
                .fg(Color::Indexed(4))
                .bg(Color::Indexed(237))
                .add_modifier(Modifier::UNDERLINED),
            link_wrap: Style::new().fg(Color::Indexed(237)),
            rule: Style::new().fg(Color::Indexed(240)),
            table_border: Style::new().fg(Color::Indexed(240)),
            table_header: Style::new().fg(Color::Indexed(255)).add_modifier(Modifier::BOLD),
            list_marker: Style::new().fg(Color::Indexed(222)),
            blockquote: [
                Style::new().fg(Color::Indexed(202)),
                Style::new().fg(Color::Indexed(203)),
                Style::new().fg(Color::Indexed(204)),
                Style::new().fg(Color::Indexed(205)),
                Style::new().fg(Color::Indexed(206)),
                Style::new().fg(Color::Indexed(207)),
            ],
        },
        code: Code {
            theme: CodeTheme::Base16OceanDark,
        },
    };

    /// Builds the default theme stamped with `epoch`. Providers use this
    /// so a caller-supplied palette is still a distinct version.
    pub fn with_epoch(epoch: u64) -> Self {
        Theme {
            epoch,
            ..Theme::DEFAULT
        }
    }

    /// Blockquote bar style for nesting `depth` (wraps around the ramp).
    pub fn blockquote(&self, depth: usize) -> Style {
        self.markdown.blockquote[depth % self.markdown.blockquote.len()]
    }
}

/// Declares the canonical registry of configurable color slots and derives
/// everything from it: the slot table, the getter, and the setter.
///
/// One invocation is the single source of truth for slot *names* and the
/// fields they address, so the `[theme.*]` config keys, [`Theme::slot`],
/// and [`Theme::set_slot`] can never be spelled one way in config and
/// another way in code. Adding a slot is a one-line change here.
///
/// Lives next to the struct definitions (rather than in the provider) so
/// the vocabulary crate can offer slot lookup without depending on the
/// palette provider.
macro_rules! theme_slots {
    ($($key:literal => $($field:ident).+ $([$idx:literal])? ;)*) => {
        /// Every configurable color slot as `(config_key, getter)`, in a
        /// stable order. Keys are the dotted TOML names under `[theme]`.
        pub const THEME_SLOTS: &[(&str, fn(&Theme) -> Style)] = &[
            $(($key, |t: &Theme| t.$($field).+ $([$idx])?)),*
        ];

        impl Theme {
            /// Reads one slot by its config key (`"text.user_border"`).
            /// `None` for an unknown key.
            pub fn slot(&self, key: &str) -> Option<Style> {
                match key {
                    $($key => Some(self.$($field).+ $([$idx])?),)*
                    _ => None,
                }
            }

            /// [`Theme::slot`] with the built-in palette as the fallback, so
            /// a typo or a stale config key resolves instead of failing.
            pub fn slot_or_default(&self, key: &str) -> Option<Style> {
                self.slot(key).or_else(|| Theme::DEFAULT.slot(key))
            }

            /// Writes one slot by config key, returning the previous value.
            ///
            /// `None` for an unknown key, and nothing is mutated.
            ///
            /// Only colors are settable. Modifiers are what give a role its
            /// meaning (emphasis is italic *because* it is emphasis), so a
            /// color override recolors a slot without stripping what makes
            /// it that role. Components the caller left unset are inherited
            /// from the current value, so a foreground-only override
            /// recolors while leaving any backdrop intact.
            pub fn set_slot(&mut self, key: &str, style: Style) -> Option<Style> {
                match key {
                    $($key => {
                        let merged = merge_style(self.$($field).+ $([$idx])?, style);
                        let prev = self.$($field).+ $([$idx])?;
                        self.$($field).+ $([$idx])? = merged;
                        Some(prev)
                    })*
                    _ => None,
                }
            }
        }
    };
}

/// Layers `over` onto `current`: components present in `over` win,
/// components absent from it are inherited.
///
/// Absence means "unset", which is how an unset field is spelled in both
/// `Style` (`None` / empty `Modifier`) and the config parser (a spec that
/// names no color and no modifier).
fn merge_style(current: Style, over: Style) -> Style {
    let mut out = current;
    if over.fg.is_some() {
        out.fg = over.fg;
    }
    if over.bg.is_some() {
        out.bg = over.bg;
    }
    if !over.add_modifier.is_empty() {
        out.add_modifier |= over.add_modifier;
    }
    if !over.sub_modifier.is_empty() {
        out.sub_modifier |= over.sub_modifier;
    }
    out
}

theme_slots! {
    // [theme.text] - transcript text roles.
    "text.assistant"          => text.assistant;
    "text.user"               => text.user;
    "text.user_border"        => text.user_border;
    "text.tool"               => text.tool;
    "text.error"              => text.error;
    "text.error_emphasis"     => text.error_emphasis;
    "text.muted"              => text.muted;

    // [theme.chrome] - input box and popup.
    "chrome.input_border"     => chrome.input_border;
    "chrome.input_title"      => chrome.input_title;
    "chrome.input_title_busy" => chrome.input_title_busy;
    "chrome.popup_border"     => chrome.popup_border;
    "chrome.popup_title"      => chrome.popup_title;
    "chrome.popup_cursor"     => chrome.popup_cursor;

    // [theme.markdown] - assistant-message markup.
    "markdown.emphasis"       => markdown.emphasis;
    "markdown.strong"         => markdown.strong;
    "markdown.strike"         => markdown.strike;
    "markdown.link_text"      => markdown.link_text;
    "markdown.link_wrap"      => markdown.link_wrap;
    "markdown.rule"           => markdown.rule;
    "markdown.table_border"   => markdown.table_border;
    "markdown.table_header"   => markdown.table_header;
    "markdown.list_marker"    => markdown.list_marker;

    // Blockquote bars, one per nesting depth (depth wraps around).
    "markdown.blockquote_0"   => markdown.blockquote[0];
    "markdown.blockquote_1"   => markdown.blockquote[1];
    "markdown.blockquote_2"   => markdown.blockquote[2];
    "markdown.blockquote_3"   => markdown.blockquote[3];
    "markdown.blockquote_4"   => markdown.blockquote[4];
    "markdown.blockquote_5"   => markdown.blockquote[5];
}

/// Theme surface consumers inject. Kept as a trait (not a bare
/// `Arc<Theme>`) so the provider can swap the active theme at runtime —
/// `/theme`, a config reload — without any consumer changing shape.
///
/// Both methods are cheap: `snapshot` returns a `Copy` value and `epoch`
/// is a single atomic load, so a frame can call them as often as it likes.
pub trait ThemeApi: Send + Sync + 'static {
    /// The theme to draw this frame with.
    fn snapshot(&self) -> Theme;
    /// Same value as `snapshot().epoch`, for consumers that only key a
    /// cache and never read styles.
    fn epoch(&self) -> u64;
}

/// Sized handle around a `dyn ThemeApi`, so the service registry
/// (`inject_key` requires `Sized`) can carry it. Same idiom as
/// [`crate::render::RendererHandle`].
#[derive(Clone)]
pub struct ThemeHandle(pub Arc<dyn ThemeApi>);

impl ThemeHandle {
    /// Borrow the provider as a trait object.
    pub fn as_theme(&self) -> &dyn ThemeApi {
        &*self.0
    }
}

impl std::ops::Deref for ThemeHandle {
    type Target = dyn ThemeApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// A bare `Theme` is itself a valid provider: `Arc::new(Theme::default())`
/// coerces to `Arc<dyn ThemeApi>` with no wrapper. Tests and embedders
/// without a DI context rely on this; the plugin publishes a *swappable*
/// `ThemeService` under the same trait.
impl ThemeApi for Theme {
    fn snapshot(&self) -> Theme {
        *self
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl ThemeApi for ThemeHandle {
    fn snapshot(&self) -> Theme {
        self.0.snapshot()
    }
    fn epoch(&self) -> u64 {
        self.0.epoch()
    }
}

impl<T: ThemeApi + ?Sized> ThemeApi for Arc<T> {
    fn snapshot(&self) -> Theme {
        (**self).snapshot()
    }
    fn epoch(&self) -> u64 {
        (**self).epoch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_the_previous_hardcoded_palette() {
        // Locked to the exact colors the view and markdown engine used
        // before the theme existed, so this refactor is a pure move.
        let t = Theme::default();
        assert_eq!(t.text.assistant, Style::new().fg(Color::White));
        assert_eq!(t.chrome.input_border, Style::new().fg(Color::Blue));
        assert_eq!(
            t.chrome.popup_cursor,
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(t.markdown.list_marker, Style::new().fg(Color::Indexed(222)));
        assert_eq!(t.blockquote(0), Style::new().fg(Color::Indexed(202)));
        assert_eq!(t.blockquote(5), Style::new().fg(Color::Indexed(207)));
        // Depth wraps rather than panicking on a deep nest.
        assert_eq!(t.blockquote(6), t.blockquote(0));
        assert_eq!(t.blockquote(13), t.blockquote(1));
    }

    #[test]
    fn slots_keep_their_modifiers() {
        let t = Theme::default();
        assert!(t.markdown.emphasis.add_modifier.contains(Modifier::ITALIC));
        assert!(t.markdown.strong.add_modifier.contains(Modifier::BOLD));
        assert!(
            t.markdown
                .strike
                .add_modifier
                .contains(Modifier::CROSSED_OUT | Modifier::DIM)
        );
        assert!(t.text.error_emphasis.add_modifier.contains(Modifier::BOLD));
        assert!(!t.text.error.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn default_equals_the_built_in_palette() {
        // A derived Default would be all-empty styles; these must agree.
        assert_eq!(Theme::default(), Theme::DEFAULT);
        assert_eq!(Theme::default().text.assistant, Theme::DEFAULT.text.assistant);
        assert!(Theme::default().markdown.emphasis.fg.is_some());
        assert!(Theme::default().chrome.popup_cursor.bg.is_some());
    }

    #[test]
    fn with_epoch_stamps_a_distinct_version() {
        assert_eq!(Theme::with_epoch(7).epoch, 7);
        assert_eq!(Theme::with_epoch(7).text, Theme::default().text);
        assert_ne!(Theme::with_epoch(7), Theme::with_epoch(8));
    }

    #[test]
    fn code_theme_names_round_trip_and_fall_back() {
        for t in [
            CodeTheme::Base16OceanDark,
            CodeTheme::Base16Eighties,
            CodeTheme::default(),
        ] {
            assert_eq!(CodeTheme::parse(t.as_str()), t);
        }
        assert_eq!(CodeTheme::parse("nope"), CodeTheme::default());
        assert_eq!(CodeTheme::parse("BASE16-Eighties.Dark"), CodeTheme::Base16Eighties);
    }
}
