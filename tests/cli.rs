use std::{
    fs,
    io::{Cursor, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, UNIX_EPOCH},
};

use crabz2::{Level, compress};
use flate2::{Compression, write::GzEncoder};
use lz4_flex::frame::{BlockMode as Lz4BlockMode, BlockSize as Lz4BlockSize, FrameEncoder as Lz4Encoder, FrameInfo as Lz4FrameInfo};
use zip::{CompressionMethod as ZipCompression, ZipArchive, ZipWriter, write::FullFileOptions};

mod support;
use support::{ZipMethod, zip_bytes, zip_with_modes};

fn binary() -> Command { Command::new(env!("CARGO_BIN_EXE_fbz")) }

fn write_compressed(path: &Path, plain: &[u8]) { fs::write(path, compress(plain, Level::FASTEST)).unwrap(); }

fn gzip_bytes(plain: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(plain).unwrap();
    encoder.finish().unwrap()
}

fn write_gzip(path: &Path, plain: &[u8]) { fs::write(path, gzip_bytes(plain)).unwrap(); }

fn lz4_bytes(plain: &[u8], mode: Lz4BlockMode) -> Vec<u8> {
    let info = Lz4FrameInfo::new()
        .block_size(Lz4BlockSize::Max64KB)
        .block_mode(mode)
        .block_checksums(true)
        .content_checksum(true)
        .content_size(Some(plain.len() as u64));
    let mut encoder = Lz4Encoder::with_frame_info(info, Vec::new());
    encoder.write_all(plain).unwrap();
    encoder.finish().unwrap()
}

fn linked_zip() -> Vec<u8> {
    zip_with_modes(&[
        ("nested/", b"", ZipMethod::Stored, 0o040750),
        ("nested/root.txt", b"linked contents", ZipMethod::Deflate, 0o100640),
        ("nested/symbolic.txt", b"root.txt", ZipMethod::Deflate, 0o120777),
    ])
}

fn streaming_zip64() -> Vec<u8> {
    let mut archive = ZipWriter::new_stream(Vec::new());
    let modified = 1_700_000_123_u32;
    let mut timestamp = vec![1];
    timestamp.extend_from_slice(&modified.to_le_bytes());
    let mut options = FullFileOptions::default().compression_method(ZipCompression::STORE).large_file(true).unix_permissions(0o600);
    options.add_extra_data(0x5455, timestamp, true).unwrap();
    archive.start_file("zip64.txt", options).unwrap();
    archive.write_all(b"small payload with ZIP64 fields and a data descriptor").unwrap();
    archive.finish().unwrap().into_inner()
}
fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut archive);
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o640);
            header.set_mtime(1_700_000_123);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.finish().unwrap();
    }
    archive
}

fn write_tgz(path: &Path, entries: &[(&str, &[u8])]) { write_gzip(path, &tar_bytes(entries)); }

fn archive_names(path: &Path) -> Vec<String> {
    let bytes = fs::read(path).unwrap();
    let mut names: Vec<_> = if path.extension().unwrap() == "zip" {
        ZipArchive::new(Cursor::new(bytes)).unwrap().file_names().map(|name| name.trim_end_matches('/').to_owned()).collect()
    } else {
        let tar = fbz::decompress(&bytes, Default::default()).unwrap();
        tar::Archive::new(tar.as_slice()).entries().unwrap().map(|entry| entry.unwrap().path().unwrap().display().to_string().trim_end_matches('/').to_owned()).collect()
    };
    names.sort();
    names
}

#[test]
fn archive_selection_and_preview_share_filters_across_formats() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("project");
    for dir in ["src/deep", "src/tests", "tests", "empty", ".git"] { fs::create_dir_all(root.join(dir)).unwrap(); }
    for name in [".hidden.py", "ignored.py", "local.py", "search.py", "README.md", "src/app.py", "src/deep/nested.py", "src/tests/bad.py", "tests/bad.py", ".git/config"] {
        fs::write(root.join(name), name).unwrap();
    }
    for (name, contents) in [(".gitignore", "ignored.py"), (".ignore", "local.py"), (".rgignore", "search.py")] { fs::write(root.join(name), contents).unwrap(); }
    let cases: &[(&[&str], &[&str])] = &[
        (&["-i", "-E", "tests", "-E", ".git", "--include", "*.py", "--include", "*.md", "-e", "py", "-e", "md"],
         &["project", "project/.hidden.py", "project/README.md", "project/src", "project/src/app.py", "project/src/deep", "project/src/deep/nested.py"]),
        (&["--include", "src/*", "-e", "py"], &["project", "project/src", "project/src/app.py"]),
        (&["--include", "src/**", "-e", "py", "-E", "tests"], &["project", "project/src", "project/src/app.py", "project/src/deep", "project/src/deep/nested.py"]),
    ];
    for suffix in ["zip", "tar.gz", "tar.bz2", "tar.lz4"] {
        let output = format!("archive.{suffix}");
        // An output inside the input tree must not include itself or its temporary file.
        let archive = root.join(&output);
        let packed = binary().current_dir(directory.path()).args(["c", "project", "-o"]).arg(&archive).output().unwrap();
        assert!(packed.status.success(), "{}", String::from_utf8_lossy(&packed.stderr));
        let names = archive_names(&archive);
        for name in [".hidden.py", "ignored.py", ".git/config", "empty"] { assert!(names.contains(&format!("project/{name}"))); }
        assert!(!names.contains(&format!("project/{output}")));
        let alias = root.join("../project").join(&output);
        let replaced = binary().current_dir(directory.path()).args(["c", "-f", "project", "-o"]).arg(alias).output().unwrap();
        assert!(replaced.status.success(), "{}", String::from_utf8_lossy(&replaced.stderr));
        assert_eq!(archive_names(&archive), names);
        for (flags, expected) in cases {
            let before = fs::read(&archive).unwrap();
            let preview = binary().current_dir(directory.path()).args(["c", "project", "-o"]).arg(&archive).args(*flags).arg("-n").output().unwrap();
            assert!(preview.status.success(), "{}", String::from_utf8_lossy(&preview.stderr));
            let mut preview_names: Vec<_> = std::str::from_utf8(&preview.stdout).unwrap().lines().map(|name| name.trim_end_matches('/')).collect();
            preview_names.sort();
            assert_eq!(preview_names, *expected);
            assert_eq!(fs::read(&archive).unwrap(), before);
            let packed = binary().current_dir(directory.path()).args(["c", "project", "-o"]).arg(&archive).args(*flags).arg("--force").output().unwrap();
            assert!(packed.status.success(), "{}", String::from_utf8_lossy(&packed.stderr));
            assert_eq!(archive_names(&archive), *expected);
        }
    }
}

