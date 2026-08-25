//! Turning what the caller *needs* into what a backend can *do*.
//!
//! This is where cross-platform safety actually lives. [`Latency`] names an
//! intent — "as low as this machine manages", "whatever is comfortable" — and
//! every backend answers it with its own number. A caller that asked for 1024
//! frames because that is what a `ScriptProcessorNode` wanted would be writing
//! a web idiom into an API that also has to serve `cpal`, `AudioRecord` and
//! `AVAudioEngine`, none of which have the same constraints or the same floor.
//!
//! The resolution rules are pure arithmetic over a [`Limits`], so they are
//! tested here rather than inside a backend that only compiles for one target.
//! That matters more than it sounds: every audio backend in this project is
//! `#[cfg]`-gated to a platform, so a test written beside one never runs.

/// What the caller needs, expressed as intent rather than as a block size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latency {
    /// Whatever the platform prefers. Recording to a file, playing one back,
    /// anything nobody is waiting on. Cheapest in CPU, largest in delay.
    Relaxed,
    /// As low as this platform manages without risking dropouts — a call, an
    /// instrument, anything a person is reacting to in real time.
    ///
    /// Deliberately not a number: the honest answer is 128 frames on an
    /// `AudioWorklet` and 1024 on the `ScriptProcessorNode` fallback, and a
    /// caller cannot know which it got. Asking for the intent means the answer
    /// improves when the backend does.
    Interactive,
    /// An explicit block size, in frames per channel. The escape hatch, for a
    /// caller with a reason — matching a codec's frame size, say. Still
    /// adjusted to something the backend accepts.
    Frames(u32),
}

impl Default for Latency {
    fn default() -> Latency {
        Latency::Relaxed
    }
}

/// What one backend can actually do, so [`resolve`] can be tested without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Smallest block this backend will accept.
    pub min_frames: u32,
    /// Largest. A ceiling rather than a preference — nothing asks for it.
    pub max_frames: u32,
    /// Block sizes must be a multiple of this. 128 for an `AudioWorklet`,
    /// whose render quantum is fixed at 128 frames and always will be.
    pub granularity: u32,
    /// Block sizes must also be a power of two. True only for
    /// `ScriptProcessorNode`, which is the one backend here with that rule.
    pub power_of_two: bool,
    /// What [`Latency::Relaxed`] means here.
    pub relaxed_frames: u32,
    /// What [`Latency::Interactive`] means here — this backend's floor for
    /// real-time use, which is not the same as `min_frames`. The minimum is
    /// what the API accepts; this is what it can sustain.
    pub interactive_frames: u32,
}

/// The block size a backend should use for this request.
///
/// Always returns something the backend accepts. Adjusting rather than
/// refusing is deliberate and matches how the platforms themselves behave — a
/// browser is free to clamp a requested sample rate, and `cpal` is free to hand
/// back a different buffer size than the one asked for. Refusing here would
/// fail to open a device over a latency preference, which is never the trade
/// the caller was making.
///
/// What they actually got comes back in [`Opened`](crate::Opened), so a caller
/// who cares can look rather than assume.
pub fn resolve(latency: Latency, limits: &Limits) -> u32 {
    let wanted = match latency {
        Latency::Relaxed => limits.relaxed_frames,
        Latency::Interactive => limits.interactive_frames,
        Latency::Frames(n) => n,
    };
    let clamped = wanted.clamp(limits.min_frames.max(1), limits.max_frames.max(1));
    let shaped = if limits.power_of_two {
        nearest_power_of_two(clamped)
    } else {
        nearest_multiple(clamped, limits.granularity)
    };
    // Shaping can push a value back outside the range — rounding 300 up to 512
    // when the ceiling is 400, say. The range wins, because a block the backend
    // refuses is not a latency compromise, it is a device that will not open.
    shaped.clamp(limits.min_frames.max(1), limits.max_frames.max(1))
}

fn nearest_power_of_two(n: u32) -> u32 {
    let up = n.next_power_of_two();
    let down = (up >> 1).max(1);
    // A tie rounds UP, away from the smaller block. Being one step too large is
    // a millisecond of delay; being one step too small is a dropout.
    if up - n <= n - down {
        up
    } else {
        down
    }
}

