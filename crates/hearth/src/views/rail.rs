//! The spaces rail — the 70px column that switches between direct messages
//! and each space, with the theme and settings controls pinned to the bottom.
//!
//! Nothing in here is wrapped in a `switch`. The rail's *shape* never changes:
//! the spaces are a static table, and the only thing that varies is which entry
//! is selected. So selection is read inside each node's style closure and the
//! backend re-tints the two affected nodes in place, rather than rebuilding the
//! column. That is the idiom to reach for first — a `switch` belongs where a
//! subtree's shape genuinely differs, not where its colours do.

use icons_lucide::{MESSAGE_CIRCLE, PLUS, SETTINGS, SUN};
use idea_ui::{tone, variant, IconButton, IconButtonSize, VariantRef};
use runtime_core::{component, icon, rx, text, ui, Element, StyleApplication};

use crate::model::{App, Scope, SPACES};
use crate::styles;
use crate::views::bits::{cb, press};

/// One slot in the rail: the selection pip and the round button beside it.
///
/// `selected` is a closure, not a `bool`: it's called inside the pip's style
/// sink, so the pip grows and shrinks on its own when the selection moves.
///
/// The two children are assembled here rather than written as macro siblings —
/// a childless tag followed by a braced `{ button }` is parsed as that tag's
/// children block, which nested the button inside the absolutely-positioned
/// pip and collapsed the slot to its padding.
fn slot(selected: impl Fn() -> bool + 'static, button: Element) -> Element {
    let pip: Element = runtime_core::view(Vec::new())
        .with_style(move || {
            StyleApplication::new(styles::RailPip::sheet())
                .with("state", if selected() { "active" } else { "idle" })
        })
        .into();
    let children = vec![pip, button];
    ui! {
        view(style = || StyleApplication::new(styles::RailSlot::sheet())) {
            children
        }
    }
}

#[component]
pub fn Rail(#[prop(static)] app: App) -> Element {
    ui! {
        view(style = || StyleApplication::new(styles::Rail::sheet())) {
            // Direct messages. Its own slot above the rule, because DMs sit
            // outside every space rather than inside the first one.
            {
                let in_dms = move || app.scope.get() == Scope::Dms;
                slot(
                    in_dms,
                    press(
                        vec![icon(MESSAGE_CIRCLE).size(20.0).into()],
                        move || {
                            StyleApplication::new(styles::RailButton::sheet())
                                .with("active", if in_dms() { "on" } else { "off" })
                        },
                        move || app.go_dms(),
                    ),
                )
            }

            view(style = || StyleApplication::new(styles::RailRule::sheet()))

            for space in SPACES {
                {
                    let is_active =
                        move || app.space_id.get() == space.id && app.scope.get() == Scope::Space;
                    slot(
                        is_active,
                        press(
                            vec![ui! { text(space.initials) }],
                            move || {
                                StyleApplication::new(styles::RailButton::sheet())
                                    .with("active", if is_active() { "on" } else { "off" })
                            },
                            move || app.go_space(space.id),
                        ),
                    )
                }
            }

            {
                press(
                    vec![icon(PLUS).size(20.0).into()],
                    || StyleApplication::new(styles::RailAdd::sheet()),
                    move || app.join_open.set(true),
                )
            }

            // Everything below is pinned to the bottom of the rail.
            view(style = || StyleApplication::new(styles::Spring::sheet()))

            IconButton(
                glyph = String::new(),
                icon = Some(SUN),
                tone = tone::Neutral,
                variant = variant::Ghost,
                size = IconButtonSize::Md,
                on_click = cb(move || app.dark.set(!app.dark.get())),
            )
            IconButton(
                glyph = String::new(),
                icon = Some(SETTINGS),
                tone = tone::Neutral,
                // `rx!`, not a plain `if`: the gear fills when settings are
                // open, and that's a re-style, not a different button.
                variant = rx!(if app.settings_open.get() {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Ghost)
                }),
                size = IconButtonSize::Md,
                on_click = cb(move || {
                    app.settings_open.set(!app.settings_open.get());
                    app.thread_key.set(String::new());
                }),
            )
        }
    }
}
