mod common;

use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use fbz::{DecodeOptions, Source, decompress, decompress_to_writer};

const FIVE_PERCENT_LEN: usize = 84_423_012;
const FIVE_PERCENT_BLAKE3: &str = "69f41f28dc8ac74509d368c6aaec02f3cdf891c9da4ccf8caf625687dcd61908";
const FULL_LEN: u64 = 1_688_460_257;
const ENWIKI_1000_LEN: usize = 2_715_335_085;

#[derive(Default)]
struct CountingSink(u64);

impl Write for CountingSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0 += buffer.len() as u64;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn corpus_path(name: &str) -> std::path::PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("meta").join(name) }

fn requested_threads() -> usize { std::env::var("FBZ_THREADS").ok().map(|value| value.parse().expect("FBZ_THREADS must be an integer")).unwrap_or(0) }

fn physical_footprint_mib(metrics: &common::ProcessMetrics) -> f64 {
    metrics.peak_phys_footprint_bytes.map_or(f64::NAN, |bytes| bytes as f64 / (1024.0 * 1024.0))
}

fn validation_command(binary: impl AsRef<OsStr>, path: &Path, threads: usize) -> Command {
    let mut command = Command::new(&binary);
    let operation = if binary.as_ref() == OsStr::new(env!("CARGO_BIN_EXE_fbz")) { "test" } else { "--test" };
    command.arg(operation).arg("-P").arg(threads.to_string()).arg(path);
    command
}

fn quiet_validation_command(binary: impl AsRef<OsStr>, path: &Path, threads: usize) -> Command {
    let mut command = validation_command(binary, path, threads);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command
}

fn warm_validation(binary: impl AsRef<OsStr>, path: &Path, threads: usize) {
    assert!(quiet_validation_command(binary, path, threads).status().unwrap().success());
}

fn timed_validation(binary: impl AsRef<OsStr>, path: &Path, threads: usize) -> common::Timing {
    common::measure_timing(&mut quiet_validation_command(binary, path, threads)).unwrap()
}

fn print_process_metrics(label: &str, metrics: &common::ProcessMetrics) {
    eprintln!(
        "{label}: wall {:.3}s, CPU {:.3}s user + {:.3}s system, peak RSS {:.1} MiB, peak physical footprint {:.1} MiB",
        metrics.wall.as_secs_f64(),
        metrics.user.as_secs_f64(),
        metrics.system.as_secs_f64(),
        metrics.peak_rss_bytes as f64 / (1024.0 * 1024.0),
        physical_footprint_mib(metrics),
    );
}

fn timed_fbz(path: &Path, threads: usize) -> Duration {
    let start = Instant::now();
    let source = Source::open(path).unwrap();
    let mut output = CountingSink::default();
    decompress_to_writer(source.as_slice(), &mut output, DecodeOptions { threads, ..DecodeOptions::default() }).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(output.0, FULL_LEN);
    elapsed
}

#[test]
#[ignore = "local SimpleWiki performance benchmark"]
fn simplewiki_first_five_percent() {
    let path = corpus_path("simplewiki-first-5pct.xml.bz2");
    let encoded = fs::read(&path).unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
    let threads = requested_threads();
    let options = DecodeOptions { threads, ..DecodeOptions::default() };
    let resolved_threads = options.resolved_threads();

    let start = Instant::now();
    let decoded = decompress(&encoded, options).unwrap();
    let elapsed = start.elapsed();
    let mib_per_second = decoded.len() as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64();
    let hash = blake3::hash(&decoded).to_hex().to_string();
    eprintln!("SimpleWiki 5%: {elapsed:.3?}, {mib_per_second:.1} MiB/s, {resolved_threads} threads, BLAKE3 {hash}");

    assert_eq!(decoded.len(), FIVE_PERCENT_LEN);
    assert_eq!(hash, FIVE_PERCENT_BLAKE3);
}

#[test]
#[ignore = "local full SimpleWiki performance benchmark"]
fn simplewiki_full() {
    let path = corpus_path("simplewiki-full.xml.bz2");
    let options = DecodeOptions { threads: requested_threads(), ..DecodeOptions::default() };

    let elapsed = timed_fbz(&path, options.threads);
    eprintln!("fbz ({} threads): {elapsed:.3?}", options.resolved_threads());
}

#[test]
#[cfg(unix)]
#[ignore = "local full SimpleWiki subprocess time and peak-memory benchmark"]
fn simplewiki_cli_process_metrics() {
    let path = corpus_path("simplewiki-full.xml.bz2");
    let mut command = validation_command(env!("CARGO_BIN_EXE_fbz"), &path, requested_threads());
    let metrics = common::measure(&mut command).unwrap();
    assert!(metrics.status.success());
    print_process_metrics("fbz bzip2", &metrics);
}

#[test]
#[cfg(unix)]
#[ignore = "local gzip subprocess time and peak-memory benchmark"]
fn gzip_cli_process_metrics() {
    let path = corpus_path("simplewiki-full.xml.gz");
    let threads = requested_threads();
    let mut command = validation_command(env!("CARGO_BIN_EXE_fbz"), &path, threads);
    let metrics = common::measure(&mut command).unwrap();
    assert!(metrics.status.success());
    let threads = if threads == 0 { "auto".to_owned() } else { threads.to_string() };
    print_process_metrics(&format!("fbz gzip ({threads} threads)"), &metrics);
}

