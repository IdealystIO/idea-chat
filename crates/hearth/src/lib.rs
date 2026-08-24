//! Hearth — the idea-chat client.
//!
//! An implementation of `Hearth.dc.html` on the idealyst framework, composed
//! from `idea-ui` components over a Hearth-specific theme. The layout lives in
//! [`styles`] as `stylesheet!` blocks; every control is an idea-ui component;
//! nothing here re-implements a widget the design system already ships.
//!
//! # Shape
//!
//! ```text
//! rail        70px   spaces, direct messages, theme, settings
//! room list  246px   channels + rooms, or conversations; you at the bottom
//! main        rest   header, then one of chat / live room / settings
//!                    thread docks over its right edge
//! people     224px   who's around (chat, inside a space, toggled on)
//! ```
//!
//! Four dialogs (search, join, a person, remove) mount at the root as
//! `idea_ui::Modal`s, which is why they're siblings of the shell rather than
//! children of whatever opened them.

pub mod api;
pub mod model;
#[cfg(feature = "server")]
pub mod srv;
pub mod styles;
pub mod theme;
pub mod views;

use icons_lucide::{SEARCH, SMARTPHONE, USERS};
use idea_ui::{shape, size, install_idea_theme_reactive, tone, typography_kind, variant, Button, IconButton, IconButtonSize, VariantRef};
use runtime_core::{switch, component, ui, Element, StyleApplication};

use model::{App, Scope, View};
use views::chat::ChatColumn;
use views::live::LiveRoom;
use views::mobile::MobilePreview;
use views::overlays::{JoinDialog, ProfileCard, RemoveConfirm, SearchPalette};
use views::rail::Rail;
use views::rooms::{PeopleSidebar, RoomList};
use views::settings::SettingsPage;
use views::thread::ThreadPanel;
use views::bits::{cb, Muted, Strong};

/// SDK-handler registration seam the CLI-generated wrappers invoke after
/// `runtime_vocabulary::register_builtins`. Hearth renders entirely from
/// builtin primitives and idea-ui components, so there is nothing to register.
pub fn register_scene_extensions<H: runtime_scene::Host>(
    _registry: &mut runtime_scene::Registry<H>,
) {
}

/// Android entry point used by the generated wrapper.
pub fn scene_app() -> Element {
    app()
}

#[component]
pub fn app() -> Element {
    let state = App::new();

    // Reactive install: flipping `dark` (or `accent`) re-themes every idea-ui
    // component AND every `stylesheet!` in `styles`, because both read the
    // same token names. No effect to hand-roll, no per-component swap, no
    // first-paint flash.
    install_idea_theme_reactive(move || {
        let accent = state.accent.get();
        if state.dark.get() { theme::hearth_dark(accent) } else { theme::hearth_light(accent) }
    });

    ui! {
        view(style = || StyleApplication::new(styles::Shell::sheet())) {
            Rail(app = state)
            RoomList(app = state)
            MainColumn(app = state)

            // The dialogs mount at the root rather than inside whatever opened
            // them: a `Modal` renders through a portal, and one owned by a row
            // would be torn down the moment that row re-rendered.
            SearchPalette(app = state)
            JoinDialog(app = state)
            ProfileCard(app = state)
            RemoveConfirm(app = state)
            MobilePreview(app = state)
        }
    }
}

/// The header plus whichever view is selected, with the thread panel docked
/// over its right edge when one is open.
#[component]
fn MainColumn(#[prop(static)] app: App) -> Element {
    switch(move || app.nav_key(), move |_| main_column_body(app))
}

fn main_column_body(app: App) -> Element {
    let view = app.view();
    let thread_open = !app.thread_key.get().is_empty();

    // The selected view is chosen before the macro: a `ui!` nested inside a
    // braced child doesn't parse, and the three arms are whole subtrees rather
    // than sibling lists.
    let body: Element = match view {
        View::Chat => ui! { ChatPane(app = app) },
        View::Live => ui! { LiveRoom(app = app) },
        View::Settings => ui! { SettingsPage(app = app) },
    };
    let thread: Vec<Element> =
        if thread_open { vec![ui! { ThreadPanel(app = app) }] } else { Vec::new() };

    ui! {
        view(style = || StyleApplication::new(styles::Main::sheet())) {
            Header(app = app)
            view(style = || StyleApplication::new(styles::Body::sheet())) {
                body
            }
            thread
        }
    }
}

/// The chat view plus, when it applies, the people sidebar beside it.
#[component]
fn ChatPane(#[prop(static)] app: App) -> Element {
    switch(move || (app.scope.get(), app.show_people.get()), move |_| chat_pane_body(app))
}

fn chat_pane_body(app: App) -> Element {
    let people: Vec<Element> = if views::rooms::people_visible(app) {
        vec![ui! { PeopleSidebar(app = app) }]
    } else {
        Vec::new()
    };
    ui! {
        view(style = || StyleApplication::new(styles::Body::sheet())) {
            ChatColumn(app = app)
            people
        }
    }
}

/// The title bar: where you are, and the three things you can do from anywhere.
#[component]
fn Header(#[prop(static)] app: App) -> Element {
    switch(move || (app.nav_key(), app.prefs_key()), move |_| header_body(app))
}

fn header_body(app: App) -> Element {
    let settings = app.settings_open.get();
    let title = if settings { "Settings".to_string() } else { app.room_title() };
    let subtitle = if settings {
        String::new()
    } else if app.viewing_dm() {
        "Just the two of you".to_string()
    } else if app.current_kind().is_chat() {
        format!("Everyone in {} can read this", model::space(app.space_id.get()).name)
    } else {
        String::new()
    };
    let in_space = app.scope.get() == Scope::Space;
    let people_on = app.show_people.get();
    let subtitle_node: Vec<Element> = if subtitle.is_empty() {
        Vec::new()
    } else {
        vec![ui! { Muted(label = subtitle, kind = typography_kind::BodySm) }]
    };
    // The roster toggle only means something inside a space; a DM has exactly
    // two people in it and both of them are obvious.
    let roster_toggle: Vec<Element> = if in_space {
        vec![ui! {
            IconButton(
                glyph = String::new(),
                icon = Some(USERS),
                tone = tone::Neutral,
                variant = if people_on {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Ghost)
                },
                size = IconButtonSize::Md,
                on_click = cb(move || app.show_people.set(!app.show_people.get())),
            )
        }]
    } else {
        Vec::new()
    };

    ui! {
        view(style = || StyleApplication::new(styles::Header::sheet())) {
            view(style = || StyleApplication::new(styles::MessageHead::sheet())) {
                Strong(label = title, kind = typography_kind::BodyLg)
                subtitle_node
            }
            view(style = || StyleApplication::new(styles::Spring::sheet()))

            Button(
                label = "Search".to_string(),
                leading_icon = Some(SEARCH),
                tone = tone::Neutral,
                variant = variant::Soft,
                size = size::Sm,
                shape = shape::Md,
                on_click = cb(move || {
                    app.query.set(String::new());
                    app.search_open.set(true);
                }),
            )
            roster_toggle
            IconButton(
                glyph = String::new(),
                icon = Some(SMARTPHONE),
                tone = tone::Neutral,
                variant = if app.mobile_open.get() {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Ghost)
                },
                size = IconButtonSize::Md,
                on_click = cb(move || app.mobile_open.set(!app.mobile_open.get())),
            )
        }
    }
}
