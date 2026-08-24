//! The spaces rail — the 70px column that switches between direct messages
//! and each space, with the theme and settings controls pinned to the bottom.
//!
//! The space list comes from the server now, so this region is wrapped in a
//! `switch` keyed on the load revision. Selection within it is still read
//! inside each node's style closure: which space is highlighted is a re-tint
//! of two nodes, not a different tree.

use icons_lucide::{MESSAGE_CIRCLE, PLUS, SETTINGS, SUN};
use idea_ui::{tone, variant, IconButton, IconButtonSize, VariantRef};
use runtime_core::{component, icon, rx, switch, text, ui, Element, StyleApplication};

use crate::model::{initials, App, Scope};
use crate::net;
use crate::styles;
use crate::views::bits::{cb, press};

/// One slot in the rail: the selection pip and the round button beside it.
///
/// `selected` is a closure, not a `bool`: it is called inside the pip's style
/// sink, so the pip grows and shrinks on its own when the selection moves.
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
    switch(move || app.revision.get(), move |_| rail_body(app))
}

fn rail_body(app: App) -> Element {
    let spaces: Vec<(String, String)> =
        app.spaces.with(|s| s.iter().map(|s| (s.id.clone(), s.name.clone())).collect());

    let buttons: Vec<Element> = spaces
        .into_iter()
        .map(|(id, name)| {
            let for_style = id.clone();
            let for_click = id.clone();
            let for_pip = for_style.clone();
            let pip_active =
                move || app.space_id.get() == for_pip && app.scope.get() == Scope::Space;
            let is_active =
                move || app.space_id.get() == for_style && app.scope.get() == Scope::Space;
            slot(
                pip_active,
                press(
                    vec![ui! { text(initials(&name)) }],
                    move || {
                        StyleApplication::new(styles::RailButton::sheet())
                            .with("active", if is_active() { "on" } else { "off" })
                    },
                    move || net::go_space(app, &for_click),
                ),
            )
        })
        .collect();

    // Built outside the macro: a braced child whose expression contains a
    // block confuses the `ui!` parser.
    let mut space_buttons = buttons;
    space_buttons.push(press(
        vec![icon(PLUS).size(20.0).into()],
        || StyleApplication::new(styles::RailAdd::sheet()),
        move || {
            app.minted_invite.set(String::new());
            app.join_open.set(true);
        },
    ));

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
                        move || net::go_dms(app),
                    ),
                )
            }

            view(style = || StyleApplication::new(styles::RailRule::sheet()))

            space_buttons

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
                variant = rx!(if app.settings_open.get() {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Ghost)
                }),
                size = IconButtonSize::Md,
                on_click = cb(move || {
                    app.settings_open.set(!app.settings_open.get());
                    app.thread_root.set(String::new());
                }),
            )
        }
    }
}
