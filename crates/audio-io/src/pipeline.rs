//! Processing as a graph of movable stages, rather than a callback.
//!
//! # Why this is not a closure
//!
//! The obvious API for "do something to each block" is `FnMut(&mut [f32])`. It
//! is also the one thing that would permanently nail processing to whichever
//! thread built it: a closure captures its environment, and an environment
//! cannot be relocated to an audio thread, a worklet, or another process.
//!
//! A [`Stage`] is a value. It owns its own state, it is `Send`, and it is
//! handed to the backend rather than called by the caller. That means the
//! backend decides where the graph runs, and can decide differently later
//! without a single caller changing — which is the whole point, because today
//! the web backend runs the graph on the main thread and one day it will not.
//!
//! # What crosses a thread boundary
//!
//! Audio does not. Numbers do.
//!
//! A stage that wants to report a level, or be told about a gain change, does
//! it through an atomic it was built with — the same pattern the app already
//! uses to get meters from the capture callback into the UI. That seam is a few
//! nanoseconds and it works from any thread, which is exactly what makes the
//! graph relocatable. A stage that reaches for a `Rc`, a signal, or a channel
//! back to the UI has quietly re-attached itself to one thread.

use core::fmt;

/// One block of interleaved PCM, on its way through a pipeline.
///
/// Mutable in place: a stage that needed to allocate a new buffer per block
/// would be allocating on an audio thread, which is the one place you cannot.
pub struct Block<'a> {
    /// Interleaved samples. `samples.len() == frames * channels`.
    pub samples: &'a mut [f32],
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames since this stream started.
    ///
    /// A sample-accurate monotonic clock, which is what a packetiser and a
    /// jitter buffer both actually want. Deliberately not a wall-clock
    /// timestamp: wall clocks step, get rebased, and disagree between threads.
    pub frames_elapsed: u64,
}

impl Block<'_> {
    /// Frames in this block, per channel.
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    /// When this block starts, in microseconds since the stream did.
    ///
    /// Derived from the frame count rather than read from a clock, so it stays
    /// exact and monotonic however late the block was delivered.
    pub fn pts_micros(&self) -> u64 {
        self.frames_elapsed.saturating_mul(1_000_000) / self.sample_rate.max(1) as u64
    }
}

impl fmt::Debug for Block<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Block")
            .field("frames", &self.frames())
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("frames_elapsed", &self.frames_elapsed)
            .finish()
    }
}

/// One step of processing, as a value the backend can place.
///
/// `Send` because the backend will move it to whichever thread runs the graph,
/// and `'static` because that thread outlives the call that built it. Between
/// them they rule out exactly the things that would pin a stage down.
pub trait Stage: Send + 'static {
    /// Process one block, in place.
    ///
    /// Called on whatever thread the backend runs the graph on — an audio
    /// thread wherever the platform has one. Do not allocate, do not lock, and
    /// do not touch UI state: report through an atomic instead.
    fn process(&mut self, block: &mut Block<'_>);

    /// A name for diagnostics. Stages are opaque values, so without this a
    /// pipeline is an unreadable list of boxes.
    fn name(&self) -> &'static str {
        "stage"
    }
}

/// An ordered chain of [`Stage`]s.
///
/// Built on whichever thread is convenient and then handed to a backend, which
/// is the moment it stops being the caller's. There is deliberately no way to
/// reach into a pipeline once it is running: a handle back into a stage would
/// be a lock on the audio thread.
#[derive(Default)]
pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    /// An empty pipeline — capture straight through to wherever it is going.
    pub fn new() -> Pipeline {
        Pipeline { stages: Vec::new() }
    }

    /// Append a stage.
    pub fn then(mut self, stage: impl Stage) -> Pipeline {
        self.stages.push(Box::new(stage));
        self
    }

    /// How many stages, for a log line.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// The stage names in order, for diagnostics.
    pub fn names(&self) -> Vec<&'static str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// Run every stage over one block, in order.
    ///
    /// Called by a backend, not by an app. It is `pub` because a backend lives
    /// in another module, and because a test wants to run a pipeline without
    /// standing up an audio device.
    pub fn process(&mut self, block: &mut Block<'_>) {
        for stage in &mut self.stages {
            stage.process(block);
        }
    }
}

