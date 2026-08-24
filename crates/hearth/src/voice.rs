//! The live voice session — the one piece of the app that is not a request.
//!
//! Everything else Hearth does is a round trip: ask the server, render the
//! answer. A call is a socket that stays open, a microphone that stays on, and
//! a `Session` that has to outlive whatever component started it. So it lives
//! here, in a thread-local, rather than in the reactive graph:
//! `voice_client::session::Session` leaves the room when it is dropped, and
//! anything owned by a component would hang up the moment that component
//! re-rendered.
//!
//! # Getting in
//!
//! ```text
//!   join_room ──▶ api::voice_ticket   channel access is checked HERE
//!             ◀── relay, room, participant, token
//!             ──▶ session::join(…, token)
//!                     └─ ws /_voice/node/{n}/ws?room=…&token=…
//!                        the relay verifies the ticket itself
//! ```
//!
//! The client never decides whether it may enter a room; it asks, and carries
//! the answer. That is the whole reason the ticket exists — see `voice_auth`.

use std::cell::RefCell;

use runtime_core::driver::spawn_async;
use voice_client::session::{self, Capture, Session};

use crate::api;
use crate::model::App;

/// Wait, on the one target that has voice.
///
/// `runtime_core::driver` can spawn a future but has nothing that waits, so
/// this is the browser's timer. A framework gap worth noting rather than
/// working around silently — anything that polls or retries needs it.
async fn sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

