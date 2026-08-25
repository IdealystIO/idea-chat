//! Turning elapsed real time into a number of frames to play.
//!
//! [`Buffer::pull`](crate::Buffer::pull) produces exactly one frame and is
//! deliberately silent about *when* to call it: playout is the consumer's
//! clock, and the buffer has no business owning it. This is the small piece of
//! arithmetic that answers "how many now", for a consumer that has a clock to
//! read but nothing to schedule against.
//!
//! That is not a hypothetical shape. In a browser there is no sleep worth
//! having — `requestAnimationFrame` and `setInterval` are both throttled to
//! roughly once a second in a background tab, which is exactly when a call has
//! to keep working — so the practical arrangement is to pull on *arrival* and
//! use the clock to decide how much. The arrival is the trigger; this is the
//! pacer.
//!
//! # The cap is the whole design
//!
//! Real time keeps running while a tab is backgrounded or a machine is asleep.
//! Without a ceiling, the first packet after a resume would ask for every frame
//! the clock says is owed — minutes of it, all stale. So [`Clock::due`] never
//! answers more than `catchup`, and when it would have, it gives up on the
//! backlog and resynchronises to now. Falling behind quietly and permanently is
//! the failure this exists to prevent: a clock that stayed behind would return
//! the cap on every single call, forever.

/// A playout clock: elapsed time in, frames out.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    frame_ms: f64,
    catchup: usize,
    /// The moment the audio handed over so far will have finished playing.
    played_to_ms: f64,
}

impl Clock {
    /// `frame_ms` is how long one frame lasts — [`Buffer::frame_ms`](crate::Buffer::frame_ms).
    ///
    /// `catchup` is the most frames one call may answer. Ten frames at 20 ms is
    /// 200 ms, which is about as much stale audio as is worth playing to catch
    /// up at all.
    ///
    /// `now_ms` starts the clock, so the first [`due`](Clock::due) at that
    /// instant answers zero rather than treating the epoch as a backlog.
    pub fn new(frame_ms: f32, catchup: usize, now_ms: f64) -> Clock {
        Clock {
            // A non-positive frame length would divide by zero or run backwards.
            // Nothing sensible can be paced against it, so the clock answers
            // zero forever and the caller's stats show a stream that never
            // played rather than one that played nonsense.
            frame_ms: if frame_ms > 0.0 { frame_ms as f64 } else { 0.0 },
            catchup,
            played_to_ms: now_ms,
        }
    }

    /// How many frames are due, and mark them as played.
    ///
    /// Only whole frames: the remainder stays on the clock and shows up in a
    /// later call, which is what keeps an arrival pattern that does not divide
    /// evenly into frames from slowly losing audio.
    pub fn due(&mut self, now_ms: f64) -> usize {
        if self.frame_ms <= 0.0 {
            return 0;
        }
        let elapsed = now_ms - self.played_to_ms;
        if elapsed < self.frame_ms {
            // Includes a clock that went backwards, which `Date::now` can do
            // across an NTP step. Nothing is owed, and nothing is rewound.
            return 0;
        }
        let due = (elapsed / self.frame_ms).floor() as usize;
        if due > self.catchup {
            self.played_to_ms = now_ms;
            return self.catchup;
        }
        self.played_to_ms += due as f64 * self.frame_ms;
        due
    }
}

#[cfg(test)]
mod tests {
    use super::Clock;

    const FRAME_MS: f32 = 20.0;
    const CATCHUP: usize = 10;

    #[test]
    fn nothing_is_due_the_instant_the_clock_starts() {
        let mut clock = Clock::new(FRAME_MS, CATCHUP, 1000.0);
        assert_eq!(clock.due(1000.0), 0);
        assert_eq!(clock.due(1019.9), 0, "not until a whole frame has passed");
    }

    #[test]
    fn one_frame_of_elapsed_time_is_one_frame() {
        let mut clock = Clock::new(FRAME_MS, CATCHUP, 0.0);
        assert_eq!(clock.due(20.0), 1);
        assert_eq!(clock.due(40.0), 1);
        assert_eq!(clock.due(100.0), 3);
    }

    #[test]
    fn the_remainder_is_kept_rather_than_lost() {
        // Packets do not arrive on frame boundaries. Rounding the leftover away
        // each time would drop a frame every few packets — audible as a click
        // on a link with nothing wrong with it.
        let mut clock = Clock::new(FRAME_MS, CATCHUP, 0.0);
        let mut played = 0;
        // Arrivals every 20 ms, offset so none lands on a boundary.
        for i in 1..=50 {
            played += clock.due(i as f64 * 20.0 + 7.0);
        }
        assert_eq!(played, 50, "a second of arrivals is a second of audio");
    }

    #[test]
    fn a_backlog_is_capped_and_the_clock_catches_up_with_itself() {
        // A backgrounded tab. Ten minutes of owed audio is not worth playing,
        // and the important half is the second assertion: a clock that capped
        // without resynchronising would report the cap on every call forever,
        // permanently a burst behind and never saying so.
        let mut clock = Clock::new(FRAME_MS, CATCHUP, 0.0);
        assert_eq!(clock.due(600_000.0), CATCHUP);
        assert_eq!(clock.due(600_000.0), 0, "the backlog was given up, not carried");
        assert_eq!(clock.due(600_040.0), 2, "and it is measuring from now");
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_rewind() {
        // `Date::now` steps backwards across an NTP correction, and it is the
        // fallback when `performance` is unavailable.
        let mut clock = Clock::new(FRAME_MS, CATCHUP, 1000.0);
        assert_eq!(clock.due(500.0), 0);
        assert_eq!(clock.due(1040.0), 2, "and it resumes from where it was");
    }

    #[test]
    fn a_frame_length_that_makes_no_sense_plays_nothing() {
        let mut clock = Clock::new(0.0, CATCHUP, 0.0);
        assert_eq!(clock.due(10_000.0), 0);
    }
}
