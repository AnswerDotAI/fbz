use std::{io::Write, sync::Arc, thread};

use rayon::{ThreadPool, ThreadPoolBuilder};

use crate::format::scan_with_pool;
use crate::pipeline::{Job, OrderedResults, PipelineLimits, run_ordered};
use crate::{
    BlockCandidate, BlockIndex, DecodeError, DecodeFormat, EndCandidate, Error, Index, MAX_DECODED_BLOCK, OutputSink, Result, StreamIndex, WriterSink,
    combine_stream_crc, decoder,
};

pub const DEFAULT_MEMORY_LIMIT: usize = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeProgress { pub compressed_bytes: u64, pub decoded_bytes: u64 }

#[derive(Clone, Copy, Debug)]
pub struct DecodeOptions {
    /// Compression format, or automatic magic/filename detection.
    pub format: DecodeFormat,
    /// Zero selects the process's available parallelism.
    pub threads: usize,
    /// Maximum decoded bytes reserved for in-flight and completed speculative blocks.
    pub memory_limit: usize,
}

impl Default for DecodeOptions { fn default() -> Self { Self { format: DecodeFormat::Auto, threads: 0, memory_limit: DEFAULT_MEMORY_LIMIT } } }

impl DecodeOptions {
    pub fn resolved_threads(self) -> usize { if self.threads != 0 { self.threads } else { thread::available_parallelism().map(usize::from).unwrap_or(1) } }

    pub(crate) fn validate(self) -> Result<Self> {
        if self.memory_limit < MAX_DECODED_BLOCK {
            return Err(Error::InvalidConfiguration(format!("memory limit must be at least {MAX_DECODED_BLOCK} bytes")));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug)]
enum Marker { Block(BlockCandidate), End(EndCandidate) }

impl Marker { fn bit_offset(&self) -> u64 { match self { Self::Block(block) => block.bit_offset, Self::End(end) => end.bit_offset } } }

pub fn decompress(data: &[u8], options: DecodeOptions) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    decompress_to_writer(data, &mut output, options)?;
    Ok(output)
}

/// Decode to a streaming output. A one-thread request uses the same pure-Rust
/// block codec without the indexing/speculation overhead.
pub fn decompress_to_writer(data: &[u8], output: &mut impl Write, options: DecodeOptions) -> Result<()> {
    decompress_to_writer_with_progress(data, output, options, |_| {})
}

pub fn decompress_to_writer_with_progress(data: &[u8], output: &mut impl Write, options: DecodeOptions, progress: impl FnMut(DecodeProgress)) -> Result<()> {
    let mut output = WriterSink::new(output);
    decompress_to_sink_with_progress(data, &mut output, options, progress)
}

/// Decode into an output that can take ownership of completed chunks.
#[doc(hidden)]
pub fn decompress_to_sink_with_progress(
    data: &[u8],
    output: &mut impl OutputSink,
    options: DecodeOptions,
    mut progress: impl FnMut(DecodeProgress),
) -> Result<()> {
    let options = options.validate()?;
    if options.resolved_threads() == 1 {
        return decoder::decode_serial_with_progress(data, output, &mut |compressed_bytes, decoded_bytes| {
            progress(DecodeProgress { compressed_bytes, decoded_bytes });
        });
    }
    let prefetched = if data.get(4..10) == Some(&[0x31, 0x41, 0x59, 0x26, 0x53, 0x59]) {
        let level = parse_header(data, 0)?;
        let (expected_crc, mut decoded) = decoder::decode_first_candidate(data)?;
        if decoded.block_len > usize::from(level) * 100_000 { return Err(Error::Decode { bit_offset: 32, source: DecodeError::BlockOverflow }); }
        output.write_owned_from(std::mem::take(&mut decoded.output), 0)?;
        Some(PrefetchedCandidate { bit_offset: 32, expected_crc, decoded })
    } else { None };
    decode_to_sink_impl_with_prefetched(data, output, options, &mut progress, prefetched).map(|_| ())
}

pub fn build_index(data: &[u8], options: DecodeOptions) -> Result<Index> { build_index_with_progress(data, options, |_| {}) }

pub fn build_index_with_progress(data: &[u8], options: DecodeOptions, mut progress: impl FnMut(DecodeProgress)) -> Result<Index> {
    let mut output = WriterSink::new(std::io::sink());
    decode_to_sink_impl(data, &mut output, options, &mut progress)
}

/// Decode a complete bzip2 input, validate every CRC, and build its block index.
pub fn decode_to_writer(data: &[u8], output: &mut impl Write, options: DecodeOptions) -> Result<Index> {
    let mut output = WriterSink::new(output);
    decode_to_sink_impl(data, &mut output, options, &mut |_| {})
}