#[test]
fn archive_selection_rejects_invalid_uses_and_dry_run_creates_nothing() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("file.py"), "data").unwrap();
    for flags in [vec!["--ignore"], vec!["-E", "*.py"], vec!["--include", "*.py"], vec!["-e", "py"], vec!["-n"]] {
        for mode in [vec!["d"], vec!["c", "-o", "out.gz"]] {
            let output = binary().current_dir(directory.path()).args(&mode).arg("file.py").args(&flags).output().unwrap();
            assert_eq!(output.status.code(), Some(2));
        }
    }
    for glob in ["[", "*.rs"] {
        let output = binary().current_dir(directory.path()).args(["c", "file.py", "-o", "out.zip", "--include", glob]).output().unwrap();
        assert!(!output.status.success());
        assert!(!directory.path().join("out.zip").exists());
    }
    let preview = binary().current_dir(directory.path()).args(["c", "-n", "file.py", "--format", "zip", "-C", "new-directory"]).output().unwrap();
    assert!(preview.status.success(), "{}", String::from_utf8_lossy(&preview.stderr));
    assert_eq!(preview.stdout, b"file.py\n");
    assert!(!directory.path().join("new-directory").exists());
}

#[test]
#[cfg(unix)]
fn archive_selection_preserves_root_and_nested_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("tree")).unwrap();
    fs::write(directory.path().join("tree/file"), "data").unwrap();
    for (link, target) in [("alias", "tree"), ("broken", "missing"), ("tree/link", "file")] { std::os::unix::fs::symlink(target, directory.path().join(link)).unwrap(); }
    for suffix in ["zip", "tar.gz"] {
        let archive = format!("archive.{suffix}");
        let output = binary().current_dir(directory.path()).args(["c", "tree", "alias", "broken", "-o", &archive]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let output = binary().current_dir(directory.path()).args([&archive, "-C", suffix]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        for (link, target) in [("alias", "tree"), ("broken", "missing"), ("tree/link", "file")] { assert_eq!(fs::read_link(directory.path().join(suffix).join(link)).unwrap(), Path::new(target)); }
    }
}

#[test]
#[cfg(unix)]
fn archive_selection_rejects_special_entries_instead_of_omitting_them() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("tree")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(directory.path().join("tree/socket")).unwrap();
    for archive in ["out.zip", "out.tar.gz"] {
        let output = binary().current_dir(directory.path()).args(["c", "tree", "-o", archive]).output().unwrap();
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported filesystem entry"));
        assert!(!directory.path().join(archive).exists());
    }
}

#[test]
fn gzip_compression_infers_format_and_interoperates() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("sample.txt");
    let output = directory.path().join("sample.txt.gz");
    let plain = support::patterned_bytes(2_000_000);
    fs::write(&input, &plain).unwrap();

    let compressed = binary().args(["c", input.to_str().unwrap(), "-o", output.to_str().unwrap(), "-P", "3"]).output().unwrap();
    assert!(compressed.status.success(), "{}", String::from_utf8_lossy(&compressed.stderr));
    assert!(Command::new("gzip").args(["-t", output.to_str().unwrap()]).status().unwrap().success());

    let decoded = binary().args([output.to_str().unwrap(), "-o", "-"]).output().unwrap();
    assert!(decoded.status.success());
    assert_eq!(decoded.stdout, plain);

    let missing_format = binary().args(["c", input.to_str().unwrap()]).output().unwrap();
    assert_eq!(missing_format.status.code(), Some(2));
}

#[test]
fn gzip_compression_supports_stdin_stdout_and_default_names() {
    let plain = b"stream compression through the public CLI".repeat(10_000);
    let mut child = binary().args(["c", "--format", "gzip", "-", "-o", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&plain).unwrap();
    let compressed = child.wait_with_output().unwrap();
    assert!(compressed.status.success());
    assert_eq!(fbz::gzip::decompress(&compressed.stdout).unwrap(), plain);

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("named");
    fs::write(&input, &plain).unwrap();
    let result = binary().args(["c", "--format", "gzip", input.to_str().unwrap()]).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(directory.path().join("named.gz").exists());

    let removed = directory.path().join("removed-after-stdout");
    fs::write(&removed, &plain).unwrap();
    let result = binary().args(["c", "--format", "gzip", "--rm", removed.to_str().unwrap(), "-o", "-"]).output().unwrap();
    assert!(result.status.success());
    assert_eq!(fbz::gzip::decompress(&result.stdout).unwrap(), plain);
    assert!(!removed.exists());
}

#[test]
fn tar_gzip_compression_streams_multiple_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.txt");
    let tree = directory.path().join("tree");
    fs::create_dir(&tree).unwrap();
    fs::write(&first, b"first contents").unwrap();
    fs::write(tree.join("second.txt"), b"second contents").unwrap();
    let archive = directory.path().join("bundle.tar.gz");

    let packed = binary().args(["c", first.to_str().unwrap(), tree.to_str().unwrap(), "-o", archive.to_str().unwrap(), "-P", "3"]).output().unwrap();
    assert!(packed.status.success(), "{}", String::from_utf8_lossy(&packed.stderr));
    assert!(Command::new("gzip").args(["-t", archive.to_str().unwrap()]).status().unwrap().success());
    let skipped = binary().args(["c", "--skip-existing", first.to_str().unwrap(), tree.to_str().unwrap(), "-o", archive.to_str().unwrap()]).output().unwrap();
    assert!(skipped.status.success());

    let destination = directory.path().join("unpacked");
    let unpacked = binary().args([archive.to_str().unwrap(), "-C", destination.to_str().unwrap()]).output().unwrap();
    assert!(unpacked.status.success(), "{}", String::from_utf8_lossy(&unpacked.stderr));
    assert_eq!(fs::read(destination.join("first.txt")).unwrap(), b"first contents");
    assert_eq!(fs::read(destination.join("tree/second.txt")).unwrap(), b"second contents");
}

