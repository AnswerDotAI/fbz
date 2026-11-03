use std::hint;
#[cfg(not(debug_assertions))]
#[path = "common/benchmark.rs"]
mod benchmark;

use std::io::Read;

#[cfg(not(debug_assertions))]
use fbz::DecodeOptions;
use fbz::gzip;
use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};

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
