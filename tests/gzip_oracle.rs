use std::hint;
#[cfg(not(debug_assertions))]
#[path = "common/benchmark.rs"]
mod benchmark;

use std::io::{Read, Write};

use fbz::{DecodeOptions, DecodeProgress, Error, WriterSink, deflate, gzip::{self, BlockKind}};
use flate2::{Compression, GzBuilder, read::MultiGzDecoder, write::{DeflateEncoder, GzEncoder}};

fn oracle_decompress(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    MultiGzDecoder::new(input).read_to_end(&mut output).unwrap();
    output
}

fn patterned(size: usize) -> Vec<u8> { (0..size).map(|index| ((index * 37 + index / 251) & 255) as u8).collect() }

#[cfg(not(debug_assertions))]
fn random_bytes(size: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(size);
    let mut state = 0x1234_5678_u32;
    for _ in 0..size {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        output.push(state as u8);
    }
    output
}

#[cfg(not(debug_assertions))]
fn random_nibbles(size: usize) -> Vec<u8> {
    let mut bytes = random_bytes(size);
    for byte in &mut bytes { *byte &= 0x0f; }
    bytes
}

fn compress(input: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn gzip_public_api_matches_oracle() {
    let plain = patterned(250_000);
    let encoded = compress(&plain);
    assert_eq!(gzip::decompress(&encoded).unwrap(), oracle_decompress(&encoded));
}

#[test]
#[cfg(not(debug_assertions))]
fn large_stored_gzip_matches_oracle() {
    let plain = random_bytes(20 * 1024 * 1024);
    let encoded = compress(&plain);
    assert!(encoded.len() >= 16 * 1024 * 1024);
    let options = DecodeOptions { threads: 4, memory_limit: 256 * 1024 * 1024, ..DecodeOptions::default() };
    assert_eq!(gzip::decompress_with_options(&encoded, options).unwrap(), oracle_decompress(&encoded));
}

#[test]
#[cfg(not(debug_assertions))]
fn parallel_dynamic_gzip_matches_oracle() {
    let mut plain = random_nibbles(40 * 1024 * 1024);
    let mut encoded = compress(&plain);
    let stored = random_bytes(20 * 1024 * 1024);
    encoded.extend(compress(&stored));
    plain.extend(stored);
    assert!(encoded.len() >= 32 * 1024 * 1024);
    let options = DecodeOptions { threads: 4, memory_limit: 256 * 1024 * 1024, ..DecodeOptions::default() };
    let mut decoded = Vec::new();
    let report = gzip::decompress_to_writer_with_options(&encoded, &mut decoded, options).unwrap();
    assert!(report.speculative_chunks > 0);
    assert_eq!(report.members.len(), 2);
    assert_eq!(decoded, plain);
}

#[cfg(not(debug_assertions))]
fn assert_performance(plain: &[u8], limit: f64) {
    let encoded = compress(plain);
    assert_eq!(gzip::decompress(&encoded).unwrap(), oracle_decompress(&encoded));
    let repeats = 2;
    let fbz_time = benchmark::elapsed(repeats, || { hint::black_box(gzip::decompress(&encoded).unwrap()); });
    let oracle_time = benchmark::elapsed(repeats, || { hint::black_box(oracle_decompress(&encoded)); });
    assert!(fbz_time.as_secs_f64() <= oracle_time.as_secs_f64() * limit, "fbz gzip {fbz_time:?} exceeded {limit}x oracle {oracle_time:?}");
}

#[test]
#[cfg(not(debug_assertions))]
fn gzip_performance_regression_stays_bounded() {
    let plain = patterned(8 * 1024 * 1024);
    assert_performance(&plain, 1.3);
}

#[test]
#[cfg(not(debug_assertions))]
fn gzip_literal_performance_regression_stays_bounded() {
    let plain = random_bytes(4 * 1024 * 1024);
    assert_performance(&plain, 1.3);
}

fn compress_level(data: &[u8], level: Compression) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), level);
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn compress_raw(data: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn raw_deflate_reuses_serial_and_parallel_decoder() {
    let plain = patterned(20 * 1024 * 1024);
    let compressed = compress_raw(&plain);
    for threads in [1, 4] {
        let mut output = Vec::new();
        let mut sink = WriterSink::new(&mut output);
        let report =
            deflate::decompress_to_sink_with_options_and_progress(&compressed, &mut sink, DecodeOptions { threads, ..DecodeOptions::default() }, |_| {})
                .unwrap();
        assert_eq!(output, plain);
        assert_eq!(report.decoded_len, plain.len() as u64);
        assert_eq!(report.crc, gzip::crc32(&plain));
    }
    for size in 0..256 {
        let plain = patterned(size);
        let compressed = compress_raw(&plain);
        let mut output = Vec::new();
        let mut sink = WriterSink::new(&mut output);
        deflate::decompress_to_sink_with_options_and_progress(&compressed, &mut sink, DecodeOptions { threads: 1, ..DecodeOptions::default() }, |_| {})
            .unwrap();
        assert_eq!(output, plain);
    }
}

#[test]
fn decodes_stored_fixed_dynamic_concatenated_headers_and_varied_levels() {
    let mut kinds = Vec::new();
    for (plain, level) in
        [(b"stored bytes".repeat(2_000), Compression::none()), (b"fixed huffman".to_vec(), Compression::fast()), (patterned(350_000), Compression::best())] {
            let compressed = compress_level(&plain, level);
            let mut output = Vec::new();
            let report = gzip::decompress_to_writer(&compressed, &mut output).unwrap();
            assert_eq!(output, plain);
            kinds.extend(report.blocks.into_iter().map(|block| block.kind));
        }
    assert!(kinds.contains(&BlockKind::Stored) && kinds.contains(&BlockKind::FixedHuffman) && kinds.contains(&BlockKind::DynamicHuffman));

    let first = patterned(180_000);
    let second = b"second member".repeat(4_000);
    let mut concat = compress_level(&first, Compression::fast());
    concat.extend_from_slice(&compress_level(&second, Compression::best()));
    let mut expected = first;
    expected.extend_from_slice(&second);
    let mut output = Vec::new();
    let mut reports = Vec::new();
    let report = gzip::decompress_to_writer_with_progress(&concat, &mut output, |progress| reports.push(progress)).unwrap();
    assert_eq!(output, expected);
    assert_eq!(report.members.len(), 2);
    assert_eq!(reports.last(), Some(&DecodeProgress { compressed_bytes: concat.len() as u64, decoded_bytes: expected.len() as u64 }));
    assert!(reports.windows(2).all(|pair| pair[0].compressed_bytes <= pair[1].compressed_bytes && pair[0].decoded_bytes <= pair[1].decoded_bytes));

    let plain = b"header metadata";
    let mut encoder = GzBuilder::new().mtime(123456).operating_system(3).write(Vec::new(), Compression::fast());
    encoder.write_all(plain).unwrap();
    let base = encoder.finish().unwrap();
    let mut with_hdr = base[..10].to_vec();
    with_hdr[3] = 0x1e;
    with_hdr.extend_from_slice(&(3_u16).to_le_bytes());
    with_hdr.extend_from_slice(b"xyz");
    with_hdr.extend_from_slice(b"name.txt\0comment\0");
    let header_crc = gzip::crc32(&with_hdr) as u16;
    with_hdr.extend_from_slice(&header_crc.to_le_bytes());
    with_hdr.extend_from_slice(&base[10..]);
    let mut output = Vec::new();
    let report = gzip::decompress_to_writer(&with_hdr, &mut output).unwrap();
    assert_eq!(output, plain);
    assert_eq!(report.members[0].mtime, 123456);
    assert_eq!(report.members[0].name.as_deref(), Some(b"name.txt".as_slice()));
    assert_eq!(report.members[0].comment.as_deref(), Some(b"comment".as_slice()));

    let mut random = Vec::with_capacity(70_000);
    let mut state = 0x1234_5678_u32;
    for _ in 0..70_000 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        random.push(state as u8);
    }
    for plain in [Vec::new(), vec![0], vec![0; 32_769], b"abc".repeat(20_000), patterned(70_000), random] {
        for level in [Compression::none(), Compression::fast(), Compression::new(6), Compression::best()] {
            assert_eq!(gzip::decompress(&compress_level(&plain, level)).unwrap(), plain);
        }
    }
}

