//! Gzip framing and DEFLATE compression/decompression implemented in safe Rust.

use std::{io::Write, sync::OnceLock};

use crate::{
    DecodeOptions, DecodeProgress, EncodeOptions, Error, OutputSink, Result, WriterSink,
    deflate::{DISTANCE_BASE, DISTANCE_EXTRA, LENGTH_BASE, LENGTH_EXTRA},
    deflate_encode,
    history::extend_match,
    pipeline::{Job, PipelineLimits, run_staged_ordered},
};

const WINDOW_SIZE: usize = 32 * 1024;
const OUTPUT_CHUNK: usize = 64 * 1024;
const HISTORY_COMPACT: usize = 1024 * 1024;
const MAX_CODE_BITS: usize = 15;
const PARALLEL_GRID: usize = 512 * 1024;
const MIN_PARALLEL_INPUT: usize = 16 * 1024 * 1024;
const PARALLEL_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const PARALLEL_JOB_MEMORY: usize = 2 * PARALLEL_OUTPUT_LIMIT + 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind { Stored, FixedHuffman, DynamicHuffman }

impl BlockKind {
    pub fn as_str(self) -> &'static str { match self { Self::Stored => "stored", Self::FixedHuffman => "fixed", Self::DynamicHuffman => "dynamic" } }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub compressed_start: u64,
    pub deflate_start: u64,
    pub compressed_end: u64,
    pub decoded_start: u64,
    pub decoded_len: u64,
    pub expected_crc: u32,
    pub mtime: u32,
    pub extra_flags: u8,
    pub operating_system: u8,
    pub name: Option<Vec<u8>>,
    pub comment: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub member: u32,
    pub kind: BlockKind,
    pub final_block: bool,
    pub compressed_start_bit: u64,
    pub compressed_end_bit: u64,
    pub decoded_start: u64,
    pub decoded_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub source_len: u64,
    pub decoded_len: u64,
    pub members: Vec<Member>,
    pub blocks: Vec<Block>,
    pub speculative_chunks: u64,
    pub fallback_chunks: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeflateReport {
    pub source_len: u64,
    pub compressed_end_bit: u64,
    pub decoded_len: u64,
    pub crc: u32,
    pub blocks: Vec<Block>,
    pub speculative_chunks: u64,
    pub fallback_chunks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeReport { pub input_len: u64, pub output_len: u64, pub crc: u32 }

pub struct Encoder<W: Write> { inner: deflate_encode::Encoder<W> }

impl<W: Write> Encoder<W> {
    pub fn new(mut output: W, options: EncodeOptions) -> Result<Self> {
        let options = deflate_encode::validate_options(options)?;
        let extra_flags = match options.level_or(6) { 1..=2 => 4, 8..=9 => 2, _ => 0 };
        output.write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, extra_flags, 255])?;
        Ok(Self { inner: deflate_encode::Encoder::new(output, options)? })
    }

    pub fn finish(self) -> Result<(W, EncodeReport)> {
        let (mut output, deflate) = self.inner.finish()?;
        output.write_all(&deflate.crc.to_le_bytes())?;
        output.write_all(&(deflate.input_len as u32).to_le_bytes())?;
        output.flush()?;
        Ok((output, EncodeReport { input_len: deflate.input_len, output_len: deflate.output_len + 18, crc: deflate.crc }))
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.inner.write(bytes) }

    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}

pub fn compress(data: &[u8]) -> Result<Vec<u8>> { compress_with_options(data, EncodeOptions::default()) }

pub fn compress_with_options(data: &[u8], options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    compress_to_writer(&mut std::io::Cursor::new(data), &mut output, options)?;
    Ok(output)
}

pub fn compress_to_writer(input: &mut impl std::io::Read, output: &mut impl Write, options: EncodeOptions) -> Result<EncodeReport> {
    let mut encoder = Encoder::new(output, options)?;
    std::io::copy(input, &mut encoder)?;
    let (_, report) = encoder.finish()?;
    Ok(report)
}

#[derive(Clone, Debug)]
struct Header {
    deflate_start: usize,
    mtime: u32,
    extra_flags: u8,
    operating_system: u8,
    name: Option<Vec<u8>>,
    comment: Option<Vec<u8>>,
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>> { decompress_with_options(data, DecodeOptions::default()) }

pub fn decompress_with_options(data: &[u8], options: DecodeOptions) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    decompress_to_writer_with_options(data, &mut output, options)?;
    Ok(output)
}

pub fn decompress_to_writer(data: &[u8], output: &mut impl Write) -> Result<Report> {
    decompress_to_writer_with_options(data, output, DecodeOptions::default())
}

pub fn decompress_to_writer_with_options(data: &[u8], output: &mut impl Write, options: DecodeOptions) -> Result<Report> {
    decompress_to_writer_with_options_and_progress(data, output, options, |_| {})
}

pub fn decompress_to_writer_with_progress(data: &[u8], output: &mut impl Write, progress: impl FnMut(DecodeProgress)) -> Result<Report> {
    decompress_to_writer_with_options_and_progress(data, output, DecodeOptions::default(), progress)
}

pub fn decompress_to_writer_with_options_and_progress(
    data: &[u8],
    output: &mut impl Write,
    options: DecodeOptions,
    progress: impl FnMut(DecodeProgress),
) -> Result<Report> { let mut output = WriterSink::new(output); decompress_to_sink_with_options_and_progress(data, &mut output, options, progress) }

