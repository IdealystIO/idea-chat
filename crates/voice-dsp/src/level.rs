//! Levelling — the "rebalanced for quality" half of conditioning.
//!
//! Suppression decides what is *noise*. This decides how *loud* what is left
//! should be, and it matters more than it sounds: a call is a mix, and a mix
//! where one person is at -35 dBFS and another at -8 dBFS is not fixed by
//! removing either one's background hiss. Everyone reaches for the volume
//! slider, and the slider is the wrong control — it moves the whole room.
//!
//! Two stages, in this order:
//!
//! 1. **Automatic gain** — a slow-moving gain that pushes the speaker's
//!    short-term RMS toward a target. Slow on the way up, quick on the way
//!    down, and frozen entirely below the gate.
//! 2. **Peak limiting** — an instantaneous ceiling, because stage 1 works on
//!    an average and averages say nothing about the loudest sample. Without
//!    it, a gain that is right for a sentence clips its plosives.
//!
//! # Why the gate is load-bearing
//!
//! An RMS-targeting gain applied to silence asks the impossible: no gain makes
//! zero reach -20 dBFS, so the gain climbs to its limit and waits there for
//! the next syllable to hit like a slammed door. Worse *after* neural
//! suppression than before it, because the suppressor's output between words
//! is far closer to true digital silence than any microphone's.
//!
//! So below [`Leveling::gate_dbfs`] the whole detector freezes — level *and*
//! gain. It is not a noise gate: nothing is muted, and the residual room tone
//! the suppressor deliberately left in passes through at whatever gain the
//! speech before it earned.
//!
//! Freezing the gain alone is not enough, which is the subtle part. The level
//! detector is deliberately slow (it has to be, or the gain follows the
//! syllables), so at the moment speech stops it spends hundreds of
//! milliseconds *decaying* toward the gate — and every sample of that decay
//! reports a quieter and quieter speaker, which is exactly the input that
//! makes a gain climb. The gain would be most of the way to its limit before
//! the gate ever noticed the silence.
//!
//! Hence two detectors from the same energy: a fast one whose only job is to
//! answer "is anyone speaking *right now*", and the slow one that answers "how
//! loud have they been". The fast one crosses the gate in a few milliseconds
//! and freezes the slow one before it can decay into a lie.

/// Levelling parameters. Every field is in the units a person would argue
/// about, not in coefficients — the coefficients are derived per sample rate
/// when a [`Leveler`] is built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leveling {
    /// Short-term RMS the gain aims for, in dBFS.
    pub target_dbfs: f32,
    /// The most the gain may boost, in dB. Caps how much of a quiet room's
    /// floor a quiet speaker drags up with them.
    pub max_gain_db: f32,
    /// The most the gain may cut, in dB.
    pub min_gain_db: f32,
    /// Time constant for gain *reduction*. Short: being too loud is the
    /// audible failure, so it is corrected quickly.
    pub attack_ms: f32,
    /// Time constant for gain *increase*. Long, so the gain does not audibly
    /// breathe between words.
    pub release_ms: f32,
    /// Below this short-term RMS the gain freezes. See the module docs.
    pub gate_dbfs: f32,
    /// Absolute peak ceiling, in dBFS. Headroom below 0 rather than at it,
    /// because the receiver resamples and a resampler's interpolation can
    /// overshoot a sample that was exactly at full scale.
    pub ceiling_dbfs: f32,
}

impl Default for Leveling {
    fn default() -> Self {
        Leveling {
            // Broadcast speech sits around -20 dBFS RMS; it leaves room for
            // peaks without asking every participant to lean into the mic.
            target_dbfs: -20.0,
            // +18 dB rescues a laptop mic across a desk. Beyond that a
            // suppressor's residue starts arriving with the voice.
            max_gain_db: 18.0,
            min_gain_db: -12.0,
            attack_ms: 15.0,
            release_ms: 400.0,
            // Comfortably under the quietest real speech and comfortably over
            // a suppressed noise floor.
            gate_dbfs: -55.0,
            ceiling_dbfs: -1.0,
        }
    }
}

/// One speaker's levelling state. Not `Copy`: the whole point is that it
/// remembers, so a per-stream instance is the unit.
pub struct Leveler {
    cfg: Leveling,
    /// Smoothing coefficients, derived once for this sample rate.
    env_coef: f32,
    fast_coef: f32,
    attack_coef: f32,
    release_coef: f32,
    limiter_release_coef: f32,
    /// Running mean square of the input, pre-gain. Slow: this is the level
    /// the gain aims at, and it must not follow individual syllables.
    mean_square: f32,
    /// The same energy on a few-millisecond time constant. Used for one thing
    /// only — deciding whether anyone is speaking — because the slow envelope
    /// above cannot answer that question quickly enough to be useful.
    fast_mean_square: f32,
    /// Current gain, linear.
    gain: f32,
    /// Current limiter attenuation, linear, `0.0..=1.0`.
    limiter: f32,
    /// Cached from `cfg` so the hot loop does no `powf`.
    target_lin: f32,
    gate_mean_square: f32,
    min_gain: f32,
    max_gain: f32,
    ceiling: f32,
}