/// Decode a complete bzip2 input, return its index, and report completed work.
pub fn decode_to_writer_with_progress(data: &[u8], output: &mut impl Write, options: DecodeOptions, mut progress: impl FnMut(DecodeProgress)) -> Result<Index> {
    let mut output = WriterSink::new(output);
    decode_to_sink_impl(data, &mut output, options, &mut progress)
}

fn decode_to_sink_impl(data: &[u8], output: &mut impl OutputSink, options: DecodeOptions, progress: &mut impl FnMut(DecodeProgress)) -> Result<Index> {
    decode_to_sink_impl_with_prefetched(data, output, options, progress, None)
}

struct PrefetchedCandidate { bit_offset: u64, expected_crc: u32, decoded: decoder::DecodedCandidate }

fn decode_to_sink_impl_with_prefetched(
    data: &[u8],
    output: &mut impl OutputSink,
    options: DecodeOptions,
    progress: &mut impl FnMut(DecodeProgress),
    prefetched: Option<PrefetchedCandidate>,
) -> Result<Index> {
    let options = options.validate()?;
    let threads = options.resolved_threads();
    let pool = thread_pool(threads)?;
    let scanned = scan_with_pool(data, pool.as_deref(), || output.is_cancelled())?;
    let mut markers: Vec<_> = scanned.blocks.into_iter().map(Marker::Block).collect();
    markers.extend(scanned.stream_ends.into_iter().map(Marker::End));
    markers.sort_unstable_by_key(Marker::bit_offset);
    markers.dedup_by_key(|marker| marker.bit_offset());

    if markers.len() > data.len() / 16 + 64 { return Err(Error::InvalidConfiguration("input contains too many speculative markers".into())); }

    let prefetched = prefetched
        .map(|prefetched| {
            let marker_index = marker_at(&markers, prefetched.bit_offset)?;
            let Marker::Block(block) = &markers[marker_index] else { return Err(required_marker(prefetched.bit_offset)) };
            if block.expected_crc != prefetched.expected_crc {
                return Err(Error::Decode { bit_offset: prefetched.bit_offset, source: DecodeError::InvalidBlock });
            }
            Ok((marker_index, prefetched.decoded))
        })
        .transpose()?;

    let Some(pool) = pool else {
        debug_assert!(prefetched.is_none());
        let mut candidates = SerialCandidates { data, markers: &markers };
        return assemble(data, output, &markers, &mut candidates, progress);
    };
    let jobs: Vec<_> = markers
        .iter()
        .enumerate()
        .filter_map(|(marker_index, marker)| match marker {
            Marker::Block(_) if prefetched.as_ref().is_some_and(|(prefetched_index, _)| *prefetched_index == marker_index) => None,
            Marker::Block(block) => Some(Job {
                key: marker_index,
                reservation: MAX_DECODED_BLOCK,
                payload: CandidateJob { start_bit: block.bit_offset, expected_crc: block.expected_crc },
            }),
            Marker::End(_) => None,
        })
        .collect();
    run_ordered(
        &pool,
        &jobs,
        PipelineLimits { memory: options.memory_limit, active: usize::MAX },
        |job| decoder::decode_candidate(data, job.start_bit, job.expected_crc),
        candidate_len,
        |results| { let mut candidates = ParallelCandidates { results, prefetched }; assemble(data, output, &markers, &mut candidates, progress) },
    )
}