/// Decode into an output that can take ownership of completed chunks.
#[doc(hidden)]
pub fn decompress_to_sink_with_options_and_progress(
    data: &[u8],
    output: &mut impl OutputSink,
    options: DecodeOptions,
    mut progress: impl FnMut(DecodeProgress),
) -> Result<Report> {
    let options = options.validate()?;
    let mut members = Vec::new();
    let mut blocks = Vec::new();
    let mut position = 0_usize;
    let mut decoded_total = 0_u64;
    let mut speculative_total = 0_u64;
    let mut fallback_total = 0_u64;

    while position < data.len() {
        if !members.is_empty() && data[position..].iter().all(|&byte| byte == 0) { break; }
        let member_start = position;
        let header = parse_header(data, position)?;
        let member_number = u32::try_from(members.len()).map_err(|_| invalid("too many gzip members"))?;
        let stream = DeflateStream { start_byte: header.deflate_start, end_byte: data.len(), member: member_number, decoded_base: decoded_total };
        let decoded = decompress_deflate_stream(data, stream, output, options, &mut progress)?;
        let trailer = (decoded.compressed_end_bit as usize).div_ceil(8);
        let trailer_end = trailer.checked_add(8).ok_or_else(|| invalid("trailer offset overflow"))?;
        let trailer_bytes = data.get(trailer..trailer_end).ok_or_else(|| invalid_at(trailer, "truncated member trailer"))?;
        let expected_crc = u32::from_le_bytes(trailer_bytes[..4].try_into().unwrap());
        let expected_size = u32::from_le_bytes(trailer_bytes[4..].try_into().unwrap());
        if decoded.crc != expected_crc { return Err(invalid_at(trailer, format!("CRC32 mismatch: expected {expected_crc:08x}, decoded {:08x}", decoded.crc))); }
        if decoded.decoded_len as u32 != expected_size {
            return Err(invalid_at(trailer + 4, format!("ISIZE mismatch: expected {expected_size}, decoded {}", decoded.decoded_len as u32)));
        }
        decoded_total = decoded_total.checked_add(decoded.decoded_len).ok_or_else(|| invalid("decoded offset overflow"))?;
        speculative_total += decoded.speculative_chunks;
        fallback_total += decoded.fallback_chunks;
        blocks.extend(decoded.blocks);
        position = trailer_end;
        members.push(Member {
            compressed_start: member_start as u64,
            deflate_start: header.deflate_start as u64,
            compressed_end: position as u64,
            decoded_start: decoded_total - decoded.decoded_len,
            decoded_len: decoded.decoded_len,
            expected_crc,
            mtime: header.mtime,
            extra_flags: header.extra_flags,
            operating_system: header.operating_system,
            name: header.name,
            comment: header.comment,
        });
        progress(DecodeProgress { compressed_bytes: position as u64, decoded_bytes: decoded_total });
    }
    if members.is_empty() { return Err(invalid("input contains no gzip members")); }
    output.flush()?;
    progress(DecodeProgress { compressed_bytes: data.len() as u64, decoded_bytes: decoded_total });
    Ok(Report {
        source_len: data.len() as u64,
        decoded_len: decoded_total,
        members,
        blocks,
        speculative_chunks: speculative_total,
        fallback_chunks: fallback_total,
    })
}

/// Decode one raw DEFLATE stream into an output that can take ownership of completed chunks.
#[doc(hidden)]
pub(crate) fn decompress_deflate_to_sink_with_options_and_progress(
    data: &[u8],
    output: &mut impl OutputSink,
    options: DecodeOptions,
    mut progress: impl FnMut(DecodeProgress),
) -> Result<DeflateReport> {
    let options = options.validate()?;
    let stream = DeflateStream { start_byte: 0, end_byte: data.len(), member: 0, decoded_base: 0 };
    let report = decompress_deflate_stream(data, stream, output, options, &mut progress)?;
    if (report.compressed_end_bit as usize).div_ceil(8) != data.len() {
        return Err(invalid_bit(report.compressed_end_bit as usize, "trailing data after final DEFLATE block"));
    }
    output.flush()?;
    progress(DecodeProgress { compressed_bytes: data.len() as u64, decoded_bytes: report.decoded_len });
    Ok(report)
}

#[derive(Clone, Copy)]
enum InitialHistory { Empty, Unknown }

struct MarkerOutput {
    marked: Vec<u16>,
    clean: Vec<u8>,
    clean_start: usize,
    history: InitialHistory,
    limit: usize,
    clean_mode: bool,
}

impl MarkerOutput {
    fn new(history: InitialHistory, limit: usize) -> Self {
        Self { marked: Vec::new(), clean: Vec::new(), clean_start: 0, history, limit, clean_mode: matches!(history, InitialHistory::Empty) }
    }

    fn len(&self) -> usize { self.marked.len() + self.clean.len().saturating_sub(self.clean_start) }

    fn ensure_capacity(&self, additional: usize) -> Result<()> {
        if additional > self.limit.saturating_sub(self.len()) { return Err(invalid("parallel DEFLATE chunk exceeded its memory budget")); }
        Ok(())
    }

    fn try_clean(&mut self) {
        if self.clean_mode || self.marked.len() < WINDOW_SIZE { return; }
        let suffix = &self.marked[self.marked.len() - WINDOW_SIZE..];
        if suffix.iter().any(|&symbol| symbol > u8::MAX as u16) { return; }
        self.clean = Vec::with_capacity(WINDOW_SIZE + 4 * PARALLEL_GRID);
        self.clean.extend(suffix.iter().map(|&symbol| symbol as u8));
        self.clean_start = WINDOW_SIZE;
        self.clean_mode = true;
    }
}

impl DeflateOutput for MarkerOutput {
    fn total_decoded(&self) -> u64 { self.len() as u64 }

    fn emit(&mut self, byte: u8) -> Result<()> {
        self.ensure_capacity(1)?;
        if self.clean_mode { self.clean.push(byte); }
        else { self.marked.push(u16::from(byte)); }
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<()> {
        self.ensure_capacity(bytes.len())?;
        if self.clean_mode { self.clean.extend_from_slice(bytes); }
        else { self.marked.extend(bytes.iter().map(|&byte| u16::from(byte))); }
        Ok(())
    }

    fn copy(&mut self, distance: usize, length: usize) -> Result<()> {
        self.ensure_capacity(length)?;
        if self.clean_mode {
            let available = self.clean.len().min(WINDOW_SIZE);
            if distance == 0 || distance > available { return Err(invalid(format!("back-reference distance {distance} exceeds {available} available bytes"))); }
            extend_match(&mut self.clean, distance, length);
            return Ok(());
        }

        let available = match self.history { InitialHistory::Empty => self.marked.len().min(WINDOW_SIZE), InitialHistory::Unknown => WINDOW_SIZE };
        if distance == 0 || distance > available { return Err(invalid(format!("back-reference distance {distance} exceeds {available} available bytes"))); }
        self.marked.reserve(length);
        let append_start = self.marked.len();
        let mut copied = 0;
        if distance > append_start {
            let from_window = (distance - append_start).min(length);
            let first = WINDOW_SIZE + append_start - distance;
            self.marked.extend((0..from_window).map(|offset| (WINDOW_SIZE + first + offset) as u16));
            copied = from_window;
        }
        if copied < length { extend_match(&mut self.marked, distance, length - copied); }
        Ok(())
    }
}

struct Segment {
    start_bit: usize,
    end_bit: usize,
    marked: Vec<u16>,
    clean: Vec<u8>,
    clean_start: usize,
    clean_crc: crc32fast::Hasher,
    blocks: Vec<Block>,
    final_block: bool,
}

impl Segment {
    fn clean_output(&self) -> &[u8] { &self.clean[self.clean_start..] }

