#[cfg(not(debug_assertions))]
#[path = "common/benchmark.rs"]
mod benchmark;

use std::{ffi::{c_char, c_uint}, fs, path::{Path, PathBuf}, ptr};

use crabz2::{Level, compress};
use fbz::{DecodeOptions, decompress};
use libbz2_rs_sys::{BZ_OK, BZ_STREAM_END, BZ2_bzDecompress, BZ2_bzDecompressEnd, BZ2_bzDecompressInit, bz_stream};

const CHUNK: usize = 64 * 1024;

fn corpus_files(extension: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut files = Vec::new();
    for source in ["go", "lbzip2"] {
        for entry in fs::read_dir(root.join(source)).unwrap() {
            let path = entry.unwrap().path();
            if path.to_string_lossy().ends_with(extension) { files.push(path) }
        }
    }
    files.sort();
    files
}

/// Decode every concatenated stream through the low-level API so the oracle
/// has the same whole-file semantics as fbz.
fn oracle_decompress(input: &[u8]) -> Result<Vec<u8>, i32> {
    if input.is_empty() { return Err(libbz2_rs_sys::BZ_DATA_ERROR_MAGIC); }
    let mut decoded = Vec::new();
    let mut input_offset = 0;

    while input_offset < input.len() {
        let remaining = &input[input_offset..];
        let input_len = c_uint::try_from(remaining.len()).unwrap();
        let mut stream = bz_stream { next_in: remaining.as_ptr().cast::<c_char>(), avail_in: input_len, total_in_lo32: 0, total_in_hi32: 0,
            next_out: ptr::null_mut(), avail_out: 0, total_out_lo32: 0, total_out_hi32: 0, state: ptr::null_mut(), bzalloc: None, bzfree: None,
            opaque: ptr::null_mut() };
        let init = unsafe { BZ2_bzDecompressInit(&mut stream, 0, 0) };
        if init != BZ_OK { return Err(init); }

        let result = loop {
            let start = decoded.len();
            decoded.resize(start + CHUNK, 0);
            stream.next_out = decoded[start..].as_mut_ptr().cast::<c_char>();
            stream.avail_out = CHUNK as c_uint;
            let status = unsafe { BZ2_bzDecompress(&mut stream) };
            decoded.truncate(start + CHUNK - stream.avail_out as usize);

            if status == BZ_STREAM_END { break Ok(()); }
            if status != BZ_OK { break Err(status); }
            if stream.avail_in == 0 && stream.avail_out != 0 { break Err(libbz2_rs_sys::BZ_UNEXPECTED_EOF); }
        };
        let consumed = input_len - stream.avail_in;
        let end = unsafe { BZ2_bzDecompressEnd(&mut stream) };
        result?;
        if end != BZ_OK { return Err(end); }
        if consumed == 0 { return Err(libbz2_rs_sys::BZ_DATA_ERROR); }
        input_offset += consumed as usize;
    }
    Ok(decoded)
}

fn patterned(size: usize, stride: usize) -> Vec<u8> { (0..size).map(|index| ((index * stride + index / 251) & 255) as u8).collect() }

#[test]
fn valid_upstream_corpus_matches_oracle() {
    let files = corpus_files(".bz2");
    assert!(files.len() >= 16);
    for path in files {
        let encoded = fs::read(&path).unwrap();
        let expected = oracle_decompress(&encoded).unwrap_or_else(|status| panic!("oracle rejected {} with {status}", path.display()));
        for threads in [1, 2] {
            let actual = decompress(&encoded, DecodeOptions { threads, ..DecodeOptions::default() })
                .unwrap_or_else(|error| panic!("fbz rejected {} with {error}", path.display()));
            assert_eq!(actual, expected, "{} with {threads} threads", path.display());
        }
    }
}

#[test]
fn corrupt_upstream_corpus_is_rejected() {
    let files = corpus_files(".bz2.bad");
    assert!(files.len() >= 5);
    for path in files {
        let encoded = fs::read(&path).unwrap();
        assert!(oracle_decompress(&encoded).is_err(), "oracle accepted {}", path.display());
        assert!(decompress(&encoded, DecodeOptions::default()).is_err(), "fbz accepted {}", path.display());
    }
}

#[test]
fn generated_shapes_match_oracle() {
    let cases = [Vec::new(), vec![0; 200_000], (0_u8..=255).cycle().take(200_000).collect(), patterned(200_000, 37), patterned(1_100_000, 251)];
    for (case, level) in cases.into_iter().zip([Level::FASTEST, Level::BEST, Level::FASTEST, Level::BEST, Level::FASTEST]) {
        let encoded = compress(&case, level);
        assert_eq!(decompress(&encoded, DecodeOptions { threads: 2, ..DecodeOptions::default() }).unwrap(), oracle_decompress(&encoded).unwrap());
    }
}

#[test]
#[cfg(not(debug_assertions))]
fn performance_regression_stays_bounded() {
    let source = oracle_decompress(include_bytes!("corpus/go/Isaac.Newton-Opticks.txt.bz2")).unwrap();
    let plain = source.repeat(2);
    let encoded = compress(&plain, Level::FASTEST);
    let repeats = 3;
    let fbz_time = benchmark::elapsed(repeats, || {
        std::hint::black_box(decompress(&encoded, DecodeOptions { threads: 2, ..DecodeOptions::default() }).unwrap());
    });
    let oracle_time = benchmark::elapsed(repeats, || { std::hint::black_box(oracle_decompress(&encoded).unwrap()); });
    assert!(fbz_time.as_secs_f64() <= oracle_time.as_secs_f64() * 1.3, "fbz {fbz_time:?} exceeded 1.3x oracle {oracle_time:?}");
}
