//! The last stage: what is still there after the model and the declicker.
//!
//! DeepFilterNet does not remove noise, it *attenuates* it, and what survives
//! is a quiet, shifting bed that the ear finds easy to hear precisely because
//! it is what remains when nobody is talking. The declicker takes the sharp
//! things out of it. This takes the rest.
//!
//! ```text
//!   DFN3 ──▶ declicker ──▶ leveller ──▶ [ expander ] ──▶ wire
//! ```
//!
//! # Last on purpose
//!
//! After the leveller, not before it. The leveller boosts a quiet speaker by up
//! to 18 dB, and it has no way to tell their voice from the noise underneath —
//! so an expander placed before it would have its work amplified straight back
//! in. Nothing after this stage may make anything louder.
//!
//! # An expander, not a gate
//!
//! A gate is a switch, and a switch on a voice channel is the failure everybody
//! has heard: the room goes completely dead between sentences, breaths vanish,
//! and the first syllable after a pause arrives with the door already swinging.
//! People describe it as "you keep cutting out" and they are describing a gate.
//!
//! So this is an expander with a capped depth. Below the threshold the signal
//! is pushed *down*, by at most [`Residual::depth_db`], and never to nothing.
//! The room stays a room; it just gets quieter.
//!
//! # The model already decided; ask it
//!
//! The obvious way to drive this stage is by level: quiet is noise, loud is
//! speech. It is also wrong in the case that matters, because a quietly-spoken
//! word and a residual chirp are the same level and only one of them should
//! survive.
//!
//! There is a much better signal available, and we have already paid for it.
//! DeepFilterNet attenuates what it believes is noise and passes what it
//! believes is speech, so **how much it changed a block is its own verdict on
//! that block**. A hop that came out 20 dB quieter than it went in is a hop the
//! model called noise — and the chirp that survives is precisely the part it
//! could not quite remove. A hop that came out unchanged is speech, at whatever
//! volume.
//!
//! So the primary key here is `model_gain_db`: output energy against input
//! energy, aligned across the model's ~30 ms latency. Below
//! [`Residual::verdict_db`] the model rejected the block and this stage
//! finishes the job.
//!
//! # Without a verdict it does nothing, deliberately
//!
//! The first version of this file judged by level, with a tracked noise floor,
//! and it could not be made to work. A floor that learns during speech climbs
//! until it swallows the speaker; a floor that only learns while the expander
//! is closed can never open in the first place; and a floor primed from the
//! first sample is primed from whatever that sample happened to be. Every
//! variant traded ducking quiet speech against not working.
//!
//! That is not a tuning failure, it is the problem being genuinely ambiguous:
//! a quietly-spoken word and a residual chirp are the same level, and level is
//! all that measurement has. So `None` here means "no opinion available", and
//! the stage passes audio through untouched rather than guessing. The verdict
//! is not an optimisation; it is the only thing that makes this stage possible.
//!
//! # The lookahead leads by more than one hop
//!
//! A verdict describes the block it was measured on, and the model's own
//! lookahead means the first hop of a word can read as rejected — the output
//! has not caught up with the input yet. Taken literally that ducks the front
//! of every sentence.
//!
//! [`LOOKAHEAD_MS`] is longer than one 10 ms hop for exactly that reason: the
//! audio leaving this stage is from the previous block, while the verdict being
//! applied is from the current one. The decision is always at least one block
//! ahead of the sound it is deciding about.

/// How hard to chase what is left, in the units a person would argue about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Residual {
    /// How much attenuation by the model counts as "the model rejected this",
    /// in dB. A hop the model left within this much of where it found it is
    /// treated as speech.
    ///
    /// The whole stage hangs off this number. Nearer zero trusts the model less
    /// and expands more, taking quiet speech with it.
    pub verdict_db: f32,
    /// The most anything below the threshold may be attenuated, in dB. Capped
    /// — see the module docs on why this is not a gate.
    pub depth_db: f32,
    /// How long the expander stays open after the signal drops below the
    /// threshold. Covers the gaps *inside* speech — between words, between
    /// syllables — which are shorter than people think and are what a
    /// hold-less expander chatters on.
    pub hold_ms: f32,
    /// How long the attenuation takes to arrive once the hold has run out.
    /// Slow enough that the room fades rather than drops.
    pub release_ms: f32,
}

