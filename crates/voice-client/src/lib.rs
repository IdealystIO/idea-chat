//! idea-chat step 1 — a two-party voice loop over raw PCM.
//!
//! The point of this app is to prove the audio transport seams before any
//! decision is made about codecs, QUIC, or what belongs in the SDK. It does the
//! whole round trip — mic capture out, remote PCM in, speakers — using only
//! what the framework already ships, plus one small web-only shim standing in
//! for the missing `audio-output` SDK (see [`playback`]).
//!
//! # Running it
//!
//! ```text
//! cargo run -p voice-server                     # relay on :8787
//! idealyst dev --target web -p voice-client     # the app
//! ```
//!
//! Open the app in two tabs, give them **different** IDs (1 and 2), join the
//! same room, and talk. Use headphones — echo cancellation is the browser's
//! here and it is not on the native path at all.

// Which connection a call runs on, and the sync/async seam the capture
// callback publishes through. Not web-only: the WebSocket arm is every
// target's, and the shape is what keeps the audio path from knowing which.
mod carrier;
mod playback;
// Public because Hearth drives the same transport from its own UI: the loop
// this crate proves is the loop the chat app uses, rather than a second
// implementation that can drift from it.
pub mod session;
// Which transport a call should use. Web-only because `datagram` needs the
// browser's WebTransport, and native has no audio output to feed anyway.
#[cfg(target_arch = "wasm32")]
pub mod transport;
// Public for the same reason: Hearth chooses where suppression runs, and the
// model fetch behind `Mode::Client` is a cost its UI has to be able to explain.
pub mod suppress;

use idea_ui::{install_idea_theme, light_theme, Stack, StackGap, StackPadding, Typography};
use runtime_core::{rx, signal, text, ui, Element, Signal};

/// SDK-handler registration seam, invoked by the CLI-generated wrappers after
/// `runtime_vocabulary::register_builtins`. Nothing here renders through the
/// scene registry — `microphone`, `net`, and `media-stream` are plain
/// capability crates — so there is nothing to register.
pub fn register_scene_extensions<H: runtime_scene::Host>(
    _registry: &mut runtime_scene::Registry<H>,
) {
}

/// Android entry point used by the generated wrapper.
pub fn scene_app() -> Element {
    app()
}

/// Render a level as a fixed-width bar so the meter does not reflow the layout
/// on every frame.
fn meter_bar(level: f32) -> String {
    let level = level.clamp(0.0, 1.0);
    let filled = (level * 24.0).round() as usize;
    format!("[{}{}] {:>3.0}%", "#".repeat(filled), "-".repeat(24 - filled), level * 100.0)
}

