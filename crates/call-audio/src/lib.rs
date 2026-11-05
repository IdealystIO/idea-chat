//! A call's voice, both ways: the microphone to Opus datagrams
//! ([`Sender`]), and datagrams to the speakers ([`Receiver`]).
//!
//! It knows nothing of the link. The sender hands its datagrams to a channel
//! and the receiver is handed what arrives, in `call_proto::audio`'s format,
//! so whoever owns the connection decides how they travel.
//!
//! **Voice processing** is the `voice_dsp` layer around the suppression
//! stage, one switch per stage ([`VoiceStages`]): mains hum and rumble out
//! before anything measures the signal, then declick, level (automatic gain
//! with a peak ceiling, so nobody arrives too loud or too quiet) and expand
//! (the residual noise pushed down between words — only under the neural
//! stage, whose own verdict says which blocks were noise).
//!
//! **Capture times** ride on every packet: when the frame's first sample was
//! captured, on the page's `performance.now()`, after accounting for every
//! stage's delay — the moment the sound happened, not the moment it was sent.
//! The receiver turns them back into an `av_sync::AudioClock` for whoever
//! holds the video to.
//!
//! Audio track `TRACK_MAIN` is the microphone; `TRACK_SCREEN` is a screen
//! share's own audio, sent as it is.
//!
//! The pipeline is web-only for now (native has no audio output yet; see
//! `audio-io`); the settings build everywhere.

mod settings;
pub use settings::*;

#[cfg(target_arch = "wasm32")]
mod neural;
#[cfg(target_arch = "wasm32")]
mod receive;
#[cfg(target_arch = "wasm32")]
mod send;
#[cfg(target_arch = "wasm32")]
pub use receive::{Levels, Receiver, Volumes, PEAK_WINDOW_MS};
#[cfg(target_arch = "wasm32")]
pub use neural::{install_neural, neural_available, prepare_neural, BuildNeural, NeuralStage, PrepareNeural};
#[cfg(target_arch = "wasm32")]
pub use send::{Rate, SendSettings, Sender, VOICE_RATE};
/// For an app building its own neural stage: the stream type it reads and
/// writes.
#[cfg(target_arch = "wasm32")]
pub use media_stream::AudioStream;
/// A volume slider's position (1.0 = as sent) to the gain [`Volumes::set`]
/// takes — the curve Hearth's sliders use too.
#[cfg(target_arch = "wasm32")]
pub use voice_dsp::taper;

#[cfg(target_arch = "wasm32")]
const FRAME: audio_codec::FrameMs = audio_codec::FrameMs::Twenty;

/// The page's `performance.now()`, in ms — the clock every capture time is on.
#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> f64 {
    thread_local! {
        static CLOCK: Option<web_sys::Performance> = web_sys::window().and_then(|w| w.performance());
    }
    CLOCK.with(|c| c.as_ref().map(|p| p.now()).unwrap_or(0.0))
}

#[cfg(target_arch = "wasm32")]
fn log(message: &str) {
    web_sys::console::log_1(&message.into());
}
