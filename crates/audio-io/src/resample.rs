//! Linear resampling, for the one case that actually turns up.
//!
//! An output device runs at one rate. A room does not: a caller whose hardware
//! reports 44.1 kHz sends 44.1 kHz, and everyone else is at 48. Something has to
//! reconcile them, and doing it here means a mixed-rate room sounds right rather
//! than sounding fast.
//!
//! Linear, not sinc. The interpolation error is well above the band anything in
//! a voice call cares about, it costs two multiplies a sample, and it runs on a
//! path that must not allocate. A better resampler belongs in whatever does
//! clock-drift correction, which is a different job with a different budget.

/// Resample interleaved PCM from `from` Hz to `to` Hz, appending to `out`.
///
/// `tail` carries the last frame of the previous call, so consecutive blocks
/// interpolate across their boundary instead of restarting from silence — a
/// discontinuity every block is a buzz at the block rate, which is the classic
/// way this goes wrong and is perfectly audible.
pub fn linear(
    input: &[f32],
    channels: u16,
    from: u32,
    to: u32,
    tail: &mut Vec<f32>,
    position: &mut f64,
    out: &mut Vec<f32>,
) {
    let channels = channels.max(1) as usize;
    if input.is_empty() {
        return;
    }
    if from == to || from == 0 || to == 0 {
        out.extend_from_slice(input);
        return;
    }

    let step = from as f64 / to as f64;
    let frames = input.len() / channels;
    if tail.len() != channels {
        // First block: start on the first frame rather than interpolating from
        // silence, which would be a click at the start of every stream.
        tail.clear();
        tail.extend_from_slice(&input[..channels]);
        *position = 0.0;
    }

    // `position` is in frames, relative to the START of `input`, and may begin
    // negative — that is the fractional part left over from the previous block,
    // interpolating between its last frame (`tail`) and this block's first.
    let mut at = *position;
    while at < frames as f64 {
        let index = at.floor();
        let frac = (at - index) as f32;
        let i = index as isize;
        for c in 0..channels {
            let a = if i < 0 { tail[c] } else { input[i as usize * channels + c] };
            let next = i + 1;
            let b = if next < 0 {
                tail[c]
            } else if (next as usize) < frames {
                input[next as usize * channels + c]
            } else {
                // Past the end: hold the last sample. The next block's `tail`
                // will pick the interpolation back up.
                input[(frames - 1) * channels + c]
            };
            out.push(a + (b - a) * frac);
        }
        at += step;
    }

    tail.clear();
    tail.extend_from_slice(&input[(frames - 1) * channels..]);
    *position = at - frames as f64;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[f32]], channels: u16, from: u32, to: u32) -> Vec<f32> {
        let mut tail = Vec::new();
        let mut position = 0.0;
        let mut out = Vec::new();
        for chunk in chunks {
            linear(chunk, channels, from, to, &mut tail, &mut position, &mut out);
        }
        out
    }

    #[test]
    fn the_same_rate_is_a_straight_copy() {
        // Not merely "close": resampling at 1:1 must be bit-exact, or every
        // call in the common case is paying for nothing and losing precision.
        let input = [0.1f32, -0.2, 0.3, -0.4];
        assert_eq!(run(&[&input], 1, 48_000, 48_000), input.to_vec());
    }

    #[test]
    fn halving_the_rate_halves_the_samples() {
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = run(&[&input], 1, 48_000, 24_000);
        assert!((out.len() as i32 - 50).abs() <= 1, "got {}", out.len());
    }

    #[test]
    fn a_common_upsample_lands_at_the_right_length() {
        // 44.1 kHz to 48 kHz, which is the case this exists for.
        let input: Vec<f32> = (0..441).map(|i| (i as f32 / 441.0).sin()).collect();
        let out = run(&[&input], 1, 44_100, 48_000);
        assert!((out.len() as i32 - 480).abs() <= 1, "got {}", out.len());
    }

    #[test]
    fn a_ramp_stays_a_ramp() {
        // Linear interpolation of a linear signal is exact, so this is the one
        // case where the output can be checked against the truth rather than
        // against a tolerance.
        let input: Vec<f32> = (0..48).map(|i| i as f32).collect();
        let out = run(&[&input], 1, 48_000, 96_000);
        for pair in out.windows(2) {
            let d = pair[1] - pair[0];
            assert!(d >= -0.001 && d <= 0.51, "a ramp got a kink: {pair:?}");
        }
    }

    #[test]
    fn consecutive_blocks_join_without_a_discontinuity() {
        // The bug this signature exists to prevent. Resampling each block
        // independently restarts the interpolation at every boundary, which is
        // a buzz at the block rate — perfectly audible, and invisible if you
        // only ever test one block.
        let a: Vec<f32> = (0..64).map(|i| i as f32).collect();
        let b: Vec<f32> = (64..128).map(|i| i as f32).collect();
        let joined = run(&[&a, &b], 1, 48_000, 32_000);

        let mut worst = 0.0f32;
        for pair in joined.windows(2) {
            worst = worst.max((pair[1] - pair[0]).abs());
        }
        // A continuous ramp at 2:3 steps by 1.5 a sample. A restart at the
        // boundary would show up as a step of dozens.
        assert!(worst < 2.0, "discontinuity of {worst} across a block boundary");
    }

    #[test]
    fn stereo_channels_do_not_bleed_into_each_other() {
        // Interleaved, so an off-by-one in the indexing swaps the channels —
        // which sounds fine on a mono fixture and is why this one is stereo
        // with a silent right.
        let input: Vec<f32> = (0..64).flat_map(|i| [i as f32, 0.0]).collect();
        let out = run(&[&input], 2, 48_000, 24_000);
        for frame in out.chunks_exact(2) {
            assert_eq!(frame[1], 0.0, "the right channel picked up the left");
        }
    }

    #[test]
    fn an_empty_block_produces_nothing_and_breaks_nothing() {
        let mut tail = Vec::new();
        let mut position = 0.0;
        let mut out = Vec::new();
        linear(&[], 1, 44_100, 48_000, &mut tail, &mut position, &mut out);
        assert!(out.is_empty());
    }
}
