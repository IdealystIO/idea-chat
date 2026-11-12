//! Sigil's lossy realtime envelope (spec §6.3).
//!
//! ```text
//! [kind | SEALED][track][u8 n][n bytes of clear header][SFrame header][ciphertext][tag: 10]
//!  └────────────── clear, authenticated as SFrame metadata ──────────┘
//! KID = epoch (low 8 bits) << 40 | leaf index << 8 | context
//! ```
//!
//! The first two bytes are call-proto's kind and track, so a call relay can
//! route and gate on them unchanged; the rest of the clear header is the
//! service's own (for voice and video, call-proto's sequence numbers and
//! timestamps). Nothing in the clear can be changed without the tag failing.
//!
//! Keys come from the group, not from a key someone handed out: each epoch's
//! [`base`](Sealer::new) is `sigil::Group::sframe_base`, and every member of
//! the epoch computes the same one. The KID names the sender's **leaf**, so a
//! receiver knows which member sent a frame from the frame itself — the
//! relay's numbering is not trusted for that.
//!
//! What it does not do: everyone holding an epoch's base key could forge
//! frames as any leaf. That is the price of datagram speed (spec §6.3).

use std::collections::{BTreeMap, HashMap};

use call_e2ee::sframe::{self, Header, Keys};
use call_proto::SEALED;

/// How long a replaced epoch's frames are still accepted, so traffic in
/// flight across a commit still plays (spec §4.3).
pub const GRACE_MS: u64 = 10_000;
/// The most clear-header bytes after kind and track.
pub const MAX_CLEAR: usize = 255;
const REPLAY_WINDOW: u64 = 64;

fn kid(epoch: u64, leaf: u32, context: u8) -> u64 {
    ((epoch & 0xff) << 40) | ((leaf as u64) << 8) | context as u64
}

fn split_kid(kid: u64) -> (u8, u32, u8) {
    ((kid >> 40) as u8, (kid >> 8) as u32, kid as u8)
}

/// Seals one sender's frames for one service.
pub struct Sealer {
    leaf: u32,
    context: u8,
    epoch: u64,
    keys: Keys,
    ctr: u64,
}

impl Sealer {
    /// A sender at `leaf`, starting in `epoch` with that epoch's base key.
    /// `context` separates several streams one device sends (a camera and a
    /// screen, say) so their counters never share a key.
    pub fn new(epoch: u64, base: &[u8; 16], leaf: u32, context: u8) -> Sealer {
        Sealer { leaf, context, epoch, keys: Keys::derive(kid(epoch, leaf, context), base), ctr: 0 }
    }

    /// Move to a new epoch: new key, counter from zero. The group's leaf
    /// for this device may have changed too.
    pub fn rekey(&mut self, epoch: u64, base: &[u8; 16], leaf: u32) {
        *self = Sealer::new(epoch, base, leaf, self.context);
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Seal `payload` behind a clear header: `kind` and `track` as
    /// call-proto numbers them, then up to [`MAX_CLEAR`] bytes of the
    /// service's own.
    pub fn seal(&mut self, kind: u8, track: u8, clear: &[u8], payload: &[u8]) -> Option<Vec<u8>> {
        if clear.len() > MAX_CLEAR || kind & SEALED != 0 {
            return None;
        }
        let mut meta = Vec::with_capacity(3 + clear.len());
        meta.extend_from_slice(&[kind | SEALED, track, clear.len() as u8]);
        meta.extend_from_slice(clear);
        let header = Header { kid: kid(self.epoch, self.leaf, self.context), ctr: self.ctr };
        self.ctr += 1;
        let sealed = sframe::encrypt(&self.keys, header, &meta, payload);
        Some([meta, sealed].concat())
    }
}

/// An opened frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub epoch: u64,
    /// The sender's leaf in that epoch.
    pub leaf: u32,
    pub context: u8,
    pub kind: u8,
    pub track: u8,
    pub clear: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The kind byte doesn't carry the sealed flag: someone sending in the
    /// clear. Never played.
    NotSealed,
    Malformed,
    /// An epoch this receiver doesn't hold, or one past its grace.
    UnknownEpoch,
    Forged,
    Replayed,
}