    fn retained_bytes(&self) -> usize { self.marked.capacity() * size_of::<u16>() + self.clean.capacity() + self.blocks.capacity() * size_of::<Block>() }
}

fn decode_segment(data: &[u8], start_bit: usize, stop_bit: usize, history: InitialHistory, output_limit: usize) -> Result<Segment> {
    let mut bits = Bits::at(data, start_bit)?;
    let mut emitter = MarkerOutput::new(history, output_limit);
    let mut blocks = Vec::new();
    let final_block = loop {
        let final_block = decode_block(&mut bits, &mut emitter, 0, &mut blocks, &mut |_| {})?;
        emitter.try_clean();
        if final_block {
            bits.align_byte();
            break true;
        }
        if bits.position_bits() >= stop_bit && blocks.last().is_some_and(|block| block.kind == BlockKind::DynamicHuffman) { break false; }
    };
    let mut clean_crc = crc32fast::Hasher::new();
    clean_crc.update(&emitter.clean[emitter.clean_start..]);
    Ok(Segment {
        start_bit,
        end_bit: bits.position_bits(),
        marked: emitter.marked,
        clean: emitter.clean,
        clean_start: emitter.clean_start,
        clean_crc,
        blocks,
        final_block,
    })
}

#[derive(Clone, Copy)]
struct GzipJob {
    search_start: usize,
    search_end: usize,
    stop_bit: usize,
    output_limit: usize,
}

fn run_gzip_job(data: &[u8], job: &GzipJob) -> Result<Segment> {
    let mut search = job.search_start;
    loop {
        let start = find_dynamic_boundary(data, search, job.search_end)
            .ok_or_else(|| invalid_bit(job.search_start, "no decodable dynamic DEFLATE boundary found near the parallel grid point"))?;
        match decode_segment(data, start, job.stop_bit, InitialHistory::Unknown, job.output_limit) {
            Ok(segment) => return Ok(segment),
            Err(_) => search = start.saturating_add(1),
        }
    }
}

fn find_dynamic_boundary(data: &[u8], start_bit: usize, end_bit: usize) -> Option<usize> {
    let end = end_bit.min(data.len().saturating_mul(8));
    let first_byte = start_bit / 8;
    let last_byte = end.div_ceil(8).min(data.len());
    for byte_offset in first_byte..last_byte {
        let low = u16::from(data[byte_offset]);
        let high = data.get(byte_offset + 1).map_or(0, |byte| u16::from(*byte));
        let header_window = low | (high << 8);
        for bit_in_byte in 0..8 {
            let bit = byte_offset * 8 + bit_in_byte;
            if bit < start_bit || bit.saturating_add(13) >= end || ((header_window >> bit_in_byte) & 0b111) != 0b100 { continue; }
            let mut header = Bits::at(data, bit + 3).ok()?;
            if header.read(5).ok()? > 29 || header.read(5).ok()? > 29 || !valid_precode_shape(data, bit) { continue; }
            let mut validation = Bits::at(data, bit + 3).ok()?;
            if dynamic_tables(&mut validation).is_ok() { return Some(bit); }
        }
    }
    None
}

fn valid_precode_shape(data: &[u8], block_bit: usize) -> bool {
    const PRECODE_BITS: usize = 4 + 19 * 3;
    let Some(precode_bit) = block_bit.checked_add(13) else { return false };
    if precode_bit.checked_add(PRECODE_BITS).is_none_or(|end| end > data.len().saturating_mul(8)) { return false; }
    let byte = precode_bit / 8;
    let shift = precode_bit & 7;
    let low = word_at(data, byte);
    let bits = if shift == 0 { low } else { (low >> shift) | (u64::from(data.get(byte + 8).copied().unwrap_or(0)) << (64 - shift)) };
    let count = 4 + (bits & 0b1111) as usize;
    let lengths = bits >> 4;
    let mut counts = [0_u8; 8];
    let mut used = 0_u8;
    for index in 0..count {
        let length = ((lengths >> (index * 3)) & 0b111) as usize;
        if length != 0 {
            counts[length] += 1;
            used += 1;
        }
    }
    if used == 0 { return false; }
    let mut remaining = 1_i16;
    for count in counts.iter().skip(1) { remaining = remaining * 2 - i16::from(*count); if remaining < 0 { return false; } }
    remaining == 0 || used == 1
}

#[inline(always)]
fn word_at(data: &[u8], byte: usize) -> u64 {
    if data.len().saturating_sub(byte) >= 8 { u64::from_le_bytes(data[byte..byte + 8].try_into().unwrap()) } else { data[byte..].iter().take(8).enumerate().fold(0_u64, |word, (index, &value)| word | u64::from(value) << (index * 8)) }
}

fn resolve_symbols(symbols: &[u16], predecessor: &[u8]) -> Result<Vec<u8>> {
    let mut resolved = Vec::with_capacity(symbols.len());
    if predecessor.len() == WINDOW_SIZE && symbols.len() >= 128 * 1024 {
        let mut lookup = [0_u8; u16::MAX as usize + 1];
        for (value, byte) in lookup[..=u8::MAX as usize].iter_mut().enumerate() { *byte = value as u8; }
        lookup[WINDOW_SIZE..].copy_from_slice(predecessor);
        for (target, &symbol) in resolved.spare_capacity_mut().iter_mut().zip(symbols) { target.write(lookup[symbol as usize]); }
        // SAFETY: the loop initialized one distinct spare-capacity byte per input symbol.
        unsafe { resolved.set_len(symbols.len()) };
    }
    else {
        let missing = WINDOW_SIZE.saturating_sub(predecessor.len());
        for &symbol in symbols {
            let byte = if symbol <= u8::MAX as u16 { symbol as u8 } else {
                let index = symbol as usize - WINDOW_SIZE;
                if index < missing { return Err(invalid("speculative marker references unavailable predecessor history")); }
                predecessor[index - missing]
            };
            resolved.push(byte);
        }
    }
    Ok(resolved)
}

fn successor_window(segment: &Segment, predecessor: &[u8]) -> Result<Vec<u8>> {
    let clean = segment.clean_output();
    if clean.len() >= WINDOW_SIZE { return Ok(clean[clean.len() - WINDOW_SIZE..].to_vec()); }
    let marked_count = WINDOW_SIZE.saturating_sub(clean.len()).min(segment.marked.len());
    let marked = resolve_symbols(&segment.marked[segment.marked.len() - marked_count..], predecessor)?;
    let keep_predecessor = WINDOW_SIZE.saturating_sub(marked.len() + clean.len()).min(predecessor.len());
    let mut window = Vec::with_capacity(keep_predecessor + marked.len() + clean.len());
    window.extend_from_slice(&predecessor[predecessor.len() - keep_predecessor..]);
    window.extend_from_slice(&marked);
    window.extend_from_slice(clean);
    Ok(window)
}

struct ResolveTask { segment: Segment, predecessor: Vec<u8> }

struct ResolvedSegment {
    marked: Vec<u8>,
    clean: Vec<u8>,
    clean_start: usize,
    blocks: Vec<Block>,
    compressed_end_bit: usize,
    crc: crc32fast::Hasher,
}

fn resolve_segment(segment: Segment, predecessor: &[u8]) -> Result<ResolvedSegment> {
    let marked = resolve_symbols(&segment.marked, predecessor)?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(&marked);
    crc.combine(&segment.clean_crc);
    Ok(ResolvedSegment { marked, clean: segment.clean, clean_start: segment.clean_start, blocks: segment.blocks, compressed_end_bit: segment.end_bit, crc })
}

struct SegmentCommitter<'a, W: ?Sized, P: ?Sized> {
    member: u32,
    decoded_base: u64,
    decoded: u64,
    output: &'a mut W,
    crc: crc32fast::Hasher,
    blocks: &'a mut Vec<Block>,
    progress: &'a mut P,
}

