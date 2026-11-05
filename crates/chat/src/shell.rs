//! Signed in: the shell the design calls "0 · Shell anatomy" —
//!
//! ```text
//! ┌────┬──────────────┬──────────────────────────────┬────────────┐
//! │ sw │ navigation   │ main stage                   │ context    │
//! │ it │              │                              │ (optional) │
//! │ ch │              │                              │            │
//! │ er ├──────────────┤                              │            │
//! │    │ call strip   │                              │            │
//! │    │ presence     │                              │            │
//! └────┴──────────────┴──────────────────────────────┴────────────┘
//! ```
//!
//! The switcher and navigation are one sidebar: pinned from tablet width
//! up, a drawer below it (`idea_ui_nav::AppShell`). The stage is the
//! navigator's outlet; each screen puts its own context panel beside its
//! stage, inline on wide screens and over the stage on narrower ones.

use std::cell::Cell;

use chat_model::Id;
use idea_ui_nav::{sidebar_pinned, AppShell};
use runtime_core::{
    component, current_breakpoint, signal, stylesheet, ui, Breakpoint, Element, FlexDirection, Length, Ref, RouteParams,
    Signal,
};
use swap_navigator::{Route, Screen, SwapBuilder, SwapHandle, SwapNavigator};

use crate::channel::ChannelStage;
use crate::home::HomeStage;
use crate::live::LiveLink;
use crate::nav::Sidebar;
use crate::overlays::Overlays;
use crate::platform;
use crate::store::{Panel, Place, Store};

/// Where the sidebar stops being a drawer.
pub const PIN_AT: Breakpoint = Breakpoint::Md;
/// The switcher's and the navigation's widths together.
pub const SIDEBAR_WIDTH: f32 = 64.0 + 248.0;

/// Route parameters: a conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectParams {
    pub channel: Id,
}

impl RouteParams for DirectParams {
    fn to_path(&self, pattern: &str) -> String {
        pattern.replace(":channel", &self.channel.to_string())
    }
    fn from_segments(s: &std::collections::HashMap<String, String>) -> Option<Self> {
        Some(DirectParams { channel: s.get("channel")?.parse().ok()? })
    }
}

/// Route parameters: a channel in a space.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelParams {
    pub space: Id,
    pub channel: Id,
}

impl RouteParams for ChannelParams {
    fn to_path(&self, pattern: &str) -> String {
        pattern.replace(":space", &self.space.to_string()).replace(":channel", &self.channel.to_string())
    }
    fn from_segments(s: &std::collections::HashMap<String, String>) -> Option<Self> {
        Some(ChannelParams { space: s.get("space")?.parse().ok()?, channel: s.get("channel")?.parse().ok()? })
    }
}

fn home_route() -> Route<()> {
    Route::new("home", "/")
}

fn direct_route() -> Route<DirectParams> {
    Route::new("direct", "/dm/:channel")
}

fn channel_route() -> Route<ChannelParams> {
    Route::new("channel", "/s/:space/:channel")
}

thread_local! {
    static NAV: Cell<Option<Ref<SwapHandle>>> = const { Cell::new(None) };
}

/// Go somewhere. Closes the drawer when it is one.
pub fn go(place: Place) {
    let Some(nav) = NAV.with(|n| n.get()) else { return };
    nav.with(|h| match place {
        Place::Home => h.select(&home_route(), ()),
        Place::Direct(channel) => h.select(&direct_route(), DirectParams { channel }),
        Place::Channel { space, channel } => h.select(&channel_route(), ChannelParams { space, channel }),
    });
    if let Place::Channel { space, channel } = place {
        platform::set_pref(&format!("last.{space}"), &channel.to_string());
    }
    if !sidebar_pinned(PIN_AT) {
        crate::store::store().ui().drawer.set(false);
    }
}

