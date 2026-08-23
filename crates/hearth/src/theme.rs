//! Hearth's palette.
//!
//! This is a *theme*, not a component library — every value here lands in
//! idea-ui's own token vocabulary (`color-*`, `intent-*-*`, `spacing-*`,
//! `radius-*`, `typography-*-size`), so idea-ui's components pick it up with
//! no per-component styling and the app's own `stylesheet!` chrome reads the
//! same names. The design ships four accents; picking one only rewrites the
//! `primary` intent, which is exactly the seam the token vocabulary gives us.
//!
//! Two deliberate departures from `idea_ui::light_theme()` / `dark_theme()`:
//!
//! - **The dark scheme is the reference.** Hearth is a place you leave open
//!   all evening. Its dark surfaces are warmer than idea-ui's blue-slate
//!   defaults (a touch of red in the neutrals) so a room full of avatars
//!   doesn't read as a dashboard.
//! - **Tighter type and radii.** A chat client shows far more rows per screen
//!   than a form does; the body scale drops a step and the radius scale
//!   softens, which is what makes the message list read as conversation
//!   rather than as a table.

use idea_ui::theme::{Colors, IdeaThemeDefaults, Intents, Radius, Spacing, Typography};
use idea_ui::{dark_theme, light_theme};
use runtime_core::{Color, Tokenized};

/// The accents the design offers. `Indigo` is the default in `Hearth.dc.html`;
/// the other three are the same design's alternates, kept because switching
/// accent is a one-token change and there is no reason to drop the axis.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Accent {
    Indigo,
    Teal,
    Amber,
    Violet,
}

impl Accent {
    /// The accent's solid fill — what a `primary_filled` Button paints with.
    fn solid(self) -> &'static str {
        match self {
            Accent::Indigo => "#4f46e5",
            Accent::Teal => "#0f766e",
            Accent::Amber => "#b45309",
            Accent::Violet => "#7c3aed",
        }
    }

    /// Soft wash + soft text, per scheme. Hand-picked rather than derived:
    /// a programmatic tint of the solid colour lands at the wrong luminance
    /// on the warm accents (amber especially), and these are read behind
    /// body text where contrast is not negotiable.
    fn soft(self, dark: bool) -> (&'static str, &'static str) {
        match (self, dark) {
            (Accent::Indigo, false) => ("#eef1ff", "#3730a3"),
            (Accent::Indigo, true) => ("#1e2140", "#c3c7ff"),
            (Accent::Teal, false) => ("#e6f5f2", "#115e56"),
            (Accent::Teal, true) => ("#123330", "#9ee3d8"),
            (Accent::Amber, false) => ("#fdf3e3", "#8a4408"),
            (Accent::Amber, true) => ("#3a2611", "#f5c88a"),
            (Accent::Violet, false) => ("#f3ecff", "#5b21b6"),
            (Accent::Violet, true) => ("#2a1d45", "#d5bcff"),
        }
    }

    /// The accent as text/icon colour on a plain surface. On dark schemes the
    /// solid fill is too dim to read as a foreground, so it lifts.
    fn fg(self, dark: bool) -> &'static str {
        match (self, dark) {
            (_, false) => self.solid(),
            (Accent::Indigo, true) => "#8b87ff",
            (Accent::Teal, true) => "#4dc4b4",
            (Accent::Amber, true) => "#e2a04a",
            (Accent::Violet, true) => "#a986ff",
        }
    }

    /// Border tint for soft/outlined surfaces.
    fn border(self, dark: bool) -> &'static str {
        match (self, dark) {
            (Accent::Indigo, false) => "#c7cbfe",
            (Accent::Indigo, true) => "#3a3b74",
            (Accent::Teal, false) => "#a7ded6",
            (Accent::Teal, true) => "#1f5750",
            (Accent::Amber, false) => "#f2d5a6",
            (Accent::Amber, true) => "#5f4220",
            (Accent::Violet, false) => "#d8c6ff",
            (Accent::Violet, true) => "#4a3475",
        }
    }
}

/// `Tokenized::token` with the canonical name, so a value written here is
/// reachable from a `stylesheet!` as `t.color.background()` and from the
/// design's CSS as `var(--color-background)` — one fact, two spellings.
fn c(name: &'static str, value: &str) -> Tokenized<Color> {
    Tokenized::token(name, Color(value.to_string()))
}

/// Hearth's spacing. Identical steps to idea-ui's defaults — the design's
/// `--spacing-*` usage assumes 4/8/12/16/24/32 and every panel width in
/// `Hearth.dc.html` is measured against them.
fn spacing() -> Spacing {
    Spacing { xs: 4.0, sm: 8.0, md: 12.0, lg: 16.0, xl: 24.0, xxl: 32.0 }
}

/// Softer than idea-ui's 4/8/12. The design's own boxes sit at 8–16px and
/// its room rows at 8, which is the shape a list of names wants.
fn radius() -> Radius {
    Radius { sm: 6.0, md: 10.0, lg: 14.0 }
}

/// The design's type scale, verbatim from `tokens.css`: a chat client is
/// mostly `body`, so the interesting end of this scale is the small end.
fn typography() -> Typography {
    Typography {
        display_size: 40.0,
        h1_size: 32.0,
        h2_size: 24.0,
        h3_size: 19.0,
        body_xl_size: 18.0,
        body_lg_size: 16.0,
        body_size: 14.0,
        body_sm_size: 13.0,
        caption_size: 12.0,
        overline_size: 11.0,
    }
}