impl<'a, W: OutputSink + ?Sized, P: FnMut(DecodeProgress) + ?Sized> SegmentCommitter<'a, W, P> {
    fn new(member: u32, decoded_base: u64, output: &'a mut W, blocks: &'a mut Vec<Block>, progress: &'a mut P) -> Self {
        Self { member, decoded_base, decoded: 0, output, crc: crc32fast::Hasher::new(), blocks, progress }
    }

    fn commit(&mut self, segment: ResolvedSegment) -> Result<()> {
        let ResolvedSegment { marked, clean, clean_start, mut blocks, compressed_end_bit, crc } = segment;
        let decoded_len = marked.len() + clean.len() - clean_start;
        self.output.write_owned_from(marked, 0)?;
        self.output.write_owned_from(clean, clean_start)?;
        self.crc.combine(&crc);
        for block in &mut blocks {
            block.member = self.member;
            block.decoded_start += self.decoded_base + self.decoded;
        }
        self.decoded = self.decoded.checked_add(decoded_len as u64).ok_or_else(|| invalid("decoded offset overflow"))?;
        self.blocks.append(&mut blocks);
        (self.progress)(DecodeProgress { compressed_bytes: compressed_end_bit.div_ceil(8) as u64, decoded_bytes: self.decoded_base + self.decoded });
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct DeflateStream {
    start_byte: usize,
    end_byte: usize,
    member: u32,
    decoded_base: u64,
}

fn decompress_deflate_stream(
    data: &[u8],
    stream: DeflateStream,
    output: &mut impl OutputSink,
    mut options: DecodeOptions,
    progress: &mut impl FnMut(DecodeProgress),
) -> Result<DeflateReport> {
    let DeflateStream { start_byte, end_byte, member, decoded_base } = stream;
    let data = data.get(..end_byte).ok_or_else(|| invalid("DEFLATE end exceeds input"))?;
    if start_byte > end_byte { return Err(invalid("DEFLATE start exceeds end")); }
    let threads = options.resolved_threads();
    options.threads = threads;
    if threads == 1 || end_byte - start_byte < MIN_PARALLEL_INPUT || options.memory_limit < PARALLEL_JOB_MEMORY {
        return decompress_deflate_serial_stream(data, start_byte, output, member, decoded_base, progress);
    }
    let first_grid = start_byte.saturating_add(PARALLEL_GRID);
    let initial_segment = match decode_segment(data, start_byte * 8, first_grid.min(end_byte) * 8, InitialHistory::Empty, PARALLEL_OUTPUT_LIMIT) {
        Ok(segment) => segment,
        Err(_) => return decompress_deflate_serial_stream(data, start_byte, output, member, decoded_base, progress),
    };
    decompress_deflate_parallel_stream(data, start_byte, end_byte, output, options, threads, member, decoded_base, progress, initial_segment)
}

fn decompress_deflate_serial_stream(
    data: &[u8],
    start_byte: usize,
    output: &mut impl OutputSink,
    member: u32,
    decoded_base: u64,
    progress: &mut impl FnMut(DecodeProgress),
) -> Result<DeflateReport> {
    let mut blocks = Vec::new();
    let mut emitter = Emitter::new(output, decoded_base);
    let mut bits = Bits::new(data, start_byte);
    decode_deflate(&mut bits, &mut emitter, member, &mut blocks, progress)?;
    let compressed_end_bit = bits.position_bits() as u64;
    let (crc, decoded_len) = emitter.finish()?;
    Ok(DeflateReport { source_len: (data.len() - start_byte) as u64, compressed_end_bit, decoded_len, crc, blocks, speculative_chunks: 0, fallback_chunks: 0 })
}

#[allow(clippy::too_many_arguments)]
fn decompress_deflate_parallel_stream(
    data: &[u8],
    start_byte: usize,
    end_byte: usize,
    output: &mut impl OutputSink,
    options: DecodeOptions,
    threads: usize,
    member: u32,
    decoded_base: u64,
    progress: &mut impl FnMut(DecodeProgress),
    initial_segment: Segment,
) -> Result<DeflateReport> {
    let per_job = PARALLEL_JOB_MEMORY;
    let output_limit = PARALLEL_OUTPUT_LIMIT;
    let horizon = threads.saturating_add(2);
    let parallel_budget = options.memory_limit.min(per_job.saturating_mul(horizon));
    let first_grid = start_byte.saturating_add(PARALLEL_GRID);
    let mut jobs = Vec::new();
    let mut key = 1;
    let mut grid = first_grid;
    while grid < end_byte {
        jobs.push(Job {
            key,
            reservation: per_job,
            payload: GzipJob {
                search_start: grid * 8,
                search_end: grid.saturating_add(2 * PARALLEL_GRID).min(end_byte) * 8,
                stop_bit: grid.saturating_add(PARALLEL_GRID).min(end_byte) * 8,
                output_limit,
            },
        });
        key += 1;
        grid = grid.saturating_add(PARALLEL_GRID);
    }

    let mut blocks = Vec::new();
    let (compressed_end_bit, decoded_len, crc, speculative_chunks, fallback_chunks) = run_staged_ordered(
        threads,
        &jobs,
        PipelineLimits { memory: parallel_budget, active: horizon.saturating_mul(2) },
        |job| run_gzip_job(data, job),
        |result| result.as_ref().map_or(0, Segment::retained_bytes),
        |task: ResolveTask| resolve_segment(task.segment, &task.predecessor),
        |results| {
            let mut predecessor = Vec::new();
            let mut committer = SegmentCommitter::new(member, decoded_base, output, &mut blocks, progress);
            let mut key = 1;
            let mut resolve_sequence = 0;
            let mut next_resolve = 0;
            let mut outstanding = 0;
            let mut speculative_chunks = 0_u64;
            let mut fallback_chunks = 0_u64;

            let mut segment = initial_segment;
            let mut next_start = segment.end_bit;
            let mut final_block = segment.final_block;
            let next_window = successor_window(&segment, &predecessor)?;
            let resolved = resolve_segment(segment, &predecessor)?;
            committer.commit(resolved)?;
            predecessor = next_window;

            while !final_block {
                let estimated_stop = start_byte.saturating_add((key + 1) * PARALLEL_GRID).min(end_byte) * 8;
                let (lease, speculative) = results.take_primary(key)?;
                let accepted = matches!(&speculative, Ok(candidate) if candidate.start_bit == next_start);
                if !accepted {
                    results.retire(lease);
                    fallback_chunks += 1;
                    while outstanding != 0 {
                        let resolved = results.take_stage(next_resolve)??;
                        committer.commit(resolved)?;
                        next_resolve += 1;
                        outstanding -= 1;
                    }
                    segment = decode_segment(data, next_start, estimated_stop, InitialHistory::Unknown, output_limit)?;
                    next_start = segment.end_bit;
                    final_block = segment.final_block;
                    let next_window = successor_window(&segment, &predecessor)?;
                    let resolved = resolve_segment(segment, &predecessor)?;
                    committer.commit(resolved)?;
                    predecessor = next_window;
                    key += 1;
                    continue;
                }

                speculative_chunks += 1;
                let segment = speculative.unwrap();
                next_start = segment.end_bit;
                final_block = segment.final_block;
                let next_window = successor_window(&segment, &predecessor)?;
                results.submit(resolve_sequence, lease, ResolveTask { segment, predecessor })?;
                predecessor = next_window;
                resolve_sequence += 1;
                outstanding += 1;
                key += 1;

                if outstanding >= threads {
                    let resolved = results.take_stage(next_resolve)??;
                    committer.commit(resolved)?;
                    next_resolve += 1;
                    outstanding -= 1;
                }
            }

            while outstanding != 0 {
                let resolved = results.take_stage(next_resolve)??;
                committer.commit(resolved)?;
                next_resolve += 1;
                outstanding -= 1;
            }

            let SegmentCommitter { decoded, crc, .. } = committer;
            Ok((next_start as u64, decoded, crc.finalize(), speculative_chunks, fallback_chunks))
        },
    )?;
    Ok(DeflateReport { source_len: (end_byte - start_byte) as u64, compressed_end_bit, decoded_len, crc, blocks, speculative_chunks, fallback_chunks })
}

fn parse_header(data: &[u8], start: usize) -> Result<Header> {
    let fixed_end = start.checked_add(10).ok_or_else(|| invalid("header offset overflow"))?;
    let fixed = data.get(start..fixed_end).ok_or_else(|| invalid_at(start, "truncated member header"))?;
    if fixed[0..2] != [0x1f, 0x8b] { return Err(invalid_at(start, "missing 1f 8b magic")); }
    if fixed[2] != 8 { return Err(invalid_at(start + 2, format!("unsupported compression method {}", fixed[2]))); }
    let flags = fixed[3];
    if flags & 0xe0 != 0 { return Err(invalid_at(start + 3, format!("reserved header flags set: {flags:02x}"))); }
    let mtime = u32::from_le_bytes(fixed[4..8].try_into().unwrap());
    let mut cursor = fixed_end;
    if flags & 0x04 != 0 {
        let length_bytes = data.get(cursor..cursor + 2).ok_or_else(|| invalid_at(cursor, "truncated FEXTRA length"))?;
        let length = u16::from_le_bytes(length_bytes.try_into().unwrap()) as usize;
        cursor = cursor.checked_add(2 + length).ok_or_else(|| invalid("header offset overflow"))?;
        if cursor > data.len() { return Err(invalid_at(cursor.saturating_sub(length), "truncated FEXTRA data")); }
    }
    let name = if flags & 0x08 != 0 { Some(read_zero_terminated(data, &mut cursor, "FNAME")?) } else { None };
    let comment = if flags & 0x10 != 0 { Some(read_zero_terminated(data, &mut cursor, "FCOMMENT")?) } else { None };
    if flags & 0x02 != 0 {
        let expected_bytes = data.get(cursor..cursor + 2).ok_or_else(|| invalid_at(cursor, "truncated FHCRC"))?;
        let expected = u16::from_le_bytes(expected_bytes.try_into().unwrap());
        let actual = crc32(&data[start..cursor]) as u16;
        if actual != expected { return Err(invalid_at(cursor, format!("header CRC16 mismatch: expected {expected:04x}, decoded {actual:04x}"))); }
        cursor += 2;
    }
    Ok(Header { deflate_start: cursor, mtime, extra_flags: fixed[8], operating_system: fixed[9], name, comment })
}

fn read_zero_terminated(data: &[u8], cursor: &mut usize, field: &str) -> Result<Vec<u8>> {
    let rest = data.get(*cursor..).ok_or_else(|| invalid_at(*cursor, format!("truncated {field}")))?;
    let length = rest.iter().position(|&byte| byte == 0).ok_or_else(|| invalid_at(*cursor, format!("unterminated {field}")))?;
    let value = rest[..length].to_vec();
    *cursor = cursor.checked_add(length + 1).ok_or_else(|| invalid("header offset overflow"))?;
    Ok(value)
}

fn decode_deflate(
    bits: &mut Bits<'_>,
    emitter: &mut impl DeflateOutput,
    member: u32,
    blocks: &mut Vec<Block>,
    progress: &mut impl FnMut(DecodeProgress),
) -> Result<()> { loop { if decode_block(bits, emitter, member, blocks, progress)? { return Ok(()); } } }

fn decode_block(
    bits: &mut Bits<'_>,
    emitter: &mut impl DeflateOutput,
    member: u32,
    blocks: &mut Vec<Block>,
    progress: &mut impl FnMut(DecodeProgress),
) -> Result<bool> {
    let compressed_start_bit = bits.position_bits() as u64;
    let decoded_start = emitter.total_decoded();
    let final_block = bits.read(1)? != 0;
    let kind = match bits.read(2)? {
        0 => {
            decode_stored(bits, emitter)?;
            BlockKind::Stored
        }
        1 => {
            let (literal, distance) = fixed_tables()?;
            decode_huffman_block(bits, emitter, literal, distance)?;
            BlockKind::FixedHuffman
        }
        2 => {
            let (literal, distance) = dynamic_tables(bits)?;
            decode_huffman_block(bits, emitter, &literal, &distance)?;
            BlockKind::DynamicHuffman
        }
        _ => return Err(invalid_bit(bits.position_bits().saturating_sub(2), "reserved DEFLATE block type")),
    };
    let decoded_end = emitter.total_decoded();
    let compressed_end_bit = bits.position_bits() as u64;
    blocks.push(Block { member, kind, final_block, compressed_start_bit, compressed_end_bit, decoded_start, decoded_len: decoded_end - decoded_start });
    progress(DecodeProgress { compressed_bytes: compressed_end_bit.div_ceil(8), decoded_bytes: decoded_end });
    Ok(final_block)
}

fn decode_stored(bits: &mut Bits<'_>, emitter: &mut impl DeflateOutput) -> Result<()> {
    bits.align_byte();
    let length = bits.read(16)? as u16;
    let complement = bits.read(16)? as u16;
    if length != !complement { return Err(invalid_bit(bits.position_bits().saturating_sub(16), "stored-block LEN/NLEN mismatch")); }
    emitter.extend(bits.read_aligned_bytes(length as usize)?)?;
    Ok(())
}

fn decode_huffman_block(bits: &mut Bits<'_>, emitter: &mut impl DeflateOutput, literal: &Huffman, distance: &Huffman) -> Result<()> {
    loop {
        let symbol = literal.decode(bits)?;
        match symbol {
            0..=255 => emitter.emit(symbol as u8)?,
            256 => return Ok(()),
            257..=285 => {
                let length_index = symbol as usize - 257;
                let length = LENGTH_BASE[length_index] + bits.read(LENGTH_EXTRA[length_index])? as usize;
                let distance_symbol = distance.decode(bits)? as usize;
                if distance_symbol >= DISTANCE_BASE.len() {
                    return Err(invalid_bit(bits.position_bits(), format!("invalid distance symbol {distance_symbol}")));
                }
                let distance = DISTANCE_BASE[distance_symbol] + bits.read(DISTANCE_EXTRA[distance_symbol])? as usize;
                emitter.copy(distance, length)?;
            }
            _ => return Err(invalid_bit(bits.position_bits(), format!("invalid literal/length symbol {symbol}"))),
        }
    }
}

fn dynamic_tables(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman)> {
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let literal_count = bits.read(5)? as usize + 257;
    let distance_count = bits.read(5)? as usize + 1;
    let code_count = bits.read(4)? as usize + 4;
    let mut code_lengths = [0_u8; 19];
    for &symbol in &ORDER[..code_count] { code_lengths[symbol] = bits.read(3)? as u8; }
    let code_table = Huffman::new(&code_lengths)?;
    let total = literal_count + distance_count;
    let mut lengths = Vec::with_capacity(total);
    while lengths.len() < total {
        match code_table.decode(bits)? {
            length @ 0..=15 => lengths.push(length as u8),
            16 => {
                let previous = *lengths.last().ok_or_else(|| invalid_bit(bits.position_bits(), "repeat code 16 has no previous length"))?;
                let count = bits.read(2)? as usize + 3;
                append_lengths(&mut lengths, total, previous, count, bits.position_bits())?;
            }
            17 => {
                let count = bits.read(3)? as usize + 3;
                append_lengths(&mut lengths, total, 0, count, bits.position_bits())?;
            }
            18 => {
                let count = bits.read(7)? as usize + 11;
                append_lengths(&mut lengths, total, 0, count, bits.position_bits())?;
            }
            symbol => return Err(invalid_bit(bits.position_bits(), format!("invalid code-length symbol {symbol}"))),
        }
    }
    if lengths[256] == 0 { return Err(invalid_bit(bits.position_bits(), "literal/length table has no end-of-block symbol")); }
    Ok((Huffman::new(&lengths[..literal_count])?, Huffman::new(&lengths[literal_count..])?))
}

fn append_lengths(lengths: &mut Vec<u8>, total: usize, value: u8, count: usize, bit: usize) -> Result<()> {
    if lengths.len().saturating_add(count) > total { return Err(invalid_bit(bit, "code-length repeat exceeds table")); }
    lengths.resize(lengths.len() + count, value);
    Ok(())
}

fn fixed_tables() -> Result<&'static (Huffman, Huffman)> {
    static TABLES: OnceLock<(Huffman, Huffman)> = OnceLock::new();
    if let Some(tables) = TABLES.get() { return Ok(tables); }
    let mut literal_lengths = [0_u8; 288];
    literal_lengths[..144].fill(8);
    literal_lengths[144..256].fill(9);
    literal_lengths[256..280].fill(7);
    literal_lengths[280..].fill(8);
    let tables = (Huffman::new(&literal_lengths)?, Huffman::new(&[5; 32])?);
    let _ = TABLES.set(tables);
    Ok(TABLES.get().unwrap())
}

#[derive(Clone, Debug)]
struct Huffman {
    // `(bit length << 9) | symbol`, indexed by the next `max_bits` stream bits.
    table: Vec<u16>,
    max_bits: u8,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self> {
        let mut counts = [0_u16; MAX_CODE_BITS + 1];
        let mut max_bits = 0_u8;
        for &length in lengths {
            if length as usize > MAX_CODE_BITS { return Err(invalid(format!("Huffman code length {length} exceeds {MAX_CODE_BITS}"))); }
            if length != 0 {
                counts[length as usize] += 1;
                max_bits = max_bits.max(length);
            }
        }
        if max_bits == 0 { return Ok(Self { table: Vec::new(), max_bits: 0 }); }
        let mut remaining = 1_i32;
        for &count in &counts[1..] {
            remaining = (remaining << 1) - i32::from(count);
            if remaining < 0 { return Err(invalid("oversubscribed Huffman table")); }
        }
        let mut next_code = [0_u16; MAX_CODE_BITS + 1];
        let mut code = 0_u16;
        for bits in 1..=MAX_CODE_BITS {
            code = (code + counts[bits - 1]) << 1;
            next_code[bits] = code;
        }
        let mut table = vec![u16::MAX; 1_usize << max_bits];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length == 0 { continue; }
            let canonical = next_code[length as usize];
            next_code[length as usize] += 1;
            let reversed = reverse_low_bits(canonical, length) as usize;
            let suffix_bits = max_bits - length;
            let packed = (u16::from(length) << 9) | symbol as u16;
            for suffix in 0..(1_usize << suffix_bits) { table[reversed | suffix << length] = packed; }
        }
        Ok(Self { table, max_bits })
    }

