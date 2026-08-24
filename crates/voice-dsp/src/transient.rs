//! Transient suppression — the keystroke half of "noise".
//!
//! DeepFilterNet is a *speech enhancement* model, and it is very good at what
//! it was trained to be good at: noise with structure that persists. Fans,
//! traffic, air conditioning, room tone. Its weakness is the other kind — a key
//! bottoming out, a mug on a desk, a door — because an impulse occupies one or
//! two of its 10 ms hops and looks, inside that hop, a great deal like the
//! onset of a word.
//!
//! This is the stage that handles those, and it works on the property the model
//! does not use: **how a sound starts, and whether it goes on**.
//!
//! ```text
//!   DFN3 ──▶ [ declicker ] ──▶ leveller ──▶ wire
//! ```
//!
//! # What separates a keystroke from a consonant
//!
//! Both are broadband and both start abruptly, so neither of those alone is
//! enough — and getting it wrong is expensive in a way that removing too little
//! noise is not. A suppressor that eats plosives makes people sound like they
//! are lisping, and they cannot tell you why.
//!
//! Three things are measured per sample, and all three have to agree:
//!
//! - **Brightness** — the energy in the first difference of the signal, against
//!   the energy in the signal itself. A key click is almost all edge; a vowel
//!   is almost none. This alone separates clicks from voiced speech, and does
//!   nothing at all for `/s/`.
//! - **Onset ratio** — that same edge energy against a slowly-tracked estimate
//!   of what it has recently been. New is suspicious; ongoing is not.
//! - **Persistence**, implicitly. The floor rises over ~120 ms, so anything
//!   that lasts is absorbed into it and stops being detected. A 10 ms click is
//!   long gone before the floor moves; a 120 ms fricative outlives the
//!   suspicion it starts with. That is why the floor's attack is the most
//!   load-bearing constant here — it *is* the definition of "transient".
//!
//! # What this can and cannot do
//!
//! # Lookahead, and why it is not optional
//!
//! A click's loudest sample is its first one. A detector watching the same
//! sample it is about to output has already let that one through, and by the
//! time the gain is down the keystroke is over — the first version of this
//! stage attenuated a keystroke by 0.8 dB for exactly that reason, while
//! reporting that it had detected every one.
//!
//! So the signal is delayed by [`LOOKAHEAD_MS`] and the detector runs on the
//! undelayed copy: by the time a click reaches the output, the gain has been
//! down for several milliseconds. It costs 3 ms of latency, against
//! DeepFilterNet's 30 ms in the same chain.
//!
//! # What this can and cannot do
//!
//! It ducks; it does not un-mix. A broadband gain reduction removes a keystroke
//! cleanly when it lands between words, and dulls ~20 ms of speech when it
//! lands on top of one. That is the honest limit of doing this in the time
//! domain, and it is why the reduction is capped rather than total: a capped
//! duck on a plosive is a slightly soft `/t/`, while an uncapped one is a hole.
//!
//! Removing a click from *underneath* a vowel needs the spectral domain — an
//! FFT, and a subtraction per bin. That is the next thing to build if this is
//! not enough, and it is a much larger one.

/// How hard to chase transients, in the units a person would argue about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transients {
    /// How much louder than the tracked floor an edge must be to count.
    ///
    /// Lower catches more keystrokes and more consonants with them.
    pub onset_ratio: f32,
    /// How much of the signal must be edge rather than body, `0.0..=1.0`.
    ///
    /// The one that keeps vowels out of it: a 200 Hz tone's first difference is
    /// a couple of percent of its amplitude, a click's is nearly all of it.
    pub brightness: f32,
    /// The most a transient may be attenuated, in dB. Capped on purpose — see
    /// the module docs on what an uncapped duck does to a plosive.
    pub max_reduction_db: f32,
    /// How long the duck takes to let go. Long enough to cover a whole click,
    /// short enough not to swallow the word after it.
    pub release_ms: f32,
}

impl Default for Transients {
    fn default() -> Self {
        Transients {
            onset_ratio: 4.0,
            brightness: 0.30,
            // Enough that a keystroke stops being the loudest thing in the
            // room, not so much that a `/t/` disappears.
            max_reduction_db: 18.0,
            release_ms: 25.0,
        }
    }
}

