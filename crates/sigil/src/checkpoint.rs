//! Checkpoints (spec §7.3): trusting a service's state without replaying the
//! records that built it.
//!
//! A service turns its state into chunks under paths; [`root`] hashes them
//! into one value. A member posts a [`Checkpoint`] naming the root after some
//! record; others who hold the state post a [`Cosign`] with the root *they*
//! compute. [`evaluate`] decides from those signatures alone — no state, no
//! keys — so a server can run it too, to check a prune request.
//!
//! ```text
//! leaf = SHA-256(0x00 ‖ u32 len(path) ‖ path ‖ chunk)
//! node = SHA-256(0x01 ‖ left ‖ right)     leaves sorted by path; an odd one carries up
//! root = SHA-256(0x02)                    for empty state
//! ```

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::blob::BlobRef;
use crate::policy::{Role, TrustRule};
use crate::wire::{Reader, Writer};
use crate::{Error, Result, UserId};

pub type Hash = [u8; 32];

pub fn leaf(path: &[u8], chunk: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([0x00]);
    h.update((path.len() as u32).to_be_bytes());
    h.update(path);
    h.update(chunk);
    h.finalize().into()
}

/// The root over `leaves`, which must be sorted by path with no repeats.
pub fn root(leaves: &[(Vec<u8>, Hash)]) -> Result<Hash> {
    if leaves.windows(2).any(|w| w[0].0 >= w[1].0) {
        return Err(Error::Malformed("checkpoint leaves: unsorted or repeated paths"));
    }
    if leaves.is_empty() {
        return Ok(Sha256::digest([0x02]).into());
    }
    let mut level: Vec<Hash> = leaves.iter().map(|(_, h)| *h).collect();
    while level.len() > 1 {
        level = level
            .chunks(2)
            .map(|pair| match pair {
                [l, r] => {
                    let mut h = Sha256::new();
                    h.update([0x01]);
                    h.update(l);
                    h.update(r);
                    h.finalize().into()
                }
                [only] => *only,
                _ => unreachable!(),
            })
            .collect();
    }
    Ok(level[0])
}

/// The state's root, from its chunks in any order.
pub fn root_of(chunks: &BTreeMap<Vec<u8>, Vec<u8>>) -> Hash {
    let leaves: Vec<_> = chunks.iter().map(|(p, c)| (p.clone(), leaf(p, c))).collect();
    root(&leaves).expect("a map is sorted and unique")
}

/// Where each chunk of a checkpointed state is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: Vec<u8>,
    pub leaf: Hash,
    pub blob: BlobRef,
}

impl Manifest {
    pub fn root(&self) -> Result<Hash> {
        let leaves: Vec<_> = self.entries.iter().map(|e| (e.path.clone(), e.leaf)).collect();
        root(&leaves)
    }

    /// Check a fetched chunk against its entry.
    pub fn check(entry: &Entry, chunk: &[u8]) -> Result<()> {
        if leaf(&entry.path, chunk) == entry.leaf {
            Ok(())
        } else {
            Err(Error::Unauthentic)
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(self.entries.len() as u32);
        for e in &self.entries {
            w.bytes(&e.path).fixed(&e.leaf);
            e.blob.write(&mut w);
        }
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Manifest> {
        let mut r = Reader::new(bytes, "manifest");
        let mut entries = Vec::new();
        for _ in 0..r.u32()? {
            entries.push(Entry { path: r.bytes()?.to_vec(), leaf: r.array()?, blob: BlobRef::read(&mut r)? });
        }
        r.done()?;
        Ok(Manifest { entries })
    }
}

/// "After server sequence `after`, the state's root is `root`."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub after: u64,
    pub root: Hash,
    pub manifest: BlobRef,
}

impl Checkpoint {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u64(self.after).fixed(&self.root);
        self.manifest.write(&mut w);
        w.finish()
    }
    pub fn decode(bytes: &[u8]) -> Result<Checkpoint> {
        let mut r = Reader::new(bytes, "checkpoint");
        let c = Checkpoint { after: r.u64()?, root: r.array()?, manifest: BlobRef::read(&mut r)? };
        r.done()?;
        Ok(c)
    }
}

/// "For the checkpoint at server sequence `checkpoint`, whose record hashes
/// to `record`, I compute `root`." Matching roots vouch; differing ones
/// dispute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cosign {
    pub checkpoint: u64,
    pub record: Hash,
    pub root: Hash,
}

impl Cosign {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u64(self.checkpoint).fixed(&self.record).fixed(&self.root);
        w.finish()
    }
    pub fn decode(bytes: &[u8]) -> Result<Cosign> {
        let mut r = Reader::new(bytes, "cosign");
        let c = Cosign { checkpoint: r.u64()?, record: r.array()?, root: r.array()? };
        r.done()?;
        Ok(c)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    /// Not enough agreement yet.
    Pending { have: usize, need: usize },
    /// Someone computed a different root. Not trusted; people see a warning
    /// naming who stands behind each root.
    Disputed { roots: BTreeMap<Hash, BTreeSet<UserId>> },
}