#[test]
fn zip_compression_reuses_fbz_deflate_and_extracts_safely() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.txt");
    let tree = directory.path().join("tree");
    fs::create_dir(&tree).unwrap();
    fs::write(&first, b"first zip contents").unwrap();
    fs::write(tree.join("second.txt"), b"second zip contents".repeat(20_000)).unwrap();
    let archive = directory.path().join("bundle.zip");

    let packed = binary().args(["c", first.to_str().unwrap(), tree.to_str().unwrap(), "-o", archive.to_str().unwrap(), "-P", "3"]).output().unwrap();
    assert!(packed.status.success(), "{}", String::from_utf8_lossy(&packed.stderr));
    assert!(Command::new("unzip").args(["-t", archive.to_str().unwrap()]).status().unwrap().success());
    let skipped = binary().args(["c", "--skip-existing", first.to_str().unwrap(), tree.to_str().unwrap(), "-o", archive.to_str().unwrap()]).output().unwrap();
    assert!(skipped.status.success());

    let destination = directory.path().join("unzipped");
    let unpacked = binary().args([archive.to_str().unwrap(), "-C", destination.to_str().unwrap()]).output().unwrap();
    assert!(unpacked.status.success(), "{}", String::from_utf8_lossy(&unpacked.stderr));
    assert_eq!(fs::read(destination.join("first.txt")).unwrap(), b"first zip contents");
    assert_eq!(fs::read(destination.join("tree/second.txt")).unwrap(), b"second zip contents".repeat(20_000));
}

#[test]
fn lz4_compression_and_tar_composition_interoperate() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("sample.txt");
    let frame = directory.path().join("sample.txt.lz4");
    let plain = b"LZ4 compression contents".repeat(10_000);
    fs::write(&input, &plain).unwrap();

    let compressed = binary().args(["c", input.to_str().unwrap(), "-o", frame.to_str().unwrap(), "-P", "3"]).output().unwrap();
    assert!(compressed.status.success(), "{}", String::from_utf8_lossy(&compressed.stderr));
    assert!(Command::new("lz4").args(["-t", frame.to_str().unwrap()]).status().unwrap().success());
    let decoded = binary().args([frame.to_str().unwrap(), "-o", "-"]).output().unwrap();
    assert!(decoded.status.success());
    assert_eq!(decoded.stdout, plain);

    let archive = directory.path().join("bundle.tar.lz4");
    let packed = binary().args(["c", input.to_str().unwrap(), "-o", archive.to_str().unwrap(), "-P", "3"]).output().unwrap();
    assert!(packed.status.success(), "{}", String::from_utf8_lossy(&packed.stderr));
    let destination = directory.path().join("unpacked-lz4");
    let unpacked = binary().args([archive.to_str().unwrap(), "-C", destination.to_str().unwrap()]).output().unwrap();
    assert!(unpacked.status.success(), "{}", String::from_utf8_lossy(&unpacked.stderr));
    assert_eq!(fs::read(destination.join("sample.txt")).unwrap(), plain);
}

#[test]
fn bzip2_compression_and_tar_composition_interoperate() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("sample.txt");
    let stream = directory.path().join("sample.txt.bz2");
    let plain = b"bzip2 compression contents".repeat(10_000);
    fs::write(&input, &plain).unwrap();

    let compressed = binary().args(["c", input.to_str().unwrap(), "-o", stream.to_str().unwrap(), "-P", "3"]).output().unwrap();
    assert!(compressed.status.success(), "{}", String::from_utf8_lossy(&compressed.stderr));
    assert!(Command::new("bzip2").args(["-t", stream.to_str().unwrap()]).status().unwrap().success());
    let decoded = binary().args([stream.to_str().unwrap(), "-o", "-"]).output().unwrap();
    assert!(decoded.status.success());
    assert_eq!(decoded.stdout, plain);

    let archive = directory.path().join("bundle.tar.bz2");
    let packed = binary().args(["c", input.to_str().unwrap(), "-o", archive.to_str().unwrap(), "-P", "3"]).output().unwrap();
    assert!(packed.status.success(), "{}", String::from_utf8_lossy(&packed.stderr));
    let destination = directory.path().join("unpacked-bzip2");
    let unpacked = binary().args([archive.to_str().unwrap(), "-C", destination.to_str().unwrap()]).output().unwrap();
    assert!(unpacked.status.success(), "{}", String::from_utf8_lossy(&unpacked.stderr));
    assert_eq!(fs::read(destination.join("sample.txt")).unwrap(), plain);
}

fn traversal_tar() -> Vec<u8> {
    let contents = b"must stay inside destination";
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_path("safe.txt").unwrap();
    header.as_mut_bytes()[..100].fill(0);
    header.as_mut_bytes()[..14].copy_from_slice(b"../outside.txt");
    header.set_cksum();

    let mut archive = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut archive);
        builder.append(&header, contents.as_slice()).unwrap();
        builder.finish().unwrap();
    }
    archive
}

