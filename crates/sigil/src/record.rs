//! Stateful records (spec §6.1): what the server stores.
//!
//! ```text
//! header (40 bytes, clear, authenticated):
//!   u8 version · u8 kind · [16] group · u64 epoch · u16 service · u32 sender · u64 seq
//! ciphertext: AES-128-GCM(record_key, record_nonce ⊕ (sender ‖ seq), aad = header)
//! signature:  Ed25519(sender's leaf key, "sigil/record/v1" ‖ header ‖ ciphertext)   (64 bytes)
//! ```
//!
//! The header is readable by the server, which routes and pages on it; the
//! body is readable by whoever holds the service's [`StateKey`] for `epoch`;
//! the signature says which member wrote it, which the shared key alone
//! cannot.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use ed25519_dalek::{Signature, VerifyingKey};
use openmls_traits::signatures::Signer;

use crate::keys::StateKey;
use crate::service::ServiceId;
use crate::wire::{Reader, Writer};
use crate::{Error, GroupId, Result};

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 40;
pub const SIGNATURE_LEN: usize = 64;
const LABEL: &[u8] = b"sigil/record/v1";

/// What a record is. `Data` belongs to the service; the rest to the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Data,
    /// The previous epoch's state key, sealed under this one's (§5.3).
    Link,
    Checkpoint,
    Cosign,
}

impl RecordKind {
    fn code(self) -> u8 {
        match self {
            RecordKind::Data => 0,
            RecordKind::Link => 1,
            RecordKind::Checkpoint => 2,
            RecordKind::Cosign => 3,
        }
    }
    fn from_code(c: u8) -> Result<RecordKind> {
        match c {
            0 => Ok(RecordKind::Data),
            1 => Ok(RecordKind::Link),
            2 => Ok(RecordKind::Checkpoint),
            3 => Ok(RecordKind::Cosign),
            _ => Err(Error::Malformed("record kind")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: RecordKind,
    pub group: GroupId,
    /// The epoch it was written in: names its key and its signer's leaf.
    pub epoch: u64,
    pub service: ServiceId,
    /// The writer's leaf index.
    pub sender: u32,
    /// Per (sender, service), never reset, so readers can see gaps.
    pub seq: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut w = Writer::default();
        w.u8(VERSION).u8(self.kind.code()).fixed(&self.group).u64(self.epoch).u16(self.service).u32(self.sender).u64(self.seq);
        w.finish().try_into().expect("header is 40 bytes")
    }

    fn decode(bytes: &[u8]) -> Result<Header> {
        let mut r = Reader::new(bytes, "record header");
        if r.u8()? != VERSION {
            return Err(Error::Malformed("record version"));
        }
        let h = Header {
            kind: RecordKind::from_code(r.u8()?)?,
            group: r.array()?,
            epoch: r.u64()?,
            service: r.u16()?,
            sender: r.u32()?,
            seq: r.u64()?,
        };
        r.done()?;
        Ok(h)
    }

    fn nonce(&self, key: &StateKey) -> [u8; 12] {
        let mut n = key.record_nonce();
        let mut tail = [0u8; 12];
        tail[..4].copy_from_slice(&self.sender.to_be_bytes());
        tail[4..].copy_from_slice(&self.seq.to_be_bytes());
        n.iter_mut().zip(tail).for_each(|(a, b)| *a ^= b);
        n
    }
}

/// A record split into its parts, unverified. What a server sees.
pub struct Parsed<'a> {
    pub header: Header,
    pub ciphertext: &'a [u8],
    pub signature: &'a [u8; SIGNATURE_LEN],
    signed: &'a [u8],
}

/// Split a record. Checks its shape, not its signature or contents.
pub fn parse(bytes: &[u8]) -> Result<Parsed<'_>> {
    if bytes.len() < HEADER_LEN + 16 + SIGNATURE_LEN {
        return Err(Error::Malformed("record"));
    }
    let (signed, signature) = bytes.split_at(bytes.len() - SIGNATURE_LEN);
    Ok(Parsed {
        header: Header::decode(&signed[..HEADER_LEN])?,
        ciphertext: &signed[HEADER_LEN..],
        signature: signature.try_into().expect("split at 64"),
        signed,
    })
}

fn signing_input(signed: &[u8]) -> Vec<u8> {
    [LABEL, signed].concat()
}

/// Seal and sign a record.
pub fn seal(header: &Header, key: &StateKey, plaintext: &[u8], signer: &impl Signer) -> Result<Vec<u8>> {
    let head = header.encode();
    let cipher = Aes128Gcm::new_from_slice(&key.record_key()).expect("16-byte key");
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&header.nonce(key)), Payload { msg: plaintext, aad: &head })
        .map_err(|_| Error::Malformed("record plaintext"))?;
    let mut out = [head.as_slice(), &ciphertext].concat();
    let signature = signer.sign(&signing_input(&out)).map_err(|e| Error::Mls(format!("{e:?}")))?;
    if signature.len() != SIGNATURE_LEN {
        return Err(Error::Mls("signer is not Ed25519".into()));
    }
    out.extend_from_slice(&signature);
    Ok(out)
}

