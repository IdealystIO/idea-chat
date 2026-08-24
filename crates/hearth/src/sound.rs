//! Placeholder tones.
//!
//! Synthesised rather than shipped as files, so there is nothing to license,
//! nothing to load, and no doubt about what they are: three shapes that say
//! "something arrived", "someone came in", "someone went out". They are meant
//! to be replaced by something designed.
//!
//! # When they play
//!
//! Sound is the most intrusive thing an app can do, so the rules are narrow:
//!
//! - **Message** — a message from somebody else, in the channel being read.
//!   Not your own, because you know you sent it.
//! - **Join / leave** — only about the voice room *you are in*. Someone
//!   arriving in a room you are not in is not your business and does not get
//!   to interrupt you. Your own arrival and departure do play, as confirmation
//!   that the thing you clicked worked.

use wasm_bindgen::JsValue;
use web_sys::{AudioContext, OscillatorType};

/// The shape of a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Somebody said something.
    Message,
    /// Somebody came into the room you are in.
    Join,
    /// Somebody left it.
    Leave,
}

impl Tone {
    /// `(start Hz, end Hz, seconds, peak gain)`.
    ///
    /// Rising for arrival, falling for departure — the association is old
    /// enough to be worth borrowing, and it means the two are distinguishable
    /// without being learned. The message tone is a single short note, quieter
    /// than both, because it is the one that happens most.
    fn shape(self) -> (f32, f32, f64, f32) {
        match self {
            Tone::Message => (880.0, 880.0, 0.09, 0.05),
            Tone::Join => (523.25, 783.99, 0.13, 0.08),
            Tone::Leave => (783.99, 523.25, 0.13, 0.08),
        }
    }
}

/// Play one.
///
/// Best effort and silent about failure: a browser that will not make noise
/// until the page has been clicked is the normal case, not an error worth
/// telling anyone about.
pub fn play(tone: Tone) {
    let _ = try_play(tone);
}

fn try_play(tone: Tone) -> Result<(), JsValue> {
    let (from, to, secs, peak) = tone.shape();

    // A context per tone. Wasteful in principle; correct in practice — a
    // long-lived context created before the first user gesture is left
    // suspended by the browser and never recovers on its own, which is a
    // silent app rather than a wasted allocation.
    let ctx = AudioContext::new()?;
    let now = ctx.current_time();

    let osc = ctx.create_oscillator()?;
    osc.set_type(OscillatorType::Sine);
    osc.frequency().set_value(from);
    if (to - from).abs() > f32::EPSILON {
        osc.frequency().linear_ramp_to_value_at_time(to, now + secs)?;
    }

    // An envelope, not a switch: a square-edged tone clicks, and the click is
    // more noticeable than the note.
    let gain = ctx.create_gain()?;
    gain.gain().set_value(0.0);
    gain.gain().linear_ramp_to_value_at_time(peak, now + 0.012)?;
    gain.gain().linear_ramp_to_value_at_time(0.0, now + secs)?;

    osc.connect_with_audio_node(&gain)?;
    gain.connect_with_audio_node(&ctx.destination())?;
    osc.start()?;
    osc.stop_with_when(now + secs)?;

    // Close once it has finished, or the page accumulates one live audio
    // context per notification for as long as it is open.
    let closer = wasm_bindgen::closure::Closure::once_into_js(move || {
        let _ = ctx.close();
    });
    web_sys::window()
        .ok_or_else(|| JsValue::from_str("no window"))?
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            closer.as_ref().unchecked_ref(),
            ((secs + 0.2) * 1000.0) as i32,
        )?;
    Ok(())
}

use wasm_bindgen::JsCast;
