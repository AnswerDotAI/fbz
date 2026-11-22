use std::{io::{Read, Seek, SeekFrom}, thread};

use crabz2::{Level, compress};
use fbz::{
    BLOCK_MAGIC, BitReader, DecodeOptions, DecodeProgress, Error, Index, IndexedReader, MAX_DECODED_BLOCK, OutputSink, build_index, bz2_crc32,
    combine_stream_crc, decode_block, decode_to_writer, decompress_bzip2, decompress_bzip2_to_writer_with_progress, output_pipe, scan,
};

fn patterned(size: usize) -> Vec<u8> { (0..size).map(|index| ((index * 37 + index / 251) & 255) as u8).collect() }

fn reference_bits(data: &[u8], offset: usize, count: usize) -> u64 {
    (offset..offset + count).fold(0, |result, bit| (result << 1) | u64::from((data[bit / 8] >> (7 - bit % 8)) & 1))
}

fn append_bits(target: &mut Vec<bool>, value: u64, count: usize) { target.extend((0..count).rev().map(|shift| value >> shift & 1 != 0)); }

fn pack(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8).map(|chunk| chunk.iter().fold(0, |byte, &bit| (byte << 1) | u8::from(bit)) << (8 - chunk.len())).collect()
}

#[test]
fn bitreader_reads_every_alignment_width_and_single_bit() {
    let data = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x55];
    for offset in 0..8 {
        for count in 0..=64.min(data.len() * 8 - offset) {
            let mut reader = BitReader::at(&data, offset as u64).unwrap();
            assert_eq!(reader.read_bits(count as u8).unwrap(), reference_bits(&data, offset, count));
            assert_eq!(reader.position(), (offset + count) as u64);
        }
    }
    let mut reader = BitReader::new(&[0b1010_0001]);
    let bits: Vec<_> = (0..8).map(|_| reader.read_bit().unwrap()).collect();
    assert_eq!(bits, [true, false, true, false, false, false, false, true]);
    assert!(matches!(reader.read_bit(), Err(Error::UnexpectedEof { .. })));
}

#[test]
fn crc_known_values_and_stream_combine() {
    assert_eq!(bz2_crc32(b""), 0);
    assert_eq!(bz2_crc32(b"123456789"), 0xfc89_1918);
    assert_eq!(combine_stream_crc(0x8000_0001, 0x1234_5678), 0x1234_567b);
}

#[test]
fn scan_and_decode_block_handle_all_bit_alignments() {
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
    assert!(matches!(scan(b"not bzip2"), Err(Error::InvalidStreamHeader)));

    const HELLO: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x71, 0x1c, 0x50, 0xc0, 0x00, 0x00, 0x03, 0xd9, 0x80, 0x00, 0x10, 0x40, 0x00, 0x10, 0x00,
        0x3a, 0x44, 0x90, 0x10, 0x20, 0x00, 0x31, 0x03, 0x40, 0xd0, 0x29, 0x80, 0x1e, 0xa2, 0xe0, 0x4c, 0xed, 0x69, 0xe0, 0xe1, 0x77, 0x24, 0x53, 0x85, 0x09,
        0x07, 0x11, 0xc5, 0x0c, 0x00,
    ];
    let scanned = scan(HELLO).unwrap();
    let block = &scanned.blocks[0];
    let end = &scanned.stream_ends[0];
    let out = decode_block(HELLO, block.bit_offset, end.bit_offset, 9, block.expected_crc).unwrap();
    assert_eq!(out, b"hello crabz2\n");
    assert_eq!(combine_stream_crc(0, block.expected_crc), end.expected_stream_crc);
}

