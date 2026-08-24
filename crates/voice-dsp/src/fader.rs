//! Receive-side gain — the listener's half of the mix.
//!
//! Everything else in this crate runs before the wire and is therefore a
//! decision the *speaker* makes on everyone's behalf. This one runs after it,
//! per sender, on the listening machine, and nobody else can hear the result.
//! That is the whole distinction: suppression and levelling are about what
//! leaves your microphone, and this is about what arrives at your ears.
//!
//! # Why not the `<audio>` element's volume
//!
//! Web playback already has a volume control, and it was the obvious place to
//! put this. Two things rule it out:
//!
//! - It cannot exceed unity, so "turn this person up" is not expressible. The
//!   listener can only ever attenuate, which makes the control useless in the
//!   one case people actually reach for it — the person who is too quiet.
//! - It exists only on web. The native playback path is still the missing
//!   `audio-output` SDK, and a volume control that has to be reimplemented
//!   when that lands is a volume control that will behave differently there.
//!
//! Scaling the PCM before it reaches the writer has neither problem, costs one
//! multiply per sample on a path that is already touching every sample, and is
//! testable without a browser.
//!
//! # Why it ramps, and why it limits
//!
//! **Ramp**, because a gain that changes between one block and the next is a
//! step discontinuity, and a step discontinuity is a click. Muting someone by
//! jumping to zero would produce exactly the artefact the rest of this crate
//! spends its time removing.
//!
//! **Limit**, because gains above unity exist here and the sender already used
//! its headroom: the levelling stage lands peaks just under 0 dBFS, so any
//! boost at all would clip without a ceiling. Limiting turns "boosted too far"
//! into something that sounds squashed, which is recoverable, rather than
//! something that sounds broken, which is not.

/// How long the applied gain takes to reach a newly set target. Long enough to
/// be inaudible, short enough that a volume slider feels connected to the
/// sound rather than lagging behind it.
const RAMP_MS: f32 = 20.0;

/// Peak ceiling, in dBFS. Below 0 rather than at it, matching [`crate::Leveling`]:
/// the samples still have to survive whatever the output device resamples them
/// to, and a resampler's interpolation can overshoot a sample sitting exactly
/// at full scale.
const CEILING_DBFS: f32 = -1.0;

/// The most a listener may boost one sender, linear. +12 dB is already a lot —
/// it is the difference between someone across a room and someone at the mic —
/// and past it the noise the suppressor deliberately left in comes up with the
/// voice.
pub const MAX_GAIN: f32 = 4.0;

/// Slider position to gain.
///
/// Volume controls are not linear in amplitude, because hearing is not: a
/// linear slider spends its top half on differences nobody can hear and its
/// bottom half falling off a cliff. Squaring the position approximates the
/// usual audio taper closely enough, and has the property that matters most
/// here — **`taper(1.0)` is exactly unity**, so "100%" means "as sent" rather
/// than "some number we picked".
///
/// `position` is in the units the UI shows: `1.0` is 100%.
pub fn taper(position: f32) -> f32 {
    let p = position.max(0.0);
    (p * p).min(MAX_GAIN)
}

/// One sender's listener-side gain.
///
/// Holds the ramp and limiter state, so there is one per peer per call and it
/// is built with the gain it should already be at — see [`Fader::new`].
pub struct Fader {
    /// Where the gain is heading. Set by the listener.
    target: f32,
    /// Where the gain actually is. Chases `target` over [`RAMP_MS`].
    gain: f32,
    ramp_coef: f32,
    limiter: f32,
    limiter_release_coef: f32,
    ceiling: f32,
    rate: u32,
}

/// One-pole smoothing coefficient for a time constant, at a sample rate.
fn coefficient(tau_ms: f32, rate: u32) -> f32 {
    if tau_ms <= 0.0 || rate == 0 {
        return 0.0;
    }
    (-1000.0 / (tau_ms * rate as f32)).exp()
}

impl Fader {
    /// Build a fader for `rate` Hz, already settled at `gain`.
    ///
    /// Settled, not ramping toward it, and that is deliberate: a peer whose
    /// volume was turned down before they spoke should be quiet from their
    /// first syllable, not fade in from full over 20 ms. The ramp is for
    /// *changes*, and the initial value is not a change.
    pub fn new(rate: u32, gain: f32) -> Fader {
        let gain = gain.clamp(0.0, MAX_GAIN);
        Fader {
            target: gain,
            gain,
            ramp_coef: coefficient(RAMP_MS, rate),
            limiter: 1.0,
            // Same 120 ms as the levelling limiter: faster pumps audibly,
            // slower ducks a whole word behind one plosive.
            limiter_release_coef: coefficient(120.0, rate),
            ceiling: 10f32.powf(CEILING_DBFS / 20.0),
            rate,
        }
    }

    /// The sample rate this fader's coefficients were derived for. A stream
    /// that changes rate mid-call needs a new fader, not a re-used one.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Aim for `gain`. Reached over [`RAMP_MS`], not immediately.
    pub fn set_gain(&mut self, gain: f32) {
        self.target = gain.clamp(0.0, MAX_GAIN);
    }