impl fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipeline").field("stages", &self.names()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    struct Gain(f32);
    impl Stage for Gain {
        fn process(&mut self, block: &mut Block<'_>) {
            for s in block.samples.iter_mut() {
                *s *= self.0;
            }
        }
        fn name(&self) -> &'static str {
            "gain"
        }
    }

    /// The reporting pattern the module docs describe: a number out through an
    /// atomic, nothing else.
    struct Peak(Arc<AtomicU32>);
    impl Stage for Peak {
        fn process(&mut self, block: &mut Block<'_>) {
            let peak = block.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            self.0.store(peak.to_bits(), Ordering::Relaxed);
        }
        fn name(&self) -> &'static str {
            "peak"
        }
    }

    fn block<'a>(samples: &'a mut [f32], channels: u16) -> Block<'a> {
        Block { samples, sample_rate: 48_000, channels, frames_elapsed: 0 }
    }

    #[test]
    fn stages_run_in_the_order_they_were_added() {
        let mut pipeline = Pipeline::new().then(Gain(2.0)).then(Gain(3.0));
        let mut samples = [1.0f32, -1.0];
        pipeline.process(&mut block(&mut samples, 1));
        assert_eq!(samples, [6.0, -6.0]);
        assert_eq!(pipeline.names(), vec!["gain", "gain"]);
    }

    #[test]
    fn an_empty_pipeline_is_a_straight_wire() {
        let mut pipeline = Pipeline::new();
        let mut samples = [0.25f32, -0.5];
        pipeline.process(&mut block(&mut samples, 1));
        assert_eq!(samples, [0.25, -0.5], "and not silence, which is the failure that hides");
        assert!(pipeline.is_empty());
    }

    #[test]
    fn a_stage_reports_through_an_atomic_and_leaves_the_audio_alone() {
        let peak = Arc::new(AtomicU32::new(0));
        let mut pipeline = Pipeline::new().then(Peak(peak.clone()));
        let mut samples = [0.1f32, -0.75, 0.3];
        pipeline.process(&mut block(&mut samples, 1));
        assert_eq!(f32::from_bits(peak.load(Ordering::Relaxed)), 0.75);
        assert_eq!(samples, [0.1, -0.75, 0.3], "a meter is not a filter");
    }

    #[test]
    fn a_pipeline_is_send_so_a_backend_can_move_it() {
        // The assertion the whole design rests on, and it is a compile-time
        // one: if `Stage` ever loses `Send`, or a stage captures something that
        // is not, this stops building. That is the failure we want — at the
        // definition, not at the point somebody tries to run the graph on an
        // audio thread three months later.
        fn assert_send<T: Send>(_: &T) {}
        let pipeline = Pipeline::new().then(Gain(1.0));
        assert_send(&pipeline);
        std::thread::spawn(move || {
            let mut pipeline = pipeline;
            let mut samples = [1.0f32];
            pipeline.process(&mut block(&mut samples, 1));
            assert_eq!(samples, [1.0]);
        })
        .join()
        .expect("a pipeline survives being moved to another thread");
    }

    #[test]
    fn a_blocks_timestamp_comes_from_its_frame_count() {
        // Not from a clock. A block delivered late still describes the moment
        // it was CAPTURED, which is what a packetiser and a jitter buffer both
        // need — and what a wall clock would get wrong exactly when the machine
        // is busy enough for it to matter.
        let mut samples = [0.0f32; 960];
        let mut b = Block {
            samples: &mut samples,
            sample_rate: 48_000,
            channels: 1,
            frames_elapsed: 48_000,
        };
        assert_eq!(b.pts_micros(), 1_000_000, "one second in");
        assert_eq!(b.frames(), 960);
        b.channels = 2;
        assert_eq!(b.frames(), 480, "interleaved, so stereo is half the frames");
    }
}
