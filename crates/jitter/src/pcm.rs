//! The codec-free [`Decode`](crate::Decode): raw interleaved i16, the format
//! this project's packets carry today.
//!
//! Here so the buffer can be adopted before a codec is, and so the trait has a
//! reference implementation that is obviously correct. It is also the honest
//! demonstration of why the trait hands loss to the codec: this can only repeat
//! what it already heard (see [`Concealer`]), where a real codec resynthesises
//! from its own state.

use crate::{Concealer, Decode};

/// Decodes raw little-endian interleaved i16 and conceals by repetition.
pub struct Pcm {
    frame: usize,
    conceal: Concealer,
}

impl Pcm {
    /// `frame` is samples per packet, interleaved across channels.
    pub fn new(rate: u32, channels: u16, frame: usize) -> Pcm {
        Pcm { frame: frame.max(1), conceal: Concealer::new(rate, channels) }
    }
}

impl Decode for Pcm {
    fn frame(&self) -> usize {
        self.frame
    }

    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) {
        let Some(bytes) = packet else {
            self.conceal.fill(out);
            return;
        };
        // A short packet fills what it can and leaves the rest silent rather
        // than failing: the buffer's contract is that `out` always comes back
        // full, and half a frame of audio beats a frame of nothing.
        out.fill(0.0);
        for (sample, chunk) in out.iter_mut().zip(bytes.chunks_exact(2)) {
            *sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / i16::MAX as f32;
        }
        self.conceal.observe(out);
    }
}
