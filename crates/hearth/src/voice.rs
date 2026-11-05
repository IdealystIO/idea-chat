//! The live voice call — the one piece of the app that is not a request.
//!
//! Everything else Hearth does is a round trip: ask the server, render the
//! answer. A call is a connection that stays open, a microphone that stays on,
//! and a `Call` that has to outlive whatever component started it. So it lives
//! here, in a thread-local, rather than in the reactive graph: dropping the
//! `Call` hangs up, and anything owned by a component would hang up the moment
//! that component re-rendered.
//!
//! # Getting in
//!
//! ```text
//!   join_room ──▶ api::voice_ticket   channel access is checked HERE
//!             ◀── relay, cert_hash, room, grant, user
//!             ──▶ call_session::join(relay, …, grant)
//!                     └─ WebTransport https://relay/?room=…&grant=…
//!                        the relay checks the grant itself (call-auth)
//! ```
//!
//! The client never decides whether it may enter a room; it asks, and carries
//! the answer. The grant lasts two minutes, and the session renews it at
//! half-life by asking for another ticket — which is the server re-checking,
//! every minute, that we may still see the channel. A moderator removing us
//! revokes it outright (`api::voice_remove`), and the relay closes us at once.
//!
//! # Who is who
//!
//! The relay numbers connections and names each by its grant's user — our
//! account ids — in the session's roster. Per-person volume and mute are kept
//! by account id (`crate::volume`) and applied to whatever number that person
//! has in this call, whenever someone arrives.

use std::cell::RefCell;
use std::rc::Rc;

use call_proto::relay::FromRelay;
use call_session::{Access, Call, Renew, Settings};
use runtime_core::driver::spawn_async;

use crate::api;
use crate::live::Action;
use crate::model::App;

/// Where the DeepFilterNet model is served from — `/assets`, which the
/// server maps to `crates/hearth/assets/`.
const MODEL_PATH: &str = "/assets/DeepFilterNet3_onnx.tar.gz";

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
    /// old value is what actually closes the previous connection, so switching
    /// rooms cannot leave two microphones publishing.
    static LIVE: Rc<RefCell<Option<Call>>> = Rc::default();

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

fn live() -> Rc<RefCell<Option<Call>>> {
    LIVE.with(Rc::clone)
}

/// Run `f` against the live call, if there is one.
pub fn with_call<R>(f: impl FnOnce(&Call) -> R) -> Option<R> {
    live().borrow().as_ref().map(f)
}

/// What the call's audio does, from this device's settings. Client-side
/// suppression starts OFF and is switched on by the lazy chunk once it has
/// loaded (see [`engage`]) — the call does not wait for 15 MB of model engine.
fn settings(app: App) -> Settings {
    let tuning = app.tuning();
    Settings {
        processing: processing(voice_dsp::Mode::Off),
        voice: call_audio::VoiceStages {
            dehum: true,
            declick: tuning.transients.is_some(),
            level: tuning.leveling.is_some(),
            expand: tuning.residual.is_some(),
        },
        model: model_settings(&tuning),
        model_path: MODEL_PATH,
        gains: gains(app, app.deafened.get()).0,
        default_gain: gains(app, app.deafened.get()).1,
    }
}

/// Our gain on each person this device has an opinion about, and on everyone
/// else: master × their slider, or nothing (muted for us, or deafened).
fn gains(app: App, deafened: bool) -> (std::collections::HashMap<String, f32>, f32) {
    let master = if deafened { 0.0 } else { voice_dsp::taper(crate::volume::master()) };
    let muted = app.muted.get();
    let mut by_user: std::collections::HashMap<String, f32> = crate::volume::everyone()
        .into_iter()
        .map(|(user, position)| {
            let gain = master * crate::volume::person_gain(position, muted.contains(&user));
            (user, gain)
        })
        .collect();
    for user in &muted {
        by_user.insert(user.clone(), 0.0);
    }
    (by_user, master * crate::volume::person_gain(crate::volume::PERSON_DEFAULT, false))
}

