use std::{
    collections::HashMap,
    fs,
    io::{self, Cursor},
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fbz::{DecodeOptions, DecodeProgress, Error, OutputSink, Result, WriterSink, deflate, gzip};
use rayon::prelude::*;
use zip::{CompressionMethod, ZipArchive, extra_fields::ExtraField};

use super::archive_extract;

const MULTI_ENTRY_INTRA_THRESHOLD: u64 = 64 * 1024 * 1024;
const SINGLE_ENTRY_INTRA_THRESHOLD: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind { File, Directory, Symlink }

#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub path: PathBuf,
    pub compression_method: u16,
    pub compressed_size: u64,
    pub decoded_size: u64,
    pub crc: u32,
    method: CompressionMethod,
    data_start: usize,
    data_end: usize,
    mode: Option<u32>,
    modified: Option<SystemTime>,
    kind: EntryKind,
}

#[derive(Clone, Debug)]
pub(super) struct Report { pub source_len: u64, pub decoded_len: u64, pub entries: Vec<Entry> }

fn invalid(message: impl Into<String>) -> Error { Error::InvalidZip(message.into()) }

fn zip_error(error: zip::result::ZipError) -> Error { invalid(error.to_string()) }

fn ntfs_time(ticks: u64) -> Option<SystemTime> {
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let duration = Duration::from_nanos(ticks.abs_diff(UNIX_EPOCH_TICKS).checked_mul(100)?);
    if ticks >= UNIX_EPOCH_TICKS { UNIX_EPOCH.checked_add(duration) } else { UNIX_EPOCH.checked_sub(duration) }
}

fn modified_time(file: &zip::read::ZipFile<'_, impl io::Read>) -> Option<SystemTime> {
    file.extra_data_fields().find_map(|field| match field {
        ExtraField::ExtendedTimestamp(timestamp) => timestamp.mod_time().and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds.into()))),
        ExtraField::Ntfs(timestamp) => ntfs_time(timestamp.mtime()),
    })
}

fn central_entry_count(data: &[u8], start: u64) -> Result<usize> {
    let mut position = usize::try_from(start).map_err(|_| invalid("central directory offset exceeds this platform"))?;
    let mut count = 0;
    while data.get(position..).is_some_and(|remaining| remaining.starts_with(b"PK\x01\x02")) {
        let header = data.get(position..).and_then(|remaining| remaining.get(..46)).ok_or_else(|| invalid("truncated central directory entry"))?;
        let name_len = u16::from_le_bytes(header[28..30].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(header[30..32].try_into().unwrap()) as usize;
        let comment_len = u16::from_le_bytes(header[32..34].try_into().unwrap()) as usize;
        position = position
            .checked_add(46)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(extra_len))
            .and_then(|value| value.checked_add(comment_len))
            .filter(|&value| value <= data.len())
            .ok_or_else(|| invalid("central directory entry exceeds the archive"))?;
        count += 1;
    }
    Ok(count)
}

