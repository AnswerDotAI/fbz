//! MSB-first bit writer — the mirror image of the decoder's `BitReader`.
//!
//! Sans-io by construction: everything lands in a `Vec<u8>` the caller owns.

use std::mem;
use std::vec::Vec;

/// Accumulates bits most-significant-first into a byte buffer.
pub struct BitWriter { out: Vec<u8>, acc: u64, nbits: u32 }

impl BitWriter {
    pub fn new() -> Self { BitWriter { out: Vec::new(), acc: 0, nbits: 0 } }

    pub fn with_capacity(cap: usize) -> Self { BitWriter { out: Vec::with_capacity(cap), acc: 0, nbits: 0 } }

    /// Write the low `n` bits of `val`, most significant first. `n <= 32`.
    #[inline]
    pub fn write_bits(&mut self, n: u32, val: u32) {
        debug_assert!(n <= 32);
        let val = if n == 32 { val as u64 } else { (val as u64) & ((1u64 << n) - 1) };
        self.acc = (self.acc << n) | val;
        self.nbits += n;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push((self.acc >> self.nbits) as u8);
        }
        self.acc &= (1u64 << self.nbits) - 1;
    }

    #[inline]
    pub fn write_bit(&mut self, bit: u32) { self.write_bits(1, bit); }

    /// Write a 48-bit block or end-of-stream magic.
    pub fn write_magic(&mut self, magic: u64) {
        self.write_bits(24, (magic >> 24) as u32);
        self.write_bits(24, (magic & 0xff_ffff) as u32);
    }

    pub fn write_u8(&mut self, byte: u8) { self.write_bits(8, byte as u32); }

    pub fn write_u32(&mut self, val: u32) { self.write_bits(32, val); }

    /// Take the whole bytes emitted so far, leaving any partial byte behind.
    ///
    /// This is what keeps a streaming writer's memory bounded: output can be
    /// handed off as it is produced instead of accumulating until `finish`.
    pub fn drain(&mut self) -> Vec<u8> { mem::take(&mut self.out) }

    /// Append an exact MSB-first bit buffer whose final byte may be padded.
    pub fn write_buffer(&mut self, bytes: &[u8], bit_len: usize) {
        debug_assert!(bit_len <= bytes.len() * 8);
        let full = bit_len / 8;
        for &byte in &bytes[..full] { self.write_u8(byte); }
        let trailing = bit_len % 8;
        if trailing != 0 { self.write_bits(trailing as u32, u32::from(bytes[full] >> (8 - trailing))); }
    }

    /// Pad a copy of the final byte while retaining the exact meaningful length.
    pub fn finish_bits(mut self) -> (Vec<u8>, usize) {
        let bit_len = self.out.len() * 8 + self.nbits as usize;
        if self.nbits > 0 { self.out.push((self.acc << (8 - self.nbits)) as u8); }
        (self.out, bit_len)
    }

    /// Pad the final partial byte with zero bits and take the buffer.
    pub fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            let pad = 8 - self.nbits;
            self.write_bits(pad, 0);
        }
        debug_assert_eq!(self.nbits, 0);
        self.out
    }
}

impl Default for BitWriter { fn default() -> Self { BitWriter::new() } }
