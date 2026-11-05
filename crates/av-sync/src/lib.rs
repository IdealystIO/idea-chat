//! Lip sync for a call, as arithmetic on timestamps.
//!
//! Audio is the clock. Every audio packet and every video frame carries its
//! capture time on the sender's clock (ms). The receiver works out what
//! capture time is coming out of its speakers right now — an [`AudioClock`] —
//! and holds each decoded video frame until the audio reaches it. Without
//! that, video plays the moment it decodes while audio sits behind the jitter
//! buffer, the output cushion and the sender's processing latency, so the
//! picture runs ahead of the voice by an amount that moves as the jitter
//! buffer adapts.
//!
//! Three parts, in the order a sound travels:
//!
//! - **Sender:** [`CaptureClock`] says when any sample of the mic stream was
//!   really captured, so a packet's capture time is the moment the sound
//!   happened however long the processing took.
//! - **Receiver, audio:** [`audible`] turns the newest packet's capture time
//!   into an [`AudioClock`]; [`Clocks`] keeps one per (sender, audio track)
//!   and says which one each video track follows.
//! - **Receiver, video:** [`due`] says which queued frame to show now.
//!
//! Holding only ever delays video. A frame that is already late is shown at
//! once, and so is everything when the audio clock goes stale (nobody is
//! sending audio) or a frame has waited the longest hold — a frozen picture is
//! worse than an unsynced one.

use std::collections::{HashMap, VecDeque};

/// The track number whose audio a video track falls back to — the
/// microphone. Matches `call_proto::TRACK_MAIN`, which this crate does not
/// depend on.
pub const MAIN_TRACK: u8 = 0;

/// An audio clock older than this is no clock: whoever it belonged to has
/// stopped sending audio, and video shows as it comes.
pub const STALE_CLOCK_MS: f64 = 1000.0;

/// When each sample of a capture stream was captured, by its index in the
/// stream: one mark per block delivered. Indices count every block across
/// reopened captures, so a processing stage that started at index `n` can say
/// when any sample it emits was really captured.
#[derive(Debug, Default)]
pub struct CaptureClock {
    marks: VecDeque<(u64, f64)>,
    next: u64,
}

impl CaptureClock {
    /// A block of `frames` at `rate` delivered at `now` (ms): its first
    /// sample was captured a block's length earlier.
    pub fn mark(&mut self, frames: u64, rate: u32, now: f64) {
        self.marks.push_back((self.next, now - frames as f64 * 1000.0 / rate.max(1) as f64));
        self.next += frames;
        // A few seconds is all anything looks back.
        while self.marks.len() > 1024 {
            self.marks.pop_front();
        }
    }

    /// The index the next block will start at — where a stage opened now
    /// starts counting.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// When sample `index` was captured, in ms, at `rate`. Before the first
    /// mark (a stage reaching back past the start) is extrapolated from it.
    pub fn at(&self, index: i64, rate: u32) -> Option<f64> {
        let i = self.marks.partition_point(|&(k, _)| (k as i64) <= index);
        let &(k, t) = self.marks.get(i.saturating_sub(1))?;
        Some(t + (index - k as i64) as f64 * 1000.0 / rate.max(1) as f64)
    }
}

/// What capture time is coming out of the speakers, given that the packet
/// just received was captured at `capture` and lasts `frame_ms`: its end,
/// less everything still between it and the ear — what the jitter buffer
/// holds, what the output has queued, and the device's own latency (on
/// Bluetooth the biggest of the three). All in ms.
pub fn audible(capture: f64, frame_ms: f64, buffered_ms: f64, queued_ms: f64, device_ms: f64) -> f64 {
    capture + frame_ms - buffered_ms - queued_ms - device_ms
}

/// Capture time `capture` (sender's clock) was audible at `at` (ours).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioClock {
    pub capture: f64,
    pub at: f64,
}

impl AudioClock {
    /// The capture time audible at `now`, assuming playback runs in real time
    /// since the clock was set.
    pub fn audible_at(&self, now: f64) -> f64 {
        self.capture + (now - self.at)
    }

    pub fn fresh(&self, now: f64) -> bool {
        now - self.at < STALE_CLOCK_MS
    }
}

/// The audio clocks of everyone we hear, per (sender, audio track).
#[derive(Debug, Default)]
pub struct Clocks(HashMap<(u32, u8), AudioClock>);

impl Clocks {
    pub fn set(&mut self, sender: u32, audio_track: u8, clock: AudioClock) {
        self.0.insert((sender, audio_track), clock);
    }

    /// The clock video track `track` of `sender` follows: the audio track of
    /// its own number, or the microphone's when it has none — a screen share
    /// without sound is held to the voice of whoever is talking over it.
    pub fn for_video(&self, sender: u32, track: u8) -> Option<AudioClock> {
        self.0.get(&(sender, track)).or_else(|| self.0.get(&(sender, MAIN_TRACK))).copied()
    }