#[test]
fn rejects_corrupt_and_malformed_gzip_without_panicking() {
    let compressed = compress_level(&patterned(20_000), Compression::best());
    let mut reserved = compressed.clone();
    reserved[3] |= 0x20;
    assert!(matches!(gzip::decompress(&reserved), Err(Error::InvalidGzip(_))));

    let mut payload = compressed.clone();
    payload[12] ^= 1;
    assert!(gzip::decompress(&payload).is_err());

    let mut crc = compressed.clone();
    let crc_byte = crc.len() - 8;
    crc[crc_byte] ^= 1;
    assert!(matches!(gzip::decompress(&crc), Err(Error::InvalidGzip(message)) if message.contains("CRC32 mismatch")));

    let mut size = compressed;
    let size_byte = size.len() - 4;
    size[size_byte] ^= 1;
    assert!(matches!(gzip::decompress(&size), Err(Error::InvalidGzip(message)) if message.contains("ISIZE mismatch")));

    let valid = compress_level(&patterned(4_000), Compression::best());
    for end in 0..valid.len() { assert!(gzip::decompress(&valid[..end]).is_err()); }
    for byte in 0..valid.len().min(64) {
        let mut damaged = valid.clone();
        damaged[byte] ^= 0x5a;
        let _ = gzip::decompress(&damaged);
    }
}
