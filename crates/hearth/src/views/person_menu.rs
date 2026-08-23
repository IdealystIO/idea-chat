//! The person menu, and the wrapper that anchors one to any row showing a
//! person.
//!
//! The design opens this on right-click, at the pointer. The framework's press
//! handlers are `Fn()` — no event, no button, no coordinates — so there is no
//! secondary click to hook and no point to position against. Rather than
//! inventing a second affordance, Hearth opens the same menu on a normal press
//! and anchors it to the row with `idea_ui::Menu`, with **View profile** as
//! its first item so the design's primary action stays one click away.
//!
//! Anchoring is the better half of the trade: a menu pinned to its row lands
//! correctly on every backend, whereas viewport coordinates are a web idea
//! that has to be re-derived on iOS and Android. See README.md for the gap.

use icons_lucide::{
    MESSAGE_SQUARE, SHIELD, SHIELD_OFF, USER_MINUS, USER_PLUS, VOLUME_2, VOLUME_X,
};
use idea_ui::{typography_kind, AvatarSize, Menu, MenuItem, MenuLabel, MenuSeparator};
use runtime_core::{ui, Element, Ref, StyleApplication, ViewHandle};

use crate::model::{self, App};
use crate::styles;
use crate::views::bits::{cb, Face, Glyph, Muted};

/// Wrap a row that shows a person so that pressing it opens their menu.
///
/// `site` distinguishes the lists a person can appear in at once — the roster,
/// a room's faces, a DM row, a message author. Without it, pressing Ana in the
/// roster would also open the menu attached to Ana's face under The Porch.
pub fn person_target(
    app: App,
    site: &str,
    name: &str,
    build: impl FnOnce(Box<dyn Fn()>) -> Element,
) -> Element {
    let anchor: Ref<ViewHandle> = Ref::new();
    let site = site.to_string();
    let name = name.to_string();

    let open = {
        let site = site.clone();
        let name = name.clone();
        Box::new(move || app.open_menu(&site, &name)) as Box<dyn Fn()>
    };
    let mut children = vec![build(open)];
    if app.menu_at(&site).as_deref() == Some(name.as_str()) {
        children.push(person_menu(app, &name, anchor.clone()));
    }

    // Built outside `ui!`: the wrapper needs both a children list and a
    // `.bind()` for the menu to anchor against, and a tag carrying a method
    // chain can't also take a children block.
    runtime_core::view(children)
        .with_style(|| StyleApplication::new(styles::Bare::sheet()))
        .bind(anchor)
        .into()
}

/// The menu itself.
fn person_menu(app: App, name: &str, anchor: Ref<ViewHandle>) -> Element {
    let name = name.to_string();
    let is_mod_view = app.you_are_moderator.get() && app.scope.get() == crate::model::Scope::Space;
    let muted_for_me = app.is_muted(&name);
    let timed_out = app.is_timed_out(&name);
    let is_moderator = app.is_moderator(&name);
    let has_dm = model::dm_for(&name).is_some();

    // Menu adornments are `Option<Element>`, so they're built up front rather
    // than inline: an `Element` can't be constructed inside a prop position
    // that a nested `ui!` would occupy.
    // One clone per handler, made up front. `ui!` wraps a conditional branch
    // in a `move` closure, so a `name.clone()` written inside the `if has_dm`
    // arm moves `name` into that branch and the items after it can't see it.
    let (n_profile, n_dm, n_mute, n_timeout, n_role, n_remove) = (
        name.clone(),
        name.clone(),
        name.clone(),
        name.clone(),
        name.clone(),
        name.clone(),
    );
    let profile_face: Element = ui! { Face(name = name.clone(), size = AvatarSize::Xs) };
    let timeout_hint: Option<Element> = if timed_out {
        None
    } else {
        Some(ui! { Muted(label = "1 hour".to_string(), kind = typography_kind::Overline) })
    };

    ui! {
        Menu(
            target = Some(runtime_core::primitives::portal::AnchorTarget::from(anchor)),
            on_dismiss = Some(cb(move || app.close_menu())),
        ) {
            MenuLabel(text = name)
            MenuItem(
                label = "View profile".to_string(),
                leading = Some(profile_face),
                on_select = cb(move || app.open_profile(&n_profile)),
            )
            // Only offered when a conversation actually exists. A "Send a
            // message" that silently does nothing is worse than its absence.
            if has_dm {
                MenuItem(
                    label = "Send a message".to_string(),
                    leading = Some(glyph(MESSAGE_SQUARE)),
                    on_select = cb(move || {
                        app.close_menu();
                        if let Some(d) = model::dm_for(&n_dm) {
                            app.go(d.id);
                        }
                    }),
                )
            }
            MenuItem(
                label = "Invite to a room".to_string(),
                leading = Some(glyph(USER_PLUS)),
                on_select = cb(move || {
                    app.close_menu();
                    app.join_open.set(true);
                }),
            )

            MenuSeparator()
            MenuItem(
                label = if muted_for_me { "Unmute for me".to_string() } else { "Mute for me".to_string() },
                leading = Some(glyph(if muted_for_me { VOLUME_2 } else { VOLUME_X })),
                on_select = cb(move || {
                    app.toggle_in(app.muted, &n_mute);
                    app.close_menu();
                }),
            )

            // Moderation only appears inside a space, and only when you can
            // actually moderate it. Showing greyed-out powers you don't have
            // is how an app teaches people to ignore its menus.
            if is_mod_view {
                MenuSeparator()
                MenuItem(
                    label = if timed_out {
                        "Let them speak in rooms".to_string()
                    } else {
                        "Mute in rooms".to_string()
                    },
                    leading = Some(glyph(VOLUME_X)),
                    trailing = timeout_hint,
                    on_select = cb(move || {
                        app.toggle_in(app.timed_out, &n_timeout);
                        app.close_menu();
                    }),
                )
                MenuItem(
                    label = if is_moderator {
                        "Remove moderator".to_string()
                    } else {
                        "Make moderator".to_string()
                    },
                    leading = Some(glyph(if is_moderator { SHIELD_OFF } else { SHIELD })),
                    on_select = cb(move || {
                        app.toggle_in(app.moderators, &n_role);
                        app.close_menu();
                    }),
                )
                MenuItem(
                    label = "Remove from space…".to_string(),
                    leading = Some(glyph(USER_MINUS)),
                    on_select = cb(move || {
                        app.close_menu();
                        app.remove_target.set(n_remove.clone());
                    }),
                )
            }
        }
    }
}

/// A 15px leading icon for a menu row.
fn glyph(data: runtime_core::IconData) -> Element {
    ui! { Glyph(data = Some(data)) }
}