fn nearest_multiple(n: u32, granularity: u32) -> u32 {
    let g = granularity.max(1);
    let down = (n / g) * g;
    let up = down + g;
    let down = if down == 0 { g } else { down };
    if up - n <= n - down {
        up
    } else {
        down
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `AudioWorklet`: 128-frame quanta, no power-of-two rule.
    const WORKLET: Limits = Limits {
        min_frames: 128,
        max_frames: 16_384,
        granularity: 128,
        power_of_two: false,
        relaxed_frames: 2048,
        interactive_frames: 256,
    };

    /// The `ScriptProcessorNode` fallback: powers of two, and a much higher
    /// real-time floor, because its callback is on the main thread.
    const SCRIPT_PROCESSOR: Limits = Limits {
        min_frames: 256,
        max_frames: 16_384,
        granularity: 256,
        power_of_two: true,
        relaxed_frames: 4096,
        interactive_frames: 1024,
    };

    #[test]
    fn interactive_means_something_different_on_each_backend() {
        // The whole point of the enum. A caller asking for a number would have
        // had to know which backend it got, which it cannot.
        assert_eq!(resolve(Latency::Interactive, &WORKLET), 256);
        assert_eq!(resolve(Latency::Interactive, &SCRIPT_PROCESSOR), 1024);
        assert!(
            resolve(Latency::Interactive, &WORKLET)
                < resolve(Latency::Interactive, &SCRIPT_PROCESSOR),
            "a worklet is off the main thread, so it can afford a smaller block",
        );
    }

    #[test]
    fn relaxed_is_bigger_than_interactive_everywhere() {
        for limits in [WORKLET, SCRIPT_PROCESSOR] {
            assert!(
                resolve(Latency::Relaxed, &limits) > resolve(Latency::Interactive, &limits),
                "{limits:?}",
            );
        }
    }

    #[test]
    fn an_explicit_request_is_shaped_to_what_the_backend_takes() {
        // 960 frames is 20 ms at 48 kHz — a real thing to ask for, because it
        // is the Opus frame size. Neither backend can give it exactly.
        assert_eq!(resolve(Latency::Frames(960), &WORKLET), 1024, "the nearest 128");
        assert_eq!(resolve(Latency::Frames(960), &SCRIPT_PROCESSOR), 1024, "the nearest power");
        assert_eq!(resolve(Latency::Frames(900), &WORKLET), 896, "and it rounds down when nearer");
    }

    #[test]
    fn a_tie_rounds_up_because_too_small_is_a_dropout() {
        // Exactly between 512 and 1024. One step too large costs a
        // millisecond; one step too small costs an audible glitch.
        assert_eq!(resolve(Latency::Frames(768), &SCRIPT_PROCESSOR), 1024);
        assert_eq!(resolve(Latency::Frames(192), &WORKLET), 256);
    }

    #[test]
    fn a_request_outside_the_range_is_clamped_not_refused() {
        assert_eq!(resolve(Latency::Frames(0), &WORKLET), 128);
        assert_eq!(resolve(Latency::Frames(u32::MAX), &WORKLET), 16_384);
        assert_eq!(resolve(Latency::Frames(1), &SCRIPT_PROCESSOR), 256);
    }

    #[test]
    fn shaping_never_escapes_the_range() {
        // Rounding happens after clamping, so it can push a value back out —
        // and a block the backend refuses is not a compromise, it is a device
        // that will not open.
        let narrow = Limits {
            min_frames: 128,
            max_frames: 400,
            granularity: 128,
            power_of_two: false,
            relaxed_frames: 256,
            interactive_frames: 128,
        };
        let got = resolve(Latency::Frames(390), &narrow);
        assert!(got <= 400, "rounded up past the ceiling: {got}");
        assert!(got >= 128, "rounded down past the floor: {got}");
    }

    #[test]
    fn every_answer_is_one_the_backend_accepts() {
        // The contract, checked exhaustively rather than at the cases somebody
        // thought of. This is the assertion that a new backend's `Limits` has
        // to survive.
        for limits in [WORKLET, SCRIPT_PROCESSOR] {
            for frames in (0..3000).step_by(7) {
                let got = resolve(Latency::Frames(frames), &limits);
                assert!(got >= limits.min_frames, "{frames} -> {got}, under the floor");
                assert!(got <= limits.max_frames, "{frames} -> {got}, over the ceiling");
                if limits.power_of_two {
                    assert!(got.is_power_of_two(), "{frames} -> {got}, not a power of two");
                }
                assert_eq!(got % limits.granularity, 0, "{frames} -> {got}, off the grid");
            }
        }
    }

    #[test]
    fn the_default_is_relaxed_so_a_recorder_pays_nothing_for_this() {
        assert_eq!(Latency::default(), Latency::Relaxed);
    }
}