pub fn app() -> Element {
    install_idea_theme(light_theme());

    let relay: Signal<String> = signal(session::default_director());
    let room: Signal<String> = signal("lobby".to_string());
    // Random per tab. A fixed default guarantees the collision it was supposed
    // to avoid: two tabs both claim it, the relay's self-echo filter suppresses
    // everything between them, and both show zero peers in silence.
    let id: Signal<String> = signal(session::random_id().to_string());

    let status: Signal<String> = signal("Idle — set an ID and join".to_string());
    let joined: Signal<bool> = signal(false);
    let mic_level: Signal<f32> = signal(0.0);
    let remote_level: Signal<f32> = signal(0.0);
    let counters: Signal<String> = signal(String::new());
    let volume: Signal<f32> = signal(1.0);
    // Default OFF: the first thing anyone does is open two tabs and listen for
    // their own voice, and AEC cancels exactly that. See `session::Capture`.
    let processing: Signal<bool> = signal(false);

    // The live session. Held here so dropping it (on leave) tears down capture,
    // the socket, and every peer's playback in one move.
    let active: std::rc::Rc<std::cell::RefCell<Option<session::Session>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));

    // Pump the audio-thread atomics into signals once per frame, writing only
    // on change so a silent room does not re-render continuously.
    {
        let last = std::cell::Cell::new((u32::MAX, u32::MAX, u32::MAX, u32::MAX));
        let last_volume = std::cell::Cell::new(f32::NAN);
        let active_for_pump = active.clone();
        let raf = runtime_core::raf_loop(move || {
            let s = session::stats();

            // Output gain rides the same pump rather than its own effect: it is
            // a once-per-frame poll with a change guard, which is exactly what
            // an effect would do here anyway.
            let v = volume.get();
            if v != last_volume.get() {
                last_volume.set(v);
                if let Some(session) = active_for_pump.borrow().as_ref() {
                    // Tapered, so this slider means the same thing Hearth's
                    // does: 100% is "as it arrived", and the bottom half of
                    // the travel is not a cliff.
                    session.set_volume(voice_dsp::taper(v));
                }
            }

            let mic_bits = s.mic_peak.to_bits();
            let remote_bits = s.remote_peak.to_bits();
            let (lm, lr, ls, lrx) = last.get();

            if mic_bits != lm {
                mic_level.set(s.mic_peak);
            }
            if remote_bits != lr {
                remote_level.set(s.remote_peak);
            }
            if s.no_capacity {
                status.set(
                    "The fleet has no voice node with capacity — this is the \
                     scale-out signal. Try again once a node comes up."
                        .to_string(),
                );
            }
            if s.connect_failed {
                status.set("Could not reach the voice node after a redirect.".to_string());
            }
            if s.duplicate_id {
                status.set(
                    "This ID is already in the room — pick a different one. \
                     (Both sides stay silent: the relay never echoes an ID to itself.)"
                        .to_string(),
                );
            }
            if s.sent != ls || s.received != lrx {
                counters.set(format!(
                    "{} · sent {} · recv {} · dropped {} · buffer {} ms{} · peers {} · {} Hz{}{}",
                    // Which carrier, because "it works" over the fallback reads
                    // exactly like "it works" over the one this project is for.
                    s.transport,
                    s.sent,
                    s.received,
                    s.dropped,
                    // The delay the jitter buffer is carrying, and what it had
                    // to invent. Both are structurally zero on a reliable
                    // transport, which is why neither was here before.
                    s.playout_ms,
                    if s.concealed > 0 {
                        format!(" ({} concealed)", s.concealed)
                    } else {
                        String::new()
                    },
                    s.peers,
                    s.sample_rate,
                    // Shown only when it happens. A permanent "unsent 0" trains
                    // the eye to skip the field that matters.
                    if s.unsent > 0 { format!(" · unsent {}", s.unsent) } else { String::new() },
                    if s.redirects > 0 {
                        format!(" · moved {}×", s.redirects)
                    } else {
                        String::new()
                    }
                ));
            }
            last.set((mic_bits, remote_bits, s.sent, s.received));
        });
        // Page-lifetime loop; the handle must outlive this scope.
        std::mem::forget(raf);
    }

    let on_join = {
        let active = active.clone();
        move || {
            if joined.get() {
                // Leave: dropping the session closes the socket and detaches
                // every sink.
                *active.borrow_mut() = None;
                joined.set(false);
                mic_level.set(0.0);
                remote_level.set(0.0);
                status.set("Left the channel".to_string());
                return;
            }

            let Ok(my_id) = id.get().trim().parse::<u32>() else {
                status.set("ID must be a number".to_string());
                return;
            };

            let (base, room_name) = (relay.get(), room.get());
            let capture = if processing.get() {
                session::Capture::Processed
            } else {
                session::Capture::Raw
            };
            joined.set(true);
            status.set("Requesting microphone…".to_string());

            let active = active.clone();
            runtime_core::driver::spawn_async(async move {
                // The loopback demo stays on the ordinary path: it exists to prove the
                // transport, and a neural pass in the middle would make "did the
                // audio arrive" and "did the model like it" the same question.
                match session::join(
                    &base,
                    &room_name,
                    my_id,
                    capture,
                    voice_dsp::Mode::Off,
                    None,
                )
                .await
                {
                    Ok(sess) => {
                        status.set(format!("Live in '{room_name}' as {my_id} — talk"));
                        *active.borrow_mut() = Some(sess);
                    }
                    Err(e) => {
                        joined.set(false);
                        status.set(e);
                    }
                }
            });
        }
    };

    ui! {
        Stack(gap = StackGap::Md, padding = StackPadding::Lg) {
            Typography(
                content = "idea-chat — voice loop".to_string(),
                kind = idea_ui::typography_kind::H1,
            )
            Typography(
                content = "Raw PCM over a binary WebSocket. No codec, no jitter buffer — \
                    just capture out and playback in, to prove the seams. Open two tabs \
                    with different IDs and use headphones."
                    .to_string(),
                muted = true,
            )

            // `value` alone is signal -> widget. The widget -> signal half is
            // `on_change`; without it the field edits visibly but the signal
            // never moves, and a join silently uses the stale value.
            text_input(
                value = relay,
                on_change = move |t| relay.set(t),
                placeholder = "director http:// URL (or a ws:// node)".to_string(),
            )
            text_input(
                value = room,
                on_change = move |t| room.set(t),
                placeholder = "room".to_string(),
            )
            text_input(
                value = id,
                on_change = move |t| id.set(t),
                placeholder = "your numeric ID".to_string(),
            )

            button(
                label = rx!(if joined.get() { "Leave".to_string() } else { "Join".to_string() }),
                on_click = on_join,
            )

            text(move || format!("output volume {:>3.0}%", volume.get() * 100.0))
            slider(value = volume, min = 0.0, max = 1.0)

            view {
                toggle(value = processing, on_change = move |on| processing.set(on))
                text(move || {
                    if processing.get() {
                        "echo cancellation ON — correct for a real call, but it \
                         cancels your own voice in a two-tab self-test"
                            .to_string()
                    } else {
                        "raw capture — no echo cancellation (use headphones)".to_string()
                    }
                })
            }

            text(move || status.get())
            text(move || format!("mic    {}", meter_bar(mic_level.get())))
            text(move || format!("remote {}", meter_bar(remote_level.get())))
            text(move || counters.get())
        }
    }
}