impl Default for Residual {
    fn default() -> Self {
        Residual {
            // DeepFilterNet's gain on speech sits close to unity; on noise it
            // goes far below. -9 dB is comfortably between the two.
            verdict_db: -9.0,
            // Enough to put a residual bed below what anyone notices, not so
            // much that a pause sounds like a dropped call.
            depth_db: 14.0,
            hold_ms: 220.0,
            release_ms: 150.0,
        }
    }
}

/// How far ahead the detector runs, so the expander is already open when a
/// word arrives.
///
/// Same lesson as the declicker's lookahead, in the other direction: an
/// expander that opens on the sample it is outputting has already clipped the
/// front of the word that opened it, and a clipped word onset is exactly what
/// "you keep cutting out" sounds like.
pub const LOOKAHEAD_MS: f32 = 12.0;

/// One stream's expander state.
pub struct Expander {
    cfg: Residual,
    /// Current gain, linear.
    gain: f32,
    /// Samples left on the hold timer.
    hold: u32,
    hold_samples: u32,
    attack_coef: f32,
    release_coef: f32,
    /// `depth_db` as a linear gain.
    depth: f32,
    /// Deepest attenuation applied since the last read. Diagnostics only.
    deepest: f32,
    delay: Vec<f32>,
    cursor: usize,
}

fn coefficient(tau_ms: f32, rate: u32) -> f32 {
    if tau_ms <= 0.0 || rate == 0 {
        return 0.0;
    }
    (-1000.0 / (tau_ms * rate as f32)).exp()
}

impl Expander {
    /// Build an expander for `rate` Hz.
    pub fn new(cfg: Residual, rate: u32) -> Expander {
        Expander {
            cfg,
            gain: 1.0,
            hold: 0,
            hold_samples: (cfg.hold_ms / 1000.0 * rate as f32) as u32,
            // Opening is quick — the lookahead covers it. Closing is
            // `release_ms`, which is the audible one.
            attack_coef: coefficient(1.0, rate),
            release_coef: coefficient(cfg.release_ms, rate),
            depth: 10f32.powf(-cfg.depth_db / 20.0),
            deepest: 1.0,
            delay: vec![0.0; ((LOOKAHEAD_MS / 1000.0) * rate as f32).max(1.0) as usize],
            cursor: 0,
        }
    }

    /// The deepest attenuation applied so far, in dB (negative).
    pub fn deepest_db(&self) -> f32 {
        20.0 * self.deepest.max(1.0e-6).log10()
    }

    /// The deepest attenuation since the last call, in dB, and reset.
    pub fn take_deepest_db(&mut self) -> f32 {
        let db = self.deepest_db();
        self.deepest = 1.0;
        db
    }

    /// The configuration this expander was built with.
    pub fn config(&self) -> Residual {
        self.cfg
    }

