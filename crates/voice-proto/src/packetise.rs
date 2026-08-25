//! Re-cutting capture blocks into fixed-size ones for the wire.
//!
//! The microphone hands us whatever block size the platform chose — on web
//! that is a `ScriptProcessorNode` at 4096 frames, which is 85.3 ms at 48 kHz.
//! One packet per block is fine over a stream, and impossible over datagrams:
//! 85 ms of stereo i16 is 8160 bytes and a QUIC datagram tops out near 1200.
//! There is no tuning that fixes that — the block has to be cut up.
//!
//! 20 ms is the size to cut to. It is what Opus is built around, what every
//! jitter buffer assumes, and what makes a single loss cost 20 ms of speech
//! instead of 85.
//!
//! This does **not** reduce capture latency. The samples still arrive 85 ms at
//! a time; they simply leave in smaller pieces. Making the *capture* block
//! smaller is a framework change — see finding 10c.
//!
//! ```no_run
//! # use voice_proto::Packetiser;
//! let mut p = Packetiser::new(20, 48_000, 1);
//! p.push(&[0.0; 4096], 0);
//! p.drain(|samples, pts_micros| {
//!     assert_eq!(samples.len(), 960);
//!     let _ = pts_micros;
//! });
//! ```

/// How far a block's own timestamp may sit from where the running sample count
/// says it should, before the samples still in hand are treated as belonging to
/// a call that has since moved on.
///
/// Only large gaps matter. Web capture timestamps are `Date.now()`-rebased and
/// not monotonic (see the module docs on [`crate::Packet`]), so a few ms of
/// disagreement is normal and must not be read as a dropout. A real one is at
/// least a whole block — 85 ms — so the threshold sits well above the noise and
/// well below the event.
const MAX_SKEW_MICROS: i64 = 200_000;

/// Accumulates capture blocks and hands back fixed-size frames.
///
/// Sample rate and channel count are fixed at construction: this cuts a stream
/// up, it does not resample it. A block arriving in a different format is
/// [`push`](Packetiser::push)ed at the caller's peril — build a new one instead.
#[derive(Debug)]
pub struct Packetiser {
    /// Samples (not frames) per emitted packet — `frames * channels`.
    frame: usize,
    rate: u32,
    channels: u16,
    /// Samples held back from previous blocks, plus whatever was just pushed.
    buf: Vec<f32>,
    /// Capture timestamp of `buf[0]`, carried forward as the buffer drains.
    pts_micros: u64,
    /// False until the first push, so the first block sets the timeline rather
    /// than being measured against a zero that means nothing.
    started: bool,
}

impl Packetiser {
    /// A packetiser emitting `frame_ms` at a time.
    ///
    /// `frame_ms` that does not divide evenly into `rate` is rounded down; at
    /// 48 kHz every size anyone uses (10, 20, 40, 60) is exact.
    pub fn new(frame_ms: u32, rate: u32, channels: u16) -> Packetiser {
        let frames = (rate as usize * frame_ms as usize) / 1000;
        Packetiser {
            frame: frames.max(1) * channels.max(1) as usize,
            rate: rate.max(1),
            channels: channels.max(1),
            buf: Vec::new(),
            pts_micros: 0,
            started: false,
        }
    }

    /// Samples in one emitted packet — `frames * channels`.
    pub fn frame(&self) -> usize {
        self.frame
    }

    /// Sample rate the emitted packets carry.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Channel count the emitted packets carry.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Samples held back, waiting for enough to fill a frame.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Take one capture block.
    ///
    /// `pts_micros` is the block's own capture timestamp. It is used to notice
    /// a gap: if it disagrees with the running sample count by more than
    /// [`MAX_SKEW_MICROS`], the samples still in hand are from before the gap
    /// and are dropped rather than spliced onto what came after.
    pub fn push(&mut self, samples: &[f32], pts_micros: u64) {
        if !self.started {
            self.started = true;
            self.pts_micros = pts_micros;
        } else {
            let expected = self.pts_micros + self.micros(self.buf.len());
            // Signed, because a web capture clock is `Date.now()`-rebased and
            // can step backwards as well as forwards.
            if (pts_micros as i64 - expected as i64).abs() > MAX_SKEW_MICROS {
                self.buf.clear();
                self.pts_micros = pts_micros;
            }
        }
        self.buf.extend_from_slice(samples);
    }

    /// Emit every whole frame now available, oldest first.
    ///
    /// `emit` is handed a borrowed slice rather than a `Vec` so the hot path
    /// allocates nothing: at 20 ms this runs fifty times a second for the life
    /// of a call.
    pub fn drain(&mut self, mut emit: impl FnMut(&[f32], u64)) {
        while self.buf.len() >= self.frame {
            emit(&self.buf[..self.frame], self.pts_micros);
            self.pts_micros += self.micros(self.frame);
            self.buf.drain(..self.frame);
        }
    }

    /// Emit whatever is left, padded with silence to a whole frame.
    ///
    /// For the end of a call, where a partial frame is better than a truncated
    /// word. Not for use mid-stream: padding inserts silence the microphone
    /// never heard, which is a click.
    pub fn flush(&mut self, mut emit: impl FnMut(&[f32], u64)) {
        if self.buf.is_empty() {
            return;
        }
        let whole = self.buf.len().div_ceil(self.frame) * self.frame;
        self.buf.resize(whole, 0.0);
        // `&mut F` is itself `FnMut`, so the caller's closure is reborrowed
        // rather than moved — `drain` leaves the buffer exactly empty.
        self.drain(&mut emit);
    }

