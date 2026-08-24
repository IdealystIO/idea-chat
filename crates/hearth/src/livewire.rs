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
    while let Some(frame) = sock.recv().await {
        let Ok(frame) = frame else { continue };
        match frame {
            Down::Snapshot { seq, state } => {
                // Replace, never merge. A snapshot is the whole truth as of
                // `seq`; merging it into whatever was there would preserve
                // exactly the stale entries it exists to clear.
                cursor = Cursor::at(seq);
                app.voice_rooms.set(state.voice);
                app.live_up.set(true);
            }
            Down::Event { seq, event } => match cursor.step(seq) {
                Step::Apply => fold(app, &event),
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
fn fold(app: App, event: &Event) {
    // Read, apply, write. The signal holds the projection the UI renders; the
    // reducer is the only thing that ever computes it.
    let mut state = State { voice: app.voice_rooms.with(|v| v.clone()) };
    Live::apply(&mut state, event);
    app.voice_rooms.set(state.voice);
}
