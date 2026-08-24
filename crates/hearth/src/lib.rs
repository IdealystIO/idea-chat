//! Hearth — the idea-chat client, and the server functions behind it.
//!
//! One crate, two builds. By default this compiles the client: the UI, and
//! HTTP stubs for every `#[server]` function. Under `--features server` it
//! compiles the other half — the real function bodies, the database, the
//! session store — and `src/bin/server.rs` serves both the API and the wasm
//! bundle from one origin, which the session cookie requires.
//!
//! # Shape
//!
//! ```text
//! rail        70px   spaces, direct messages, theme, settings
//! room list  246px   channels + rooms, or conversations; you at the bottom
//! main        rest   header, then one of chat / live room / settings
//!                    thread docks over its right edge
//! people     224px   who's in this space (chat, inside a space, toggled on)
//! ```
//!
//! Before any of that renders, [`model::Phase`] decides whether the screen is
//! the app, a sign-in form, a claim form, or a connection error.

pub mod api;
pub mod model;
// The client half. The server binary never renders, and `net` holds the client
// stubs of the very fns the server build defines bodies for — so both are off
// under `--features server`.
#[cfg(not(feature = "server"))]
pub mod net;
#[cfg(feature = "server")]
pub mod srv;
pub mod styles;
pub mod theme;
#[cfg(not(feature = "server"))]
pub mod views;
// The live call. Web-only: playback on native is the missing `audio-output`
// SDK, so rather than shipping a session that captures and never plays, the
// module is absent and the UI says voice is not available on this build.
#[cfg(all(not(feature = "server"), target_arch = "wasm32"))]
pub mod voice;

/// SDK-handler registration seam the CLI-generated wrappers invoke after
/// `runtime_vocabulary::register_builtins`. Hearth renders entirely from
/// builtin primitives and idea-ui components, so there is nothing to register.
pub fn register_scene_extensions<H: runtime_scene::Host>(
    _registry: &mut runtime_scene::Registry<H>,
) {
}

/// Android entry point used by the generated wrapper.
#[cfg(not(feature = "server"))]
pub fn scene_app() -> runtime_core::Element {
    app()
}

#[cfg(not(feature = "server"))]
mod client {
    use icons_lucide::{MENU, SEARCH, USERS};
    use idea_ui::{
        install_idea_theme_reactive, shape, size, tone, typography_kind, variant, Button,
        IconButton, IconButtonSize, VariantRef,
    };
    use idea_ui_nav::{sidebar_pinned, AppShell, Drawer, DrawerSide};
    use runtime_core::{
        component, current_breakpoint, effect, switch, ui, Breakpoint, Element, StyleApplication,
    };

    use crate::model::{App, Phase, Scope, View};
    use crate::net;
    use crate::styles;
    use crate::theme;
    use crate::views;
    use crate::views::auth::AuthScreen;
    use crate::views::bits::{cb, Muted, Strong};
    use crate::views::chat::ChatColumn;
    use crate::views::live::LiveRoom;
    use crate::views::overlays::{JoinDialog, ProfileCard, SearchPalette};
    use crate::views::rail::Rail;
    use crate::views::rooms::{PeopleSidebar, RoomList};
    use crate::views::settings::SettingsPage;
    use crate::views::thread::ThreadPanel;

    #[component]
    pub fn app() -> Element {
        let state = App::new();

        // Reactive install: flipping `dark` (or `accent`) re-themes every
        // idea-ui component AND every `stylesheet!` in `styles`, because both
        // read the same token names.
        install_idea_theme_reactive(move || {
            let accent = state.accent.get();
            if state.dark.get() {
                theme::hearth_dark(accent)
            } else {
                theme::hearth_light(accent)
            }
        });

        // Point the server-fn client at the right origin before anything can
        // call it, then ask the server what this instance is.
        net::configure();
        // Test seam for the voice end-to-end run; see `voice::install_stats_probe`.
        #[cfg(target_arch = "wasm32")]
        {
            crate::voice::install_stats_probe();
            crate::voice::watch_presence(state);
        }
        effect!({
            net::start(state);
        });

        // The whole screen swaps on phase, so this is a genuine structural
        // switch rather than a re-style.
        switch(move || state.phase.get(), move |phase| match phase {
            Phase::Ready => ui! { Shell(app = state) },
            _ => ui! { AuthScreen(app = state) },
        })
    }

    /// The signed-in app.
    ///
    /// The rail and the room list are one navigation surface, so they are one
    /// `AppShell` sidebar: pinned in-flow from `Lg` up, an off-canvas drawer
    /// below it. `AppShell` builds that sidebar exactly once and switches
    /// between the two arrangements with static breakpoint styling, so
    /// crossing the threshold never remounts the room list or re-fires what it
    /// loaded — which is why this is the SDK's job and not a pair of
    /// hand-rolled columns.
    #[component]
    fn Shell(#[prop(static)] app: App) -> Element {
        let sidebar: Vec<Element> = vec![ui! {
            view(style = || StyleApplication::new(styles::NavPanel::sheet())) {
                Rail(app = app)
                RoomList(app = app)
            }
        }];

        ui! {
            view(style = || StyleApplication::new(styles::Shell::sheet())) {
                AppShell(
                    sidebar = sidebar,
                    is_open = app.nav_open,
                    pin_at = Breakpoint::Lg,
                    width = 316.0,
                ) {
                    MainColumn(app = app)
                }

                // The dialogs mount at the root rather than inside whatever
                // opened them: a `Modal` renders through a portal, and one
                // owned by a row would be torn down when that row re-rendered.
                SearchPalette(app = app)
                JoinDialog(app = app)
                ProfileCard(app = app)
            }
        }
    }