    #[inline(always)]
    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16> {
        if self.max_bits == 0 { return Err(invalid_bit(bits.position_bits(), "attempted to decode an empty Huffman table")); }
        let remaining = bits.data.len().saturating_mul(8).saturating_sub(bits.bit);
        let peek_bits = usize::from(self.max_bits).min(remaining) as u8;
        let packed = self.table[bits.peek(peek_bits)? as usize];
        if packed == u16::MAX { return Err(invalid_bit(bits.position_bits(), "invalid Huffman code")); }
        let length = (packed >> 9) as u8;
        if usize::from(length) > remaining { return Err(invalid_bit(bits.position_bits(), "truncated Huffman code")); }
        bits.drop(length);
        Ok(packed & 0x01ff)
    }
}

fn reverse_low_bits(value: u16, count: u8) -> u16 { value.reverse_bits() >> (u16::BITS as u8 - count) }

#[derive(Clone)]
struct Bits<'a> { data: &'a [u8], bit: usize }

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], start_byte: usize) -> Self { Self { data, bit: start_byte.saturating_mul(8) } }

    fn at(data: &'a [u8], bit: usize) -> Result<Self> {
        if bit > data.len().saturating_mul(8) { return Err(invalid_bit(bit, "unexpected end of DEFLATE data")); }
        Ok(Self { data, bit })
    }

    #[inline(always)]
    fn position_bits(&self) -> usize { self.bit }

    #[inline(always)]
    fn peek(&self, count: u8) -> Result<u32> {
        let count = usize::from(count);
        if self.bit.checked_add(count).is_none_or(|end| end > self.data.len().saturating_mul(8)) {
            return Err(invalid_bit(self.bit, "unexpected end of DEFLATE data"));
        }
        let byte = self.bit / 8;
        let shift = self.bit & 7;
        let word = if self.data.len().saturating_sub(byte) >= 8 { u64::from_le_bytes(self.data[byte..byte + 8].try_into().unwrap()) } else { self.data[byte..].iter().take(8).enumerate().fold(0_u64, |word, (index, &value)| word | u64::from(value) << (index * 8)) };
        let mask = if count == 0 { 0 } else { (1_u64 << count) - 1 };
        Ok(((word >> shift) & mask) as u32)
    }

    #[inline(always)]
    fn drop(&mut self, count: u8) { self.bit += usize::from(count); }

    #[inline(always)]
    fn read(&mut self, count: u8) -> Result<u32> {
        if count == 0 { return Ok(0); }
        let value = self.peek(count)?;
        self.drop(count);
        Ok(value)
    }

    fn align_byte(&mut self) { self.bit = self.bit.saturating_add(7) & !7; }

    fn read_aligned_bytes(&mut self, count: usize) -> Result<&'a [u8]> {
        if self.bit & 7 != 0 { return Err(invalid_bit(self.bit, "internal unaligned byte read")); }
        let start = self.bit / 8;
        let end = start.checked_add(count).ok_or_else(|| invalid("DEFLATE offset overflow"))?;
        let bytes = self.data.get(start..end).ok_or_else(|| invalid_bit(self.bit, "truncated stored block"))?;
        self.bit = end * 8;
        Ok(bytes)
    }
}

