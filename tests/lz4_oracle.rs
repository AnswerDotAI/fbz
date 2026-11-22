use std::io::{Read, Write};

use fbz::{DecodeOptions, Error, MAX_DECODED_BLOCK, lz4};
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

#[test]
fn handles_concatenated_skippable_corruption_and_block_overflow() {
    let first = encode(b"first", BlockSize::Max64KB, BlockMode::Independent, true, true, true);
    let second = encode(b"second", BlockSize::Max64KB, BlockMode::Linked, false, false, true);
    let mut encoded = first;
    encoded.extend_from_slice(&0x184d_2a50_u32.to_le_bytes());
    encoded.extend_from_slice(&3_u32.to_le_bytes());
    encoded.extend_from_slice(b"xyz");
    encoded.extend_from_slice(&second);
    let mut output = Vec::new();
    let report = lz4::decompress_to_writer_with_options(&encoded, &mut output, DecodeOptions { threads: 4, ..DecodeOptions::default() }).unwrap();
    assert_eq!(output, b"firstsecond");
    assert_eq!(report.frames.len(), 2);
    assert_eq!(report.decoded_len, 11);

    let data = b"checksum coverage".repeat(10_000);
    let mut corrupt = encode(&data, BlockSize::Max64KB, BlockMode::Independent, true, true, true);
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 1;
    assert!(matches!(lz4::decompress(&corrupt), Err(Error::InvalidLz4(_))));
    assert!(matches!(lz4::decompress_block(&[0x1f, 1, 0, 255, 255, 255, 255], &[], 32), Err(Error::InvalidLz4(_))));
}

#[test]
fn overlapping_matches_repeat_the_requested_history() {
    for initial_len in 1..20usize {
        let seed: Vec<u8> = (0..initial_len as u8).collect();
        for distance in 1..=initial_len {
            for length in 4..80usize {
                let mut block = Vec::new();
                let lit_nib = initial_len.min(15) as u8;
                let match_extra = length - 4;
                let match_nib = match_extra.min(15) as u8;
                block.push((lit_nib << 4) | match_nib);
                if initial_len >= 15 { block.push((initial_len - 15) as u8); }
                block.extend_from_slice(&seed);
                block.extend_from_slice(&(distance as u16).to_le_bytes());
                if match_extra >= 15 { block.push((match_extra - 15) as u8); }
                let mut expected = seed.clone();
                for _ in 0..length { expected.push(expected[expected.len() - distance]); }
                let actual = lz4::decompress_block(&block, &[], initial_len + length).unwrap();
                assert_eq!(actual, expected, "initial={initial_len}, distance={distance}, length={length}");
            }
        }
    }
}
