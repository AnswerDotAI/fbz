#[allow(dead_code, unused_imports)]
mod common;
mod support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn input_file(directory: &tempfile::TempDir) -> std::path::PathBuf {
    let path = directory.path().join("simplewiki-prefix.xml");
    fs::write(&path, support::simplewiki_prefix()).unwrap();
    path
}

fn fbz(format: &str, input: &Path) -> Command { fbz_with_threads(format, input, 0) }

fn fbz_with_threads(format: &str, input: &Path, threads: usize) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fbz"));
    command.args(["c", "--format", format, "-q", "-P", &threads.to_string(), "-o", "-"]).arg(input).stdout(Stdio::null()).stderr(Stdio::null());
    command
}

fn reference(program: &str, arguments: &[&str], input: &Path) -> Command {
    let mut command = Command::new(program);
    command.args(arguments).arg(input).stdout(Stdio::null()).stderr(Stdio::null());
    command
}

fn compare(format: &str, program: &str, arguments: &[&str]) {
    let directory = tempfile::tempdir().unwrap();
    let input = input_file(&directory);
    assert!(fbz(format, &input).status().unwrap().success());
    assert!(reference(program, arguments, &input).status().unwrap().success());
    let ours = common::measure(&mut fbz(format, &input)).unwrap();
    let theirs = common::measure(&mut reference(program, arguments, &input)).unwrap();
    assert!(ours.status.success());
    assert!(theirs.status.success());
    eprintln!(
        "{format} compression: fbz {:.1} ms / {:.1} MiB RSS, {program} {:.1} ms / {:.1} MiB RSS, {:.2}x speed",
        ours.wall.as_secs_f64() * 1_000.0,
        ours.peak_rss_bytes as f64 / 1_048_576.0,
        theirs.wall.as_secs_f64() * 1_000.0,
        theirs.peak_rss_bytes as f64 / 1_048_576.0,
        theirs.wall.as_secs_f64() / ours.wall.as_secs_f64(),
    );
}

#[test]
#[ignore = "local single-run bzip2 compression CLI comparison"]
fn bzip2_compression_cli_comparison() { compare("bzip2", "bzip2", &["-9", "-c"]); }

#[test]
#[ignore = "local single-run gzip compression CLI comparison"]
fn gzip_compression_cli_comparison() { compare("gzip", "gzip", &["-6", "-c"]); }

#[test]
#[ignore = "local single-run LZ4 compression CLI comparison"]
fn lz4_compression_cli_comparison() { compare("lz4", "lz4", &["-c"]); }

#[derive(Clone, Copy)]
enum ZipShape { Single, Many }

fn zip_inputs(directory: &Path, shape: ZipShape) -> Vec<PathBuf> {
    let contents = support::simplewiki_prefix();
    match shape {
        ZipShape::Single => {
            fs::write(directory.join("payload.xml"), contents).unwrap();
            vec![PathBuf::from("payload.xml")]
        }
        ZipShape::Many => contents
            .chunks(contents.len().div_ceil(18))
            .enumerate()
            .map(|(index, chunk)| {
                let name = PathBuf::from(format!("part-{index:02}.xml"));
                fs::write(directory.join(&name), chunk).unwrap();
                name
            })
            .collect(),
    }
}

fn fbz_zip(directory: &Path, output: &str, inputs: &[PathBuf]) -> Command { fbz_zip_with_threads(directory, output, inputs, 0) }

fn fbz_zip_with_threads(directory: &Path, output: &str, inputs: &[PathBuf], threads: usize) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fbz"));
    command.current_dir(directory).args(["c", "--format", "zip", "-q", "-P", &threads.to_string(), "-o", output]);
    command.args(inputs);
    command
}

fn info_zip(directory: &Path, output: &str, inputs: &[PathBuf]) -> Command {
    let mut command = Command::new("zip");
    command.current_dir(directory).args(["-q", "-6", output]);
    command.args(inputs);
    command
}

