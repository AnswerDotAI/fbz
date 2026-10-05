use std::{
    fs,
    io::{self, Write},
    process::Command,
    time::{Duration, Instant},
};

use crabz2::{Level, compress};
use fbz::{DecodeOptions, OutputSink, gzip as gzip_decoder};
use flate2::{Compression, write::GzEncoder};

mod support;
use support::simplewiki_prefix;

fn requested_threads() -> usize { std::env::var("FBZ_THREADS").ok().map(|value| value.parse().expect("FBZ_THREADS must be an integer")).unwrap_or(0) }

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fbz"));
    command.args(["-P", &requested_threads().to_string()]);
    command
}

fn tar_bytes(contents: &[u8]) -> Vec<u8> {
    let mut archive = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut archive);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "payload.bin", contents).unwrap();
        builder.finish().unwrap();
    }
    archive
}

fn gzip(contents: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(contents).unwrap();
    encoder.finish().unwrap()
}

fn timed(action: impl FnOnce()) -> Duration {
    let started = Instant::now();
    action();
    started.elapsed()
}

fn timed_command(command: &mut Command) -> Duration { timed(|| assert!(command.status().unwrap().success())) }

struct Fixture { directory: tempfile::TempDir, contents: Vec<u8>, input: std::path::PathBuf }

fn fixture(extension: &str, encode: impl Fn(&[u8]) -> Vec<u8>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let contents = simplewiki_prefix();
    let archive = tar_bytes(&contents);
    let input = directory.path().join(format!("archive.{extension}"));
    let encoded = encode(&archive);
    eprintln!("{extension}: {} MiB decoded from {:.1} MiB compressed", archive.len() / (1024 * 1024), encoded.len() as f64 / (1024.0 * 1024.0));
    fs::write(&input, encoded).unwrap();
    Fixture { directory, contents, input }
}

fn fbz_overhead(extension: &str, encode: impl Fn(&[u8]) -> Vec<u8>) {
    let fixture = fixture(extension, encode);
    let warm = fixture.directory.path().join("warm");
    fs::create_dir(&warm).unwrap();
    assert!(binary().args(["-C", warm.to_str().unwrap(), fixture.input.to_str().unwrap()]).status().unwrap().success());

    let raw = fixture.directory.path().join("archive.tar");
    let raw_time = timed_command(binary().args([fixture.input.to_str().unwrap(), "-o", raw.to_str().unwrap()]));
    let extracted = fixture.directory.path().join("extracted");
    let extract_time = timed_command(binary().args(["-C", extracted.to_str().unwrap(), fixture.input.to_str().unwrap()]));
    assert_eq!(fs::read(extracted.join("payload.bin")).unwrap(), fixture.contents);

    let raw_ratio = extract_time.as_secs_f64() / raw_time.as_secs_f64();
    eprintln!("{extension}: raw tar {raw_time:.3?}, fbz extract {extract_time:.3?} ({raw_ratio:.3}x raw)");
    assert!(raw_ratio <= 3.0, "tar extraction exceeded the broad 3x raw-decode guard; measured {raw_ratio:.3}x");
}

fn system_reference(extension: &str, encode: impl Fn(&[u8]) -> Vec<u8>) {
    let fixture = fixture(extension, encode);
    let warm = fixture.directory.path().join("warm");
    fs::create_dir(&warm).unwrap();
    assert!(Command::new("tar").args(["-xf", fixture.input.to_str().unwrap(), "-C", warm.to_str().unwrap()]).status().unwrap().success());

    let extracted = fixture.directory.path().join("extracted");
    fs::create_dir(&extracted).unwrap();
    let extract_time = timed_command(Command::new("tar").args(["-xf", fixture.input.to_str().unwrap(), "-C", extracted.to_str().unwrap()]));
    assert_eq!(fs::read(extracted.join("payload.bin")).unwrap(), fixture.contents);
    eprintln!("{extension}: system tar {extract_time:.3?}");
}

#[test]
#[ignore = "local single-run tar crate extraction reference"]
fn tar_crate_reference() {
    let directory = tempfile::tempdir().unwrap();
    let contents = simplewiki_prefix();
    let archive = tar_bytes(&contents);
    let unpack = |destination: &std::path::Path| {
        fs::create_dir(destination).unwrap();
        tar::Archive::new(archive.as_slice()).unpack(destination).unwrap();
    };

    unpack(&directory.path().join("warm"));
    let extracted = directory.path().join("extracted");
    let extract_time = timed(|| unpack(&extracted));
    assert_eq!(fs::read(extracted.join("payload.bin")).unwrap(), contents);
    eprintln!("uncompressed tar crate: {extract_time:.3?}");
}

struct CadenceSink { started: Instant, bytes: usize, events: Vec<(usize, Duration)> }

impl CadenceSink {
    fn new() -> Self { Self { started: Instant::now(), bytes: 0, events: Vec::new() } }

    fn record(&mut self, bytes: usize) {
        if bytes == 0 { return; }
        self.bytes += bytes;
        self.events.push((self.bytes, self.started.elapsed()));
    }

    fn milestone(&self, bytes: usize) -> Duration { self.events.iter().find(|(total, _)| *total >= bytes).unwrap().1 }
}

impl OutputSink for CadenceSink {
    fn write_borrowed(&mut self, buffer: &[u8]) -> io::Result<()> { self.record(buffer.len()); Ok(()) }

    fn write_owned_from(&mut self, buffer: Vec<u8>, start: usize) -> io::Result<()> {
        let bytes = buffer.len().checked_sub(start).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "owned chunk start exceeds its length"))?;
        self.record(bytes);
        Ok(())
    }
}

#[test]
#[ignore = "local single-run parallel gzip output cadence"]
fn tgz_output_cadence() {
    let contents = simplewiki_prefix();
    let archive = tar_bytes(&contents);
    let encoded = gzip(&archive);
    let options = DecodeOptions { threads: requested_threads(), ..DecodeOptions::default() };
    let threads = options.resolved_threads();
    let mut output = CadenceSink::new();
    let report = gzip_decoder::decompress_to_sink_with_options_and_progress(&encoded, &mut output, options, |_| {}).unwrap();
    let elapsed = output.started.elapsed();
    assert_eq!(output.bytes, archive.len());
    assert_eq!(report.decoded_len, archive.len() as u64);
    eprintln!(
        "gzip {threads} threads, {} chunks: first {:.3?}, 25% {:.3?}, 50% {:.3?}, 75% {:.3?}, complete {elapsed:.3?}",
        output.events.len(),
        output.events[0].1,
        output.milestone(archive.len() / 4),
        output.milestone(archive.len() / 2),
        output.milestone(archive.len() * 3 / 4),
    );
}

#[test]
#[ignore = "local single-run fbz gzip tar extraction overhead"]
fn tgz_fbz_overhead() { fbz_overhead("tgz", gzip); }

#[test]
#[ignore = "local single-run system gzip tar extraction reference"]
fn tgz_system_reference() { system_reference("tgz", gzip); }

#[test]
#[ignore = "local single-run fbz bzip2 tar extraction overhead"]
fn tbz2_fbz_overhead() { fbz_overhead("tbz2", |contents| compress(contents, Level::BEST)); }

#[test]
#[ignore = "local single-run system bzip2 tar extraction reference"]
fn tbz2_system_reference() { system_reference("tbz2", |contents| compress(contents, Level::BEST)); }
