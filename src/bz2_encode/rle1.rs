//! RLE1 — the first run-length stage, applied to the raw plaintext.
//!
//! A run of four identical bytes is followed by a single byte giving the number
//! of *extra* repeats (0..=255), so the longest run one group can express is
//! 259 bytes. The decoder resets its run state after consuming that count byte,
//! which is what lets a block boundary fall between any two groups — the
//! property the block builder relies on.

use std::vec::Vec;

/// The longest run one group can express: four literals plus 255 extra.
pub const MAX_RUN: usize = 4 + 255;

/// A maximal run of one byte value, capped at [`MAX_RUN`]. This is the atom the
/// block builder places: a group is never split across blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group { pub byte: u8, pub raw_len: usize }

impl Group {
    /// How many bytes this group occupies in the RLE1 output.
    pub fn encoded_len(self) -> usize { if self.raw_len >= 4 { 5 } else { self.raw_len } }

    /// Append the group's encoded form to `out`.
    pub fn write_into(self, out: &mut Vec<u8>) {
        let b = self.byte;
        if self.raw_len >= 4 {
            out.extend_from_slice(&[b, b, b, b]);
            out.push((self.raw_len - 4) as u8);
        } else { for _ in 0..self.raw_len { out.push(b); } }
    }
}

/// Splits a byte stream into RLE1 groups as it arrives.
///
/// Incremental by design: a run may span any number of calls, so the caller
/// never has to buffer plaintext just to find run boundaries.
pub struct Splitter { byte: u8, len: usize }

impl Splitter {
    pub fn new() -> Splitter { Splitter { byte: 0, len: 0 } }

    /// Feed one byte. Returns the group that just closed, if any.
    #[inline]
    pub fn push(&mut self, b: u8) -> Option<Group> {
        if self.len > 0 && b == self.byte && self.len < MAX_RUN { self.len += 1; return None; }
        let done = self.take();
        self.byte = b;
        self.len = 1;
        done
    }

    /// Close the stream, returning the final partial group.
    pub fn finish(&mut self) -> Option<Group> { self.take() }

    fn take(&mut self) -> Option<Group> {
        if self.len == 0 { return None; }
        let group = Group { byte: self.byte, raw_len: self.len };
        self.len = 0;
        Some(group)
    }
}

impl Default for Splitter { fn default() -> Self { Splitter::new() } }
