use crate::{Result, decoder};

pub const MAX_ENCODED_BLOCK: usize = 900_000;
pub const MAX_DECODED_BLOCK: usize = MAX_ENCODED_BLOCK / 5 * 259 + 4;

/// Decode and CRC-validate one bzip2 block bounded by two exact markers.
pub fn decode_block(data: &[u8], start_bit: u64, end_bit: u64, level: u8, expected_crc: u32) -> Result<Vec<u8>> {
    decoder::decode_block(data, start_bit, end_bit, level, expected_crc)
}
