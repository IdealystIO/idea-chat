//! A call's media: the relay link, the end-to-end key, the mic, camera and
//! screen. Web only — native has no audio output or codecs yet.

use std::cell::Cell;
use std::collections::BTreeSet;

use chat_model::{CallTicket, Id};
use icons_lucide::MONITOR_UP;
use idea_ui::{typography_kind, AvatarSize, IdeaThemeRef, Typography};
use runtime_core::{component, memo, signal, stylesheet, ui, AlignItems, Element, FlexDirection, FlexWrap, JustifyContent, Signal};

use crate::bits::PersonAvatar;
use crate::store::store;

/// How the call's link is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Connecting,
    Connected,
    /// The link dropped without us leaving; rejoining.
    Reconnecting,
    Unsupported,
    Failed,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Idle => "Not connected",
            Status::Connecting => "Connecting…",
            Status::Connected => "Voice connected",
            Status::Reconnecting => "Reconnecting…",
            Status::Unsupported => "Calls need the web app for now",
            Status::Failed => "Couldn't connect",
        }
    }
}

thread_local! {
    static STATUS: Cell<Option<Signal<Status>>> = const { Cell::new(None) };
    static SPEAKING: Cell<Option<Signal<BTreeSet<Id>>>> = const { Cell::new(None) };
    static REJOINING: Cell<Option<Signal<Option<Id>>>> = const { Cell::new(None) };
}

/// The channel whose call we lost and are rejoining, if any — so the call
/// tray stays up through it, though the server no longer lists us.
pub fn rejoining() -> Signal<Option<Id>> {
    REJOINING.with(|s| match s.get() {
        Some(s) => s,
        None => {
            let sig = signal(None);
            REJOINING.with(|c| c.set(Some(sig)));
            sig
        }
    })
}

/// Who in our call is talking right now — us included — by each one's own
/// voice detection. Empty outside a call.
pub fn speaking() -> Signal<BTreeSet<Id>> {
    SPEAKING.with(|s| match s.get() {
        Some(s) => s,
        None => {
            let sig = signal(BTreeSet::new());
            SPEAKING.with(|c| c.set(Some(sig)));
            sig
        }
    })
}

pub fn status() -> Signal<Status> {
    STATUS.with(|s| match s.get() {
        Some(s) => s,
        None => {
            let sig = signal(Status::Idle);
            STATUS.with(|c| c.set(Some(sig)));
            sig
        }
    })
}

/// Get the room key, dial the relay and open the mic.
pub fn start(ticket: CallTicket, muted: bool, camera: bool) {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = camera;
        status().set(Status::Connecting);
        web::start(ticket, muted);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (ticket, muted, camera);
        status().set(Status::Unsupported);
    }
}

/// Hang up: the mic is released and playback stops.
pub fn stop() {
    #[cfg(target_arch = "wasm32")]
    web::stop();
    status().set(Status::Idle);
    speaking().set(BTreeSet::new());
    rejoining().set(None);
}

/// What we send and hear. Camera and screen are not wired to media yet.
pub fn set(muted: bool, deafened: bool, camera: bool, screen: bool) {
    let _ = (camera, screen);
    #[cfg(target_arch = "wasm32")]
    web::set(muted, deafened);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (muted, deafened);
}

