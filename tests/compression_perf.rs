mod support;

use std::{io::Write, time::Instant};

use fbz::{EncodeOptions, gzip};
use flate2::{Compression, write::GzEncoder};
use lz4_flex::frame::FrameEncoder as Lz4Encoder;

fn flate2_gzip(input: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

fn lz4_flex(input: &[u8]) -> Vec<u8> {
    let mut encoder = Lz4Encoder::new(Vec::new());
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

#[test]
#[ignore = "local single-run gzip compression comparison"]
fn gzip_compression_comparison() {
    let input = support::simplewiki_prefix();
    let options = EncodeOptions { threads: 0, memory_limit: 1024 * 1024 * 1024, level: Some(6) };

    let start = Instant::now();
    let ours = gzip::compress_with_options(&input, options).unwrap();
    let ours_time = start.elapsed();

    let start = Instant::now();
    let oracle = flate2_gzip(&input);
    let oracle_time = start.elapsed();

    assert_eq!(gzip::decompress(&ours).unwrap(), input);
    eprintln!(
        "gzip compression: fbz {:.3?} {} bytes; flate2 {:.3?} {} bytes; speed {:.2}x, size {:.3}x", ours_time, ours.len(), oracle_time, oracle.len(),
        oracle_time.as_secs_f64() / ours_time.as_secs_f64(), ours.len() as f64 / oracle.len() as f64,
    );
}

#[test]
#[ignore = "local single-run LZ4 compression comparison"]
fn lz4_compression_comparison() {
    let input = support::simplewiki_prefix();
    let options = EncodeOptions { threads: 0, memory_limit: 1024 * 1024 * 1024, level: Some(6) };

    let start = Instant::now();
    let ours = fbz::lz4::compress(&input, options).unwrap();
    let ours_time = start.elapsed();

    let start = Instant::now();
    let oracle = lz4_flex(&input);
    let oracle_time = start.elapsed();

    assert_eq!(fbz::lz4::decompress(&ours).unwrap(), input);
    eprintln!(
        "LZ4 compression: fbz {:.3?} {} bytes; lz4_flex {:.3?} {} bytes; speed {:.2}x, size {:.3}x", ours_time, ours.len(), oracle_time, oracle.len(),
        oracle_time.as_secs_f64() / ours_time.as_secs_f64(), ours.len() as f64 / oracle.len() as f64,
    );
}

#[test]
#[ignore = "local single-run LZ4 compression thread diagnostic"]
fn lz4_compression_thread_sweep() {
    let input = support::simplewiki_prefix();
    for threads in [1, 2, 4, 8, 12, 18] {
        let options = EncodeOptions { threads, memory_limit: 1024 * 1024 * 1024, level: None };
        let start = Instant::now();
        let encoded = fbz::lz4::compress(&input, options).unwrap();
        eprintln!("LZ4 compression {threads:>2} workers: {:.3?}, {} bytes", start.elapsed(), encoded.len());
    }
}
