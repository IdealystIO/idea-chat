//! Idea Chat's look: idea-ui's theme with the design's colours.
//!
//! The shell has three planes, and the theme's three surface tokens are
//! them, so every component and sheet that reads `background`, `surface`
//! and `surface_alt` lands on the right one:
//!
//! | token         | plane                                  | light     | dark      |
//! |---------------|----------------------------------------|-----------|-----------|
//! | `surface_alt` | the space switcher; hover washes       | `#EFEFEC` | `#141518` |
//! | `background`  | navigation and context panels          | `#F7F7F5` | `#1A1B1E` |
//! | `surface`     | the main stage, cards, menus, modals   | `#FFFFFF` | `#212226` |
//!
//! The accent is the design's blue; the design's warm orange — hang up,
//! unread, mentions — is the danger intent, darkened where white text sits
//! on it so it stays legible.

use idea_ui::{color_token, dark_theme, light_theme, IdeaThemeDefaults, IntentColors};

pub fn light() -> IdeaThemeDefaults {
    let mut t = light_theme();
    let c = &mut t.colors;
    c.background = color_token!("color-background", "#F7F7F5");
    c.surface = color_token!("color-surface", "#FFFFFF");
    c.surface_alt = color_token!("color-surface-alt", "#EFEFEC");
    c.text = color_token!("color-text", "#1F1F1D");
    c.text_muted = color_token!("color-text-muted", "#6B6B66");
    c.text_inverse = color_token!("color-text-inverse", "#FFFFFF");
    c.border = color_token!("color-border", "#E6E6E2");
    c.border_hover = color_token!("color-border-hover", "#D6D6D1");
    c.border_strong = color_token!("color-border-strong", "#A8A8A2");
    c.focus_ring = color_token!("color-focus-ring", "#4A78D6");
    c.overlay = color_token!("color-overlay", "rgba(40, 40, 36, 0.32)");
    t.intents.primary = IntentColors {
        solid_bg: color_token!("intent-primary-solid-bg", "#4A78D6"),
        solid_text: color_token!("intent-primary-solid-text", "#FFFFFF"),
        soft_bg: color_token!("intent-primary-soft-bg", "#DDE6F7"),
        soft_text: color_token!("intent-primary-soft-text", "#2B529F"),
        fg: color_token!("intent-primary-fg", "#3A66C4"),
        border: color_token!("intent-primary-border", "#A9C0EA"),
    };
    t.intents.danger = IntentColors {
        solid_bg: color_token!("intent-danger-solid-bg", "#BF5B26"),
        solid_text: color_token!("intent-danger-solid-text", "#FFFFFF"),
        soft_bg: color_token!("intent-danger-soft-bg", "#F8E6DA"),
        soft_text: color_token!("intent-danger-soft-text", "#8F4119"),
        fg: color_token!("intent-danger-fg", "#B4541F"),
        border: color_token!("intent-danger-border", "#EBC4A3"),
    };
    t.intents.neutral = IntentColors {
        solid_bg: color_token!("intent-neutral-solid-bg", "#3A3A37"),
        solid_text: color_token!("intent-neutral-solid-text", "#FFFFFF"),
        soft_bg: color_token!("intent-neutral-soft-bg", "#EDEDEA"),
        soft_text: color_token!("intent-neutral-soft-text", "#3A3A37"),
        fg: color_token!("intent-neutral-fg", "#55554F"),
        border: color_token!("intent-neutral-border", "#DCDCD7"),
    };
    t
}

pub fn dark() -> IdeaThemeDefaults {
    let mut t = dark_theme();
    let c = &mut t.colors;
    c.background = color_token!("color-background", "#1A1B1E");
    c.surface = color_token!("color-surface", "#212226");
    c.surface_alt = color_token!("color-surface-alt", "#141518");
    c.text = color_token!("color-text", "#ECECEA");
    c.text_muted = color_token!("color-text-muted", "#A2A29D");
    c.text_inverse = color_token!("color-text-inverse", "#141518");
    c.border = color_token!("color-border", "#2E2F33");
    c.border_hover = color_token!("color-border-hover", "#3B3C41");
    c.border_strong = color_token!("color-border-strong", "#5E5F64");
    c.focus_ring = color_token!("color-focus-ring", "#7C9CF0");
    c.overlay = color_token!("color-overlay", "rgba(0, 0, 0, 0.55)");
    t.intents.primary = IntentColors {
        solid_bg: color_token!("intent-primary-solid-bg", "#4A78D6"),
        solid_text: color_token!("intent-primary-solid-text", "#FFFFFF"),
        soft_bg: color_token!("intent-primary-soft-bg", "#26334F"),
        soft_text: color_token!("intent-primary-soft-text", "#B9CCF5"),
        fg: color_token!("intent-primary-fg", "#86A5F0"),
        border: color_token!("intent-primary-border", "#3C5486"),
    };
    t.intents.danger = IntentColors {
        solid_bg: color_token!("intent-danger-solid-bg", "#C2602B"),
        solid_text: color_token!("intent-danger-solid-text", "#FFFFFF"),
        soft_bg: color_token!("intent-danger-soft-bg", "#3B2619"),
        soft_text: color_token!("intent-danger-soft-text", "#F2B48F"),
        fg: color_token!("intent-danger-fg", "#E88F5C"),
        border: color_token!("intent-danger-border", "#6B3A1F"),
    };
    t.intents.neutral = IntentColors {
        solid_bg: color_token!("intent-neutral-solid-bg", "#D8D8D4"),
        solid_text: color_token!("intent-neutral-solid-text", "#141518"),
        soft_bg: color_token!("intent-neutral-soft-bg", "#2A2B2F"),
        soft_text: color_token!("intent-neutral-soft-text", "#D8D8D4"),
        fg: color_token!("intent-neutral-fg", "#B5B5B0"),
        border: color_token!("intent-neutral-border", "#3B3C41"),
    };
    t
}
