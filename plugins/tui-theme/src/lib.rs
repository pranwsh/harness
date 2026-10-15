//! Theme provider: publishes the active [`Theme`] for every TUI consumer.
//!
//! This crate owns *palette selection* and nothing else. The vocabulary it
//! publishes (which roles exist, which modifiers each carries) lives in
//! `harness_tui_state::theme`; the colors come from here. Keeping the two
//! apart is what lets any plugin ask for `theme.text.error` without
//! depending on this provider — the crate graph stays acyclic and a
//! different theme can be swapped in by replacing this one plugin.
//!
//! Resolution order, first hit wins:
//! 1. `[theme]` in `config.toml` (optional; see [`ThemeConfig`]).
//! 2. The built-in [`Theme::DEFAULT`].
//!
//! Injects `KEY_CONFIG`, so it parks until config is up and needs no ctor
//! wiring at the composition root.

use std::sync::{Arc, RwLock};

use harness_contracts::{AppConfig, CodeThemeConfig, KEY_CONFIG, KEY_THEME, ThemeConfig};
use harness_core::{Context, Result};
use harness_tui_state::theme::{CodeTheme, Theme, ThemeApi, ThemeHandle};
use ratatui::style::{Color, Style};

/// Owns the active theme. Reads are a `Copy` load with no lock, so a
/// draw can snapshot per frame; writes (`set`) are lock-guarded and bump
/// [`Theme::epoch`] so downstream render caches invalidate.
///
/// Holds an `ArcSwap` only if writes become frequent; today a theme
/// changes at most a few times per session (a `/theme` command, a config
/// reload), so a `RwLock` costs less and keeps the write path obvious.
pub struct ThemeService {
    active: RwLock<Theme>,
}

impl ThemeService {
    /// Publishes [`Theme::DEFAULT`] stamped as epoch 1. Epoch starts at 1
    /// rather than 0 so "never swapped" and "swapped once" are
    /// distinguishable to a cache that starts pre-populated.
    pub fn default_theme() -> Self {
        ThemeService::with_theme(Theme::DEFAULT)
    }

    /// Publishes `theme`, stamped with the next epoch. Returns the stored
    /// snapshot so a caller can log or assert on it.
    pub fn with_theme(theme: Theme) -> Self {
        ThemeService {
            active: RwLock::new(Theme { epoch: 1, ..theme }),
        }
    }

    /// Replaces the active theme, bumping `epoch` so caches that baked
    /// styles into rows discard them. No-op swap detection: re-setting an
    /// identical palette still bumps, which is cheap and keeps the
    /// invariant simple ("epoch is monotonic").
    pub fn set(&self, theme: Theme) {
        let mut active = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let epoch = active.epoch.wrapping_add(1);
        *active = Theme { epoch, ..theme };
    }

    /// Applies `[theme]` overrides onto the default palette. An absent
    /// section yields `Theme::DEFAULT` unchanged.
    pub fn from_config(config: &ThemeConfig) -> Self {
        ThemeService::with_theme(theme_from(config).0)
    }

    /// [`ThemeService::from_config`] plus the warnings, for a provider that
    /// wants to surface bad config at startup.
    pub fn from_config_checked(config: &ThemeConfig) -> (Self, Vec<ThemeWarning>) {
        let (theme, warnings) = theme_from(config);
        (ThemeService::with_theme(theme), warnings)
    }
}

impl ThemeApi for ThemeService {
    fn snapshot(&self) -> Theme {
        *self.active.read().unwrap_or_else(|e| e.into_inner())
    }

    fn epoch(&self) -> u64 {
        self.snapshot().epoch
    }
}

/// Plugin publishing the theme under [`KEY_THEME`] as an `Arc<ThemeHandle>`.
/// Any plugin can also provide this key to swap palettes wholesale
/// without touching the TUI shell.
pub struct ThemePlugin;

impl harness_core::Plugin for ThemePlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("theme").provides(KEY_THEME).injects(KEY_CONFIG)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        // Config is injected but the theme is optional: a config service
        // with no `[theme]` simply yields the default palette. Reading it
        // here (rather than taking an `AppConfig` argument) keeps the
        // plugin free of ctor wiring at the composition root.
        let config: Arc<harness_contracts::ConfigHandle> = ctx.inject_key(KEY_CONFIG)?;
        let (service, warnings) =
            ThemeService::from_config_checked(&theme_from_app_config(&config.get()));
        // Bad theme config is the user's typo, not a fatal error: report it
        // once on stderr and keep every slot we *could* resolve.
        for w in &warnings {
            eprintln!("harness: theme: {}: {}", w.key, w.reason);
        }
        ctx.provide_key(KEY_THEME, Arc::new(ThemeHandle(Arc::new(service))));
        Ok(())
    }
}

