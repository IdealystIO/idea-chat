//! Real-time audio capture and output, as close to the audio thread as the
//! platform allows.
//!
//! Nothing here replaces [`media_stream::AudioStream`] as the way audio moves
//! between parts of an app — that is what it is for, and a second currency
//! would fork the ecosystem for no gain. What this replaces is the *backend*:
//! where samples are produced and consumed, and on which thread.
//!
//! # The one thing this exists to fix
//!
//! Every native backend already delivers audio on an audio thread — `cpal`
//! does, `AudioRecord` does, and the callback's `Send` bound says so. The web
//! backend was the exception: `ScriptProcessorNode` runs its callback on the
//! **main thread**, sharing a deadline with layout, GC and every re-render the
//! app does. Miss it and capture does not arrive late, it is *gone*; on the
//! output side you get a gap.
//!
//! An `AudioWorklet` runs on the audio rendering thread, at a fixed 128-frame
//! quantum. A busy main thread can no longer cost you samples. That is the
//! whole point, and it is a robustness argument before it is a latency one.
//!
//! # What is and is not off the main thread today
//!
//! **I/O is.** Capture and playback both run in the worklet.
//!
//! **The graph is not, on web, yet.** A [`Pipeline`] runs where the backend
//! puts it, and the web backend currently puts it on the main thread — because
//! moving Rust into an `AudioWorkletGlobalScope` means a second wasm module
//! with its own linear memory, which is a build-tooling project rather than an
//! audio one.
//!
//! The reason [`Stage`] is a value rather than a closure is so that day needs
//! no caller to change. See [`pipeline`].

#![forbid(unsafe_code)]

pub mod latency;
pub mod pipeline;
pub mod resample;

pub use latency::{Latency, Limits};
pub use pipeline::{Block, Pipeline, Stage};

/// What went wrong opening a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// This platform has no implementation here yet, and the note says what
    /// would fix it. Not an error the caller can retry.
    Unsupported(&'static str),
    /// The person said no, or the browser never asked.
    PermissionDenied,
    /// Nothing to capture from.
    NoDevice,
    /// The platform refused, in its own words.
    Backend(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Unsupported(why) => write!(f, "{why}"),
            Error::PermissionDenied => write!(f, "microphone permission was refused"),
            Error::NoDevice => write!(f, "no audio device"),
            Error::Backend(e) => write!(f, "{e}"),
        }
    }
}

/// What the platform actually gave you.
///
/// A config is a request — every audio backend in existence is free to hand
/// back something else — so this is how a caller finds out rather than assumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames per block, per channel.
    pub block_frames: u32,
    /// Whether audio actually reaches this device on a thread of its own.
    ///
    /// The field exists so "we took it off the main thread" is *checkable*
    /// rather than asserted. True on every native backend and on an
    /// `AudioWorklet`; false on the `ScriptProcessorNode` fallback, which is
    /// what a browser too old for worklets, or a page without a secure context,
    /// will silently give you.
    pub off_main_thread: bool,
}

impl Opened {
    /// How long one block lasts, in milliseconds.
    pub fn block_ms(&self) -> f32 {
        self.block_frames as f32 * 1000.0 / self.sample_rate.max(1) as f32
    }
}

/// What to open a microphone with.
pub struct CaptureConfig {
    /// `None` asks for this crate's preferred rate rather than the device's.
    ///
    /// That is the opposite of the usual default and it is deliberate: a device
    /// that reports 44.1 kHz is a real configuration on real hardware, and
    /// Opus takes 8, 12, 16, 24 or 48 kHz and nothing else. Letting the browser
    /// resample once, in the audio graph, is cheaper and far less surprising
    /// than discovering at encode time that the codec will not start.
    pub sample_rate: Option<u32>,
    pub channels: u16,
    pub latency: Latency,
    /// Browser echo cancellation. It needs the far-end reference signal, which
    /// only the browser has, so nothing above this layer can replace it.
    pub echo_cancellation: bool,
    /// Browser noise suppression. Turn it off when something else in the
    /// pipeline is going to do it — running both is worse than running either.
    pub noise_suppression: bool,
    pub auto_gain_control: bool,
}

impl Default for CaptureConfig {
    fn default() -> CaptureConfig {
        CaptureConfig {
            sample_rate: None,
            channels: 1,
            latency: Latency::Interactive,
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain_control: true,
        }
    }
}

/// What to open a speaker with.
pub struct OutputConfig {
    pub sample_rate: Option<u32>,
    pub channels: u16,
    /// The render block — how often the audio thread asks for samples.
    pub latency: Latency,
    /// How much audio to accumulate before the first sample plays, in frames.
    ///
    /// A separate number from [`latency`](Self::latency) because it answers a
    /// separate question. The render block is how often the *consumer* runs;
    /// this is how bursty the *producer* is allowed to be. They used to be the
    /// same constant in `media-stream`, and that is exactly why 85 ms of
    /// cushion looked like a reasonable default.
    ///
    /// `None` is one render block, which assumes a producer that keeps up
    /// smoothly. Anything driven by packet arrival is not that producer: it
    /// hands over a packet's worth at an instant and then nothing until the
    /// next one, and a cushion smaller than that gap runs dry between every
    /// pair of packets.
    pub cushion_frames: Option<u32>,
}

impl Default for OutputConfig {
    fn default() -> OutputConfig {
        OutputConfig {
            sample_rate: None,
            channels: 1,
            latency: Latency::Interactive,
            cushion_frames: None,
        }
    }
}

/// The rate this crate asks for when the caller expresses no preference.
///
/// 48 kHz, because it is what every codec worth using accepts and what every
/// modern device runs internally anyway.
pub const PREFERRED_RATE: u32 = 48_000;

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::{Capture, Output};

#[cfg(not(target_arch = "wasm32"))]
mod unsupported;
#[cfg(not(target_arch = "wasm32"))]
pub use unsupported::{Capture, Output};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_length_is_reported_in_milliseconds() {
        let opened =
            Opened { sample_rate: 48_000, channels: 1, block_frames: 256, off_main_thread: true };
        assert!((opened.block_ms() - 5.333).abs() < 0.01, "{}", opened.block_ms());
    }

    #[test]
    fn capture_defaults_to_interactive_because_that_is_what_a_microphone_is_for() {
        // A speaker or a microphone opened by this crate is nearly always part
        // of something a person is reacting to. `Relaxed` is the right default
        // for a stream type; it is the wrong one for a device.
        assert_eq!(CaptureConfig::default().latency, Latency::Interactive);
        assert_eq!(OutputConfig::default().latency, Latency::Interactive);
    }
}
