use std::io;

use crate::{BitReader, Error, Result};
use rayon::{ThreadPool, prelude::*};

pub const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
pub const END_MAGIC: u64 = 0x1772_4538_5090;
const MAGIC_BITS: u8 = 48;
const MAGIC_MASK: u64 = (1_u64 << MAGIC_BITS) - 1;
const WINDOW_MASK: u64 = (1_u64 << 56) - 1;
const SCAN_CHUNK: usize = 1 << 20;
const BLOCK_PREFIX: [u8; 256] = prefix_table(BLOCK_MAGIC);
const END_PREFIX: [u8; 256] = prefix_table(END_MAGIC);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamHeaderCandidate { pub byte_offset: u64, pub block_size_100k: u8 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockCandidate {
    pub bit_offset: u64,
    pub expected_crc: u32,
    pub randomized: bool,
    pub orig_ptr: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndCandidate { pub bit_offset: u64, pub expected_stream_crc: u32 }

/// Candidate stream headers and bit-level block markers found in a bzip2 input.
///
/// This is an intentionally cheap structural scan, not full validation: marker
/// bit patterns can occur in compressed payloads, so entries after the first
/// stream header are candidates until a decoder validates the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanResult { pub streams: Vec<StreamHeaderCandidate>, pub blocks: Vec<BlockCandidate>, pub stream_ends: Vec<EndCandidate> }

pub fn scan(data: &[u8]) -> Result<ScanResult> { if !is_stream_header(data, 0) { return Err(Error::InvalidStreamHeader); } Ok(scan_range(data, 0, data.len())) }

pub(crate) fn scan_with_pool(data: &[u8], pool: Option<&ThreadPool>, mut cancelled: impl FnMut() -> bool) -> Result<ScanResult> {
    if !is_stream_header(data, 0) { return Err(Error::InvalidStreamHeader); }
    let cancellation_error = || Error::Io(io::Error::new(io::ErrorKind::BrokenPipe, "output reader stopped reading"));
    if cancelled() { return Err(cancellation_error()); }
    let Some(pool) = pool.filter(|_| data.len() > SCAN_CHUNK) else {
        let result = scan_range(data, 0, data.len());
        return if cancelled() { Err(cancellation_error()) } else { Ok(result) };
    };
    let chunks = data.len().div_ceil(SCAN_CHUNK);
    let mut result = ScanResult { streams: Vec::new(), blocks: Vec::new(), stream_ends: Vec::new() };
    let wave = pool.current_num_threads().max(1);
    for first in (0..chunks).step_by(wave) {
        if cancelled() { return Err(cancellation_error()); }
        let end = first.saturating_add(wave).min(chunks);
        let partial: Vec<_> = pool
            .install(|| (first..end).into_par_iter().map(|chunk| scan_range(data, chunk * SCAN_CHUNK, ((chunk + 1) * SCAN_CHUNK).min(data.len()))).collect());
        for mut chunk in partial {
            result.streams.append(&mut chunk.streams);
            result.blocks.append(&mut chunk.blocks);
            result.stream_ends.append(&mut chunk.stream_ends);
        }
    }
    if cancelled() { Err(cancellation_error()) } else { Ok(result) }
}

fn scan_range(data: &[u8], start: usize, end: usize) -> ScanResult {
    let streams = (start..end)
        .filter(|&offset| is_stream_header(data, offset))
        .map(|offset| StreamHeaderCandidate { byte_offset: offset as u64, block_size_100k: data[offset + 3] - b'0' })
        .collect();
    let mut blocks = Vec::new();
    let mut stream_ends = Vec::new();

    if data.len() < 7 || start >= end { return ScanResult { streams, blocks, stream_ends }; }

    let mut window = (start..start + 7).fold(0_u64, |word, offset| (word << 8) | u64::from(byte_at(data, offset)));
    for byte_offset in start..end {
        let mut shifts = BLOCK_PREFIX[(window >> 48) as usize] | END_PREFIX[(window >> 48) as usize];
        while shifts != 0 {
            let shift = shifts.trailing_zeros();
            shifts &= shifts - 1;
            let marker = (window >> (8 - shift)) & MAGIC_MASK;
            let bit_offset = byte_offset as u64 * 8 + u64::from(shift);
            if marker == BLOCK_MAGIC { if let Some(block) = parse_block(data, bit_offset) { blocks.push(block); } } else if marker == END_MAGIC
                && let Some(end) = parse_end(data, bit_offset)
            { stream_ends.push(end); }
        }
        window = ((window << 8) & WINDOW_MASK) | u64::from(byte_at(data, byte_offset + 7));
    }

    ScanResult { streams, blocks, stream_ends }
}

const fn prefix_table(magic: u64) -> [u8; 256] {
    let mut table = [0_u8; 256];
    let mut byte = 0_usize;
    while byte < 256 {
        let mut shift = 0_u32;
        while shift < 8 {
            let wanted = (magic >> (40 + shift)) as u8;
            let kept = ((1_u16 << (8 - shift)) - 1) as u8;
            if byte as u8 & kept == wanted { table[byte] |= 1 << shift; }
            shift += 1;
        }
        byte += 1;
    }
    table
}

fn byte_at(data: &[u8], offset: usize) -> u8 { data.get(offset).copied().unwrap_or(0) }

fn is_stream_header(data: &[u8], offset: usize) -> bool { data.get(offset..offset + 3) == Some(b"BZh") && matches!(data.get(offset + 3), Some(b'1'..=b'9')) }

fn parse_block(data: &[u8], bit_offset: u64) -> Option<BlockCandidate> {
    let mut reader = BitReader::at(data, bit_offset + u64::from(MAGIC_BITS)).ok()?;
    Some(BlockCandidate {
        bit_offset,
        expected_crc: reader.read_bits(32).ok()? as u32,
        randomized: reader.read_bit().ok()?,
        orig_ptr: reader.read_bits(24).ok()? as u32,
    })
}

fn parse_end(data: &[u8], bit_offset: u64) -> Option<EndCandidate> {
    let mut reader = BitReader::at(data, bit_offset + u64::from(MAGIC_BITS)).ok()?;
    Some(EndCandidate { bit_offset, expected_stream_crc: reader.read_bits(32).ok()? as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_bits(target: &mut Vec<bool>, value: u64, count: usize) { target.extend((0..count).rev().map(|shift| value >> shift & 1 != 0)); }

    fn pack(bits: &[bool]) -> Vec<u8> {
        bits.chunks(8).map(|chunk| chunk.iter().fold(0, |byte, &bit| (byte << 1) | u8::from(bit)) << (8 - chunk.len())).collect()
    }

    #[test]
    fn finds_markers_at_every_bit_alignment() {
        for prefix in 0..8 {
            let mut bits = Vec::new();
            for byte in b"BZh9" { append_bits(&mut bits, u64::from(*byte), 8); }
            bits.extend(std::iter::repeat_n(false, prefix));
            append_bits(&mut bits, BLOCK_MAGIC, 48);
            append_bits(&mut bits, 0x1234_5678, 32);
            append_bits(&mut bits, 1, 1);
            append_bits(&mut bits, 0x00ab_cdef, 24);
            let data = pack(&bits);
            let result = scan(&data).unwrap();
            let [block] = result.blocks.as_slice() else { panic!("expected one block, found {:?}", result.blocks); };
            assert_eq!(block.bit_offset, 32 + prefix as u64);
            assert_eq!(block.expected_crc, 0x1234_5678);
            assert!(block.randomized);
            assert_eq!(block.orig_ptr, 0x00ab_cdef);
        }
    }

    #[test]
    fn rejects_non_bzip_input() { assert!(matches!(scan(b"not bzip2"), Err(Error::InvalidStreamHeader))); }
}
