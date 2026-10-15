use std::io::{Read, Write};

use fbz::{DecodeOptions, MAX_DECODED_BLOCK, lz4};
use lz4_flex::frame::{BlockMode, BlockSize, FrameDecoder, FrameEncoder, FrameInfo};

fn encode(data: &[u8], block_size: BlockSize, block_mode: BlockMode, block_checksum: bool, content_checksum: bool, content_size: bool) -> Vec<u8> {
    let info = FrameInfo::new().block_size(block_size).block_mode(block_mode).block_checksums(block_checksum).content_checksum(content_checksum)
        .content_size(content_size.then_some(data.len() as u64));
    let mut encoder = FrameEncoder::with_frame_info(info, Vec::new());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn oracle_decode(encoded: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    FrameDecoder::new(encoded).read_to_end(&mut output).unwrap();
    output
}

fn pseudorandom(length: u32) -> Vec<u8> {
    (0..length).scan(0x9e37_79b9_u32, |state, _| {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        Some(*state as u8)
    }).collect()
}

fn assert_matches(data: &[u8], encoded: &[u8], options: DecodeOptions) {
    assert_eq!(lz4::decompress_with_options(encoded, options).unwrap(), data);
    assert_eq!(oracle_decode(encoded), data);
}

#[test]
fn frame_matrix_matches_lz4_flex() {
    let options = DecodeOptions { threads: 4, ..DecodeOptions::default() };
    let matrix_data = b"frame descriptor and block boundary coverage ".repeat(2_500);
    for block_size in [BlockSize::Max64KB, BlockSize::Max256KB, BlockSize::Max1MB, BlockSize::Max4MB] {
        for block_mode in [BlockMode::Independent, BlockMode::Linked] {
            for block_checksum in [false, true] {
                for content_checksum in [false, true] {
                    for content_size in [false, true] {
                        let encoded = encode(&matrix_data, block_size, block_mode, block_checksum, content_checksum, content_size);
                        assert_matches(&matrix_data, &encoded, options);
                    }
                }
            }
        }
    }

    let shapes = [
        Vec::new(), b"short LZ4 payload".to_vec(), b"repeated dictionary material ".repeat(2_000), (0_u8..=255).collect::<Vec<_>>().repeat(200),
        pseudorandom(50_000),
    ];
    for data in shapes {
        let encoded = encode(&data, BlockSize::Max64KB, BlockMode::Independent, true, true, true);
        assert_matches(&data, &encoded, options);
    }
}

#[test]
fn incompressible_multiblock_input_exercises_parallel_scheduler() {
    let data = pseudorandom(1_200_000);
    let encoded = encode(&data, BlockSize::Max64KB, BlockMode::Independent, true, true, true);
    assert!(encoded.len() > 1024 * 1024);
    assert_matches(&data, &encoded, DecodeOptions { threads: 4, ..DecodeOptions::default() });
}

#[test]
fn a_small_speculative_budget_falls_back_to_incremental_serial_blocks() {
    let data = b"large declared blocks do not require speculative memory ".repeat(120_000);
    let encoded = encode(&data, BlockSize::Max4MB, BlockMode::Independent, true, true, true);
    assert_matches(&data, &encoded, DecodeOptions { threads: 4, memory_limit: MAX_DECODED_BLOCK, ..DecodeOptions::default() });
}