fn parse(data: &[u8], max_output: Option<usize>) -> Result<Report> {
    let mut archive = ZipArchive::new(Cursor::new(data)).map_err(zip_error)?;
    let central_start = archive.central_directory_start();
    if central_entry_count(data, central_start)? != archive.len() { return Err(invalid("central directory contains duplicate entry names")); }
    let mut entries = Vec::with_capacity(archive.len());
    let mut decoded_len = 0_u64;
    for index in 0..archive.len() {
        let file = archive.by_index_raw(index).map_err(zip_error)?;
        if file.encrypted() { return Err(invalid(format!("encrypted entry {:?} is not supported", file.name()))); }
        let path = file.enclosed_name().ok_or_else(|| invalid(format!("unsafe entry path {:?}", file.name())))?;
        if path.as_os_str().is_empty() { return Err(invalid("empty entry path")); }
        let data_start = usize::try_from(file.data_start().ok_or_else(|| invalid(format!("cannot locate entry {:?}", file.name())))?)
            .map_err(|_| invalid(format!("entry offset for {:?} exceeds this platform", file.name())))?;
        let data_end_u64 =
            (data_start as u64).checked_add(file.compressed_size()).ok_or_else(|| invalid(format!("compressed range for {:?} overflows", file.name())))?;
        if data_end_u64 > central_start || data_end_u64 > data.len() as u64 {
            return Err(invalid(format!("compressed range for {:?} overlaps the central directory or exceeds the archive", file.name())));
        }
        let data_end = data_end_u64 as usize;
        let kind = if file.is_dir() { EntryKind::Directory } else if file.is_symlink() { EntryKind::Symlink } else { EntryKind::File };
        decoded_len = decoded_len.checked_add(file.size()).ok_or_else(|| invalid("total decoded size overflows u64"))?;
        if let Some(limit) = max_output
            && decoded_len > limit as u64
        { return Err(invalid(format!("decoded output exceeds {limit} bytes"))); }
        let method = file.compression();
        #[allow(deprecated)]
        let compression_method = method.to_u16();
        entries.push(Entry {
            path,
            compression_method,
            compressed_size: file.compressed_size(),
            decoded_size: file.size(),
            crc: file.crc32(),
            method,
            data_start,
            data_end,
            mode: file.unix_mode(),
            modified: modified_time(&file),
            kind,
        });
    }
    validate_layout(&entries)?;
    Ok(Report { source_len: data.len() as u64, decoded_len, entries })
}

fn validate_layout(entries: &[Entry]) -> Result<()> {
    let mut paths = HashMap::with_capacity(entries.len());
    for entry in entries {
        if paths.insert(entry.path.clone(), entry.kind).is_some() { return Err(invalid(format!("duplicate entry path {}", entry.path.display()))); }
    }
    for entry in entries {
        for ancestor in entry.path.ancestors().skip(1).filter(|path| !path.as_os_str().is_empty()) {
            if paths.get(ancestor).is_some_and(|kind| *kind != EntryKind::Directory) {
                return Err(invalid(format!("non-directory entry {} is an ancestor of {}", ancestor.display(), entry.path.display())));
            }
        }
    }
    let mut ranges: Vec<_> = entries.iter().filter(|entry| entry.compressed_size != 0).map(|entry| (entry.data_start, entry.data_end, &entry.path)).collect();
    ranges.sort_unstable_by_key(|range| range.0);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 { return Err(invalid(format!("compressed data for {} overlaps {}", pair[0].2.display(), pair[1].2.display()))); }
    }
    Ok(())
}

struct ExpectedOutput<W> { inner: W, expected: u64, written: u64 }

impl<W> ExpectedOutput<W> {
    fn new(inner: W, expected: u64) -> Self { Self { inner, expected, written: 0 } }

    fn check(&self, count: usize) -> io::Result<()> {
        if self.written.saturating_add(count as u64) > self.expected {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("ZIP entry exceeds its declared {}-byte size", self.expected)));
        }
        Ok(())
    }
}