/// How far the signal is delayed so the gain can get ahead of a click.
///
/// Comfortably more than the gain's attack, so the duck is fully engaged before
/// the sample that triggered it arrives. See the module docs.
pub const LOOKAHEAD_MS: f32 = 3.0;

/// Below this edge level nothing is a transient, whatever the ratios say.
///
/// In near-silence the floor is near-zero too, so the ratio between them is
/// meaningless — dither alone would read as a hail of tiny clicks and the gain
/// would chatter. About -80 dBFS.
const SILENCE: f32 = 1.0e-4;

/// One stream's transient state.
pub struct Declicker {
    cfg: Transients,
    /// Previous sample, for the first difference.
    prev: f32,
    /// Fast envelope of the first difference — the "edge" signal.
    edge: f32,
    /// Fast envelope of the signal itself, for brightness.
    body: f32,
    /// Slowly-tracked background edge level.
    floor: f32,
    /// Current gain, linear.
    gain: f32,
    fast_coef: f32,
    floor_up: f32,
    floor_down: f32,
    attack_coef: f32,
    release_coef: f32,
    /// `max_reduction_db` as a linear gain.
    depth: f32,
    /// Deepest duck applied since the last read. Diagnostics only.
    deepest: f32,
    /// The delay line the detector runs ahead of. See [`LOOKAHEAD_MS`].
    delay: Vec<f32>,
    /// Write cursor into `delay`; the sample it points at is the oldest.
    cursor: usize,
}

fn coefficient(tau_ms: f32, rate: u32) -> f32 {
    if tau_ms <= 0.0 || rate == 0 {
        return 0.0;
    }
    (-1000.0 / (tau_ms * rate as f32)).exp()
}

impl Declicker {
    /// Build a declicker for `rate` Hz.
    pub fn new(cfg: Transients, rate: u32) -> Declicker {
        Declicker {
            cfg,
            prev: 0.0,
            edge: 0.0,
            body: 0.0,
            floor: 0.0,
            gain: 1.0,
            // ~1.5 ms: fast enough to see a click's shape, slow enough not to
            // follow individual waveform cycles.
            fast_coef: coefficient(1.5, rate),
            // The definition of "transient", as above. Rising slowly is what
            // makes a short sound suspicious and a long one ordinary.
            floor_up: coefficient(120.0, rate),
            // Falling faster, so the floor comes back down between words and
            // the next keystroke is measured against silence rather than
            // against the last syllable.
            floor_down: coefficient(250.0, rate),
            // Not instant: a gain step IS a click, and this stage exists to
            // remove those. 0.4 ms is inaudible and still covers a 5 ms edge.
            attack_coef: coefficient(0.4, rate),
            release_coef: coefficient(cfg.release_ms, rate),
            depth: 10f32.powf(-cfg.max_reduction_db / 20.0),
            deepest: 1.0,
            delay: vec![0.0; ((LOOKAHEAD_MS / 1000.0) * rate as f32).max(1.0) as usize],
            cursor: 0,
        }
    }

    /// The deepest duck since the last call, in dB, and reset.
    ///
    /// Separate from [`deepest_db`](Self::deepest_db) because a meter wants
    /// "what happened in this frame" while a test wants "what happened at all".
    pub fn take_deepest_db(&mut self) -> f32 {
        let db = self.deepest_db();
        self.deepest = 1.0;
        db
    }

    /// The deepest duck applied so far, in dB (negative). For meters — a
    /// declicker that is doing nothing and one that is chewing on every vowel
    /// sound very different and look identical.
    pub fn deepest_db(&self) -> f32 {
        20.0 * self.deepest.max(1.0e-6).log10()
    }

    /// The configuration this declicker was built with.
    pub fn config(&self) -> Transients {
        self.cfg
    }