struct Epoch {
    epoch: u64,
    base: [u8; 16],
    /// When it stops being accepted: set once a newer epoch arrives.
    until: Option<u64>,
}

struct Window {
    highest: u64,
    seen: u64,
}

impl Window {
    fn admit(&mut self, ctr: u64) -> bool {
        if ctr > self.highest {
            let shift = ctr - self.highest;
            self.seen = if shift >= REPLAY_WINDOW { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.highest = ctr;
            return true;
        }
        let back = self.highest - ctr;
        if back >= REPLAY_WINDOW || self.seen & (1 << back) != 0 {
            return false;
        }
        self.seen |= 1 << back;
        true
    }
}

/// Opens everyone else's frames for one service.
#[derive(Default)]
pub struct Opener {
    /// By the KID's 8 epoch bits.
    epochs: BTreeMap<u8, Epoch>,
    keys: HashMap<u64, Keys>,
    windows: HashMap<u64, Window>,
}

impl Opener {
    pub fn new() -> Opener {
        Opener::default()
    }

    /// Start accepting `epoch`, and give the epoch before it [`GRACE_MS`]
    /// from `now_ms` before it stops.
    pub fn enter(&mut self, epoch: u64, base: &[u8; 16], now_ms: u64) {
        for e in self.epochs.values_mut().filter(|e| e.until.is_none() && e.epoch < epoch) {
            e.until = Some(now_ms + GRACE_MS);
        }
        let slot = (epoch & 0xff) as u8;
        self.keys.retain(|kid, _| split_kid(*kid).0 != slot);
        self.windows.retain(|kid, _| split_kid(*kid).0 != slot);
        self.epochs.insert(slot, Epoch { epoch, base: *base, until: None });
    }

