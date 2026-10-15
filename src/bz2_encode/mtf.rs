//! Move-to-front plus RLE2 — the stage that turns the BWT output into the
//! symbol alphabet the Huffman coder sees.
//!
//! Symbol space, matching the decoder exactly:
//!
//! * `0` = RUNA, `1` = RUNB — a bijective base-2 encoding of a run of MTF
//!   index zero, least significant digit first, RUNA worth `1 << k` and RUNB
//!   worth `2 << k`.
//! * `2 ..= n_in_use` — MTF index `sym - 1`, i.e. indices 1 and up.
//! * `n_in_use + 1` = EOB.

use std::vec::Vec;

pub const RUNA: u16 = 0;
pub const RUNB: u16 = 1;

/// The MTF/RLE2 symbol stream for one block.
pub struct Symbols {
    /// The byte values present in the block, ascending — bzip2's `seqToUnseq`.
    pub in_use: Vec<u8>,
    /// The symbol stream, terminated by EOB.
    pub syms: Vec<u16>,
}

impl Symbols {
    /// Size of the Huffman alphabet: `n_in_use + 2`.
    pub fn alpha_size(&self) -> usize { self.in_use.len() + 2 }
}

/// Which of the 256 byte values occur in `block`.
pub fn in_use(block: &[u8]) -> Vec<u8> {
    let mut seen = [false; 256];
    for &b in block { seen[b as usize] = true; }
    (0..256usize).filter(|&b| seen[b]).map(|b| b as u8).collect()
}

/// Move-to-front and RLE2 encode the BWT output.
pub fn encode(block: &[u8]) -> Symbols {
    let in_use = in_use(block);
    let mut syms = Vec::with_capacity(block.len() / 2 + 8);

    // MTF list starts as the in-use bytes in ascending order. A linear scan is
    // what libbz2 does too: after the BWT the hit is almost always near the
    // front, so the average probe is very short.
    let mut mtf = in_use.clone();
    let mut zeros: u32 = 0;

    for &b in block {
        let idx = mtf.iter().position(|&m| m == b).expect("byte not in use");
        if idx == 0 { zeros += 1; continue; }
        flush_zero_run(&mut syms, &mut zeros);
        mtf.copy_within(0..idx, 1);
        mtf[0] = b;
        syms.push(idx as u16 + 1);
    }
    flush_zero_run(&mut syms, &mut zeros);

    let eob = (in_use.len() + 1) as u16;
    syms.push(eob);

    Symbols { in_use, syms }
}

/// Emit a pending run of MTF index zero as RUNA/RUNB digits.
fn flush_zero_run(syms: &mut Vec<u16>, zeros: &mut u32) {
    let mut n = *zeros;
    *zeros = 0;
    while n > 0 {
        if n % 2 == 1 {
            syms.push(RUNA);
            n = (n - 1) / 2;
        } else {
            syms.push(RUNB);
            n = (n - 2) / 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;

    /// The decoder's MTF/RLE2 loop, so we test against our own oracle.
    fn decode(sym: &Symbols) -> Vec<u8> {
        let mut mtf = sym.in_use.clone();
        let eob = (sym.in_use.len() + 1) as u16;
        let mut out = Vec::new();
        let mut run: u64 = 0;
        let mut run_bit: u32 = 0;
        for &s in &sym.syms {
            if s <= 1 {
                run += ((s as u64) + 1) << run_bit;
                run_bit += 1;
                continue;
            }
            if run > 0 {
                let b = mtf[0];
                for _ in 0..run { out.push(b); }
                run = 0;
                run_bit = 0;
            }
            if s == eob { break; }
            let nn = (s - 1) as usize;
            let b = mtf[nn];
            mtf.copy_within(0..nn, 1);
            mtf[0] = b;
            out.push(b);
        }
        out
    }

    #[test]
    fn encodes_a_zero_run_bijectively() {
        // "aaaa" -> MTF indices 0,0,0,0 -> run of 4 -> RUNB, RUNA.
        let sym = encode(b"aaaa");
        assert_eq!(sym.in_use, vec![b'a']);
        assert_eq!(sym.syms, vec![RUNB, RUNA, 2]); // 2 == eob for one byte value
        assert_eq!(sym.syms.last(), Some(&2)); // EOB == n_in_use + 1
        assert_eq!(sym.alpha_size(), 3);
        assert_eq!(decode(&sym), b"aaaa");
    }

    #[test]
    fn run_lengths_one_through_ten_use_the_right_digits() {
        let expected: [&[u16]; 10] = [
            &[RUNA], &[RUNB], &[RUNA, RUNA], &[RUNB, RUNA], &[RUNA, RUNB], &[RUNB, RUNB], &[RUNA, RUNA, RUNA], &[RUNB, RUNA, RUNA], &[RUNA, RUNB, RUNA],
            &[RUNB, RUNB, RUNA],
        ];
        for (i, want) in expected.iter().enumerate() {
            let n = i + 1;
            let mut syms = Vec::new();
            let mut zeros = n as u32;
            flush_zero_run(&mut syms, &mut zeros);
            assert_eq!(&syms[..], *want, "run of {}", n);
            // And the digits sum back to n the way the decoder adds them up.
            let total: u64 = syms.iter().enumerate().map(|(k, &s)| ((s as u64) + 1) << k).sum();
            assert_eq!(total, n as u64);
        }
    }

    #[test]
    fn moves_bytes_to_the_front() {
        // in_use = [a, b, c]; "abc" -> indices 0, 1, 2.
        let sym = encode(b"abc");
        assert_eq!(sym.in_use, vec![b'a', b'b', b'c']);
        // zero run of 1 (RUNA), then index 1 -> sym 2, then index 2 -> sym 3, EOB 4.
        assert_eq!(sym.syms, vec![RUNA, 2, 3, 4]);
        assert_eq!(decode(&sym), b"abc");
    }

    #[test]
    fn alternating_bytes_never_hit_index_zero_twice() {
        let sym = encode(b"ababab");
        // a -> 0 (RUNA), b -> 1, a -> 1, b -> 1, a -> 1, b -> 1
        assert_eq!(sym.syms, vec![RUNA, 2, 2, 2, 2, 2, 3]);
        assert_eq!(decode(&sym), b"ababab");
    }

    #[test]
    fn round_trips_varied_blocks() {
        let mut state = 0x2545_f491u32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for case in 1..200usize {
            let n = case * 7;
            let alphabet = 1 + (case % 200) as u32;
            let block: Vec<u8> = (0..n).map(|_| (next() % alphabet) as u8).collect();
            let sym = encode(&block);
            assert_eq!(decode(&sym), block, "case {}", case);
        }
    }

    #[test]
    fn round_trips_a_long_single_byte_run() {
        let block = vec![0xffu8; 100_000];
        let sym = encode(&block);
        assert_eq!(decode(&sym), block);
    }

    #[test]
    fn round_trips_all_256_byte_values() {
        let block: Vec<u8> = (0..=255u8).chain((0..=255u8).rev()).collect();
        let sym = encode(&block);
        assert_eq!(sym.in_use.len(), 256);
        assert_eq!(sym.alpha_size(), 258);
        assert_eq!(sym.syms.last(), Some(&257)); // EOB == n_in_use + 1
        assert_eq!(decode(&sym), block);
    }
}