    /// Expand `samples` in place. Mono, 48 kHz.
    ///
    /// `model_gain_db` is how much the neural stage changed this block —
    /// output energy over input energy, in dB, aligned across the model's
    /// latency. `None` means no opinion is available and the block passes
    /// through untouched; see the module docs on why guessing is worse than
    /// abstaining.
    pub fn process(&mut self, samples: &mut [f32], model_gain_db: Option<f32>) {
        // The model's verdict on this whole block. Per block rather than per
        // sample because that is the granularity the model itself works at —
        // interpolating it would invent precision that is not there.
        let Some(gain_db) = model_gain_db else {
            // No opinion: still run the delay line, so turning the verdict on
            // and off mid-call does not shift the timeline underneath everyone.
            for sample in samples.iter_mut() {
                let delayed = self.delay[self.cursor];
                self.delay[self.cursor] = *sample;
                self.cursor = (self.cursor + 1) % self.delay.len();
                *sample = delayed;
            }
            self.gain = 1.0;
            self.hold = self.hold_samples;
            return;
        };
        let believed = gain_db > self.cfg.verdict_db;

        for sample in samples.iter_mut() {
            let x = *sample;

            if believed {
                self.hold = self.hold_samples;
            } else if self.hold > 0 {
                self.hold -= 1;
            }

            // Flat to the configured depth, not a soft knee. A knee sounds
            // gentler in theory and in practice just means the residue is
            // attenuated by half of what the setting says.
            let target = if believed || self.hold > 0 { 1.0 } else { self.depth };
            let coef =
                if target < self.gain { self.release_coef } else { self.attack_coef };
            self.gain = target + (self.gain - target) * coef;

            if self.gain < self.deepest {
                self.deepest = self.gain;
            }

            let delayed = self.delay[self.cursor];
            self.delay[self.cursor] = x;
            self.cursor = (self.cursor + 1) % self.delay.len();
            *sample = delayed * self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    /// One DeepFilterNet hop.
    const HOP: usize = 480;

    fn dbfs(x: f32) -> f32 {
        20.0 * x.max(1.0e-9).log10()
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    fn noise(seconds: f32, amplitude: f32, seed0: u32) -> Vec<f32> {
        let n = (RATE as f32 * seconds) as usize;
        let mut seed = seed0;
        (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 9) as f32 / (1u32 << 23) as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    fn voice(seconds: f32, amplitude: f32) -> Vec<f32> {
        let n = (RATE as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                ((t * 200.0 * std::f32::consts::TAU).sin()
                    + (t * 400.0 * std::f32::consts::TAU).sin() * 0.5)
                    * amplitude
            })
            .collect()
    }

    /// Run a buffer through hop by hop, with one verdict per hop — the way the
    /// real pipeline feeds it.
    fn run(buf: &mut [f32], verdict: impl Fn(usize) -> Option<f32>) -> Expander {
        let mut exp = Expander::new(Residual::default(), RATE);
        for (i, hop) in buf.chunks_mut(HOP).enumerate() {
            exp.process(hop, verdict(i));
        }
        exp
    }

    // The point. A block the model attenuated hard was, in the model's own
    // judgement, noise — and whatever survived it is residue.
    #[test]
    fn what_the_model_rejected_is_pushed_the_rest_of_the_way_down() {
        let mut buf = noise(2.0, 0.003, 0x1111);
        let before = dbfs(rms(&buf));
        run(&mut buf, |_| Some(-20.0));

        // After the hold has run out.
        let after = dbfs(rms(&buf[buf.len() - RATE as usize / 2..]));
        assert!(
            before - after > 10.0,
            "residue the model rejected comes down: {before:.1} -> {after:.1} dBFS",
        );
    }

    // ...and it stays an expander. Something remains.
    #[test]
    fn it_never_closes_completely() {
        let mut buf = noise(2.0, 0.003, 0x2222);
        let exp = run(&mut buf, |_| Some(-20.0));
        assert!(
            exp.deepest_db() >= -(Residual::default().depth_db + 0.5),
            "attenuation is capped at the configured depth, got {:.1} dB",
            exp.deepest_db(),
        );
        assert!(rms(&buf[buf.len() - RATE as usize / 4..]) > 0.0, "quieter, not gone");
    }

    // A block the model kept is speech, at whatever volume — including a
    // volume the level-only version of this stage would have eaten.
    #[test]
    fn what_the_model_kept_is_untouched_however_quiet() {
        let mut buf = voice(1.0, 0.004); // about -50 dBFS: a whisper
        let before = dbfs(rms(&buf));
        run(&mut buf, |_| Some(-1.0));
        let after = dbfs(rms(&buf[RATE as usize / 2..]));
        assert!(
            (before - after).abs() < 0.5,
            "quiet speech the model kept survives: {before:.2} -> {after:.2} dBFS",
        );
    }

    // The abstention. No verdict, no opinion — and crucially the delay line
    // still runs, so switching the verdict on and off cannot shift everyone's
    // timeline mid-call.
    #[test]
    fn without_a_verdict_it_only_delays() {
        let source = noise(0.5, 0.003, 0x3333);
        let mut buf = source.clone();
        run(&mut buf, |_| None);

        let delay = ((LOOKAHEAD_MS / 1000.0) * RATE as f32) as usize;
        for (i, out) in buf.iter().enumerate().skip(delay) {
            assert!((out - source[i - delay]).abs() < 1e-9, "sample {i} passed through");
        }
    }

    // The failure the lookahead exists for: a word's first hop can read as
    // rejected, because the model's own lookahead means its output has not
    // caught up with its input yet.
    #[test]
    fn a_wrong_verdict_on_a_words_first_hop_does_not_clip_it() {
        let quiet = noise(1.0, 0.003, 0x4444);
        let word = voice(0.3, 0.1);
        let mut buf = quiet.clone();
        buf.extend_from_slice(&word);

        let quiet_hops = quiet.len() / HOP;
        let mut out = buf.clone();
        run(&mut out, |i| {
            // Rejected throughout the pause, and for the word's FIRST hop too —
            // the mistake this is meant to survive.
            if i <= quiet_hops { Some(-20.0) } else { Some(-1.0) }
        });

        let delay = ((LOOKAHEAD_MS / 1000.0) * RATE as f32) as usize;
        let start = quiet.len() + delay;
        let window = start..start + RATE as usize * 15 / 1000;
        let want = dbfs(rms(&buf[quiet.len()..quiet.len() + RATE as usize * 15 / 1000]));
        let got = dbfs(rms(&out[window]));
        assert!(
            want - got < 2.0,
            "the start of the word survives a bad first verdict: {want:.1} -> {got:.1} dBFS",
        );
    }

    // Gaps between words are shorter than people think, and an expander that
    // shuts in every one of them is the "you keep cutting out" complaint.
    #[test]
    fn short_gaps_between_words_do_not_close_it() {
        let word = (RATE as f32 * 0.4) as usize / HOP;
        let gap = (RATE as f32 * 0.12) as usize / HOP;   // 120 ms, inside the hold
        let mut buf = voice(0.4, 0.1);
        buf.extend_from_slice(&noise(0.12, 0.003, 0x5555));
        buf.extend_from_slice(&voice(0.4, 0.1));
        let source = buf.clone();

        run(&mut buf, |i| if i < word || i >= word + gap { Some(-1.0) } else { Some(-20.0) });

        let delay = ((LOOKAHEAD_MS / 1000.0) * RATE as f32) as usize;
        let mid = word * HOP + gap * HOP / 2 + delay;
        let window = mid..mid + HOP;
        let want = dbfs(rms(&source[mid - delay..mid - delay + HOP]));
        let got = dbfs(rms(&buf[window]));
        assert!(want - got < 3.0, "a 120 ms gap stays open: {want:.1} -> {got:.1} dBFS");
    }

    #[test]
    fn silence_stays_silent() {
        let mut buf = vec![0.0f32; RATE as usize];
        run(&mut buf, |_| Some(-20.0));
        assert!(buf.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn chunking_does_not_change_the_result() {
        let mut source = noise(0.3, 0.003, 0x6666);
        source.extend_from_slice(&voice(0.2, 0.1));

        let mut whole = source.clone();
        let mut a = Expander::new(Residual::default(), RATE);
        for hop in whole.chunks_mut(HOP) {
            a.process(hop, Some(-20.0));
        }

        // The same audio, same verdicts, delivered in smaller pieces.
        let mut chunked = source.clone();
        let mut b = Expander::new(Residual::default(), RATE);
        for hop in chunked.chunks_mut(HOP) {
            for eighth in hop.chunks_mut(HOP / 8) {
                b.process(eighth, Some(-20.0));
            }
        }
        for (x, y) in whole.iter().zip(chunked.iter()) {
            assert!((x - y).abs() < 1e-6, "{x} vs {y}");
        }
    }

    #[test]
    fn extremes_stay_finite() {
        let mut exp = Expander::new(Residual::default(), RATE);
        let mut buf: Vec<f32> =
            (0..RATE as usize).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        exp.process(&mut buf, Some(-20.0));
        exp.process(&mut vec![0.0; RATE as usize], Some(f32::NEG_INFINITY));
        exp.process(&mut vec![1e-9; RATE as usize], None);
        assert!(buf.iter().all(|s| s.is_finite()));
        assert!(exp.deepest_db().is_finite());
    }
}
