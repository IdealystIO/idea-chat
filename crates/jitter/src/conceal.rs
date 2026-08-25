//! What to play when the packet did not turn up.
//!
//! Silence is the obvious answer and the wrong one. Speech is continuous, so a
//! hole punched in a vowel is a step discontinuity at both edges — which is to
//! say two clicks, the exact artefact the rest of this stack spends its time
//! removing. A 20 ms hole is far more audible as two clicks than as 20 ms of
//! slightly wrong vowel.
//!
//! # What it does instead
//!
//! Voiced speech is periodic over short spans: a vowel repeats its pitch
//! period, somewhere between about 2.5 ms (a high voice) and 20 ms (a low
//! one). So the concealer finds that period in the audio it has just played,
//! and keeps playing it.
//!
//! Three things stop that sounding like a stuck record:
//!
//! - **It fades.** Real speech does not hold one period indefinitely, and a
//!   long gap that keeps repeating becomes a robotic drone — a worse failure
//!   than the silence it replaced, because it sounds like broken software
//!   rather than a broken network. Amplitude reaches zero at
//!   [`Concealer::FADE_MS`].
//! - **It cross-fades back in.** See [`Concealer::observe`]; the join at the
//!   *end* of a gap is a discontinuity too, and the first version of this file
//!   got exactly that wrong.
//! - **It refuses to guess** when there was nothing periodic to begin with.
//!   Unvoiced sound — an `s`, a room, a keyboard — has no period worth
//!   repeating, and the search reports that by finding nothing.
//!
//! This is the classic approach (ITU-T G.711 Appendix I is the canonical
//! write-up) and it is deliberately the *fallback*. A real codec conceals
//! better because it still has the signal model and can resynthesise a
//! plausible continuation, where this can only repeat what it already heard —
//! which is why [`crate::Decode`] hands loss to the codec and this is what a
//! raw-PCM stream uses instead.

/// The longest pitch period searched for — about 50 Hz, below any human
/// fundamental.
const MAX_PERIOD_MS: f32 = 20.0;
/// The shortest — about 400 Hz, above any fundamental.
const MIN_PERIOD_MS: f32 = 2.5;
/// How much history to keep: comfortably more than one longest period plus the
/// window the search compares over.
const HISTORY_MS: f32 = 45.0;
/// Cross-fade length when real audio returns.
const BLEND_MS: f32 = 4.0;

/// Repeats the recent past to cover a gap. One per stream.
pub struct Concealer {
    rate: u32,
    channels: usize,
    /// Recently played REAL audio, interleaved, newest at the end.
    ///
    /// Concealed output is deliberately never fed back in. It would move the
    /// window the repetition is measured from, so the phase would jump every
    /// frame — one click per frame instead of one continuous repetition.
    history: Vec<f32>,
    capacity: usize,
    /// Frames of concealment emitted since the last real frame. The fade is a
    /// function of this rather than of a frame count, so it does not depend on
    /// how the caller happened to choose its frame size.
    emitted: usize,
    /// The period being repeated for the current gap, held so the cross-fade
    /// out of the gap continues the same repetition rather than re-deciding.
    period: usize,
}

impl Concealer {
    /// Amplitude reaches zero this long into a gap.
    pub const FADE_MS: f32 = 60.0;

    pub fn new(rate: u32, channels: u16) -> Concealer {
        let rate = rate.max(1);
        let channels = channels.max(1) as usize;
        let capacity = ((HISTORY_MS / 1000.0) * rate as f32) as usize * channels;
        Concealer {
            rate,
            channels,
            history: Vec::with_capacity(capacity),
            capacity: capacity.max(channels),
            emitted: 0,
            period: 0,
        }
    }

