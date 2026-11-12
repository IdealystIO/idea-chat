//! Big-endian, length-prefixed encoding for Sigil's own structures.
//!
//! Deliberately small: fixed-width integers, and byte strings and text with a
//! `u32` length in front. Readers refuse trailing bytes, so one value has one
//! encoding.

use crate::{Error, Result};

#[derive(Default)]
pub(crate) struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn fixed(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32).fixed(v)
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    pub fn finish(self) -> Vec<u8> {
        self.0
    }
}

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    what: &'static str,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], what: &'static str) -> Self {
        Reader { buf, what }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            return Err(Error::Malformed(self.what));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("took N"))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn str(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| Error::Malformed(self.what))
    }
    pub fn done(self) -> Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(Error::Malformed(self.what))
        }
    }
}
