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
        if self.len > 0 && b == self.byte && self.len < MAX_RUN {
            self.len += 1;
            return None;
        }
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

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;

    /// Drive the splitter over a whole buffer, as the block builder does.
    fn encode(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut split = Splitter::new();
        let mut raw = 0usize;
        for &b in input {
            if let Some(g) = split.push(b) {
                g.write_into(&mut out);
                raw += g.raw_len;
            }
        }
        if let Some(g) = split.finish() {
            g.write_into(&mut out);
            raw += g.raw_len;
        }
        assert_eq!(raw, input.len(), "groups must account for every input byte");
        out
    }

    /// The inverse transform, lifted from the decoder's tail loop, so the tests
    /// check the two halves against each other rather than against a guess.
    fn decode(enc: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev: i32 = -1;
        let mut count = 0u32;
        for &b in enc {
            if count == 4 {
                for _ in 0..b { out.push(prev as u8); }
                count = 0;
                prev = -1;
            } else {
                out.push(b);
                if b as i32 == prev { count += 1; } else {
                    prev = b as i32;
                    count = 1;
                }
            }
        }
        out
    }

    #[test]
    fn passes_short_runs_through() {
        assert_eq!(encode(b"abcaaabbb"), b"abcaaabbb");
        assert_eq!(decode(&encode(b"abcaaabbb")), b"abcaaabbb");
    }

    #[test]
    fn encodes_a_run_of_exactly_four() {
        assert_eq!(encode(b"aaaa"), b"aaaa\x00");
        assert_eq!(decode(&encode(b"aaaa")), b"aaaa");
    }

    #[test]
    fn encodes_a_run_of_five() {
        assert_eq!(encode(b"aaaaa"), b"aaaa\x01");
        assert_eq!(decode(&encode(b"aaaaa")), b"aaaaa");
    }

    #[test]
    fn splits_runs_longer_than_the_cap() {
        let input = vec![b'z'; 300];
        let enc = encode(&input);
        // 259 in the first group, then 41 in a second.
        assert_eq!(&enc[..5], b"zzzz\xff");
        assert_eq!(&enc[5..10], &[b'z', b'z', b'z', b'z', 41 - 4]);
        assert_eq!(enc.len(), 10);
        assert_eq!(decode(&enc), input);
    }

    #[test]
    fn splits_a_run_of_exactly_262() {
        // 259 plus a 3-byte tail that has to come out as literals.
        let input = vec![b'q'; 262];
        assert_eq!(encode(&input), b"qqqq\xffqqq");
        assert_eq!(decode(&encode(&input)), input);
    }

    #[test]
    fn group_sizes_are_reported_correctly() {
        for raw_len in 1..=MAX_RUN {
            let g = Group { byte: 7, raw_len };
            let mut out = Vec::new();
            g.write_into(&mut out);
            assert_eq!(out.len(), g.encoded_len(), "raw_len {raw_len}");
            assert_eq!(decode(&out), vec![7u8; raw_len]);
        }
    }

    #[test]
    fn handles_empty_input() { assert!(encode(b"").is_empty()); }

    #[test]
    fn a_run_never_exceeds_the_cap() {
        let input = vec![b'k'; 5000];
        let mut split = Splitter::new();
        let mut groups = Vec::new();
        for &b in &input { if let Some(g) = split.push(b) { groups.push(g); } }
        if let Some(g) = split.finish() { groups.push(g); }
        assert!(groups.iter().all(|g| g.raw_len <= MAX_RUN));
        assert_eq!(groups.iter().map(|g| g.raw_len).sum::<usize>(), 5000);
    }

    #[test]
    fn round_trips_a_gnarly_input() {
        let mut input = Vec::new();
        for i in 0..2000u32 { let b = (i / 7 % 5) as u8; for _ in 0..(i % 9) { input.push(b); } }
        for cut in [0, 1, 2, 3, 4, 5, 100, 1000, input.len()] {
            let slice = &input[..cut.min(input.len())];
            assert_eq!(decode(&encode(slice)), slice);
        }
    }
}