fn linked_tar() -> Vec<u8> {
    let contents = b"linked contents";
    let mut archive = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut archive);
        let mut file = tar::Header::new_gnu();
        file.set_size(contents.len() as u64);
        file.set_mode(0o640);
        file.set_mtime(1_700_000_123);
        file.set_cksum();
        builder.append_data(&mut file, "root.txt", contents.as_slice()).unwrap();

        for (path, entry_type) in [("symbolic.txt", tar::EntryType::Symlink), ("hard.txt", tar::EntryType::Link)] {
            let mut link = tar::Header::new_gnu();
            link.set_path(path).unwrap();
            link.set_link_name("root.txt").unwrap();
            link.set_entry_type(entry_type);
            link.set_size(0);
            link.set_mode(0o777);
            link.set_mtime(1_700_000_123);
            link.set_cksum();
            builder.append(&link, std::io::empty()).unwrap();
        }
        builder.finish().unwrap();
    }
    archive
}

#[test]
fn decode_test_index_and_list() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("sample.bz2");
    let output = directory.path().join("sample");
    let index = directory.path().join("sample.fbz2i");
    let plain = support::patterned_bytes(250_000);
    write_compressed(&input, &plain);

    let decoded = binary().args([input.to_str().unwrap(), "-P", "2"]).status().unwrap();
    assert!(decoded.success());
    assert_eq!(fs::read(&output).unwrap(), plain);

    let stdout = binary().args([input.to_str().unwrap(), "-o", "-"]).output().unwrap();
    assert!(stdout.status.success());
    assert_eq!(stdout.stdout, plain);

    let tested = binary().args(["test", input.to_str().unwrap()]).status().unwrap();
    assert!(tested.success());

    let indexed = binary().args(["index", input.to_str().unwrap(), "-o", index.to_str().unwrap()]).status().unwrap();
    assert!(indexed.success());
    assert!(fs::metadata(index).unwrap().len() > 100);

    let listed = binary().args(["list", input.to_str().unwrap()]).output().unwrap();
    assert!(listed.status.success());
    assert!(String::from_utf8(listed.stdout).unwrap().contains("blocks\t"));

    let conflicting = binary().args(["test", "--json", input.to_str().unwrap()]).output().unwrap();
    assert_eq!(conflicting.status.code(), Some(2));
}

#[test]
fn commands_aliases_and_implicit_paths_are_unambiguous() {
    let directory = tempfile::tempdir().unwrap();
    let plain = b"command selection";
    write_gzip(&directory.path().join("payload.gz"), plain);
    fs::write(directory.path().join("plain"), plain).unwrap();
    for command in ["c", "compress"] {
        let output = binary().current_dir(directory.path()).args([command, "plain", "--format", "gzip", "-o", "-"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(fbz::gzip::decompress(&output.stdout).unwrap(), plain);
    }
    for args in [vec!["payload.gz"], vec!["d", "payload.gz"], vec!["decompress", "payload.gz"],
        vec!["-qP1", "d", "payload.gz"], vec!["-P", "1", "payload.gz"], vec!["--memory-limit=64M", "payload.gz"]] {
        let output = binary().current_dir(directory.path()).args(args).args(["-o", "-"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.stdout, plain);
    }
    for command in ["l", "list"] {
        let output = binary().current_dir(directory.path()).args([command, "payload.gz", "--json"]).output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    }
    let output = binary().current_dir(directory.path()).args(["-C", "test", "payload.gz"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read(directory.path().join("test/payload")).unwrap(), plain);
    write_gzip(&directory.path().join("c"), plain);
    assert_eq!(binary().current_dir(directory.path()).arg("c").output().unwrap().status.code(), Some(2));
    for args in [vec!["d", "c", "-o", "-"], vec!["-o-", "./c"], vec!["-o", "-", "--", "c"]] {
        let output = binary().current_dir(directory.path()).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.stdout, plain);
    }
    let output = binary().current_dir(directory.path()).args(["payload.gz", "-C", "out", "c"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read(directory.path().join("out/c.out")).unwrap(), plain);
}

#[test]
fn command_help_and_options_are_scoped() {
    for flag in ["-h", "--help"] {
        let root = String::from_utf8(binary().arg(flag).output().unwrap().stdout).unwrap();
        assert!(root.contains("fbz <command> -h") && root.contains("fbz c -h"));
        assert!(root.contains("filters (-i/-E/-e)") && root.contains("extraction (-x)") && root.contains("JSON (--json)"));
        assert!(root.contains("Size limit (--max-output)") && root.contains("skip existing, size limit"));
    }
    let help = |command: &str| String::from_utf8(binary().args([command, "--help"]).output().unwrap().stdout).unwrap();
    assert!(help("c").contains("--include") && !help("c").contains("--max-output"));
    assert!(help("d").contains("--extract") && !help("d").contains("--include"));
    assert!(help("l").contains("--json") && !help("l").contains("--output"));
    assert!(!help("test").contains("--force") && !help("index").contains("--output-dir"));
    for mode in ["-z", "--compress", "--list", "--test", "--index"] {
        assert_eq!(binary().args([mode, "file.gz"]).output().unwrap().status.code(), Some(2));
    }
}

#[test]
fn stdin_decodes_to_stdout() {
    let plain = b"stdin uses the same decode options";
    let compressed = compress(plain, Level::BEST);
    let mut child = binary().args(["-", "-P", "2", "--memory-limit", "128M"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&compressed).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, plain);
}

#[test]
fn corruption_has_distinct_exit_status_and_atomic_output() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("corrupt.bz2");
    let output = directory.path().join("output");
    let mut compressed = compress(b"corrupt me", Level::BEST);
    let last = compressed.len() - 2;
    compressed[last] ^= 1;
    fs::write(&input, compressed).unwrap();

    let result = binary().args([input.to_str().unwrap(), "-o", output.to_str().unwrap()]).status().unwrap();
    assert_eq!(result.code(), Some(3));
    assert!(!output.exists());
}

#[test]
fn multiple_inputs_support_output_directory_and_skip_existing() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.bz2");
    let second = directory.path().join("second.bz2");
    let output_dir = directory.path().join("decoded");
    write_compressed(&first, b"first contents");
    write_compressed(&second, b"second contents");

    let decoded = binary().args(["-C", output_dir.to_str().unwrap(), first.to_str().unwrap(), second.to_str().unwrap()]).output().unwrap();
    assert!(decoded.status.success(), "{}", String::from_utf8_lossy(&decoded.stderr));
    assert_eq!(fs::read(output_dir.join("first")).unwrap(), b"first contents");
    assert_eq!(fs::read(output_dir.join("second")).unwrap(), b"second contents");

    fs::write(output_dir.join("first"), b"keep me").unwrap();
    let skipped = binary().args(["--skip-existing", "-C", output_dir.to_str().unwrap(), first.to_str().unwrap(), second.to_str().unwrap()]).output().unwrap();
    assert!(skipped.status.success());
    assert_eq!(fs::read(output_dir.join("first")).unwrap(), b"keep me");
    assert!(String::from_utf8(skipped.stderr).unwrap().contains("skipping existing"));

    let quiet =
        binary().args(["--quiet", "--skip-existing", "-C", output_dir.to_str().unwrap(), first.to_str().unwrap(), second.to_str().unwrap()]).output().unwrap();
    assert!(quiet.status.success());
    assert!(quiet.stderr.is_empty());

    let replaced = binary().args(["--force", "-C", output_dir.to_str().unwrap(), first.to_str().unwrap(), second.to_str().unwrap()]).output().unwrap();
    assert!(replaced.status.success());
    assert_eq!(fs::read(output_dir.join("first")).unwrap(), b"first contents");

    let rejected = binary().args([first.to_str().unwrap(), second.to_str().unwrap(), "-o", output_dir.join("one").to_str().unwrap()]).output().unwrap();
    assert_eq!(rejected.status.code(), Some(2));
}

#[test]
fn remove_input_happens_only_after_success() {
    let directory = tempfile::tempdir().unwrap();
    let valid = directory.path().join("valid.bz2");
    let corrupt = directory.path().join("corrupt.bz2");
    write_compressed(&valid, b"remove after success");
    fs::write(&corrupt, b"not bzip2").unwrap();

    let decoded = binary().args(["--rm", valid.to_str().unwrap()]).output().unwrap();
    assert!(decoded.status.success());
    assert!(!valid.exists());
    assert_eq!(fs::read(directory.path().join("valid")).unwrap(), b"remove after success");

    let failed = binary().args(["--rm", corrupt.to_str().unwrap()]).output().unwrap();
    assert!(!failed.status.success());
    assert!(corrupt.exists());
}

#[test]
fn decode_preserves_modified_time_and_permissions() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("metadata.bz2");
    let output = directory.path().join("metadata");
    write_compressed(&input, b"metadata");
    let modified = UNIX_EPOCH + Duration::from_secs(1_700_000_123);
    fs::OpenOptions::new().write(true).open(&input).unwrap().set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&input, fs::Permissions::from_mode(0o640)).unwrap();
    }

    let decoded = binary().arg(input.to_str().unwrap()).output().unwrap();
    assert!(decoded.status.success());
    let input_metadata = fs::metadata(input).unwrap();
    let output_metadata = fs::metadata(output).unwrap();
    assert_eq!(output_metadata.modified().unwrap(), input_metadata.modified().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(output_metadata.permissions().mode() & 0o777, 0o640);
    }
}