/// Reads `[theme]` out of a full app config. Kept as a free function so
/// the deserialization shape stays in `contracts` while the mapping onto a
/// [`Theme`] lives here, next to the rest of the provider.
fn theme_from_app_config(config: &AppConfig) -> ThemeConfig {
    config.theme.clone()
}

/// One slot override that could not be applied, reported to the user
/// instead of silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeWarning {
    /// The `[theme]` key as written.
    pub key: String,
    /// Why it was rejected.
    pub reason: String,
}

/// The palette described by `[theme]`: the built-in default with every
/// present key applied.
///
/// Degradation is deliberate and total: an unknown code-theme name falls
/// back to the default, an unparseable color or an unknown slot key leaves
/// that slot on its built-in value, and each is collected in the returned
/// warnings. Nothing here can cost the user their UI — a typo costs one
/// slot and one line of stderr.
pub fn theme_from(config: &ThemeConfig) -> (Theme, Vec<ThemeWarning>) {
    let mut theme = Theme::with_epoch(1);
    let mut warnings = Vec::new();

    if let Some(name) = &config.code.name {
        theme.code.theme = CodeTheme::parse(name);
    }

    for (key, spec) in &config.colors {
        // An unknown slot is a config/UI-version mismatch, not a color
        // problem: report it (so the user learns the key is dead) but keep
        // going.
        if Theme::DEFAULT.slot(key).is_none() {
            warnings.push(ThemeWarning {
                key: key.clone(),
                reason: "unknown theme slot (check the key spelling)".to_owned(),
            });
            continue;
        }
        let mut style = Style::default();
        let mut ok = true;
        // An empty spelling means "not specified", so it is skipped rather
        // than reported: a half-written override leaves the slot on its
        // working built-in, silently and without a spurious warning.
        let named = named_component;
        if let Some(spelling) = named(spec.fg()) {
            match parse_color(spelling) {
                Some(c) => style = style.fg(c),
                None => {
                    ok = false;
                    warnings.push(color_warning(key, "fg", spelling));
                }
            }
        }
        if let Some(spelling) = named(spec.bg()) {
            match parse_color(spelling) {
                Some(c) => style = style.bg(c),
                None => {
                    ok = false;
                    warnings.push(color_warning(key, "bg", spelling));
                }
            }
        }
        // A spec that named nothing (`""`) leaves the slot untouched: an
        // empty override must never blank a color that already works.
        if ok && (style.fg.is_some() || style.bg.is_some()) {
            theme.set_slot(key, style);
        }
    }

    (theme, warnings)
}

/// A color component that was not specified at all (absent, or blank).
///
/// Blank means "leave this slot alone": a half-written override must not
/// blank a color that already works, nor warn about it.
fn named_component(spec: Option<&str>) -> Option<&str> {
    spec.filter(|c| !c.trim().is_empty())
}

fn color_warning(key: &str, component: &str, spelling: &str) -> ThemeWarning {
    ThemeWarning {
        key: key.to_owned(),
        reason: format!(
            "{component}: {spelling:?} is not a color (try \"red\", \"240\", or \"#a1b2c3\")"
        ),
    }
}

/// Parses a color spelling: an ANSI name, a 256-color index, or a `#rrggbb`
/// hex triple. `None` when unparseable — the caller keeps the built-in.
///
/// ratatui already parses all three forms via `FromStr`, hex included, so
/// this delegates rather than reimplementing: the terminal is the source of
/// truth for what `LightCyan` means, and a future ratatui spelling keeps
/// working here for free.
fn parse_color(spelling: &str) -> Option<Color> {
    let text = spelling.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<Color>().ok()
}

