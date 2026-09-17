#[allow(dead_code, unused_imports)]
mod common;
mod support;

use std::{fs, hint::black_box, io::Write, path::PathBuf, process::Command, time::Instant};

use fbz::{DecodeOptions, lz4};
use lz4_flex::frame::{BlockMode, BlockSize, FrameEncoder, FrameInfo};
use support::simplewiki_prefix;

fn requested_threads() -> usize { std::env::var("FBZ_THREADS").ok().map(|value| value.parse().expect("FBZ_THREADS must be an integer")).unwrap_or(0) }

struct Fixture { _directory: tempfile::TempDir, input: PathBuf }

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let contents = simplewiki_prefix();
        let info = FrameInfo::new().block_size(BlockSize::Max4MB).block_mode(BlockMode::Independent).block_checksums(false).content_checksum(true);
        let mut encoder = FrameEncoder::with_frame_info(info, Vec::new());
        encoder.write_all(&contents).unwrap();
        let encoded = encoder.finish().unwrap();
        let input = directory.path().join("simplewiki-first-5pct.xml.lz4");
        fs::write(&input, &encoded).unwrap();
        eprintln!("LZ4 fixture: {:.1} MiB compressed, {:.1} MiB decoded", encoded.len() as f64 / 1_048_576.0, contents.len() as f64 / 1_048_576.0);
        Self { _directory: directory, input }
    }
}

fn fbz_command(input: &std::path::Path) -> Command { fbz_command_with_threads(input, requested_threads()) }

fn fbz_command_with_threads(input: &std::path::Path, threads: usize) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fbz"));
    command.args(["test", "-q", "-P", &threads.to_string()]).arg(input);
    command
}

fn lz4_command(input: &std::path::Path) -> Command {
    let mut command = Command::new("lz4");
    command.args(["-t", "-q"]).arg(input);
    command
}

#[test]
#[cfg(unix)]
#[ignore = "local single-run LZ4 comparison against the Homebrew CLI"]
fn lz4_cli_comparison() {
    let fixture = Fixture::new();
    assert!(fbz_command(&fixture.input).status().unwrap().success());
    assert!(lz4_command(&fixture.input).status().unwrap().success());

    let ours = common::measure(&mut fbz_command(&fixture.input)).unwrap();
    let reference = common::measure(&mut lz4_command(&fixture.input)).unwrap();
    assert!(ours.status.success());
    assert!(reference.status.success());
    eprintln!(
        "fbz LZ4: {:.3} ms, peak RSS {:.1} MiB, physical {:.1} MiB",
        ours.wall.as_secs_f64() * 1_000.0,
        ours.peak_rss_bytes as f64 / 1_048_576.0,
        ours.peak_phys_footprint_bytes.map_or(f64::NAN, |bytes| bytes as f64 / 1_048_576.0),
    );
    eprintln!(
        "lz4 1.10.0: {:.3} ms, peak RSS {:.1} MiB, physical {:.1} MiB",
        reference.wall.as_secs_f64() * 1_000.0,
        reference.peak_rss_bytes as f64 / 1_048_576.0,
        reference.peak_phys_footprint_bytes.map_or(f64::NAN, |bytes| bytes as f64 / 1_048_576.0),
    );
    let ratio = ours.wall.as_secs_f64() / reference.wall.as_secs_f64();
    eprintln!("fbz/reference: {ratio:.3}x");
    assert!(ratio <= 1.2, "fbz must remain within 20% of lz4 1.10.0; measured {ratio:.3}x");
}

#[test]
#[cfg(unix)]
#[ignore = "local one-run-per-count LZ4 thread-scaling diagnostic"]
fn lz4_thread_sweep() {
    let fixture = Fixture::new();
    for threads in [1, 2, 4, 6, 8, 12, 18] {
        assert!(fbz_command_with_threads(&fixture.input, threads).status().unwrap().success());
        let result = common::measure(&mut fbz_command_with_threads(&fixture.input, threads)).unwrap();
        assert!(result.status.success());
        eprintln!("{threads:>2} threads: {:>7.3} ms, peak RSS {:>5.1} MiB", result.wall.as_secs_f64() * 1_000.0, result.peak_rss_bytes as f64 / 1_048_576.0,);
    }
}

fn frame(contents: &[u8], block_size: BlockSize) -> Vec<u8> {
    let info = FrameInfo::new()
        .block_size(block_size)
        .block_mode(BlockMode::Independent)
        .block_checksums(false)
        .content_checksum(false)
        .content_size(Some(contents.len() as u64));
    let mut encoder = FrameEncoder::with_frame_info(info, Vec::new());
    encoder.write_all(contents).unwrap();
    encoder.finish().unwrap()
}

fn timed_decode(encoded: &[u8], threads: usize) -> (std::time::Duration, lz4::Report) {
    let mut output = Vec::new();
    let start = Instant::now();
    let report = lz4::decompress_to_writer_with_options(encoded, &mut output, DecodeOptions { threads, ..DecodeOptions::default() }).unwrap();
    let elapsed = start.elapsed();
    black_box(output);
    (elapsed, report)
}

#[test]
#[ignore = "local LZ4 long-match and stored-block diagnostics"]
fn lz4_shape_diagnostics() {
    let repeated = vec![b'x'; 4 * 1024 * 1024];
    let repeated_frame = frame(&repeated, BlockSize::Max4MB);
    timed_decode(&repeated_frame, 1);
    let (repeated_time, repeated_report) = timed_decode(&repeated_frame, 1);
    assert!(repeated_report.blocks.iter().any(|block| !block.stored));

    let mut state = 0x9e37_79b9_u32;
    let random: Vec<_> = (0..16 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let stored_frame = frame(&random, BlockSize::Max4MB);
    timed_decode(&stored_frame, 1);
    timed_decode(&stored_frame, 4);
    let (stored_serial, serial_report) = timed_decode(&stored_frame, 1);
    let (stored_parallel, parallel_report) = timed_decode(&stored_frame, 4);
    assert!(serial_report.blocks.iter().all(|block| block.stored));
    assert!(parallel_report.blocks.iter().all(|block| block.stored));
    eprintln!(
        "long match: {:.3} ms; stored serial: {:.3} ms; stored four-thread: {:.3} ms",
        repeated_time.as_secs_f64() * 1_000.0,
        stored_serial.as_secs_f64() * 1_000.0,
        stored_parallel.as_secs_f64() * 1_000.0,
    );
}
