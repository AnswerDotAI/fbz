use std::io;
mod support;

use std::io::{Read, Write};

use fbz::{EncodeOptions, lz4};
use lz4_flex::frame::FrameDecoder;

#[test]
fn lz4_encoder_roundtrips_with_fbz_and_lz4_flex() {
    let inputs = support::compression_inputs(b"hello LZ4", 10_000_000);
    for (index, input) in inputs.into_iter().enumerate() {
        let threads = if index < 2 { 1 } else { 4 };
        let options = EncodeOptions { threads, memory_limit: 128 * 1024 * 1024, level: Some(6) };
        let encoded = lz4::compress(&input, options).unwrap();
        assert_eq!(lz4::decompress(&encoded).unwrap(), input);

        let mut decoded = Vec::new();
        FrameDecoder::new(&encoded[..]).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, input);

        let report = lz4::decompress_to_writer(&encoded, &mut io::sink()).unwrap();
        assert_eq!(report.frames.len(), 1);
        assert_eq!(report.frames[0].block_mode, lz4::BlockMode::Independent);
        assert!(report.frames[0].content_checksum);
    }
}

#[test]
fn lz4_encoder_accepts_incremental_writes_and_low_memory() {
    let input: Vec<_> = (0..1_000_000).map(|index| (index % 251) as u8).collect();
    let mut encoded = Vec::new();
    let mut encoder = lz4::Encoder::new(&mut encoded, EncodeOptions { threads: 4, memory_limit: 2 * 1024 * 1024, level: Some(3) }).unwrap();
    for chunk in input.chunks(7777) { encoder.write_all(chunk).unwrap(); }
    encoder.flush().unwrap();
    let (_, report) = encoder.finish().unwrap();
    assert_eq!(report.input_len, input.len() as u64);
    assert_eq!(lz4::decompress(&encoded).unwrap(), input);
}
