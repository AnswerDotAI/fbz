//! Parallel bzip2 compression.
//!
//! The BWT, MTF/RLE2, and grouped-Huffman implementation is adapted from
//! crabz2 0.4.0 by John Murray under the MIT license included in this folder.

mod bitwriter;
mod bwt;
mod huffman;
mod mtf;
mod rle1;

use std::io::{self, Read, Write};

use bitwriter::BitWriter;

use crate::{EncodeOptions, Error, Result, crc::Bz2Crc, pipeline::StreamingOrdered};

const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
const EOS_MAGIC: u64 = 0x1772_4538_5090;
const AUTO_WORKERS: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeReport { pub input_len: u64, pub output_len: u64, pub blocks: u64 }

struct BlockJob { rle: Vec<u8>, crc: u32, input_len: usize }

struct EncodedBlock {
    bytes: Vec<u8>,
    bit_len: usize,
    crc: u32,
    input_len: usize,
}

fn write_symbol_map(output: &mut BitWriter, in_use: &[u8]) {
    let mut used = [false; 256];
    for &byte in in_use { used[byte as usize] = true; }
    let mut groups = 0_u32;
    for (index, chunk) in used.chunks(16).enumerate() { if chunk.iter().any(|&value| value) { groups |= 1 << (15 - index); } }
    output.write_bits(16, groups);
    for (index, chunk) in used.chunks(16).enumerate() {
        if groups & (1 << (15 - index)) == 0 { continue; }
        let mut bits = 0_u32;
        for (bit, &value) in chunk.iter().enumerate() { if value { bits |= 1 << (15 - bit); } }
        output.write_bits(16, bits);
    }
}

fn encode_block(job: BlockJob) -> EncodedBlock {
    let (last, origin) = bwt::transform(&job.rle);
    let symbols = mtf::encode(&last);
    let coding = huffman::build(&symbols.syms, symbols.alpha_size());
    let mut output = BitWriter::with_capacity(job.rle.len() / 2);
    output.write_magic(BLOCK_MAGIC);
    output.write_u32(job.crc);
    output.write_bit(0);
    output.write_bits(24, origin as u32);
    write_symbol_map(&mut output, &symbols.in_use);
    output.write_bits(3, coding.n_groups as u32);
    output.write_bits(15, coding.selectors.len() as u32);
    huffman::write_selectors(&mut output, &coding.selectors, coding.n_groups);
    huffman::write_tables(&mut output, &coding.lens);
    for (group, &table) in coding.selectors.iter().enumerate() {
        let start = group * huffman::GROUP_SIZE;
        let end = (start + huffman::GROUP_SIZE).min(symbols.syms.len());
        let lengths = &coding.lens[table as usize];
        let codes = &coding.codes[table as usize];
        for &symbol in &symbols.syms[start..end] { output.write_bits(lengths[symbol as usize] as u32, codes[symbol as usize]); }
    }
    let (bytes, bit_len) = output.finish_bits();
    EncodedBlock { bytes, bit_len, crc: job.crc, input_len: job.input_len }
}

fn reservation(block_limit: usize) -> usize { block_limit.saturating_mul(32) }

pub struct Encoder<W: Write> {
    output: Option<W>,
    bits: BitWriter,
    pipeline: StreamingOrdered<EncodedBlock>,
    block_limit: usize,
    reservation: usize,
    block: Vec<u8>,
    block_crc: Bz2Crc,
    block_input_len: usize,
    runs: rle1::Splitter,
    combined_crc: u32,
    input_len: u64,
    output_len: u64,
    blocks: u64,
}