fn assemble(
    data: &[u8],
    output: &mut impl OutputSink,
    markers: &[Marker],
    candidates: &mut impl Candidates,
    progress: &mut impl FnMut(DecodeProgress),
) -> Result<Index> {
    let mut blocks = Vec::new();
    let mut streams = Vec::new();
    let mut decoded_offset = 0_u64;
    let mut header_byte = 0_u64;

    while header_byte < data.len() as u64 {
        let level = parse_header(data, header_byte)?;
        let stream_number = streams.len() as u64;
        let first_block = blocks.len() as u64;
        let stream_decoded_start = decoded_offset;
        let mut combined_crc = 0_u32;
        let mut current_bit = header_byte.checked_mul(8).and_then(|bit| bit.checked_add(32)).ok_or_else(offset_overflow)?;
        let mut marker_index = marker_at(markers, current_bit)?;
        loop {
            match &markers[marker_index] {
                Marker::End(end) => {
                    if end.expected_stream_crc != combined_crc { return Err(Error::Decode { bit_offset: end.bit_offset, source: DecodeError::CrcMismatch }); }
                    let after_eos = end.bit_offset.checked_add(80).ok_or_else(offset_overflow)?;
                    header_byte = after_eos.checked_add(7).ok_or_else(offset_overflow)? / 8;
                    streams.push(StreamIndex {
                        compressed_header_byte: current_stream_header(current_bit, first_block, &blocks, header_byte),
                        block_size_100k: level,
                        first_block,
                        block_count: blocks.len() as u64 - first_block,
                        decoded_start: stream_decoded_start,
                        decoded_len: decoded_offset - stream_decoded_start,
                        eos_bit: end.bit_offset,
                        expected_stream_crc: end.expected_stream_crc,
                    });
                    break;
                }
                Marker::Block(block) => {
                    let decoded = candidates.take(marker_index)?;
                    if decoded.block_len > usize::from(level) * 100_000 {
                        return Err(Error::Decode { bit_offset: block.bit_offset, source: DecodeError::BlockOverflow });
                    }
                    let end_index = marker_at(markers, decoded.end_bit)?;
                    candidates.discard_before(end_index);
                    let decoded_len = decoded.decoded_len as u64;
                    output.write_owned_from(decoded.output, 0)?;
                    blocks.push(BlockIndex {
                        compressed_start_bit: block.bit_offset,
                        compressed_end_bit: markers[end_index].bit_offset(),
                        decoded_start: decoded_offset,
                        decoded_len,
                        expected_crc: block.expected_crc,
                        stream: stream_number,
                    });
                    decoded_offset = decoded_offset.checked_add(decoded_len).ok_or_else(offset_overflow)?;
                    progress(DecodeProgress { compressed_bytes: decoded.end_bit.div_ceil(8), decoded_bytes: decoded_offset });
                    combined_crc = combine_stream_crc(combined_crc, block.expected_crc);
                    current_bit = markers[end_index].bit_offset();
                    marker_index = end_index;
                }
            }
        }
        if header_byte == data.len() as u64 { break; }
        if header_byte > data.len() as u64 { return Err(Error::Decode { bit_offset: data.len() as u64 * 8, source: DecodeError::Truncated }); }
    }

    output.flush()?;
    progress(DecodeProgress { compressed_bytes: data.len() as u64, decoded_bytes: decoded_offset });
    Ok(Index::new(data, decoded_offset, streams, blocks))
}

trait Candidates {
    fn take(&mut self, marker_index: usize) -> Result<decoder::DecodedCandidate>;
    fn discard_before(&mut self, marker_index: usize);
}

struct SerialCandidates<'a> { data: &'a [u8], markers: &'a [Marker] }

impl Candidates for SerialCandidates<'_> {
    fn take(&mut self, marker_index: usize) -> Result<decoder::DecodedCandidate> {
        let Marker::Block(block) = &self.markers[marker_index] else { return Err(required_marker(self.markers[marker_index].bit_offset())) };
        decoder::decode_candidate(self.data, block.bit_offset, block.expected_crc)
    }

    fn discard_before(&mut self, _marker_index: usize) {}
}

#[derive(Clone, Copy)]
struct CandidateJob { start_bit: u64, expected_crc: u32 }

fn candidate_len(result: &Result<decoder::DecodedCandidate>) -> usize { result.as_ref().map_or(0, |decoded| decoded.output.len()) }

struct ParallelCandidates<'results, 'pipeline> {
    results: &'results mut OrderedResults<'pipeline, Result<decoder::DecodedCandidate>>,
    prefetched: Option<(usize, decoder::DecodedCandidate)>,
}

impl Candidates for ParallelCandidates<'_, '_> {
    fn take(&mut self, marker_index: usize) -> Result<decoder::DecodedCandidate> {
        if self.prefetched.as_ref().is_some_and(|(prefetched_index, _)| *prefetched_index == marker_index) { return Ok(self.prefetched.take().unwrap().1); }
        self.results.take(marker_index)?
    }

    fn discard_before(&mut self, marker_index: usize) {
        if self.prefetched.as_ref().is_some_and(|(prefetched_index, _)| *prefetched_index < marker_index) { self.prefetched.take(); }
        self.results.discard_before(marker_index);
    }
}

pub(crate) fn thread_pool(threads: usize) -> Result<Option<Arc<ThreadPool>>> {
    if threads <= 1 { return Ok(None); }
    ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|number| format!("fbz-{number}"))
        .build()
        .map(Arc::new)
        .map(Some)
        .map_err(|error| Error::InvalidConfiguration(error.to_string()))
}

fn marker_at(markers: &[Marker], bit_offset: u64) -> Result<usize> {
    markers.binary_search_by_key(&bit_offset, Marker::bit_offset).map_err(|_| required_marker(bit_offset))
}

