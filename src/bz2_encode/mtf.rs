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