impl<W: Write> Encoder<W> {
    pub fn new(output: W, options: EncodeOptions) -> Result<Self> {
        let options = options.validate()?;
        let level = options.level_or(9);
        let block_limit = level as usize * 100_000 - 19;
        let reservation = reservation(block_limit);
        if reservation > options.memory_limit {
            return Err(Error::InvalidConfiguration(format!(
                "bzip2 level {level} compression requires a memory limit of at least {reservation} bytes; choose a lower level or raise --memory-limit"
            )));
        }
        let requested = options.resolved_threads();
        let requested = if options.threads == 0 { requested.min(AUTO_WORKERS) } else { requested };
        let workers = requested.min((options.memory_limit / reservation).max(1));
        let mut encoder = Self {
            output: Some(output),
            bits: BitWriter::new(),
            pipeline: StreamingOrdered::new(workers, options.memory_limit, "fbz-bzip2-encode")?,
            block_limit,
            reservation,
            block: Vec::with_capacity(block_limit),
            block_crc: Bz2Crc::new(),
            block_input_len: 0,
            runs: rle1::Splitter::new(),
            combined_crc: 0,
            input_len: 0,
            output_len: 0,
            blocks: 0,
        };
        for byte in [b'B', b'Z', b'h', b'0' + level] { encoder.bits.write_u8(byte); }
        encoder.drain_bits()?;
        Ok(encoder)
    }

    fn drain_bits(&mut self) -> Result<()> {
        let bytes = self.bits.drain();
        self.output.as_mut().unwrap().write_all(&bytes)?;
        self.output_len += bytes.len() as u64;
        Ok(())
    }

    fn commit_next(&mut self) -> Result<()> {
        let block = self.pipeline.take_next()?;
        self.bits.write_buffer(&block.bytes, block.bit_len);
        self.drain_bits()?;
        self.combined_crc = self.combined_crc.rotate_left(1) ^ block.crc;
        self.input_len += block.input_len as u64;
        self.blocks += 1;
        Ok(())
    }

    fn submit_block(&mut self) -> Result<()> {
        if self.block.is_empty() { return Ok(()); }
        while !self.pipeline.can_submit(self.reservation) { self.commit_next()?; }
        let rle = std::mem::replace(&mut self.block, Vec::with_capacity(self.block_limit));
        let crc = std::mem::replace(&mut self.block_crc, Bz2Crc::new()).finish();
        let input_len = std::mem::take(&mut self.block_input_len);
        self.pipeline.submit(self.reservation, move || encode_block(BlockJob { rle, crc, input_len }))
    }

    fn commit_group(&mut self, group: rle1::Group) -> Result<()> {
        if self.block.len() + group.encoded_len() > self.block_limit { self.submit_block()?; }
        group.write_into(&mut self.block);
        self.block_crc.push_repeat(group.byte, group.raw_len);
        self.block_input_len += group.raw_len;
        Ok(())
    }

    pub fn finish(mut self) -> Result<(W, EncodeReport)> {
        if let Some(group) = self.runs.finish() { self.commit_group(group)?; }
        self.submit_block()?;
        while self.pipeline.has_pending() { self.commit_next()?; }
        self.bits.write_magic(EOS_MAGIC);
        self.bits.write_u32(self.combined_crc);
        let trailing = std::mem::take(&mut self.bits).finish();
        self.output.as_mut().unwrap().write_all(&trailing)?;
        self.output.as_mut().unwrap().flush()?;
        self.output_len += trailing.len() as u64;
        let report = EncodeReport { input_len: self.input_len, output_len: self.output_len, blocks: self.blocks };
        Ok((self.output.take().unwrap(), report))
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for &byte in bytes { if let Some(group) = self.runs.push(byte) { self.commit_group(group).map_err(Error::into_io)?; } }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> { self.drain_bits().map_err(Error::into_io)?; self.output.as_mut().unwrap().flush() }
}

pub fn compress(data: &[u8], options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    compress_to_writer(&mut io::Cursor::new(data), &mut output, options)?;
    Ok(output)
}

pub fn compress_to_writer(input: &mut impl Read, output: &mut impl Write, options: EncodeOptions) -> Result<EncodeReport> {
    let mut encoder = Encoder::new(output, options)?;
    io::copy(input, &mut encoder)?;
    let (_, report) = encoder.finish()?;
    Ok(report)
}
