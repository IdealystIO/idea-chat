//! Wire format for one chunk of voice audio.
//!
//! Deliberately dependency-free and framework-free: this is the piece most
//! likely to be promoted into an SDK later, so it must not drag `runtime-core`
//! or `media-stream` along with it.
//!
//! # What the payload is
//!
//! Either raw interleaved i16, or one Opus frame — [`Encoding`] says which,
//! per packet, and both are relayed by the same code.
//!
//! It is per packet and not per deployment because the choice is not always
//! ours. Opus accepts 8, 12, 16, 24 or 48 kHz and nothing else; a microphone
//! handing us 44.1 kHz cannot be encoded without a resampler we do not have.
//! Rather than refuse the call, such a client sends PCM and says so.
//!
//! Raw is also what proved the loop first, and it is worth keeping for that:
//! mono i16 at 48 kHz is ~768 kbps against Opus's ~24, which is a difference
//! you want to be able to A/B rather than take on faith.
//!
//! # Layout (little-endian)
//!
//! ```text
//! offset  size  field
//!      0     1  magic  (0x56 'V')
//!      1     1  version (currently 2)
//!      2     2  channels
//!      4     4  sender
//!      8     4  seq
//!     12     4  sample_rate
//!     16     8  pts_micros
//!     24     4  sample_count  (interleaved samples once DECODED)
//!     28     1  encoding      (0 = pcm_s16le, 1 = opus)
//!     29     3  reserved (zero)
//!     32     n  payload
//! ```
//!
//! `sample_count` describes the decoded audio, not the payload, so it means the
//! same thing under both encodings — for PCM it is `n / 2`, and for Opus it is
//! what the frame expands to. A receiver can therefore size its output buffer
//! and advance its clock without knowing the codec.
//!
//! Version 1 was the same header without the last four bytes, PCM only. It is
//! rejected rather than upgraded: the two coexisted for no release.
//!
//! `pts_micros` is carried straight through from the capture clock so a future
//! jitter buffer has a sender-side timeline to order and pace against. Note the
//! wasm caveat recorded in the design doc: on web the capture clock is
//! `Date.now()`-rebased and NOT monotonic, so a jitter buffer running on the web
//! leg should pace off sample counts rather than trusting this field.

#![forbid(unsafe_code)]

// Cutting capture blocks down to a size a datagram can carry. Lives here
// because it is a wire concern, not an audio one — see the module docs.
pub mod packetise;
pub use packetise::Packetiser;

// Turning audio into a packet's payload and back. Behind a feature, so the
// default build of this crate still links nothing.
#[cfg(feature = "codec")]
pub mod codec;

/// First byte of every packet — cheap guard against a stray text frame or a
/// mismatched endpoint.
pub const MAGIC: u8 = 0x56;
/// Wire-format version. Bump on any layout change.
pub const VERSION: u8 = 2;
/// Bytes before the payload begins.
pub const HEADER_LEN: usize = 32;

/// Why a buffer could not be parsed as a [`Packet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than a header.
    TooShort,
    /// First byte was not [`MAGIC`].
    BadMagic,
    /// Version this build does not understand.
    UnknownVersion(u8),
    /// Header declared more samples than the buffer actually carries.
    Truncated {
        /// Samples the header promised.
        expected: usize,
        /// Samples actually present.
        actual: usize,
    },
    /// A zero `channels` or `sample_rate` — a decoder cannot act on it.
    InvalidFormat,
    /// An encoding byte this build does not know. Forwarding it would hand a
    /// receiver bytes it cannot read, so the relay drops it here instead.
    UnknownEncoding(u8),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::TooShort => write!(f, "buffer shorter than a packet header"),
            DecodeError::BadMagic => write!(f, "bad magic byte"),
            DecodeError::UnknownVersion(v) => write!(f, "unknown wire version {v}"),
            DecodeError::Truncated { expected, actual } => {
                write!(f, "truncated payload: header declared {expected} samples, got {actual}")
            }
            DecodeError::InvalidFormat => write!(f, "zero channels or sample_rate"),
            DecodeError::UnknownEncoding(c) => write!(f, "unknown payload encoding {c}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// How the bytes in a packet's payload are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Interleaved signed 16-bit little-endian samples.
    PcmS16Le,
    /// One Opus frame. Opaque here — this crate does not decode it, which is
    /// what keeps it dependency-free and what lets a relay forward it without
    /// linking a codec.
    Opus,
}

