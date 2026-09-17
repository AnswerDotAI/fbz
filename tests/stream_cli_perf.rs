#[allow(dead_code, unused_imports)]
mod common;

use std::{path::PathBuf, process::Command};

fn fixture(extension: &str) -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("meta/simplewiki-first-5pct.xml.{extension}")) }

fn fbz(input: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fbz"));
    command.args(["test", "-q", "-P", "0"]).arg(input);
    command
}

fn reference(program: &str, input: &std::path::Path) -> Command {
    let mut command = Command::new(program);
    command.arg("-t").arg(input);
    command
}

fn compare(extension: &str, program: &str) {
    let input = fixture(extension);
    assert!(fbz(&input).status().unwrap().success());
    assert!(reference(program, &input).status().unwrap().success());

    let ours = common::measure_timing(&mut fbz(&input)).unwrap();
    let theirs = common::measure_timing(&mut reference(program, &input)).unwrap();
    assert!(ours.status.success());
    assert!(theirs.status.success());
    let speedup = theirs.wall.as_secs_f64() / ours.wall.as_secs_f64();
    eprintln!("{extension}: fbz {:.3?}, {program} {:.3?}, {speedup:.2}x", ours.wall, theirs.wall);
}

#[test]
#[ignore = "local single-run bzip2 CLI comparison"]
fn bzip2_cli_comparison() { compare("bz2", "bzip2"); }

#[test]
#[ignore = "local single-run gzip CLI comparison"]
fn gzip_cli_comparison() { compare("gz", "gzip"); }
