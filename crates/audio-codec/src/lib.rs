//! Opus, for a call.
//!
//! # Why a codec at all, when raw PCM already works
//!
//! Three reasons, and only the first is the obvious one.
//!
//! **Bandwidth.** Mono 48 kHz i16 is 768 kbps per talker. Opus does the same
//! job at 24 — a factor of thirty — which is the difference between a room of
//! six being unremarkable and being a problem.
//!
//! **Datagram size.** This is the one that forces the issue. A QUIC datagram
//! must fit in a single packet, so roughly 1200 bytes. One 85 ms block of raw
//! PCM is 8160 bytes: it does not fit, and no amount of tuning makes it fit.
//! A 20 ms Opus frame at 24 kbps is about 60 bytes.
//!
//! **Concealment.** Least obvious, most valuable. A codec that loses a packet
//! can resynthesise a plausible continuation from its own decoder state,
//! because it knows what a voice is. A PCM concealer can only repeat what it
//! already heard. Opus also carries optional **in-band FEC**: a coarse copy of
//! the previous frame rides inside the next one, so a single lost packet is
//! often recovered outright rather than concealed. On a lossy link that is
//! worth more than any amount of jitter buffer.
//!
//! # Which implementation, and why it matters here
//!
//! `rusty-opus` is a pure-Rust port. The usual choice is a binding to libopus,
//! which is C — and C is exactly what cannot be built for
//! `wasm32-unknown-unknown` without a cross-compiler and a libc that does not
//! exist there. A pure-Rust codec compiles for the browser the same way
//! everything else in this workspace does.
//!
//! The cost is that it is a reimplementation rather than the reference, so
//! this crate's tests check that it actually round-trips audio rather than
//! assuming it — see the module tests.

#![forbid(unsafe_code)]

use rusty_opus::{Application, OpusDecoder, OpusEncoder};

/// Opus frame durations that make sense for a call, in milliseconds.
///
/// Not the full set the codec allows. 2.5 and 5 ms exist and are for latency
/// budgets tighter than anything reachable here — the capture block alone is
/// 85 ms — and they spend a large fraction of each packet on header. 40 and 60
/// ms trade latency for bitrate in a direction a conversation does not want.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMs {
    /// Matches DeepFilterNet's hop, so the suppression chain can hand frames
    /// straight over with nothing accumulating in between.
    Ten,
    /// The usual choice for speech: half the packet overhead of 10 ms, and
    /// 10 ms of added delay is not the term that matters in this pipeline.
    Twenty,
}

impl FrameMs {
    pub const fn ms(self) -> u32 {
        match self {
            FrameMs::Ten => 10,
            FrameMs::Twenty => 20,
        }
    }

    /// Samples per channel at `rate`.
    pub fn frames(self, rate: u32) -> usize {
        (rate as usize * self.ms() as usize) / 1000
    }
}

/// What the encoder is being asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// Target bitrate. 24 kbps is transparent-enough mono speech; below about
    /// 16 it starts to sound like a codec rather than a person.
    pub bitrate_bps: i32,
    /// 0..=10. Encoder effort, not quality-per-bit at the low end — this is the
    /// CPU dial, and the browser is the machine that cares.
    pub complexity: i32,
    /// In-band forward error correction: a coarse copy of the previous frame
    /// rides in the next one, so an isolated loss can be *recovered* rather
    /// than concealed.
    ///
    /// **Off by default, and not by preference.** `rusty-opus` 0.9.1 produces
    /// packets its own decoder cannot decode once FEC is actually active —
    /// `fec = true` together with any non-zero [`Settings::expected_loss_pct`]
    /// drops waveform correlation from 0.90 to 0.04 and the output clips.
    /// `fec = true` with a loss estimate of zero is harmless only because the
    /// encoder then declines to spend bits on it.
    ///
    /// The crate's own FEC example exercises `decode_fec` against reference
    /// packets, so the *decoder* side is tested and the encoder's LBRR path
    /// evidently is not. Which side is at fault cannot be settled here without
    /// libopus to cross-check against, and libopus is the C dependency this
    /// crate exists to avoid.
    ///
    /// [`tests::fec_is_still_broken_upstream`] pins the failure, so turning
    /// this on becomes a one-line change the moment it is fixed.
    pub fec: bool,
    /// What the encoder should assume the link is losing. Zero makes FEC
    /// pointless because the encoder will not spend bits on it.
    pub expected_loss_pct: i32,
    /// Discontinuous transmission: stop sending during silence and let the far
    /// end generate comfort noise. Saves bandwidth and, more usefully here,
    /// stops a muted or silent participant costing anything at all.
    pub dtx: bool,
    pub frame: FrameMs,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bitrate_bps: 24_000,
            // 5 rather than 10: this runs inline on the browser's main thread,
            // alongside a neural suppressor that already wants that thread.
            complexity: 5,
            // See the field docs. This is the one setting here that is a
            // workaround rather than a decision.
            fec: false,
            // Kept non-zero even with FEC off: it also tells the encoder how
            // robust to be, and the alternative is discovering the setting was
            // wrong during the call that needed it.
            expected_loss_pct: 5,
            dtx: true,
            frame: FrameMs::Twenty,
        }
    }
}