trait DeflateOutput {
    fn total_decoded(&self) -> u64;
    fn emit(&mut self, byte: u8) -> Result<()>;
    fn extend(&mut self, bytes: &[u8]) -> Result<()>;
    fn copy(&mut self, distance: usize, length: usize) -> Result<()>;
}

struct Emitter<'a, W> {
    output: &'a mut W,
    buffer: Vec<u8>,
    history_len: usize,
    crc: crc32fast::Hasher,
    member_decoded: u64,
    decoded_base: u64,
}

impl<'a, W: OutputSink> Emitter<'a, W> {
    fn new(output: &'a mut W, decoded_base: u64) -> Self {
        Self {
            output,
            buffer: Vec::with_capacity(WINDOW_SIZE + OUTPUT_CHUNK + 258),
            history_len: 0,
            crc: crc32fast::Hasher::new(),
            member_decoded: 0,
            decoded_base,
        }
    }

    fn decoded_position(&self) -> u64 { self.decoded_base + self.member_decoded }

    fn emit_byte(&mut self, byte: u8) -> Result<()> {
        self.buffer.push(byte);
        self.member_decoded = self.member_decoded.checked_add(1).ok_or_else(|| invalid("decoded offset overflow"))?;
        if self.buffer.len() - self.history_len >= OUTPUT_CHUNK { self.flush_pending()?; }
        Ok(())
    }