    /// The gain being aimed for — what the listener asked for, which is not
    /// necessarily what is applied to this instant's samples.
    pub fn gain(&self) -> f32 {
        self.target
    }

    /// Apply the gain to `samples` in place.
    ///
    /// Interleaved multi-channel audio is fine: gain is per sample and the
    /// limiter's ceiling is per sample, so neither cares how the samples are
    /// grouped.
    pub fn process(&mut self, samples: &mut [f32]) {
        // Unity with nothing to ramp toward and no limiting in reach is the
        // common case — a listener who has never touched a volume control —
        // and it should cost nothing.
        if self.gain == 1.0 && self.target == 1.0 && self.limiter == 1.0 {
            return;
        }
        for sample in samples.iter_mut() {
            self.gain = self.target + (self.gain - self.target) * self.ramp_coef;
            let amplified = *sample * self.gain;

            let magnitude = amplified.abs();
            if magnitude > self.ceiling {
                self.limiter = self.limiter.min(self.ceiling / magnitude);
            } else {
                self.limiter = 1.0 + (self.limiter - 1.0) * self.limiter_release_coef;
            }

            *sample = amplified * self.limiter;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn tone(seconds: f32, amplitude: f32) -> Vec<f32> {
        let n = (RATE as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                (t * 200.0 * core::f32::consts::TAU).sin() * amplitude
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        let sum: f32 = samples.iter().map(|s| s * s).sum();
        (sum / samples.len() as f32).sqrt()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// The default is "leave it exactly as it arrived". A receive-side stage
    /// that colours the signal when nobody asked it to would be a bug nobody
    /// could attribute.
    #[test]
    fn unity_is_a_pass_through() {
        let mut fader = Fader::new(RATE, 1.0);
        let original = tone(0.2, 0.3);
        let mut samples = original.clone();
        fader.process(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn attenuation_scales_the_signal() {
        let mut fader = Fader::new(RATE, 0.5);
        let mut samples = tone(0.2, 0.3);
        let before = rms(&samples);
        fader.process(&mut samples);
        let after = rms(&samples);
        assert!((after / before - 0.5).abs() < 0.01, "expected half, got {}", after / before);
    }

    #[test]
    fn boost_is_available_above_unity() {
        let mut fader = Fader::new(RATE, 2.0);
        let mut samples = tone(0.2, 0.1);
        let before = rms(&samples);
        fader.process(&mut samples);
        assert!(rms(&samples) > before * 1.9);
    }

    /// The reason the limiter is here at all: the sender's levelling already
    /// lands peaks just under full scale, so any boost would clip.
    #[test]
    fn boost_does_not_clip() {
        let mut fader = Fader::new(RATE, 4.0);
        let mut samples = tone(0.5, 0.7);
        fader.process(&mut samples);
        assert!(peak(&samples) <= 1.0, "clipped at {}", peak(&samples));
    }

    /// A volume change mid-stream must not be a step, because a step is a
    /// click — the exact artefact the rest of this crate exists to remove.
    #[test]
    fn a_gain_change_does_not_step() {
        let mut fader = Fader::new(RATE, 1.0);
        let mut first = tone(0.05, 0.3);
        fader.process(&mut first);

        fader.set_gain(0.0);
        let mut second = tone(0.05, 0.3);
        fader.process(&mut second);

        // Across the seam AND within the ramp, no single sample may jump by
        // more than a fraction of full scale.
        let joined: Vec<f32> = first.iter().chain(second.iter()).copied().collect();
        let biggest = joined.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(biggest < 0.05, "stepped by {biggest}");
    }

    /// Zero really is zero once the ramp has run, so "mute this person" is
    /// silence rather than a very quiet person.
    #[test]
    fn zero_silences_after_the_ramp() {
        let mut fader = Fader::new(RATE, 0.0);
        let mut samples = tone(0.2, 0.5);
        fader.process(&mut samples);
        assert!(peak(&samples) < 1e-6);
    }

    /// A fader built at a gain starts there rather than sliding into it: a
    /// peer turned down before they ever spoke must not be loud for 20 ms.
    #[test]
    fn the_initial_gain_is_not_ramped_into() {
        let mut fader = Fader::new(RATE, 0.25);
        let mut samples = tone(0.005, 0.4);
        fader.process(&mut samples);
        assert!(peak(&samples) <= 0.4 * 0.25 + 1e-4);
    }

    #[test]
    fn the_taper_is_unity_at_one_hundred_percent() {
        assert_eq!(taper(1.0), 1.0);
        assert_eq!(taper(0.0), 0.0);
        assert!((taper(0.5) - 0.25).abs() < 1e-6);
        assert!((taper(2.0) - 4.0).abs() < 1e-6);
    }

    /// The taper's ceiling and the fader's are the same number, so a UI that
    /// only ever passes tapered positions can never ask for more than the
    /// fader will give — no silent clamp between the slider and the sound.
    #[test]
    fn the_taper_cannot_outrun_the_fader() {
        assert_eq!(taper(10.0), MAX_GAIN);
        let mut fader = Fader::new(RATE, taper(10.0));
        assert_eq!(fader.gain(), MAX_GAIN);
        fader.set_gain(taper(10.0));
        assert_eq!(fader.gain(), MAX_GAIN);
    }
}
