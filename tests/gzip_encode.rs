use std::io;
mod support;

use std::io::Write;

use fbz::{EncodeOptions, gzip};
use flate2::read::GzDecoder;

#[test]
fn gzip_encoder_roundtrips_with_fbz_and_flate2() {
    let inputs = support::compression_inputs(b"hello gzip", 2_500_000);
    for (index, input) in inputs.into_iter().enumerate() {
        let threads = if index < 2 { 1 } else { 4 };
        let options = EncodeOptions { threads, memory_limit: 64 * 1024 * 1024, level: Some(6) };
        let encoded = gzip::compress_with_options(&input, options).unwrap();
        assert_eq!(gzip::decompress(&encoded).unwrap(), input);

        let mut decoded = Vec::new();
        io::copy(&mut GzDecoder::new(&encoded[..]), &mut decoded).unwrap();
        assert_eq!(decoded, input);

        let report = gzip::decompress_to_writer(&encoded, &mut io::sink()).unwrap();
        assert_eq!(report.members.len(), 1);
    }
}

#[test]
fn gzip_encoder_accepts_incremental_writes_and_flushes() {
    let input: Vec<_> = (0..400_000).map(|index| (index % 251) as u8).collect();
    let mut encoded = Vec::new();
    let mut encoder = gzip::Encoder::new(&mut encoded, EncodeOptions { threads: 3, memory_limit: 32 * 1024 * 1024, level: Some(3) }).unwrap();
    for chunk in input.chunks(7777) { encoder.write_all(chunk).unwrap(); }
    encoder.flush().unwrap();
    let (_, report) = encoder.finish().unwrap();
    assert_eq!(report.input_len, input.len() as u64);
    assert_eq!(gzip::decompress(&encoded).unwrap(), input);
}

#[test]
fn gzip_encoder_rejects_invalid_options() {
    let error = gzip::compress_with_options(b"data", EncodeOptions { level: Some(0), ..EncodeOptions::default() }).unwrap_err();
    assert!(error.to_string().contains("level"));
    let error = gzip::compress_with_options(b"data", EncodeOptions { memory_limit: 1, ..EncodeOptions::default() }).unwrap_err();
    assert!(error.to_string().contains("memory limit"));
    let mut output = b"unchanged".to_vec();
    assert!(gzip::Encoder::new(&mut output, EncodeOptions { memory_limit: 1, ..EncodeOptions::default() }).is_err());
    assert_eq!(output, b"unchanged");
}
