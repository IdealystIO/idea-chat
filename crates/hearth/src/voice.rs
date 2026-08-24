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
use voice_client::suppress;

use crate::api;
use crate::live::Action;
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

    /// Which suppression swap is the current one.
    ///
    /// Bumped synchronously by every request, so a swap that takes a few
    /// hundred milliseconds to build its model can tell "still wanted" from
    /// "superseded" when it finishes.
    ///
    /// A counter rather than a read of `app.suppress`, and that is the whole
    /// point: `Signal::set` STAGES. The caller sets the signal on the line
    /// before this runs, and reading it back — even after an await — can still
    /// return the previous value, so the guard threw away the swap it was
    /// meant to protect and the setting silently did nothing.
    static SWAP: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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

        // `Processed` — browser echo cancellation on. The loopback demo wants
        // it off, because there the signal you are listening for IS your own
        // voice out of the speakers; a real call is the case AEC exists for.
        //
        // The MODE is a separate axis and decides the neural pass: it also
        // turns the browser's own noise suppression off, because stacking the
        // two sounds worse than either. See `voice_dsp`.
        let mode = app.suppress.get();
        let joined = session::join(
            &ticket.relay,
            &ticket.room,
            ticket.participant,
            Capture::Processed,
            mode,
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
                crate::livewire::send(Action::JoinVoice { channel_id: room.clone() });
                announce(app, room);
            }
            Err(e) => {
                app.voice_error.set(e);
                app.voice_live.set(false);
                app.in_voice.set(String::new());
            }
        }
    });
}

/// Change where suppression runs, on the call that is already up.
///
/// Handles the modes that cost nothing to reach. `Mode::Client` is deliberately
/// **not** here: DeepFilterNet lives in a lazily-loaded wasm chunk, and a
/// function the settings page can call would anchor the whole inference engine
/// in the main bundle for every visitor, including the ones who never open a
/// voice room. Turning it on mounts `views::suppressor::Suppressor` instead,
/// and the swap happens from inside the chunk.
///
/// `mode` is passed rather than read back out of `app.suppress`, because
/// `Signal::set` stages: the caller has just set it, and a read here would
/// still return the previous value.
pub fn set_suppression(app: App, mode: voice_dsp::Mode) {
    // Nothing to swap. The next join reads the setting like it always did.
    if LIVE.with(|slot| slot.borrow().is_none()) {
        return;
    }
    match mode {
        voice_dsp::Mode::Server => {
            note("server-side is decided when the call connects — this one keeps what it has");
        }
        // The chunk's job. Mounting it is what starts the load.
        voice_dsp::Mode::Client => {}
        voice_dsp::Mode::Off => disengage(app),
    }
}

/// Put the call back on the plain microphone.
///
/// Names nothing from the denoise SDK — `direct` is a subscription that
/// forwards frames — so this whole path stays in the main bundle, which is
/// what lets turning suppression *off* work even if the chunk never loaded.
fn disengage(app: App) {
    let generation = claim_swap();
    spawn_async(async move {
        // A new capture device, because whether the BROWSER suppresses is
        // fixed when the track is opened. Going back to `Off` has to restore
        // WebRTC's own suppressor, which is not something the running track
        // can be talked into.
        let mic = match session::open_capture(Capture::Processed, voice_dsp::Mode::Off).await {
            Ok(mic) => mic,
            Err(e) => {
                app.voice_error.set(e);
                return;
            }
        };
        if !still_wanted(generation) {
            return;
        }
        LIVE.with(|slot| {
            let borrowed = slot.borrow();
            let Some(session) = borrowed.as_ref() else {
                note("the call ended while the microphone was reopening");
                return;
            };
            let chain = suppress::direct(&mic, session.wire());
            session.install(mic, chain, voice_dsp::Mode::Off);
            note("switched to off mid-call");
        });
        app.voice_error.set(String::new());
    });
}

/// Turn client-side suppression on, from inside the lazy chunk.
///
/// **Only ever called from `views::suppressor::Suppressor`'s body.** That is
/// not a style rule — it is the entire mechanism. `wasm-split` moves code that
/// is reachable *only* from a lazy component's body into that component's
/// chunk, so a second caller anywhere in the app would drag DeepFilterNet,
/// `tract` and the model plumbing back into the main bundle and the split would
/// silently stop paying for itself.
pub fn engage(app: App) {
    if LIVE.with(|slot| slot.borrow().is_none()) {
        return;
    }
    let generation = claim_swap();

    spawn_async(async move {
        // A new capture device, not the running one. Whether the browser is
        // suppressing is fixed when the microphone is opened, so reusing the
        // existing track would leave WebRTC's suppressor stacked under
        // DeepFilterNet — the one combination the design forbids, and audible
        // as a thin, over-processed voice rather than a clean one.
        let mic = match session::open_capture(Capture::Processed, voice_dsp::Mode::Client).await {
            Ok(mic) => mic,
            Err(e) => {
                app.voice_error.set(e);
                return;
            }
        };

        let prepared = match suppress::prepare(&mic, voice_dsp::Tuning::default()).await {
            Ok(prepared) => prepared,
            Err(e) => {
                // Say so, and leave the call exactly as it was. A half-applied
                // setting is worse than one that did not apply: the old chain
                // and the old microphone are both still running, so the call
                // keeps working.
                app.voice_error.set(e);
                return;
            }
        };

        if !still_wanted(generation) {
            return;
        }

        LIVE.with(|slot| {
            let borrowed = slot.borrow();
            let Some(session) = borrowed.as_ref() else {
                note("the call ended while the model was loading");
                return;
            };
            let chain = prepared.install(session.wire());
            session.install(mic, chain, voice_dsp::Mode::Client);
            note("switched to client mid-call");
        });
        app.voice_error.set(String::new());
    });
}