/// Decide a checkpoint's trust from who vouched for which root.
///
/// `signers` are the poster and every cosigner, each with the root they
/// signed; only verified signatures belong here. `people` is how many
/// distinct people are in the group, so a group smaller than the rule's
/// threshold needs all of them.
pub fn evaluate(
    root: &Hash,
    signers: &[(UserId, Hash)],
    rule: TrustRule,
    role_of: impl Fn(&str) -> Role,
    people: usize,
) -> Trust {
    let mut roots: BTreeMap<Hash, BTreeSet<UserId>> = BTreeMap::new();
    for (user, r) in signers {
        roots.entry(*r).or_default().insert(user.clone());
    }
    if roots.keys().any(|r| r != root) {
        return Trust::Disputed { roots };
    }
    let agree = roots.remove(root).unwrap_or_default();
    if agree.iter().any(|u| rule.or_any_of.contains(role_of(u))) {
        return Trust::Trusted;
    }
    let need = (rule.people as usize).min(people).max(1);
    if agree.len() >= need {
        Trust::Trusted
    } else {
        Trust::Pending { have: agree.len(), need }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Roles;

    fn state() -> BTreeMap<Vec<u8>, Vec<u8>> {
        BTreeMap::from([(b"a".to_vec(), b"one".to_vec()), (b"b".to_vec(), b"two".to_vec()), (b"c".to_vec(), b"three".to_vec())])
    }

    #[test]
    fn roots_depend_on_every_chunk_and_path() {
        let base = root_of(&state());
        let mut changed = state();
        changed.insert(b"b".to_vec(), b"TWO".to_vec());
        assert_ne!(root_of(&changed), base);
        let mut moved = state();
        let v = moved.remove(b"c".as_slice()).unwrap();
        moved.insert(b"d".to_vec(), v);
        assert_ne!(root_of(&moved), base);
        assert_ne!(root_of(&BTreeMap::new()), base);
        // Path and chunk boundaries can't be shifted between each other.
        assert_ne!(leaf(b"ab", b"c"), leaf(b"a", b"bc"));
    }

    #[test]
    fn refuses_unsorted_or_repeated_leaves() {
        let l = leaf(b"x", b"y");
        assert!(root(&[(b"b".to_vec(), l), (b"a".to_vec(), l)]).is_err());
        assert!(root(&[(b"a".to_vec(), l), (b"a".to_vec(), l)]).is_err());
    }

    #[test]
    fn manifests_round_trip_and_check_chunks() {
        let blob = BlobRef { id: "m".into(), key: [1; 32], size: 3, sha256: [2; 32], content_type: "x".into() };
        let m = Manifest {
            entries: state().iter().map(|(p, c)| Entry { path: p.clone(), leaf: leaf(p, c), blob: blob.clone() }).collect(),
        };
        assert_eq!(m.root().unwrap(), root_of(&state()));
        assert_eq!(Manifest::decode(&m.encode()).unwrap(), m);
        assert!(Manifest::check(&m.entries[0], b"one").is_ok());
        assert!(Manifest::check(&m.entries[0], b"uno").is_err());
        let c = Checkpoint { after: 5000, root: m.root().unwrap(), manifest: blob };
        assert_eq!(Checkpoint::decode(&c.encode()).unwrap(), c);
        let s = Cosign { checkpoint: 5001, record: [3; 32], root: c.root };
        assert_eq!(Cosign::decode(&s.encode()).unwrap(), s);
    }

    fn rule() -> TrustRule {
        TrustRule { people: 2, or_any_of: Roles::of(&[Role::Admin]) }
    }

    fn role(u: &str) -> Role {
        if u == "admin" {
            Role::Admin
        } else {
            Role::Member
        }
    }

    #[test]
    fn trust_needs_enough_distinct_people() {
        let r = [1; 32];
        assert_eq!(evaluate(&r, &[("a".into(), r)], rule(), role, 5), Trust::Pending { have: 1, need: 2 });
        // The same person twice (two devices) is still one person.
        assert_eq!(evaluate(&r, &[("a".into(), r), ("a".into(), r)], rule(), role, 5), Trust::Pending { have: 1, need: 2 });
        assert_eq!(evaluate(&r, &[("a".into(), r), ("b".into(), r)], rule(), role, 5), Trust::Trusted);
        assert_eq!(evaluate(&r, &[("admin".into(), r)], rule(), role, 5), Trust::Trusted);
    }

    #[test]
    fn a_small_group_needs_everyone() {
        let r = [1; 32];
        let one = TrustRule { people: 3, or_any_of: Roles::NONE };
        assert_eq!(evaluate(&r, &[("a".into(), r)], one, role, 2), Trust::Pending { have: 1, need: 2 });
        assert_eq!(evaluate(&r, &[("a".into(), r), ("b".into(), r)], one, role, 2), Trust::Trusted);
    }

    #[test]
    fn any_disagreement_is_a_dispute() {
        let r = [1; 32];
        let other = [2; 32];
        let t = evaluate(&r, &[("admin".into(), r), ("b".into(), r), ("c".into(), other)], rule(), role, 5);
        let Trust::Disputed { roots } = t else { panic!("expected a dispute") };
        assert_eq!(roots[&other], BTreeSet::from(["c".to_string()]));
        assert_eq!(roots[&r].len(), 2);
    }
}