#[test]
fn max_output_is_enforced_before_persisting() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("limited.bz2");
    let output = directory.path().join("limited");
    let plain = vec![b'x'; 20_000];
    write_compressed(&input, &plain);

    let rejected = binary().args(["--max-output", "19999", input.to_str().unwrap()]).output().unwrap();
    assert_eq!(rejected.status.code(), Some(3));
    assert!(!output.exists());
    assert!(String::from_utf8(rejected.stderr).unwrap().contains("decoded output exceeds"));

    let accepted = binary().args(["--max-output", "20K", input.to_str().unwrap()]).output().unwrap();
    assert!(accepted.status.success());
    assert_eq!(fs::read(output).unwrap(), plain);
}

#[test]
fn list_json_describes_one_or_many_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.bz2");
    let second = directory.path().join("second.bz2");
    write_compressed(&first, b"first");
    write_compressed(&second, b"second");

    let single = binary().args(["list", "--json", first.to_str().unwrap()]).output().unwrap();
    assert!(single.status.success());
    let value: serde_json::Value = serde_json::from_slice(&single.stdout).unwrap();
    assert_eq!(value["input"], first.to_str().unwrap());
    assert_eq!(value["decoded_bytes"], 5);
    assert_eq!(value["streams"].as_array().unwrap().len(), 1);
    assert_eq!(value["blocks"].as_array().unwrap().len(), 1);

    let multiple = binary().args(["list", "--json", first.to_str().unwrap(), second.to_str().unwrap()]).output().unwrap();
    assert!(multiple.status.success());
    let value: serde_json::Value = serde_json::from_slice(&multiple.stdout).unwrap();
    assert_eq!(value.as_array().unwrap().len(), 2);

    let limited = binary().args(["list", "--max-output", "4", first.to_str().unwrap()]).output().unwrap();
    assert_eq!(limited.status.code(), Some(3));

    let missing_mode = binary().args(["--json", first.to_str().unwrap()]).output().unwrap();
    assert_eq!(missing_mode.status.code(), Some(2));
}