    fn extend_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(bytes);
        self.member_decoded = self.member_decoded.checked_add(bytes.len() as u64).ok_or_else(|| invalid("decoded offset overflow"))?;
        if self.buffer.len() - self.history_len >= OUTPUT_CHUNK { self.flush_pending()?; }
        Ok(())
    }

    fn copy_match(&mut self, distance: usize, length: usize) -> Result<()> {
        let available = self.member_decoded.min(WINDOW_SIZE as u64) as usize;
        if distance == 0 || distance > available { return Err(invalid(format!("back-reference distance {distance} exceeds {available} available bytes"))); }
        extend_match(&mut self.buffer, distance, length);
        self.member_decoded = self.member_decoded.checked_add(length as u64).ok_or_else(|| invalid("decoded offset overflow"))?;
        if self.buffer.len() - self.history_len >= OUTPUT_CHUNK { self.flush_pending()?; }
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<()> {
        let pending = &self.buffer[self.history_len..];
        if pending.is_empty() { return Ok(()); }
        self.output.write_borrowed(pending)?;
        self.crc.update(pending);
        self.history_len = self.buffer.len();
        if self.buffer.len() >= HISTORY_COMPACT {
            let keep = self.buffer.len().min(WINDOW_SIZE);
            let start = self.buffer.len() - keep;
            self.buffer.copy_within(start.., 0);
            self.buffer.truncate(keep);
            self.history_len = keep;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(u32, u64)> { self.flush_pending()?; Ok((self.crc.finalize(), self.member_decoded)) }
}

impl<W: OutputSink> DeflateOutput for Emitter<'_, W> {
    fn total_decoded(&self) -> u64 { self.decoded_position() }

    fn emit(&mut self, byte: u8) -> Result<()> { self.emit_byte(byte) }

    fn extend(&mut self, bytes: &[u8]) -> Result<()> { self.extend_bytes(bytes) }

    fn copy(&mut self, distance: usize, length: usize) -> Result<()> { self.copy_match(distance, length) }
}

pub fn crc32(data: &[u8]) -> u32 { crc32fast::hash(data) }

fn invalid(message: impl Into<String>) -> Error { Error::InvalidGzip(message.into()) }

fn invalid_at(byte: usize, message: impl Into<String>) -> Error { invalid(format!("at byte {byte}: {}", message.into())) }

fn invalid_bit(bit: usize, message: impl Into<String>) -> Error { invalid(format!("at bit {bit}: {}", message.into())) }

#[cfg(test)]
mod tests {
    use flate2::{
        Compression, GzBuilder,
        write::{DeflateEncoder, GzEncoder},
    };

    use super::*;

    fn patterned(size: usize) -> Vec<u8> { (0..size).map(|index| ((index * 37 + index / 251) & 255) as u8).collect() }

    fn compress(data: &[u8], level: Compression) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), level);
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn compress_raw(data: &[u8]) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn raw_deflate_reuses_serial_and_parallel_decoder() {
        let plain = patterned(20 * 1024 * 1024);
        let compressed = compress_raw(&plain);
        for threads in [1, 4] {
            let mut output = Vec::new();
            let mut sink = WriterSink::new(&mut output);
            let report =
                decompress_deflate_to_sink_with_options_and_progress(&compressed, &mut sink, DecodeOptions { threads, ..DecodeOptions::default() }, |_| {})
                    .unwrap();
            assert_eq!(output, plain);
            assert_eq!(report.decoded_len, plain.len() as u64);
            assert_eq!(report.crc, crc32(&plain));
        }
        for size in 0..256 {
            let plain = patterned(size);
            let compressed = compress_raw(&plain);
            let mut output = Vec::new();
            let mut sink = WriterSink::new(&mut output);
            decompress_deflate_to_sink_with_options_and_progress(&compressed, &mut sink, DecodeOptions { threads: 1, ..DecodeOptions::default() }, |_| {})
                .unwrap();
            assert_eq!(output, plain);
        }
    }

    #[test]
    fn decodes_stored_fixed_and_dynamic_blocks() {
        let cases =
            [(b"stored bytes".repeat(2_000), Compression::none()), (b"fixed huffman".to_vec(), Compression::fast()), (patterned(350_000), Compression::best())];
        let mut kinds = Vec::new();
        for (plain, level) in cases {
            let compressed = compress(&plain, level);
            let mut output = Vec::new();
            let report = decompress_to_writer(&compressed, &mut output).unwrap();
            assert_eq!(output, plain);
            kinds.extend(report.blocks.into_iter().map(|block| block.kind));
        }
        assert!(kinds.contains(&BlockKind::Stored));
        assert!(kinds.contains(&BlockKind::FixedHuffman));
        assert!(kinds.contains(&BlockKind::DynamicHuffman));
    }

    #[test]
    fn validates_concatenated_members_and_reports_progress() {
        let first = patterned(180_000);
        let second = b"second member".repeat(4_000);
        let mut compressed = compress(&first, Compression::fast());
        compressed.extend_from_slice(&compress(&second, Compression::best()));
        let mut expected = first;
        expected.extend_from_slice(&second);
        let mut output = Vec::new();
        let mut reports = Vec::new();
        let report = decompress_to_writer_with_progress(&compressed, &mut output, |progress| reports.push(progress)).unwrap();
        assert_eq!(output, expected);
        assert_eq!(report.members.len(), 2);
        assert_eq!(reports.last(), Some(&DecodeProgress { compressed_bytes: compressed.len() as u64, decoded_bytes: expected.len() as u64 }));
        assert!(reports.windows(2).all(|pair| pair[0].compressed_bytes <= pair[1].compressed_bytes && pair[0].decoded_bytes <= pair[1].decoded_bytes));
    }

    #[test]
    fn parses_optional_header_fields_and_header_crc() {
        let plain = b"header metadata";
        let base = GzBuilder::new().mtime(123456).operating_system(3).write(Vec::new(), Compression::fast());
        let mut encoder = base;
        encoder.write_all(plain).unwrap();
        let base = encoder.finish().unwrap();
        let mut compressed = base[..10].to_vec();
        compressed[3] = 0x1e;
        compressed.extend_from_slice(&(3_u16).to_le_bytes());
        compressed.extend_from_slice(b"xyz");
        compressed.extend_from_slice(b"name.txt\0comment\0");
        let header_crc = crc32(&compressed) as u16;
        compressed.extend_from_slice(&header_crc.to_le_bytes());
        compressed.extend_from_slice(&base[10..]);
        let mut output = Vec::new();
        let report = decompress_to_writer(&compressed, &mut output).unwrap();
        assert_eq!(output, plain);
        assert_eq!(report.members[0].mtime, 123456);
        assert_eq!(report.members[0].name.as_deref(), Some(b"name.txt".as_slice()));
        assert_eq!(report.members[0].comment.as_deref(), Some(b"comment".as_slice()));
    }

    #[test]
    fn rejects_header_payload_and_size_corruption() {
        let plain = patterned(20_000);
        let compressed = compress(&plain, Compression::best());

        let mut reserved = compressed.clone();
        reserved[3] |= 0x20;
        assert!(matches!(decompress(&reserved), Err(Error::InvalidGzip(_))));

        let mut payload = compressed.clone();
        payload[12] ^= 1;
        assert!(decompress(&payload).is_err());

        let mut crc = compressed.clone();
        let crc_byte = crc.len() - 8;
        crc[crc_byte] ^= 1;
        assert!(matches!(decompress(&crc), Err(Error::InvalidGzip(message)) if message.contains("CRC32 mismatch")));

        let mut size = compressed;
        let size_byte = size.len() - 4;
        size[size_byte] ^= 1;
        assert!(matches!(decompress(&size), Err(Error::InvalidGzip(message)) if message.contains("ISIZE mismatch")));
    }

    #[test]
    fn differentially_decodes_varied_inputs_and_levels() {
        let mut random = Vec::with_capacity(70_000);
        let mut state = 0x1234_5678_u32;
        for _ in 0..70_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            random.push(state as u8);
        }
        let inputs = [Vec::new(), vec![0], vec![0; 32_769], b"abc".repeat(20_000), patterned(70_000), random];
        for plain in inputs {
            for level in [Compression::none(), Compression::fast(), Compression::new(6), Compression::best()] {
                let compressed = compress(&plain, level);
                assert_eq!(decompress(&compressed).unwrap(), plain);
            }
        }
    }

    #[test]
    fn malformed_inputs_return_errors_without_panicking() {
        let valid = compress(&patterned(4_000), Compression::best());
        for end in 0..valid.len() { assert!(decompress(&valid[..end]).is_err()); }
        for byte in 0..valid.len().min(64) {
            let mut damaged = valid.clone();
            damaged[byte] ^= 0x5a;
            let _ = decompress(&damaged);
        }
    }
}