    /// Duration of `samples` interleaved samples, in microseconds.
    fn micros(&self, samples: usize) -> u64 {
        let frames = samples / self.channels as usize;
        (frames as u64 * 1_000_000) / self.rate as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect every frame a packetiser emits for one block.
    fn take(p: &mut Packetiser, samples: &[f32], pts: u64) -> Vec<(Vec<f32>, u64)> {
        p.push(samples, pts);
        let mut out = Vec::new();
        p.drain(|s, pts| out.push((s.to_vec(), pts)));
        out
    }

    #[test]
    fn a_capture_block_becomes_whole_frames_and_a_remainder() {
        let mut p = Packetiser::new(20, 48_000, 1);
        assert_eq!(p.frame(), 960);

        let out = take(&mut p, &vec![0.5; 4096], 0);
        // 4096 = 4 * 960 + 256.
        assert_eq!(out.len(), 4);
        assert_eq!(p.pending(), 256, "the remainder waits for the next block");
    }

    #[test]
    fn a_frame_spanning_two_blocks_is_not_lost() {
        let mut p = Packetiser::new(20, 48_000, 1);
        // Ramp so every sample is distinguishable.
        let first: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let second: Vec<f32> = (1000..2000).map(|i| i as f32).collect();

        let mut out = Vec::new();
        p.push(&first, 0);
        p.drain(|s, _| out.push(s.to_vec()));
        p.push(&second, 20_833);
        p.drain(|s, _| out.push(s.to_vec()));

        assert_eq!(out.len(), 2);
        let joined: Vec<f32> = out.concat();
        let expected: Vec<f32> = (0..1920).map(|i| i as f32).collect();
        assert_eq!(joined, expected, "samples come out in order, none dropped");
    }

    #[test]
    fn timestamps_advance_by_one_frame() {
        let mut p = Packetiser::new(20, 48_000, 1);
        let out = take(&mut p, &vec![0.0; 4096], 1_000_000);
        let stamps: Vec<u64> = out.iter().map(|(_, pts)| *pts).collect();
        assert_eq!(stamps, vec![1_000_000, 1_020_000, 1_040_000, 1_060_000]);
    }

    #[test]
    fn stereo_counts_frames_not_samples() {
        let mut p = Packetiser::new(20, 48_000, 2);
        assert_eq!(p.frame(), 1920, "960 frames of two channels");
        let out = take(&mut p, &vec![0.0; 1920 * 3], 0);
        assert_eq!(out.len(), 3);
        let stamps: Vec<u64> = out.iter().map(|(_, pts)| *pts).collect();
        assert_eq!(stamps, vec![0, 20_000, 40_000], "20 ms, not 10");
    }

    #[test]
    fn a_gap_drops_the_stale_remainder_instead_of_splicing_it() {
        let mut p = Packetiser::new(20, 48_000, 1);
        take(&mut p, &vec![1.0; 1000], 0);
        assert_eq!(p.pending(), 40);

        // Next block claims to be a full second later: the call was suspended.
        let out = take(&mut p, &vec![2.0; 960], 1_000_000);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].0.iter().all(|&v| v == 2.0),
            "the 40 samples from before the gap must not lead this frame"
        );
        assert_eq!(out[0].1, 1_000_000, "and the timeline restarts at the block");
    }

    #[test]
    fn ordinary_timestamp_jitter_is_not_a_gap() {
        let mut p = Packetiser::new(20, 48_000, 1);
        take(&mut p, &vec![1.0; 1000], 0);
        // Web capture stamps wobble by a few ms. 40 samples must survive that.
        let out = take(&mut p, &vec![2.0; 920], 25_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0[0], 1.0, "the remainder still leads the frame");
    }

    #[test]
    fn flush_pads_the_last_partial_frame() {
        let mut p = Packetiser::new(20, 48_000, 1);
        take(&mut p, &vec![1.0; 100], 0);
        let mut out = Vec::new();
        p.flush(|s, _| out.push(s.to_vec()));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 960);
        assert_eq!(out[0][99], 1.0);
        assert_eq!(out[0][100], 0.0, "padded with silence, not with garbage");
        assert_eq!(p.pending(), 0);
    }

    #[test]
    fn flush_with_nothing_held_emits_nothing() {
        let mut p = Packetiser::new(20, 48_000, 1);
        take(&mut p, &vec![1.0; 960], 0);
        let mut count = 0;
        p.flush(|_, _| count += 1);
        assert_eq!(count, 0, "an exactly-drained buffer has nothing to pad");
    }

    #[test]
    fn ten_ms_is_half_the_frame() {
        let p = Packetiser::new(10, 48_000, 1);
        assert_eq!(p.frame(), 480);
    }

    #[test]
    fn raw_pcm_does_not_fit_a_datagram_and_opus_does() {
        // Why the codec is not optional on the QUIC path, in one assertion.
        // 20 ms of mono i16 at 48 kHz is 1920 bytes of payload; a QUIC datagram
        // tops out around 1200. Cutting finer does not save it either — the
        // header is per packet, so 10 ms halves the payload and doubles the
        // overhead.
        let p = Packetiser::new(20, 48_000, 1);
        let raw = p.frame() * 2 + crate::HEADER_LEN;
        assert_eq!(raw, 1952);
        assert!(raw > 1200, "raw PCM does not fit");

        // The same 20 ms as Opus at 24 kbps is 60 bytes, so the packet is
        // dominated by its own header and fits many times over.
        let opus = 60 + crate::HEADER_LEN;
        assert_eq!(opus, 92);
        assert!(opus < 1200);
    }
}