#[test]
fn gzip_extension_selects_decoder_across_cli_modes() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("sample.gz");
    let output = directory.path().join("sample");
    let plain = support::patterned_bytes(250_000);
    write_gzip(&input, &plain);

    let decoded = binary().arg(input.to_str().unwrap()).output().unwrap();
    assert!(decoded.status.success(), "{}", String::from_utf8_lossy(&decoded.stderr));
    assert_eq!(fs::read(output).unwrap(), plain);

    let tested = binary().args(["test", input.to_str().unwrap()]).output().unwrap();
    assert!(tested.status.success());

    let listed = binary().args(["list", "--json", input.to_str().unwrap()]).output().unwrap();
    assert!(listed.status.success());
    let value: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(value["format"], "gzip");
    assert_eq!(value["decoded_bytes"], plain.len());
    assert_eq!(value["members"].as_array().unwrap().len(), 1);
    assert!(!value["blocks"].as_array().unwrap().is_empty());

    let indexed = binary().args(["index", input.to_str().unwrap()]).output().unwrap();
    assert_eq!(indexed.status.code(), Some(2));
    assert!(String::from_utf8(indexed.stderr).unwrap().contains("only for bzip2"));
}

#[test]
fn gzip_magic_fallback_stdin_limits_and_corruption_work() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("mystery.data");
    let output = directory.path().join("mystery.data.out");
    let plain = b"gzip magic fallback".repeat(2_000);
    write_gzip(&input, &plain);

    let decoded = binary().arg(input.to_str().unwrap()).output().unwrap();
    assert!(decoded.status.success());
    assert_eq!(fs::read(&output).unwrap(), plain);
    fs::remove_file(&output).unwrap();

    let compressed = fs::read(&input).unwrap();
    let mut child = binary().arg("-").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&compressed).unwrap();
    let stdin = child.wait_with_output().unwrap();
    assert!(stdin.status.success());
    assert_eq!(stdin.stdout, plain);

    let limited = binary().args(["--max-output", "1K", input.to_str().unwrap()]).output().unwrap();
    assert_eq!(limited.status.code(), Some(3));
    assert!(!output.exists());

    let mut corrupt = compressed;
    let crc = corrupt.len() - 8;
    corrupt[crc] ^= 1;
    fs::write(&input, corrupt).unwrap();
    let rejected = binary().arg(input.to_str().unwrap()).output().unwrap();
    assert_eq!(rejected.status.code(), Some(3));
    assert!(!output.exists());
}

#[test]
fn lz4_extension_magic_reporting_limits_and_corruption_work() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("sample.lz4");
    let output = directory.path().join("sample");
    let plain = support::patterned_bytes(2_000_000);
    let encoded = lz4_bytes(&plain, Lz4BlockMode::Independent);
    fs::write(&input, &encoded).unwrap();

    let decoded = binary().args(["-P", "4", input.to_str().unwrap()]).output().unwrap();
    assert!(decoded.status.success(), "{}", String::from_utf8_lossy(&decoded.stderr));
    assert_eq!(fs::read(&output).unwrap(), plain);

    let tested = binary().args(["test", "-P", "4", input.to_str().unwrap()]).output().unwrap();
    assert!(tested.status.success());
    let listed = binary().args(["list", "--json", "-P", "4", input.to_str().unwrap()]).output().unwrap();
    assert!(listed.status.success());
    let value: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(value["format"], "lz4");
    assert_eq!(value["decoded_bytes"], plain.len());
    assert_eq!(value["frames"][0]["block_mode"], "independent");
    assert!(value["blocks"].as_array().unwrap().len() > 1);

    let magic_input = directory.path().join("mystery.data");
    fs::write(&magic_input, &encoded).unwrap();
    let magic_output = directory.path().join("mystery.data.out");
    let magic = binary().args(["-P", "4", magic_input.to_str().unwrap()]).output().unwrap();
    assert!(magic.status.success());
    assert_eq!(fs::read(&magic_output).unwrap(), plain);

    fs::remove_file(&output).unwrap();
    let limited = binary().args(["--max-output", "1K", input.to_str().unwrap()]).output().unwrap();
    assert_eq!(limited.status.code(), Some(3));
    assert!(!output.exists());

    let mut corrupt = encoded;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    fs::write(&input, corrupt).unwrap();
    let rejected = binary().arg(input.to_str().unwrap()).output().unwrap();
    assert_eq!(rejected.status.code(), Some(3));
    assert!(!output.exists());
}