impl Encoding {
    /// The byte written at offset 28.
    pub fn code(self) -> u8 {
        match self {
            Encoding::PcmS16Le => 0,
            Encoding::Opus => 1,
        }
    }

    /// The encoding a byte names, or `None` for one this build does not know.
    pub fn from_code(code: u8) -> Option<Encoding> {
        match code {
            0 => Some(Encoding::PcmS16Le),
            1 => Some(Encoding::Opus),
            _ => None,
        }
    }

    /// The word used in logs and in the wire's own documentation.
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::PcmS16Le => "pcm_s16le",
            Encoding::Opus => "opus",
        }
    }
}

/// One chunk of audio plus the metadata a receiver needs to play it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Which participant produced this chunk. Assigned by the client at join.
    pub sender: u32,
    /// Monotonically increasing per sender. Gaps mean loss; a jitter buffer
    /// uses this to reorder and to drive concealment.
    pub seq: u32,
    /// Samples per second of the decoded audio. Carried per-packet so no
    /// resampling is needed anywhere in this path — the receiver hands this
    /// straight to `AudioWriter::write_pcm_f32`, and the platform sink
    /// resamples.
    pub sample_rate: u32,
    /// Interleaving factor of the decoded audio.
    pub channels: u16,
    /// Capture-clock timestamp (microseconds) for the START of this chunk.
    pub pts_micros: u64,
    /// How to read [`payload`](Packet::payload).
    pub encoding: Encoding,
    /// Interleaved samples this packet carries **once decoded** — that is,
    /// `frames * channels`. Under PCM it is derivable from the payload length;
    /// under Opus it is not, and a receiver needs it to size its output and to
    /// know how much silence to conceal when the packet never arrives.
    pub sample_count: u32,
    /// Encoding-dependent bytes.
    pub payload: Vec<u8>,
}

