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

use icons_lucide::{MESSAGE_SQUARE, PHONE_OFF, VOLUME_2, VOLUME_X};
use idea_ui::{AvatarSize, ControlSize, Menu, MenuItem, MenuLabel, MenuSeparator, Slider};
use runtime_core::{text, ui, Element, Ref, StyleApplication, ViewHandle};

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
    // The voice room they are in, if any — where a moderator can remove them
    // from. Never ourselves: that is "Leave room".
    let their_room = (is_mod_view && user_id != app.my_id())
        .then(|| {
            app.voice_rooms
                .with(|rooms| rooms.iter().find(|r| r.people.iter().any(|p| p.id == user_id)).map(|r| r.channel_id.clone()))
        })
        .flatten();
    let n_remove = user_id.clone();

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
            // Says which of the two states this person is in, because the
            // slider alone cannot: a level of 40% and a level of 40% behind a
            // mute look identical, and only one of them makes a sound.
            MenuLabel(text = if muted_for_me {
                "Muted — volume kept".to_string()
            } else {
                "Volume for me".to_string()
            })
            volume_row(app, &user_id, muted_for_me)
            MenuItem(
                label = if muted_for_me { "Unmute for me".to_string() } else { "Mute for me".to_string() },
                leading = Some(glyph(if muted_for_me { VOLUME_2 } else { VOLUME_X })),
                on_select = cb(move || {
                    // The toggle reports the state being moved to; reading the
                    // signal back would give the one it just replaced.
                    let muted = app.toggle_mute(&n_mute);
                    apply(&n_mute, muted);
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
            // Works because the relay enforces it: the server revokes their
            // grant there, and their call ends then and there.
            if let Some(room) = their_room.clone() {
                MenuItem(
                    label = "Remove from call".to_string(),
                    leading = Some(glyph(PHONE_OFF)),
                    on_select = cb({
                        let (room, user) = (room.clone(), n_remove.clone());
                        move || {
                            net::remove_from_call(app, &room, &user);
                            app.close_menu();
                        }
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

/// One person's volume, as a slider, inside their menu.
///
/// Deliberately not a `MenuItem`: it is not a command, it is a value, and a
/// row that dismissed the menu on the first press would be unusable as one.
///
/// The whole row is a listener-side setting — see [`crate::volume`]. Nothing
/// leaves this device and the person on the other end is not told, which is
/// what lets it sit in a menu with no confirmation attached.
fn volume_row(app: App, user_id: &str, muted: bool) -> Element {
    let id = user_id.to_string();
    let on_change = std::rc::Rc::new(move |position: f32| {
        // The signal first, so the slider tracks the drag; then the store, so
        // it survives a refresh; then the call, so it is audible now. While
        // muted the last step changes nothing audible and is still correct —
        // it sets the level this person returns to.
        app.person_volume.set(position);
        crate::volume::set_person(&id, position);
        #[cfg(target_arch = "wasm32")]
        crate::voice::refresh_person(&id, position, muted);
    }) as std::rc::Rc<dyn Fn(f32)>;

    ui! {
        view(style = || StyleApplication::new(styles::MenuControl::sheet())) {
            Slider(
                value = app.person_volume,
                on_change = on_change,
                min = 0.0,
                max = crate::volume::PERSON_MAX,
                width = 132.0,
                size = ControlSize::Sm,
                // Live even while muted, on purpose. It is not the mute
                // control, it is the level this person comes BACK to — and a
                // slider that refused to move would be teaching people that
                // the two are the same thing, which is exactly what having
                // both exists to deny.
                leading_icon = Some(VOLUME_X),
                trailing_icon = Some(VOLUME_2),
            )
            text(move || format!("{:.0}%", app.person_volume.get() * 100.0))
                .with_style(|| StyleApplication::new(styles::VolumeReadout::sheet()))
        }
    }
}

/// Push a person's current volume-and-mute onto the call in progress.
fn apply(user_id: &str, muted: bool) {
    #[cfg(target_arch = "wasm32")]
    crate::voice::refresh_person(user_id, crate::volume::person(user_id), muted);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (user_id, muted);
}