/// Check a parsed record's signature against the writer's leaf key.
pub fn verify(parsed: &Parsed<'_>, signature_key: &[u8]) -> Result<()> {
    let key: [u8; 32] = signature_key.try_into().map_err(|_| Error::Unauthentic)?;
    let vk = VerifyingKey::from_bytes(&key).map_err(|_| Error::Unauthentic)?;
    vk.verify_strict(&signing_input(parsed.signed), &Signature::from_bytes(parsed.signature))
        .map_err(|_| Error::Unauthentic)
}

/// Verify a record and open it.
pub fn open(bytes: &[u8], key: &StateKey, signature_key: &[u8]) -> Result<(Header, Vec<u8>)> {
    let parsed = parse(bytes)?;
    verify(&parsed, signature_key)?;
    let head = parsed.header.encode();
    let cipher = Aes128Gcm::new_from_slice(&key.record_key()).expect("16-byte key");
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&parsed.header.nonce(key)), Payload { msg: parsed.ciphertext, aad: &head })
        .map_err(|_| Error::Unauthentic)?;
    Ok((parsed.header, plaintext))
}

/// One leaf of an epoch's roster, as a link carries it: leaf index, the
/// member's serialized MLS credential, and their signature key.
pub type RosterEntry = (u32, Vec<u8>, Vec<u8>);

/// A `link` record's body (§5.3): an earlier epoch's state key, and who held
/// each leaf then, so records from that epoch can be verified as well as
/// read.
pub fn link_body(previous_epoch: u64, previous: &StateKey, roster: &[RosterEntry]) -> Vec<u8> {
    let mut w = Writer::default();
    w.u64(previous_epoch).fixed(previous.as_bytes()).u32(roster.len() as u32);
    for (leaf, credential, key) in roster {
        w.u32(*leaf).bytes(credential).bytes(key);
    }
    w.finish()
}

pub fn parse_link(body: &[u8]) -> Result<(u64, StateKey, Vec<RosterEntry>)> {
    let mut r = Reader::new(body, "link");
    let epoch = r.u64()?;
    let key = StateKey::from_bytes(r.array()?);
    let mut roster = Vec::new();
    for _ in 0..r.u32()? {
        roster.push((r.u32()?, r.bytes()?.to_vec(), r.bytes()?.to_vec()));
    }
    r.done()?;
    Ok((epoch, key, roster))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use openmls_basic_credential::SignatureKeyPair;
    use openmls_traits::types::SignatureScheme;

    pub fn signer() -> SignatureKeyPair {
        SignatureKeyPair::new(SignatureScheme::ED25519).unwrap()
    }

    fn header() -> Header {
        Header { kind: RecordKind::Data, group: [7; 16], epoch: 4, service: 1, sender: 2, seq: 9 }
    }

    #[test]
    fn seals_and_opens() {
        let s = signer();
        let key = StateKey::from_bytes([5; 32]);
        let rec = seal(&header(), &key, b"hello", &s).unwrap();
        assert_eq!(rec.len(), HEADER_LEN + 5 + 16 + SIGNATURE_LEN);
        let (h, body) = open(&rec, &key, s.public()).unwrap();
        assert_eq!(h, header());
        assert_eq!(body, b"hello");
    }

    #[test]
    fn the_server_reads_the_header_only() {
        let rec = seal(&header(), &StateKey::from_bytes([5; 32]), b"secret", &signer()).unwrap();
        let p = parse(&rec).unwrap();
        assert_eq!(p.header, header());
        assert!(!rec.windows(6).any(|w| w == b"secret"));
    }

    #[test]
    fn refuses_tampering_wrong_keys_and_wrong_signers() {
        let s = signer();
        let key = StateKey::from_bytes([5; 32]);
        let rec = seal(&header(), &key, b"hello", &s).unwrap();
        for i in 0..rec.len() {
            let mut bad = rec.clone();
            bad[i] ^= 1;
            assert!(open(&bad, &key, s.public()).is_err(), "byte {i}");
        }
        assert_eq!(open(&rec, &StateKey::from_bytes([6; 32]), s.public()), Err(Error::Unauthentic));
        assert_eq!(open(&rec, &key, signer().public()), Err(Error::Unauthentic));
    }

    #[test]
    fn nonces_differ_by_sender_and_seq() {
        let key = StateKey::from_bytes([5; 32]);
        let a = header();
        let b = Header { seq: 10, ..a };
        let c = Header { sender: 3, ..a };
        assert_ne!(a.nonce(&key), b.nonce(&key));
        assert_ne!(a.nonce(&key), c.nonce(&key));
    }

    #[test]
    fn links_round_trip() {
        let k = StateKey::from_bytes([8; 32]);
        let roster = vec![(0, b"cred".to_vec(), vec![1; 32]), (3, b"other".to_vec(), vec![2; 32])];
        let (e, back, r) = parse_link(&link_body(3, &k, &roster)).unwrap();
        assert_eq!((e, back, r), (3, k, roster));
    }
}