/// Rewrite the `primary` intent to the chosen accent, leaving the other six
/// intents (success/danger/warning/info/secondary/neutral) as idea-ui shipped
/// them — those carry *meaning*, not brand, and re-tinting them to match an
/// accent is how a design system stops being able to say "this went wrong".
fn with_accent(mut intents: Intents, accent: Accent, dark: bool) -> Intents {
    let (soft_bg, soft_text) = accent.soft(dark);
    intents.primary.solid_bg = c("intent-primary-solid-bg", accent.solid());
    intents.primary.solid_text = c("intent-primary-solid-text", "#ffffff");
    intents.primary.soft_bg = c("intent-primary-soft-bg", soft_bg);
    intents.primary.soft_text = c("intent-primary-soft-text", soft_text);
    intents.primary.fg = c("intent-primary-fg", accent.fg(dark));
    intents.primary.border = c("intent-primary-border", accent.border(dark));
    intents
}

/// The light scheme — paper, not glare. `background` is a hair warm so the
/// white `surface` panels (room list, composer, cards) separate from it
/// without needing a border everywhere.
pub fn hearth_light(accent: Accent) -> IdeaThemeDefaults {
    let base = light_theme();
    IdeaThemeDefaults {
        colors: Colors {
            background: c("color-background", "#f7f6f4"),
            surface: c("color-surface", "#ffffff"),
            surface_alt: c("color-surface-alt", "#efedea"),

            text: c("color-text", "#1c1917"),
            text_muted: c("color-text-muted", "#78716c"),
            text_inverse: c("color-text-inverse", "#ffffff"),

            border: c("color-border", "#e5e1dc"),
            border_hover: c("color-border-hover", "#d3cec7"),
            border_strong: c("color-border-strong", "#a8a29e"),

            focus_ring: c("color-focus-ring", accent.solid()),
            overlay: c("color-overlay", "rgba(28, 25, 23, 0.45)"),
        },
        intents: with_accent(base.intents, accent, false),
        spacing: spacing(),
        radius: radius(),
        typography: typography(),
        font: base.font,
    }
}

/// The dark scheme — the one Hearth is designed in. Warm charcoal rather than
/// blue-black: `surface` is the room list and composer, `surface_alt` is the
/// hover/active wash and the live-room backdrop, `background` is the message
/// column. Keeping those three distinct is what lets the whole app work with
/// almost no borders.
pub fn hearth_dark(accent: Accent) -> IdeaThemeDefaults {
    let base = dark_theme();
    IdeaThemeDefaults {
        colors: Colors {
            background: c("color-background", "#16151a"),
            surface: c("color-surface", "#1c1b21"),
            surface_alt: c("color-surface-alt", "#26252d"),

            text: c("color-text", "#ece9e6"),
            text_muted: c("color-text-muted", "#9a938d"),
            text_inverse: c("color-text-inverse", "#16151a"),

            border: c("color-border", "#2e2c35"),
            border_hover: c("color-border-hover", "#3d3a45"),
            border_strong: c("color-border-strong", "#5b5661"),

            focus_ring: c("color-focus-ring", accent.fg(true)),
            overlay: c("color-overlay", "rgba(8, 7, 10, 0.66)"),
        },
        intents: with_accent(base.intents, accent, true),
        spacing: spacing(),
        radius: radius(),
        typography: typography(),
        font: base.font,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idea_ui::theme::IdeaTheme;

    /// The accent axis must reach the `primary` intent and *only* it —
    /// re-tinting `danger` to match a brand colour is how a palette loses the
    /// ability to signal failure.
    #[test]
    fn accent_rewrites_primary_and_leaves_semantic_intents_alone() {
        let indigo = hearth_dark(Accent::Indigo);
        let amber = hearth_dark(Accent::Amber);

        assert_ne!(
            indigo.intents().primary.solid_bg.value(),
            amber.intents().primary.solid_bg.value(),
        );
        assert_eq!(
            indigo.intents().danger.solid_bg.value(),
            amber.intents().danger.solid_bg.value(),
        );
        assert_eq!(
            indigo.intents().success.solid_bg.value(),
            amber.intents().success.solid_bg.value(),
        );
    }

    /// Every colour Hearth writes must carry its canonical token name, or the
    /// value lands as a literal that no theme swap can reach — the failure
    /// mode is a panel that stays light when the app goes dark.
    #[test]
    fn every_color_carries_its_canonical_token_name() {
        for theme in [hearth_light(Accent::Indigo), hearth_dark(Accent::Indigo)] {
            let c = theme.colors();
            for (name, tok) in [
                ("color-background", &c.background),
                ("color-surface", &c.surface),
                ("color-surface-alt", &c.surface_alt),
                ("color-text", &c.text),
                ("color-text-muted", &c.text_muted),
                ("color-border", &c.border),
                ("color-focus-ring", &c.focus_ring),
            ] {
                assert_eq!(tok.name(), Some(name), "{name} lost its token name");
            }
        }
    }

    /// The two schemes must actually differ on the surfaces the layout leans
    /// on; a dark theme that shares `surface` with its light peer renders as
    /// light-mode panels floating on a dark page.
    #[test]
    fn light_and_dark_separate_all_three_surface_layers() {
        let l = hearth_light(Accent::Indigo);
        let d = hearth_dark(Accent::Indigo);
        for (lv, dv) in [
            (&l.colors().background, &d.colors().background),
            (&l.colors().surface, &d.colors().surface),
            (&l.colors().surface_alt, &d.colors().surface_alt),
            (&l.colors().text, &d.colors().text),
        ] {
            assert_ne!(lv.value(), dv.value());
        }

        // …and within one scheme the three layers must be distinct from each
        // other, which is what lets the app work with almost no borders.
        for theme in [&l, &d] {
            let c = theme.colors();
            assert_ne!(c.background.value(), c.surface.value());
            assert_ne!(c.surface.value(), c.surface_alt.value());
        }
    }
}