impl<W: OutputSink> OutputSink for ExpectedOutput<W> {
    fn write_borrowed(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.check(bytes.len())?;
        self.inner.write_borrowed(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn write_owned_from(&mut self, bytes: Vec<u8>, start: usize) -> io::Result<()> {
        let count = bytes.get(start..).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "owned chunk start exceeds its length"))?.len();
        self.check(count)?;
        self.inner.write_owned_from(bytes, start)?;
        self.written += count as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> { self.inner.flush() }

    fn is_cancelled(&self) -> bool { self.inner.is_cancelled() }
}

fn entry_error(entry: &Entry, error: Error) -> Error {
    let detail = match error { Error::InvalidGzip(message) | Error::InvalidZip(message) => message, other => other.to_string() };
    invalid(format!("entry {}: {detail}", entry.path.display()))
}

fn decode_entry(data: &[u8], entry: &Entry, output: &mut impl OutputSink, options: DecodeOptions, progress: &mut dyn FnMut(DecodeProgress)) -> Result<()> {
    let compressed = &data[entry.data_start..entry.data_end];
    let mut output = ExpectedOutput::new(output, entry.decoded_size);
    let (decoded_size, crc) = if entry.method == CompressionMethod::STORE {
        if entry.compressed_size != entry.decoded_size {
            return Err(invalid(format!("stored entry {} has different compressed and decoded sizes", entry.path.display())));
        }
        output.write_borrowed(compressed)?;
        progress(DecodeProgress { compressed_bytes: entry.compressed_size, decoded_bytes: entry.decoded_size });
        (compressed.len() as u64, gzip::crc32(compressed))
    } else if entry.method == CompressionMethod::DEFLATE {
        let report =
            deflate::decompress_to_sink_with_options_and_progress(compressed, &mut output, options, progress).map_err(|error| entry_error(entry, error))?;
        (report.decoded_len, report.crc)
    } else { return Err(invalid(format!("entry {} uses unsupported compression method {}", entry.path.display(), entry.compression_method))); };
    output.flush()?;
    if decoded_size != entry.decoded_size {
        return Err(invalid(format!("entry {} size mismatch: expected {}, decoded {decoded_size}", entry.path.display(), entry.decoded_size)));
    }
    if crc != entry.crc { return Err(invalid(format!("entry {} CRC32 mismatch: expected {:08x}, decoded {crc:08x}", entry.path.display(), entry.crc))); }
    Ok(())
}

fn uses_intra_entry(entry: &Entry, entry_count: usize) -> bool {
    if entry.method != CompressionMethod::DEFLATE { return false; }
    let threshold = if entry_count == 1 { SINGLE_ENTRY_INTRA_THRESHOLD } else { MULTI_ENTRY_INTRA_THRESHOLD };
    entry.compressed_size >= threshold
}

fn run_entries<P>(
    entries: &[Entry],
    source_len: u64,
    options: DecodeOptions,
    run: impl Fn(&Entry, DecodeOptions, &mut dyn FnMut(DecodeProgress)) -> Result<()> + Sync,
    progress: P,
) -> Result<()>
where
    P: FnMut(DecodeProgress) + Send,
{
    let threads = options.resolved_threads();
    let compressed = AtomicU64::new(0);
    let decoded = AtomicU64::new(0);
    let completed = AtomicUsize::new(0);
    let progress = Mutex::new(progress);
    let run = |entry: &Entry, entry_options| {
        let mut entry_compressed = 0;
        let mut entry_decoded = 0;
        let mut update = |entry_progress: DecodeProgress| {
            let compressed_delta = entry_progress.compressed_bytes.saturating_sub(entry_compressed);
            let decoded_delta = entry_progress.decoded_bytes.saturating_sub(entry_decoded);
            entry_compressed = entry_progress.compressed_bytes;
            entry_decoded = entry_progress.decoded_bytes;
            let compressed_bytes = compressed.fetch_add(compressed_delta, Ordering::Relaxed) + compressed_delta;
            let decoded_bytes = decoded.fetch_add(decoded_delta, Ordering::Relaxed) + decoded_delta;
            progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner)(DecodeProgress { compressed_bytes, decoded_bytes });
        };
        run(entry, entry_options, &mut update)?;
        let compressed_delta = entry.compressed_size.saturating_sub(entry_compressed);
        let decoded_delta = entry.decoded_size.saturating_sub(entry_decoded);
        let compressed = compressed.fetch_add(compressed_delta, Ordering::Relaxed) + compressed_delta;
        let decoded = decoded.fetch_add(decoded_delta, Ordering::Relaxed) + decoded_delta;
        let completed = completed.fetch_add(1, Ordering::Relaxed) + 1;
        let compressed_bytes = if completed == entries.len() { source_len } else { compressed };
        progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner)(DecodeProgress { compressed_bytes, decoded_bytes: decoded });
        Ok(())
    };
    let (within, across): (Vec<_>, Vec<_>) = entries.iter().partition(|entry| uses_intra_entry(entry, entries.len()));
    for entry in within { run(entry, DecodeOptions { threads, ..options })?; }
    if across.is_empty() { return Ok(()); }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("fbz-zip-{index}"))
        .build()
        .map_err(|error| invalid(error.to_string()))?;
    pool.install(|| across.par_iter().try_for_each(|entry| run(entry, DecodeOptions { threads: 1, ..options })))
}