fn compare_zip(shape: ZipShape) {
    let directory = tempfile::tempdir().unwrap();
    let inputs = zip_inputs(directory.path(), shape);
    assert!(fbz_zip(directory.path(), "fbz-warm.zip", &inputs).status().unwrap().success());
    assert!(info_zip(directory.path(), "zip-warm.zip", &inputs).status().unwrap().success());
    let ours = common::measure(&mut fbz_zip(directory.path(), "fbz.zip", &inputs)).unwrap();
    let theirs = common::measure(&mut info_zip(directory.path(), "zip.zip", &inputs)).unwrap();
    assert!(ours.status.success());
    assert!(theirs.status.success());
    let our_size = fs::metadata(directory.path().join("fbz.zip")).unwrap().len();
    let their_size = fs::metadata(directory.path().join("zip.zip")).unwrap().len();
    eprintln!(
        "ZIP {}: fbz {:.1} ms / {:.1} MiB RSS / {:.1} MiB, zip {:.1} ms / {:.1} MiB RSS / {:.1} MiB, {:.2}x speed",
        match shape {
            ZipShape::Single => "one entry",
            ZipShape::Many => "18 entries",
        },
        ours.wall.as_secs_f64() * 1_000.0,
        ours.peak_rss_bytes as f64 / 1_048_576.0,
        our_size as f64 / 1_048_576.0,
        theirs.wall.as_secs_f64() * 1_000.0,
        theirs.peak_rss_bytes as f64 / 1_048_576.0,
        their_size as f64 / 1_048_576.0,
        theirs.wall.as_secs_f64() / ours.wall.as_secs_f64(),
    );
}

fn fbz_tar(directory: &Path, format: &str, output: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fbz"));
    command.current_dir(directory).args(["c", "--format", format, "-q", "-o", output, "payload.xml"]);
    command
}

fn system_tar(directory: &Path, flag: &str, output: &str) -> Command {
    let mut command = Command::new("tar");
    command.current_dir(directory).args([flag, output, "payload.xml"]);
    command
}

fn compare_tar(format: &str, flag: &str, suffix: &str) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("payload.xml"), support::simplewiki_prefix()).unwrap();
    assert!(fbz_tar(directory.path(), format, &format!("fbz-warm.{suffix}")).status().unwrap().success());
    assert!(system_tar(directory.path(), flag, &format!("tar-warm.{suffix}")).status().unwrap().success());
    let ours = common::measure(&mut fbz_tar(directory.path(), format, &format!("fbz.{suffix}"))).unwrap();
    let theirs = common::measure(&mut system_tar(directory.path(), flag, &format!("tar.{suffix}"))).unwrap();
    assert!(ours.status.success());
    assert!(theirs.status.success());
    eprintln!(
        ".{suffix} creation: fbz {:.1} ms / {:.1} MiB RSS, tar {:.1} ms / {:.1} MiB RSS, {:.2}x speed",
        ours.wall.as_secs_f64() * 1_000.0,
        ours.peak_rss_bytes as f64 / 1_048_576.0,
        theirs.wall.as_secs_f64() * 1_000.0,
        theirs.peak_rss_bytes as f64 / 1_048_576.0,
        theirs.wall.as_secs_f64() / ours.wall.as_secs_f64(),
    );
}

#[test]
#[ignore = "local single-run tar.bz2 creation comparison"]
fn tar_bzip2_compression_cli_comparison() { compare_tar("tar-bzip2", "-cjf", "tar.bz2"); }

#[test]
#[ignore = "local single-run tar.gz creation comparison"]
fn tar_gzip_compression_cli_comparison() { compare_tar("tar-gzip", "-czf", "tar.gz"); }

#[test]
#[ignore = "local single-run one-entry ZIP creation comparison"]
fn zip_single_compression_cli_comparison() { compare_zip(ZipShape::Single); }

#[test]
#[ignore = "local single-run many-entry ZIP creation comparison"]
fn zip_many_compression_cli_comparison() { compare_zip(ZipShape::Many); }

#[test]
#[ignore = "local one-run-per-count ZIP compression scaling diagnostic"]
fn zip_compression_thread_sweep() {
    let directory = tempfile::tempdir().unwrap();
    let inputs = zip_inputs(directory.path(), ZipShape::Many);
    for threads in [8, 12, 18] {
        let output = format!("fbz-{threads}.zip");
        let metrics = common::measure(&mut fbz_zip_with_threads(directory.path(), &output, &inputs, threads)).unwrap();
        assert!(metrics.status.success());
        eprintln!("{threads:>2} threads: {:>7.1} ms, peak RSS {:>5.1} MiB", metrics.wall.as_secs_f64() * 1_000.0, metrics.peak_rss_bytes as f64 / 1_048_576.0,);
    }
}

#[test]
#[ignore = "local one-run-per-count bzip2 compression scaling diagnostic"]
fn bzip2_compression_thread_sweep() {
    let directory = tempfile::tempdir().unwrap();
    let input = input_file(&directory);
    for threads in [1, 2, 4, 6, 8, 12, 18] {
        let metrics = common::measure(&mut fbz_with_threads("bzip2", &input, threads)).unwrap();
        assert!(metrics.status.success());
        eprintln!("{threads:>2} threads: {:>7.1} ms, peak RSS {:>5.1} MiB", metrics.wall.as_secs_f64() * 1_000.0, metrics.peak_rss_bytes as f64 / 1_048_576.0,);
    }
}
