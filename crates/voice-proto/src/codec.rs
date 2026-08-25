//! The bridge between a [`Packet`](crate::Packet) and a codec.
//!
//! Behind the `codec` feature, because the rest of this crate is deliberately
//! dependency-free — a relay forwards packets without linking a codec, and the
//! wire format is the piece most likely to be promoted into an SDK later.
//! Anything that actually turns audio into bytes lives here, on the far side of
//! that line.
//!
//! Both halves of a call need this and they need it to agree, which is the
//! reason it is one module and not two similar ones: the client encodes and the
//! relay decodes on its suppression path, and a disagreement about frame size
//! or fallback rules would be a silent failure on one side only.

use crate::{Encoding, Packet};

/// The encoding side: what this end puts on the wire.
///
/// Opus when the capture format allows it, raw PCM when it does not. The
/// fallback is not defensive padding — Opus accepts 8, 12, 16, 24 or 48 kHz and
/// nothing else, and a browser handing us 44.1 kHz is a real configuration on
/// real hardware. Sending raw at that point is a worse call, not a failed one.
pub struct Encoded {
    kind: Kind,
    note: Option<String>,
}

enum Kind {
    Opus {
        encoder: audio_codec::Encoder,
        /// Reused across every frame, so the send path allocates nothing.
        out: Vec<u8>,
    },
    Pcm,
}

impl Encoded {
    /// Choose an encoding for a capture format.
    ///
    /// `frame` is the interleaved sample count the caller will hand to
    /// [`packet`](Encoded::packet). An encoder expecting a different size would
    /// reject every call, so a disagreement falls back rather than silently
    /// sending nothing.
    pub fn open(rate: u32, channels: u16, frame: usize) -> Encoded {
        match audio_codec::Encoder::new(audio_codec::Settings::default(), rate, channels) {
            Ok(encoder) if encoder.frame() == frame => Encoded {
                kind: Kind::Opus { encoder, out: vec![0; audio_codec::MAX_PACKET] },
                note: None,
            },
            Ok(encoder) => Encoded {
                kind: Kind::Pcm,
                note: Some(format!(
                    "codec frame {} != packet frame {frame} at {rate} Hz — sending raw",
                    encoder.frame(),
                )),
            },
            // Names the rate, because that is almost always the reason and
            // "the codec did not start" on its own sends someone looking at
            // the codec.
            Err(e) => Encoded {
                kind: Kind::Pcm,
                note: Some(format!("no Opus at {rate} Hz x{channels} ({e}) — sending raw")),
            },
        }
    }

    /// Why this fell back to raw, if it did.
    ///
    /// Returned rather than logged so the caller decides where it goes — the
    /// browser console on one side of a call, stderr on the other.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// What this will actually produce.
    pub fn encoding(&self) -> Encoding {
        match self.kind {
            Kind::Opus { .. } => Encoding::Opus,
            Kind::Pcm => Encoding::PcmS16Le,
        }
    }

    /// Build the packet for one frame of audio.
    ///
    /// A failed encode falls back to PCM for that one packet rather than
    /// dropping it: a hole in the audio is worse than a large packet, and the
    /// encoder is stateful enough that one bad frame need not end the call.
    pub fn packet(
        &mut self,
        sender: u32,
        seq: u32,
        rate: u32,
        channels: u16,
        pts_micros: u64,
        samples: &[f32],
    ) -> Packet {
        if let Kind::Opus { encoder, out } = &mut self.kind {
            match encoder.encode(samples, out) {
                Ok(frame) => {
                    return Packet::opus(
                        sender,
                        seq,
                        rate,
                        channels,
                        pts_micros,
                        samples.len() as u32,
                        frame,
                    )
                }
                Err(e) => {
                    self.note = Some(format!("encode failed ({e}) — that frame went raw"));
                }
            }
        }
        Packet::from_f32(sender, seq, rate, channels, pts_micros, samples)
    }
}

/// The decoding side: one talker's packets turned back into audio.
///
/// Per sender, and that is load-bearing. Opus is stateful — the decoder carries
/// the signal model that makes concealment sound like the voice it is
/// concealing — so sharing one across two talkers would blend them.
#[derive(Default)]
pub struct Decoded {
    decoder: Option<audio_codec::Decoder>,
    /// Reused decode target, so the receive path allocates nothing per packet.
    scratch: Vec<f32>,
    note: Option<String>,
}

