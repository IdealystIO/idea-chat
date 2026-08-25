//! Peak metering — what a level indicator should show, as opposed to what the
//! last packet happened to contain.
//!
//! # Why this is not `max(&samples)`
//!
//! It used to be. Each arriving chunk overwrote the meter with its own peak,
//! and that read acceptably while a chunk was 85 ms of audio: speech has
//! energy somewhere in most 85 ms windows, so the number moved but stayed
//! plausible.
//!
//! Cutting packets to 20 ms broke it, and broke it in the direction that looks
//! like a fault elsewhere. A fiftieth of a second lands between syllables far
//! more often than a twelfth does, so a meter showing only the newest one sits
//! at zero most of the time and flickers the rest — and a listener watching it
//! concludes they cannot be heard. Nothing about the audio changed.
//!
//! A real meter therefore *holds*: it takes the peak instantly and falls back
//! slowly, so the eye sees an envelope rather than a sample. That is what
//! hardware meters have always done, and the fall time is the only parameter
//! worth having.

/// Seconds for a held peak to fall to a tenth of its value — about 20 dB.
///
/// 400 ms is slow enough to bridge the gaps inside a word and fast enough that
/// letting go of a talk button reads as immediate. Faster and it flickers on
/// 20 ms packets; slower and a meter stays lit after the room has gone quiet,
/// which is worse than flicker because it is wrong rather than merely busy.
pub const FALL_SECS: f32 = 0.4;

/// A peak that rises instantly and falls on a timer.
///
/// Deliberately not a struct with interior state to be shared across a thread
/// boundary: on the capture path the producer is the audio callback and the
/// consumer is a frame loop, and the value between them is one atomic. So this
/// is a pure function over the previous value, and the caller owns the storage.
///
/// ```
/// # use voice_dsp::hold_peak;
/// // A loud packet takes the meter straight to its level.
/// let m = hold_peak(0.0, 0.8, 0.02);
/// assert_eq!(m, 0.8);
/// // Silence afterwards does not blank it.
/// let m = hold_peak(m, 0.0, 0.02);
/// assert!(m > 0.6, "{m}");
/// ```
pub fn hold_peak(previous: f32, fresh: f32, dt_secs: f32) -> f32 {
    // A non-finite or negative reading is a bug upstream, not a level. Treat it
    // as nothing heard rather than poisoning the meter with a NaN that every
    // later `max` would propagate forever.
    let fresh = if fresh.is_finite() { fresh.max(0.0) } else { 0.0 };
    let previous = if previous.is_finite() { previous.max(0.0) } else { 0.0 };
    if dt_secs <= 0.0 {
        return previous.max(fresh);
    }
    let decayed = previous * 0.1f32.powf(dt_secs / FALL_SECS);
    decayed.max(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peak_is_taken_immediately() {
        assert_eq!(hold_peak(0.0, 0.5, 0.02), 0.5);
        assert_eq!(hold_peak(0.2, 0.9, 0.02), 0.9);
    }

    #[test]
    fn it_falls_to_a_tenth_over_the_fall_time() {
        let held = hold_peak(1.0, 0.0, FALL_SECS);
        assert!((held - 0.1).abs() < 1e-5, "{held}");
    }

    #[test]
    fn a_gap_between_syllables_does_not_blank_it() {
        // The case that broke: 20 ms packets, and a talker between words.
        let mut m = hold_peak(0.0, 0.6, 0.02);
        for _ in 0..5 {
            m = hold_peak(m, 0.0, 0.02);
        }
        assert!(m > 0.3, "100 ms of quiet should not empty the meter, got {m}");
    }

    #[test]
    fn silence_does_reach_zero_eventually() {
        // Held is not stuck. Muting someone has to read as muted.
        let mut m = hold_peak(0.0, 1.0, 0.02);
        for _ in 0..100 {
            m = hold_peak(m, 0.0, 0.02);
        }
        assert!(m < 0.005, "two seconds of silence should read as silent, got {m}");
    }

    #[test]
    fn a_nan_reading_does_not_poison_the_meter() {
        // `max` propagates NaN in one direction and not the other, so a single
        // bad sample could pin a meter forever. It cannot.
        let m = hold_peak(0.5, f32::NAN, 0.02);
        assert!(m.is_finite(), "{m}");
        assert!(hold_peak(f32::NAN, 0.5, 0.02).is_finite());
    }

    #[test]
    fn a_zero_interval_holds_rather_than_dividing_by_it() {
        assert_eq!(hold_peak(0.7, 0.1, 0.0), 0.7);
    }
}