#[test]
fn decodes_multiblock_progress_concatenated_bounded_and_rejects_crc_corruption() {
    let plain = patterned(350_000);
    let compressed = compress(&plain, Level::FASTEST);
    for threads in [1, 2, 4, 0] {
        let options = DecodeOptions { threads, ..DecodeOptions::default() };
        assert_eq!(decompress_bzip2(&compressed, options).unwrap(), plain);
    }
    for threads in [1, 4] {
        let options = DecodeOptions { threads, ..DecodeOptions::default() };
        let mut output = Vec::new();
        let mut reports = Vec::new();
        decompress_bzip2_to_writer_with_progress(&compressed, &mut output, options, |progress| reports.push(progress)).unwrap();
        assert_eq!(output, plain);
        assert!(reports.windows(2).all(|pair| pair[0].compressed_bytes <= pair[1].compressed_bytes && pair[0].decoded_bytes <= pair[1].decoded_bytes));
        assert_eq!(reports.last(), Some(&DecodeProgress { compressed_bytes: compressed.len() as u64, decoded_bytes: plain.len() as u64 }));
    }

    let first = patterned(180_000);
    let second = patterned(70_000);
    let mut concat = compress(&first, Level::FASTEST);
    concat.extend_from_slice(&compress(&second, Level::BEST));
    let mut expected = first;
    expected.extend_from_slice(&second);
    let mut output = Vec::new();
    let index = decode_to_writer(&concat, &mut output, DecodeOptions::default()).unwrap();
    assert_eq!(output, expected);
    assert_eq!(index.streams.len(), 2);
    assert_eq!(index.decoded_len, expected.len() as u64);
    assert!(index.blocks.len() >= 3);

    let mut short_streams = Vec::new();
    let mut short_expected = Vec::new();
    for stream in 0..32 {
        let piece = patterned(4_000 + stream * 31);
        short_streams.extend_from_slice(&compress(&piece, Level::BEST));
        short_expected.extend_from_slice(&piece);
    }
    let bounded = DecodeOptions { threads: 4, memory_limit: MAX_DECODED_BLOCK * 2, ..DecodeOptions::default() };
    assert_eq!(decompress_bzip2(&short_streams, bounded).unwrap(), short_expected);

    let mut corrupt = compress(b"integrity matters", Level::BEST);
    let last = corrupt.len() - 2;
    corrupt[last] ^= 1;
    assert!(matches!(decompress_bzip2(&corrupt, DecodeOptions::default()), Err(Error::Decode { .. })));
}

#[test]
fn index_roundtrip_tamper_and_indexed_reader_seeking() {
    let source = compress(b"", Level::FASTEST);
    let index = build_index(&source, DecodeOptions::default()).unwrap();
    let encoded = index.to_bytes();
    assert_eq!(Index::from_bytes(&encoded, &source).unwrap(), index);
    assert!(matches!(Index::from_bytes(&encoded, b"BZh8"), Err(Error::InvalidIndex(_))));
    let mut tampered = encoded;
    tampered[0] ^= 1;
    assert!(matches!(Index::from_bytes(&tampered, &source), Err(Error::InvalidIndex(_))));

    let plain: Vec<_> = (0..350_000).map(|index| ((index * 17 + index / 101) & 255) as u8).collect();
    let compressed = compress(&plain, Level::FASTEST);
    let mut reader = IndexedReader::from_bytes(compressed.clone(), DecodeOptions { threads: 2, ..DecodeOptions::default() }).unwrap();
    assert!(reader.index().blocks.len() >= 3);
    for &(position, count) in &[(0, 17), (99_990, 40), (170_123, 8192), (349_990, 20)] {
        reader.seek(SeekFrom::Start(position as u64)).unwrap();
        let mut actual = vec![0; count];
        let got = reader.read(&mut actual).unwrap();
        assert_eq!(&actual[..got], &plain[position..(position + count).min(plain.len())]);
    }
    assert_eq!(reader.seek(SeekFrom::End(-10)).unwrap(), plain.len() as u64 - 10);

    let encoded_index = reader.index().to_bytes();
    let mut loaded = IndexedReader::from_bytes_with_index(compressed, &encoded_index, 1024 * 1024).unwrap();
    let mut actual = Vec::new();
    loaded.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, plain);
}

#[test]
fn output_pipe_transfers_owned_slices_across_threads() {
    let (mut writer, mut reader) = output_pipe();
    let worker = thread::spawn(move || writer.write_owned_from(vec![1, 2, 3, 4], 1).unwrap());
    let mut output = [0; 3];
    assert_eq!(reader.read(&mut output).unwrap(), 3);
    assert_eq!(output, [2, 3, 4]);
    worker.join().unwrap();
}