    /// The header plus whichever view is selected, with the thread panel
    /// docked over its right edge when one is open.
    #[component]
    fn MainColumn(#[prop(static)] app: App) -> Element {
        switch(
            move || {
                (
                    app.channel_id.get(),
                    app.settings_open.get(),
                    app.thread_root.get(),
                    app.stage.get(),
                )
            },
            move |_| main_column_body(app),
        )
    }

    fn main_column_body(app: App) -> Element {
        let body: Element = match app.view() {
            View::Chat => ui! { ChatPane(app = app) },
            View::Live => ui! { LiveRoom(app = app) },
            View::Settings => ui! { SettingsPage(app = app) },
        };
        let thread: Vec<Element> = if app.thread_root.get().is_empty() {
            Vec::new()
        } else {
            vec![ui! { ThreadPanel(app = app) }]
        };

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

    /// The chat view plus, when it applies, the people sidebar.
    ///
    /// Pinned beside the messages on a wide screen, an edge drawer on a narrow
    /// one. A 224px column taken out of a phone's width leaves the messages
    /// unreadable, so below `Xl` the roster overlays instead of competing.
    #[component]
    fn ChatPane(#[prop(static)] app: App) -> Element {
        switch(
            move || {
                (
                    app.scope.get(),
                    app.show_people.get(),
                    app.roster_open.get(),
                    current_breakpoint().get(),
                )
            },
            move |_| chat_pane_body(app),
        )
    }

    fn chat_pane_body(app: App) -> Element {
        let wanted = views::rooms::people_visible(app);
        let roomy = sidebar_pinned(Breakpoint::Xl);

        if roomy {
            let people: Vec<Element> =
                if wanted { vec![ui! { PeopleSidebar(app = app) }] } else { Vec::new() };
            return ui! {
                view(style = || StyleApplication::new(styles::Body::sheet())) {
                    ChatColumn(app = app)
                    people
                }
            };
        }

        // Outside a space there is no roster to show, and the header hides its
        // toggle there — so do not leave a drawer that a stale `show_people`
        // could still slide open over a DM.
        if app.scope.get() != Scope::Space {
            return ui! {
                view(style = || StyleApplication::new(styles::Body::sheet())) {
                    ChatColumn(app = app)
                }
            };
        }

        // Narrow: the roster is an overlay on the trailing edge. `show_people`
        // is the same signal the header toggle already drives, so the control
        // does not change meaning with the viewport — only its presentation.
        let panel: Vec<Element> = vec![ui! { PeopleSidebar(app = app) }];
        ui! {
            view(style = || StyleApplication::new(styles::Body::sheet())) {
                Drawer(
                    sidebar = panel,
                    is_open = app.roster_open,
                    side = DrawerSide::End,
                    width = 260.0,
                ) {
                    ChatColumn(app = app)
                }
            }
        }
    }

    /// The title bar: where you are, and the three things you can do from
    /// anywhere.
    #[component]
    fn Header(#[prop(static)] app: App) -> Element {
        switch(
            move || {
                (
                    app.channel_id.get(),
                    app.settings_open.get(),
                    app.show_people.get(),
                    app.roster_open.get(),
                    app.revision.get(),
                    app.stage.get(),
                    // Rebuild when the sidebar pins or unpins: the hamburger
                    // exists only while it is a drawer.
                    current_breakpoint().get(),
                )
            },
            move |_| header_body(app),
        )
    }

    fn header_body(app: App) -> Element {
        let settings = app.settings_open.get();
        let on_stage = !app.stage.get().is_empty();
        let title = if settings {
            "Settings".to_string()
        } else if on_stage {
            app.voice_room_name()
        } else {
            app.channel_title()
        };
        // A narrow header has room for the name or the explanation, not both.
        let roomy = sidebar_pinned(Breakpoint::Md);
        let subtitle = if settings || !roomy {
            String::new()
        } else if on_stage {
            "You're in this room".to_string()
        } else if app.viewing_dm() {
            "Just the two of you".to_string()
        } else if app.channel_kind().is_chat() {
            app.space()
                .map(|s| format!("Everyone in {} can read this", s.name))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let in_space = app.scope.get() == Scope::Space && !on_stage;
        let people_on = if sidebar_pinned(Breakpoint::Xl) {
            app.show_people.get()
        } else {
            app.roster_open.get()
        };

        // Only when the sidebar is a drawer — pinned, it is already on screen
        // and a button to reveal it would be a button that does nothing.
        let hamburger: Vec<Element> = if sidebar_pinned(Breakpoint::Lg) {
            Vec::new()
        } else {
            vec![ui! {
                IconButton(
                    glyph = String::new(),
                    icon = Some(MENU),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    size = IconButtonSize::Md,
                    on_click = cb(move || app.nav_open.set(!app.nav_open.get())),
                )
            }]
        };

        let subtitle_node: Vec<Element> = if subtitle.is_empty() {
            Vec::new()
        } else {
            vec![ui! { Muted(label = subtitle, kind = typography_kind::BodySm) }]
        };
        // The roster toggle only means something inside a space; a DM has
        // exactly two people in it and both of them are obvious.
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
                    on_click = cb(move || {
                        // Pinned column on a wide screen, overlay on a narrow
                        // one — the button means "show me who's here" either
                        // way, but the two are different pieces of state.
                        if sidebar_pinned(Breakpoint::Xl) {
                            app.show_people.set(!app.show_people.get());
                        } else {
                            app.roster_open.set(!app.roster_open.get());
                        }
                    }),
                )
            }]
        } else {
            Vec::new()
        };

        ui! {
            view(style = || StyleApplication::new(styles::Header::sheet())) {
                hamburger
                view(style = || StyleApplication::new(styles::HeaderTitle::sheet())) {
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
            }
        }
    }
}

#[cfg(not(feature = "server"))]
pub use client::app;
