//! ZIP archive creation using fbz's raw-DEFLATE encoder.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use rayon::ThreadPoolBuilder;

use crate::{
    EncodeOptions, Error, Result, deflate,
    pipeline::{Job, PipelineLimits, run_ordered},
};

const UTF8_DATA_DESCRIPTOR: u16 = 0x0808;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
const SMALL_WORKING_MEMORY: usize = 8 * 1024 * 1024;
const SINGLE_ENTRY_PARALLEL: u64 = 16 * 1024 * 1024;
const MULTI_ENTRY_PARALLEL: u64 = 64 * 1024 * 1024;

/// One archive entry. Directories are stored without recursively adding their contents.
#[derive(Clone, Debug)]
pub struct PathInput { pub source: PathBuf, pub archive_path: PathBuf }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind { File, Directory, Symlink }

#[derive(Clone, Debug)]
struct Entry {
    source: PathBuf,
    name: String,
    kind: Kind,
    size: u64,
    mode: u32,
    modified: Option<u32>,
}

#[derive(Clone, Debug)]
struct CentralEntry {
    name: String,
    method: u16,
    crc: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_offset: u64,
    mode: u32,
    modified: Option<u32>,
    zip64: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodeReport { pub entries: usize, pub input_len: u64, pub output_len: u64 }

fn invalid(message: impl Into<String>) -> Error { Error::InvalidZip(message.into()) }

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let kind = if metadata.file_type().is_symlink() { 0o120000 } else if metadata.is_dir() { 0o040000 } else if metadata.file_type().is_file() { 0o100000 } else if metadata.file_type().is_fifo() { 0o010000 } else { 0 };
    kind | metadata.permissions().mode()
}

#[cfg(not(unix))]
fn mode(metadata: &fs::Metadata) -> u32 { if metadata.is_dir() { 0o040755 } else { 0o100644 } }

fn modified(metadata: &fs::Metadata) -> Option<u32> { metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs().try_into().ok() }

#[cfg(unix)]
fn symlink_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(fs::read_link(path)?.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn symlink_bytes(path: &Path) -> Result<Vec<u8>> { Ok(fs::read_link(path)?.as_os_str().to_string_lossy().into_owned().into_bytes()) }

fn zip_name(path: &Path, directory: bool) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| invalid(format!("ZIP path {} is not UTF-8", path.display())))?),
            std::path::Component::CurDir => {}
            _ => return Err(invalid(format!("unsafe ZIP path {}", path.display()))),
        }
    }
    if parts.is_empty() { return Err(invalid("empty ZIP path")); }
    let mut name = parts.join("/");
    if directory { name.push('/'); }
    Ok(name)
}

fn entry(input: &PathInput) -> Result<Entry> {
    let PathInput { source, archive_path } = input;
    let metadata = fs::symlink_metadata(source)?;
    let kind = if metadata.file_type().is_symlink() { Kind::Symlink } else if metadata.is_dir() { Kind::Directory } else if metadata.is_file() { Kind::File } else { return Err(invalid(format!("unsupported filesystem entry {}", source.display()))); };
    let size = match kind { Kind::File => metadata.len(), Kind::Symlink => symlink_bytes(source)?.len() as u64, Kind::Directory => 0 };
    Ok(Entry {
        source: source.to_path_buf(),
        name: zip_name(archive_path, kind == Kind::Directory)?,
        kind,
        size,
        mode: mode(&metadata),
        modified: modified(&metadata),
    })
}

struct CountingWriter<W> { inner: W, position: u64 }

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.position += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> { self.inner.flush() }
}

fn push_u16(output: &mut impl Write, value: u16) -> io::Result<()> { output.write_all(&value.to_le_bytes()) }
fn push_u32(output: &mut impl Write, value: u32) -> io::Result<()> { output.write_all(&value.to_le_bytes()) }
fn push_u64(output: &mut impl Write, value: u64) -> io::Result<()> { output.write_all(&value.to_le_bytes()) }

