mod support;

use std::{fs, io::{self, Read, Write}, path::{Path, PathBuf}, time::{Duration, Instant}};

use fbz::{DecodeOptions, Reader, Source, decompress_to_writer, gzip, lz4};
use lz4_flex::frame::{BlockMode, BlockSize, FrameEncoder, FrameInfo};
use support::simplewiki_prefix;

const DECODED_LEN: u64 = 84_423_012;

fn fixture(extension: &str) -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("meta/simplewiki-first-5pct.xml.{extension}")) }

fn reader_run(path: &Path) -> (Duration, u64) {
    let started = Instant::now();
    let mut reader = Reader::open(path, DecodeOptions::default()).unwrap();
    let decoded = io::copy(&mut reader, &mut io::sink()).unwrap();
    (started.elapsed(), decoded)
}

enum Codec { Bzip2, Gzip, Lz4 }

fn writer_run(path: &Path, codec: Codec) -> Duration {
    let started = Instant::now();
    let source = Source::open(path).unwrap();
    match codec {
        Codec::Bzip2 => decompress_to_writer(source.as_slice(), &mut io::sink(), DecodeOptions::default()).unwrap(),
        Codec::Gzip => { gzip::decompress_to_writer_with_options(source.as_slice(), &mut io::sink(), DecodeOptions::default()).unwrap(); }
        Codec::Lz4 => { lz4::decompress_to_writer_with_options(source.as_slice(), &mut io::sink(), DecodeOptions::default()).unwrap(); }
    }
    started.elapsed()
}

#[test]
#[ignore = "local single-run Reader latency and writer-path comparison"]
fn reader_writer_comparison() {
    for (extension, codec) in [("bz2", Codec::Bzip2), ("gz", Codec::Gzip)] {
        let path = fixture(extension);
        let first_started = Instant::now();
        let mut reader = Reader::open(&path, DecodeOptions::default()).unwrap();
        let mut first = [0];
        reader.read_exact(&mut first).unwrap();
        let first_byte = first_started.elapsed();
        drop(reader);

        let (reader_time, decoded) = reader_run(&path);
        let writer_time = writer_run(&path, codec);
        assert_eq!(decoded, DECODED_LEN);
        println!(
            "{extension}: first byte {first_byte:?}, Reader {reader_time:?}, writer {writer_time:?}, ratio {:.3}x",
            reader_time.as_secs_f64() / writer_time.as_secs_f64()
        );
    }
}

#[test]
#[ignore = "local single-run incremental LZ4 Reader latency and writer comparison"]
fn lz4_reader_writer_comparison() {
    let directory = tempfile::tempdir().unwrap();
    let plain = simplewiki_prefix();
    let info = FrameInfo::new().block_size(BlockSize::Max4MB).block_mode(BlockMode::Independent).block_checksums(false).content_checksum(true)
        .content_size(Some(plain.len() as u64));
    let mut encoder = FrameEncoder::with_frame_info(info, Vec::new());
    encoder.write_all(&plain).unwrap();
    let path = directory.path().join("simplewiki-first-5pct.xml.lz4");
    fs::write(&path, encoder.finish().unwrap()).unwrap();

    let first_started = Instant::now();
    let mut reader = Reader::open(&path, DecodeOptions::default()).unwrap();
    let mut first = [0];
    reader.read_exact(&mut first).unwrap();
    let first_byte = first_started.elapsed();
    drop(reader);

    let (reader_time, decoded) = reader_run(&path);
    let writer_time = writer_run(&path, Codec::Lz4);
    assert_eq!(decoded, plain.len() as u64);
    println!(
        "lz4: first byte {first_byte:?}, Reader {reader_time:?}, writer {writer_time:?}, ratio {:.3}x", reader_time.as_secs_f64() / writer_time.as_secs_f64()
    );
}