/// Marker so the config types used by this plugin's public surface appear
/// in its docs and stay reachable without a direct `contracts` import.
pub type CodeConfig = CodeThemeConfig;

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{AppConfig, ConfigHandle};
    use ratatui::style::{Color, Modifier, Style};

    fn config(toml_body: &str) -> AppConfig {
        let raw = format!(
            "[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n{toml_body}"
        );
        AppConfig::from_toml(&raw, "<test>").expect("config parses")
    }

    fn service(app: &AppConfig) -> ThemeService {
        ThemeService::from_config(&app.theme)
    }

    #[test]
    fn absent_theme_section_yields_the_default_palette() {
        let s = service(&config(""));
        assert_eq!(s.snapshot().text, Theme::DEFAULT.text);
        assert_eq!(s.snapshot().markdown, Theme::DEFAULT.markdown);
        assert_eq!(s.snapshot().chrome, Theme::DEFAULT.chrome);
    }

    #[test]
    fn code_theme_is_overridable_from_config() {
        let s = service(&config("[theme.code]\nname = \"base16-eighties.dark\"\n"));
        assert_eq!(s.snapshot().code.theme.as_str(), "base16-eighties.dark");
    }

    #[test]
    fn unknown_code_theme_degrades_instead_of_failing() {
        // A typo must cost one slot, never the whole UI.
        let s = service(&config("[theme.code]\nname = \"totally-made-up\"\n"));
        assert_eq!(
            s.snapshot().code.theme.as_str(),
            Theme::DEFAULT.code.theme.as_str()
        );
    }

    #[test]
    fn setting_a_theme_bumps_the_epoch() {
        let s = ThemeService::default_theme();
        let before = s.snapshot().epoch;
        let mut next = Theme::DEFAULT;
        next.text.error = Style::new().fg(Color::Magenta);
        s.set(next);
        assert!(s.snapshot().epoch > before, "epoch must advance");
        assert_eq!(s.snapshot().text.error.fg, Some(Color::Magenta));
    }

    #[test]
    fn snapshots_are_consistent_with_the_epoch() {
        let s = ThemeService::default_theme();
        s.set(Theme::with_epoch(99));
        assert_eq!(s.snapshot().epoch, s.epoch());
    }

    #[test]
    fn plugin_publishes_the_theme_through_di() {
        let ctx = Context::root();
        ctx.load(ThemePlugin).unwrap();
        // Theme parks until config appears, then cascades in.
        assert!(ctx.inject_key::<ThemeHandle>(KEY_THEME).is_err());
        ctx.load(
            harness_config::ConfigPlugin::from_toml(
                "[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n[theme.code]\nname=\"base16-eighties.dark\"\n",
            )
            .unwrap(),
        )
        .unwrap();

        let theme: Arc<ThemeHandle> = ctx.inject_key(KEY_THEME).unwrap();
        assert_eq!(theme.snapshot().code.theme.as_str(), "base16-eighties.dark");
        assert_eq!(theme.snapshot().text.assistant, Theme::DEFAULT.text.assistant);
        // The injected config is the one it read its palette from.
        let handle: Arc<ConfigHandle> = ctx.inject_key(KEY_CONFIG).unwrap();
        assert_eq!(handle.get().theme.code.name.as_deref(), Some("base16-eighties.dark"));
    }

    #[test]
    fn published_theme_carries_the_default_modifiers() {
        // The palette is roles *plus* the modifiers their role implies:
        // emphasis must not lose its italic just because it was re-themed.
        let s = service(&config(""));
        let t = s.snapshot();
        assert!(t.markdown.emphasis.add_modifier.contains(Modifier::ITALIC));
        assert!(t.markdown.strong.add_modifier.contains(Modifier::BOLD));
        assert!(t.chrome.input_title.add_modifier.contains(Modifier::BOLD));
        assert!(t.chrome.popup_cursor.bg.is_some());
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;
    use harness_contracts::ColorSpec;
    use ratatui::style::Modifier;

    fn cfg(body: &str) -> ThemeConfig {
        let raw = format!(
            "[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n{body}"
        );
        AppConfig::from_toml(&raw, "<t>").expect("parses").theme
    }

    #[test]
    fn nested_table_form_applies() {
        let c = cfg(
            "[theme.colors]\n\"text.user_border\" = \"magenta\"\n\"chrome.input_border\" = \"green\"\n",
        );
        let (t, w) = theme_from(&c);
        assert!(w.is_empty(), "warnings: {w:?}");
        assert_eq!(t.text.user_border.fg, Some(Color::Magenta));
        assert_eq!(t.chrome.input_border.fg, Some(Color::Green));
    }

    #[test]
    fn detailed_form_sets_fg_and_bg() {
        let c = cfg("[theme.colors]\n\"chrome.popup_cursor\" = { fg = \"black\", bg = \"white\" }\n");
        let (t, w) = theme_from(&c);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.chrome.popup_cursor.fg, Some(Color::Black));
        assert_eq!(t.chrome.popup_cursor.bg, Some(Color::White));
    }

    #[test]
    fn unspecified_components_inherit() {
        // Overriding only the fg must not drop an existing bg.
        let c = cfg("[theme.colors]\n\"markdown.link_text\" = { fg = \"yellow\" }\n");
        let (t, _) = theme_from(&c);
        assert_eq!(t.markdown.link_text.fg, Some(Color::Yellow));
        assert_eq!(
            t.markdown.link_text.bg,
            Theme::DEFAULT.markdown.link_text.bg,
            "background must survive a foreground-only override"
        );
    }

    #[test]
    fn modifiers_survive_a_recolor() {
        // The whole point of roles: recoloring must not strip the meaning.
        let c = cfg("[theme.colors]\n\"markdown.emphasis\" = \"cyan\"\n\"text.error_emphasis\" = \"bright red\"\n");
        let (t, _) = theme_from(&c);
        assert!(t.markdown.emphasis.add_modifier.contains(Modifier::ITALIC));
        assert!(t.markdown.strong.add_modifier.contains(Modifier::BOLD));
        assert!(t.text.error_emphasis.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn accepts_names_indices_and_hex() {
        let c = cfg(concat!(
            "[theme.colors]\n",
            "\"text.assistant\" = \"light cyan\"\n",
            "\"text.tool\" = \"240\"\n",
            "\"text.muted\" = \"#a1b2c3\"\n",
        ));
        let (t, w) = theme_from(&c);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.text.assistant.fg, Some(Color::LightCyan));
        assert_eq!(t.text.tool.fg, Some(Color::Indexed(240)));
        assert_eq!(t.text.muted.fg, Some(Color::Rgb(0xa1, 0xb2, 0xc3)));
    }

    #[test]
    fn blockquote_ramp_is_individually_configurable() {
        let c = cfg("[theme.colors]\n\"markdown.blockquote_0\" = \"1\"\n\"markdown.blockquote_3\" = \"2\"\n");
        let (t, w) = theme_from(&c);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.blockquote(0).fg, Some(Color::Indexed(1)));
        assert_eq!(t.blockquote(3).fg, Some(Color::Indexed(2)));
        // Untouched depths keep the built-in ramp.
        assert_eq!(t.blockquote(1), Theme::DEFAULT.blockquote(1));
    }

    #[test]
    fn bad_color_warns_and_keeps_the_builtin() {
        let c = cfg("[theme.colors]\n\"text.error\" = \"not-a-color\"\n");
        let (t, w) = theme_from(&c);
        assert_eq!(t.text.error, Theme::DEFAULT.text.error, "slot untouched");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].key, "text.error");
        assert!(w[0].reason.contains("not a color"), "{:?}", w[0].reason);
    }

    #[test]
    fn unknown_slot_warns_but_does_not_panic() {
        let c = cfg("[theme.colors]\n\"text.nonexistent\" = \"red\"\n\"text.error\" = \"red\"\n");
        let (t, w) = theme_from(&c);
        assert_eq!(w.len(), 1, "one warning for the bad key only: {w:?}");
        assert!(w[0].reason.contains("unknown theme slot"));
        // The good key in the same table still applied.
        assert_eq!(t.text.error.fg, Some(Color::Red));
    }

    #[test]
    fn empty_spec_leaves_the_slot_alone() {
        let c = cfg("[theme.colors]\n\"text.assistant\" = \"\"\n");
        let (t, w) = theme_from(&c);
        assert_eq!(t.text.assistant, Theme::DEFAULT.text.assistant);
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn code_theme_still_works_alongside_colors() {
        let c = cfg(concat!(
            "[theme.code]\nname = \"base16-eighties.dark\"\n",
            "[theme.colors]\n\"text.user\" = \"white\"\n",
        ));
        let (t, w) = theme_from(&c);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.code.theme.as_str(), "base16-eighties.dark");
        assert_eq!(t.text.user.fg, Some(Color::White));
    }

    #[test]
    fn every_registered_slot_is_addressable_from_config() {
        // Guards the registry against drift: each declared slot must be
        // settable by exactly the key the registry advertises.
        for (key, get) in harness_tui_state::theme::THEME_SLOTS {
            let raw = format!("[theme.colors]\n{key:?} = \"77\"\n");
            let app = AppConfig::from_toml(
                &format!("[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n{raw}"),
                "<t>",
            )
            .unwrap_or_else(|e| panic!("{key} must be a valid TOML key: {e}"));
            let (t, w) = theme_from(&app.theme);
            assert!(w.is_empty(), "{key}: {w:?}");
            assert_eq!(
                get(&t).fg,
                Some(Color::Indexed(77)),
                "{key} was not applied"
            );
        }
    }

    #[test]
    fn a_real_config_file_recolors_the_slots_the_user_cares_about() {
        // End-to-end through the same path startup uses: file -> AppConfig
        // -> Theme. Spans the two concerns from the original ask (input box,
        // message outline) plus the fiddly cases (bg, hex, blockquote ramp,
        // code theme) and confirms untouched slots are left alone.
        let path = std::path::Path::new("/tmp/harness-theme-e2e.toml");
        std::fs::write(
            path,
            r##"[llm]
base_url = "https://example.com"
model = "m"
api_key = "k"

[theme.colors]
"text.user_border" = "magenta"
"chrome.input_border" = "green"
"text.error" = { fg = "light red", bg = "black" }
"markdown.emphasis" = "#7fb3ff"
"markdown.blockquote_0" = "33"

[theme.code]
name = "base16-eighties.dark"
"##,
        )
        .expect("write config");
        let app = AppConfig::from_file(path.to_str().unwrap()).expect("parse config");
        std::fs::remove_file(path).ok();

        let (t, w) = theme_from(&app.theme);
        assert!(w.is_empty(), "no warnings: {w:?}");

        // The two the user named.
        assert_eq!(t.chrome.input_border.fg, Some(Color::Green), "input box");
        assert_eq!(t.text.user_border.fg, Some(Color::Magenta), "message outline");

        // A set background, and a hex color.
        assert_eq!(t.text.error.bg, Some(Color::Black));
        assert_eq!(t.text.error.fg, Some(Color::LightRed));
        assert_eq!(t.markdown.emphasis.fg, Some(Color::Rgb(0x7f, 0xb3, 0xff)));

        // A role keeps its meaning through a recolor.
        assert!(t.markdown.emphasis.add_modifier.contains(Modifier::ITALIC));

        // Ramp + code theme + untouched slot.
        assert_eq!(t.blockquote(0).fg, Some(Color::Indexed(33)));
        assert_eq!(t.blockquote(1), Theme::DEFAULT.blockquote(1));
        assert_eq!(t.code.theme.as_str(), "base16-eighties.dark");
        assert_eq!(t.text.tool, Theme::DEFAULT.text.tool);
    }

    #[test]
    fn a_bad_color_costs_one_slot_not_the_ui() {
        // The degradation contract: a typo is reported, every other slot in
        // the same file still applies, and the bad one keeps a working color.
        let c = cfg(concat!(
            "[theme.colors]\n",
            "\"chrome.input_border\" = \"chartreuse\"\n",
            "\"text.user_border\" = \"nonsense-color\"\n",
            "\"text.assistant\"  = \"white\"\n",
        ));
        let (t, w) = theme_from(&c);
        // Only the name ratatui knows applies. ("chartreuse" is not a
        // ratatui color name, so it is an unparseable value like any other
        // typo -- it warns and the slot keeps its built-in, exactly like
        // "nonsense-color" below.)
        assert_eq!(t.chrome.input_border, Theme::DEFAULT.chrome.input_border);
        assert_eq!(t.text.assistant.fg, Some(Color::White), "later keys still apply");
        assert_eq!(t.text.user_border, Theme::DEFAULT.text.user_border, "kept working color");
        assert_eq!(w.len(), 2, "both unparseable values warned: {w:?}");
        assert!(w.iter().any(|x| x.key == "text.user_border"));
        assert!(w.iter().any(|x| x.key == "chrome.input_border"));
    }

    #[test]
    fn color_spec_short_and_long_forms() {
        let simple = ColorSpec::Simple("red".into());
        assert_eq!(simple.fg(), Some("red"));
        assert_eq!(simple.bg(), None, "a bare string is foreground-only");
        let detailed = ColorSpec::Detailed {
            fg: Some("red".into()),
            bg: Some("blue".into()),
        };
        assert_eq!(detailed.fg(), Some("red"));
        assert_eq!(detailed.bg(), Some("blue"));
    }
}
