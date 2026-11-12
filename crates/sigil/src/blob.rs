//! Blobs (spec §7.1): large payloads under a key of their own.
//!
//! The plaintext is padded to a multiple of [`PAD`], cut into [`CHUNK`]-byte
//! pieces, and each sealed with AES-256-GCM. A piece's nonce carries its index
//! and whether it is the last, so pieces can't be reordered, dropped from the
//! end, or spliced in from another blob. The [`BlobRef`] that points to a
//! blob — inside a record, so readable only by the record's readers — carries
//! the key, the true size and a hash of the plaintext.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use sha2::{Digest, Sha256};

use crate::wire::{Reader, Writer};
use crate::{Error, Result};

pub const CHUNK: usize = 64 * 1024;
const TAG: usize = 16;
/// Ciphertext sizes are multiples of this, so the server learns sizes only
/// roughly.
pub const PAD: usize = 1024;
const AAD: &[u8] = b"sigil/blob/v1";

fn nonce(index: u64, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&index.to_be_bytes());
    n[8] = last as u8;
    n
}

/// Seal `plaintext` under a fresh, single-use `key`.
pub fn seal(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let padded_len = plaintext.len().div_ceil(PAD).max(1) * PAD;
    let mut padded = plaintext.to_vec();
    padded.resize(padded_len, 0);
    let cipher = Aes256Gcm::new_from_slice(key).expect("32-byte key");
    let pieces: Vec<&[u8]> = padded.chunks(CHUNK).collect();
    let mut out = Vec::with_capacity(padded_len + pieces.len() * TAG);
    for (i, piece) in pieces.iter().enumerate() {
        let last = i + 1 == pieces.len();
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce(i as u64, last)), Payload { msg: piece, aad: AAD })
            .expect("chunk fits");
        out.extend_from_slice(&sealed);
    }
    out
}

/// Open a blob and cut it back to `size`.
pub fn open(key: &[u8; 32], ciphertext: &[u8], size: u64) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("32-byte key");
    let pieces: Vec<&[u8]> = ciphertext.chunks(CHUNK + TAG).collect();
    if pieces.is_empty() {
        return Err(Error::Malformed("blob"));
    }
    let mut out = Vec::with_capacity(ciphertext.len());
    for (i, piece) in pieces.iter().enumerate() {
        let last = i + 1 == pieces.len();
        let plain = cipher
            .decrypt(Nonce::from_slice(&nonce(i as u64, last)), Payload { msg: piece, aad: AAD })
            .map_err(|_| Error::Unauthentic)?;
        out.extend_from_slice(&plain);
    }
    if size as usize > out.len() {
        return Err(Error::Malformed("blob size"));
    }
    out.truncate(size as usize);
    Ok(out)
}

/// Where a blob is and how to open it. Lives inside a sealed record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    /// The storage id the server knows it by.
    pub id: String,
    pub key: [u8; 32],
    pub size: u64,
    pub sha256: [u8; 32],
    pub content_type: String,
}

impl BlobRef {
    /// Seal `plaintext` under `key` (random, fresh for this blob) and describe
    /// it. Returns the reference and the ciphertext to store under `id`.
    pub fn seal(id: &str, key: [u8; 32], plaintext: &[u8], content_type: &str) -> (BlobRef, Vec<u8>) {
        let r = BlobRef {
            id: id.to_string(),
            key,
            size: plaintext.len() as u64,
            sha256: Sha256::digest(plaintext).into(),
            content_type: content_type.to_string(),
        };
        (r, seal(&key, plaintext))
    }

    /// Open the blob's ciphertext and check it is the one referred to.
    pub fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let plain = open(&self.key, ciphertext, self.size)?;
        if Sha256::digest(&plain).as_slice() != self.sha256 {
            return Err(Error::Unauthentic);
        }
        Ok(plain)
    }

    pub(crate) fn write(&self, w: &mut Writer) {
        w.str(&self.id).fixed(&self.key).u64(self.size).fixed(&self.sha256).str(&self.content_type);
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<BlobRef> {
        Ok(BlobRef { id: r.str()?, key: r.array()?, size: r.u64()?, sha256: r.array()?, content_type: r.str()? })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        self.write(&mut w);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<BlobRef> {
        let mut r = Reader::new(bytes, "blob ref");
        let b = BlobRef::read(&mut r)?;
        r.done()?;
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_size_class() {
        for size in [0, 1, PAD - 1, PAD, PAD + 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17] {
            let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
            let (r, ct) = BlobRef::seal("b1", [9; 32], &data, "application/octet-stream");
            let pieces = ct.len().div_ceil(CHUNK + TAG);
            assert_eq!((ct.len() - pieces * TAG) % PAD, 0, "size {size} is padded");
            assert_eq!(r.open(&ct).unwrap(), data, "size {size}");
            assert_eq!(BlobRef::decode(&r.encode()).unwrap(), r);
        }
    }

    #[test]
    fn refuses_truncation_reordering_and_tampering() {
        let data = vec![1u8; 2 * CHUNK + 5];
        let (r, ct) = BlobRef::seal("b", [9; 32], &data, "x");
        let piece = CHUNK + TAG;
        // Dropping the last piece leaves a non-final piece at the end.
        assert!(r.open(&ct[..2 * piece]).is_err());
        // Swapping the first two pieces.
        let swapped = [&ct[piece..2 * piece], &ct[..piece], &ct[2 * piece..]].concat();
        assert!(r.open(&swapped).is_err());
        let mut bad = ct.clone();
        bad[10] ^= 1;
        assert!(r.open(&bad).is_err());
        // Another blob's ciphertext under the same reference.
        let (_, other) = BlobRef::seal("b", [9; 32], &vec![2u8; 2 * CHUNK + 5], "x");
        assert!(r.open(&other).is_err());
    }
}
