mod support;

use std::{fs, io::Write, process::Command};

use fbz::{Bzip2Encoder, DecodeOptions, EncodeFormat, EncodeOptions, compress, decompress};

#[test]
fn bzip2_encoder_roundtrips_levels_and_system_decoder() {
    let inputs = support::compression_inputs(b"hello bzip2", 30_000);
    for level in [1, 6, 9] {
        for input in &inputs {
            let options = EncodeOptions { threads: 3, memory_limit: 256 * 1024 * 1024, level: Some(level) };
            let encoded = compress(input, EncodeFormat::Bzip2, options).unwrap();
            assert_eq!(decompress(&encoded, DecodeOptions::default()).unwrap(), *input);
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("data.bz2");
            fs::write(&path, encoded).unwrap();
            assert!(Command::new("bzip2").args(["-t", path.to_str().unwrap()]).status().unwrap().success());
        }
    }
}

#[test]
fn bzip2_encoder_parallel_blocks_and_incremental_writes() {
    let input = support::patterned_bytes_with(220_000, 17, 101);
    let mut encoded = Vec::new();
    let mut encoder = Bzip2Encoder::new(&mut encoded, EncodeOptions { threads: 4, memory_limit: 64 * 1024 * 1024, level: Some(1) }).unwrap();
    for chunk in input.chunks(7777) { encoder.write_all(chunk).unwrap(); }
    encoder.flush().unwrap();
    let (_, report) = encoder.finish().unwrap();
    assert_eq!(report.input_len, input.len() as u64);
    assert!(report.blocks >= 2);
    assert_eq!(decompress(&encoded, DecodeOptions::default()).unwrap(), input);
}

#[test]
fn bzip2_encoder_default_is_level_nine_and_memory_is_bounded() {
    let encoded = compress(b"default level", EncodeFormat::Bzip2, EncodeOptions::default()).unwrap();
    assert_eq!(&encoded[..4], b"BZh9");
    let error = compress(b"too little memory", EncodeFormat::Bzip2, EncodeOptions { memory_limit: 1024 * 1024, level: Some(9), ..EncodeOptions::default() })
        .unwrap_err();
    assert!(error.to_string().contains("memory limit"));
}

#[test]
fn bzip2_encoder_handles_rle_mtf_and_bwt_edge_cases() {
    let mut cases: Vec<Vec<u8>> = vec![
        b"a".to_vec(), b"abcaaabbb".to_vec(), b"aaaa".to_vec(), b"aaaaa".to_vec(), b"abc".to_vec(), b"ababab".to_vec(), b"banana".to_vec(),
        b"^BANANA|".to_vec(), vec![b'z'; 259], vec![b'q'; 262], vec![b'z'; 300], vec![b'k'; 5_000], vec![0xff; 100_000],
        (0..=255u8).chain((0..=255u8).rev()).collect(),
    ];
    for n in 1..40 { cases.push(vec![7u8; n]); }
    for period in 1..8usize { for reps in 1..8usize { cases.push((0..period * reps).map(|i| (i % period) as u8).collect()); } }
    let mut gnarly = Vec::new();
    for i in 0..2000u32 { let b = (i / 7 % 5) as u8; for _ in 0..(i % 9) { gnarly.push(b); } }
    for cut in [0, 1, 2, 3, 4, 5, 100, 1000, gnarly.len()] { cases.push(gnarly[..cut.min(gnarly.len())].to_vec()); }
    let mut state = 0x1234_5678u32;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    for case in 1..120usize {
        let alphabet = 1 + (case % 50) as u32;
        cases.push((0..case * 3).map(|_| (next() % alphabet) as u8).collect());
    }
    let mut mixed = Vec::new();
    while mixed.len() < 60_000 {
        mixed.extend_from_slice(b"the quick brown fox jumps over the lazy dog. ");
        mixed.extend_from_slice(b"aaaaaaaaaaaaaaaaaaaa");
    }
    cases.push(mixed);
    cases.push((0..30_000).map(|_| (next() >> 24) as u8).collect());

    let options = EncodeOptions { threads: 1, memory_limit: 64 * 1024 * 1024, level: Some(1) };
    for (index, input) in cases.iter().enumerate() {
        let encoded = compress(input, EncodeFormat::Bzip2, options).unwrap();
        assert_eq!(decompress(&encoded, DecodeOptions::default()).unwrap(), *input, "case {index} len {}", input.len());
    }
}
