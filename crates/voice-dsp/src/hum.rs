//! Hum removal — mains hum and rumble, before anything else sees the signal.
//!
//! ```text
//!   mic ──▶ [ dehum ] ──▶ suppression ──▶ declicker ──▶ leveller ──▶ expander
//! ```
//!
//! # What hum is, and why a suppressor is the wrong tool for it
//!
//! Hum is a ground loop or an unshielded cable picking up the mains: 50 Hz or
//! 60 Hz depending on the country, and its harmonics, because the waveform it
//! picks up is rarely a clean sine. It is the most predictable noise there is —
//! the frequencies are known before the call starts — so it is removed with
//! filters aimed at exactly those frequencies, not with a model that has to
//! work out what it is listening to.
//!
//! First in the chain on purpose. The leveller would otherwise measure hum as
//! part of the speaker's level, and a neural suppressor spends effort on it
//! that it could spend on the noise that is actually hard.
//!
//! # Two kinds of filter
//!
//! - **A high-pass** below the voice. Nothing a person says lives under ~80 Hz,
//!   and everything there — the mains fundamental, desk thumps, HVAC rumble,
//!   a laptop fan's lowest mode — is noise. Second-order, so it rolls off
//!   gently enough not to thin a deep voice.
//! - **Narrow notches** at the mains harmonics, which sit *inside* the voice
//!   band (100–300 Hz is where a voice's fundamental lives). A notch a few
//!   hertz wide takes the hum and leaves the voice: a voice's harmonics move
//!   with its pitch and spend almost no time on any one frequency, while hum
//!   sits on its frequency forever.
//!
//! # Notches only where there is hum
//!
//! A notch a few hertz wide has a long memory: when the voice stops, it keeps
//! "un-removing" its frequency for hundreds of milliseconds. Ten of them at
//! once, after every phrase, is a chord of low tones fading out — heard as a
//! metallic, droning reverb. Measured on real speech followed by silence: with
//! all ten notches always on, the output 200 ms after the voice stopped was
//! still at -35 dB relative to the speech, and -50 dB at 400 ms; the high-pass
//! alone was at -64 dB by 50 ms.
//!
//! So each notch is switched on only once hum has been *measured* at its
//! frequency: a steady narrow peak standing [`Hum::detect_db`] above its
//! neighbours for [`DETECT_WINDOWS`] half-second windows running (and off
//! again only after [`RELEASE_WINDOWS`] without). Hum sits on
//! its frequency forever; a voice's harmonics move with its pitch and do not
//! hold a half-second peak in one place. A line with no hum gets the
//! high-pass and nothing else — no ringing, and none of the warmth the notches
//! would otherwise take out of a low voice.
//!
//! Both 50 Hz and 60 Hz series are watched unless told otherwise; only the
//! one that is actually present ends up notched.

/// Which mains frequency to notch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mains {
    Hz50,
    Hz60,
    /// Both series. The default: which one a caller's building runs on is not
    /// something the app knows.
    Both,
}

/// Hum-removal parameters, in hertz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hum {
    pub mains: Mains,
    /// How many multiples of the mains frequency to notch, fundamental
    /// included. Hum's energy is concentrated in the first few; past ~300 Hz
    /// the notches start to sit on enough of the voice to matter.
    pub harmonics: u32,
    /// Width of each notch at -3 dB. Wide enough to cover the mains drifting
    /// (a grid is held within a fraction of a hertz, multiplied by the
    /// harmonic number), narrow enough to miss a voice passing through.
    pub notch_width_hz: f32,
    /// High-pass corner. `0.0` turns it off.
    pub highpass_hz: f32,
    /// How far a candidate frequency must stand above its neighbours, in a
    /// half-second window, to count as hum in that window.
    pub detect_db: f32,
}

impl Default for Hum {
    fn default() -> Self {
        Hum { mains: Mains::Both, harmonics: 5, notch_width_hz: 4.0, highpass_hz: 80.0, detect_db: 10.0 }
    }
}

/// Consecutive half-second windows of evidence before a notch switches on.
pub const DETECT_WINDOWS: u32 = 2;
/// Consecutive windows WITHOUT evidence before it switches off again. Far
/// longer than switching on, because loud speech can mask a hum's peak for a
/// window or two — and a notch that let go each time would let a burst of hum
/// through each time.
pub const RELEASE_WINDOWS: u32 = 6;
const WINDOW_SECS: f64 = 0.5;
/// Neighbour offset for the detector: far enough to be outside the hum's own
/// peak at this window length (2 Hz resolution), close enough to share its
/// background.
const NEIGHBOUR_HZ: f64 = 7.0;

/// The power of one frequency over a window, one sample at a time.
#[derive(Debug, Clone, Copy)]
struct Goertzel {
    coeff: f64,
    s1: f64,
    s2: f64,
}

impl Goertzel {
    fn new(rate: f64, hz: f64) -> Goertzel {
        Goertzel { coeff: 2.0 * (std::f64::consts::TAU * hz / rate).cos(), s1: 0.0, s2: 0.0 }
    }