/// One-pole smoothing coefficient for a time constant, at a sample rate.
///
/// `tau_ms <= 0` means "no smoothing" — the value tracks its input exactly,
/// which is what an attack of 0 should mean rather than a division by zero.
fn coefficient(tau_ms: f32, rate: u32) -> f32 {
    if tau_ms <= 0.0 || rate == 0 {
        return 0.0;
    }
    (-1000.0 / (tau_ms * rate as f32)).exp()
}

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

impl Leveler {
    /// Build a leveller for `rate` Hz.
    ///
    /// The rate is fixed for the leveller's life. Everything upstream of this
    /// in both pipelines is DeepFilterNet output, which is always 48 kHz, so a
    /// mid-stream rate change would mean something further up had gone wrong;
    /// [`Leveler::process`] takes no rate for the same reason.
    pub fn new(cfg: Leveling, rate: u32) -> Leveler {
        let target_lin = db_to_lin(cfg.target_dbfs);
        let gate_lin = db_to_lin(cfg.gate_dbfs);
        Leveler {
            cfg,
            // ~50 ms of averaging: long enough to ignore individual pitch
            // periods, short enough to follow a sentence.
            env_coef: coefficient(50.0, rate),
            // 5 ms: about one cycle of the lowest voiced pitch, so it settles
            // within a syllable and collapses within a few of them.
            fast_coef: coefficient(5.0, rate),
            attack_coef: coefficient(cfg.attack_ms, rate),
            release_coef: coefficient(cfg.release_ms, rate),
            // The limiter attacks instantly and recovers over ~120 ms; faster
            // and it audibly pumps, slower and one plosive ducks a whole word.
            limiter_release_coef: coefficient(120.0, rate),
            mean_square: 0.0,
            fast_mean_square: 0.0,
            // Start at unity: the first frames of a call should sound like the
            // microphone, not like a gain sweep.
            gain: 1.0,
            limiter: 1.0,
            target_lin,
            gate_mean_square: gate_lin * gate_lin,
            min_gain: db_to_lin(cfg.min_gain_db),
            max_gain: db_to_lin(cfg.max_gain_db),
            ceiling: db_to_lin(cfg.ceiling_dbfs),
        }
    }

    /// The gain currently applied, in dB. For meters and diagnostics — a
    /// levelled call that sounds wrong is much easier to explain when you can
    /// see the gain sitting at its limit.
    pub fn gain_db(&self) -> f32 {
        20.0 * self.gain.log10()
    }

    /// Level `samples` in place. Mono is assumed, which is what both
    /// suppression paths emit.
    pub fn process(&mut self, samples: &mut [f32]) {
        for sample in samples.iter_mut() {
            let x = *sample;

            // 1. Is anyone speaking right now? The fast detector answers in
            //    milliseconds, which is the whole reason it exists.
            let energy = x * x;
            self.fast_mean_square =
                self.fast_mean_square * self.fast_coef + energy * (1.0 - self.fast_coef);

            // 2. If so, track how loud they have been and move the gain toward
            //    what would put that on target. If not, everything freezes —
            //    see the module docs on why the slow detector must not be left
            //    running through a silence. `sqrt` of a smoothed mean square is
            //    the RMS; comparing squares keeps the gate out of the square
            //    root's way.
            if self.fast_mean_square > self.gate_mean_square {
                self.mean_square =
                    self.mean_square * self.env_coef + energy * (1.0 - self.env_coef);
                let rms = self.mean_square.sqrt();
                let desired = (self.target_lin / rms).clamp(self.min_gain, self.max_gain);
                // Toward a SMALLER gain is the attack: too loud is the failure
                // people hear, so it is the one corrected quickly.
                let coef =
                    if desired < self.gain { self.attack_coef } else { self.release_coef };
                self.gain = desired + (self.gain - desired) * coef;
            }

            let amplified = x * self.gain;

            // 3. Ceiling. The average said nothing about this sample, so the
            //    limiter answers instantly and lets go slowly.
            let magnitude = amplified.abs();
            if magnitude > self.ceiling {
                // Exactly enough attenuation to land on the ceiling, taken
                // immediately — a limiter that eases into it is a limiter that
                // clips on the way.
                self.limiter = self.limiter.min(self.ceiling / magnitude);
            } else {
                self.limiter = 1.0 + (self.limiter - 1.0) * self.limiter_release_coef;
            }

            *sample = amplified * self.limiter;
        }
    }

