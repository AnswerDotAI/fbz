mod support;

use std::{fs, path::PathBuf, process::Command};

use fbz::{
    EncodeOptions,
    zip::{PathInput, create_to_writer},
};

#[test]
fn zip_encoder_roundtrips_files_and_directories() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("small.txt"), b"small contents").unwrap();
    fs::write(source.join("repeated.bin"), b"repeat me".repeat(200_000)).unwrap();
    fs::write(source.join("not-selected.txt"), b"must not be recursively included").unwrap();

    let mut encoded = Vec::new();
    let report = create_to_writer(
        &["", "small.txt", "repeated.bin"].map(|name| PathInput { source: source.join(name), archive_path: PathBuf::from("bundle").join(name) }),
        &mut encoded,
        EncodeOptions { threads: 4, memory_limit: 64 * 1024 * 1024, level: Some(6) },
    )
    .unwrap();
    assert_eq!(report.entries, 3);

    let path = directory.path().join("archive.zip");
    fs::write(&path, encoded).unwrap();
    assert!(Command::new("unzip").args(["-t", path.to_str().unwrap()]).status().unwrap().success());
    let small = Command::new("unzip").args(["-p", path.to_str().unwrap(), "bundle/small.txt"]).output().unwrap();
    assert!(small.status.success());
    assert_eq!(small.stdout, b"small contents");
    let repeated = Command::new("unzip").args(["-p", path.to_str().unwrap(), "bundle/repeated.bin"]).output().unwrap();
    assert!(repeated.status.success());
    assert_eq!(repeated.stdout, b"repeat me".repeat(200_000));
}

#[test]
fn zip_encoder_uses_parallel_deflate_for_a_large_entry() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("large.bin");
    let plain = support::patterned_bytes(20 * 1024 * 1024);
    fs::write(&source, &plain).unwrap();
    let mut encoded = Vec::new();
    create_to_writer(
        &[PathInput { source, archive_path: PathBuf::from("large.bin") }],
        &mut encoded,
        EncodeOptions { threads: 4, memory_limit: 64 * 1024 * 1024, level: Some(3) },
    )
    .unwrap();
    let path = directory.path().join("large.zip");
    fs::write(&path, encoded).unwrap();
    let decoded = Command::new("unzip").args(["-p", path.to_str().unwrap(), "large.bin"]).output().unwrap();
    assert!(decoded.status.success());
    assert_eq!(decoded.stdout, plain);
}