    /// Someone hung up.
    pub fn forget(&mut self, sender: u32) {
        self.0.retain(|&(s, _), _| s != sender);
    }
}

/// A decoded frame waiting to be shown: when it was captured (sender's
/// clock) and when it came out of the decoder (ours), in ms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Queued {
    pub capture: f64,
    pub decoded: f64,
}

/// How many frames off the front of `queue` (oldest first) are due at `now`.
/// The last of them is the one to show; any before it are already passed
/// over. 0 means show nothing new yet.
///
/// A frame is due once the audio has reached its capture time — or at once
/// when there is no fresh clock to wait for, or it has waited `max_hold`.
pub fn due(queue: &[Queued], clock: Option<AudioClock>, now: f64, max_hold: f64) -> usize {
    let audible = clock.filter(|c| c.fresh(now)).map(|c| c.audible_at(now));
    queue
        .iter()
        .take_while(|f| match audible {
            Some(heard) => f.capture <= heard || now - f.decoded >= max_hold,
            None => true,
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_was_captured_where_its_block_says() {
        let mut c = CaptureClock::default();
        // 480 frames at 48 kHz is 10 ms; delivered at 110 → started at 100.
        c.mark(480, 48_000, 110.0);
        c.mark(480, 48_000, 120.0);
        assert_eq!(c.at(0, 48_000), Some(100.0));
        assert_eq!(c.at(480 + 240, 48_000), Some(115.0));
        // Reaching back before the first mark extrapolates.
        assert_eq!(c.at(-480, 48_000), Some(90.0));
        assert_eq!(CaptureClock::default().at(0, 48_000), None);
    }

    #[test]
    fn what_is_heard_is_behind_what_arrived_by_everything_in_between() {
        // A 20 ms packet captured at 1000, behind 60 ms of jitter buffer, 40
        // of output queue and 30 of device: its end is heard 130 ms from now,
        // so what is audible now was captured at 1020 - 130.
        assert_eq!(audible(1000.0, 20.0, 60.0, 40.0, 30.0), 890.0);
    }

    #[test]
    fn a_clock_runs_on_and_goes_stale() {
        let c = AudioClock { capture: 500.0, at: 10_000.0 };
        assert_eq!(c.audible_at(10_250.0), 750.0);
        assert!(c.fresh(10_999.0));
        assert!(!c.fresh(11_000.0));
    }

    #[test]
    fn video_follows_its_own_audio_or_else_the_mic() {
        let mut clocks = Clocks::default();
        let mic = AudioClock { capture: 1.0, at: 2.0 };
        let screen = AudioClock { capture: 3.0, at: 4.0 };
        clocks.set(7, 0, mic);
        assert_eq!(clocks.for_video(7, 1), Some(mic), "a silent share follows the voice");
        clocks.set(7, 1, screen);
        assert_eq!(clocks.for_video(7, 1), Some(screen));
        assert_eq!(clocks.for_video(7, 0), Some(mic));
        assert_eq!(clocks.for_video(8, 0), None);
        clocks.forget(7);
        assert_eq!(clocks.for_video(7, 0), None);
    }

    fn q(capture: f64, decoded: f64) -> Queued {
        Queued { capture, decoded }
    }

    #[test]
    fn frames_wait_for_the_audio() {
        let clock = Some(AudioClock { capture: 100.0, at: 1000.0 });
        let queue = [q(90.0, 990.0), q(130.0, 995.0), q(170.0, 998.0)];
        // At 1000 the audio is at 100: the first frame is due, not the rest.
        assert_eq!(due(&queue, clock, 1000.0, 600.0), 1);
        // 40 ms on it has reached 140: the second too, so the first is skipped.
        assert_eq!(due(&queue, clock, 1040.0, 600.0), 2);
    }

    #[test]
    fn nothing_waits_forever() {
        let clock = Some(AudioClock { capture: 0.0, at: 1000.0 });
        let queue = [q(5000.0, 400.0), q(6000.0, 999.0)];
        // The first has waited 600 ms: shown although the audio is far behind.
        assert_eq!(due(&queue, clock, 1000.0, 600.0), 1);
    }

    #[test]
    fn without_a_live_clock_frames_show_as_they_come() {
        let queue = [q(5000.0, 999.0), q(6000.0, 999.0)];
        assert_eq!(due(&queue, None, 1000.0, 600.0), 2);
        let stale = Some(AudioClock { capture: 0.0, at: 0.0 });
        assert_eq!(due(&queue, stale, 1000.0, 600.0), 2);
        assert_eq!(due(&[], None, 1000.0, 600.0), 0);
    }
}
