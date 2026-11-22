//! Burrows–Wheeler transform via a from-scratch SA-IS suffix array.
//!
//! bzip2 sorts the *rotations* of the block, not sentinel-terminated suffixes.
//! We get rotation order out of a suffix sorter by running SA-IS over the
//! doubled block `block ‖ block` and keeping the suffixes that start below `n`:
//! the first `n` characters of such a suffix are exactly the corresponding
//! rotation, so suffix order and rotation order agree wherever the rotations
//! differ. Where two rotations are *equal* the doubled-string order is decided
//! by the trailing remainder, but equal rotations necessarily share the same
//! preceding byte, so the last column — and therefore the transform — is
//! unaffected.

use core::cmp::Ordering;
use std::vec;
use std::vec::Vec;

/// Sentinel for "no entry yet" inside the suffix array under construction.
const EMPTY: u32 = u32::MAX;

/// Burrows–Wheeler transform of `block`.
///
/// Returns the last column of the sorted rotation matrix and the row index of
/// the unrotated block (bzip2's `origPtr`). `block` must not be empty.
pub fn transform(block: &[u8]) -> (Vec<u8>, usize) {
    let n = block.len();
    assert!(n > 0, "BWT of an empty block");

    if n == 1 { return (vec![block[0]], 0); }

    // Doubled block, shifted up by one so 0 can serve as the unique sentinel.
    let mut doubled = Vec::with_capacity(2 * n + 1);
    for _ in 0..2 { doubled.extend(block.iter().map(|&b| b as u32 + 1)); }
    doubled.push(0);

    let sa = sais(&doubled, 257);

    let mut last = Vec::with_capacity(n);
    let mut orig_ptr = 0usize;
    for &suffix in &sa {
        let p = suffix as usize;
        if p >= n { continue; }
        if p == 0 { orig_ptr = last.len(); }
        last.push(block[(p + n - 1) % n]);
    }
    debug_assert_eq!(last.len(), n);

    (last, orig_ptr)
}

/// Suffix array of `s` by SA-IS. `s` must end with a unique smallest value `0`,
/// and every value must be below `k`.
fn sais(s: &[u32], k: usize) -> Vec<u32> {
    let n = s.len();
    let mut sa = vec![EMPTY; n];
    if n == 1 { sa[0] = 0; return sa; }

    let types = classify(s);
    let counts = counts(s, k);

    // Pass 1: seed the LMS suffixes in arbitrary order and induce, which sorts
    // the LMS *substrings* even though the suffixes themselves are not yet.
    let mut bucket = bucket_ends(&counts);
    for i in (1..n).rev() {
        if is_lms(&types, i) {
            let c = s[i] as usize;
            bucket[c] -= 1;
            sa[bucket[c] as usize] = i as u32;
        }
    }
    induce(s, &mut sa, &types, &counts);

    // Name the sorted LMS substrings.
    let lms_sorted: Vec<u32> = sa.iter().copied().filter(|&p| p != EMPTY && p > 0 && is_lms(&types, p as usize)).collect();
    let n1 = lms_sorted.len();

    let mut names = vec![EMPTY; n / 2 + 1];
    let mut name = 0u32;
    let mut prev: Option<usize> = None;
    for &p in &lms_sorted {
        let p = p as usize;
        let fresh = match prev { None => true, Some(q) => !lms_substr_eq(s, &types, p, q) };
        if fresh {
            name += 1;
            prev = Some(p);
        }
        names[p / 2] = name - 1;
    }

    // The reduced string: LMS names in order of position. Its last symbol is
    // the sentinel's name, which is 0 and unique, so the recursion's
    // precondition holds.
    let lms_pos: Vec<u32> = (1..n).filter(|&i| is_lms(&types, i)).map(|i| i as u32).collect();
    debug_assert_eq!(lms_pos.len(), n1);
    let reduced: Vec<u32> = lms_pos.iter().map(|&p| names[p as usize / 2]).collect();

    let sub_sa = if (name as usize) < n1 { sais(&reduced, name as usize) } else {
        // All names distinct: the suffix array is just the inverse permutation.
        let mut sub = vec![0u32; n1];
        for (i, &c) in reduced.iter().enumerate() { sub[c as usize] = i as u32; }
        sub
    };

    // Pass 2: seed the LMS suffixes in their true order, induce the rest.
    for slot in sa.iter_mut() { *slot = EMPTY; }
    let mut bucket = bucket_ends(&counts);
    for i in (0..n1).rev() {
        let p = lms_pos[sub_sa[i] as usize] as usize;
        let c = s[p] as usize;
        bucket[c] -= 1;
        sa[bucket[c] as usize] = p as u32;
    }
    induce(s, &mut sa, &types, &counts);

    sa
}

/// `true` marks an S-type position (its suffix is smaller than the next one's).
fn classify(s: &[u32]) -> Vec<bool> {
    let n = s.len();
    let mut types = vec![false; n];
    types[n - 1] = true;
    for i in (0..n - 1).rev() {
        types[i] = match s[i].cmp(&s[i + 1]) { Ordering::Less => true, Ordering::Greater => false, Ordering::Equal => types[i + 1] };
    }
    types
}

#[inline]
fn is_lms(types: &[bool], i: usize) -> bool { i > 0 && types[i] && !types[i - 1] }

fn counts(s: &[u32], k: usize) -> Vec<u32> {
    let mut counts = vec![0u32; k];
    for &c in s { counts[c as usize] += 1; }
    counts
}

fn bucket_starts(counts: &[u32]) -> Vec<u32> {
    let mut starts = Vec::with_capacity(counts.len());
    let mut sum = 0u32;
    for &c in counts {
        starts.push(sum);
        sum += c;
    }
    starts
}

fn bucket_ends(counts: &[u32]) -> Vec<u32> {
    let mut ends = Vec::with_capacity(counts.len());
    let mut sum = 0u32;
    for &c in counts {
        sum += c;
        ends.push(sum);
    }
    ends
}

/// Induced sorting: L-type suffixes left to right, then S-type right to left.
fn induce(s: &[u32], sa: &mut [u32], types: &[bool], counts: &[u32]) {
    let n = s.len();

    let mut bucket = bucket_starts(counts);
    for i in 0..n {
        let p = sa[i];
        if p != EMPTY && p > 0 {
            let j = (p - 1) as usize;
            if !types[j] {
                let c = s[j] as usize;
                sa[bucket[c] as usize] = j as u32;
                bucket[c] += 1;
            }
        }
    }

    let mut bucket = bucket_ends(counts);
    for i in (0..n).rev() {
        let p = sa[i];
        if p != EMPTY && p > 0 {
            let j = (p - 1) as usize;
            if types[j] {
                let c = s[j] as usize;
                bucket[c] -= 1;
                sa[bucket[c] as usize] = j as u32;
            }
        }
    }
}

/// Compare the LMS substrings starting at `p` and `q` (each runs to and
/// including the next LMS position).
fn lms_substr_eq(s: &[u32], types: &[bool], p: usize, q: usize) -> bool {
    let n = s.len();
    if p == n - 1 || q == n - 1 { return p == q; }
    let mut d = 0usize;
    loop {
        if p + d >= n || q + d >= n { return false; }
        let p_lms = d > 0 && is_lms(types, p + d);
        let q_lms = d > 0 && is_lms(types, q + d);
        if p_lms && q_lms { return true; }
        if p_lms != q_lms { return false; }
        if s[p + d] != s[q + d] || types[p + d] != types[q + d] { return false; }
        d += 1;
    }
}