fn timestamp_extra(modified: Option<u32>) -> Vec<u8> {
    let Some(modified) = modified else { return Vec::new() };
    let mut extra = Vec::with_capacity(9);
    extra.extend_from_slice(&0x5455_u16.to_le_bytes());
    extra.extend_from_slice(&5_u16.to_le_bytes());
    extra.push(1);
    extra.extend_from_slice(&modified.to_le_bytes());
    extra
}

fn may_need_zip64(size: u64) -> bool { size >= 0xff00_0000 }

fn write_local_header(output: &mut impl Write, entry: &Entry, method: u16, zip64: bool) -> Result<()> {
    let name = entry.name.as_bytes();
    let mut extra = Vec::new();
    if zip64 {
        extra.extend_from_slice(&1_u16.to_le_bytes());
        extra.extend_from_slice(&16_u16.to_le_bytes());
        extra.extend_from_slice(&entry.size.to_le_bytes());
        extra.extend_from_slice(&0_u64.to_le_bytes());
    }
    extra.extend_from_slice(&timestamp_extra(entry.modified));
    push_u32(output, 0x0403_4b50)?;
    push_u16(output, if zip64 { 45 } else { 20 })?;
    push_u16(output, UTF8_DATA_DESCRIPTOR)?;
    push_u16(output, method)?;
    push_u16(output, 0)?;
    push_u16(output, 0)?;
    push_u32(output, 0)?;
    push_u32(output, if zip64 { u32::MAX } else { 0 })?;
    push_u32(output, if zip64 { u32::MAX } else { 0 })?;
    push_u16(output, name.len().try_into().map_err(|_| invalid("ZIP path exceeds 65535 bytes"))?)?;
    push_u16(output, extra.len().try_into().map_err(|_| invalid("ZIP local extra data is too large"))?)?;
    output.write_all(name)?;
    output.write_all(&extra)?;
    Ok(())
}

fn write_descriptor(output: &mut impl Write, crc: u32, compressed: u64, uncompressed: u64, zip64: bool) -> Result<()> {
    push_u32(output, 0x0807_4b50)?;
    push_u32(output, crc)?;
    if zip64 {
        push_u64(output, compressed)?;
        push_u64(output, uncompressed)?;
    }
    else {
        push_u32(output, compressed.try_into().map_err(|_| invalid("compressed ZIP entry unexpectedly requires Zip64"))?)?;
        push_u32(output, uncompressed.try_into().map_err(|_| invalid("ZIP entry unexpectedly requires Zip64"))?)?;
    }
    Ok(())
}

fn write_central(output: &mut impl Write, entry: &CentralEntry) -> Result<()> {
    let name = entry.name.as_bytes();
    let size64 = entry.uncompressed_size > u32::MAX as u64;
    let compressed64 = entry.compressed_size > u32::MAX as u64;
    let offset64 = entry.local_offset > u32::MAX as u64;
    let mut zip64_values = Vec::new();
    if size64 { zip64_values.extend_from_slice(&entry.uncompressed_size.to_le_bytes()) }
    if compressed64 { zip64_values.extend_from_slice(&entry.compressed_size.to_le_bytes()) }
    if offset64 { zip64_values.extend_from_slice(&entry.local_offset.to_le_bytes()) }
    let mut extra = Vec::new();
    if !zip64_values.is_empty() {
        extra.extend_from_slice(&1_u16.to_le_bytes());
        extra.extend_from_slice(&(zip64_values.len() as u16).to_le_bytes());
        extra.extend_from_slice(&zip64_values);
    }
    extra.extend_from_slice(&timestamp_extra(entry.modified));
    let needed = if entry.zip64 || !zip64_values.is_empty() { 45 } else { 20 };
    push_u32(output, 0x0201_4b50)?;
    push_u16(output, (3 << 8) | needed)?;
    push_u16(output, needed)?;
    push_u16(output, UTF8_DATA_DESCRIPTOR)?;
    push_u16(output, entry.method)?;
    push_u16(output, 0)?;
    push_u16(output, 0)?;
    push_u32(output, entry.crc)?;
    push_u32(output, if compressed64 { u32::MAX } else { entry.compressed_size as u32 })?;
    push_u32(output, if size64 { u32::MAX } else { entry.uncompressed_size as u32 })?;
    push_u16(output, name.len().try_into().map_err(|_| invalid("ZIP path exceeds 65535 bytes"))?)?;
    push_u16(output, extra.len().try_into().map_err(|_| invalid("ZIP central extra data is too large"))?)?;
    push_u16(output, 0)?;
    push_u16(output, 0)?;
    push_u16(output, 0)?;
    push_u32(output, entry.mode << 16)?;
    push_u32(output, if offset64 { u32::MAX } else { entry.local_offset as u32 })?;
    output.write_all(name)?;
    output.write_all(&extra)?;
    Ok(())
}

