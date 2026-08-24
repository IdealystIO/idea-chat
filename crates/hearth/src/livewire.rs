//! The client's end of the live connection.
//!
//! Opens one socket, holds it for the life of the session, and folds what
//! arrives into the app's state with [`crate::live::Live::apply`] — the same
//! function the server runs. Nothing here polls, and nothing here decides:
//! the client sends actions and applies facts.
//!
//! # Reconnecting
//!
//! A dropped socket is not an error state, it is a normal thing that happens
//! to laptops. Reconnecting takes a **fresh snapshot** rather than resuming,
//! which is why the server keeps no event log — and why a detected gap is
//! handled by reconnecting rather than by asking for the missing frames.

use std::cell::RefCell;

use idea_state::{Cursor, Down, Model, Step, Up};
use runtime_core::driver::spawn_async;
use server::{Socket, SocketSender};

use crate::live::{Action, Event, Live, State};
use crate::model::{App, Phase};

/// Frames in, frames out.
type Sock = Socket<Down<State, Event>, Up<Action>>;

thread_local! {
    /// The send half of the live connection, when there is one.
    ///
    /// Separate from the receive loop because sending happens from the UI —
    /// a click — while receiving owns the socket in its own task. `Rc` inside,
    /// which is fine: the browser is one thread.
    static LINK: RefCell<Option<SocketSender<Up<Action>>>> = const { RefCell::new(None) };
}

/// How long to wait before reconnecting, and how far that backs off.
///
/// Starts fast because the overwhelmingly common cause is a sleep/wake or a
/// blip, where a quick retry is invisible; backs off so a server that is
/// genuinely down is not hammered by every open tab.
const RECONNECT_MIN_MS: u32 = 500;
const RECONNECT_MAX_MS: u32 = 15_000;

/// How often to check whether there is a session yet.
///
/// Only runs before the first connection and after a sign-out, so it is a
/// short-lived poll rather than a standing one — and it is checking a local
/// signal, not asking the server anything.
const SIGNED_IN_POLL_MS: u32 = 150;

async fn sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

fn socket_url() -> String {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_else(|| "http://127.0.0.1:8090".to_string());
    // Same origin, so the session cookie is sent with the upgrade and a page
    // on https gets wss without anything having to be configured.
    let ws = origin.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    format!("{ws}/_srv/_ws/live")
}

/// Tell the server which channel this client is reading.
///
/// The server replies by restating the world with that channel's page in it,
/// so switching channels is one round trip and no request — and every message
/// posted there from then on arrives as an event.
pub fn watch(channel_id: &str) -> bool {
    WATCHED.with(|w| *w.borrow_mut() = Some(channel_id.to_string()));
    send(Action::Watch { channel_id: channel_id.to_string() })
}

thread_local! {
    /// The channel last asked for, shared with the receive loop so a snapshot
    /// arriving after a navigation does not ask for it a second time.
    static WATCHED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Ask the server for something.
///
/// Returns `false` if there is no connection right now. Callers treat that as
/// "it will be true again shortly" rather than an error: the reconnect brings
/// a fresh snapshot, and anything that matters is re-asserted by the heartbeat.
pub fn send(action: Action) -> bool {
    LINK.with(|l| {
        l.borrow()
            .as_ref()
            .map(|tx| tx.send(Up::Action(action)).is_ok())
            .unwrap_or(false)
    })
}

/// Whether the live connection is up.
pub fn connected() -> bool {
    LINK.with(|l| l.borrow().is_some())
}

/// Open the connection and keep it open for as long as the app is running.
pub fn start(app: App) {
    spawn_async(async move {
        let mut backoff = RECONNECT_MIN_MS;
        loop {
            // Never open a socket without a session. The upgrade would be
            // refused — correctly — and a refused handshake currently throws
            // out of the SDK's WebSocket ("closure invoked recursively or
            // after being dropped"), so an app that connected first and signed
            // in second would log an uncaught error on every load. Waiting is
            // also just right: there is nothing to synchronize until we know
            // who is asking.
            while app.phase.get() != Phase::Ready {
                sleep(SIGNED_IN_POLL_MS).await;
            }

            match Sock::connect(&socket_url()).await {
                Ok(sock) => {
                    backoff = RECONNECT_MIN_MS;
                    run(app, sock).await;
                }
                Err(_) => {
                    // Signed out, server down, network gone — all the same
                    // from here, and all handled by trying again.
                }
            }
            LINK.with(|l| *l.borrow_mut() = None);
            app.live_up.set(false);
            sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_MAX_MS);
        }
    });
}

