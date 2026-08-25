//! Wire format for one chunk of voice audio.
//!
//! Deliberately dependency-free and framework-free: this is the piece most
//! likely to be promoted into an SDK later, so it must not drag `runtime-core`
//! or `media-stream` along with it.
//!
//! # Why raw PCM (for now)
//!
//! Step 1 ships **uncompressed i16**. Mono at 16 kHz is ~256 kbps — genuinely
//! usable, not just a toy — and at a device-native 48 kHz it is ~768 kbps,
//! which is fine on wifi for a two-party test. Shipping raw first means the
//! whole capture -> transport -> playback loop can be proven before any codec
//! decision is made.
//!
//! A codec slots in later as a pure `&[f32] -> Vec<u8> -> Vec<f32>` transform
//! between [`Packet::from_f32`] and the transport. Nothing structural changes:
//! the header already carries `sample_rate`, `channels`, `seq`, and `pts_micros`,
//! which is everything a decoder and a jitter buffer need.
//!
//! # Layout (little-endian)
//!
//! ```text
//! offset  size  field
//!      0     1  magic  (0x56 'V')
//!      1     1  version (currently 1)
//!      2     2  channels
//!      4     4  sender
//!      8     4  seq
//!     12     4  sample_rate
//!     16     8  pts_micros
//!     24     4  sample_count  (total i16s that follow = frames * channels)
//!     28   2*n  interleaved i16 samples
//! ```
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

/// First byte of every packet — cheap guard against a stray text frame or a
/// mismatched endpoint.
pub const MAGIC: u8 = 0x56;
/// Wire-format version. Bump on any layout change.
pub const VERSION: u8 = 1;
/// Bytes before the sample payload begins.
pub const HEADER_LEN: usize = 28;

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
        }
    }
}

impl std::error::Error for DecodeError {}

/// One chunk of interleaved PCM plus the metadata a receiver needs to play it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Which participant produced this chunk. Assigned by the client at join.
    pub sender: u32,
    /// Monotonically increasing per sender. Gaps mean loss; a jitter buffer
    /// uses this to reorder and to drive concealment.
    pub seq: u32,
    /// Samples per second of the payload. Carried per-packet so no resampling
    /// is needed anywhere in the step-1 path — the receiver hands this straight
    /// to `AudioWriter::write_pcm_f32`, and the platform sink resamples.
    pub sample_rate: u32,
    /// Interleaving factor of the payload.
    pub channels: u16,
    /// Capture-clock timestamp (microseconds) for the START of this chunk.
    pub pts_micros: u64,
    /// Interleaved samples. `len()` is `frames * channels`.
    pub samples: Vec<i16>,
}

impl Packet {
    /// Build a packet from normalized `f32` samples, as delivered by an
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
        Packet {
            sender,
            seq,
            sample_rate,
            channels,
            pts_micros,
            samples: samples.iter().copied().map(f32_to_i16).collect(),
        }
    }

    /// Payload back as normalized `f32`, ready for `AudioWriter::write_pcm_f32`.
    pub fn to_f32(&self) -> Vec<f32> {
        self.samples.iter().copied().map(i16_to_f32).collect()
    }

    /// Number of sample frames (samples per channel).
    pub fn frame_count(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
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
        let mut out = Vec::with_capacity(HEADER_LEN + self.samples.len() * 2);
        out.push(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.channels.to_le_bytes());
        out.extend_from_slice(&self.sender.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&self.pts_micros.to_le_bytes());
        out.extend_from_slice(&(self.samples.len() as u32).to_le_bytes());
        for s in &self.samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
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
        let count = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) as usize;

        if channels == 0 || sample_rate == 0 {
            return Err(DecodeError::InvalidFormat);
        }

        let payload = &bytes[HEADER_LEN..];
        let actual = payload.len() / 2;
        if actual < count {
            return Err(DecodeError::Truncated { expected: count, actual });
        }

        let mut samples = Vec::with_capacity(count);
        for chunk in payload[..count * 2].chunks_exact(2) {
            samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }

        Ok(Packet { sender, seq, sample_rate, channels, pts_micros, samples })
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

    fn sample_packet() -> Packet {
        Packet {
            sender: 7,
            seq: 42,
            sample_rate: 48_000,
            channels: 1,
            pts_micros: 1_234_567,
            samples: vec![0, 100, -100, i16::MAX, i16::MIN],
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
        let p = Packet { samples: vec![], ..sample_packet() };
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
        for (a, b) in input.iter().zip(p.to_f32()) {
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
        let stereo = Packet { channels: 2, sample_rate: 48_000, samples: vec![0; 480], ..sample_packet() };
        assert_eq!(stereo.frame_count(), 240);
        assert!((stereo.duration_secs() - 0.005).abs() < 1e-9);
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