/// The largest packet this crate will ever produce.
///
/// Sized to fit a QUIC datagram with room for the application header, because
/// a packet that does not fit is not a slow packet, it is a dropped one.
pub const MAX_PACKET: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub &'static str);

impl core::fmt::Display for CodecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "opus: {}", self.0)
    }
}

/// One talker's encoder.
pub struct Encoder {
    inner: OpusEncoder,
    frame: usize,
    channels: usize,
}

impl Encoder {
    pub fn new(settings: Settings, rate: u32, channels: u16) -> Result<Encoder, CodecError> {
        let channels = channels.max(1) as usize;
        // `Voip` rather than `Audio`: it biases toward speech intelligibility
        // and lets SILK handle the low band, which is the whole point here.
        let mut inner = OpusEncoder::new(rate as i32, channels, Application::Voip)
            .map_err(CodecError)?;
        inner.bitrate_bps = settings.bitrate_bps;
        inner.complexity = settings.complexity;
        inner.use_inband_fec = settings.fec;
        inner.packet_loss_perc = settings.expected_loss_pct;
        inner.use_dtx = settings.dtx;
        Ok(Encoder { inner, frame: settings.frame.frames(rate) * channels, channels })
    }

    /// Samples per call to [`Encoder::encode`], interleaved.
    pub fn frame(&self) -> usize {
        self.frame
    }

    /// Encode exactly one frame.
    ///
    /// Returns the packet, which may be very short: with DTX on, a silent
    /// frame encodes to a byte or two meaning "still here, nothing to say".
    /// That is not an error and must still be sent — the far end uses it to
    /// keep its own clock and its comfort noise running.
    pub fn encode<'a>(
        &mut self,
        pcm: &[f32],
        out: &'a mut [u8],
    ) -> Result<&'a [u8], CodecError> {
        if pcm.len() != self.frame {
            return Err(CodecError("frame size does not match the encoder"));
        }
        let per_channel = self.frame / self.channels;
        let n = self.inner.encode(pcm, per_channel, out).map_err(CodecError)?;
        Ok(&out[..n])
    }
}

/// One peer's decoder. Implements [`jitter::Decode`], so the jitter buffer
/// hands it lost packets and it answers with Opus's own concealment.
pub struct Decoder {
    inner: OpusDecoder,
    frame: usize,
    channels: usize,
}

impl Decoder {
    pub fn new(frame: FrameMs, rate: u32, channels: u16) -> Result<Decoder, CodecError> {
        let channels = channels.max(1) as usize;
        let inner = OpusDecoder::new(rate as i32, channels).map_err(CodecError)?;
        Ok(Decoder { inner, frame: frame.frames(rate) * channels, channels })
    }
}