    #[inline]
    fn push(&mut self, x: f64) {
        let s = x + self.coeff * self.s1 - self.s2;
        self.s2 = self.s1;
        self.s1 = s;
    }

    /// Power since the last call, and start over.
    fn take(&mut self) -> f64 {
        let p = self.s1 * self.s1 + self.s2 * self.s2 - self.coeff * self.s1 * self.s2;
        self.s1 = 0.0;
        self.s2 = 0.0;
        p
    }
}

/// One mains harmonic: its notch, whether it is in use, and the detector that
/// decides.
struct Candidate {
    notch: Biquad,
    active: bool,
    hits: u32,
    misses: u32,
    at: Goertzel,
    below: Goertzel,
    above: Goertzel,
}

/// One second-order section, transposed direct form II.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// From the RBJ cookbook coefficients, normalised by `a0`.
    fn new(b: [f64; 3], a: [f64; 3]) -> Biquad {
        let n = |x: f64| (x / a[0]) as f32;
        Biquad { b0: n(b[0]), b1: n(b[1]), b2: n(b[2]), a1: n(a[1]), a2: n(a[2]), z1: 0.0, z2: 0.0 }
    }

    fn notch(rate: f64, hz: f64, width_hz: f64) -> Biquad {
        let w = std::f64::consts::TAU * hz / rate;
        let alpha = w.sin() * (std::f64::consts::LN_2 / 2.0 * (width_hz / hz) * w / w.sin()).sinh();
        Biquad::new([1.0, -2.0 * w.cos(), 1.0], [1.0 + alpha, -2.0 * w.cos(), 1.0 - alpha])
    }

    fn highpass(rate: f64, hz: f64) -> Biquad {
        let w = std::f64::consts::TAU * hz / rate;
        let alpha = w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2); // Butterworth, Q = 1/√2
        let c = w.cos();
        Biquad::new([(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0], [1.0 + alpha, -2.0 * c, 1.0 - alpha])
    }

    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// One stream's hum filter. Not `Copy`: filters have memory.
pub struct Dehum {
    highpass: Option<Biquad>,
    candidates: Vec<Candidate>,
    detect_ratio: f64,
    window: usize,
    filled: usize,
}

impl Dehum {
    pub fn new(cfg: Hum, rate: u32) -> Dehum {
        let rate = rate.max(1) as f64;
        let nyquist = rate / 2.0;
        let highpass = (cfg.highpass_hz > 0.0 && (cfg.highpass_hz as f64) < nyquist)
            .then(|| Biquad::highpass(rate, cfg.highpass_hz as f64));
        let mut candidates = Vec::new();
        let series: &[f64] = match cfg.mains {
            Mains::Hz50 => &[50.0],
            Mains::Hz60 => &[60.0],
            Mains::Both => &[50.0, 60.0],
        };
        for &mains in series {
            for h in 1..=cfg.harmonics.max(1) {
                let hz = mains * h as f64;
                if hz + NEIGHBOUR_HZ < nyquist {
                    candidates.push(Candidate {
                        notch: Biquad::notch(rate, hz, cfg.notch_width_hz.max(0.5) as f64),
                        active: false,
                        hits: 0,
                        misses: 0,
                        at: Goertzel::new(rate, hz),
                        below: Goertzel::new(rate, hz - NEIGHBOUR_HZ),
                        above: Goertzel::new(rate, hz + NEIGHBOUR_HZ),
                    });
                }
            }
        }
        Dehum {
            highpass,
            candidates,
            detect_ratio: 10f64.powf(cfg.detect_db as f64 / 10.0),
            window: (rate * WINDOW_SECS) as usize,
            filled: 0,
        }
    }

    /// The frequencies currently being notched, for meters and tests.
    pub fn notched(&self) -> usize {
        self.candidates.iter().filter(|c| c.active).count()
    }

    /// Close a detection window: update each candidate's evidence and switch
    /// its notch on or off.
    fn decide(&mut self) {
        for c in self.candidates.iter_mut() {
            let (at, below, above) = (c.at.take(), c.below.take(), c.above.take());
            let hum = at > self.detect_ratio * 0.5 * (below + above) && at > 1e-6;
            if hum {
                c.hits = (c.hits + 1).min(DETECT_WINDOWS);
                c.misses = 0;
            } else {
                c.hits = 0;
                c.misses = (c.misses + 1).min(RELEASE_WINDOWS);
            }
            if !c.active && c.hits >= DETECT_WINDOWS {
                c.active = true;
                // A clean start rather than whatever the idle state held.
                c.notch.z1 = 0.0;
                c.notch.z2 = 0.0;
            } else if c.active && c.misses >= RELEASE_WINDOWS {
                c.active = false;
            }
        }
    }