fn stored_bytes(entry: &Entry) -> Result<Vec<u8>> {
    match entry.kind {
        Kind::Directory => Ok(Vec::new()),
        Kind::Symlink => symlink_bytes(&entry.source),
        Kind::File => fs::read(&entry.source).map_err(Error::from),
    }
}

struct Prepared {
    entry: Entry,
    bytes: Vec<u8>,
    method: u16,
    crc: u32,
    uncompressed_size: u64,
}

fn prepare(entry: Entry, level: u8) -> Result<Prepared> {
    let plain = stored_bytes(&entry)?;
    let crc = crc32fast::hash(&plain);
    if entry.kind != Kind::File {
        let uncompressed_size = plain.len() as u64;
        return Ok(Prepared { entry, bytes: plain, method: METHOD_STORED, crc, uncompressed_size });
    }
    let (encoded, report) = deflate::compress_bytes_serial(&plain, level)?;
    if encoded.len() < plain.len() {
        Ok(Prepared { entry, bytes: encoded, method: METHOD_DEFLATE, crc: report.crc, uncompressed_size: report.input_len })
    } else {
        let uncompressed_size = plain.len() as u64;
        Ok(Prepared { entry, bytes: plain, method: METHOD_STORED, crc, uncompressed_size })
    }
}

fn write_prepared<W: Write>(output: &mut CountingWriter<W>, prepared: Prepared, central: &mut Vec<CentralEntry>) -> Result<()> {
    let zip64 = may_need_zip64(prepared.uncompressed_size);
    let local_offset = output.position;
    write_local_header(output, &prepared.entry, prepared.method, zip64)?;
    output.write_all(&prepared.bytes)?;
    write_descriptor(output, prepared.crc, prepared.bytes.len() as u64, prepared.uncompressed_size, zip64)?;
    central.push(CentralEntry {
        name: prepared.entry.name,
        method: prepared.method,
        crc: prepared.crc,
        compressed_size: prepared.bytes.len() as u64,
        uncompressed_size: prepared.uncompressed_size,
        local_offset,
        mode: prepared.entry.mode,
        modified: prepared.entry.modified,
        zip64,
    });
    Ok(())
}

fn write_large<W: Write>(output: &mut CountingWriter<W>, entry: Entry, options: EncodeOptions, central: &mut Vec<CentralEntry>) -> Result<()> {
    let zip64 = may_need_zip64(entry.size);
    let local_offset = output.position;
    write_local_header(output, &entry, METHOD_DEFLATE, zip64)?;
    let compressed_start = output.position;
    let mut source = fs::File::open(&entry.source)?;
    let report = deflate::compress_to_writer(&mut source, output, options)?;
    let compressed_size = output.position - compressed_start;
    write_descriptor(output, report.crc, compressed_size, report.input_len, zip64)?;
    central.push(CentralEntry {
        name: entry.name,
        method: METHOD_DEFLATE,
        crc: report.crc,
        compressed_size,
        uncompressed_size: report.input_len,
        local_offset,
        mode: entry.mode,
        modified: entry.modified,
        zip64,
    });
    Ok(())
}