/// The browser side: the room key's exchange, and the call session.
///
/// ```text
/// ticket.make_key ──▶ generate the room key
/// otherwise       ──▶ request_key, every 2 s, until a KeyDelivered
///                     sealed to this device opens
/// then            ──▶ call_session::join(relay, grant, key)
/// ```
///
/// While we hold a room's key we answer every `KeyRequested` for it by
/// sealing the key to the asking device. A device can ask before the one
/// that made the key has it in hand — that one may still be reading its
/// ticket — so asking repeats until something answers.
#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use call_session::{Access, Call, Renew, Settings};
    use chat_keys::{Context, PublicKey};
    use chat_model::live::Event;
    use chat_model::{CallTicket, Id, SealedKey};
    use runtime_core::driver::spawn_async;

    use super::{rejoining, speaking, status, Status};
    use crate::sounds::{self, Cue};
    use crate::store::Conn;
    use crate::platform;
    use crate::store::store;

    /// Rejoining a lost call: the first wait, the longest, and how long in
    /// all before giving up.
    const REJOIN_FIRST_MS: i32 = 1_000;
    const REJOIN_MAX_MS: i32 = 8_000;
    const REJOIN_FOR_MS: f64 = 120_000.0;

    /// How long to keep asking for a room key before giving up.
    const KEY_TRIES: u32 = 10;
    const KEY_RETRY_MS: i32 = 2_000;

    /// The room we are in, and its key once we have it.
    struct Room {
        id: Id,
        key: Option<String>,
    }

    thread_local! {
        /// The one live call. Dropping it hangs up and releases the mic.
        static LIVE: Rc<RefCell<Option<Call>>> = Rc::default();
        static ROOM: RefCell<Option<Room>> = const { RefCell::new(None) };
        /// Bumped by every start and stop, so a join that finishes after
        /// the person left (or moved) knows to drop what it opened.
        static GENERATION: Cell<u64> = const { Cell::new(0) };
        /// What we send, as last asked: applied to a call the moment it opens.
        static MUTED: Cell<bool> = const { Cell::new(false) };
        static DEAFENED: Cell<bool> = const { Cell::new(false) };
        static LISTENING: Cell<bool> = const { Cell::new(false) };
        /// When we started trying to get back into a lost call, and which.
        static REJOIN_SINCE: Cell<Option<(Id, f64)>> = const { Cell::new(None) };
        /// Who else was in our call, as last seen — kept through a loss, so
        /// the call we get back into knows whom to expect.
        static COMPANY: RefCell<Vec<Id>> = const { RefCell::new(Vec::new()) };
    }

    fn live() -> Rc<RefCell<Option<Call>>> {
        LIVE.with(|l| l.clone())
    }

    fn bump() -> u64 {
        GENERATION.with(|g| {
            g.set(g.get() + 1);
            g.get()
        })
    }

    fn current(generation: u64) -> bool {
        GENERATION.with(|g| g.get()) == generation
    }

    fn room_key() -> Option<(Id, String)> {
        ROOM.with(|r| r.borrow().as_ref().and_then(|r| r.key.clone().map(|k| (r.id, k))))
    }

    pub fn start(ticket: CallTicket, muted: bool) {
        listen();
        *live().borrow_mut() = None;
        let generation = bump();
        MUTED.with(|m| m.set(muted));
        let channel = ticket.channel;
        let key = ticket.make_key.then(|| call_e2ee::RoomKey::generate(platform::random::<16>()).1);
        ROOM.with(|r| *r.borrow_mut() = Some(Room { id: ticket.room, key: key.clone() }));
        spawn_async(async move {
            let key = match key {
                Some(k) => k,
                None => match ask_for_key(ticket.room, generation).await {
                    Some(k) => k,
                    None if !current(generation) => return,
                    // Nobody holds it — the server's picture of the room may
                    // be stale (people from a server that died) — so try
                    // again from the top, as for a lost link.
                    None => return lost(generation, channel, false),
                },
            };
            if !current(generation) {
                return;
            }
            let access = Access {
                // As the grant names it (`Calls::grant`): no dashes.
                room: ticket.room.simple().to_string(),
                grant: Some(ticket.grant.clone()),
                renew: Some(Renew::With(Rc::new(|| {
                    Box::pin(async { chat_api::calls::renew_grant().await.map_err(|e| chat_api::describe(&e)) })
                }))),
                key: call_e2ee::RoomKey::from_text(&key),
            };
            // The relay says "revoked" before it closes a link the server
            // took out of the call; anything else closing it is a loss.
            let revoked = Rc::new(Cell::new(false));
            let on_control = move |text: String| match text.as_str() {
                "revoked" => revoked.set(true),
                "link closed" => lost(generation, channel, revoked.get()),
                _ => {}
            };
            let joined = call_session::join(&ticket.relay_url, &ticket.cert_hash, access, settings(), on_control).await;
            if !current(generation) {
                return;
            }
            match joined {
                Ok(call) => {
                    *live().borrow_mut() = Some(call);
                    apply();
                    status().set(Status::Connected);
                    rejoining().set(None);
                    let was_lost = REJOIN_SINCE.with(|s| s.take()).is_some();
                    sounds::play(if was_lost { Cue::Back } else { Cue::Joined });
                    watch_speaking(generation);
                    watch_people(generation, channel, was_lost);
                }
                Err(e) => fail(&e),
            }
        });
    }

    /// The call's link closed under us. Unless we left (which moves the
    /// generation on), let go of the dead call — releasing the mic — and get
    /// back in.
    ///
    /// Taken out by the server ("revoked"), we wait for the live connection
    /// to be back with a fresh picture: our connection blinking out also
    /// takes us out, and that is worth undoing, but if the server lists us
    /// in a call now it is from another device, which took it over. Lost
    /// any other way (the server or relay went down, the network), we just
    /// rejoin; joining waits on the live connection by itself.
    fn lost(generation: u64, channel: Id, revoked: bool) {
        if !current(generation) {
            return;
        }
        // Already rejoining this channel: keep the clock that started then,
        // and the cue that said so.
        let since = REJOIN_SINCE.with(|s| match s.get() {
            Some((c, t)) if c == channel => t,
            _ => {
                let t = call_audio::now_ms();
                s.set(Some((channel, t)));
                // Taken out by the server is not a dropped line: it may be
                // another device taking over, which plays "left" once known.
                if !revoked {
                    sounds::play(Cue::Lost);
                }
                t
            }
        });
        let generation = bump();
        // Not dropped here: this runs inside the call's own receive loop.
        spawn_async(async move {
            *live().borrow_mut() = None;
        });
        speaking().set(Default::default());
        status().set(Status::Reconnecting);
        rejoining().set(Some(channel));
        call_session::log(&format!("[chat] call: link lost ({}), rejoining", if revoked { "revoked" } else { "dropped" }));
        spawn_async(async move {
            let started = since;
            let mut wait = REJOIN_FIRST_MS;
            loop {
                sleep(wait).await;
                wait = (wait * 2).min(REJOIN_MAX_MS);
                if !current(generation) {
                    return;
                }
                if call_audio::now_ms() - started > REJOIN_FOR_MS {
                    rejoining().set(None);
                    REJOIN_SINCE.with(|s| s.set(None));
                    sounds::play(Cue::Left);
                    return fail("Lost the call and couldn't get back in. Join again when you're back online.");
                }
                let s = store();
                if s.conn().peek() != Conn::Open {
                    continue;
                }
                if revoked && s.read(|st| st.my_call().is_some()) {
                    // Another of our devices has the call now.
                    rejoining().set(None);
                    REJOIN_SINCE.with(|s| s.set(None));
                    sounds::play(Cue::Left);
                    status().set(Status::Idle);
                    return;
                }
                let Some((device, _)) = crate::device::current() else { continue };
                match chat_api::calls::join_call(channel, device).await {
                    // `resume` starts the call afresh, moving the generation on.
                    Ok(ticket) if current(generation) => return crate::calls::resume(ticket),
                    Ok(_) => return,
                    Err(e) if chat_api::is_signed_out(&e) => return,
                    Err(e) => call_session::log(&format!("[chat] call: rejoin: {}", chat_api::describe(&e))),
                }
            }
        });
    }

    /// How often the call's company is checked for comings and goings.
    const PEOPLE_MS: i32 = 200;
    /// Someone gone this long is gone: shorter, and it was a reconnect —
    /// theirs, which they would rather nobody heard.
    const LEAVE_GRACE_MS: f64 = 2_000.0;
    /// Back from a lost call, how long the people who were in it with us
    /// have to turn up again before they count as gone: they are
    /// reconnecting too, on their own backoff.
    const REJOIN_GRACE_MS: f64 = 15_000.0;

    /// Play a cue when someone comes into or leaves this call, while it
    /// lasts.
    ///
    /// By user, from the server's picture, never from the relay's: a
    /// reconnect renumbers everyone at the relay. Whoever is there when we
    /// arrive is the starting point, not news — so joining a full call, or
    /// getting back into one, is one cue rather than one per person. Nothing
    /// plays while we are deafened (we said we wanted quiet), or after the
    /// call ends (a new generation means a new watcher with its own
    /// starting point).
    ///
    /// Back from a lost call (`was_lost`), the people who were in it with us
    /// are expected back, not announced: whoever of us rejoins first must
    /// not hear the other "arrive".
    fn watch_people(generation: u64, channel: Id, was_lost: bool) {
        let s = store();
        let me = s.me();
        spawn_async(async move {
            let mut known: Option<std::collections::BTreeSet<Id>> = None;
            let mut leaving: std::collections::BTreeMap<Id, f64> = Default::default();
            if was_lost {
                let until = call_audio::now_ms() + REJOIN_GRACE_MS;
                for user in COMPANY.with(|c| c.borrow().clone()) {
                    leaving.insert(user, until);
                }
            }
            while current(generation) {
                let here: std::collections::BTreeSet<Id> =
                    s.read(|st| st.in_call(channel).iter().map(|v| v.user).filter(|u| Some(*u) != me).collect());
                let quiet = DEAFENED.with(Cell::get);
                let now = call_audio::now_ms();
                if let Some(before) = &known {
                    for user in here.difference(before) {
                        // Back within the grace: a reconnect, not news.
                        if leaving.remove(user).is_none() && !quiet {
                            sounds::play(Cue::PersonJoined);
                        }
                    }
                    for user in before.difference(&here) {
                        leaving.insert(*user, now + LEAVE_GRACE_MS);
                    }
                }
                leaving.retain(|user, at| {
                    if here.contains(user) {
                        return false;
                    }
                    if now >= *at {
                        if !quiet {
                            sounds::play(Cue::PersonLeft);
                        }
                        return false;
                    }
                    true
                });
                COMPANY.with(|c| *c.borrow_mut() = here.iter().copied().collect());
                known = Some(here);
                sleep(PEOPLE_MS).await;
            }
        });
    }

    /// How often who-is-talking is read off the call for the page.
    const SPEAKING_MS: i32 = 80;

    /// Keep [`speaking`] current while this call lasts: us from our own
    /// detection (no round trip, so we light with our voice), everyone else
    /// from what they announce, mapped from relay numbers to users by their
    /// grants.
    fn watch_speaking(generation: u64) {
        let me = store().me();
        spawn_async(async move {
            while current(generation) {
                let now: std::collections::BTreeSet<Id> = {
                    let live = live();
                    let Some(call) = live.borrow().as_ref().map(|call| {
                        let roster = call.roster.borrow();
                        let mut set: std::collections::BTreeSet<Id> = roster
                            .speaking
                            .iter()
                            .filter_map(|id| roster.names.get(id))
                            .filter_map(|user| Id::parse_str(user).ok())
                            .collect();
                        if let Some(me) = me.filter(|_| call.speaking()) {
                            set.insert(me);
                        }
                        set
                    }) else {
                        break;
                    };
                    call
                };
                let sig = speaking();
                if sig.peek() != now {
                    sig.set(now);
                }
                sleep(SPEAKING_MS).await;
            }
        });
    }

    fn fail(why: &str) {
        call_session::log(&format!("[chat] call: {why}"));
        status().set(Status::Failed);
        idea_ui::push_toast(why.to_string(), idea_ui::tone::Danger);
    }

    pub fn stop() {
        let was_in = live().borrow().is_some() || REJOIN_SINCE.with(|s| s.get()).is_some();
        bump();
        REJOIN_SINCE.with(|s| s.set(None));
        if was_in {
            sounds::play(Cue::Left);
        }
        ROOM.with(|r| *r.borrow_mut() = None);
        *live().borrow_mut() = None;
    }

    pub fn set(muted: bool, deafened: bool) {
        MUTED.with(|m| m.set(muted));
        DEAFENED.with(|d| d.set(deafened));
        apply();
    }

    /// Push mute and deafen onto the live call, if there is one.
    fn apply() {
        let (muted, deafened) = (MUTED.with(Cell::get), DEAFENED.with(Cell::get));
        let live = live();
        call_session::set_muted(&live, muted);
        if let Some(call) = live.borrow().as_ref() {
            call.set_default_volume(if deafened { 0.0 } else { 1.0 });
        };
    }

    fn settings() -> Settings {
        Settings {
            processing: call_audio::Processing {
                echo_cancellation: true,
                suppression: call_audio::Suppression::Browser,
                auto_gain_control: false,
            },
            voice: call_audio::VoiceStages::default(),
            model: call_audio::ModelSettings::default(),
            model_path: "",
            gains: Default::default(),
            default_gain: if DEAFENED.with(Cell::get) { 0.0 } else { 1.0 },
        }
    }

    /// Ask the room's devices for its key until one arrives, the person
    /// leaves, or we give up.
    async fn ask_for_key(room: Id, generation: u64) -> Option<String> {
        for _ in 0..KEY_TRIES {
            if !current(generation) {
                return None;
            }
            if let Some((r, k)) = room_key() {
                if r == room {
                    return Some(k);
                }
            }
            if let Err(e) = chat_api::calls::request_key(room).await {
                call_session::log(&format!("[chat] request_key: {}", chat_api::describe(&e)));
            }
            sleep(KEY_RETRY_MS).await;
            if let Some((r, k)) = room_key() {
                if r == room {
                    return Some(k);
                }
            }
        }
        None
    }

    /// Hear key requests and deliveries, once for the app's life.
    fn listen() {
        if LISTENING.with(|l| l.replace(true)) {
            return;
        }
        install_probe();
        store().on_key_event(Rc::new(|event| match event {
            Event::KeyRequested { request } => {
                let Some((room, key)) = room_key() else { return };
                let Some((device, mine)) = crate::device::current() else { return };
                if request.room != room || request.device == device {
                    return;
                }
                let Ok(to) = PublicKey::from_text(&request.public_key) else { return };
                let (room_s, from_s, to_s) = (room.to_string(), device.to_string(), request.device.to_string());
                let context = Context { room: &room_s, sender_device: &from_s, recipient_device: &to_s };
                let Ok(sealed) = chat_keys::seal(&key, &mine, &to, context, platform::random::<32>()) else { return };
                let sealed = SealedKey { room, from_device: device, from_key: mine.public().as_text(), to_device: request.device, sealed };
                spawn_async(async move {
                    if let Err(e) = chat_api::calls::deliver_key(sealed).await {
                        call_session::log(&format!("[chat] deliver_key: {}", chat_api::describe(&e)));
                    }
                });
            }
            Event::KeyDelivered { key } => {
                let Some((device, mine)) = crate::device::current() else { return };
                if key.to_device != device {
                    return;
                }
                let waiting = ROOM.with(|r| r.borrow().as_ref().is_some_and(|r| r.id == key.room && r.key.is_none()));
                if !waiting {
                    return;
                }
                let Ok(from) = PublicKey::from_text(&key.from_key) else { return };
                let (room_s, from_s, to_s) = (key.room.to_string(), key.from_device.to_string(), device.to_string());
                let context = Context { room: &room_s, sender_device: &from_s, recipient_device: &to_s };
                match chat_keys::open(&key.sealed, &mine, &from, context) {
                    Ok(text) => ROOM.with(|r| {
                        if let Some(r) = r.borrow_mut().as_mut() {
                            r.key = Some(text.to_string());
                        }
                    }),
                    Err(e) => call_session::log(&format!("[chat] a call key did not open: {e:?}")),
                }
            }
            _ => {}
        }));
    }

    /// Publish the call's numbers as `globalThis.__chatCallStats()`.
    ///
    /// A seam for the end-to-end test, like Hearth's: whether audio is
    /// actually moving, and sealed, does not show in the UI. Peaks are
    /// "since last read".
    fn install_probe() {
        use wasm_bindgen::prelude::*;
        let probe = Closure::wrap(Box::new(|| -> String {
            let live = live();
            let Some(call) = live.borrow().as_ref().map(|c| {
                let mut st = c.stats.borrow_mut();
                let s = *st;
                st.mic_peak = 0.0;
                st.out_peak = 0.0;
                let fresh = call_audio::now_ms() - s.remote_peak_ms < 2.0 * call_audio::PEAK_WINDOW_MS;
                let t = *c.traffic.borrow();
                format!(
                    r#"{{"live":true,"encrypted":{},"muted":{},"sent":{},"unsent":{},"recv":{},"peers":{},"unreadable":{},"micPeak":{},"remotePeak":{},"speaking":{},"othersSpeaking":{},"speakingUsers":{}}}"#,
                    c.encrypted(),
                    c.muted(),
                    s.sent,
                    s.unsent,
                    s.received,
                    s.peers,
                    t.unreadable,
                    (s.mic_peak * 100.0) as u32,
                    ((if fresh { s.remote_peak } else { 0.0 }) * 100.0) as u32,
                    c.speaking(),
                    c.roster.borrow().speaking.len(),
                    speaking().peek().len(),
                )
            }) else {
                return r#"{"live":false}"#.to_string();
            };
            call
        }) as Box<dyn Fn() -> String>);
        let _ = js_sys::Reflect::set(&js_sys::global(), &JsValue::from_str("__chatCallStats"), probe.as_ref());
        probe.forget();
    }

    /// The browser's timer: `runtime_core::driver` spawns but cannot wait.
    async fn sleep(ms: i32) {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            if let Some(w) = web_sys::window() {
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
            }
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

stylesheet! {
    Grid<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            min_height: 0.0,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            gap: 12.0,
            padding_left: 16.0,
            padding_right: 16.0,
            padding_top: 16.0,
            padding_bottom: 16.0,
            background: t.color.background(),
        }
    }
}

stylesheet! {
    Tile<IdeaThemeRef> {
        base(t) {
            width: 220.0,
            height: 140.0,
            border_radius: 14.0,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            gap: 8.0,
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
        }
    }
}

/// The call you are in, on the stage.
#[component]
pub fn Room(#[prop(static)] channel: Id) -> Element {
    let s = store();
    let people = {
        let s = s.clone();
        memo(move || s.read(|st| st.in_call(channel).iter().map(|v| (v.user, st.display_name(v.user))).collect::<Vec<_>>()))
    };
    let _ = MONITOR_UP;
    ui! {
        view(style = Grid()) {
            for (user, name) in people, key = user.to_string() {
                view(style = Tile()) {
                    PersonAvatar(user = user, size = AvatarSize::Lg, presence = false, speaking = true)
                    Typography(content = name.clone(), kind = typography_kind::BodySm)
                }
            }
        }
    }
}