    /// Suppress transients in `samples`, in place. Mono, 48 kHz.
    pub fn process(&mut self, samples: &mut [f32]) {
        for sample in samples.iter_mut() {
            let x = *sample;

            // 1. Edge and body, both on a fast envelope.
            let edge = (x - self.prev).abs();
            self.prev = x;
            self.edge = self.edge * self.fast_coef + edge * (1.0 - self.fast_coef);
            self.body = self.body * self.fast_coef + x.abs() * (1.0 - self.fast_coef);

            // 2. The floor: slow up, quicker down. Anything that lasts is
            //    absorbed into it and stops looking like an onset.
            let coef = if self.edge > self.floor { self.floor_up } else { self.floor_down };
            self.floor = self.edge + (self.floor - self.edge) * coef;

            // 3. All three must agree, and there must be something there at
            //    all — see `SILENCE`.
            let ratio = self.edge / (self.floor + SILENCE);
            let brightness = self.edge / (self.body + SILENCE);
            let transient = self.edge > SILENCE
                && ratio > self.cfg.onset_ratio
                && brightness > self.cfg.brightness;

            let target = if transient { self.depth } else { 1.0 };
            let coef =
                if target < self.gain { self.attack_coef } else { self.release_coef };
            self.gain = target + (self.gain - target) * coef;

            if self.gain < self.deepest {
                self.deepest = self.gain;
            }

            // Output the sample from `LOOKAHEAD_MS` ago, at the gain the
            // detector has worked out since. This is what lets a click be
            // attenuated from its very first sample.
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

    fn dbfs(x: f32) -> f32 {
        20.0 * x.max(1.0e-9).log10()
    }

    fn peak(s: &[f32]) -> f32 {
        s.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    /// Voiced speech, near enough: two harmonics, no edges to speak of.
    fn voice(seconds: f32, amplitude: f32) -> Vec<f32> {
        let n = (RATE as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let a = (t * 200.0 * std::f32::consts::TAU).sin();
                let b = (t * 400.0 * std::f32::consts::TAU).sin() * 0.5;
                (a + b) * amplitude
            })
            .collect()
    }

    /// A keystroke: a few milliseconds of broadband energy that decays.
    fn click_into(buf: &mut [f32], at: usize, amplitude: f32) {
        let len = (RATE as usize) / 200; // 5 ms
        let mut seed = 0x5EED_1234u32.wrapping_add(at as u32);
        for i in 0..len {
            let Some(slot) = buf.get_mut(at + i) else { break };
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 9) as f32 / (1u32 << 23) as f32 * 2.0 - 1.0;
            // Sharp attack, quick decay — the shape of a key bottoming out.
            let envelope = (-(i as f32) / (len as f32 * 0.3)).exp();
            *slot += noise * amplitude * envelope;
        }
    }

    // The whole point. A keystroke over speech gets quieter; the speech under
    // it does not.
    #[test]
    fn a_keystroke_is_ducked_and_the_voice_is_not() {
        let clean = voice(2.0, 0.2);

        let mut typed = clean.clone();
        for k in 0..12 {
            click_into(&mut typed, RATE as usize / 8 + k * RATE as usize / 8, 0.6);
        }

        let mut out = typed.clone();
        Declicker::new(Transients::default(), RATE).process(&mut out);

        // The claim, stated the way it is actually judged by ear: a keystroke
        // stops being the loudest thing in the room. Asserting "reduced by
        // N dB" instead would be measuring the wrong quantity — the output
        // peak cannot fall below the voice's own peak no matter how well the
        // clicks are removed, so a large enough N is unsatisfiable and a small
        // enough one passes on a stage that does nothing.
        let before = dbfs(peak(&typed));
        let after = dbfs(peak(&out));
        let voice_peak = dbfs(peak(&clean));
        assert!(
            after <= voice_peak + 0.5,
            "keystrokes no longer stand above the voice: {before:.1} -> {after:.1} dBFS \
             against a voice peak of {voice_peak:.1} dBFS",
        );

        // And the voice, measured where no click lands, is left alone. The
        // last quarter second contains no keystroke.
        let tail = clean.len() - RATE as usize / 4..;
        let voice_before = dbfs(rms(&clean[tail.clone()]));
        let voice_after = dbfs(rms(&out[tail]));
        assert!(
            (voice_before - voice_after).abs() < 1.0,
            "the voice is untouched: {voice_before:.2} -> {voice_after:.2} dBFS",
        );
    }

    // The failure that would make this unshippable: a declicker that treats
    // every vowel as an onset and chews the whole signal.
    #[test]
    fn steady_speech_is_never_ducked() {
        let mut dec = Declicker::new(Transients::default(), RATE);
        // A fixture that begins at full amplitude IS an onset, and ducking it
        // is correct — so the first 300 ms is warm-up and the claim is about
        // the steady state after it.
        dec.process(&mut voice(0.3, 0.2));
        dec.take_deepest_db();

        dec.process(&mut voice(2.0, 0.2));
        assert!(
            dec.deepest_db() > -0.5,
            "no meaningful reduction was ever applied, got {:.2} dB",
            dec.deepest_db(),
        );
    }

    // A fricative is broadband and starts abruptly, exactly like a click — and
    // must survive, because `/s/` and `/f/` are most of what makes speech
    // intelligible. It survives by LASTING: the floor rises to meet it.
    #[test]
    fn a_fricative_outlives_the_suspicion_it_starts_with() {
        // 250 ms of broadband noise, the length of a long `/s/`.
        let n = RATE as usize / 4;
        let mut seed = 0xABCD_0001u32;
        let mut buf: Vec<f32> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 9) as f32 / (1u32 << 23) as f32 * 2.0 - 1.0) * 0.08
            })
            .collect();
        let before = dbfs(rms(&buf));
        Declicker::new(Transients::default(), RATE).process(&mut buf);

        // Its beginning is legitimately ducked — that is what a transient
        // suppressor does, and 30 ms of a 250 ms sound is not a lisp. What
        // matters is the whole thing surviving.
        let after = dbfs(rms(&buf));
        assert!(
            before - after < 4.0,
            "a sustained fricative is not treated as a click: {before:.1} -> {after:.1} dBFS",
        );

        // And the second half is untouched entirely.
        let late = dbfs(rms(&buf[n / 2..]));
        assert!(late > before - 1.0, "the tail of the fricative is clean: {late:.1} dBFS");
    }

    #[test]
    fn silence_stays_silent() {
        let mut buf = vec![0.0f32; RATE as usize];
        Declicker::new(Transients::default(), RATE).process(&mut buf);
        assert!(buf.iter().all(|s| *s == 0.0));
    }

    // The delay line is the mechanism, so its length is part of the contract:
    // a chain that silently changed its latency would drift lip-sync and put
    // the leveller and the declicker on different timelines.
    #[test]
    fn the_output_is_delayed_by_the_lookahead() {
        let mut buf = vec![0.0f32; 1000];
        buf[0] = 0.5;                       // a lone impulse, too quiet to duck
        let mut dec = Declicker::new(
            Transients { max_reduction_db: 0.0, ..Transients::default() },
            RATE,
        );
        dec.process(&mut buf);
        let expected = ((LOOKAHEAD_MS / 1000.0) * RATE as f32) as usize;
        let at = buf.iter().position(|s| s.abs() > 0.4).expect("the impulse comes out");
        assert_eq!(at, expected, "delayed by exactly the lookahead");
    }

    // The pipeline feeds this 480-sample hops; the tests feed it seconds. They
    // have to agree, or production behaves differently from every test here.
    #[test]
    fn chunking_does_not_change_the_result() {
        let mut source = voice(0.5, 0.2);
        click_into(&mut source, 8000, 0.6);

        let mut whole = source.clone();
        Declicker::new(Transients::default(), RATE).process(&mut whole);

        let mut chunked = source.clone();
        let mut dec = Declicker::new(Transients::default(), RATE);
        for hop in chunked.chunks_mut(480) {
            dec.process(hop);
        }

        for (a, b) in whole.iter().zip(chunked.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn extremes_stay_finite() {
        let mut dec = Declicker::new(Transients::default(), RATE);
        let mut buf: Vec<f32> =
            (0..RATE as usize).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        dec.process(&mut buf);
        dec.process(&mut vec![0.0; RATE as usize]);
        dec.process(&mut vec![1e-9; RATE as usize]);
        assert!(buf.iter().all(|s| s.is_finite()));
        assert!(dec.deepest_db().is_finite());
    }
}
