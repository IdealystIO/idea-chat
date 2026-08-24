//! The model's own opinion, recovered from what it did.
//!
//! DeepFilterNet does not tell us what it thought. But it attenuates what it
//! believes is noise and passes what it believes is speech, so the ratio of
//! what came out to what went in **is** its judgement on that block, in dB, for
//! free. A hop that came out 20 dB down was called noise; one that came out
//! unchanged was called speech, at whatever volume.
//!
//! That is a far better signal than level for deciding what to do with the
//! residue, because a quietly-spoken word and a residual chirp are the same
//! level and only one of them should survive.
//!
//! # Alignment is the whole difficulty
//!
//! The model has about 30 ms of latency, so the block coming out now
//! corresponds to audio that went in three hops ago. Comparing the two as they
//! arrive would put a word's onset against the silence before it and read a
//! huge attenuation exactly where speech is starting.
//!
//! [`Alignment`] is a delay line for the input measurement, sized in hops, so
//! the comparison is between the same moment on both sides.
//!
//! # Why this lives here rather than in either pipeline
//!
//! Both the client and the relay compute it, from different places — the client
//! from a second subscription on the microphone, the relay from the packets it
//! is already decoding. Having one definition of the arithmetic is what keeps
//! "on this device" and "on the server" the same experiment with the work
//! moved, rather than two features that happen to share a name.

/// DeepFilterNet 3's latency, in 10 ms hops.
///
/// One hop of buffering plus two of lookahead — the `conv_lookahead` and
/// `df_lookahead` in the model's own config. If a future model changes those
/// this constant has to change with it, and the symptom of getting it wrong is
/// suppression that nips the front of words.
pub const MODEL_HOPS: usize = 3;

/// Root mean square of a block. The measurement both sides take.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// How much the model changed a block, in dB. Negative means it attenuated.
///
/// Guarded at both ends: silence in and silence out is 0 dB — "unchanged" —
/// which reads as speech and therefore leaves the block alone. That is the
/// right way to be wrong, because doing nothing to silence is free.
pub fn model_gain_db(output_rms: f32, input_rms: f32) -> f32 {
    const FLOOR: f32 = 1.0e-9;
    20.0 * ((output_rms + FLOOR) / (input_rms + FLOOR)).log10()
}

/// Holds input measurements until the output they belong to arrives.
pub struct Alignment {
    ring: Vec<f32>,
    cursor: usize,
}

impl Alignment {
    /// A delay line `hops` long. [`MODEL_HOPS`] is the one to pass unless you
    /// are aligning against something other than DeepFilterNet.
    pub fn new(hops: usize) -> Alignment {
        Alignment { ring: vec![0.0; hops.max(1)], cursor: 0 }
    }

    /// Record this hop's input energy and return the one it should be compared
    /// against — the measurement from `hops` ago.
    ///
    /// Returns 0.0 until the line has filled, which makes the first few blocks
    /// of a call read as "unchanged" and pass through. Starting a call by
    /// expanding the first 30 ms would be a poor trade for the precision.
    pub fn delay(&mut self, input_rms: f32) -> f32 {
        let then = self.ring[self.cursor];
        self.ring[self.cursor] = input_rms;
        self.cursor = (self.cursor + 1) % self.ring.len();
        then
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero_and_of_a_constant_is_itself() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0; 480]), 0.0);
        assert!((rms(&[0.5; 480]) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn an_unchanged_block_reads_as_zero_db() {
        assert!(model_gain_db(0.1, 0.1).abs() < 1e-3);
    }

    #[test]
    fn attenuation_reads_negative() {
        assert!((model_gain_db(0.01, 0.1) - -20.0).abs() < 0.01);
    }

    // Silence must not read as "the model removed everything", or every pause
    // would be expanded on the strength of a division by almost zero.
    #[test]
    fn silence_reads_as_unchanged() {
        assert!(model_gain_db(0.0, 0.0).abs() < 1e-3);
    }

    #[test]
    fn the_line_returns_what_went_in_that_many_hops_ago() {
        let mut a = Alignment::new(3);
        assert_eq!(a.delay(1.0), 0.0);
        assert_eq!(a.delay(2.0), 0.0);
        assert_eq!(a.delay(3.0), 0.0);
        assert_eq!(a.delay(4.0), 1.0);
        assert_eq!(a.delay(5.0), 2.0);
        assert_eq!(a.delay(6.0), 3.0);
    }

    #[test]
    fn a_zero_length_line_still_works() {
        let mut a = Alignment::new(0);
        assert_eq!(a.delay(1.0), 0.0);
        assert_eq!(a.delay(2.0), 1.0);
    }
}