    /// Filter `samples` in place. Mono.
    pub fn process(&mut self, samples: &mut [f32]) {
        for sample in samples.iter_mut() {
            // Detection reads the input as it arrives, before any filtering:
            // the question is whether the line hums, not whether it still does.
            let raw = *sample as f64;
            for c in self.candidates.iter_mut() {
                c.at.push(raw);
                c.below.push(raw);
                c.above.push(raw);
            }
            self.filled += 1;
            if self.filled >= self.window {
                self.filled = 0;
                self.decide();
            }

            let mut x = *sample;
            if let Some(hp) = self.highpass.as_mut() {
                x = hp.run(x);
            }
            for c in self.candidates.iter_mut().filter(|c| c.active) {
                x = c.notch.run(x);
            }
            // Denormals: a filter ringing down into silence would otherwise
            // spend its time in subnormal arithmetic, which is very slow.
            *sample = if x.abs() < 1e-20 { 0.0 } else { x };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn sine(hz: f32, secs: f32) -> Vec<f32> {
        (0..(RATE as f32 * secs) as usize)
            .map(|i| (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin() * 0.5)
            .collect()
    }

    fn rms(a: &[f32]) -> f32 {
        (a.iter().map(|v| v * v).sum::<f32>() / a.len().max(1) as f32).sqrt()
    }

    /// Gain in dB through the filter over the last second of four — after the
    /// detector has had time to decide and the filter to settle.
    fn gain_db(cfg: Hum, hz: f32) -> f32 {
        let mut x = sine(hz, 4.0);
        let tail = 3 * RATE as usize;
        let before = rms(&x[tail..]);
        let mut d = Dehum::new(cfg, RATE);
        for c in x.chunks_mut(480) {
            d.process(c);
        }
        20.0 * (rms(&x[tail..]) / before).log10()
    }

    /// A voice-like signal whose pitch glides, as speech does.
    fn gliding_voice(secs: f32) -> Vec<f32> {
        let mut phase = 0.0f32;
        (0..(RATE as f32 * secs) as usize)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let f0 = 120.0 + 40.0 * (t * 2.3).sin();
                phase += std::f32::consts::TAU * f0 / RATE as f32;
                (1..=8).map(|h| (phase * h as f32).sin() / h as f32).sum::<f32>() * 0.2
            })
            .collect()
    }

    #[test]
    fn steady_mains_and_its_harmonics_are_found_and_removed() {
        for hz in [100.0, 120.0, 150.0, 180.0, 240.0, 250.0, 300.0] {
            let g = gain_db(Hum::default(), hz);
            assert!(g < -25.0, "{hz} Hz only came down {g:.1} dB");
        }
    }

    #[test]
    fn rumble_and_the_fundamental_are_cut() {
        assert!(gain_db(Hum::default(), 30.0) < -15.0);
        assert!(gain_db(Hum::default(), 60.0) < -25.0);
    }

    #[test]
    fn the_voice_band_passes() {
        for hz in [135.0, 210.0, 440.0, 1000.0, 3000.0, 6000.0] {
            let g = gain_db(Hum::default(), hz);
            assert!(g.abs() < 1.0, "{hz} Hz changed by {g:.2} dB");
        }
    }

    #[test]
    fn only_the_series_that_is_present_is_notched() {
        let mut d = Dehum::new(Hum::default(), RATE);
        let mut x = sine(60.0, 3.0);
        for c in x.chunks_mut(480) {
            d.process(c);
        }
        assert_eq!(d.notched(), 1, "a pure 60 Hz hum should switch on exactly its own notch");
    }

    #[test]
    fn a_voice_on_its_own_switches_no_notch_on() {
        let mut d = Dehum::new(Hum::default(), RATE);
        let mut x = gliding_voice(6.0);
        for c in x.chunks_mut(480) {
            d.process(c);
        }
        assert_eq!(d.notched(), 0);
    }

    #[test]
    fn nothing_rings_on_after_the_voice_stops() {
        // The metallic drone: notches remembering the voice after it ended.
        let mut x = gliding_voice(3.0);
        let speech = rms(&x);
        x.extend(std::iter::repeat(0.0).take(RATE as usize));
        let mut d = Dehum::new(Hum::default(), RATE);
        for c in x.chunks_mut(480) {
            d.process(c);
        }
        let after = 3 * RATE as usize + 50 * 48;
        let level = 20.0 * (rms(&x[after..after + 2400]) / speech).log10();
        assert!(level < -55.0, "50 ms after the voice stopped the output is still at {level:.1} dB");
    }

    #[test]
    fn a_notch_lets_go_once_the_hum_is_gone() {
        let mut d = Dehum::new(Hum::default(), RATE);
        let mut x = sine(120.0, 3.0);
        x.extend(gliding_voice(5.0));
        for c in x.chunks_mut(480) {
            d.process(c);
        }
        assert_eq!(d.notched(), 0);
    }

    #[test]
    fn silence_stays_silent() {
        let mut x = vec![0.0f32; 4800];
        Dehum::new(Hum::default(), RATE).process(&mut x);
        assert!(x.iter().all(|&v| v == 0.0));
    }
}