fn parse_header(data: &[u8], byte_offset: u64) -> Result<u8> {
    let offset = usize::try_from(byte_offset).map_err(|_| offset_overflow())?;
    let Some(header) = data.get(offset..offset.saturating_add(4)) else {
        return Err(Error::Decode { bit_offset: byte_offset.saturating_mul(8), source: DecodeError::Truncated });
    };
    if &header[..3] != b"BZh" { return Err(Error::Decode { bit_offset: byte_offset * 8, source: DecodeError::InvalidMagic }); }
    if !(b'1'..=b'9').contains(&header[3]) { return Err(Error::Decode { bit_offset: byte_offset * 8 + 24, source: DecodeError::InvalidLevel }); }
    Ok(header[3] - b'0')
}

fn current_stream_header(current_bit: u64, first_block: u64, blocks: &[BlockIndex], next_header: u64) -> u64 {
    if let Some(first) = blocks.get(first_block as usize) { (first.compressed_start_bit - 32) / 8 } else {
        // Empty streams have no block from which to derive the header. `current_bit`
        // is their EOS position, exactly 32 bits after the header.
        let candidate = current_bit.saturating_sub(32) / 8;
        candidate.min(next_header)
    }
}

fn required_marker(bit_offset: u64) -> Error { Error::Decode { bit_offset, source: DecodeError::InvalidMagic } }

fn offset_overflow() -> Error { Error::InvalidConfiguration("offset arithmetic overflow".into()) }

#[cfg(test)]
mod tests {
    use super::*;
    use crabz2::{Level, compress};

    fn patterned(size: usize) -> Vec<u8> { (0..size).map(|index| ((index * 37 + index / 251) & 255) as u8).collect() }

    #[test]
    fn decodes_multiple_blocks_at_every_thread_setting() {
        let plain = patterned(350_000);
        let compressed = compress(&plain, Level::FASTEST);
        for threads in [1, 2, 4, 0] {
            let options = DecodeOptions { threads, ..DecodeOptions::default() };
            assert_eq!(decompress(&compressed, options).unwrap(), plain);
        }
    }

    #[test]
    fn progress_reaches_exact_input_and_output_lengths() {
        let plain = patterned(350_000);
        let compressed = compress(&plain, Level::FASTEST);
        for threads in [1, 4] {
            let options = DecodeOptions { threads, ..DecodeOptions::default() };
            let mut output = Vec::new();
            let mut reports = Vec::new();
            decompress_to_writer_with_progress(&compressed, &mut output, options, |progress| reports.push(progress)).unwrap();
            assert_eq!(output, plain);
            assert!(reports.windows(2).all(|pair| { pair[0].compressed_bytes <= pair[1].compressed_bytes && pair[0].decoded_bytes <= pair[1].decoded_bytes }));
            assert_eq!(reports.last(), Some(&DecodeProgress { compressed_bytes: compressed.len() as u64, decoded_bytes: plain.len() as u64 }));
        }
    }

    #[test]
    fn validates_concatenated_streams_and_indexes_them() {
        let first = patterned(180_000);
        let second = patterned(70_000);
        let mut compressed = compress(&first, Level::FASTEST);
        compressed.extend_from_slice(&compress(&second, Level::BEST));
        let mut expected = first;
        expected.extend_from_slice(&second);

        let mut output = Vec::new();
        let index = decode_to_writer(&compressed, &mut output, DecodeOptions::default()).unwrap();
        assert_eq!(output, expected);
        assert_eq!(index.streams.len(), 2);
        assert_eq!(index.decoded_len, expected.len() as u64);
        assert!(index.blocks.len() >= 3);
    }

    #[test]
    fn bounded_scheduler_crosses_many_short_streams() {
        let mut compressed = Vec::new();
        let mut expected = Vec::new();
        for stream in 0..32 {
            let plain = patterned(4_000 + stream * 31);
            compressed.extend_from_slice(&compress(&plain, Level::BEST));
            expected.extend_from_slice(&plain);
        }
        let options = DecodeOptions { threads: 4, memory_limit: MAX_DECODED_BLOCK * 2, ..DecodeOptions::default() };
        assert_eq!(decompress(&compressed, options).unwrap(), expected);
    }

    #[test]
    fn rejects_stream_crc_corruption() {
        let mut compressed = compress(b"integrity matters", Level::BEST);
        let last = compressed.len() - 2;
        compressed[last] ^= 1;
        assert!(matches!(decompress(&compressed, DecodeOptions::default()), Err(Error::Decode { .. })));
    }
}