thread_local! {
    /// The one live call. Replaced on join, cleared on leave — and dropping the
    /// old value is what actually closes the previous socket, so switching
    /// rooms cannot leave two microphones publishing.
    static LIVE: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Join `channel_id`, asking the server for permission first.
///
/// Optimistic in the UI — `in_voice` is set before the socket opens, because a
/// room you have tapped should look joined immediately — but honest about the
/// difference: the tray says "Connecting" until audio is actually flowing, and
/// says what went wrong if it does not.
pub fn join_room(app: App, channel_id: &str) {
    let room = channel_id.to_string();
    app.in_voice.set(room.clone());
    app.voice_live.set(false);
    app.voice_error.set(String::new());

    spawn_async(async move {
        let ticket = match api::voice_ticket(room.clone()).await {
            Ok(t) => t,
            Err(e) => {
                // Only report against the room the user is still in: a slow
                // failure for a room they already left must not overwrite the
                // state of the one they are in now.
                if app.in_voice.get() == room {
                    app.voice_error.set(crate::net::message(e));
                    app.in_voice.set(String::new());
                }
                return;
            }
        };

        // `Processed` — browser echo cancellation and noise suppression on.
        // The loopback demo wants them off, because there the signal you are
        // listening for IS your own voice out of the speakers; a real call is
        // the case AEC exists for.
        let joined = session::join(
            &ticket.relay,
            &ticket.room,
            ticket.participant,
            Capture::Processed,
            Some(&ticket.token),
        )
        .await;

        // The user may have left, or moved to another room, while the
        // microphone was being opened. Dropping the new session here is the
        // correct outcome — not an error to report.
        if app.in_voice.get() != room {
            return;
        }

        match joined {
            Ok(session) => {
                LIVE.with(|slot| *slot.borrow_mut() = Some(session));
                app.voice_live.set(true);
                // Only now — presence claimed at the moment of ASKING to join
                // would show people in rooms they never reached.
                announce(app, room);
                refresh_presence(app);
            }
            Err(e) => {
                app.voice_error.set(e);
                app.voice_live.set(false);
                app.in_voice.set(String::new());
            }
        }
    });
}

/// Hang up. Idempotent — leaving a room you are not in is not an error.
pub fn leave_room(app: App) {
    LIVE.with(|slot| slot.borrow_mut().take());
    let was = app.in_voice.get();
    app.voice_live.set(false);
    app.voice_error.set(String::new());
    if !was.is_empty() {
        // Best-effort: a tab that dies says nothing, which is what the server's
        // freshness window is for. This only makes the ordinary case immediate.
        spawn_async(async move {
            let _ = api::voice_gone(was).await;
            refresh_presence(app);
        });
    }
}

/// How often to tell the server we are still in the room.
///
/// Comfortably inside the server's freshness window (`PRESENCE_TTL_SECS`), so
/// a single dropped request does not blink someone out of the sidebar. The gap
/// between the two is the whole tolerance budget — closing it would make every
/// slow request a disappearance.
const HEARTBEAT_MS: u32 = 15_000;

/// Keep saying "still here" until the room changes or the call ends.
///
/// A loop rather than an interval so it cannot outlive its own room: every tick
/// re-reads `in_voice` and stops the moment it is something else, which is what
/// makes switching rooms safe — the old loop retires instead of racing the new
/// one to write presence.
fn announce(app: App, room: String) {
    spawn_async(async move {
        loop {
            // Send BEFORE reading anything back. `Signal::set` stages: the
            // caller set `voice_live` true on the line that reached here, and
            // reads return the committed value until the world flushes — so a
            // loop that checked liveness first would see `false`, exit, and
            // never send a single heartbeat. The room is the argument for the
            // same reason: it does not have to be read out of a signal.
            //
            // A failure is not worth reporting. The next tick tries again, and
            // if they all fail the server drops us on its own.
            let _ = api::voice_here(room.clone()).await;
            sleep(HEARTBEAT_MS).await;

            // By now those writes are long committed, so this is a real
            // question: are we still in this room? Leaving, or moving to
            // another, retires this loop rather than racing the new one.
            if app.in_voice.get() != room {
                return;
            }
        }
    });
}

/// Live transport counters, as JSON.
fn stats_json() -> String {
    let s = session::stats();
    format!(
        r#"{{"sent":{},"recv":{},"dropped":{},"peers":{},"rate":{},"micPeak":{},"remotePeak":{}}}"#,
        s.sent,
        s.received,
        s.dropped,
        s.peers,
        s.sample_rate,
        (s.mic_peak * 100.0) as u32,
        (s.remote_peak * 100.0) as u32,
    )
}

/// Publish [`stats_json`] as `globalThis.__hearthVoiceStats()`.
///
/// A deliberate seam, not a feature. Whether a call is *connected* shows in the
/// UI; whether audio is actually **moving** does not, and "the socket opened"
/// is a far weaker claim than "the far side heard a signal". The end-to-end
/// test needs the stronger one, and the alternative — rendering peak meters as
/// text so a test can scrape them — would be shaping the product around its
/// test.
///
/// Installed explicitly rather than as a plain `#[wasm_bindgen]` export
/// because the bundle is an ES module: its exports are module-scoped and never
/// appear on a global, so an exported function is unreachable from a page
/// script or a test harness.
///
/// Reads counters the voice client already keeps; it starts nothing.
pub fn install_stats_probe() {
    use wasm_bindgen::prelude::*;

    let cb = Closure::wrap(Box::new(stats_json) as Box<dyn Fn() -> String>);
    let global = js_sys::global();
    let _ = js_sys::Reflect::set(&global, &JsValue::from_str("__hearthVoiceStats"), cb.as_ref());
    // Handed to JS for the life of the page; dropping it would leave a dead
    // function object behind that throws when called.
    cb.forget();
}

/// How often to ask who is in this space's rooms.
///
/// Polling because there is no live socket yet. Slower than the heartbeat on
/// purpose: presence is a thing you glance at, and a sidebar that redraws
/// every second costs more than it tells anyone. When the socket lands, this
/// becomes a subscription and the interval goes away.
const PRESENCE_POLL_MS: u32 = 8_000;

/// How long to wait before the first poll.
///
/// Short, because the loop starts while the app is still signing in: its first
/// pass finds no space selected and would otherwise sleep a full interval
/// before looking again — which is the whole of the "occupants appear a few
/// seconds late" delay, and it happened on every load.
const FIRST_POLL_MS: u32 = 400;

/// Keep the sidebar's occupant lists current for whichever space is open.
///
/// Started once, lives as long as the page. It follows the selection rather
/// than being restarted on every change, so switching spaces does not leave a
/// second poller running against the old one.
pub fn watch_presence(app: App) {
    spawn_async(async move {
        // What we last asked about. A poll every N seconds is fine for "has
        // anyone moved"; it is far too slow for "I just opened a different
        // space", which needs an answer now — otherwise the sidebar shows the
        // previous space's rooms as empty until the timer comes round.
        let mut asked_about = String::new();
        loop {
            let space = app.space_id.get();
            if space != asked_about {
                asked_about = space.clone();
                if !space.is_empty() {
                    refresh_presence(app);
                }
            }
            if !space.is_empty() {
                match api::voice_presence(space.clone()).await {
                    // Only apply if the selection has not moved on while the
                    // request was in flight — otherwise a slow answer for the
                    // previous space paints itself over the current one.
                    Ok(rooms) if app.space_id.get() == space => app.voice_rooms.set(rooms),
                    Ok(_) => {}
                    // Silent: presence is decoration on top of a working app,
                    // and an error banner for it would be noise. The next poll
                    // tries again.
                    Err(_) => {}
                }
            } else if !app.voice_rooms.with(|r| r.is_empty()) {
                app.voice_rooms.set(Vec::new());
            }
            sleep(if asked_about.is_empty() { FIRST_POLL_MS } else { PRESENCE_POLL_MS }).await;
        }
    });
}

/// Ask again now, without waiting for the next tick.
///
/// Called when this client changes a room, so your own avatar appears where
/// you just went instead of up to a poll later.
pub fn refresh_presence(app: App) {
    spawn_async(async move {
        let space = app.space_id.get();
        if space.is_empty() {
            return;
        }
        if let Ok(rooms) = api::voice_presence(space.clone()).await {
            if app.space_id.get() == space {
                app.voice_rooms.set(rooms);
            }
        }
    });
}