    /// The configuration this leveller was built with.
    pub fn config(&self) -> Leveling {
        self.cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// `seconds` of a 200 Hz tone at `amplitude` — a stand-in for voiced
    /// speech, which is what the leveller is tuned for.
    fn tone(seconds: f32, amplitude: f32) -> Vec<f32> {
        let n = (RATE as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                (t * 200.0 * std::f32::consts::TAU).sin() * amplitude
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        let sum: f32 = samples.iter().map(|s| s * s).sum();
        (sum / samples.len() as f32).sqrt()
    }

    fn dbfs(x: f32) -> f32 {
        20.0 * x.log10()
    }

    /// The last 500 ms, once the gain has settled. Judging the whole buffer
    /// would average in the ramp and understate where it arrived.
    fn tail(samples: &[f32]) -> &[f32] {
        &samples[samples.len() - RATE as usize / 2..]
    }

    #[test]
    fn quiet_speaker_is_brought_up_to_target() {
        // -32 dBFS RMS: someone sitting back from a laptop microphone. Chosen
        // to need +12 dB — inside the +18 dB cap, so this test measures the
        // target and not the clamp (which has its own test below).
        let mut buf = tone(3.0, 0.0354);
        let before = dbfs(rms(tail(&buf)));
        Leveler::new(Leveling::default(), RATE).process(&mut buf);
        let after = dbfs(rms(tail(&buf)));

        assert!(before < -28.0, "the fixture really is quiet: {before:.1} dBFS");
        assert!(
            (after - -20.0).abs() < 2.0,
            "settles near the -20 dBFS target, got {after:.1}",
        );
    }

    #[test]
    fn loud_speaker_is_brought_down_to_target() {
        // -10 dBFS RMS: someone with the mic close. Needs -10 dB, inside the
        // -12 dB cap, for the same reason as the quiet case.
        let mut buf = tone(3.0, 0.447);
        Leveler::new(Leveling::default(), RATE).process(&mut buf);
        let after = dbfs(rms(tail(&buf)));
        assert!(
            (after - -20.0).abs() < 2.0,
            "settles near the -20 dBFS target, got {after:.1}",
        );
    }

    // The gain is bounded, so a speaker quieter than `max_gain_db` can rescue
    // does NOT get dragged to target — it gets exactly the cap. Asserting the
    // cap rather than the target is the point: the alternative is a leveller
    // that hunts for a target it cannot reach by amplifying the room.
    #[test]
    fn boost_stops_at_the_ceiling_on_the_gain() {
        let cfg = Leveling { max_gain_db: 6.0, ..Leveling::default() };
        let mut buf = tone(3.0, 0.0354);
        let mut lev = Leveler::new(cfg, RATE);
        lev.process(&mut buf);
        assert!(
            (lev.gain_db() - 6.0).abs() < 0.5,
            "gain sits at its +6 dB cap, got {:.1} dB",
            lev.gain_db(),
        );
    }

    // The failure the gate exists to prevent. Without it the gain climbs to
    // `max_gain_db` during silence and the next syllable arrives 18 dB hot.
    #[test]
    fn silence_does_not_wind_the_gain_up() {
        let mut lev = Leveler::new(Leveling::default(), RATE);
        // Speak, then stop for two seconds — five times the release constant,
        // so an ungated gain would be pinned at its limit by the end.
        lev.process(&mut tone(1.0, 0.1));
        let speaking = lev.gain_db();
        lev.process(&mut vec![0.0; RATE as usize * 2]);
        assert!(
            (lev.gain_db() - speaking).abs() < 0.5,
            "gain held through silence: {speaking:.1} -> {:.1} dB",
            lev.gain_db(),
        );
    }

    #[test]
    fn silence_stays_silent() {
        let mut buf = vec![0.0f32; RATE as usize];
        Leveler::new(Leveling::default(), RATE).process(&mut buf);
        assert!(buf.iter().all(|s| *s == 0.0), "no gain makes something of nothing");
    }

    // The limiter's whole job, stated as the property: whatever goes in, the
    // ceiling holds. A transient 12 dB over the average is a hand clap.
    #[test]
    fn peaks_never_exceed_the_ceiling() {
        let mut buf = tone(2.0, 0.2);
        for sample in buf.iter_mut().skip(RATE as usize).take(240) {
            *sample = 0.99;
        }
        Leveler::new(Leveling::default(), RATE).process(&mut buf);

        let ceiling = db_to_lin(-1.0);
        let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= ceiling + 1e-4, "peak {peak:.4} within ceiling {ceiling:.4}");
    }

    // A leveller that produces a NaN poisons every sample after it, and the
    // symptom is silence on the far side with no error anywhere.
    #[test]
    fn extremes_stay_finite() {
        let mut lev = Leveler::new(Leveling::default(), RATE);
        let mut buf: Vec<f32> = (0..RATE as usize)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        lev.process(&mut buf);
        lev.process(&mut vec![0.0; RATE as usize]);
        lev.process(&mut vec![1e-9; RATE as usize]);
        assert!(buf.iter().all(|s| s.is_finite()), "no NaN or inf escaped");
        assert!(lev.gain_db().is_finite());
    }

    // Levelling in one pass and in many must agree — the server feeds it
    // 480-sample hops and the tests feed it seconds, and a leveller whose
    // state did not carry across calls would sound different in production
    // than in every test here.
    #[test]
    fn chunking_does_not_change_the_result() {
        let source = tone(1.0, 0.05);

        let mut whole = source.clone();
        Leveler::new(Leveling::default(), RATE).process(&mut whole);

        let mut chunked = source.clone();
        let mut lev = Leveler::new(Leveling::default(), RATE);
        for hop in chunked.chunks_mut(480) {
            lev.process(hop);
        }

        for (a, b) in whole.iter().zip(chunked.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }
}
