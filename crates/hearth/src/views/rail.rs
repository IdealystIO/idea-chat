//! The spaces rail — the 70px column that switches between direct messages
//! and each space, with the theme and settings controls pinned to the bottom.

use icons_lucide::{MESSAGE_CIRCLE, PLUS, SETTINGS, SUN};
use idea_ui::{tone, variant, IconButton, IconButtonSize, VariantRef};
use runtime_core::{switch, component, icon, text, ui, Element, StyleApplication};

use crate::model::{App, Scope, SPACES};
use crate::styles;
use crate::views::bits::{cb, press};

/// One slot in the rail: the selection pip and the round button beside it.
///
/// The two children are assembled here rather than written as macro siblings:
/// a childless tag followed by a braced `{ button }` is parsed as that tag's
/// children block, which nested the button inside the absolutely-positioned
/// pip and collapsed the slot to its padding.
fn slot(pip_state: &'static str, button: Element) -> Element {
    let pip: Element = runtime_core::view(Vec::new())
        .with_style(move || {
            StyleApplication::new(styles::RailPip::sheet()).with("state", pip_state)
        })
        .into();
    let children = vec![pip, button];
    ui! {
        view(style = || StyleApplication::new(styles::RailSlot::sheet())) {
            children
        }
    }
}

/// Each top-level region is a `#[component]` rather than a plain `fn`, because
/// a component body IS the reactive scope. A helper fn's `signal.get()` is
/// attributed to whatever scope happens to be building at the time; with no
/// component boundary at all, nothing subscribes and a press updates state
/// that never reaches the screen. Making each region a component also bounds
/// what re-renders: pressing a channel rebuilds the room list and the message
/// column, not the settings page.
#[component]
pub fn Rail(#[prop(static)] app: App) -> Element {
    switch(move || app.nav_key(), move |_| rail_body(app))
}

fn rail_body(app: App) -> Element {
    ui! {
        view(style = || StyleApplication::new(styles::Rail::sheet())) {
            // Direct messages. Its own slot above the rule, because DMs sit
            // outside every space rather than inside the first one.
            {
                let active = app.scope.get() == Scope::Dms;
                slot(
                    if active { "active" } else { "idle" },
                    press(
                        vec![icon(MESSAGE_CIRCLE).size(20.0).into()],
                        move || StyleApplication::new(styles::RailButton::sheet())
                            .with("active", if active { "on" } else { "off" }),
                        move || app.go_dms(),
                    ),
                )
            }

            view(style = || StyleApplication::new(styles::RailRule::sheet()))

            for space in SPACES {
                {
                    let active = app.space_id.get() == space.id && app.scope.get() == Scope::Space;
                    slot(
                        if active { "active" } else { "idle" },
                        press(
                            vec![ui! { text(space.initials) }],
                            move || StyleApplication::new(styles::RailButton::sheet())
                                .with("active", if active { "on" } else { "off" }),
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
                variant = if app.settings_open.get() {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Ghost)
                },
                size = IconButtonSize::Md,
                on_click = cb(move || {
                    app.settings_open.set(!app.settings_open.get());
                    app.thread_key.set(String::new());
                }),
            )
        }
    }
}
