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
    app.voice_live.set(false);
    app.voice_error.set(String::new());
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