#[test]
fn linked_tar_lz4_streams_through_the_shared_extractor() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("bundle.tar.lz4");
    let output = directory.path().join("unpacked");
    let plain_tar = tar_bytes(&[("first.txt", b"first"), ("nested/second.txt", b"second")]);
    fs::write(&input, lz4_bytes(&plain_tar, Lz4BlockMode::Linked)).unwrap();

    let extracted = binary().args(["-P", "4", "-C", output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert!(extracted.status.success(), "{}", String::from_utf8_lossy(&extracted.stderr));
    assert_eq!(fs::read(output.join("first.txt")).unwrap(), b"first");
    assert_eq!(fs::read(output.join("nested/second.txt")).unwrap(), b"second");
}

#[test]
fn mixed_bzip2_gzip_and_tar_inputs_share_output_policy() {
    let directory = tempfile::tempdir().unwrap();
    let bzip2 = directory.path().join("first.bz2");
    let gzip = directory.path().join("second.gz");
    let tgz = directory.path().join("bundle.tgz");
    let output_dir = directory.path().join("decoded");
    write_compressed(&bzip2, b"bzip2");
    write_gzip(&gzip, b"gzip");
    write_tgz(&tgz, &[("from-tar.txt", b"tar payload")]);

    let decoded = binary().args(["-C", output_dir.to_str().unwrap(), bzip2.to_str().unwrap(), gzip.to_str().unwrap(), tgz.to_str().unwrap()]).output().unwrap();
    assert!(decoded.status.success(), "{}", String::from_utf8_lossy(&decoded.stderr));
    assert_eq!(fs::read(output_dir.join("first")).unwrap(), b"bzip2");
    assert_eq!(fs::read(output_dir.join("second")).unwrap(), b"gzip");
    assert_eq!(fs::read(output_dir.join("from-tar.txt")).unwrap(), b"tar payload");
}

#[test]
fn tar_gzip_and_bzip2_auto_extract_or_decode_raw() {
    let directory = tempfile::tempdir().unwrap();
    let tar_gzip = directory.path().join("bundle.tar.gz");
    let tar_bzip2 = directory.path().join("bundle.tar.bz2");
    let gzip_output = directory.path().join("from-gzip");
    let bzip2_output = directory.path().join("from-bzip2");
    let raw_output = directory.path().join("bundle.tar");
    let long_path = format!("nested/{}/contents.txt", "long-segment-".repeat(10));
    let entries = [("root.txt", b"root contents".as_slice()), (long_path.as_str(), b"nested contents".as_slice())];
    let plain_tar = tar_bytes(&entries);
    write_gzip(&tar_gzip, &plain_tar);
    write_compressed(&tar_bzip2, &plain_tar);

    let gzip = binary().args(["-C", gzip_output.to_str().unwrap(), tar_gzip.to_str().unwrap()]).output().unwrap();
    assert!(gzip.status.success(), "{}", String::from_utf8_lossy(&gzip.stderr));
    let bzip2 = binary().args(["-C", bzip2_output.to_str().unwrap(), tar_bzip2.to_str().unwrap()]).output().unwrap();
    assert!(bzip2.status.success(), "{}", String::from_utf8_lossy(&bzip2.stderr));
    for output in [&gzip_output, &bzip2_output] {
        assert_eq!(fs::read(output.join("root.txt")).unwrap(), b"root contents");
        assert_eq!(fs::read(output.join(&long_path)).unwrap(), b"nested contents");
    }

    let raw = binary().args([tar_gzip.to_str().unwrap(), "-o", raw_output.to_str().unwrap()]).output().unwrap();
    assert!(raw.status.success());
    assert_eq!(fs::read(raw_output).unwrap(), plain_tar);
}

#[test]
fn tar_extraction_is_validated_before_entries_are_committed() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("policy.tgz");
    let output = directory.path().join("output");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("existing.txt"), b"keep me").unwrap();
    write_tgz(&input, &[("new.txt", b"new"), ("existing.txt", b"replacement")]);

    let skipped_output = directory.path().join("skipped");
    let skipped = binary().args(["--skip-existing", "-C", skipped_output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert_eq!(skipped.status.code(), Some(2));
    assert!(!skipped_output.exists());

    let rejected = binary().args(["-C", output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    assert_eq!(fs::read(output.join("existing.txt")).unwrap(), b"keep me");
    assert!(!output.join("new.txt").exists());

    let replaced = binary().args(["--force", "-C", output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert!(replaced.status.success(), "{}", String::from_utf8_lossy(&replaced.stderr));
    assert_eq!(fs::read(output.join("existing.txt")).unwrap(), b"replacement");
    assert_eq!(fs::read(output.join("new.txt")).unwrap(), b"new");

    fs::remove_dir_all(&output).unwrap();
    let mut corrupt = fs::read(&input).unwrap();
    let crc = corrupt.len() - 8;
    corrupt[crc] ^= 1;
    fs::write(&input, corrupt).unwrap();
    let corrupt_result = binary().args(["--rm", "-C", output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert_eq!(corrupt_result.status.code(), Some(3));
    assert!(input.exists());
    assert!(!output.join("new.txt").exists());
}

#[test]
fn explicit_extract_supports_stdin_limits_and_rejects_traversal() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    let plain_tar = tar_bytes(&[("stdin.txt", b"streamed")]);
    let compressed = gzip_bytes(&plain_tar);

    let mut child = binary().args(["--extract", "-C", output.to_str().unwrap(), "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&compressed).unwrap();
    let extracted = child.wait_with_output().unwrap();
    assert!(extracted.status.success(), "{}", String::from_utf8_lossy(&extracted.stderr));
    assert_eq!(fs::read(output.join("stdin.txt")).unwrap(), b"streamed");

    let limited = directory.path().join("limited");
    let limit = (plain_tar.len() - 1).to_string();
    let mut limited_child = binary().args(["--extract", "--max-output", &limit, "-C", limited.to_str().unwrap(), "-"]).stdin(Stdio::piped()).spawn().unwrap();
    limited_child.stdin.take().unwrap().write_all(&compressed).unwrap();
    let limited_result = limited_child.wait().unwrap();
    assert_eq!(limited_result.code(), Some(3));
    assert!(!limited.join("stdin.txt").exists());

    let traversal = directory.path().join("traversal.tgz");
    write_gzip(&traversal, &traversal_tar());
    let traversal_output = directory.path().join("traversal-output");
    let result = binary().args(["-C", traversal_output.to_str().unwrap(), traversal.to_str().unwrap()]).output().unwrap();
    assert!(result.status.success());
    assert_eq!(fs::read_dir(&traversal_output).unwrap().count(), 0);
    assert!(!directory.path().join("outside.txt").exists());
}
#[test]
#[cfg(unix)]
fn tar_staging_preserves_symbolic_and_hard_links() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("links.tgz");
    let output = directory.path().join("output");
    write_gzip(&input, &linked_tar());

    let result = binary().args(["--rm", "-C", output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(!input.exists());
    assert_eq!(fs::read_link(output.join("symbolic.txt")).unwrap(), Path::new("root.txt"));
    assert_eq!(fs::read(output.join("hard.txt")).unwrap(), b"linked contents");
    let root_metadata = fs::metadata(output.join("root.txt")).unwrap();
    assert_eq!(root_metadata.ino(), fs::metadata(output.join("hard.txt")).unwrap().ino());
    assert_eq!(root_metadata.permissions().mode() & 0o777, 0o640);
    assert_eq!(root_metadata.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs(), 1_700_000_123);
}

#[test]
#[cfg(unix)]
fn zip_auto_extracts_validates_lists_and_preserves_links() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("mixed.zip");
    let output = directory.path().join("output");
    fs::write(&input, linked_zip()).unwrap();

    let tested = binary().args(["test", input.to_str().unwrap()]).output().unwrap();
    assert!(tested.status.success(), "{}", String::from_utf8_lossy(&tested.stderr));

    let listed = binary().args(["list", "--json", input.to_str().unwrap()]).output().unwrap();
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let listing: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listing["format"], "zip");
    assert_eq!(listing["entries"].as_array().unwrap().len(), 3);

    let extracted = binary().args(["-C", output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert!(extracted.status.success(), "{}", String::from_utf8_lossy(&extracted.stderr));
    assert_eq!(fs::read(output.join("nested/root.txt")).unwrap(), b"linked contents");
    assert_eq!(fs::read_link(output.join("nested/symbolic.txt")).unwrap(), Path::new("root.txt"));
    assert_eq!(fs::metadata(output.join("nested/root.txt")).unwrap().permissions().mode() & 0o777, 0o640);
    assert_eq!(fs::metadata(output.join("nested")).unwrap().permissions().mode() & 0o777, 0o750);

    let raw = binary().args([input.to_str().unwrap(), "-o", directory.path().join("raw").to_str().unwrap()]).output().unwrap();
    assert_eq!(raw.status.code(), Some(2));
}

#[test]
fn zip_stored_deflate_limits_corruption_and_paths_are_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("data.zip");
    let plain = b"parallel deflate entry".repeat(10_000);
    let stored = b"stored bytes".repeat(1_000);
    let archive = zip_bytes(&[("deflated.txt", &plain, ZipMethod::Deflate), ("stored.bin", &stored, ZipMethod::Stored)]);
    fs::write(&input, &archive).unwrap();

    let limited = directory.path().join("limited");
    let result = binary()
        .args(["--max-output", &(plain.len() + stored.len() - 1).to_string(), "-C", limited.to_str().unwrap(), input.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(3));
    assert_eq!(fs::read_dir(&limited).unwrap().count(), 0);

    let mut corrupt = archive.clone();
    let data_byte = {
        let mut parsed = ZipArchive::new(Cursor::new(corrupt.as_slice())).unwrap();
        let entry = parsed.by_index_raw(0).unwrap();
        entry.data_start().unwrap() as usize + entry.compressed_size() as usize / 2
    };
    corrupt[data_byte] ^= 0x40;
    fs::write(&input, corrupt).unwrap();
    let output = directory.path().join("corrupt-output");
    let result = binary().args(["-C", output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert_eq!(result.status.code(), Some(3));
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);

    let mut traversal = zip_bytes(&[("aa/x.txt", b"outside", ZipMethod::Deflate)]);
    for offset in 0..=traversal.len() - b"aa/x.txt".len() {
        if &traversal[offset..offset + b"aa/x.txt".len()] == b"aa/x.txt" { traversal[offset..offset + b"../x.txt".len()].copy_from_slice(b"../x.txt"); }
    }
    fs::write(&input, traversal).unwrap();
    let traversal_output = directory.path().join("traversal-output");
    let result = binary().args(["-C", traversal_output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert_eq!(result.status.code(), Some(3));
    assert_eq!(fs::read_dir(&traversal_output).unwrap().count(), 0);
    assert!(!directory.path().join("x.txt").exists());
}

#[test]
fn zip64_and_streaming_data_descriptors_use_the_same_extractor() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("streaming.zip");
    let output = directory.path().join("output");
    fs::write(&input, streaming_zip64()).unwrap();
    let result = binary().args(["-C", output.to_str().unwrap(), input.to_str().unwrap()]).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let extracted = output.join("zip64.txt");
    assert_eq!(fs::read(&extracted).unwrap(), b"small payload with ZIP64 fields and a data descriptor");
    assert_eq!(fs::metadata(extracted).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs(), 1_700_000_123);
}

#[test]
fn zip_rejects_encryption_unsupported_methods_and_duplicate_paths() {
    fn header(archive: &[u8], signature: &[u8; 4]) -> usize { archive.windows(signature.len()).position(|bytes| bytes == signature).unwrap() }
    fn rejected(directory: &Path, name: &str, archive: &[u8], expected: &str) {
        let input = directory.join(name);
        fs::write(&input, archive).unwrap();
        let result = binary().args(["test", input.to_str().unwrap()]).output().unwrap();
        assert_eq!(result.status.code(), Some(3), "{name}: {}", String::from_utf8_lossy(&result.stderr));
        assert!(String::from_utf8_lossy(&result.stderr).contains(expected), "{}", String::from_utf8_lossy(&result.stderr));
    }

    let directory = tempfile::tempdir().unwrap();
    let mut encrypted = zip_bytes(&[("secret.txt", b"not actually encrypted", ZipMethod::Stored)]);
    let local = header(&encrypted, b"PK\x03\x04");
    let central = header(&encrypted, b"PK\x01\x02");
    encrypted[local + 6] |= 1;
    encrypted[central + 8] |= 1;
    rejected(directory.path(), "encrypted.zip", &encrypted, "encrypted entry");

    let mut unsupported = zip_bytes(&[("modern.txt", b"unsupported codec", ZipMethod::Stored)]);
    let local = header(&unsupported, b"PK\x03\x04");
    let central = header(&unsupported, b"PK\x01\x02");
    unsupported[local + 8..local + 10].copy_from_slice(&93_u16.to_le_bytes());
    unsupported[central + 10..central + 12].copy_from_slice(&93_u16.to_le_bytes());
    rejected(directory.path(), "unsupported.zip", &unsupported, "unsupported compression method 93");

    let duplicate = zip_bytes(&[("same.txt", b"first", ZipMethod::Deflate), ("same.txt", b"second", ZipMethod::Deflate)]);
    rejected(directory.path(), "duplicate.zip", &duplicate, "central directory contains duplicate entry names");
}