impl Packet {
    /// Build a PCM packet from normalized `f32` samples, as delivered by an
    /// `AudioFrame`. Values outside `[-1.0, 1.0]` are clamped rather than
    /// allowed to wrap — a wrapped sample is a loud click, and clipping is the
    /// far less objectionable failure.
    pub fn from_f32(
        sender: u32,
        seq: u32,
        sample_rate: u32,
        channels: u16,
        pts_micros: u64,
        samples: &[f32],
    ) -> Self {
        let mut payload = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            payload.extend_from_slice(&f32_to_i16(*s).to_le_bytes());
        }
        Packet {
            sender,
            seq,
            sample_rate,
            channels,
            pts_micros,
            encoding: Encoding::PcmS16Le,
            sample_count: samples.len() as u32,
            payload,
        }
    }

    /// Build a packet around an already-encoded Opus frame.
    ///
    /// `sample_count` is what the frame decodes to — the encoder knows it,
    /// because it is exactly what was handed in.
    pub fn opus(
        sender: u32,
        seq: u32,
        sample_rate: u32,
        channels: u16,
        pts_micros: u64,
        sample_count: u32,
        frame: &[u8],
    ) -> Self {
        Packet {
            sender,
            seq,
            sample_rate,
            channels,
            pts_micros,
            encoding: Encoding::Opus,
            sample_count,
            payload: frame.to_vec(),
        }
    }

    /// The PCM samples, or `None` when the payload is encoded.
    ///
    /// Deliberately not "decode it for me": this crate has no codec and is not
    /// going to grow one. A caller that can handle Opus reaches for
    /// [`payload`](Packet::payload) and its own decoder.
    pub fn samples(&self) -> Option<Vec<i16>> {
        if self.encoding != Encoding::PcmS16Le {
            return None;
        }
        Some(
            self.payload
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect(),
        )
    }

    /// PCM payload back as normalized `f32`, ready for
    /// `AudioWriter::write_pcm_f32`. `None` when the payload is encoded.
    pub fn to_f32(&self) -> Option<Vec<f32>> {
        Some(self.samples()?.into_iter().map(i16_to_f32).collect())
    }

    /// Number of sample frames (samples per channel) once decoded.
    pub fn frame_count(&self) -> usize {
        self.sample_count as usize / self.channels.max(1) as usize
    }

    /// Duration of this chunk in seconds.
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.frame_count() as f64 / self.sample_rate as f64
    }

    /// Serialize to the wire layout documented at the module level.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.channels.to_le_bytes());
        out.extend_from_slice(&self.sender.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&self.pts_micros.to_le_bytes());
        out.extend_from_slice(&self.sample_count.to_le_bytes());
        out.push(self.encoding.code());
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse the wire layout. Rejects rather than guesses: a short payload is
    /// [`DecodeError::Truncated`], not a silently half-filled buffer, because a
    /// relay that corrupts length is a bug worth surfacing loudly.
    pub fn decode(bytes: &[u8]) -> Result<Packet, DecodeError> {
        if bytes.len() < HEADER_LEN {
            return Err(DecodeError::TooShort);
        }
        if bytes[0] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        if bytes[1] != VERSION {
            return Err(DecodeError::UnknownVersion(bytes[1]));
        }
        let channels = u16::from_le_bytes([bytes[2], bytes[3]]);
        let sender = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let seq = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let sample_rate = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let pts_micros = u64::from_le_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        ]);
        let sample_count = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        let encoding =
            Encoding::from_code(bytes[28]).ok_or(DecodeError::UnknownEncoding(bytes[28]))?;

        if channels == 0 || sample_rate == 0 {
            return Err(DecodeError::InvalidFormat);
        }

        let payload = &bytes[HEADER_LEN..];
        // Only PCM has a payload length the header predicts. An Opus frame is
        // whatever the encoder produced, so there is nothing to check — which
        // is also why `sample_count` is carried rather than inferred.
        if encoding == Encoding::PcmS16Le {
            let actual = payload.len() / 2;
            if actual < sample_count as usize {
                return Err(DecodeError::Truncated {
                    expected: sample_count as usize,
                    actual,
                });
            }
        }

        Ok(Packet {
            sender,
            seq,
            sample_rate,
            channels,
            pts_micros,
            encoding,
            sample_count,
            payload: payload.to_vec(),
        })
    }
}

/// Normalized f32 -> i16, clamping instead of wrapping.
///
/// Scales by 32767 (not 32768) so `+1.0` maps to `i16::MAX` exactly and the
/// conversion is symmetric around zero — an asymmetric scale puts a DC offset
/// on every round trip.
#[inline]
pub fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// i16 -> normalized f32, the inverse of [`f32_to_i16`].
#[inline]
pub fn i16_to_f32(v: i16) -> f32 {
    v as f32 / i16::MAX as f32
}

/// The [`Packet::sender`] number for an account id.
///
/// A packet carries four bytes of sender, not sixteen — the header is per
/// 10 ms of audio and a full account id in every one of them is bandwidth
/// spent on something that never changes. So the number is DERIVED, and
/// deriving it in one place is the point of this function: the server mints
/// tickets with it, and the listener has to reverse the same mapping to know
/// whose volume slider affects whose voice. Two spellings of this arithmetic
/// would present as a volume control that moves the wrong person.
///
/// Bytes 8..12 rather than 0..4 because UUIDv7 puts a millisecond timestamp in
/// the front: two accounts created in the same millisecond would collide on
/// the first four bytes and be indistinguishable on the wire. Byte 8 onward is
/// random in both v4 and v7.
///
/// Zero is reserved as "unset" in the relay's query string, so an id that
/// derives to it is nudged to 1.
pub fn participant_id(account: &[u8; 16]) -> u32 {
    let n = u32::from_be_bytes([account[8], account[9], account[10], account[11]]);
    if n == 0 {
        1
    } else {
        n
    }
}