/// Drive one connection until it ends.
async fn run(app: App, mut sock: Sock) {
    LINK.with(|l| *l.borrow_mut() = Some(sock.sender()));

    let mut cursor = Cursor::default();
    // What this connection has already asked to watch.
    //
    // Tracked explicitly rather than inferred from the transcript: an empty
    // channel has an empty transcript, so inferring made "have I asked?" and
    // "did it have anything in it?" the same question — and every snapshot for
    // an empty channel asked again, which the server answered with another
    // snapshot. A tight loop that presents as a UI that will not hold still.
    WATCHED.with(|w| *w.borrow_mut() = None);
    while let Some(frame) = sock.recv().await {
        let Ok(frame) = frame else { continue };
        match frame {
            Down::Snapshot { seq, state } => {
                // Replace, never merge. A snapshot is the whole truth as of
                // `seq`; merging it into whatever was there would preserve
                // exactly the stale entries it exists to clear.
                cursor = Cursor::at(seq);
                put(app, state, true);
                app.live_up.set(true);
                // A fresh connection does not know what this client is
                // reading. Saying so is what fills the transcript, so it
                // happens on every snapshot — including the one after a
                // reconnect, which is exactly when the page has to come back.
                let open = app.channel_id.get();
                let asked = WATCHED.with(|w| w.borrow().clone());
                if !open.is_empty() && asked.as_deref() != Some(open.as_str()) {
                    watch(&open);
                }
                // Re-assert the call, immediately, for the same reason.
                //
                // The call is a different socket and survived whatever
                // happened to this one; the server shortened our presence lease
                // on the way out and is waiting to see whether anyone renews
                // it. Waiting for the next scheduled heartbeat would leave up
                // to fifteen seconds in which we could be reaped and announced
                // as having left a call we never left.
                //
                // Costs nothing when the lease is still ours: the server treats
                // a heartbeat for the room we are already in as news to nobody
                // and publishes nothing — so no tone, and no reshuffled
                // sidebar.
                // ...or say that we are in none, which is what a refresh looks
                // like. The server may still hold the lease from the socket
                // that went away, and it cannot tell that from a blip on its
                // own — only the client knows whether it came back into the
                // same call. Without this, a refreshed tab sits in the sidebar
                // until its lease expires.
                let room = app.in_voice.get();
                if room.is_empty() {
                    send(Action::NotInVoice);
                } else {
                    send(Action::StillHere { channel_id: room });
                }
            }
            Down::Event { seq, event } => match cursor.step(seq) {
                Step::Apply => {
                    // Before folding, so "is this new to me" is still
                    // answerable — afterwards the state already contains it.
                    announce(app, &event);
                    fold(app, &event);
                }
                // A resnapshot overlapped an event already folded in.
                Step::Duplicate => {}
                Step::Gap => {
                    // Something was missed, so the state is no longer
                    // trustworthy. Dropping the socket is the fix: the
                    // reconnect brings a snapshot. Carrying on would leave a
                    // client that looks fine and is wrong.
                    break;
                }
            },
            Down::Refused { reason } => app.voice_error.set(reason),
            Down::Pong => {}
        }
    }
}

/// Fold one event into the app's state, with the server's own reducer.
///
/// Read, apply, write. The signals are storage; [`Live::apply`] is the only
/// thing that ever computes what they should hold — so there is no second
/// place where "a message arrived" is turned into a change.
fn fold(app: App, event: &Event) {
    let before = take(app);
    let mut state = before.clone();
    Live::apply(&mut state, event);
    if state != before {
        let structural = state.voice != before.voice
            || state.unread != before.unread
            || state.transcript != before.transcript;
        put(app, state, structural);
    }
}

/// Make a noise, if this event is one of the few that earns it.
///
/// The rules live here rather than in `sound` because they are about *this
/// client's* situation — which room it is in, which channel it is reading, and
/// who it is — and none of that is the tone's business.
fn announce(app: App, event: &Event) {
    use crate::sound::{play, Tone};

    match event {
        // Somebody else, in the channel being read. Your own message is not
        // news to you, and a channel you are not looking at gets an unread
        // pip, which is the quiet version of the same information.
        Event::MessagePosted { channel_id, message } => {
            let mine = message.author_id.as_deref() == Some(app.my_id().as_str());
            if !mine && channel_id == &app.channel_id.get() {
                play(Tone::Message);
            }
        }
        // Only about the room you are standing in. Someone arriving somewhere
        // else is not an interruption you agreed to.
        Event::VoiceJoined { channel_id, .. } => {
            if channel_id == &app.in_voice.get() {
                play(Tone::Join);
            }
        }
        Event::VoiceLeft { channel_id, user_id, .. } => {
            // Your own departure is announced by the same event everyone else
            // gets, and it should confirm the click — but by then `in_voice`
            // has already been cleared locally, so it is matched on identity
            // rather than on the room.
            if channel_id == &app.in_voice.get() || user_id == &app.my_id() {
                play(Tone::Leave);
            }
        }
        _ => {}
    }
}

/// Assemble the live state out of the signals that store it.
fn take(app: App) -> State {
    State {
        voice: app.voice_rooms.with(|v| v.clone()),
        speaking: app.speaking_now.with(|s| s.clone()),
        unread: app.unread.with(|u| u.clone()),
        // What this client is reading is a local fact the server is told, not
        // one it is asked for — so it is read from the selection rather than
        // held twice.
        watching: Some(app.channel_id.get()).filter(|c| !c.is_empty()),
        transcript: app.messages.with(|m| m.clone()),
    }
}

/// Write it back out.
///
/// `structural` says whether anything a `switch` keys on `revision` cares about
/// actually moved. It is false for the one event that arrives constantly —
/// somebody starting or stopping talking — and that distinction is load
/// bearing: `speaking_now` has its own signal and its own subscribers, so
/// bumping the revision for it rebuilt the whole roster several times a second
/// during every call. A menu opened on one of those rows was destroyed and
/// remade under the pointer, which makes a slider inside it undraggable.
fn put(app: App, state: State, structural: bool) {
    app.voice_rooms.set(state.voice);
    app.speaking_now.set(state.speaking);
    app.unread.set(state.unread);
    if let Some(watching) = state.watching {
        // Only when it still describes what we are looking at: a snapshot for
        // the previous channel must not paint itself over the current one.
        if watching == app.channel_id.get() {
            app.messages.set(state.transcript);
        }
    }
    if structural {
        app.touch();
    }
}
