#[allow(dead_code, unused_imports)]
mod common;
mod support;

use std::{fs, path::PathBuf, process::Command, time::{Duration, Instant}};

use support::{ZipMethod, simplewiki_prefix, zip_bytes};

const ENTRY_COUNT: usize = 18;

fn requested_threads() -> usize { std::env::var("FBZ_THREADS").ok().map(|value| value.parse().expect("FBZ_THREADS must be an integer")).unwrap_or(0) }

#[derive(Clone, Copy)]
enum Shape { Single, Many }

struct Fixture { directory: tempfile::TempDir, input: PathBuf, contents: Vec<u8>, chunk_size: usize, shape: Shape }

impl Fixture {
    fn new(shape: Shape) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let contents = simplewiki_prefix();
        let (archive, chunk_size) = match shape {
            Shape::Single => (zip_bytes(&[("payload.bin", &contents, ZipMethod::Deflate)]), contents.len()),
            Shape::Many => {
                let chunk_size = contents.len().div_ceil(ENTRY_COUNT);
                let names: Vec<_> = (0..ENTRY_COUNT).map(|index| format!("parts/{index:02}.bin")).collect();
                let entries: Vec<_> =
                    contents.chunks(chunk_size).enumerate().map(|(index, chunk)| (names[index].as_str(), chunk, ZipMethod::Deflate)).collect();
                (zip_bytes(&entries), chunk_size)
            }
        };
        let input = directory.path().join("fixture.zip");
        fs::write(&input, &archive).unwrap();
        eprintln!(
            "ZIP fixture: {} entries, {:.1} MiB compressed, {:.1} MiB decoded", match shape {
                Shape::Single => 1,
                Shape::Many => ENTRY_COUNT,
            }, archive.len() as f64 / 1_048_576.0, contents.len() as f64 / 1_048_576.0
        );
        Self { directory, input, contents, chunk_size, shape }
    }

    fn verify(&self, output: &std::path::Path) {
        match self.shape {
            Shape::Single => assert_eq!(fs::read(output.join("payload.bin")).unwrap(), self.contents),
            Shape::Many => {
                for (index, expected) in self.contents.chunks(self.chunk_size).enumerate() {
                    assert_eq!(fs::read(output.join(format!("parts/{index:02}.bin"))).unwrap(), expected);
                }
            }
        }
    }
}

fn fbz_command(input: &std::path::Path, output: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fbz"));
    command.args(["-q", "-P", &requested_threads().to_string(), "-C"]).arg(output).arg(input);
    command
}

fn unzip_command(input: &std::path::Path, output: &std::path::Path) -> Command {
    let mut command = Command::new("unzip");
    command.args(["-qq"]).arg(input).args(["-d"]).arg(output);
    command
}

fn timed(command: &mut Command) -> Duration {
    let start = Instant::now();
    assert!(command.status().unwrap().success());
    start.elapsed()
}

fn benchmark(shape: Shape, fbz: bool) {
    let fixture = Fixture::new(shape);
    let warm = fixture.directory.path().join("warm");
    let measured = fixture.directory.path().join("measured");
    let warm_time = if fbz { timed(&mut fbz_command(&fixture.input, &warm)) } else { timed(&mut unzip_command(&fixture.input, &warm)) };
    let elapsed = if fbz { timed(&mut fbz_command(&fixture.input, &measured)) } else { timed(&mut unzip_command(&fixture.input, &measured)) };
    fixture.verify(&measured);
    eprintln!("{}: warm {warm_time:.3?}, measured {elapsed:.3?}", if fbz { "fbz" } else { "Info-ZIP unzip 6.00 (Apple)" });
}

#[test]
#[ignore = "local single-run one-entry ZIP extraction benchmark"]
fn zip_single_fbz() { benchmark(Shape::Single, true); }

#[test]
#[ignore = "local single-run one-entry Info-ZIP baseline"]
fn zip_single_unzip() { benchmark(Shape::Single, false); }

#[test]
#[ignore = "local single-run many-entry ZIP extraction benchmark"]
fn zip_many_fbz() { benchmark(Shape::Many, true); }

#[test]
#[ignore = "local single-run many-entry Info-ZIP baseline"]
fn zip_many_unzip() { benchmark(Shape::Many, false); }

#[test]
#[cfg(unix)]
#[ignore = "local ZIP extraction time and peak-memory benchmark"]
fn zip_many_fbz_process_metrics() {
    let fixture = Fixture::new(Shape::Many);
    let output = fixture.directory.path().join("measured");
    let metrics = common::measure(&mut fbz_command(&fixture.input, &output)).unwrap();
    assert!(metrics.status.success());
    fixture.verify(&output);
    eprintln!(
        "fbz ZIP: wall {:.3}s, CPU {:.3}s user + {:.3}s system, peak RSS {:.1} MiB, peak physical footprint {:.1} MiB", metrics.wall.as_secs_f64(),
        metrics.user.as_secs_f64(), metrics.system.as_secs_f64(), metrics.peak_rss_bytes as f64 / 1_048_576.0,
        metrics.peak_phys_footprint_bytes.map_or(f64::NAN, |bytes| bytes as f64 / 1_048_576.0),
    );
}

#[test]
#[cfg(unix)]
#[ignore = "local Info-ZIP extraction time and peak-memory baseline"]
fn zip_many_unzip_process_metrics() {
    let fixture = Fixture::new(Shape::Many);
    let output = fixture.directory.path().join("measured");
    let metrics = common::measure(&mut unzip_command(&fixture.input, &output)).unwrap();
    assert!(metrics.status.success());
    fixture.verify(&output);
    eprintln!(
        "Info-ZIP unzip 6.00 (Apple): wall {:.3}s, CPU {:.3}s user + {:.3}s system, peak RSS {:.1} MiB, peak physical footprint {:.1} MiB",
        metrics.wall.as_secs_f64(), metrics.user.as_secs_f64(), metrics.system.as_secs_f64(), metrics.peak_rss_bytes as f64 / 1_048_576.0,
        metrics.peak_phys_footprint_bytes.map_or(f64::NAN, |bytes| bytes as f64 / 1_048_576.0),
    );
}