impl jitter::Decode for Decoder {
    fn frame(&self) -> usize {
        self.frame
    }

    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) {
        let per_channel = self.frame / self.channels;
        // An empty slice is how Opus itself spells "this one is missing": the
        // decoder runs its concealment from the state it already has, which is
        // the entire reason the jitter buffer routes loss here rather than
        // handling it itself.
        let result = match packet {
            Some(bytes) => self.inner.decode(bytes, per_channel, out),
            None => self.inner.decode(&[], per_channel, out),
        };
        if result.is_err() {
            // A corrupt packet is a lost packet as far as the listener is
            // concerned. Filling with silence would be a click; the decoder's
            // own concealment is still the best answer available.
            if self.inner.decode(&[], per_channel, out).is_err() {
                out.fill(0.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jitter::Decode;

    const RATE: u32 = 48_000;

    /// Something speech-shaped rather than a pure tone: a fundamental with
    /// harmonics, amplitude-modulated at syllable rate. A codec tuned for
    /// speech is entitled to do badly on a sine, so a sine would measure the
    /// wrong thing.
    fn speech(n: usize, from: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = (from + i) as f32 / RATE as f32;
                let env = 0.5 + 0.5 * (t * 4.0 * std::f32::consts::TAU).sin();
                let mut v = 0.0;
                for h in 1..=6 {
                    v += (t * 140.0 * h as f32 * std::f32::consts::TAU).sin() / h as f32;
                }
                v * 0.25 * env
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Best correlation over any plausible codec delay.
    ///
    /// Correlation rather than a sample-by-sample comparison because a codec
    /// is allowed to delay its output, and because SILK is parametric — it
    /// reproduces the *sound*, not the waveform, so even a perfect VoIP-mode
    /// round trip lands near 0.9 rather than at 1.0.
    fn correlation(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        let mut best = -1.0f32;
        for lag in 0..2000.min(n / 2) {
            let (mut dot, mut ea, mut eb) = (0.0f32, 0.0f32, 0.0f32);
            for i in 0..(n - lag).min(9600) {
                let (x, y) = (a[i], b[i + lag]);
                dot += x * y;
                ea += x * x;
                eb += y * y;
            }
            best = best.max(dot / (ea.sqrt() * eb.sqrt()).max(1e-9));
        }
        best
    }

    /// Encode and decode `count` frames back to back.
    fn round_trip(settings: Settings, count: usize) -> (Vec<f32>, Vec<f32>, Vec<usize>) {
        let mut enc = Encoder::new(settings, RATE, 1).expect("encoder");
        let mut dec = Decoder::new(settings.frame, RATE, 1).expect("decoder");
        let frame = enc.frame();
        let (mut original, mut decoded, mut sizes) = (Vec::new(), Vec::new(), Vec::new());
        let mut scratch = vec![0u8; MAX_PACKET];
        for k in 0..count {
            let pcm = speech(frame, k * frame);
            let packet = enc.encode(&pcm, &mut scratch).expect("encode").to_vec();
            sizes.push(packet.len());
            let mut out = vec![0.0; frame];
            dec.decode(Some(&packet), &mut out);
            original.extend_from_slice(&pcm);
            decoded.extend_from_slice(&out);
        }
        (original, decoded, sizes)
    }

    /// The claim the whole crate rests on. `rusty-opus` is a reimplementation
    /// rather than the reference, so this is checked rather than assumed —
    /// and checking it is what found the FEC bug below.
    #[test]
    fn it_round_trips_speech() {
        let settings = Settings { dtx: false, ..Settings::default() };
        let (original, decoded, _) = round_trip(settings, 60);
        let skip = settings.frame.frames(RATE) * 6;
        let (a, b) = (&original[skip..], &decoded[skip..]);
        assert!(
            correlation(a, b) > 0.8,
            "correlation {:.3} — that is not the same signal",
            correlation(a, b),
        );
        let (ra, rb) = (rms(a), rms(b));
        assert!(rb > ra * 0.7 && rb < ra * 1.4, "level moved: {ra:.3} -> {rb:.3}");
    }

    /// The number that decides whether QUIC datagrams are usable at all. One
    /// 85 ms block of raw PCM is 8160 bytes and simply cannot be a datagram;
    /// this has to sit comfortably inside one.
    #[test]
    fn packets_fit_in_a_datagram() {
        let (_, _, sizes) = round_trip(Settings::default(), 60);
        let biggest = *sizes.iter().max().expect("frames");
        assert!(biggest <= MAX_PACKET, "largest packet {biggest} exceeds our own ceiling");
        assert!(biggest < 400, "24 kbps should not produce {biggest}-byte frames");
        assert!(biggest > 0);
    }

    #[test]
    fn the_bitrate_is_roughly_what_was_asked_for() {
        let settings = Settings { dtx: false, ..Settings::default() };
        let (_, _, sizes) = round_trip(settings, 100);
        let seconds = 100.0 * settings.frame.ms() as f32 / 1000.0;
        let measured = sizes.iter().sum::<usize>() as f32 * 8.0 / seconds;
        assert!(
            measured > 8_000.0 && measured < 48_000.0,
            "asked for {} bps, measured {measured}",
            settings.bitrate_bps,
        );
    }

    /// Pins a third-party bug so that fixing it is noticed.
    ///
    /// `rusty-opus` 0.9.1's encoder emits packets its own decoder mishandles
    /// once in-band FEC is genuinely active. When this test starts failing,
    /// FEC works — turn it on in [`Settings::default`], because recovering a
    /// lost packet outright beats concealing it.
    #[test]
    fn fec_is_still_broken_upstream() {
        let settings = Settings { fec: true, expected_loss_pct: 5, dtx: false, ..Settings::default() };
        let (original, decoded, _) = round_trip(settings, 60);
        let skip = settings.frame.frames(RATE) * 6;
        let c = correlation(&original[skip..], &decoded[skip..]);
        assert!(
            c < 0.5,
            "in-band FEC now round-trips (correlation {c:.3}) — turn `fec` back on by default",
        );
    }

    /// The reason loss is routed to the codec rather than handled by the
    /// jitter buffer: Opus resynthesises from its own decoder state, so a
    /// concealed frame is speech-shaped rather than a repeat.
    #[test]
    fn a_lost_packet_is_concealed_with_sound() {
        let settings = Settings { dtx: false, ..Settings::default() };
        let mut enc = Encoder::new(settings, RATE, 1).expect("encoder");
        let mut dec = Decoder::new(settings.frame, RATE, 1).expect("decoder");
        let frame = enc.frame();
        let mut scratch = vec![0u8; MAX_PACKET];
        let mut out = vec![0.0; frame];
        for k in 0..20 {
            let pcm = speech(frame, k * frame);
            let packet = enc.encode(&pcm, &mut scratch).expect("encode").to_vec();
            dec.decode(Some(&packet), &mut out);
        }
        let before = rms(&out);
        dec.decode(None, &mut out);
        let concealed = rms(&out);
        assert!(
            concealed > before * 0.1,
            "concealed a vowel with near-silence: {concealed} against {before}",
        );
    }

    /// A corrupt packet must be a lost packet, not a panic and not a click.
    #[test]
    fn garbage_is_treated_as_loss() {
        let settings = Settings::default();
        let mut dec = Decoder::new(settings.frame, RATE, 1).expect("decoder");
        let mut out = vec![1234.0; settings.frame.frames(RATE)];
        dec.decode(Some(&[0xFF, 0x00, 0x13, 0x37]), &mut out);
        assert!(out.iter().all(|s| s.is_finite()), "non-finite samples");
        assert!(out.iter().all(|s| s.abs() <= 1.5), "out-of-range samples");
    }

    /// Silence should cost almost nothing. This is what stops a room of six
    /// costing six streams of bandwidth while one person talks.
    #[test]
    fn dtx_makes_silence_cheap() {
        let mut enc = Encoder::new(Settings::default(), RATE, 1).expect("encoder");
        let frame = enc.frame();
        let mut scratch = vec![0u8; MAX_PACKET];
        let silence = vec![0.0f32; frame];
        let sizes: Vec<usize> = (0..40)
            .map(|_| enc.encode(&silence, &mut scratch).expect("encode").len())
            .collect();
        // The first frames still carry the transition into silence.
        let settled = sizes[20..].iter().sum::<usize>() / 20;
        assert!(settled <= 8, "silence still costs {settled} bytes a frame");
    }

    /// The seam this crate exists for: the codec inside the buffer, with loss
    /// and reordering happening together, producing continuous audio.
    ///
    /// Push and pull are interleaved because that is what a call does — and
    /// because pushing everything first trips the buffer's own anti-drift
    /// ceiling, which is correct behaviour that would make this test measure
    /// the wrong thing.
    #[test]
    fn it_plays_through_the_jitter_buffer() {
        let settings = Settings { dtx: false, ..Settings::default() };
        let mut enc = Encoder::new(settings, RATE, 1).expect("encoder");
        let dec = Decoder::new(settings.frame, RATE, 1).expect("decoder");
        let frame = enc.frame();
        let mut buffer = jitter::Buffer::new(jitter::Tuning::default(), dec, RATE, 1);
        let mut scratch = vec![0u8; MAX_PACKET];
        let ms = settings.frame.ms() as f64;

        let mut played = Vec::new();
        let mut out = vec![0.0; frame];
        let mut pending: Option<(u32, Vec<u8>)> = None;
        for seq in 0..90u32 {
            let pcm = speech(frame, seq as usize * frame);
            let packet = enc.encode(&pcm, &mut scratch).expect("encode").to_vec();
            if seq % 9 == 4 {
                // Lost outright.
            } else if seq % 7 == 3 {
                // Held back a frame, so it arrives out of order.
                pending = Some((seq, packet));
            } else {
                let _ = buffer.push(seq, &packet, seq as f64 * ms);
                if let Some((held, bytes)) = pending.take() {
                    let _ = buffer.push(held, &bytes, seq as f64 * ms);
                }
            }
            if seq >= 3 {
                buffer.pull(&mut out);
                played.extend_from_slice(&out);
            }
        }

        let stats = buffer.stats();
        assert!(stats.concealed >= 5, "the drops were not concealed: {stats:?}");
        assert!(stats.reordered > 0, "the reordering was not exercised: {stats:?}");
        assert_eq!(stats.late, 0, "the buffer was too shallow to absorb one frame of delay");
        assert!(played.iter().all(|s| s.is_finite()));
        let audible = &played[frame * 6..];
        assert!(rms(audible) > 0.01, "nothing came out: {}", rms(audible));
    }
}