impl Decoded {
    /// The audio this packet carries, decoding it if it is encoded.
    ///
    /// `None` means the packet cannot be turned into audio at all — an encoding
    /// with no decoder, or a decoder that would not start. Callers count that
    /// as a drop rather than playing silence, because silence indistinguishable
    /// from a quiet room hides the fault.
    pub fn pcm(&mut self, packet: &Packet) -> Option<Vec<f32>> {
        match packet.encoding {
            Encoding::PcmS16Le => packet.to_f32(),
            Encoding::Opus => {
                // Built here rather than up front because a sender's format is
                // not known until they send, and their first packet is the
                // evidence.
                if self.decoder.is_none() {
                    match audio_codec::Decoder::new(
                        audio_codec::Settings::default().frame,
                        packet.sample_rate,
                        packet.channels,
                    ) {
                        Ok(d) => self.decoder = Some(d),
                        Err(e) => {
                            self.note = Some(format!(
                                "no Opus decoder at {} Hz x{} ({e})",
                                packet.sample_rate, packet.channels,
                            ));
                            return None;
                        }
                    }
                }
                let decoder = self.decoder.as_mut()?;
                // The sender said how much audio this is. Trusting the
                // decoder's own frame size instead would desynchronise the
                // moment either side changed packet length.
                self.scratch.clear();
                self.scratch.resize(packet.sample_count as usize, 0.0);
                decoder.decode(Some(&packet.payload), &mut self.scratch);
                Some(self.scratch.clone())
            }
        }
    }

    /// Why a packet could not be decoded, if one could not.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const FRAME: usize = 960;

    fn speech(n: usize, from: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = (from + i) as f32 / RATE as f32;
                let mut v = 0.0;
                for h in 1..=6 {
                    v += (t * 150.0 * h as f32 * core::f32::consts::TAU).sin() / h as f32;
                }
                v * 0.3
            })
            .collect()
    }

    #[test]
    fn a_supported_rate_gets_opus_and_says_nothing() {
        let e = Encoded::open(RATE, 1, FRAME);
        assert_eq!(e.encoding(), Encoding::Opus);
        assert_eq!(e.note(), None, "the ordinary case is not worth a log line");
    }

    #[test]
    fn a_rate_opus_will_not_take_falls_back_and_says_why() {
        // 44.1 kHz is not an Opus rate, and it is what plenty of hardware
        // actually reports.
        let e = Encoded::open(44_100, 1, 882);
        assert_eq!(e.encoding(), Encoding::PcmS16Le);
        let note = e.note().expect("a fallback is worth saying out loud");
        assert!(note.contains("44100"), "the note names the rate: {note}");
    }

    #[test]
    fn a_frame_size_the_encoder_disagrees_with_falls_back() {
        // The encoder is fixed at 20 ms. Asking it for 10 ms frames would make
        // every encode fail, which would be a silent call.
        let e = Encoded::open(RATE, 1, 480);
        assert_eq!(e.encoding(), Encoding::PcmS16Le);
        assert!(e.note().unwrap().contains("!="));
    }

    #[test]
    fn a_frame_survives_the_round_trip_and_gets_much_smaller() {
        let mut enc = Encoded::open(RATE, 1, FRAME);
        let mut dec = Decoded::default();

        // Opus needs a few frames before it is representing the signal well;
        // measuring the first one measures the codec waking up.
        let mut last: Option<(Vec<f32>, Vec<f32>)> = None;
        for i in 0..12 {
            let input = speech(FRAME, i * FRAME);
            let packet = enc.packet(1, i as u32, RATE, 1, 0, &input);
            assert_eq!(packet.encoding, Encoding::Opus);
            assert!(
                packet.payload.len() < 200,
                "20 ms of speech should be tens of bytes, got {}",
                packet.payload.len(),
            );
            // Through the wire, not just through the struct.
            let wire = Packet::decode(&packet.encode()).expect("decodes");
            let output = dec.pcm(&wire).expect("decodable");
            last = Some((input, output));
        }
        let (input, output) = last.expect("twelve frames");
        assert_eq!(output.len(), FRAME);

        let corr = correlation(&input, &output);
        assert!(corr > 0.7, "speech should survive the round trip, correlation {corr}");
    }

    #[test]
    fn a_pcm_sender_is_decoded_by_the_same_receiver() {
        // Mixed encodings coexist in one room: this is what makes a 44.1 kHz
        // client audible to everyone else rather than silent.
        let mut dec = Decoded::default();
        let input = speech(FRAME, 0);
        let packet = Packet::from_f32(1, 0, RATE, 1, 0, &input);
        let out = dec.pcm(&packet).expect("pcm needs no decoder");
        assert_eq!(out.len(), FRAME);
        assert!(correlation(&input, &out) > 0.99, "PCM is lossless bar quantisation");
    }

    /// Pearson correlation — shape, not level, which is what a codec preserves.
    fn correlation(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len()) as f32;
        let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
        let mut num = 0.0;
        let (mut da, mut db) = (0.0, 0.0);
        for (x, y) in a.iter().zip(b) {
            num += (x - ma) * (y - mb);
            da += (x - ma) * (x - ma);
            db += (y - mb) * (y - mb);
        }
        num / (da.sqrt() * db.sqrt()).max(f32::EPSILON)
    }
}
