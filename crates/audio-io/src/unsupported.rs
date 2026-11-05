//! The arm for platforms this crate has not reached yet.
//!
//! Not a stub that silently does nothing: every entry point returns
//! [`Error::Unsupported`] naming what would fix it, so a native build compiles
//! and its tests run while the absence stays loud. The same discipline the
//! app's own playback shim uses for the `audio-output` gap.
//!
//! Native capture is the *easy* one to add — `cpal` already delivers on an
//! audio thread, which is the property this whole crate exists to give the web
//! backend. Native output is the one that does not exist anywhere in the tree.

use crate::{CaptureConfig, Error, Opened, OutputConfig, Pipeline};

const CAPTURE: &str = "audio-io has no capture backend on this platform yet — \
                       cpal would provide one, and already delivers on an audio thread";
const OUTPUT: &str = "audio-io has no output backend on this platform yet — this is the \
                      `audio-output` gap (AVAudioEngine / AudioTrack / cpal)";

pub struct Capture;

impl Capture {
    pub async fn open(
        _config: CaptureConfig,
        _pipeline: Pipeline,
        _on_block: impl FnMut(&[f32], u32, u16, u64) + 'static,
    ) -> Result<Capture, Error> {
        Err(Error::Unsupported(CAPTURE))
    }

    pub fn opened(&self) -> Opened {
        unreachable!("a Capture is never constructed on this platform")
    }

    pub fn input_latency_ms(&self) -> f64 {
        0.0
    }
}

pub struct Output;

impl Output {
    pub async fn open(_config: OutputConfig) -> Result<Output, Error> {
        Err(Error::Unsupported(OUTPUT))
    }

    pub fn opened(&self) -> Opened {
        unreachable!("an Output is never constructed on this platform")
    }

    pub fn write(&self, _pcm: &[f32]) {}

    pub fn queued_frames(&self) -> u32 {
        0
    }

    pub fn underruns(&self) -> u32 {
        0
    }

    pub fn device_latency_ms(&self) -> f64 {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_says_what_is_missing_rather_than_failing_vaguely() {
        let err = pollster_lite(Capture::open(
            CaptureConfig::default(),
            Pipeline::new(),
            |_, _, _, _| {},
        ));
        match err {
            Err(Error::Unsupported(why)) => {
                assert!(why.contains("cpal"), "and names the fix: {why}");
            }
            _ => panic!("expected Unsupported"),
        }
    }

    /// The smallest possible `block_on`. These futures never await anything, so
    /// there is nothing to poll twice and no reason to take a runtime — and
    /// `Waker::noop` means no reason to reach for `unsafe` either, which this
    /// crate forbids.
    fn pollster_lite<T>(future: impl core::future::Future<Output = T>) -> T {
        use core::task::{Context, Poll, Waker};
        let mut cx = Context::from_waker(Waker::noop());
        match Box::pin(future).as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("this future never awaits"),
        }
    }
}