    /// Open a frame, or say why not.
    pub fn open(&mut self, datagram: &[u8], now_ms: u64) -> Result<Frame, Refusal> {
        let (&kind, rest) = datagram.split_first().ok_or(Refusal::Malformed)?;
        if kind & SEALED == 0 {
            return Err(Refusal::NotSealed);
        }
        let (&track, rest) = rest.split_first().ok_or(Refusal::Malformed)?;
        let (&n, rest) = rest.split_first().ok_or(Refusal::Malformed)?;
        if rest.len() < n as usize {
            return Err(Refusal::Malformed);
        }
        let (meta, sealed) = datagram.split_at(3 + n as usize);
        let (header, _) = Header::decode(sealed).ok_or(Refusal::Malformed)?;
        let (slot, leaf, context) = split_kid(header.kid);
        if header.kid >> 48 != 0 {
            return Err(Refusal::Malformed);
        }
        let e = self.epochs.get(&slot).ok_or(Refusal::UnknownEpoch)?;
        if e.until.is_some_and(|t| now_ms >= t) {
            return Err(Refusal::UnknownEpoch);
        }
        let epoch = e.epoch;
        let keys = self.keys.entry(header.kid).or_insert_with(|| Keys::derive(header.kid, &e.base));
        let (header, payload) = sframe::decrypt(sealed, meta, |_| Some(&*keys)).map_err(|f| match f {
            sframe::Failure::Malformed => Refusal::Malformed,
            sframe::Failure::UnknownKey => Refusal::UnknownEpoch,
            sframe::Failure::Forged => Refusal::Forged,
        })?;
        let window = self.windows.entry(header.kid).or_insert(Window { highest: 0, seen: 0 });
        if !window.admit(header.ctr) {
            return Err(Refusal::Replayed);
        }
        Ok(Frame { epoch, leaf, context, kind: kind & !SEALED, track, clear: meta[3..].to_vec(), payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use call_proto::{KIND_AUDIO, KIND_VIDEO, TRACK_MAIN, TRACK_SCREEN};

    const B1: [u8; 16] = [1; 16];
    const B2: [u8; 16] = [2; 16];

    #[test]
    fn frames_open_and_name_their_leaf() {
        let mut s = Sealer::new(3, &B1, 7, 0);
        let mut o = Opener::new();
        o.enter(3, &B1, 0);
        let d = s.seal(KIND_AUDIO, TRACK_MAIN, b"seq+ts", b"opus").unwrap();
        // A call relay reads kind and track where it always has.
        assert_eq!(call_proto::media(&d), Some(call_proto::Media::Audio));
        let f = o.open(&d, 1).unwrap();
        assert_eq!((f.epoch, f.leaf, f.kind, f.track), (3, 7, KIND_AUDIO, TRACK_MAIN));
        assert_eq!((f.clear.as_slice(), f.payload.as_slice()), (b"seq+ts".as_slice(), b"opus".as_slice()));
        let screen = Sealer::new(3, &B1, 7, 1).seal(KIND_VIDEO, TRACK_SCREEN, b"", b"px").unwrap();
        assert_eq!(call_proto::media(&screen), Some(call_proto::Media::Screen));
        assert_eq!(o.open(&screen, 1).unwrap().context, 1);
    }

    #[test]
    fn refuses_clear_forged_replayed_and_unknown_frames() {
        let mut s = Sealer::new(3, &B1, 7, 0);
        let mut o = Opener::new();
        o.enter(3, &B1, 0);
        let d = s.seal(KIND_AUDIO, TRACK_MAIN, b"h", b"opus").unwrap();
        for i in 0..d.len() {
            let mut bad = d.clone();
            bad[i] ^= 1;
            assert!(o.open(&bad, 1).is_err(), "byte {i}");
        }
        assert!(o.open(&d, 1).is_ok());
        assert_eq!(o.open(&d, 1), Err(Refusal::Replayed));
        let mut clear = d.clone();
        clear[0] &= !SEALED;
        assert_eq!(o.open(&clear, 1), Err(Refusal::NotSealed));
        let other = Sealer::new(4, &B2, 7, 0).seal(KIND_AUDIO, TRACK_MAIN, b"", b"x").unwrap();
        assert_eq!(o.open(&other, 1), Err(Refusal::UnknownEpoch));
        // Someone without this epoch's base key.
        let outsider = Sealer::new(3, &B2, 7, 0).seal(KIND_AUDIO, TRACK_MAIN, b"", b"x").unwrap();
        assert_eq!(o.open(&outsider, 1), Err(Refusal::Forged));
    }

    #[test]
    fn the_old_epoch_plays_through_its_grace_then_stops() {
        let mut old = Sealer::new(3, &B1, 7, 0);
        let mut o = Opener::new();
        o.enter(3, &B1, 0);
        o.enter(4, &B2, 1_000);
        let mut new = Sealer::new(4, &B2, 7, 0);
        assert!(o.open(&old.seal(KIND_AUDIO, 0, b"", b"a").unwrap(), 1_000 + GRACE_MS - 1).is_ok());
        assert!(o.open(&new.seal(KIND_AUDIO, 0, b"", b"b").unwrap(), 1_000 + GRACE_MS).is_ok());
        assert_eq!(o.open(&old.seal(KIND_AUDIO, 0, b"", b"c").unwrap(), 1_000 + GRACE_MS), Err(Refusal::UnknownEpoch));
    }

    #[test]
    fn replay_window_takes_reordering_within_it() {
        let mut s = Sealer::new(0, &B1, 1, 0);
        let mut o = Opener::new();
        o.enter(0, &B1, 0);
        let frames: Vec<_> = (0..70).map(|_| s.seal(KIND_AUDIO, 0, b"", b"x").unwrap()).collect();
        assert!(o.open(&frames[69], 0).is_ok());
        assert!(o.open(&frames[10], 0).is_ok(), "59 back, inside the window");
        assert_eq!(o.open(&frames[2], 0), Err(Refusal::Replayed), "67 back, outside it");
    }

    #[test]
    fn epoch_slots_are_reused_after_256_epochs() {
        let mut o = Opener::new();
        o.enter(5, &B1, 0);
        o.enter(5 + 256, &B2, 0);
        let f = Sealer::new(5 + 256, &B2, 1, 0).seal(KIND_AUDIO, 0, b"", b"x").unwrap();
        assert_eq!(o.open(&f, 0).unwrap().epoch, 5 + 256);
    }
}