#[test]
#[cfg(unix)]
#[ignore = "local system gzip subprocess time and peak-memory benchmark"]
fn system_gzip_process_metrics() {
    let path = corpus_path("simplewiki-full.xml.gz");
    let mut command = Command::new("gzip");
    command.arg("-dc").arg(&path).stdout(Stdio::null());
    let metrics = common::measure(&mut command).unwrap();
    assert!(metrics.status.success());
    print_process_metrics("system gzip", &metrics);
}

fn rapidgzip_binary() -> std::path::PathBuf {
    let default = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../git/rapidgzip-rust/target/release/rapidgzip-rust");
    std::env::var_os("RAPIDGZIP_BIN").map(Into::into).unwrap_or(default)
}

#[test]
#[cfg(unix)]
#[ignore = "local rapidgzip-rust subprocess time and peak-memory benchmark"]
fn rapidgzip_rust_process_metrics() {
    let path = corpus_path("simplewiki-full.xml.gz");
    let mut command = validation_command(rapidgzip_binary(), &path, requested_threads());
    let metrics = common::measure(&mut command).unwrap();
    assert!(metrics.status.success());
    print_process_metrics("rapidgzip-rust", &metrics);
}

#[test]
#[cfg(unix)]
#[ignore = "local single-run fbz full gzip validation"]
fn gzip_fbz_validation() {
    let path = corpus_path("simplewiki-full.xml.gz");
    let warm_path = corpus_path("simplewiki-first-5pct.xml.gz");
    let threads = requested_threads();
    let binary = env!("CARGO_BIN_EXE_fbz");
    warm_validation(binary, &warm_path, threads);
    let result = timed_validation(binary, &path, threads);
    assert!(result.status.success());
    eprintln!("fbz full gzip: {:.3}s", result.wall.as_secs_f64());
}

#[test]
#[cfg(unix)]
#[ignore = "local single-run gzip performance ratio against rapidgzip-rust"]
fn gzip_reference_ratio() {
    let path = corpus_path("simplewiki-full.xml.gz");
    let warm_path = corpus_path("simplewiki-first-5pct.xml.gz");
    let threads = requested_threads();
    let ours_binary = env!("CARGO_BIN_EXE_fbz");
    let reference_binary = rapidgzip_binary();

    warm_validation(ours_binary, &warm_path, threads);
    warm_validation(&reference_binary, &warm_path, threads);

    let ours = timed_validation(ours_binary, &path, threads);
    let reference = timed_validation(&reference_binary, &path, threads);
    assert!(ours.status.success());
    assert!(reference.status.success());

    let ratio = ours.wall.as_secs_f64() / reference.wall.as_secs_f64();
    eprintln!("fbz {:.3}s / rapidgzip-rust {:.3}s = {ratio:.3}x", ours.wall.as_secs_f64(), reference.wall.as_secs_f64());
    assert!(ratio <= 1.2, "fbz must remain within 20% of rapidgzip-rust; measured {ratio:.3}x");
}

fn timed_vec(name: &str, decode: impl FnOnce() -> Vec<u8>) {
    let start = Instant::now();
    let decoded = decode();
    let elapsed = start.elapsed();
    assert_eq!(decoded.len(), ENWIKI_1000_LEN);
    eprintln!("{name}: {elapsed:.3?}");
}

fn enwiki_fixture() -> (Vec<u8>, usize) {
    let encoded = fs::read(corpus_path("enwiki-first-1000-streams.xml.bz2")).unwrap();
    (encoded, requested_threads())
}

#[test]
#[ignore = "local enwiki multistream performance comparison"]
fn enwiki_first_1000_fbz_parallel() {
    let (encoded, threads) = enwiki_fixture();
    let options = DecodeOptions { threads, ..DecodeOptions::default() };
    timed_vec(&format!("fbz parallel ({} threads)", options.resolved_threads()), || decompress(&encoded, options).unwrap());
}

#[test]
#[ignore = "local enwiki multistream performance comparison"]
fn enwiki_first_1000_crabz2_parallel() {
    let (encoded, threads) = enwiki_fixture();
    timed_vec("crabz2 parallel", || crabz2::decompress_parallel(&encoded, if threads == 0 { None } else { Some(threads) }).unwrap());
}

#[test]
#[ignore = "local enwiki multistream performance comparison"]
fn enwiki_first_1000_fbz_serial() {
    let (encoded, _) = enwiki_fixture();
    timed_vec("fbz serial", || decompress(&encoded, DecodeOptions { threads: 1, ..DecodeOptions::default() }).unwrap());
}

#[test]
#[ignore = "local enwiki multistream performance comparison"]
fn enwiki_first_1000_crabz2_serial() {
    let (encoded, _) = enwiki_fixture();
    timed_vec("crabz2 serial", || crabz2::decompress(&encoded).unwrap());
}
