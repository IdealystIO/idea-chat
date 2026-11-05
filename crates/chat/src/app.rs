//! The root: theme, where the server is, and whether anyone is signed in.

use std::cell::Cell;

use chat_model::Id;
use idea_ui::{install_idea_theme_reactive, IdeaThemeRef, Spinner, SpinnerSize, ToastHost, ToastPlacement};
use runtime_core::{component, signal, stylesheet, ui, AlignItems, Element, FlexDirection, JustifyContent, Length, Signal};

use crate::auth::AuthScreens;
use crate::platform;
use crate::shell::SignedIn;

/// Who, if anyone, is signed in. Keyed on the account only: everything else
/// about them is live state, so a profile change rebuilds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Checking,
    Out,
    In(Id),
}

/// A link into the app that arrived with the page, waiting to be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Verify { email: String, code: String },
    Reset { email: String, code: String },
    Invite(String),
}

thread_local! {
    static SESSION: Cell<Option<Signal<Session>>> = const { Cell::new(None) };
    static ENTRY: std::cell::RefCell<Option<Entry>> = const { std::cell::RefCell::new(None) };
}

fn session() -> Signal<Session> {
    SESSION.with(|s| s.get()).expect("the root sets the session signal")
}

/// Signed out — by choice, or because the server says so.
pub fn signed_out() {
    platform::clear_secret("session");
    session().set(Session::Out);
}

/// A session started: keep its token (native) and go in.
pub fn signed_in(s: chat_api::auth::Session) {
    if let Some(token) = &s.token {
        platform::set_secret("session", token);
    }
    session().set(Session::In(s.me.user.id));
}

/// Take the link the page arrived with, if any and if it is of a kind.
pub fn take_entry(pick: impl Fn(&Entry) -> bool) -> Option<Entry> {
    ENTRY.with(|e| {
        let mut e = e.borrow_mut();
        if e.as_ref().is_some_and(&pick) {
            platform::clear_entry_link();
            e.take()
        } else {
            None
        }
    })
}

fn read_entry() -> Option<Entry> {
    let (path, query) = platform::entry_link()?;
    let get = |k: &str| query.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    match path.trim_end_matches('/') {
        "/verify" => Some(Entry::Verify { email: get("email")?, code: get("code")? }),
        "/reset" => Some(Entry::Reset { email: get("email")?, code: get("code")? }),
        p if p.starts_with("/invite/") => Some(Entry::Invite(p.trim_start_matches("/invite/").to_string())),
        _ => None,
    }
}

stylesheet! {
    /// The window, in the theme's colours; eased so the theme toggle fades.
    Root<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            height: Length::pct(100.0),
            width: Length::pct(100.0),
            background: t.color.background(),
            color: t.color.text(),
        }
        transitions {
            background: 200ms EaseInOut,
            color: 200ms EaseInOut,
        }
    }
}

stylesheet! {
    Splash<()> {
        base(_t) {
            flex_grow: 1.0,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
        }
    }
}

/// The theme the person chose, or the system's.
pub fn initial_dark() -> bool {
    match platform::pref("theme").as_deref() {
        Some("dark") => true,
        Some("light") => false,
        _ => matches!(runtime_core::color_scheme(), runtime_core::ColorScheme::Dark),
    }
}

#[component]
pub fn App() -> Element {
    let dark = signal(initial_dark());
    crate::DARK.with(|d| d.set(Some(dark)));
    install_idea_theme_reactive(move || if dark.get() { crate::theme::dark() } else { crate::theme::light() });
    platform::configure_server();

    let state = signal(Session::Checking);
    SESSION.with(|s| s.set(Some(state)));
    ENTRY.with(|e| *e.borrow_mut() = read_entry());

    runtime_core::spawn_then(chat_api::auth::whoami(), move |me| match me {
        Ok(me) => state.set(Session::In(me.user.id)),
        Err(_) => state.set(Session::Out),
    });

    ui! {
        view(style = Root()) {
            match state.get() {
                Session::Checking => {
                    view(style = Splash()) { Spinner(size = SpinnerSize::Large) }
                }
                Session::Out => { AuthScreens() }
                Session::In(user) => { SignedIn(me = *user, dark = dark) }
            }
            ToastHost(placement = ToastPlacement::BottomRight, edge_gap = 16.0)
        }
    }
}