/// Open a space where the person left it: the channel they were last in,
/// else its first channel they can read. A space just made or joined is not
/// in the live state until the server restates it; it opens when it is.
pub fn open_space(space: Id) {
    let s = crate::store::store();
    if !s.peek(|st| st.spaces.contains_key(&space)) {
        s.open_when_ready(space);
        return;
    }
    let last: Option<Id> = platform::pref(&format!("last.{space}")).and_then(|c| c.parse().ok());
    let target = s.peek(|st| {
        if let Some(c) = last.filter(|c| st.channels.get(c).is_some_and(|ch| ch.space == Some(space)) && st.access(*c).can_read()) {
            return Some(c);
        }
        st.sidebar(space)
            .iter()
            .flat_map(|sec| sec.entries.iter())
            .map(|e| e.channel)
            .find(|c| c.kind == chat_model::ChannelKind::Text && st.access(c.id).can_read())
            .or_else(|| st.sidebar(space).iter().flat_map(|sec| sec.entries.iter()).map(|e| e.channel).next())
            .map(|c| c.id)
    });
    match target {
        Some(channel) => go(Place::Channel { space, channel }),
        None => go(Place::Home),
    }
}

stylesheet! {
    /// The stage: the main column and, beside it, the context panel.
    pub StageRow<idea_ui::IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            flex_grow: 1.0,
            min_height: 0.0,
            min_width: 0.0,
            height: Length::pct(100.0),
            background: t.color.surface(),
            position: runtime_core::Position::Relative,
        }
    }
}

stylesheet! {
    pub StageMain<()> {
        base(_t) {
            flex_direction: FlexDirection::Column,
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            min_height: 0.0,
        }
    }
}

/// Whether the context panel sits beside the stage (rather than over it).
pub fn panel_inline() -> bool {
    current_breakpoint().get().is_at_least(Breakpoint::Xl)
}

/// The signed-in app.
#[component]
pub fn SignedIn(#[prop(static)] me: Id, #[prop(static)] dark: Signal<bool>) -> Element {
    let _ = me;
    let store = Store::new(dark.peek());
    store.provide();
    // Narrow screens start with the context panel closed.
    if !current_breakpoint().peek().is_at_least(Breakpoint::Xl) {
        store.ui().panel.set(None);
    }
    {
        // The theme follows the store's switch, and is remembered.
        let ui = store.ui();
        runtime_core::effect!({
            let d = ui.dark.get();
            if d != dark.peek() {
                dark.set(d);
                platform::set_pref("theme", if d { "dark" } else { "light" });
            }
        });
    }

    let device: Signal<Option<Option<Id>>> = signal(None);
    crate::device::ensure(device);

    let nav: Ref<SwapHandle> = Ref::new();
    NAV.with(|n| n.set(Some(nav)));
    let ui = store.ui();
    let navigator = SwapNavigator::new(&home_route())
        .screen(home_route(), move |_| {
            ui.place.set(Place::Home);
            Screen::new(ui! { HomeStage() })
        })
        .screen(direct_route(), move |p: DirectParams| {
            let open = p.channel;
            ui.place.set(Place::Direct(open));
            Screen::new(ui! { ChannelStage(channel = open) })
        })
        .screen(channel_route(), move |p: ChannelParams| {
            let open = p.channel;
            ui.place.set(Place::Channel { space: p.space, channel: open });
            Screen::new(ui! { ChannelStage(channel = open) })
        })
        .layout(move |nav| {
            ui! {
                AppShell(sidebar = vec![ui! { Sidebar() }], is_open = ui.drawer, pin_at = PIN_AT, width = SIDEBAR_WIDTH) {
                    { nav.outlet }
                }
            }
        })
        .bind(nav);

    // One root that fills: several roots would be wrapped in a plain box
    // that does not grow, and everything below it would have no height.
    ui! {
        view(style = SessionRoot()) {
            match device.get() {
                Some(d) => { LiveLink(device = *d) }
                None => { runtime_core::fragment(Vec::new()) }
            }
            navigator
            Overlays()
        }
    }
}

stylesheet! {
    SessionRoot<()> {
        base(_t) {
            flex_grow: 1.0,
            min_height: 0.0,
            flex_direction: FlexDirection::Column,
            position: runtime_core::Position::Relative,
        }
    }
}

/// The context panel's toggle state, for a stage's header button.
pub fn toggle_panel(which: Panel) {
    let ui = crate::store::store().ui();
    ui.panel.set(if ui.panel.peek() == Some(which) { None } else { Some(which) });
}