    /// Record a frame that really played — cross-fading into it if the frame
    /// before it was concealed.
    ///
    /// The cross-fade is the half that is easy to forget. A gap has two edges,
    /// and the packet that ENDS a dropout is as unrelated to the concealment
    /// before it as the concealment was to the audio before that. Recovering
    /// audibly is not recovering, so the returning frame is mixed against a
    /// *continuation* of the concealment — the samples the concealer would
    /// have produced for exactly those positions, not the ones it produced
    /// earlier, which are from the wrong point in the waveform.
    pub fn observe(&mut self, frame: &mut [f32]) {
        if self.emitted > 0 && self.period > 0 {
            let n = self.blend_frames().min(frame.len() / self.channels);
            if n > 0 {
                let mut cont = vec![0.0; n * self.channels];
                self.synth(&mut cont, self.emitted);
                for i in 0..n {
                    let t = (i + 1) as f32 / (n + 1) as f32;
                    for c in 0..self.channels {
                        let at = i * self.channels + c;
                        frame[at] = frame[at] * t + cont[at] * (1.0 - t);
                    }
                }
            }
        }
        self.emitted = 0;
        self.period = 0;
        self.push_history(frame);
    }

    /// Fill `out` with concealment for one lost frame.
    pub fn fill(&mut self, out: &mut [f32]) {
        if self.emitted == 0 {
            // A new gap: decide once what to repeat, and hold that decision for
            // the whole gap. Re-deciding per frame would move the phase.
            self.period = self.period();
        }
        if self.period == 0 || self.history.len() < self.period * self.channels {
            // Nothing periodic to repeat — the stream has not started, or is
            // silent, or the sound was unvoiced.
            out.fill(0.0);
            self.emitted += out.len() / self.channels;
            return;
        }
        self.synth(out, self.emitted);
        self.emitted += out.len() / self.channels;
    }

    /// Produce the concealment for the frames starting `from` frames into the
    /// current gap. Pure: used both to fill a lost frame and to build the
    /// overlap that the returning real frame is mixed against.
    fn synth(&self, out: &mut [f32], from: usize) {
        let span = self.period * self.channels;
        let start = self.history.len() - span;
        let fade = ((Self::FADE_MS / 1000.0) * self.rate as f32).max(1.0);
        for (i, sample) in out.iter_mut().enumerate() {
            let frame_index = from + i / self.channels;
            let channel = i % self.channels;
            let gain = (1.0 - frame_index as f32 / fade).clamp(0.0, 1.0);
            let at = start + (frame_index % self.period) * self.channels + channel;
            *sample = self.history[at] * gain;
        }
    }

    fn blend_frames(&self) -> usize {
        (((BLEND_MS / 1000.0) * self.rate as f32) as usize).max(1)
    }

    /// The pitch period of the recent past, in frames. Zero when nothing
    /// periodic was found.
    ///
    /// Normalised autocorrelation over the plausible range: for each candidate
    /// period, how well does the signal line up with itself that far back? The
    /// normalisation matters — a raw correlation peaks at the shortest lag
    /// simply because more energy overlaps there, which would pick the wrong
    /// period on every input.
    fn period(&self) -> usize {
        let frames = self.history.len() / self.channels;
        let min = (((MIN_PERIOD_MS / 1000.0) * self.rate as f32) as usize).max(1);
        let max = (((MAX_PERIOD_MS / 1000.0) * self.rate as f32) as usize).max(min + 1);
        // Compare over one shortest-period window, on channel 0 — the period is
        // a property of the voice, not of the channel it arrived on.
        let window = min;
        if frames < max + window {
            return 0;
        }
        let tail = frames - window;

        let mut best = 0usize;
        let mut best_score = 0.0f32;
        for period in min..=max {
            let mut dot = 0.0f32;
            let mut energy = 0.0f32;
            for i in 0..window {
                let a = self.history[(tail + i) * self.channels];
                let b = self.history[(tail + i - period) * self.channels];
                dot += a * b;
                energy += b * b;
            }
            if energy <= 1e-12 {
                continue;
            }
            let score = dot / energy.sqrt();
            if score > best_score {
                best_score = score;
                best = period;
            }
        }
        best
    }

    fn push_history(&mut self, frame: &[f32]) {
        self.history.extend_from_slice(frame);
        if self.history.len() > self.capacity {
            let excess = self.history.len() - self.capacity;
            self.history.drain(..excess);
        }
    }
}