fn processing(mode: voice_dsp::Mode) -> call_audio::Processing {
    call_audio::Processing {
        // Browser echo cancellation on: a real call is the case it exists for.
        echo_cancellation: true,
        // `Off` leaves the browser's own suppressor on — the ordinary
        // video-call baseline. `Client` turns it off, because stacking it under
        // DeepFilterNet sounds worse than either.
        suppression: match mode {
            voice_dsp::Mode::Client => call_audio::Suppression::Neural,
            _ => call_audio::Suppression::Browser,
        },
        auto_gain_control: false,
    }
}

fn model_settings(tuning: &voice_dsp::Tuning) -> call_audio::ModelSettings {
    call_audio::ModelSettings {
        max_reduction_db: tuning.atten_lim_db.unwrap_or(call_audio::UNLIMITED_DB),
        post_filter: tuning.post_filter,
        ..Default::default()
    }
}

/// Join `channel_id`, asking the server for permission first.
///
/// Optimistic in the UI — `in_voice` is set before the connection opens,
/// because a room you have tapped should look joined immediately — but honest
/// about the difference: the tray says "Connecting" until audio is actually
/// flowing, and says what went wrong if it does not.
pub fn join_room(app: App, channel_id: &str) {
    let room = channel_id.to_string();
    app.in_voice.set(room.clone());
    app.voice_live.set(false);
    app.voice_error.set(String::new());
    // A previous call goes now, not when the new one connects.
    live().borrow_mut().take();

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

        // Renewal is another ticket: the server checks again that we may see
        // the channel, and the relay swaps the grant in without a reconnect.
        let renew_room = room.clone();
        let fetch: call_session::Fetch = Rc::new(move || {
            let room = renew_room.clone();
            Box::pin(async move { api::voice_ticket(room).await.map(|t| t.token).map_err(crate::net::message) })
        });
        // No end-to-end key yet: the relay runs inside this server, so a key
        // it handed out would protect nothing. That waits on clients agreeing
        // keys among themselves.
        let access = Access {
            room: ticket.room.clone(),
            grant: Some(ticket.token.clone()),
            renew: Some(Renew::With(fetch)),
            key: None,
        };
        let control_room = room.clone();
        let joined = call_session::join(&ticket.relay, &ticket.cert_hash, access, settings(app), move |text| {
            on_control(app, &control_room, &text)
        })
        .await;

        // The user may have left, or moved to another room, while the
        // microphone was being opened. Dropping the new call here is the
        // correct outcome — not an error to report.
        if app.in_voice.get() != room {
            return;
        }

        match joined {
            Ok(call) => {
                *live().borrow_mut() = Some(call);
                app.voice_live.set(true);
                // Only now — presence claimed at the moment of ASKING to join
                // would show people in rooms they never reached.
                crate::livewire::send(Action::JoinVoice { channel_id: room.clone() });
                // After the call is in the slot: these reach into it. A call
                // starts unmuted, and somebody who muted before it connected
                // must not be live for the first second of it.
                apply_mic(app);
                apply_volumes(app);
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

/// What the relay says, as far as the app cares.
fn on_control(app: App, room: &str, text: &str) {
    match FromRelay::parse(text) {
        FromRelay::Revoked => end(app, room, "A moderator removed you from this call."),
        FromRelay::Expires { secs: 0 } => {
            app.voice_error.set("Your access to this room ended — you cannot be heard.".to_string());
        }
        FromRelay::Refused(why) => app.voice_error.set(format!("Could not stay in the call: {why}")),
        FromRelay::Other(t) if t == "link closed" => end(app, room, "Lost the connection to voice."),
        _ => {}
    }
}

/// The call ended without us asking: say why, and stand down as if we left.
fn end(app: App, room: &str, why: &str) {
    // Only for the call we are in; and a revocation's message outranks the
    // close that follows it.
    if app.in_voice.get() != room || live().borrow().is_none() {
        return;
    }
    if app.voice_error.get().is_empty() {
        app.voice_error.set(why.to_string());
    }
    let error = app.voice_error.get();
    let error = if error.is_empty() { why.to_string() } else { error };
    leave_room(app);
    // Kept after the call is gone: the tray shows it until it is tapped away
    // (see `views::rooms`), so an ending nobody asked for says why.
    app.voice_error.set(error);
}

// ---------------------------------------------------------------------------
// The microphone
// ---------------------------------------------------------------------------

/// Mute or unmute directly, for a caller that already knows the answer.
pub fn set_muted(muted: bool) {
    call_session::set_muted(&live(), muted);
}

/// Push the mic switch onto the live call. `Signal::set` stages, so this
/// takes the value from the signal as committed — call it after the switch.
pub fn apply_mic(app: App) {
    call_session::set_muted(&live(), !app.mic.get());
}

// ---------------------------------------------------------------------------
// Listener volume
// ---------------------------------------------------------------------------

/// Push the whole volume picture onto the live call — after a join, and
/// whenever it changes wholesale (deafen). Gains are kept by account id in
/// the session, which applies them to whatever relay numbers a person has,
/// including ones they open later.
pub fn apply_volumes(app: App) {
    apply_volumes_deafened(app, app.deafened.get());
}

/// Deafen or undeafen: everyone's audio to nothing, or back. `deafened` is
/// passed because the caller has just set the signal, and it stages.
pub fn set_deafened(app: App, deafened: bool) {
    apply_volumes_deafened(app, deafened);
}

fn apply_volumes_deafened(app: App, deafened: bool) {
    let (by_user, default) = gains(app, deafened);
    with_call(|call| {
        call.set_default_volume(default);
        for (user, gain) in &by_user {
            call.set_user_volume(user, *gain);
        }
    });
}

/// Push one person's gain onto the live call, if there is one.
///
/// Takes both halves rather than reading them: `Signal::set` stages, so a
/// caller that has just toggled mute and then asked us to read it back would
/// apply the value it was replacing.
pub fn refresh_person(user_id: &str, position: f32, muted: bool) {
    let master = voice_dsp::taper(crate::volume::master());
    with_call(|call| call.set_user_volume(user_id, master * crate::volume::person_gain(position, muted)));
}

/// Push the master onto the live call, if there is one. `position` is the
/// slider's, passed because the signal stages.
pub fn refresh_master(app: App, position: f32) {
    let master = voice_dsp::taper(position);
    let muted = app.muted.get();
    with_call(|call| {
        call.set_default_volume(master * crate::volume::person_gain(crate::volume::PERSON_DEFAULT, false));
        for (user, slider) in crate::volume::everyone() {
            call.set_user_volume(&user, master * crate::volume::person_gain(slider, muted.contains(&user)));
        }
        for user in &muted {
            call.set_user_volume(user, 0.0);
        }
    });
}

// ---------------------------------------------------------------------------
// Noise suppression
// ---------------------------------------------------------------------------

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
    if live().borrow().is_none() {
        return; // The next join reads the setting.
    }
    match mode {
        voice_dsp::Mode::Server => note("suppression on the server is not available on this relay"),
        // The chunk's job. Mounting it is what starts the load.
        voice_dsp::Mode::Client => {}
        voice_dsp::Mode::Off => swap(app, voice_dsp::Mode::Off),
    }
}

/// Reopen the call's audio with `mode`'s processing, keeping the link. A
/// mic reopen when the browser's own suppressor flips (it is fixed when the
/// track is opened); a stage swap with no gap otherwise.
fn swap(app: App, mode: voice_dsp::Mode) {
    let generation = claim_swap();
    spawn_async(async move {
        let result = call_session::reprocess(&live(), processing(mode)).await;
        if !still_wanted(generation) {
            return;
        }
        match result {
            Ok(()) => {
                note(&format!("switched to {mode:?} mid-call"));
                app.voice_error.set(String::new());
            }
            // Say so; the call carries on with what it had.
            Err(e) => app.voice_error.set(e),
        }
    });
}

/// Turn client-side suppression on, from inside the lazy chunk.
///
/// **Only ever called from `views::suppressor::Suppressor`'s body.** That is
/// not a style rule — it is the entire mechanism. `wasm-split` moves code that
/// is reachable *only* from a lazy component's body into that component's
/// chunk; this installs the DeepFilterNet builder with `call_audio`, and a
/// second caller anywhere in the app would drag `denoise`, `tract` and the
/// model plumbing back into the main bundle.
pub fn engage(app: App) {
    install_neural();
    if live().borrow().is_none() {
        return;
    }
    swap(app, voice_dsp::Mode::Client);
}

/// Hand `call_audio` a DeepFilterNet builder. From the lazy chunk only; see
/// [`engage`]. Idempotent.
fn install_neural() {
    use voice_client::suppress;
    let build: call_audio::BuildNeural = Rc::new(|mic, model: call_audio::ModelSettings| {
        Box::pin(async move {
            let mut denoiser = denoise::Denoiser::with_weights(suppress::weights().await?)
                .post_filter(model.post_filter)
                .thresholds(model.min_db_thresh, model.max_db_erb_thresh, model.max_db_df_thresh);
            if model.max_reduction_db < call_audio::UNLIMITED_DB {
                denoiser = denoiser.attenuation_limit_db(model.max_reduction_db);
            }
            let clean =
                denoiser.process(&mic).await.map_err(|e| format!("DeepFilterNet could not start: {e}"))?;
            // Three 10 ms hops at 48 kHz, measured natively.
            Ok(call_audio::NeuralStage { clean, latency: 1440 })
        })
    });
    let prepare: call_audio::PrepareNeural = Rc::new(|| Box::pin(async { suppress::weights().await.map(|_| ()) }));
    call_audio::install_neural(build, Some(prepare));
}

/// Fetch the model without starting a call, from inside the lazy chunk.
///
/// Same rule as [`engage`]: only ever called from the lazy body. Choosing
/// "This device" is the moment somebody has told us they want this, and it is
/// the last moment before a call at which 8 MB can be downloaded without
/// anybody waiting on it.
pub fn warm(app: App) {
    install_neural();
    spawn_async(async move {
        match voice_client::suppress::weights().await {
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
/// would anchor the inference engine in the main bundle. Idempotent.
pub fn install_denoise_probe() {
    use wasm_bindgen::prelude::*;

    let bench = Closure::wrap(Box::new(|seconds: u32| -> js_sys::Promise {
        wasm_bindgen_futures::future_to_promise(async move {
            match voice_client::suppress::bench(seconds).await {
                Ok(json) => Ok(JsValue::from_str(&json)),
                Err(e) => Err(JsValue::from_str(&e)),
            }
        })
    }) as Box<dyn Fn(u32) -> js_sys::Promise>);
    let global = js_sys::global();
    let _ = js_sys::Reflect::set(&global, &JsValue::from_str("__hearthDenoiseBench"), bench.as_ref());
    bench.forget();
}

/// Claim the next swap, synchronously. See `SWAP`.
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

// ---------------------------------------------------------------------------
// Speaking
// ---------------------------------------------------------------------------

/// How often to read voice activity out of the audio path. The audio
/// callbacks decide it a hundred times a second; the signal is only written
/// when the answer changes.
const ACTIVITY_MS: u32 = 50;

/// How long "not talking" must hold before that is broadcast. Speech is full
/// of gaps; going quiet waits, starting does not.
const QUIET_MS: u32 = 250;

/// Keep `app.speaking` current, and tell the room about it.
///
/// Started once at boot rather than per call. Your own highlight comes from
/// the local signal and is instant; everyone else's from the broadcast.
pub fn watch_activity(app: App) {
    spawn_async(async move {
        let mut quiet_for = 0u32;
        let mut told_the_room = false;
        loop {
            sleep(ACTIVITY_MS).await;

            // Only while a call is up. Otherwise the last value from the
            // previous call would sit there lit.
            let room = app.in_voice.get();
            let live_now = app.voice_live.get() && !room.is_empty();
            let now = live_now && with_call(|c| c.stats.borrow().speaking).unwrap_or(false);

            if now != app.speaking.get() {
                app.speaking.set(now);
            }

            quiet_for = if now { 0 } else { quiet_for.saturating_add(ACTIVITY_MS) };

            // Rising edge immediately; falling edge only once it has held.
            let should_tell = if now { true } else { quiet_for < QUIET_MS };
            if live_now && should_tell != told_the_room {
                told_the_room = should_tell;
                crate::livewire::send(Action::Speaking { channel_id: room, speaking: should_tell });
            } else if !live_now && told_the_room {
                told_the_room = false;
            }
        }
    });
}

/// Say what happened to a setting that did not visibly do anything.
fn note(what: &str) {
    use wasm_bindgen::JsValue;
    web_sys::console::log_1(&JsValue::from_str(&format!("[voice] {what}")));
}

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

/// Keep the call's video in step with the buttons, and the stage in step with
/// the call — once a display frame, from boot.
///
/// - `app.cam` / `app.sharing` are what the buttons ask for; the call is
///   changed to match, one change at a time (so a camera and a share asked
///   for together do not stack two permission prompts), and they are put back
///   when the browser refuses or the share is stopped from its own control.
/// - A screen share's audio is sent as its own track whenever the share has
///   some.
/// - `app.video` (who is sending video, and our own camera and screen) and
///   `app.mic_off` are written only when they change, because the stage
///   rebuilds on them and a rebuild restarts every video on it.
pub fn watch_media(app: App) {
    let pending = Rc::new(std::cell::Cell::new(false));
    let raf = runtime_core::raf_loop(move || {
        let slot = live();
        let Some((video, roster, resync)) =
            slot.borrow().as_ref().map(|c| (c.video.clone(), c.roster.clone(), c.screen_audio_out_of_sync()))
        else {
            if app.video.get() != crate::model::VideoState::default() {
                app.video.set(crate::model::VideoState::default());
            }
            if !app.mic_off.get().is_empty() {
                app.mic_off.set(Vec::new());
            }
            return;
        };
        if resync {
            spawn_async(async move { call_session::sync_screen_audio(&live()).await });
        }
        if video.camera_failed() {
            app.cam.set(false);
            app.sharing.set(false);
            app.voice_error.set("The camera stopped — see the console".to_string());
        }
        if video.screen_ended() && app.sharing.get() {
            app.sharing.set(false);
        }
        if !pending.get() {
            let want_camera = app.cam.get() && !video.camera_failed();
            let want_screen = app.sharing.get() && !video.screen_ended();
            let change = if want_camera != video.camera_on() {
                Some((true, want_camera))
            } else if want_screen != video.screen_on() {
                Some((false, want_screen))
            } else {
                None
            };
            if let Some((is_camera, on)) = change {
                pending.set(true);
                let (pending, video) = (pending.clone(), video.clone());
                spawn_async(async move {
                    let done = if is_camera { video.set_camera(on).await } else { video.set_screen(on).await };
                    if let Err(e) = done {
                        if is_camera { app.cam.set(false) } else { app.sharing.set(false) }
                        app.voice_error.set(e);
                    }
                    pending.set(false);
                });
            }
        }

        let r = roster.borrow();
        let mut remote: Vec<(String, u8)> = video
            .remotes()
            .into_iter()
            .filter_map(|(id, track)| r.names.get(&id).map(|user| (user.clone(), track)))
            .collect();
        remote.sort();
        let next = crate::model::VideoState { remote, camera: video.camera_on(), screen: video.screen_on() };
        if next != app.video.get() {
            app.video.set(next);
        }
        let mut off: Vec<String> = r.mic_off.iter().filter_map(|id| r.names.get(id).cloned()).collect();
        off.sort();
        if off != app.mic_off.get() {
            app.mic_off.set(off);
        }
    });
    std::mem::forget(raf);
}

/// Someone's video, to show: a camera (`call_proto::TRACK_MAIN`) or a screen
/// share (`TRACK_SCREEN`), by account id — ours too.
pub fn player(app: App, user: &str, track: u8) -> Option<runtime_core::Element> {
    let screen = track == call_proto::TRACK_SCREEN;
    with_call(|c| {
        let stream = if user == app.my_id() {
            c.video.local(track)
        } else {
            let id = c.roster.borrow().id_of(user)?;
            c.video.remote(id, track)
        };
        stream.map(|s| call_video::player(s, screen))
    })
    .flatten()
}

// ---------------------------------------------------------------------------
// Leaving, and staying
// ---------------------------------------------------------------------------

/// Hang up. Idempotent — leaving a room you are not in is not an error.
pub fn leave_room(app: App) {
    live().borrow_mut().take();
    let was = app.in_voice.get();
    app.voice_live.set(false);
    app.voice_error.set(String::new());
    app.in_voice.set(String::new());
    if !was.is_empty() {
        // The server takes them out of the room and tells everyone, including
        // this client — the sidebar updates because the event arrives.
        crate::livewire::send(Action::LeaveVoice { channel_id: was });
    }
}

/// How often to tell the server we are still in the room — comfortably inside
/// its freshness window (`PRESENCE_TTL_SECS`).
const HEARTBEAT_MS: u32 = 15_000;

/// Keep saying "still here" until the room changes or the call ends.
fn announce(app: App, room: String) {
    spawn_async(async move {
        loop {
            // Send BEFORE reading anything back: `Signal::set` stages, and the
            // caller set `voice_live` on the line that reached here.
            crate::livewire::send(Action::StillHere { channel_id: room.clone() });
            sleep(HEARTBEAT_MS).await;
            if app.in_voice.get() != room {
                return;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// The test seam
// ---------------------------------------------------------------------------

/// Live call counters, as JSON — what the end-to-end suites read.
fn stats_json() -> String {
    let Some((s, t, peers_peak, muted, processing)) = with_call(|c| {
        let mut st = c.stats.borrow_mut();
        let s = *st;
        // Peaks are "since last read".
        st.mic_peak = 0.0;
        st.out_peak = 0.0;
        // A window nobody has heard into for a while is silence, not its last
        // loud moment.
        let fresh = call_audio::now_ms() - s.remote_peak_ms < 2.0 * call_audio::PEAK_WINDOW_MS;
        (s, *c.traffic.borrow(), if fresh { s.remote_peak } else { 0.0 }, c.muted(), c.processing())
    }) else {
        // No call: zeros, so "torn down" reads as nobody, not as an error.
        return r#"{"sent":0,"bytesSent":0,"unsent":0,"codec":"none","transport":"none","recv":0,"dropped":0,"concealed":0,"playoutMs":0,"offMainThread":false,"underruns":0,"resyncs":0,"muted":false,"peers":0,"rate":0,"micPeak":0,"remotePeak":0,"suppress":"off","speaking":false,"modelGainDb":0.0,"rttMs":-1,"videoUp":0,"videoDown":0}"#.to_string();
    };
    let suppress = match processing.suppression {
        call_audio::Suppression::Neural => "client",
        _ => "off",
    };
    format!(
        r#"{{"sent":{},"bytesSent":{},"unsent":{},"codec":"opus","transport":"webtransport","recv":{},"dropped":{},"concealed":{},"playoutMs":{},"offMainThread":{},"underruns":{},"resyncs":0,"muted":{},"peers":{},"rate":{},"micPeak":{},"remotePeak":{},"suppress":"{}","speaking":{},"modelGainDb":{:.1},"rttMs":{},"videoUp":{},"videoDown":{}}}"#,
        s.sent,
        t.audio_up,
        // Packets the transport would not carry: on datagrams a stalled send
        // path is silent in exactly the way a quiet room is.
        s.unsent,
        s.received,
        s.late,
        s.concealed,
        s.playout_ms,
        s.off_main_thread,
        s.underruns,
        muted,
        s.peers,
        s.rate,
        (s.mic_peak * 100.0) as u32,
        (peers_peak * 100.0) as u32,
        suppress,
        s.speaking,
        s.model_gain_db.unwrap_or(0.0),
        t.rtt_ms.map(|r| r.round() as i64).unwrap_or(-1),
        t.video_up,
        t.video_down,
    )
}

/// Publish [`stats_json`] as `globalThis.__hearthVoiceStats()`.
///
/// A deliberate seam, not a feature: whether audio is actually **moving** does
/// not show in the UI, and the end-to-end test needs that stronger claim.
/// Installed on the global because the bundle is an ES module whose exports
/// never reach a page script.
pub fn install_stats_probe() {
    use wasm_bindgen::prelude::*;

    let cb = Closure::wrap(Box::new(stats_json) as Box<dyn Fn() -> String>);
    let global = js_sys::global();
    let _ = js_sys::Reflect::set(&global, &JsValue::from_str("__hearthVoiceStats"), cb.as_ref());
    cb.forget();
}