fn finish_archive<W: Write>(output: &mut CountingWriter<W>, central: &[CentralEntry]) -> Result<()> {
    let central_start = output.position;
    for entry in central { write_central(output, entry)?; }
    let central_size = output.position - central_start;
    let zip64 =
        central.iter().any(|entry| entry.zip64) || central.len() > u16::MAX as usize || central_start > u32::MAX as u64 || central_size > u32::MAX as u64;
    if zip64 {
        let zip64_start = output.position;
        push_u32(output, 0x0606_4b50)?;
        push_u64(output, 44)?;
        push_u16(output, (3 << 8) | 45)?;
        push_u16(output, 45)?;
        push_u32(output, 0)?;
        push_u32(output, 0)?;
        push_u64(output, central.len() as u64)?;
        push_u64(output, central.len() as u64)?;
        push_u64(output, central_size)?;
        push_u64(output, central_start)?;
        push_u32(output, 0x0706_4b50)?;
        push_u32(output, 0)?;
        push_u64(output, zip64_start)?;
        push_u32(output, 1)?;
    }
    push_u32(output, 0x0605_4b50)?;
    push_u16(output, 0)?;
    push_u16(output, 0)?;
    push_u16(output, central.len().min(u16::MAX as usize) as u16)?;
    push_u16(output, central.len().min(u16::MAX as usize) as u16)?;
    push_u32(output, central_size.min(u32::MAX as u64) as u32)?;
    push_u32(output, central_start.min(u32::MAX as u64) as u32)?;
    push_u16(output, 0)?;
    output.flush()?;
    Ok(())
}

/// Write exactly the supplied entries; directory contents must be listed explicitly.
pub fn create_to_writer<W: Write + ?Sized>(inputs: &[PathInput], output: &mut W, options: EncodeOptions) -> Result<EncodeReport> {
    let options = options.validate()?;
    let mut entries = inputs.iter().map(entry).collect::<Result<Vec<_>>>()?;
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    for pair in entries.windows(2) { if pair[0].name == pair[1].name { return Err(invalid(format!("duplicate ZIP path {}", pair[0].name))); } }
    let input_len = entries.iter().map(|entry| entry.size).sum();
    let file_count = entries.iter().filter(|entry| entry.kind == Kind::File).count();
    let threshold = if file_count == 1 { SINGLE_ENTRY_PARALLEL } else { MULTI_ENTRY_PARALLEL };
    let small_reservation = |entry: &Entry| match entry.kind {
        Kind::File => (entry.size as usize).saturating_mul(2).saturating_add(SMALL_WORKING_MEMORY),
        Kind::Directory | Kind::Symlink => entry.size as usize + 1024,
    };
    let (large, small): (Vec<_>, Vec<_>) = entries.into_iter().partition(|entry| {
        entry.kind == Kind::File && (entry.size >= threshold || entry.size > usize::MAX as u64 || small_reservation(entry) > options.memory_limit)
    });

    let mut output = CountingWriter { inner: output, position: 0 };
    let mut central = Vec::new();
    for entry in large { write_large(&mut output, entry, options, &mut central)?; }
    if options.resolved_threads() == 1 || small.len() <= 1 {
        for entry in small { write_prepared(&mut output, prepare(entry, options.level_or(6))?, &mut central)?; }
    }
    else {
        let pool = ThreadPoolBuilder::new()
            .num_threads(options.resolved_threads())
            .thread_name(|index| format!("fbz-zip-encode-{index}"))
            .build()
            .map_err(|error| invalid(error.to_string()))?;
        let jobs: Vec<_> = small.into_iter().enumerate().map(|(key, entry)| Job { key, reservation: small_reservation(&entry), payload: entry }).collect();
        run_ordered(
            &pool,
            &jobs,
            PipelineLimits { memory: options.memory_limit, active: options.resolved_threads() },
            |entry| prepare(entry.clone(), options.level_or(6)),
            |result| result.as_ref().map_or(0, |prepared| prepared.bytes.capacity()),
            |results| {
                for key in 0..jobs.len() { write_prepared(&mut output, results.take(key)??, &mut central)?; }
                Ok(())
            },
        )?;
    }
    finish_archive(&mut output, &central)?;
    Ok(EncodeReport { entries: central.len(), input_len, output_len: output.position })
}
