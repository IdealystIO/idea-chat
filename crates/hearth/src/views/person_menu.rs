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
    MESSAGE_SQUARE, VOLUME_2, VOLUME_X,
};
use idea_ui::{AvatarSize, Menu, MenuItem, MenuLabel, MenuSeparator};
use runtime_core::{ui, Element, Ref, StyleApplication, ViewHandle};

use crate::model::App;
use crate::net;
use crate::styles;
use crate::views::bits::{cb, Face, Glyph};

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
fn person_menu(app: App, user_id: &str, anchor: Ref<ViewHandle>) -> Element {
    let user_id = user_id.to_string();
    // Read the role now rather than trusting anything cached: a demotion has
    // to take effect on the next menu, not the next session.
    let is_mod_view = app.moderates() && app.scope.get() == crate::model::Scope::Space;
    let muted_for_me = app.is_muted(&user_id);
    let member = app.member(&user_id);
    let display = member
        .as_ref()
        .map(|m| m.display_name.clone())
        .unwrap_or_else(|| "Someone".to_string());
    let is_moderator = member.as_ref().map(|m| m.role != "member").unwrap_or(false);

    // Menu adornments are `Option<Element>`, so they're built up front rather
    // than inline: an `Element` can't be constructed inside a prop position
    // that a nested `ui!` would occupy.
    // One clone per handler, made up front. `ui!` wraps a conditional branch
    // in a `move` closure, so a `name.clone()` written inside the `if has_dm`
    // arm moves `name` into that branch and the items after it can't see it.
    let (n_profile, n_dm, n_mute) = (user_id.clone(), user_id.clone(), user_id.clone());
    let profile_face: Element =
        ui! { Face(name = display.clone(), id = user_id.clone(), size = AvatarSize::Xs) };

    ui! {
        Menu(
            target = Some(runtime_core::primitives::portal::AnchorTarget::from(anchor)),
            on_dismiss = Some(cb(move || app.close_menu())),
        ) {
            MenuLabel(text = display.clone())
            MenuItem(
                label = "View profile".to_string(),
                leading = Some(profile_face),
                on_select = cb(move || app.open_profile(&n_profile)),
            )
            MenuItem(
                label = "Send a message".to_string(),
                leading = Some(glyph(MESSAGE_SQUARE)),
                on_select = cb(move || net::open_dm(app, &n_dm)),
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

            // Moderation appears only where it would work. Muting in rooms
            // and removal need endpoints that do not exist yet, so they are
            // not drawn — a menu item that cannot do its job teaches people to
            // distrust the menu.
            if is_mod_view {
                MenuSeparator()
                MenuLabel(text = if is_moderator {
                    "Moderator".to_string()
                } else {
                    "Member".to_string()
                })
            }
        }
    }
}

/// A 15px leading icon for a menu row.
fn glyph(data: runtime_core::IconData) -> Element {
    ui! { Glyph(data = Some(data)) }
}