/// Fetch the model without starting a call, from inside the lazy chunk.
///
/// Same rule as [`engage`]: only ever called from the lazy body. Choosing
/// "This device" is the moment somebody has told us they want this, and it is
/// the last moment before a call at which 8 MB can be downloaded without
/// anybody waiting on it.
pub fn warm(app: App) {
    spawn_async(async move {
        match suppress::weights().await {
            Ok(bytes) => note(&format!("model ready ({} KB)", bytes.len() / 1024)),
            Err(e) => {
                // Not an error banner: nothing is broken yet, and the call that
                // needs it will report it properly if it is still failing then.
                let _ = &app;
                note(&format!("model could not be fetched: {e}"));
            }
        }
    });
}

/// Publish the suppression benchmark, from inside the lazy chunk.
///
/// Here rather than next to the other probe in [`install_stats_probe`] for the
/// same reason [`engage`] is: it calls the model, and a probe installed at boot
/// would anchor the inference engine in the main bundle. The cost is that the
/// benchmark only exists once client-side suppression has been selected, which
/// `tools/denoise-bench.js` does before looking for it.
///
/// Idempotent — the component remounts whenever a call starts or ends.
pub fn install_denoise_probe() {
    use wasm_bindgen::prelude::*;

    let bench = Closure::wrap(Box::new(|seconds: u32| -> js_sys::Promise {
        wasm_bindgen_futures::future_to_promise(async move {
            match suppress::bench(seconds).await {
                Ok(json) => Ok(JsValue::from_str(&json)),
                Err(e) => Err(JsValue::from_str(&e)),
            }
        })
    }) as Box<dyn Fn(u32) -> js_sys::Promise>);
    let global = js_sys::global();
    let _ = js_sys::Reflect::set(&global, &JsValue::from_str("__hearthDenoiseBench"), bench.as_ref());
    // Handed to JS for the life of the page; dropping it would leave a dead
    // function object behind that throws when called.
    bench.forget();
}

/// Claim the next swap, synchronously.
///
/// A counter rather than a read of `app.suppress`, and that is the whole point:
/// `Signal::set` STAGES. The caller sets the signal on the line before this
/// runs, and reading it back — even after an await — can still return the
/// previous value, so a guard written that way threw away the swap it was meant
/// to protect and the setting silently did nothing.
fn claim_swap() -> u64 {
    SWAP.with(|g| {
        let next = g.get() + 1;
        g.set(next);
        next
    })
}

/// Whether `generation` is still the swap the app is waiting for.
fn still_wanted(generation: u64) -> bool {
    if SWAP.with(|g| g.get()) == generation {
        return true;
    }
    note("superseded — the setting was changed again while this loaded");
    false
}

/// Say what happened to a setting that did not visibly do anything.
///
/// Every path out of `set_suppression` that is not "it worked" is a case where
/// somebody moved a control and heard no difference. None of them are errors
/// worth a banner, and all of them are worth being able to find — a silent
/// no-op is indistinguishable from a broken feature.
fn note(what: &str) {
    use wasm_bindgen::JsValue;
    web_sys::console::log_1(&JsValue::from_str(&format!("[suppress] {what}")));
}

/// Hang up. Idempotent — leaving a room you are not in is not an error.
pub fn leave_room(app: App) {
    LIVE.with(|slot| slot.borrow_mut().take());
    let was = app.in_voice.get();
    app.voice_live.set(false);
    app.voice_error.set(String::new());
    if !was.is_empty() {
        // The server takes them out of the room and tells everyone, including
        // this client — the sidebar updates because the event arrives, not
        // because we also changed it locally. One path, so there is nothing to
        // disagree.
        crate::livewire::send(Action::LeaveVoice { channel_id: was });
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
            // `StillHere` doubles as the join: the server treats presence in a
            // room it does not already have as an arrival and announces it, so
            // a reconnect mid-call re-asserts itself without a separate step.
            crate::livewire::send(Action::StillHere { channel_id: room.clone() });
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
        r#"{{"sent":{},"recv":{},"dropped":{},"peers":{},"rate":{},"micPeak":{},"remotePeak":{},"suppress":"{}"}}"#,
        s.sent,
        s.received,
        s.dropped,
        s.peers,
        s.sample_rate,
        (s.mic_peak * 100.0) as u32,
        (s.remote_peak * 100.0) as u32,
        // What the room is ACTUALLY getting, per the node — not what was
        // asked for. A test that asserted the request would pass against a
        // relay that quietly refused it.
        s.suppression.as_str(),
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