pub(super) fn validate(data: &[u8], options: DecodeOptions, max_output: Option<usize>, progress: impl FnMut(DecodeProgress) + Send) -> Result<Report> {
    let report = parse(data, max_output)?;
    run_entries(
        &report.entries,
        report.source_len,
        options,
        |entry, entry_options, progress| { let mut output = WriterSink::new(io::sink()); decode_entry(data, entry, &mut output, entry_options, progress) },
        progress,
    )?;
    Ok(report)
}

fn prepare_directories(root: &Path, entries: &[Entry]) -> Result<()> {
    for entry in entries {
        let path = root.join(&entry.path);
        if entry.kind == EntryKind::Directory { fs::create_dir_all(&path)?; } else if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(root: &Path, entry: &Entry, target: &[u8]) -> Result<()> {
    use std::{
        ffi::OsStr,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };

    let target = Path::new(OsStr::from_bytes(target));
    if target.is_absolute() { return Err(invalid(format!("entry {} has absolute symlink target {}", entry.path.display(), target.display()))); }
    let mut depth = entry.path.parent().map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth != 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(format!("entry {} has escaping symlink target {}", entry.path.display(), target.display())));
            }
        }
    }
    symlink(target, root.join(&entry.path))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_root: &Path, entry: &Entry, _target: &[u8]) -> Result<()> {
    Err(invalid(format!("symlink entry {} is not supported on this platform", entry.path.display())))
}

fn extract_entry(data: &[u8], root: &Path, entry: &Entry, options: DecodeOptions, progress: &mut dyn FnMut(DecodeProgress)) -> Result<()> {
    if entry.kind == EntryKind::Directory { return Ok(()); }
    let path = root.join(&entry.path);
    if entry.kind == EntryKind::Symlink {
        if entry.decoded_size > 64 * 1024 { return Err(invalid(format!("symlink target in {} is too large", entry.path.display()))); }
        let mut target = Vec::with_capacity(entry.decoded_size as usize);
        let mut output = WriterSink::new(&mut target);
        decode_entry(data, entry, &mut output, options, progress)?;
        return create_symlink(root, entry, &target);
    }
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    let mut output = WriterSink::new(&mut file);
    decode_entry(data, entry, &mut output, options, progress)?;
    set_mode(&path, entry.mode)?;
    set_modified(&path, entry.modified)?;
    Ok(())
}

fn set_modified(path: &Path, modified: Option<SystemTime>) -> Result<()> {
    if let Some(modified) = modified { fs::File::open(path)?.set_times(fs::FileTimes::new().set_modified(modified))?; }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode { fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))?; }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) -> Result<()> { Ok(()) }

pub(super) fn unpack(
    data: &[u8],
    destination: &Path,
    overwrite: bool,
    options: DecodeOptions,
    max_output: Option<usize>,
    progress: impl FnMut(DecodeProgress) + Send,
) -> Result<Report> {
    let report = parse(data, max_output)?;
    let staging = archive_extract::staging(destination)?;
    prepare_directories(staging.path(), &report.entries)?;
    run_entries(
        &report.entries,
        report.source_len,
        options,
        |entry, entry_options, progress| extract_entry(data, staging.path(), entry, entry_options, progress),
        progress,
    )?;
    let mut directories: Vec<_> = report.entries.iter().filter(|entry| entry.kind == EntryKind::Directory).collect();
    directories.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.path.components().count()));
    for entry in directories {
        set_mode(&staging.path().join(&entry.path), entry.mode)?;
        set_modified(&staging.path().join(&entry.path), entry.modified)?;
    }
    archive_extract::commit(staging.path(), destination, overwrite)?;
    Ok(report)
}