/// Peak absolute amplitude of a normalized f32 buffer, for level meters.
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn sample_packet() -> Packet {
        let samples = [0, 100, -100, i16::MAX, i16::MIN];
        Packet {
            sender: 7,
            seq: 42,
            sample_rate: 48_000,
            channels: 1,
            pts_micros: 1_234_567,
            encoding: Encoding::PcmS16Le,
            sample_count: samples.len() as u32,
            payload: pcm(&samples),
        }
    }

    #[test]
    fn encode_decode_roundtrips_every_field() {
        let p = sample_packet();
        let decoded = Packet::decode(&p.encode()).expect("decodes");
        assert_eq!(decoded, p);
    }

    #[test]
    fn header_len_matches_encoded_prefix() {
        let p = Packet { sample_count: 0, payload: vec![], ..sample_packet() };
        assert_eq!(p.encode().len(), HEADER_LEN);
    }

    #[test]
    fn f32_conversion_is_symmetric_and_clamps() {
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), -i16::MAX);
        // Out-of-range input clamps; it must never wrap to the opposite sign,
        // which is the bug that turns a hot mic into a burst of clicks.
        assert_eq!(f32_to_i16(9.0), i16::MAX);
        assert_eq!(f32_to_i16(-9.0), -i16::MAX);
        assert_eq!(f32_to_i16(0.0), 0);
    }

    #[test]
    fn f32_roundtrip_stays_within_one_lsb() {
        for v in [-1.0f32, -0.5, -0.001, 0.0, 0.001, 0.5, 1.0] {
            let back = i16_to_f32(f32_to_i16(v));
            assert!((back - v).abs() < 1.0 / i16::MAX as f32, "{v} -> {back}");
        }
    }

    #[test]
    fn from_f32_then_to_f32_preserves_signal_shape() {
        let input: Vec<f32> = (0..64).map(|i| (i as f32 / 64.0) * 2.0 - 1.0).collect();
        let p = Packet::from_f32(1, 0, 48_000, 1, 0, &input);
        for (a, b) in input.iter().zip(p.to_f32().expect("pcm")) {
            assert!((a - b).abs() < 1.0 / i16::MAX as f32);
        }
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(Packet::decode(&[MAGIC, VERSION]), Err(DecodeError::TooShort));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample_packet().encode();
        bytes[0] = 0x00;
        assert_eq!(Packet::decode(&bytes), Err(DecodeError::BadMagic));
    }

    #[test]
    fn rejects_unknown_version() {
        let mut bytes = sample_packet().encode();
        bytes[1] = 99;
        assert_eq!(Packet::decode(&bytes), Err(DecodeError::UnknownVersion(99)));
    }

    #[test]
    fn rejects_truncated_payload_instead_of_half_filling() {
        let mut bytes = sample_packet().encode();
        bytes.truncate(bytes.len() - 4); // drop two samples
        match Packet::decode(&bytes) {
            Err(DecodeError::Truncated { expected, actual }) => {
                assert_eq!(expected, 5);
                assert_eq!(actual, 3);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn rejects_zero_channels_and_zero_rate() {
        let zero_ch = Packet { channels: 0, ..sample_packet() };
        assert_eq!(Packet::decode(&zero_ch.encode()), Err(DecodeError::InvalidFormat));
        let zero_rate = Packet { sample_rate: 0, ..sample_packet() };
        assert_eq!(Packet::decode(&zero_rate.encode()), Err(DecodeError::InvalidFormat));
    }

    #[test]
    fn frame_count_and_duration_account_for_channels() {
        let stereo = Packet {
            channels: 2,
            sample_rate: 48_000,
            sample_count: 480,
            payload: pcm(&[0; 480]),
            ..sample_packet()
        };
        assert_eq!(stereo.frame_count(), 240);
        assert!((stereo.duration_secs() - 0.005).abs() < 1e-9);
    }

    #[test]
    fn an_opus_packet_roundtrips_and_keeps_what_it_decodes_to() {
        // 60-odd bytes standing in for a real frame. The point is that the
        // payload length says nothing about the audio length, so the header
        // has to carry it.
        let frame: Vec<u8> = (0..57).map(|i| i as u8).collect();
        let p = Packet::opus(7, 42, 48_000, 1, 1_000, 960, &frame);
        let decoded = Packet::decode(&p.encode()).expect("decodes");
        assert_eq!(decoded, p);
        assert_eq!(decoded.encoding, Encoding::Opus);
        assert_eq!(decoded.payload.len(), 57, "57 bytes on the wire");
        assert_eq!(decoded.sample_count, 960, "which is 20 ms of audio");
        assert_eq!(decoded.frame_count(), 960);
        assert!((decoded.duration_secs() - 0.020).abs() < 1e-9);
    }

    #[test]
    fn an_encoded_payload_is_not_length_checked_against_the_audio() {
        // The PCM rule — payload must hold `sample_count` samples — would
        // reject every Opus packet ever sent, since compression is the point.
        let p = Packet::opus(1, 0, 48_000, 1, 0, 960, &[0xAA; 8]);
        assert!(Packet::decode(&p.encode()).is_ok());
    }

    #[test]
    fn samples_are_only_offered_for_pcm() {
        let opus = Packet::opus(1, 0, 48_000, 1, 0, 960, &[0xAA; 8]);
        assert!(opus.samples().is_none(), "there is no i16 to hand back");
        assert!(opus.to_f32().is_none());
        assert!(sample_packet().samples().is_some());
    }

    #[test]
    fn rejects_an_encoding_this_build_does_not_know() {
        // Forwarding it would hand a receiver bytes it cannot read. Better to
        // drop at the relay, where the log says which byte.
        let mut bytes = sample_packet().encode();
        bytes[28] = 200;
        assert_eq!(Packet::decode(&bytes), Err(DecodeError::UnknownEncoding(200)));
    }

    #[test]
    fn version_1_is_rejected_rather_than_guessed_at() {
        // A v1 packet has a 28-byte header, so reading it as v2 would take four
        // payload bytes as `encoding` + reserved and silently shift the audio.
        let mut bytes = sample_packet().encode();
        bytes[1] = 1;
        assert_eq!(Packet::decode(&bytes), Err(DecodeError::UnknownVersion(1)));
    }

    #[test]
    fn peak_finds_largest_absolute_value() {
        assert_eq!(peak(&[0.0, -0.8, 0.3]), 0.8);
        assert_eq!(peak(&[]), 0.0);
    }

    #[test]
    fn participant_ids_are_stable_and_never_zero() {
        let mut id = [0u8; 16];
        id[8..12].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(participant_id(&id), 0xDEADBEEF);
        assert_eq!(participant_id(&id), participant_id(&id.clone()));
        assert_eq!(participant_id(&[0u8; 16]), 1);
    }

    /// The front of a UUIDv7 is a timestamp, so ids minted in the same
    /// millisecond agree there. If the derivation read those bytes, two people
    /// who signed up together would share a sender number and the relay would
    /// treat one as the other's echo.
    #[test]
    fn ids_minted_together_still_differ() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        for i in 0..8 {
            a[i] = i as u8;
            b[i] = i as u8;
        }
        a[8..12].copy_from_slice(&[1, 2, 3, 4]);
        b[8..12].copy_from_slice(&[5, 6, 7, 8]);
        assert_ne!(participant_id(&a), participant_id(&b));
    }
}
